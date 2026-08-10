//! Sandboxed one-shot execution shared by v1 compatibility and observe workers.

use crate::manifest::{HookDefinition, MAX_STDOUT_BYTES};
use crate::{HookHostError, HookHostResult};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const FINALIZE_BUDGET: Duration = Duration::from_secs(5);
const STDERR_LIMIT: usize = 8 * 1024;

/// Executes one v1-compatible request in a fresh sandboxed process.
pub(crate) fn invoke_one_shot(
    definition: &HookDefinition,
    event: &str,
    payload: &Value,
    project_root: &Path,
) -> HookHostResult<Value> {
    let project_root = fs::canonicalize(project_root).map_err(HookHostError::io)?;
    let program = definition
        .command
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| HookHostError::InvalidManifest("command is empty".to_owned()))?;
    if !program.is_file() {
        return Err(HookHostError::process("command executable is not a file"));
    }
    let temporary_directory = make_temp_directory()?;
    let execution_program = prepare_execution_program(
        &program,
        definition.entrypoint_fingerprint.as_deref(),
        &temporary_directory,
    )?;
    let profile = sandbox_profile(
        &project_root,
        &temporary_directory,
        &program,
        &execution_program,
    );
    let sandbox = sandbox_executable().ok_or(HookHostError::SandboxUnavailable);
    let child = match sandbox {
        Ok(sandbox) => spawn_child(
            sandbox,
            &profile,
            &execution_program,
            &definition.command[1..],
            &project_root,
            &temporary_directory,
        ),
        Err(error) => Err(error),
    };
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary_directory);
            return Err(error);
        }
    };
    let request = json!({
        "version": 1,
        "hook_id": definition.id,
        "event": event,
        "entrypoint": program,
        "project_root": project_root,
        "payload": payload,
    });
    let mut child = OneShotChild::new(child);
    let result = read_one_shot_response(&mut child, &request, definition.effective_timeout());
    drop(child);
    let _ = fs::remove_dir_all(&temporary_directory);
    result
}

fn spawn_child(
    sandbox: &str,
    profile: &str,
    execution_program: &Path,
    arguments: &[String],
    project_root: &Path,
    temporary_directory: &Path,
) -> HookHostResult<Child> {
    Command::new(sandbox)
        .args(["-p", profile])
        .arg(execution_program)
        .args(arguments)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("TMPDIR", temporary_directory)
        .current_dir(project_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(HookHostError::io)
}

struct OneShotChild {
    child: Child,
    reaped: bool,
}

impl OneShotChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> HookHostResult<ExitStatus> {
        let started = Instant::now();
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.reaped = true;
                    return Ok(status);
                }
                Ok(None) if started.elapsed() < timeout => {
                    thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => {
                    return Err(HookHostError::Timeout {
                        hook_id: "one-shot".to_owned(),
                    });
                }
                Err(error) => return Err(HookHostError::io(error)),
            }
        }
    }
}

impl Drop for OneShotChild {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let started = Instant::now();
        while started.elapsed() < FINALIZE_BUDGET {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.reaped = true;
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
    }
}

fn read_one_shot_response(
    child: &mut OneShotChild,
    request: &Value,
    timeout: Duration,
) -> HookHostResult<Value> {
    let mut stdin = child
        .child
        .stdin
        .take()
        .ok_or_else(|| HookHostError::process("hook stdin unavailable"))?;
    let line =
        serde_json::to_vec(request).map_err(|error| HookHostError::Json(error.to_string()))?;
    let input_result = stdin
        .write_all(&line)
        .and_then(|()| stdin.write_all(b"\n"))
        .map_err(HookHostError::io);
    drop(stdin);

    let stdout = child
        .child
        .stdout
        .take()
        .ok_or_else(|| HookHostError::process("hook stdout unavailable"))?;
    let stderr = child
        .child
        .stderr
        .take()
        .ok_or_else(|| HookHostError::process("hook stderr unavailable"))?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("pi-whim-hook-v1-stdout".to_owned())
        .spawn(move || {
            let result = read_limited(stdout, MAX_STDOUT_BYTES + 1)
                .map_err(HookHostError::io)
                .and_then(|bytes| {
                    if bytes.len() > MAX_STDOUT_BYTES {
                        Err(HookHostError::InvalidInvocation(
                            "hook output exceeds 64 KiB".to_owned(),
                        ))
                    } else {
                        Ok(bytes)
                    }
                });
            let _ = stdout_sender.send(result);
        })
        .map_err(HookHostError::io)?;
    thread::Builder::new()
        .name("pi-whim-hook-v1-stderr".to_owned())
        .spawn(move || {
            let result = read_limited(stderr, STDERR_LIMIT).map_err(HookHostError::io);
            let _ = stderr_sender.send(result);
        })
        .map_err(HookHostError::io)?;

    let started = Instant::now();
    let status = child.wait_for_exit(timeout)?;
    if !status.success() {
        return Err(HookHostError::process(format!(
            "hook exited with status {status}"
        )));
    }
    input_result?;

    let remaining = timeout.saturating_sub(started.elapsed());
    let stdout_result =
        stdout_receiver
            .recv_timeout(remaining)
            .map_err(|_| HookHostError::Timeout {
                hook_id: "one-shot".to_owned(),
            })??;
    drop(stdout_receiver);
    drop(stderr_receiver);
    if stdout_result.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&stdout_result)
            .map_err(|error| HookHostError::Json(error.to_string()))
    }
}

fn read_limited<R: Read>(mut reader: R, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.extend_from_slice(&buffer[..read]);
        if output.len() > limit {
            break;
        }
    }
    Ok(output)
}

pub(crate) fn make_temp_directory() -> HookHostResult<PathBuf> {
    let directory =
        std::env::temp_dir().join(format!("pi-whim-hook-{}", uuid::Uuid::new_v4().simple()));
    fs::create_dir_all(&directory).map_err(HookHostError::io)?;
    Ok(directory)
}

fn prepare_execution_program(
    program: &Path,
    expected_fingerprint: Option<&str>,
    temporary_directory: &Path,
) -> HookHostResult<PathBuf> {
    match expected_fingerprint {
        Some(expected) => {
            match snapshot_approved_entrypoint(program, expected, temporary_directory) {
                Ok(staged) => Ok(staged),
                Err(error) => {
                    let _ = fs::remove_dir_all(temporary_directory);
                    Err(error)
                }
            }
        }
        None => Ok(program.to_path_buf()),
    }
}

pub(crate) fn snapshot_approved_entrypoint(
    program: &Path,
    expected: &str,
    temporary_directory: &Path,
) -> HookHostResult<PathBuf> {
    let content = fs::read(program).map_err(HookHostError::io)?;
    let actual = Sha256::digest(&content)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(HookHostError::FingerprintMismatch);
    }
    let staged = temporary_directory.join("approved-entrypoint");
    let mut file = File::create(&staged).map_err(HookHostError::io)?;
    file.write_all(&content).map_err(HookHostError::io)?;
    let permissions = fs::metadata(program)
        .map_err(HookHostError::io)?
        .permissions();
    fs::set_permissions(&staged, permissions).map_err(HookHostError::io)?;
    Ok(staged)
}

pub(crate) fn launch_persistent(
    definition: &HookDefinition,
    project_root: &Path,
) -> HookHostResult<(Child, PathBuf)> {
    let program = definition
        .command
        .first()
        .map(PathBuf::from)
        .ok_or_else(|| HookHostError::InvalidManifest("command is empty".to_owned()))?;
    if !program.is_file() {
        return Err(HookHostError::process("command executable is not a file"));
    }
    let temporary_directory = make_temp_directory()?;
    let execution_program = prepare_execution_program(
        &program,
        definition.entrypoint_fingerprint.as_deref(),
        &temporary_directory,
    )?;
    let profile = sandbox_profile(
        project_root,
        &temporary_directory,
        &program,
        &execution_program,
    );
    let sandbox = sandbox_executable().ok_or(HookHostError::SandboxUnavailable);
    let child = match sandbox {
        Ok(sandbox) => spawn_child(
            sandbox,
            &profile,
            &execution_program,
            &definition.command[1..],
            project_root,
            &temporary_directory,
        ),
        Err(error) => Err(error),
    };
    match child {
        Ok(child) => Ok((child, temporary_directory)),
        Err(error) => {
            let _ = fs::remove_dir_all(&temporary_directory);
            Err(error)
        }
    }
}

pub(crate) fn sandbox_profile(
    project_root: &Path,
    temporary_directory: &Path,
    program: &Path,
    execution_program: &Path,
) -> String {
    fn quoted(path: &Path) -> String {
        path.to_string_lossy().replace('"', "\\\"")
    }
    let project_root =
        fs::canonicalize(project_root).unwrap_or_else(|_| project_root.to_path_buf());
    let temporary_directory =
        fs::canonicalize(temporary_directory).unwrap_or_else(|_| temporary_directory.to_path_buf());
    let program = fs::canonicalize(program).unwrap_or_else(|_| program.to_path_buf());
    let execution_program =
        fs::canonicalize(execution_program).unwrap_or_else(|_| execution_program.to_path_buf());
    let program_dir = program.parent().unwrap_or(&program);
    let execution_dir = execution_program.parent().unwrap_or(&execution_program);
    format!(
        "(version 1) (deny default) (import \"system.sb\") (allow process*) (allow file-read* (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"{}\") (subpath \"/usr\") (subpath \"/bin\") (subpath \"/sbin\") (subpath \"/System\") (subpath \"/dev\")) (allow file-write* (subpath \"{}\"))",
        quoted(&project_root),
        quoted(program_dir),
        quoted(&program),
        quoted(execution_dir),
        quoted(&execution_program),
        quoted(&temporary_directory),
        quoted(&temporary_directory),
    )
}

pub(crate) fn sandbox_executable() -> Option<&'static str> {
    if cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").is_file() {
        Some("/usr/bin/sandbox-exec")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HookManifest;

    fn invoke_test_command(command: &[&str]) -> HookHostResult<Value> {
        let manifest = HookManifest::parse_json(
            &json!({
                "version": 1,
                "hooks": [{
                    "id": "one-shot-exit",
                    "event": "tool_completed",
                    "kind": "observe",
                    "command": command,
                    "timeout_ms": 1000
                }]
            })
            .to_string(),
        )?;
        let definition = manifest.hooks.into_iter().next().ok_or_else(|| {
            HookHostError::InvalidManifest("test manifest has no hook".to_owned())
        })?;
        let project_root = make_temp_directory()?;
        let result = invoke_one_shot(
            &definition,
            "tool_completed",
            &json!({"status": "test"}),
            &project_root,
        );
        let _ = fs::remove_dir_all(project_root);
        result
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nonzero_exit_with_empty_stdout_is_a_process_failure() {
        let result = invoke_test_command(&["/usr/bin/false"]);
        assert!(matches!(result, Err(HookHostError::Process(_))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn nonzero_exit_with_json_stdout_is_still_a_process_failure() {
        let result = invoke_test_command(&[
            "/bin/sh",
            "-c",
            "cat >/dev/null; printf '%s\n' '{\"accepted\":true}'; exit 7",
        ]);
        assert!(matches!(result, Err(HookHostError::Process(_))));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn zero_exit_with_empty_stdout_remains_null() -> HookHostResult<()> {
        let result = invoke_test_command(&["/bin/sh", "-c", "cat >/dev/null; exit 0"])?;
        assert_eq!(result, Value::Null);
        Ok(())
    }

    #[test]
    fn fingerprint_failure_removes_pre_spawn_temporary_directory() -> HookHostResult<()> {
        let source_directory = make_temp_directory()?;
        let source = source_directory.join("entrypoint");
        fs::write(&source, b"approved content").map_err(HookHostError::io)?;
        let staging = make_temp_directory()?;
        let result = prepare_execution_program(
            &source,
            Some("0000000000000000000000000000000000000000000000000000000000000000"),
            &staging,
        );
        let _ = fs::remove_dir_all(&source_directory);
        assert!(matches!(result, Err(HookHostError::FingerprintMismatch)));
        assert!(!staging.exists());
        Ok(())
    }
}
