//! Host-side Hook dispatcher for supervisor control-plane events.

use std::{
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use pi_whim_core::{
    HookAuditOutcome, HookAuditRecord, HookConfig, HookDefinition, HookEvent, HookKind,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HOOK_OUTPUT: usize = 64 * 1024;
const OBSERVE_QUEUE_CAPACITY: usize = 64;
const FINALIZE_BUDGET: Duration = Duration::from_secs(5);

struct ObserveTask {
    hook: HookDefinition,
    event: HookEvent,
    payload: Value,
}

pub(crate) trait RustHook: Send + Sync {
    fn id(&self) -> &'static str;
    fn gate(&self, event: HookEvent, payload: &Value) -> Result<(), String>;
}

struct SafetyFloorHook;

impl RustHook for SafetyFloorHook {
    fn id(&self) -> &'static str {
        "builtin.safety_floor"
    }

    fn gate(&self, event: HookEvent, payload: &Value) -> Result<(), String> {
        let arguments = || {
            payload
                .get("arguments")
                .and_then(Value::as_object)
                .ok_or_else(|| "hook event arguments must be an object".to_owned())
        };
        match event {
            HookEvent::MessageSending => {
                let arguments = arguments()?;
                let message = arguments
                    .get("message")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "message must be a string".to_owned())?;
                if message.trim().is_empty() || message.len() > crate::MAX_MESSAGE_BYTES {
                    return Err("message violates supervisor size constraints".into());
                }
            }
            HookEvent::AgentSpawning => {
                let arguments = arguments()?;
                for field in ["name", "task"] {
                    if arguments
                        .get(field)
                        .and_then(Value::as_str)
                        .is_none_or(|value| value.trim().is_empty())
                    {
                        return Err(format!("agent {field} cannot be empty"));
                    }
                }
            }
            HookEvent::PermissionResolving | HookEvent::ToolDispatching => {}
            _ => return Ok(()),
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct HookDispatcher {
    rust_hooks: Vec<Arc<dyn RustHook>>,
    hooks: Vec<HookDefinition>,
    project_root: PathBuf,
    audit_sender: mpsc::SyncSender<HookAuditRecord>,
    revision: String,
    observe_sender: mpsc::SyncSender<ObserveTask>,
    observe_stopping: Arc<AtomicBool>,
}

impl std::fmt::Debug for HookDispatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HookDispatcher")
            .field("rust_hooks", &self.rust_hooks.len())
            .field("command_hooks", &self.hooks.len())
            .field("project_root", &self.project_root)
            .field("revision", &self.revision)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HookDecision {
    Continue(Value),
    Deny(String),
}

impl HookDispatcher {
    pub(crate) fn new(
        config: HookConfig,
        project_root: PathBuf,
        audit_sender: mpsc::SyncSender<HookAuditRecord>,
    ) -> Self {
        let (observe_sender, observe_receiver) =
            mpsc::sync_channel::<ObserveTask>(OBSERVE_QUEUE_CAPACITY);
        let worker_project_root = project_root.clone();
        let worker_audit_sender = audit_sender.clone();
        let worker_revision = config.revision.clone();
        let observe_stopping = Arc::new(AtomicBool::new(false));
        let worker_stopping = observe_stopping.clone();
        thread::spawn(move || {
            for task in observe_receiver {
                if worker_stopping.load(Ordering::Acquire) {
                    break;
                }
                observe_once(
                    &worker_audit_sender,
                    &worker_revision,
                    &task.hook,
                    task.event,
                    &task.payload,
                    &worker_project_root,
                );
            }
        });
        Self {
            rust_hooks: vec![Arc::new(SafetyFloorHook)],
            revision: config.revision.clone(),
            hooks: config.hooks,
            project_root,
            audit_sender,
            observe_sender,
            observe_stopping,
        }
    }

    pub(crate) fn gate(&self, event: HookEvent, payload: Value) -> HookDecision {
        let mut payload = payload;
        for hook in &self.rust_hooks {
            let started = std::time::Instant::now();
            let result = hook.gate(event, &payload);
            let _ = self.audit_sender.try_send(HookAuditRecord {
                hook_id: hook.id().into(),
                event,
                outcome: if result.is_ok() {
                    HookAuditOutcome::Allowed
                } else {
                    HookAuditOutcome::Denied
                },
                duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                output_truncated: false,
                revision: "builtin".into(),
            });
            if let Err(message) = result {
                return HookDecision::Deny(message);
            }
        }
        let hooks = self.matching(event, &payload).cloned().collect::<Vec<_>>();
        for hook in &hooks {
            let started = std::time::Instant::now();
            let result = invoke(hook, event, &payload, &self.project_root);
            match (hook.kind, result) {
                (HookKind::Gate, Ok(result))
                    if result.get("decision").and_then(Value::as_str) == Some("deny") =>
                {
                    self.audit(
                        hook,
                        event,
                        HookAuditOutcome::Denied,
                        started.elapsed(),
                        false,
                    );
                    return HookDecision::Deny(
                        result
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("blocked by hook")
                            .to_owned(),
                    );
                }
                (HookKind::Gate, Ok(_)) => {
                    self.audit(
                        hook,
                        event,
                        HookAuditOutcome::Allowed,
                        started.elapsed(),
                        false,
                    );
                }
                (HookKind::Transform, Ok(result)) => {
                    match result
                        .get("arguments")
                        .filter(|arguments| arguments.is_object())
                    {
                        Some(arguments) if transform_is_valid(event, &payload, &result) => {
                            payload["arguments"] = arguments.clone();
                            self.audit(
                                hook,
                                event,
                                HookAuditOutcome::Succeeded,
                                started.elapsed(),
                                false,
                            );
                        }
                        _ => self.audit(
                            hook,
                            event,
                            HookAuditOutcome::Failed,
                            started.elapsed(),
                            false,
                        ),
                    }
                }
                (HookKind::Gate, Err(error)) => {
                    self.audit(
                        hook,
                        event,
                        if error == "timed out" {
                            HookAuditOutcome::TimedOut
                        } else {
                            HookAuditOutcome::Failed
                        },
                        started.elapsed(),
                        error == "output exceeds limit",
                    );
                    return HookDecision::Deny(format!("hook {} failed: {error}", hook.id));
                }
                (HookKind::Transform, Err(error)) => {
                    self.audit(
                        hook,
                        event,
                        if error == "timed out" {
                            HookAuditOutcome::TimedOut
                        } else {
                            HookAuditOutcome::Failed
                        },
                        started.elapsed(),
                        error == "output exceeds limit",
                    );
                }
                (HookKind::Observe, _) => {}
            }
        }
        HookDecision::Continue(payload)
    }

    pub(crate) fn observe(&self, event: HookEvent, payload: Value) {
        if self.observe_stopping.load(Ordering::Acquire) {
            return;
        }
        let hooks = self
            .matching(event, &payload)
            .filter(|hook| matches!(hook.kind, HookKind::Observe))
            .cloned()
            .collect::<Vec<_>>();
        for hook in &hooks {
            let task = ObserveTask {
                hook: hook.clone(),
                event,
                payload: payload.clone(),
            };
            if self.observe_sender.try_send(task).is_err() {
                self.audit(hook, event, HookAuditOutcome::Failed, Duration::ZERO, false);
            }
        }
    }

    pub(crate) fn stop_observers(&self) {
        self.observe_stopping.store(true, Ordering::Release);
    }

    pub(crate) fn finalize(&self, event: HookEvent, payload: Value) {
        let hooks = self
            .matching(event, &payload)
            .filter(|hook| matches!(hook.kind, HookKind::Observe))
            .cloned()
            .collect::<Vec<_>>();
        let finalize_started = std::time::Instant::now();
        for hook in &hooks {
            let Some(remaining) = FINALIZE_BUDGET.checked_sub(finalize_started.elapsed()) else {
                self.audit(
                    hook,
                    event,
                    HookAuditOutcome::TimedOut,
                    Duration::ZERO,
                    false,
                );
                continue;
            };
            let mut bounded_hook = hook.clone();
            bounded_hook.timeout_ms = Some(
                bounded_hook
                    .timeout_ms
                    .unwrap_or(DEFAULT_TIMEOUT.as_millis() as u64)
                    .min(remaining.as_millis().max(1) as u64),
            );
            self.observe_once(&bounded_hook, event, &payload, &self.project_root);
        }
    }

    fn observe_once(
        &self,
        hook: &HookDefinition,
        event: HookEvent,
        payload: &Value,
        project_root: &Path,
    ) {
        observe_once(
            &self.audit_sender,
            &self.revision,
            hook,
            event,
            payload,
            project_root,
        );
    }

    fn audit(
        &self,
        hook: &HookDefinition,
        event: HookEvent,
        outcome: HookAuditOutcome,
        duration: Duration,
        output_truncated: bool,
    ) {
        let _ = self.audit_sender.try_send(HookAuditRecord {
            hook_id: hook.id.clone(),
            event,
            outcome,
            duration_ms: duration.as_millis().min(u128::from(u64::MAX)) as u64,
            output_truncated,
            revision: self.revision.clone(),
        });
    }

    fn matching(&self, event: HookEvent, payload: &Value) -> impl Iterator<Item = &HookDefinition> {
        self.hooks.iter().filter(move |hook| {
            hook.event == event
                && (hook.matcher.tools.is_empty()
                    || payload
                        .get("tool")
                        .and_then(Value::as_str)
                        .is_some_and(|tool| {
                            hook.matcher.tools.iter().any(|candidate| candidate == tool)
                        }))
                && (hook.matcher.agent_levels.is_empty()
                    || payload
                        .get("agent_level")
                        .and_then(Value::as_u64)
                        .is_some_and(|level| hook.matcher.agent_levels.contains(&(level as u8))))
        })
    }
}

fn transform_is_valid(event: HookEvent, payload: &Value, result: &Value) -> bool {
    let response_contains_only_arguments = result
        .as_object()
        .is_some_and(|response| response.keys().all(|key| key == "arguments"));
    result.get("arguments").is_some_and(|arguments| {
        response_contains_only_arguments
            && validate_transform(event, &payload["arguments"], arguments).is_ok()
    })
}

fn validate_transform(event: HookEvent, original: &Value, transformed: &Value) -> Result<(), ()> {
    let original = original.as_object().ok_or(())?;
    let transformed = transformed.as_object().ok_or(())?;
    match event {
        HookEvent::ToolDispatching => Ok(()),
        HookEvent::MessageSending => {
            let mut original = original.clone();
            let mut transformed = transformed.clone();
            let message_valid = transformed_message_is_valid(transformed.get("message"));
            original.remove("message");
            transformed.remove("message");
            (original == transformed && message_valid)
                .then_some(())
                .ok_or(())
        }
        HookEvent::AgentSpawning => validate_spawn_transform(original, transformed),
        _ => Err(()),
    }
}

fn transformed_message_is_valid(message: Option<&Value>) -> bool {
    message.and_then(Value::as_str).is_some_and(|message| {
        !message.trim().is_empty() && message.len() <= crate::MAX_MESSAGE_BYTES
    })
}

fn validate_spawn_transform(
    original: &serde_json::Map<String, Value>,
    transformed: &serde_json::Map<String, Value>,
) -> Result<(), ()> {
    const PERMISSION_FIELDS: &[&str] = &["permission_level", "enabled_tools", "trusted_extensions"];
    for key in original.keys().chain(transformed.keys()) {
        if !PERMISSION_FIELDS.contains(&key.as_str()) && original.get(key) != transformed.get(key) {
            return Err(());
        }
    }
    if original.get("permission_level") != transformed.get("permission_level") {
        let requested = transformed
            .get("permission_level")
            .and_then(Value::as_str)
            .and_then(permission_rank)
            .ok_or(())?;
        let original = original
            .get("permission_level")
            .and_then(Value::as_str)
            .and_then(permission_rank)
            .unwrap_or(0);
        if requested > original {
            return Err(());
        }
    }
    for field in ["enabled_tools", "trusted_extensions"] {
        if original.get(field) == transformed.get(field) {
            continue;
        }
        let original = string_set(original.get(field)).ok_or(())?;
        let transformed = string_set(transformed.get(field)).ok_or(())?;
        if transformed.is_empty() || !transformed.is_subset(&original) {
            return Err(());
        }
    }
    Ok(())
}

fn permission_rank(value: &str) -> Option<u8> {
    match value {
        "read_only" => Some(1),
        "controlled" => Some(2),
        "full" => Some(3),
        _ => None,
    }
}

fn string_set(value: Option<&Value>) -> Option<std::collections::HashSet<&str>> {
    value?.as_array()?.iter().map(Value::as_str).collect()
}

fn observe_once(
    audit_sender: &mpsc::SyncSender<HookAuditRecord>,
    revision: &str,
    hook: &HookDefinition,
    event: HookEvent,
    payload: &Value,
    project_root: &Path,
) {
    let started = std::time::Instant::now();
    let (outcome, truncated) = match invoke(hook, event, payload, project_root) {
        Ok(_) => (HookAuditOutcome::Succeeded, false),
        Err(error) if error == "timed out" => (HookAuditOutcome::TimedOut, false),
        Err(error) => (HookAuditOutcome::Failed, error == "output exceeds limit"),
    };
    let _ = audit_sender.try_send(HookAuditRecord {
        hook_id: hook.id.clone(),
        event,
        outcome,
        duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
        output_truncated: truncated,
        revision: revision.to_owned(),
    });
}

fn invoke(
    hook: &HookDefinition,
    event: HookEvent,
    payload: &Value,
    project_root: &Path,
) -> Result<Value, String> {
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let (program, arguments) = hook
        .command
        .split_first()
        .ok_or_else(|| "command is empty".to_owned())?;
    let program = Path::new(program);
    if !program.is_file() {
        return Err("command is not an existing file".into());
    }
    let approved_entrypoint = if let Some(expected) = hook.entrypoint_fingerprint.as_deref() {
        let content = std::fs::read(program).map_err(|error| error.to_string())?;
        let actual = Sha256::digest(&content)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        if actual != expected {
            return Err("approved command entrypoint changed".into());
        }
        let permissions = std::fs::metadata(program)
            .map_err(|error| error.to_string())?
            .permissions();
        Some((content, permissions))
    } else {
        None
    };
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return Err("sandbox-exec is unavailable".into());
    }
    let temporary_directory =
        std::env::temp_dir().join(format!("pi-whim-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temporary_directory).map_err(|error| error.to_string())?;
    let execution_program = if let Some((content, permissions)) = approved_entrypoint {
        let staged = temporary_directory.join("approved-entrypoint");
        if let Err(error) = std::fs::write(&staged, content)
            .and_then(|()| std::fs::set_permissions(&staged, permissions))
        {
            let _ = std::fs::remove_dir_all(&temporary_directory);
            return Err(error.to_string());
        }
        staged
    } else {
        program.to_path_buf()
    };
    let profile = sandbox_profile(&project_root, &temporary_directory, program);
    let mut child = match Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile])
        .arg(&execution_program)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", &temporary_directory)
        .current_dir(&project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&temporary_directory);
            return Err(error.to_string());
        }
    };
    let request = json!({
        "version": 1,
        "hook_id": hook.id,
        "event": event,
        "entrypoint": program,
        "project_root": project_root,
        "payload": payload
    });
    let result = (|| {
        child
            .stdin
            .take()
            .ok_or_else(|| "hook stdin unavailable".to_owned())?
            .write_all(format!("{}\n", request).as_bytes())
            .map_err(|error| error.to_string())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "hook stdout unavailable".to_owned())?;
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let mut output = String::new();
            let result = BufReader::new(stdout)
                .take((MAX_HOOK_OUTPUT + 1) as u64)
                .read_to_string(&mut output)
                .map(|_| output)
                .map_err(|error| format!("failed to read hook stdout: {error}"));
            let _ = sender.send(result);
        });
        let timeout = Duration::from_millis(
            hook.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT.as_millis() as u64),
        );
        receiver
            .recv_timeout(timeout)
            .map_err(|_| "timed out".to_owned())?
    })();
    let mut status = None;
    if result.is_ok() {
        for _ in 0..10 {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(1)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&temporary_directory);
                    return Err(error.to_string());
                }
            }
        }
    }
    if status.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    let _ = std::fs::remove_dir_all(&temporary_directory);
    let line = result?;
    if line.len() > MAX_HOOK_OUTPUT {
        return Err("output exceeds limit".into());
    }
    let Some(status) = status else {
        return Err("hook did not exit after closing stdout".into());
    };
    if !status.success() {
        return Err(format!("hook exited with status {status}"));
    }
    let response = if line.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&line).map_err(|error| format!("invalid JSON response: {error}"))?
    };
    if matches!(hook.kind, HookKind::Gate) {
        validate_gate_response(&response)?;
    }
    Ok(response)
}

fn validate_gate_response(response: &Value) -> Result<(), String> {
    if response.is_null() {
        return Ok(());
    }
    let object = response
        .as_object()
        .ok_or_else(|| "gate hook response must be a JSON object".to_owned())?;
    if let Some(decision) = object.get("decision") {
        match decision.as_str() {
            Some("allow" | "deny") => {}
            _ => return Err("gate hook decision must be 'allow' or 'deny'".into()),
        }
    }
    if object
        .get("message")
        .is_some_and(|message| !message.is_string())
    {
        return Err("gate hook message must be a string".into());
    }
    Ok(())
}

fn sandbox_profile(project_root: &Path, temporary_directory: &Path, program: &Path) -> String {
    fn quoted(path: &Path) -> String {
        path.to_string_lossy().replace('"', "\\\"")
    }
    let project_root =
        std::fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let temporary_directory = std::fs::canonicalize(temporary_directory)
        .unwrap_or_else(|_| temporary_directory.to_path_buf());
    let program = std::fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let program_dir = program.parent().unwrap_or(&program);
    format!(
        "(version 1) (deny default) (import \"system.sb\") (allow process*) (allow file-read* (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/System\") (subpath \"/dev\")) (allow file-write* (subpath \"{}\"))",
        quoted(&project_root),
        quoted(program_dir),
        quoted(&program),
        quoted(&temporary_directory),
        quoted(&temporary_directory)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::HookMatcher;
    use std::os::unix::fs::PermissionsExt;

    fn write_executable(path: &Path, content: &str) {
        std::fs::write(path, content).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn non_matching_hooks_are_skipped() {
        let dispatcher = HookDispatcher::new(
            HookConfig {
                version: 1,
                revision: String::new(),
                hooks: vec![HookDefinition {
                    id: "only-bash".into(),
                    event: HookEvent::ToolDispatching,
                    kind: HookKind::Gate,
                    command: vec!["false".into()],
                    timeout_ms: None,
                    matcher: HookMatcher {
                        tools: vec!["bash".into()],
                        agent_levels: vec![],
                    },
                    entrypoint_fingerprint: None,
                }],
            },
            PathBuf::from("/tmp"),
            mpsc::sync_channel(16).0,
        );
        let payload = json!({"tool":"read", "arguments":{}});
        assert_eq!(
            dispatcher.gate(HookEvent::ToolDispatching, payload.clone()),
            HookDecision::Continue(payload)
        );
    }

    #[test]
    fn gate_hooks_can_deny_an_operation() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let (audit_sender, audit_receiver) = mpsc::sync_channel(16);
        let dispatcher = HookDispatcher::new(
            HookConfig {
                version: 1,
                revision: "sha256:test".into(),
                hooks: vec![HookDefinition {
                    id: "deny".into(),
                    event: HookEvent::ToolDispatching,
                    kind: HookKind::Gate,
                    command: vec![
                        "/bin/echo".into(),
                        r#"{"decision":"deny","message":"policy"}"#.into(),
                    ],
                    timeout_ms: Some(1_000),
                    matcher: HookMatcher::default(),
                    entrypoint_fingerprint: None,
                }],
            },
            PathBuf::from("/tmp"),
            audit_sender,
        );
        assert_eq!(
            dispatcher.gate(
                HookEvent::ToolDispatching,
                json!({"tool":"bash", "agent_level": 0, "arguments":{}})
            ),
            HookDecision::Deny("policy".into())
        );
        let _builtin = audit_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        let audit = audit_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(audit.outcome, HookAuditOutcome::Denied);
        assert_eq!(audit.revision, "sha256:test");
    }

    #[test]
    fn failed_transform_preserves_the_original_arguments() {
        let (audit_sender, audit_receiver) = mpsc::sync_channel(16);
        let dispatcher = HookDispatcher::new(
            HookConfig {
                version: 1,
                revision: "sha256:test".into(),
                hooks: vec![HookDefinition {
                    id: "transform".into(),
                    event: HookEvent::ToolDispatching,
                    kind: HookKind::Transform,
                    command: vec!["/missing/hook".into()],
                    timeout_ms: None,
                    matcher: HookMatcher::default(),
                    entrypoint_fingerprint: None,
                }],
            },
            PathBuf::from("/tmp"),
            audit_sender,
        );
        let payload = json!({"tool":"bash", "arguments":{"command":"pwd"}});
        assert_eq!(
            dispatcher.gate(HookEvent::ToolDispatching, payload.clone()),
            HookDecision::Continue(payload)
        );
        let _builtin = audit_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(
            audit_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .outcome,
            HookAuditOutcome::Failed
        );
    }

    #[test]
    fn changed_approved_entrypoint_fails_a_gate_closed() {
        let dispatcher = HookDispatcher::new(
            HookConfig {
                version: 1,
                revision: "sha256:test".into(),
                hooks: vec![HookDefinition {
                    id: "approved".into(),
                    event: HookEvent::ToolDispatching,
                    kind: HookKind::Gate,
                    command: vec!["/usr/bin/true".into()],
                    timeout_ms: None,
                    matcher: HookMatcher::default(),
                    entrypoint_fingerprint: Some("00".repeat(32)),
                }],
            },
            PathBuf::from("/tmp"),
            mpsc::sync_channel(16).0,
        );
        assert!(matches!(
            dispatcher.gate(
                HookEvent::ToolDispatching,
                json!({"tool":"bash", "arguments":{}})
            ),
            HookDecision::Deny(message) if message.contains("entrypoint changed")
        ));
    }

    #[test]
    fn nonzero_hook_exit_is_a_failure() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let hook = HookDefinition {
            id: "failure".into(),
            event: HookEvent::ToolDispatching,
            kind: HookKind::Gate,
            command: vec!["/usr/bin/false".into()],
            timeout_ms: Some(1_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: None,
        };
        assert!(
            invoke(
                &hook,
                HookEvent::ToolDispatching,
                &json!({"arguments":{}}),
                Path::new("/tmp")
            )
            .unwrap_err()
            .contains("exited with status")
        );
    }

    #[test]
    fn invalid_utf8_stdout_is_a_hook_failure() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let hook = HookDefinition {
            id: "invalid-utf8".into(),
            event: HookEvent::ToolDispatching,
            kind: HookKind::Gate,
            command: vec!["/usr/bin/printf".into(), "\\377".into()],
            timeout_ms: Some(1_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: None,
        };
        assert!(
            invoke(
                &hook,
                HookEvent::ToolDispatching,
                &json!({"arguments":{}}),
                Path::new("/tmp")
            )
            .unwrap_err()
            .starts_with("failed to read hook stdout")
        );
    }

    #[test]
    fn gate_responses_reject_ambiguous_decisions() {
        assert!(validate_gate_response(&Value::Null).is_ok());
        assert!(validate_gate_response(&json!({})).is_ok());
        assert!(validate_gate_response(&json!({"decision":"allow"})).is_ok());
        assert!(validate_gate_response(&json!({"decision":"deny"})).is_ok());
        assert!(validate_gate_response(&json!({"decision":"unknown"})).is_err());
        assert!(validate_gate_response(&json!([])).is_err());
        assert!(validate_gate_response(&json!({"message": 42})).is_err());
    }

    #[test]
    fn malformed_gate_decisions_fail_closed() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let dispatcher = HookDispatcher::new(
            HookConfig {
                version: 1,
                revision: "sha256:test".into(),
                hooks: vec![HookDefinition {
                    id: "ambiguous".into(),
                    event: HookEvent::ToolDispatching,
                    kind: HookKind::Gate,
                    command: vec!["/bin/echo".into(), r#"{"decision":"maybe"}"#.into()],
                    timeout_ms: Some(1_000),
                    matcher: HookMatcher::default(),
                    entrypoint_fingerprint: None,
                }],
            },
            PathBuf::from("/tmp"),
            mpsc::sync_channel(16).0,
        );
        assert!(matches!(
            dispatcher.gate(
                HookEvent::ToolDispatching,
                json!({"tool":"bash", "arguments":{}})
            ),
            HookDecision::Deny(message) if message.contains("decision must be")
        ));
    }

    #[test]
    fn sandbox_allows_project_reads_but_denies_project_writes() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let blocked = directory.path().join("blocked");
        let script = directory.path().join("hook.sh");
        write_executable(
            &script,
            &format!(
                "#!/bin/sh\necho denied > \"{}\"\nprintf '{{}}\\n'\n",
                blocked.display()
            ),
        );

        let hook = HookDefinition {
            id: "sandbox-write".into(),
            event: HookEvent::ToolDispatching,
            kind: HookKind::Gate,
            command: vec![script.to_string_lossy().into_owned()],
            timeout_ms: Some(5_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: None,
        };
        assert_eq!(
            invoke(
                &hook,
                HookEvent::ToolDispatching,
                &json!({"arguments":{}}),
                directory.path()
            )
            .unwrap(),
            json!({})
        );
        assert!(!blocked.exists());

        let profile = sandbox_profile(directory.path(), directory.path(), &script);
        assert!(!profile.contains("network"));
    }

    #[test]
    fn hook_envelope_identifies_hook_entrypoint_and_project() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let project_root = std::fs::canonicalize(directory.path()).unwrap();
        let script = directory.path().join("envelope.sh");
        write_executable(
            &script,
            r#"#!/bin/sh
read request
[ "$PWD" = "$1" ] || exit 10
printf '%s' "$request" | /usr/bin/grep -F '"hook_id":"envelope"' >/dev/null || exit 11
printf '%s' "$request" | /usr/bin/grep -F '"entrypoint":' >/dev/null || exit 12
printf '%s' "$request" | /usr/bin/grep -F '"project_root":' >/dev/null || exit 13
printf '{}\n'
"#,
        );
        let hook = HookDefinition {
            id: "envelope".into(),
            event: HookEvent::ToolDispatching,
            kind: HookKind::Gate,
            command: vec![
                script.to_string_lossy().into_owned(),
                project_root.to_string_lossy().into_owned(),
            ],
            timeout_ms: Some(5_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: None,
        };

        assert_eq!(
            invoke(
                &hook,
                HookEvent::ToolDispatching,
                &json!({"arguments":{}}),
                &project_root
            )
            .unwrap(),
            json!({})
        );
    }

    #[test]
    fn approved_entrypoints_execute_from_the_verified_snapshot() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("approved.sh");
        let content = r#"#!/bin/sh
case "$0" in
  */pi-whim-hook-*/approved-entrypoint) printf '{"decision":"allow"}\n' ;;
  *) exit 12 ;;
esac
"#;
        write_executable(&script, content);
        let fingerprint = Sha256::digest(content.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let hook = HookDefinition {
            id: "approved".into(),
            event: HookEvent::ToolDispatching,
            kind: HookKind::Gate,
            command: vec![script.to_string_lossy().into_owned()],
            timeout_ms: Some(1_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: Some(fingerprint),
        };

        assert_eq!(
            invoke(
                &hook,
                HookEvent::ToolDispatching,
                &json!({"arguments":{}}),
                directory.path()
            )
            .unwrap(),
            json!({"decision":"allow"})
        );
    }

    fn test_dispatcher(project_root: &Path, hooks: Vec<HookDefinition>) -> HookDispatcher {
        HookDispatcher::new(
            HookConfig {
                version: 1,
                revision: "sha256:test".into(),
                hooks,
            },
            project_root.to_path_buf(),
            mpsc::sync_channel(64).0,
        )
    }

    fn test_hook(
        id: &str,
        event: HookEvent,
        kind: HookKind,
        command: Vec<String>,
    ) -> HookDefinition {
        HookDefinition {
            id: id.into(),
            event,
            kind,
            command,
            timeout_ms: Some(1_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: None,
        }
    }

    #[test]
    fn gate_hook_failures_deny_operations_closed() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let payload = json!({"tool":"bash", "agent_level":0, "arguments":{}});
        let cases = [
            test_hook(
                "nonzero",
                HookEvent::ToolDispatching,
                HookKind::Gate,
                vec!["/usr/bin/false".into()],
            ),
            HookDefinition {
                timeout_ms: Some(1),
                ..test_hook(
                    "timeout",
                    HookEvent::ToolDispatching,
                    HookKind::Gate,
                    vec!["/bin/sleep".into(), "1".into()],
                )
            },
            test_hook(
                "invalid-json",
                HookEvent::ToolDispatching,
                HookKind::Gate,
                vec!["/bin/echo".into(), "not json".into()],
            ),
            test_hook(
                "oversized-output",
                HookEvent::ToolDispatching,
                HookKind::Gate,
                vec!["/usr/bin/printf".into(), "%65537s".into(), "".into()],
            ),
        ];

        for hook in cases {
            assert!(matches!(
                test_dispatcher(directory.path(), vec![hook])
                    .gate(HookEvent::ToolDispatching, payload.clone()),
                HookDecision::Deny(_)
            ));
        }
    }

    #[test]
    fn transform_responses_preserve_arguments_when_they_violate_the_contract() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();

        for response in ["[]", r#"{"arguments":"string"}"#] {
            let payload = json!({"tool":"bash", "arguments":{"command":"pwd"}});
            let dispatcher = test_dispatcher(
                directory.path(),
                vec![test_hook(
                    "invalid-tool-transform",
                    HookEvent::ToolDispatching,
                    HookKind::Transform,
                    vec!["/bin/echo".into(), response.into()],
                )],
            );
            assert_eq!(
                dispatcher.gate(HookEvent::ToolDispatching, payload.clone()),
                HookDecision::Continue(payload)
            );
        }

        let payload = json!({"tool":"bash", "arguments":{"command":"pwd"}});
        let dispatcher = test_dispatcher(
            directory.path(),
            vec![test_hook(
                "tool-envelope-is-immutable",
                HookEvent::ToolDispatching,
                HookKind::Transform,
                vec!["/bin/echo".into(), r#"{"tool":"write"}"#.into()],
            )],
        );
        assert_eq!(
            dispatcher.gate(HookEvent::ToolDispatching, payload.clone()),
            HookDecision::Continue(payload)
        );

        let payload =
            json!({"tool":"send_message", "arguments":{"target":"child","message":"old"}});
        let dispatcher = test_dispatcher(
            directory.path(),
            vec![test_hook(
                "changed-target",
                HookEvent::MessageSending,
                HookKind::Transform,
                vec![
                    "/bin/echo".into(),
                    r#"{"arguments":{"target":"other","message":"new"}}"#.into(),
                ],
            )],
        );
        assert_eq!(
            dispatcher.gate(HookEvent::MessageSending, payload.clone()),
            HookDecision::Continue(payload)
        );
    }

    #[test]
    fn matchers_use_protocol_tool_names_levels_and_empty_wildcards() {
        let directory = tempfile::tempdir().unwrap();
        let deny_bash = test_hook(
            "bash-only",
            HookEvent::ToolDispatching,
            HookKind::Gate,
            vec!["/usr/bin/false".into()],
        );
        let dispatcher = test_dispatcher(
            directory.path(),
            vec![HookDefinition {
                matcher: HookMatcher {
                    tools: vec!["bash".into()],
                    agent_levels: vec![2],
                },
                ..deny_bash
            }],
        );
        let payload = json!({"tool":"bash_command", "agent_level":2, "arguments":{}});
        assert_eq!(
            dispatcher.gate(HookEvent::ToolDispatching, payload.clone()),
            HookDecision::Continue(payload)
        );
        let payload = json!({"tool":"bash", "agent_level":1, "arguments":{}});
        assert_eq!(
            dispatcher.gate(HookEvent::ToolDispatching, payload.clone()),
            HookDecision::Continue(payload)
        );

        // A matching tool and level reaches the hook; its missing executable fails closed.
        assert!(matches!(
            dispatcher.gate(
                HookEvent::ToolDispatching,
                json!({"tool":"bash", "agent_level":2, "arguments":{}})
            ),
            HookDecision::Deny(_)
        ));

        let dispatcher = test_dispatcher(
            directory.path(),
            vec![test_hook(
                "all-events",
                HookEvent::ToolDispatching,
                HookKind::Gate,
                vec!["/missing/hook".into()],
            )],
        );
        assert!(matches!(
            dispatcher.gate(
                HookEvent::ToolDispatching,
                json!({"tool":"read", "agent_level":0, "arguments":{}})
            ),
            HookDecision::Deny(_)
        ));
    }

    #[test]
    fn builtin_safety_floor_rejects_invalid_spawn_and_messages() {
        let directory = tempfile::tempdir().unwrap();
        let dispatcher = test_dispatcher(directory.path(), Vec::new());
        assert!(matches!(
            dispatcher.gate(
                HookEvent::AgentSpawning,
                json!({"arguments":{"task":"work"}})
            ),
            HookDecision::Deny(_)
        ));
        assert!(matches!(
            dispatcher.gate(
                HookEvent::MessageSending,
                json!({"arguments":{"message":"  "}})
            ),
            HookDecision::Deny(_)
        ));
        assert!(matches!(
            dispatcher.gate(
                HookEvent::MessageSending,
                json!({"arguments":{"message":"x".repeat(crate::MAX_MESSAGE_BYTES + 1)}})
            ),
            HookDecision::Deny(_)
        ));
    }

    #[test]
    fn hook_config_validation_rejects_security_boundaries() {
        let _directory = tempfile::tempdir().unwrap();
        let valid = || {
            test_hook(
                "valid",
                HookEvent::ToolDispatching,
                HookKind::Gate,
                vec!["/usr/bin/true".into()],
            )
        };

        let mut config = HookConfig {
            version: 2,
            revision: String::new(),
            hooks: vec![valid()],
        };
        assert!(config.validate().is_err());
        config.version = 1;
        config.hooks = (0..65)
            .map(|index| {
                test_hook(
                    &format!("hook-{index}"),
                    HookEvent::ToolDispatching,
                    HookKind::Gate,
                    vec!["/usr/bin/true".into()],
                )
            })
            .collect();
        assert!(config.validate().is_err());
        config.hooks = vec![test_hook(
            "non-ascii-ä",
            HookEvent::ToolDispatching,
            HookKind::Gate,
            vec!["/usr/bin/true".into()],
        )];
        assert!(config.validate().is_err());
        config.hooks = vec![test_hook(
            "relative",
            HookEvent::ToolDispatching,
            HookKind::Gate,
            vec!["echo".into()],
        )];
        assert!(config.validate().is_err());
        config.hooks = vec![HookDefinition {
            timeout_ms: Some(30_001),
            ..valid()
        }];
        assert!(config.validate().is_err());
    }

    #[test]
    fn event_specific_transforms_cannot_widen_their_contract() {
        assert!(transform_is_valid(
            HookEvent::MessageSending,
            &json!({"arguments":{"target":"child", "message":"old"}}),
            &json!({"arguments":{"target":"child", "message":"new"}})
        ));
        assert!(!transform_is_valid(
            HookEvent::MessageSending,
            &json!({"arguments":{"target":"child", "message":"old"}}),
            &json!({"arguments":{"target":"other", "message":"new"}})
        ));
        assert!(
            validate_transform(
                HookEvent::AgentSpawning,
                &json!({
                    "name":"worker",
                    "task":"task",
                    "permission_level":"controlled",
                    "enabled_tools":["read", "bash"]
                }),
                &json!({
                    "name":"worker",
                    "task":"task",
                    "permission_level":"read_only",
                    "enabled_tools":["read"]
                })
            )
            .is_ok()
        );
        assert!(
            validate_transform(
                HookEvent::AgentSpawning,
                &json!({"name":"worker", "task":"task", "permission_level":"controlled"}),
                &json!({"name":"worker", "task":"changed", "permission_level":"read_only"})
            )
            .is_err()
        );
    }
}
