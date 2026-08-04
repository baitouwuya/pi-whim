//! Coordinated local shell execution for root agents and subagents.
//!
//! Commands are always started by this Rust supervisor. Background children are
//! owned by an agent, reassigned to its direct parent when that agent exits, and
//! terminated when the supervisor stops.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Read,
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use uuid::Uuid;

use crate::{
    HostContext, HostError, HostResult, SharedState,
    model::{
        AgentDescriptor, AgentId, BackgroundProcess, BackgroundProcessStatus,
        BackgroundProcessSummary, BashArguments, ProcessIdArguments,
    },
};
use pi_whim_core::AgentPermissionLevel;

const DEFAULT_TIMEOUT_SECONDS: u64 = 300;
const MAX_TIMEOUT_SECONDS: u64 = 86_400;
const MAX_CAPTURE_BYTES: usize = 256 * 1024;
const MAX_TAIL_BYTES: usize = 64 * 1024;
const MAX_RUNNING_PER_AGENT: usize = 16;
const MAX_RUNNING_PER_TEAM: usize = 64;
const MAX_PROCESS_HISTORY: usize = 128;
const PROCESS_HISTORY_RETENTION: Duration = Duration::from_secs(15 * 60);
const TERMINATE_GRACE: Duration = Duration::from_secs(2);

pub fn execute(
    host: &HostContext,
    actor_id: AgentId,
    arguments: BashArguments,
    cancelled: Option<&AtomicBool>,
) -> HostResult {
    let command = arguments.command.trim();
    if command.is_empty() {
        return Err(HostError::new(
            "bash_invalid_command",
            "command cannot be empty",
        ));
    }
    if command.len() > 64 * 1024 {
        return Err(HostError::new(
            "bash_invalid_command",
            "command exceeds the 64 KiB limit",
        ));
    }
    if arguments.timeout == Some(0)
        || arguments
            .timeout
            .is_some_and(|seconds| seconds > MAX_TIMEOUT_SECONDS)
    {
        return Err(HostError::new(
            "bash_invalid_timeout",
            format!("timeout must be between 1 and {MAX_TIMEOUT_SECONDS} seconds"),
        ));
    }
    let actor = actor(host, actor_id)?;
    command_allowed(host, &actor, command, arguments.approval_ticket.as_deref())?;
    if arguments.background {
        ensure_background_capacity(host, actor_id)?;
    }
    let mut child = spawn_command(
        &host.launch.project_path,
        command,
        actor.permission_level,
        &host.launch.team_config.sandbox_config,
    )?;
    if arguments.background {
        let timeout_seconds = arguments.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let id = Uuid::new_v4();
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let summary = BackgroundProcessSummary {
            id,
            owner_id: actor.id,
            owner_session_id: actor.session_id,
            command: command.to_owned(),
            cwd: host.launch.project_path.clone(),
            status: BackgroundProcessStatus::Running,
            started_at_ms,
            timeout_seconds,
            output_bytes: 0,
            output_truncated: false,
            exit_code: None,
        };
        let process = BackgroundProcess {
            summary: summary.clone(),
            child: Arc::new(Mutex::new(child)),
            started_at: Instant::now(),
            finished_at: None,
            output: VecDeque::new(),
            output_truncated: false,
            readers: Vec::new(),
        };
        {
            let (lock, condition) = &*host.shared;
            let mut state = lock
                .lock()
                .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
            prune_history(&mut state.background_processes);
            state.background_processes.insert(id, process);
            condition.notify_all();
        }
        let stdout_reader = start_output_reader(host.shared.clone(), id, stdout);
        let stderr_reader = start_output_reader(host.shared.clone(), id, stderr);
        {
            let (lock, _) = &*host.shared;
            let mut state = lock
                .lock()
                .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
            if let Some(process) = state.background_processes.get_mut(&id) {
                process.readers.extend([stdout_reader, stderr_reader]);
            }
        }
        start_background_reaper(host.shared.clone(), id);
        return Ok(json!({
            "background": true,
            "process": summary,
            "message": format!("Background process {id} started."),
        }));
    }

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let output = Arc::new(Mutex::new(VecDeque::new()));
    let truncated = Arc::new(Mutex::new(false));
    let stdout_reader = start_foreground_reader(stdout, output.clone(), truncated.clone());
    let stderr_reader = start_foreground_reader(stderr, output.clone(), truncated.clone());
    let started = Instant::now();
    let mut timed_out = false;
    let mut was_cancelled = false;
    let timeout_seconds = arguments.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)) => {
                was_cancelled = true;
                terminate_child(&mut child);
            }
            Ok(None) if started.elapsed() >= Duration::from_secs(timeout_seconds) => {
                timed_out = true;
                terminate_child(&mut child);
            }
            Ok(None) => thread::sleep(Duration::from_millis(25)),
            Err(_) => break None,
        }
    };
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    let output = output
        .lock()
        .map(|mut value| String::from_utf8_lossy(value.make_contiguous()).into_owned())
        .unwrap_or_default();
    let truncated = truncated.lock().map(|value| *value).unwrap_or(false);
    let status_name = if was_cancelled {
        "stopped"
    } else if timed_out {
        "timed_out"
    } else if status.is_some_and(|value| value.success()) {
        "completed"
    } else {
        "failed"
    };
    Ok(json!({
        "background": false,
        "output": output,
        "status": status_name,
        "exit_code": status.and_then(|status| status.code()),
        "cancelled": was_cancelled,
        "timed_out": timed_out,
        "truncated": truncated,
    }))
}

pub fn list(host: &HostContext, actor_id: AgentId) -> HostResult {
    reap_finished(&host.shared);
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let processes = state
        .background_processes
        .values()
        .filter(|process| process.summary.owner_id == actor_id)
        .map(|process| process.summary.clone())
        .collect::<Vec<_>>();
    Ok(json!({ "processes": processes }))
}

pub fn read(host: &HostContext, actor_id: AgentId, arguments: ProcessIdArguments) -> HostResult {
    reap_finished(&host.shared);
    let tail_bytes = arguments
        .tail_bytes
        .unwrap_or(8 * 1024)
        .clamp(1, MAX_TAIL_BYTES);
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let process = state
        .background_processes
        .get(&arguments.process_id)
        .ok_or_else(|| HostError::new("process_not_found", "background process was not found"))?;
    if process.summary.owner_id != actor_id {
        return Err(HostError::new(
            "process_forbidden",
            "only the current owning agent can read this process",
        ));
    }
    let start = process.output.len().saturating_sub(tail_bytes);
    let output = String::from_utf8_lossy(
        &process
            .output
            .iter()
            .skip(start)
            .copied()
            .collect::<Vec<_>>(),
    )
    .into_owned();
    Ok(json!({
        "process": process.summary,
        "output": output,
        "output_truncated": process.output_truncated,
        "output_stream": "stdout_stderr_combined",
    }))
}

pub fn stop(host: &HostContext, actor_id: AgentId, arguments: ProcessIdArguments) -> HostResult {
    let (lock, condition) = &*host.shared;
    let (child, summary) = {
        let mut state = lock
            .lock()
            .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
        let process = state
            .background_processes
            .get_mut(&arguments.process_id)
            .ok_or_else(|| {
                HostError::new("process_not_found", "background process was not found")
            })?;
        if process.summary.owner_id != actor_id {
            return Err(HostError::new(
                "process_forbidden",
                "only the current owning agent can stop this process",
            ));
        }
        let child = process
            .summary
            .status
            .is_running()
            .then(|| process.child.clone());
        process.summary.status = BackgroundProcessStatus::Stopped;
        process.finished_at = Some(Instant::now());
        (child, process.summary.clone())
    };
    condition.notify_all();
    if let Some(child) = child {
        terminate_shared_child(&child);
        join_readers(take_readers(&host.shared, arguments.process_id));
    }
    Ok(json!({ "stopped": true, "process": summary }))
}

pub fn append_prompt_context(
    host: &HostContext,
    actor_id: AgentId,
    text: &str,
) -> Result<String, HostError> {
    reap_finished(&host.shared);
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let processes = state
        .background_processes
        .values()
        .filter(|process| {
            process.summary.owner_id == actor_id && process.summary.status.is_running()
        })
        .map(|process| {
            format!(
                "- {}: {} (running {}s, timeout {}s)",
                process.summary.id,
                one_line(&process.summary.command, 160),
                process.started_at.elapsed().as_secs(),
                process.summary.timeout_seconds
            )
        })
        .collect::<Vec<_>>();
    if processes.is_empty() {
        return Ok(text.to_owned());
    }
    Ok(format!(
        "{text}\n\n<pi_whim_background_processes>\n{}\nUse list_processes/read_process/stop_process to manage only your processes.\n</pi_whim_background_processes>",
        processes.join("\n")
    ))
}

pub fn transfer_owned_to_parent(shared: &SharedState, owner_id: AgentId) {
    let (lock, condition) = &**shared;
    let Ok(mut state) = lock.lock() else {
        return;
    };
    let Some(parent_id) = state
        .actors
        .get(&owner_id)
        .and_then(|node| node.descriptor.parent_id)
    else {
        return;
    };
    let Some(parent) = state
        .actors
        .get(&parent_id)
        .map(|node| node.descriptor.clone())
    else {
        return;
    };
    let Some(owner) = state
        .actors
        .get(&owner_id)
        .map(|node| node.descriptor.clone())
    else {
        return;
    };
    let transferred = state
        .background_processes
        .values_mut()
        .filter(|process| {
            process.summary.owner_id == owner_id && process.summary.status.is_running()
        })
        .map(|process| {
            process.summary.owner_id = parent.id;
            process.summary.owner_session_id = parent.session_id;
            process.summary.id
        })
        .collect::<Vec<_>>();
    if transferred.is_empty() {
        return;
    }
    let message_content = format!(
        "Inherited {} background process(es) from {}: {}",
        transferred.len(),
        owner.name,
        transferred
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let message = crate::model::AgentMessage {
        id: Uuid::new_v4(),
        sender_id: owner.id,
        sender_name: owner.name.clone(),
        recipient_id: parent.id,
        sender_session_id: owner.session_id,
        recipient_session_id: parent.session_id,
        kind: crate::model::MessageKind::DirectNotification,
        content: message_content.clone(),
    };
    let inbox = state.inboxes.entry(parent_id).or_default();
    while inbox.len() >= crate::MAX_INBOX_MESSAGES {
        inbox.pop_front();
    }
    inbox.push_back(message);
    if let Some(node) = state.actors.get_mut(&owner_id) {
        crate::record_session_entry(node, "notification_sent", Some(&parent), &message_content);
    }
    if let Some(node) = state.actors.get_mut(&parent_id) {
        crate::record_session_entry(
            node,
            "notification_received",
            Some(&owner),
            &message_content,
        );
    }
    condition.notify_all();
}

pub fn terminate_all(shared: &SharedState) {
    let (lock, condition) = &**shared;
    let Ok(mut state) = lock.lock() else {
        return;
    };
    let mut children = Vec::new();
    for process in state.background_processes.values_mut() {
        if process.summary.status.is_running() {
            process.summary.status = BackgroundProcessStatus::Stopped;
            process.finished_at = Some(Instant::now());
            children.push(process.child.clone());
        }
    }
    condition.notify_all();
    drop(state);
    for child in children {
        terminate_shared_child(&child);
    }
}

pub fn clear_all(shared: &SharedState) {
    terminate_all(shared);
    let (lock, condition) = &**shared;
    if let Ok(mut state) = lock.lock() {
        state.background_processes.clear();
        condition.notify_all();
    }
}

fn actor(host: &HostContext, actor_id: AgentId) -> Result<AgentDescriptor, HostError> {
    let (lock, _) = &*host.shared;
    lock.lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?
        .actors
        .get(&actor_id)
        .map(|node| node.descriptor.clone())
        .ok_or_else(|| HostError::new("unauthorized", "agent is unavailable"))
}

fn ensure_background_capacity(host: &HostContext, actor_id: AgentId) -> Result<(), HostError> {
    reap_finished(&host.shared);
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let team_running = state
        .background_processes
        .values()
        .filter(|process| process.summary.status.is_running())
        .count();
    let agent_running = state
        .background_processes
        .values()
        .filter(|process| {
            process.summary.owner_id == actor_id && process.summary.status.is_running()
        })
        .count();
    if agent_running >= MAX_RUNNING_PER_AGENT {
        return Err(HostError::with_details(
            "process_limit_reached",
            format!("an agent may run at most {MAX_RUNNING_PER_AGENT} background processes"),
            json!({ "scope": "agent", "limit": MAX_RUNNING_PER_AGENT }),
        ));
    }
    if team_running >= MAX_RUNNING_PER_TEAM {
        return Err(HostError::with_details(
            "process_limit_reached",
            format!("a team may run at most {MAX_RUNNING_PER_TEAM} background processes"),
            json!({ "scope": "team", "limit": MAX_RUNNING_PER_TEAM }),
        ));
    }
    Ok(())
}

fn command_allowed(
    host: &HostContext,
    actor: &AgentDescriptor,
    command: &str,
    approval_ticket: Option<&str>,
) -> Result<(), HostError> {
    let policy = host
        .launch
        .environment
        .get("PI_WHIM_BASH_POLICY")
        .map(String::as_str)
        .unwrap_or("allow");
    if policy == "deny" {
        return Err(HostError::new(
            "bash_forbidden",
            "Bash is disabled by policy",
        ));
    }
    let patterns = host
        .launch
        .environment
        .get("PI_WHIM_BASH_BLOCKED_PATTERNS")
        .and_then(|value| serde_json::from_str::<Vec<String>>(value).ok())
        .unwrap_or_default();
    for pattern in patterns {
        if command.contains(&pattern) {
            return Err(HostError::with_details(
                "bash_blocked",
                "command matched a blocked command filter",
                json!({ "pattern": pattern }),
            ));
        }
    }
    let agent_policy = {
        let (lock, _) = &*host.shared;
        lock.lock()
            .ok()
            .and_then(|state| state.actors.get(&actor.id).map(|node| node.policy.clone()))
            .unwrap_or_default()
    };
    if actor.permission_level == AgentPermissionLevel::ReadOnly {
        return Err(HostError::new(
            "bash_forbidden",
            "read-only agents cannot run Bash",
        ));
    }
    if actor.permission_level == AgentPermissionLevel::Controlled {
        let direct = command_matches_allowlist(command, &agent_policy.command_allowlist)?;
        if !direct {
            let high_risk = is_high_risk_command(command);
            if let Some(ticket) = approval_ticket {
                crate::consume_bash_approval(host, actor.id, command, Some(ticket))?;
            } else {
                let request_id = crate::request_bash_approval(host, actor.id, command, high_risk)?;
                return Err(HostError::with_details(
                    "approval_required",
                    "controlled command requires parent approval",
                    json!({ "request_id": request_id, "high_risk": high_risk }),
                ));
            }
        }
    }
    Ok(())
}

/// Parse only a simple argv sequence. Shell composition is rejected before a
/// glob is considered, so an allowed program cannot be used as a shell escape.
fn command_matches_allowlist(command: &str, patterns: &[String]) -> Result<bool, HostError> {
    if patterns.is_empty() {
        return Ok(false);
    }
    let tokens = shell_words(command)?;
    Ok(patterns.iter().any(|pattern| {
        shell_words(pattern)
            .ok()
            .is_some_and(|expected| argv_matches(&tokens, &expected))
    }))
}

fn shell_words(input: &str) -> Result<Vec<String>, HostError> {
    if input.contains(['|', ';', '>', '<', '`', '$', '&', '(', ')', '\n', '\r'])
        || input.contains("&&")
        || input.contains("||")
        || input.contains("$(")
    {
        return Err(HostError::new(
            "bash_invalid_command",
            "controlled commands cannot use shell composition, redirection, or substitution",
        ));
    }
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in input.chars() {
        if escaped {
            current.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if matches!(character, '\'' | '"') {
            if quote == Some(character) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(character);
            } else {
                current.push(character);
            }
        } else if character.is_whitespace() && quote.is_none() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
        } else {
            current.push(character);
        }
    }
    if quote.is_some() || escaped || !current.is_empty() && current.contains('\n') {
        return Err(HostError::new(
            "bash_invalid_command",
            "controlled command has invalid quoting",
        ));
    }
    if !current.is_empty() {
        words.push(current);
    }
    if words.is_empty() || words[0].contains('/') || words[0].contains('*') {
        return Err(HostError::new(
            "bash_invalid_command",
            "controlled commands require a literal executable name",
        ));
    }
    Ok(words)
}

fn argv_matches(actual: &[String], pattern: &[String]) -> bool {
    if pattern.is_empty() || actual.is_empty() || pattern[0] != actual[0] {
        return false;
    }
    let mut actual_index = 1;
    for (index, expected) in pattern.iter().enumerate().skip(1) {
        if expected == "**" && index + 1 == pattern.len() {
            return true;
        }
        let Some(value) = actual.get(actual_index) else {
            return false;
        };
        if expected != "*" && expected != value {
            return false;
        }
        actual_index += 1;
    }
    actual_index == actual.len()
}

fn is_high_risk_command(command: &str) -> bool {
    command.contains("rm ")
        || command.contains("curl ")
        || command.contains("wget ")
        || command.contains("ssh ")
        || command.contains("git push")
        || command.contains("chmod ")
        || command.contains("sudo ")
}

fn spawn_command(
    cwd: &std::path::Path,
    command: &str,
    permission_level: AgentPermissionLevel,
    sandbox: &pi_whim_core::SandboxConfig,
) -> Result<std::process::Child, HostError> {
    let mut process = Command::new("/bin/bash");
    if permission_level == AgentPermissionLevel::Controlled {
        if !std::path::Path::new("/usr/bin/sandbox-exec").is_file() {
            return Err(HostError::new(
                "sandbox_unavailable",
                "controlled agents require the macOS sandbox-exec backend",
            ));
        }
        let argv = shell_words(command)?;
        let executable = resolve_controlled_executable(&argv[0])?;
        let profile = sandbox_profile(cwd, sandbox);
        process = Command::new("/usr/bin/sandbox-exec");
        process
            .args(["-p", &profile])
            .arg(executable)
            .args(&argv[1..]);
    } else {
        process.args(["-lc", command]);
    }
    process
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        process.process_group(0);
    }
    process
        .spawn()
        .map_err(|error| HostError::new("bash_spawn_failed", error.to_string()))
}

fn resolve_controlled_executable(name: &str) -> Result<std::path::PathBuf, HostError> {
    for directory in ["/usr/bin", "/bin", "/usr/sbin", "/sbin"] {
        let path = std::path::Path::new(directory).join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(HostError::new(
        "bash_invalid_command",
        "controlled commands must use a system executable; PATH lookup is disabled",
    ))
}

fn sandbox_profile(
    project_root: &std::path::Path,
    sandbox: &pi_whim_core::SandboxConfig,
) -> String {
    fn quoted(path: &std::path::Path) -> String {
        path.to_string_lossy().replace('"', "\\\"")
    }
    fn canonical(path: &std::path::Path) -> std::path::PathBuf {
        std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
    }

    let root = quoted(&canonical(project_root));

    // Bash already denies network by default. The sandbox_config.deny_bash_network
    // field is a future-proofing flag for when the bash default network policy might
    // change; it has no effect on the current sandbox profile.
    let network = "(deny network*)";

    // Monotonic deny paths: placed before allow rules to narrow the allowed set.
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
        "(version 1) (deny default) (allow process*) {deny_rules} (allow file-read* (subpath \"{root}\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/System\")) (allow file-write* (subpath \"{root}\")) {network}"
    )
}

fn start_foreground_reader(
    pipe: Option<impl Read + Send + 'static>,
    output: Arc<Mutex<VecDeque<u8>>>,
    truncated: Arc<Mutex<bool>>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Some(pipe) = pipe {
            read_into(pipe, |bytes| {
                let Ok(mut output) = output.lock() else {
                    return;
                };
                append_output(&mut output, bytes, &truncated);
            });
        }
    })
}

fn start_output_reader(
    shared: SharedState,
    process_id: Uuid,
    pipe: Option<impl Read + Send + 'static>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        if let Some(pipe) = pipe {
            read_into(pipe, |bytes| {
                let (lock, _) = &*shared;
                let Ok(mut state) = lock.lock() else { return };
                let Some(process) = state.background_processes.get_mut(&process_id) else {
                    return;
                };
                let truncated = append_bounded(&mut process.output, bytes);
                process.output_truncated |= truncated;
                process.summary.output_truncated = process.output_truncated;
                process.summary.output_bytes =
                    process.summary.output_bytes.saturating_add(bytes.len());
            });
        }
    })
}

fn start_background_reaper(shared: SharedState, process_id: Uuid) {
    thread::spawn(move || {
        loop {
            let (child, started_at, timeout_seconds) = {
                let (lock, _) = &*shared;
                let Ok(state) = lock.lock() else { return };
                let Some(process) = state.background_processes.get(&process_id) else {
                    return;
                };
                if !process.summary.status.is_running() {
                    return;
                }
                (
                    process.child.clone(),
                    process.started_at,
                    process.summary.timeout_seconds,
                )
            };
            if started_at.elapsed() >= Duration::from_secs(timeout_seconds) {
                terminate_shared_child(&child);
                let readers = take_readers(&shared, process_id);
                join_readers(readers);
                set_process_status(&shared, process_id, BackgroundProcessStatus::TimedOut, None);
                return;
            }
            let status = child
                .lock()
                .ok()
                .and_then(|mut child| child.try_wait().ok())
                .flatten();
            if let Some(status) = status {
                let readers = take_readers(&shared, process_id);
                join_readers(readers);
                let next = if status.success() {
                    BackgroundProcessStatus::Completed
                } else {
                    BackgroundProcessStatus::Failed
                };
                set_process_status(&shared, process_id, next, status.code());
                return;
            }
            thread::sleep(Duration::from_millis(100));
        }
    });
}

fn set_process_status(
    shared: &SharedState,
    process_id: Uuid,
    status: BackgroundProcessStatus,
    exit_code: Option<i32>,
) {
    let (lock, condition) = &**shared;
    if let Ok(mut state) = lock.lock()
        && let Some(process) = state.background_processes.get_mut(&process_id)
        && process.summary.status.is_running()
    {
        process.summary.status = status;
        process.summary.exit_code = exit_code;
        process.finished_at = Some(Instant::now());
        condition.notify_all();
    }
}

fn prune_history(processes: &mut HashMap<Uuid, BackgroundProcess>) {
    let now = Instant::now();
    processes.retain(|_, process| {
        process.summary.status.is_running()
            || process.finished_at.is_none_or(|finished_at| {
                now.duration_since(finished_at) < PROCESS_HISTORY_RETENTION
            })
    });
    if processes.len() <= MAX_PROCESS_HISTORY {
        return;
    }
    let mut completed = processes
        .values()
        .filter(|process| !process.summary.status.is_running())
        .map(|process| (process.summary.started_at_ms, process.summary.id))
        .collect::<Vec<_>>();
    completed.sort_unstable_by_key(|(started_at, _)| *started_at);
    let remove_count = processes.len().saturating_sub(MAX_PROCESS_HISTORY);
    let remove = completed
        .into_iter()
        .take(remove_count)
        .map(|(_, id)| id)
        .collect::<HashSet<_>>();
    processes.retain(|id, _| !remove.contains(id));
}

fn reap_finished(shared: &SharedState) {
    let (lock, condition) = &**shared;
    let Ok(mut state) = lock.lock() else { return };
    prune_history(&mut state.background_processes);
    condition.notify_all();
}

fn take_readers(shared: &SharedState, process_id: Uuid) -> Vec<thread::JoinHandle<()>> {
    let (lock, _) = &**shared;
    lock.lock()
        .ok()
        .and_then(|mut state| {
            state
                .background_processes
                .get_mut(&process_id)
                .map(|process| std::mem::take(&mut process.readers))
        })
        .unwrap_or_default()
}

fn join_readers(readers: Vec<thread::JoinHandle<()>>) {
    for reader in readers {
        let _ = reader.join();
    }
}

fn read_into(mut pipe: impl Read, mut append: impl FnMut(&[u8])) {
    let mut buffer = [0u8; 8192];
    while let Ok(count) = pipe.read(&mut buffer) {
        if count == 0 {
            break;
        }
        append(&buffer[..count]);
    }
}

fn append_output(output: &mut VecDeque<u8>, bytes: &[u8], truncated: &Mutex<bool>) {
    let did_truncate = append_bounded(output, bytes);
    if did_truncate && let Ok(mut truncated) = truncated.lock() {
        *truncated = true;
    }
}

fn terminate_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    signal_process_group(child.id(), libc::SIGTERM);
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = Instant::now() + TERMINATE_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            _ => break,
        }
    }
    #[cfg(unix)]
    signal_process_group(child.id(), libc::SIGKILL);
    #[cfg(not(unix))]
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn signal_process_group(pid: u32, signal: libc::c_int) {
    // Shells are spawned as process-group leaders, so this includes descendants.
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

fn terminate_shared_child(child: &Arc<Mutex<std::process::Child>>) {
    if let Ok(mut child) = child.lock() {
        terminate_child(&mut child);
    }
}

fn append_bounded(output: &mut VecDeque<u8>, bytes: &[u8]) -> bool {
    if bytes.len() >= MAX_CAPTURE_BYTES {
        output.clear();
        output.extend(&bytes[bytes.len() - MAX_CAPTURE_BYTES..]);
        return true;
    }
    let overflow = output
        .len()
        .saturating_add(bytes.len())
        .saturating_sub(MAX_CAPTURE_BYTES);
    for _ in 0..overflow {
        output.pop_front();
    }
    output.extend(bytes);
    overflow > 0
}

fn one_line(value: &str, limit: usize) -> String {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.chars().count() <= limit {
        return value;
    }
    format!(
        "{}...",
        value
            .chars()
            .take(limit.saturating_sub(3))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_output_keeps_only_the_tail_and_reports_truncation() {
        let mut output = VecDeque::new();
        let input = vec![b'x'; MAX_CAPTURE_BYTES + 17];
        assert!(append_bounded(&mut output, &input));
        assert_eq!(output.len(), MAX_CAPTURE_BYTES);
        assert!(output.iter().all(|byte| *byte == b'x'));
    }

    #[test]
    fn one_line_collapses_whitespace_and_bounds_commands() {
        assert_eq!(one_line("  echo   hello\nworld ", 64), "echo hello world");
        assert_eq!(one_line("abcdef", 4), "a...");
    }

    #[test]
    fn structured_allowlist_matches_tokens_and_terminal_glob() {
        assert!(
            command_matches_allowlist("git status --short", &["git status **".into()]).unwrap()
        );
        assert!(
            command_matches_allowlist("cargo test -p core", &["cargo test * *".into()]).unwrap()
        );
        assert!(!command_matches_allowlist("git push", &["git status **".into()]).unwrap());
    }

    #[test]
    fn structured_allowlist_rejects_shell_composition() {
        for command in [
            "git status | cat",
            "git status && rm x",
            "git status > out",
            "git $(echo status)",
        ] {
            assert_eq!(
                command_matches_allowlist(command, &["git **".into()])
                    .unwrap_err()
                    .code,
                "bash_invalid_command"
            );
        }
    }
}
