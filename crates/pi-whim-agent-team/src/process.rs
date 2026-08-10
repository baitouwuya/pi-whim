use std::{
    io::{BufReader, Read},
    process::{Command, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::{
    HostContext, SharedState,
    capture::{MAX_CAPTURE_BYTES, RunCapture, truncate_utf8},
    model::{AgentId, AgentStatus, ProcessCommand, SpawnAgentArguments},
    observe_hook,
};
use pi_whim_core::{AgentModelSelection, AgentPermissionPolicy, HookEvent, SandboxConfig};
use serde_json::json;

const MAX_JSONL_RECORD_BYTES: usize = 1024 * 1024;

struct ChildEnvironment {
    values: std::collections::HashMap<String, String>,
    temporary_directory: std::path::PathBuf,
}

pub(crate) struct AgentFinish {
    pub interrupted: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub error: String,
    pub transcript_entries: Vec<crate::model::AgentSessionEntry>,
}

pub fn launch_child(
    host: &HostContext,
    agent_id: AgentId,
    capability: String,
    level: u8,
    policy: AgentPermissionPolicy,
    delegated_models: Vec<AgentModelSelection>,
    arguments: SpawnAgentArguments,
) -> Result<(), String> {
    if policy.level != pi_whim_core::AgentPermissionLevel::Full
        && !std::path::Path::new("/usr/bin/sandbox-exec").is_file()
    {
        return Err(
            "controlled and read-only agents require the macOS sandbox-exec backend".into(),
        );
    }
    if delegated_models.len() != 1 {
        return Err("a subagent launch requires exactly one delegated provider and model".into());
    }
    let sandbox = &host.launch.team_config.sandbox_config;
    let environment = filtered_environment(host, &delegated_models, sandbox)?;
    let extensions = trusted_extension_paths(host, &policy)?;
    let mut command = child_command(
        host,
        &policy,
        &environment.temporary_directory,
        &extensions,
        sandbox,
    );
    command.current_dir(&host.launch.project_path).args([
        "--mode",
        "json",
        "-p",
        "--no-session",
        "--no-extensions",
        "--no-skills",
    ]);
    if let Some(tools) = child_tool_allowlist(&policy) {
        command.arg("--tools").arg(tools);
    }
    for extension in extensions {
        command.arg("--extension").arg(extension);
    }
    if let Some(provider) = arguments.provider.as_deref() {
        command.arg("--provider").arg(provider);
    }
    if let Some(model) = arguments.model.as_deref() {
        command.arg("--model").arg(model);
    }
    let identity_prompt = format!(
        "You are subagent '{}' at level {}. Role: {}. Use list_agents and read_messages for coordination. send_message can reach only siblings with the same parent or a direct parent/child. Do not attempt to bypass team boundaries.",
        arguments.name,
        level,
        if arguments.role.trim().is_empty() {
            "general-purpose"
        } else {
            arguments.role.trim()
        }
    );
    command
        .arg("--append-system-prompt")
        .arg(identity_prompt)
        .arg(format!("Task: {}", arguments.task))
        .env_clear()
        .envs(&environment.values)
        .env("PI_WHIM_AGENT_HOST", &host.endpoint)
        .env("PI_WHIM_AGENT_CAPABILITY", &capability)
        .env("PI_WHIM_AGENT_ID", agent_id.to_string())
        .env("PI_WHIM_AGENT_LEVEL", level.to_string())
        .env(
            "PI_WHIM_PERMISSION_LEVEL",
            format!("{:?}", policy.level).to_ascii_lowercase(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&environment.temporary_directory);
            return Err(error.to_string());
        }
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = std::fs::remove_dir_all(&environment.temporary_directory);
        return Err("subagent stdout was unavailable".into());
    };
    let Some(stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = std::fs::remove_dir_all(&environment.temporary_directory);
        return Err("subagent stderr was unavailable".into());
    };
    let capture = Arc::new(Mutex::new(RunCapture::default()));
    let capture_for_reader = capture.clone();
    let live_capture = Some((host.shared.clone(), agent_id));
    let stdout_reader = thread::spawn(move || {
        capture_stdout(stdout, capture_for_reader, live_capture);
    });
    let stderr_reader = thread::spawn(move || drain_stderr(stderr));
    let (control_sender, control_receiver) = mpsc::channel();
    {
        let (lock, _) = &*host.shared;
        let mut state = lock.lock().map_err(|_| "agent state lock was poisoned")?;
        state.controls.insert(agent_id, control_sender);
        if let Some(node) = state.actors.get_mut(&agent_id) {
            node.descriptor.status = AgentStatus::Running;
        }
    }

    let shared = host.shared.clone();
    let interactions = host.interactions.clone();
    let hook_host = host.clone();
    let temporary_directory = environment.temporary_directory;
    thread::spawn(move || {
        let mut interrupted = false;
        let mut force_kill_at = None;
        let (exit_status, wait_error) = loop {
            match control_receiver.recv_timeout(Duration::from_millis(50)) {
                Ok(ProcessCommand::Interrupt) => {
                    interrupted = true;
                    if force_kill_at.is_none() {
                        request_graceful_termination(&mut child);
                        force_kill_at = Some(Instant::now() + Duration::from_secs(3));
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if force_kill_at.is_some_and(|deadline| Instant::now() >= deadline) {
                let _ = child.kill();
                force_kill_at = None;
            }
            match child.try_wait() {
                Ok(Some(status)) => break (Some(status), None),
                Ok(None) => {}
                Err(error) => break (None, Some(error.to_string())),
            }
        };
        let _ = stdout_reader.join();
        let error = process_error(
            stderr_reader.join().unwrap_or_default(),
            exit_status.as_ref(),
            wait_error.as_deref(),
        );
        let output = capture
            .lock()
            .map(|capture| capture.final_output.clone())
            .unwrap_or_default();
        let transcript_entries = capture
            .lock()
            .map(|capture| capture.entries.iter().cloned().collect())
            .unwrap_or_default();
        let exit_code = exit_status
            .as_ref()
            .and_then(std::process::ExitStatus::code);
        finish_agent(
            &shared,
            &interactions,
            agent_id,
            AgentFinish {
                interrupted,
                exit_code,
                output,
                error,
                transcript_entries,
            },
        );
        observe_hook(
            &hook_host,
            HookEvent::AgentFinished,
            agent_id,
            None,
            json!({"agent_id": agent_id, "interrupted": interrupted, "exit_code": exit_code}),
        );
        let _ = std::fs::remove_dir_all(temporary_directory);
    });
    Ok(())
}

fn child_tool_allowlist(policy: &AgentPermissionPolicy) -> Option<String> {
    (!policy.enabled_tools.is_empty()).then(|| policy.enabled_tools.join(","))
}

fn trusted_extension_paths(
    host: &HostContext,
    policy: &AgentPermissionPolicy,
) -> Result<Vec<std::path::PathBuf>, String> {
    let mut paths = host.launch.extension_paths.clone();
    for raw in &policy.trusted_extensions {
        let path = std::path::PathBuf::from(raw);
        if !path.is_file() {
            return Err(format!(
                "trusted extension does not exist: {}",
                path.display()
            ));
        }
        paths.push(path);
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn child_command(
    host: &HostContext,
    policy: &AgentPermissionPolicy,
    child_directory: &std::path::Path,
    extensions: &[std::path::PathBuf],
    sandbox: &SandboxConfig,
) -> Command {
    if policy.level == pi_whim_core::AgentPermissionLevel::Full {
        return Command::new(&host.launch.executable);
    }
    let profile = child_sandbox_profile(
        &host.launch.project_path,
        child_directory,
        &host.launch.executable,
        extensions,
        child_network_policy(sandbox),
        sandbox,
    );
    let mut command = Command::new("/usr/bin/sandbox-exec");
    command.args(["-p", &profile]).arg(&host.launch.executable);
    command
}

fn child_sandbox_profile(
    project_root: &std::path::Path,
    child_directory: &std::path::Path,
    executable: &std::path::Path,
    extensions: &[std::path::PathBuf],
    network_policy: &str,
    sandbox: &SandboxConfig,
) -> String {
    fn quoted(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('"', "\\\"")
    }
    /// Resolve symlinks so the sandbox `subpath` matches the kernel's real path.
    /// macOS temp directories live under `/var/folders` (symlinked to
    /// `/private/var/folders`); sandbox-exec does not resolve symlinks in
    /// `subpath` directives, causing silent access denials.
    fn canonical(path: &std::path::Path) -> std::path::PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    let project_root = canonical(project_root);
    let child_directory = canonical(child_directory);
    let executable = canonical(executable);

    let mut read_paths = vec![
        format!("(subpath \"{}\")", quoted(&project_root)),
        format!("(subpath \"{}\")", quoted(&child_directory)),
        format!("(subpath \"{}\")", quoted(&executable)),
        "(subpath \"/usr\")".into(),
        "(subpath \"/bin\")".into(),
        "(subpath \"/sbin\")".into(),
        "(subpath \"/System\")".into(),
        "(subpath \"/dev\")".into(),
    ];
    for extension in extensions {
        // Use the parent directory so the sandbox permits reading sibling files
        // (e.g., client.ts alongside index.ts) that the entrypoint imports.
        let dir = extension.parent().unwrap_or(extension);
        let dir = canonical(dir);
        read_paths.push(format!("(subpath \"{}\")", quoted(&dir)));
    }

    // Monotonic deny paths: placed before allow rules so first-match-wins
    // narrows the allowed set. These are deny rules that override any allow.
    let mut deny_rules = Vec::new();
    for path in &sandbox.child_deny_read_paths {
        deny_rules.push(format!(
            "(deny file-read* (subpath \"{}\"))",
            quoted(&canonical(path))
        ));
    }
    for path in &sandbox.child_deny_write_paths {
        deny_rules.push(format!(
            "(deny file-write* (subpath \"{}\"))",
            quoted(&canonical(path))
        ));
    }
    let deny_rules = deny_rules.join(" ");

    format!(
        "(version 1) (deny default) (import \"system.sb\") (allow process*) {deny_rules} (allow file-read* {}) (allow file-write* (subpath \"{}\")) {network_policy}",
        read_paths.join(" "),
        quoted(&project_root),
    )
}

// sandbox-exec cannot safely express an arbitrary HTTPS hostname allowlist.
// The child receives only one provider credential; network is limited to outbound
// TCP so model inference works without granting listener or local-socket access.
// When sandbox.deny_child_network is true, all network access is denied.
fn child_network_policy(sandbox: &SandboxConfig) -> &'static str {
    if sandbox.deny_child_network {
        "(deny network*)"
    } else {
        "(allow network-outbound (remote tcp))"
    }
}

fn filtered_environment(
    host: &HostContext,
    delegated_models: &[AgentModelSelection],
    sandbox: &SandboxConfig,
) -> Result<ChildEnvironment, String> {
    let model = delegated_models
        .first()
        .ok_or_else(|| "a subagent launch requires a delegated provider and model".to_owned())?;
    let provider = model.provider.as_str();
    let agent_directory = host
        .launch
        .environment
        .get("PI_CODING_AGENT_DIR")
        .ok_or_else(|| "Pi agent directory is unavailable".to_owned())?;
    let models_path = std::path::PathBuf::from(agent_directory).join("models.json");
    let source = std::fs::read_to_string(&models_path).map_err(|error| error.to_string())?;
    let mut models: serde_json::Value =
        serde_json::from_str(&source).map_err(|error| error.to_string())?;
    let providers = models["providers"]
        .as_object_mut()
        .ok_or_else(|| "models.json has no providers".to_owned())?;
    let Some(config) = providers.remove(provider) else {
        return Err("delegated provider is not configured".into());
    };
    let mut config = config;
    let configured_models = config["models"]
        .as_array_mut()
        .ok_or_else(|| "delegated provider has no models".to_owned())?;
    configured_models.retain(|configured| configured["id"].as_str() == Some(&model.model));
    if configured_models.len() != 1 {
        return Err("delegated model is not configured for its provider".into());
    }
    providers.clear();
    providers.insert(provider.to_owned(), config);
    let directory = std::env::temp_dir().join(format!("pi-whim-child-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    std::fs::write(
        directory.join("models.json"),
        serde_json::to_vec(&models).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    // Resolve symlinks (e.g., /var/folders -> /private/var/folders on macOS) so
    // PI_CODING_AGENT_DIR matches the sandbox-exec subpath, which is also
    // canonicalized. Without this, the child cannot read models.json.
    let directory = std::fs::canonicalize(&directory).unwrap_or(directory);
    // Pi resolves its managed rg and fd binaries through PATH. Keep the parent's
    // managed bin directory available while isolating the child's configuration.
    let managed_bin = std::path::PathBuf::from(agent_directory).join("bin");
    let search_path = std::env::join_paths([
        managed_bin,
        std::path::PathBuf::from("/usr/bin"),
        std::path::PathBuf::from("/bin"),
        std::path::PathBuf::from("/usr/sbin"),
        std::path::PathBuf::from("/sbin"),
    ])
    .map_err(|error| error.to_string())?;
    let mut environment = std::collections::HashMap::from([
        (
            "PI_CODING_AGENT_DIR".into(),
            directory.to_string_lossy().into_owned(),
        ),
        ("PATH".into(), search_path.to_string_lossy().into_owned()),
        ("HOME".into(), directory.to_string_lossy().into_owned()),
    ]);
    let api_key = models["providers"][provider]["apiKey"]
        .as_str()
        .and_then(|value| value.strip_prefix('$'));
    if let Some(api_key) = api_key
        && let Some(value) = host.launch.environment.get(api_key)
    {
        environment.insert(api_key.to_owned(), value.clone());
    }

    // Apply monotonic environment restrictions: drop named variables.
    // This can only remove variables, never inject or restore them.
    for var in &sandbox.drop_environment_vars {
        environment.remove(var);
    }

    Ok(ChildEnvironment {
        values: environment,
        temporary_directory: directory,
    })
}

/// Drain stdout completely so an overproducing child cannot block on its pipe. Only complete,
/// reasonably-sized JSONL records are retained for result parsing.
fn capture_stdout(
    stdout: impl Read,
    capture: Arc<Mutex<RunCapture>>,
    live_capture: Option<(SharedState, AgentId)>,
) {
    let mut reader = BufReader::new(stdout);
    let mut chunk = [0_u8; 8 * 1024];
    let mut record = Vec::with_capacity(8 * 1024);
    let mut discarding_record = false;
    while let Ok(read) = reader.read(&mut chunk) {
        if read == 0 {
            break;
        }
        for byte in &chunk[..read] {
            if *byte == b'\n' {
                if !discarding_record {
                    let line = record.strip_suffix(b"\r").unwrap_or(&record);
                    if let Ok(line) = std::str::from_utf8(line)
                        && let Ok(mut capture) = capture.lock()
                    {
                        capture.ingest_line(line);
                        if let Some((shared, agent_id)) = live_capture.as_ref() {
                            flush_captured_entries(&mut capture, shared, *agent_id);
                        }
                    }
                }
                record.clear();
                discarding_record = false;
            } else if !discarding_record {
                if record.len() < MAX_JSONL_RECORD_BYTES {
                    record.push(*byte);
                } else {
                    record.clear();
                    discarding_record = true;
                }
            }
        }
    }
}

fn flush_captured_entries(capture: &mut RunCapture, shared: &SharedState, agent_id: AgentId) {
    let entries: Vec<_> = capture.entries.drain(..).collect();
    if entries.is_empty() {
        return;
    }
    let (lock, condition) = &**shared;
    let Ok(mut state) = lock.lock() else {
        return;
    };
    if let Some(node) = state.actors.get_mut(&agent_id) {
        for entry in entries {
            crate::session_read::push_bounded_entry(
                &mut node.transcript,
                entry,
                crate::MAX_SESSION_ENTRIES,
            );
        }
        crate::catalog::publish(node);
    }
    condition.notify_all();
}

/// Keep only bounded diagnostics while consuming the full pipe. Closing the read side after a
/// fixed-size read can otherwise stall or prematurely terminate a chatty child process.
fn drain_stderr(mut stderr: impl Read) -> String {
    let mut captured = Vec::with_capacity(MAX_CAPTURE_BYTES);
    let mut chunk = [0_u8; 8 * 1024];
    let mut truncated = false;
    while let Ok(read) = stderr.read(&mut chunk) {
        if read == 0 {
            break;
        }
        let remaining = MAX_CAPTURE_BYTES.saturating_sub(captured.len());
        let accepted = remaining.min(read);
        captured.extend_from_slice(&chunk[..accepted]);
        truncated |= accepted < read;
    }
    let mut output = String::from_utf8_lossy(&captured).into_owned();
    if truncated {
        output.push_str("\n[stderr truncated by Pi-Whim]");
    }
    truncate_utf8(output, MAX_CAPTURE_BYTES)
}

/// Keep an early crash diagnosable even when it occurs before the child writes stderr.
fn process_error(
    stderr: String,
    exit_status: Option<&std::process::ExitStatus>,
    wait_error: Option<&str>,
) -> String {
    let status_error = if let Some(error) = wait_error {
        Some(format!("could not observe subagent exit status: {error}"))
    } else if let Some(status) = exit_status {
        if let Some(code) = status.code() {
            (code != 0).then(|| format!("subagent exited with status {code}"))
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                status
                    .signal()
                    .map(|signal| format!("subagent terminated by signal {signal}"))
            }
            #[cfg(not(unix))]
            {
                Some("subagent exited without a status code".into())
            }
        }
    } else {
        Some("subagent exit status was unavailable".into())
    };

    match (stderr.trim(), status_error) {
        ("", Some(status_error)) => status_error,
        (stderr, Some(status_error)) => format!("{stderr}\n{status_error}"),
        (stderr, None) => stderr.to_owned(),
    }
}

#[cfg(unix)]
fn request_graceful_termination(child: &mut std::process::Child) {
    // The PID comes directly from the live Child handle.
    unsafe {
        libc::kill(child.id() as libc::pid_t, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn request_graceful_termination(child: &mut std::process::Child) {
    let _ = child.kill();
}

pub(crate) fn finish_agent(
    shared: &SharedState,
    interactions: &std::sync::Arc<
        std::sync::Mutex<std::collections::HashMap<uuid::Uuid, crate::PendingInteraction>>,
    >,
    agent_id: AgentId,
    finish: AgentFinish,
) {
    // Background jobs outlive a finished subagent and become the direct owner's responsibility.
    crate::bash_dispatch::transfer_owned_to_parent(shared, agent_id);
    crate::revoke_interactions_for_agent(interactions, agent_id);
    let (lock, condition) = &**shared;
    let Ok(mut state) = lock.lock() else {
        return;
    };
    state.controls.remove(&agent_id);
    state.capabilities.retain(|_, owner| *owner != agent_id);
    if let Some(node) = state.actors.get_mut(&agent_id) {
        for entry in finish.transcript_entries {
            crate::session_read::push_bounded_entry(
                &mut node.transcript,
                entry,
                crate::MAX_SESSION_ENTRIES,
            );
        }
        node.outcome.output = finish.output;
        node.outcome.error = finish.error;
        node.outcome.exit_code = finish.exit_code;
        node.descriptor.status = if finish.interrupted {
            AgentStatus::Interrupted
        } else if finish.exit_code == Some(0) {
            AgentStatus::Completed
        } else {
            AgentStatus::Failed
        };
        crate::catalog::unregister_active(node.descriptor.session_id);
        crate::catalog::publish(node);
    }
    if finish.interrupted {
        let descendants: Vec<_> = state
            .actors
            .keys()
            .copied()
            .filter(|candidate| is_descendant(&state, *candidate, agent_id))
            .filter_map(|id| state.controls.get(&id).cloned())
            .collect();
        for control in descendants {
            let _ = control.send(ProcessCommand::Interrupt);
        }
    }
    condition.notify_all();
}

fn is_descendant(state: &crate::model::TeamState, candidate: AgentId, ancestor: AgentId) -> bool {
    let mut cursor = state
        .actors
        .get(&candidate)
        .and_then(|node| node.descriptor.parent_id);
    while let Some(parent) = cursor {
        if parent == ancestor {
            return true;
        }
        cursor = state
            .actors
            .get(&parent)
            .and_then(|node| node.descriptor.parent_id);
    }
    false
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[test]
    fn oversized_stdout_records_are_discarded_but_the_pipe_keeps_draining() {
        let input = format!(
            "{}\n{{\"type\":\"message_end\",\"message\":{{\"role\":\"assistant\",\"content\":\"done\"}}}}\n",
            "x".repeat(MAX_JSONL_RECORD_BYTES + 1),
        );
        let capture = Arc::new(Mutex::new(RunCapture::default()));
        capture_stdout(Cursor::new(input), capture.clone(), None);
        assert_eq!(capture.lock().unwrap().final_output, "done");
    }

    #[test]
    fn stderr_is_bounded_after_a_full_drain() {
        let error = drain_stderr(Cursor::new("e".repeat(MAX_CAPTURE_BYTES * 2)));
        assert!(error.len() <= MAX_CAPTURE_BYTES);
        assert!(error.ends_with("[output truncated by Pi-Whim]"));
    }

    #[test]
    fn child_tool_allowlist_preserves_native_search_tools() {
        let policy = AgentPermissionPolicy {
            enabled_tools: vec!["read".into(), "grep".into(), "find".into()],
            ..AgentPermissionPolicy::default()
        };

        assert_eq!(
            child_tool_allowlist(&policy).as_deref(),
            Some("read,grep,find")
        );
        assert_eq!(
            child_tool_allowlist(&AgentPermissionPolicy::default()),
            None
        );
    }

    #[test]
    fn child_sandbox_profile_limits_filesystem_and_network() {
        let sandbox = SandboxConfig::default();
        let profile = child_sandbox_profile(
            std::path::Path::new("/project"),
            std::path::Path::new("/temporary-child"),
            std::path::Path::new("/usr/local/bin/pi"),
            &[std::path::PathBuf::from("/extensions/team.ts")],
            "(allow network-outbound (remote tcp))",
            &sandbox,
        );
        assert!(profile.contains("(import \"system.sb\")"));
        // Non-existent paths fall back to the literal path when canonicalize fails.
        assert!(profile.contains("(subpath \"/project\")"));
        assert!(profile.contains("(subpath \"/temporary-child\")"));
        // Extensions use the parent directory so sibling files are readable.
        assert!(profile.contains("(subpath \"/extensions\")"));
        assert!(!profile.contains("(subpath \"/extensions/team.ts\")"));
        assert!(profile.contains("(allow network-outbound (remote tcp))"));
    }

    #[cfg(unix)]
    #[test]
    fn signal_terminated_children_have_a_nonempty_diagnostic() {
        let status = <std::process::ExitStatus as std::os::unix::process::ExitStatusExt>::from_raw(
            libc::SIGABRT,
        );

        assert!(process_error(String::new(), Some(&status), None).contains("signal"));
    }
}
