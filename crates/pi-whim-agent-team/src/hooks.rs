//! Host-side Hook dispatcher for supervisor control-plane events.

use std::{
    io::{BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use pi_whim_core::{
    HookAuditOutcome, HookAuditRecord, HookConfig, HookDefinition, HookEvent, HookKind,
};
use serde_json::{Value, json};

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HOOK_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct HookDispatcher {
    hooks: Vec<HookDefinition>,
    project_root: PathBuf,
    audit_sender: mpsc::Sender<HookAuditRecord>,
    revision: String,
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
        audit_sender: mpsc::Sender<HookAuditRecord>,
    ) -> Self {
        Self {
            revision: config.revision.clone(),
            hooks: config.hooks,
            project_root,
            audit_sender,
        }
    }

    pub(crate) fn gate(&self, event: HookEvent, payload: Value) -> HookDecision {
        let mut payload = payload;
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
                    self.audit(
                        hook,
                        event,
                        HookAuditOutcome::Succeeded,
                        started.elapsed(),
                        false,
                    );
                    if let Some(arguments) = result.get("arguments") {
                        payload["arguments"] = arguments.clone();
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
        let hooks = self
            .matching(event, &payload)
            .filter(|hook| matches!(hook.kind, HookKind::Observe))
            .cloned()
            .collect::<Vec<_>>();
        let project_root = self.project_root.clone();
        let dispatcher = self.clone();
        for hook in &hooks {
            let hook = hook.clone();
            let payload = payload.clone();
            let project_root = project_root.clone();
            let dispatcher = dispatcher.clone();
            thread::spawn(move || {
                let started = std::time::Instant::now();
                let (outcome, truncated) = match invoke(&hook, event, &payload, &project_root) {
                    Ok(_) => (HookAuditOutcome::Succeeded, false),
                    Err(error) if error == "timed out" => (HookAuditOutcome::TimedOut, false),
                    Err(error) => (HookAuditOutcome::Failed, error == "output exceeds limit"),
                };
                dispatcher.audit(&hook, event, outcome, started.elapsed(), truncated);
            });
        }
    }

    fn audit(
        &self,
        hook: &HookDefinition,
        event: HookEvent,
        outcome: HookAuditOutcome,
        duration: Duration,
        output_truncated: bool,
    ) {
        let _ = self.audit_sender.send(HookAuditRecord {
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

fn invoke(
    hook: &HookDefinition,
    event: HookEvent,
    payload: &Value,
    project_root: &Path,
) -> Result<Value, String> {
    let (program, arguments) = hook
        .command
        .split_first()
        .ok_or_else(|| "command is empty".to_owned())?;
    let program = Path::new(program);
    if !program.is_file() {
        return Err("command is not an existing file".into());
    }
    if !Path::new("/usr/bin/sandbox-exec").is_file() {
        return Err("sandbox-exec is unavailable".into());
    }
    let temporary_directory =
        std::env::temp_dir().join(format!("pi-whim-hook-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&temporary_directory).map_err(|error| error.to_string())?;
    let profile = sandbox_profile(project_root, &temporary_directory, program);
    let mut child = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", &profile])
        .arg(program)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", &temporary_directory)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let request = json!({"version": 1, "event": event, "payload": payload});
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
            let _ = BufReader::new(stdout)
                .take((MAX_HOOK_OUTPUT + 1) as u64)
                .read_to_string(&mut output);
            let _ = sender.send(output);
        });
        let timeout = Duration::from_millis(
            hook.timeout_ms
                .unwrap_or(DEFAULT_TIMEOUT.as_millis() as u64),
        );
        receiver
            .recv_timeout(timeout)
            .map_err(|_| "timed out".to_owned())
    })();
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&temporary_directory);
    let line = result?;
    if line.len() > MAX_HOOK_OUTPUT {
        return Err("output exceeds limit".into());
    }
    if line.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&line).map_err(|error| format!("invalid JSON response: {error}"))
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
        "(version 1) (deny default) (import \"system.sb\") (allow process*) (allow file-read* (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/System\") (subpath \"/dev\")) (allow file-write* (subpath \"{}\"))",
        quoted(&project_root),
        quoted(program_dir),
        quoted(&program),
        quoted(&temporary_directory)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::HookMatcher;

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
                }],
            },
            PathBuf::from("/tmp"),
            mpsc::channel().0,
        );
        assert_eq!(
            dispatcher.gate(HookEvent::ToolDispatching, json!({"tool":"read"})),
            HookDecision::Continue(json!({"tool":"read"}))
        );
    }

    #[test]
    fn gate_hooks_can_deny_an_operation() {
        if !Path::new("/usr/bin/sandbox-exec").is_file() {
            return;
        }
        let (audit_sender, audit_receiver) = mpsc::channel();
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
                }],
            },
            PathBuf::from("/tmp"),
            audit_sender,
        );
        assert_eq!(
            dispatcher.gate(
                HookEvent::ToolDispatching,
                json!({"tool":"bash", "agent_level": 0})
            ),
            HookDecision::Deny("policy".into())
        );
        let audit = audit_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(audit.outcome, HookAuditOutcome::Denied);
        assert_eq!(audit.revision, "sha256:test");
    }

    #[test]
    fn failed_transform_preserves_the_original_arguments() {
        let (audit_sender, audit_receiver) = mpsc::channel();
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
        assert_eq!(
            audit_receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .outcome,
            HookAuditOutcome::Failed
        );
    }
}
