//! Strict LF JSONL client for Pi's headless RPC mode.

use std::{
    collections::HashMap,
    io::{self, BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::Arc,
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RpcError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid JSONL record: {0}")]
    InvalidFrame(&'static str),
    #[error("Pi RPC process is unavailable")]
    Unavailable,
    #[error("request timed out")]
    Timeout,
    #[error("Pi RPC request failed: {0}")]
    Response(String),
}

#[derive(Clone, Debug)]
pub struct PiLaunch {
    pub executable: String,
    pub arguments: Vec<String>,
    pub working_directory: String,
    pub environment: HashMap<String, String>,
}

impl PiLaunch {
    pub fn new(executable: impl Into<String>, working_directory: impl Into<String>) -> Self {
        Self {
            executable: executable.into(),
            arguments: vec!["--mode".into(), "rpc".into()],
            working_directory: working_directory.into(),
            environment: HashMap::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PiResponse {
    #[serde(default)]
    pub id: Option<String>,
    pub command: String,
    pub success: bool,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum PiRpcEvent {
    Event(Value),
    Response(PiResponse),
    Stderr(String),
    ProcessExited { code: Option<i32> },
    MalformedFrame(String),
}

#[derive(Clone, Debug)]
pub struct JsonlRecord(pub Value);

/// Decode exactly one LF-framed JSONL record. A CR before the LF is tolerated.
pub fn decode_jsonl_record(raw: &[u8]) -> Result<JsonlRecord, RpcError> {
    if !raw.ends_with(b"\n") {
        return Err(RpcError::InvalidFrame("record must end with LF"));
    }
    let body = raw.strip_suffix(b"\n").expect("checked suffix");
    let body = body.strip_suffix(b"\r").unwrap_or(body);
    if body.is_empty() {
        return Err(RpcError::InvalidFrame("empty records are not valid JSON"));
    }
    Ok(JsonlRecord(serde_json::from_slice(body)?))
}

pub fn encode_jsonl_record(value: &Value) -> Result<Vec<u8>, RpcError> {
    let mut bytes = serde_json::to_vec(value)?;
    if bytes.iter().any(|byte| *byte == b'\n' || *byte == b'\r') {
        return Err(RpcError::InvalidFrame(
            "serialized record contained a raw newline",
        ));
    }
    bytes.push(b'\n');
    Ok(bytes)
}

type PendingRequests = Arc<Mutex<HashMap<String, Sender<PiResponse>>>>;

pub struct PiRpcClient {
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    child: Arc<Mutex<Option<Child>>>,
    events: Receiver<PiRpcEvent>,
    pending: PendingRequests,
}

impl PiRpcClient {
    pub fn launch(config: PiLaunch) -> Result<Self, RpcError> {
        let mut command = Command::new(&config.executable);
        command
            .args(&config.arguments)
            .current_dir(&config.working_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .envs(config.environment);
        let mut child = command.spawn()?;
        let stdin = child.stdin.take().ok_or(RpcError::Unavailable)?;
        let stdout = child.stdout.take().ok_or(RpcError::Unavailable)?;
        let stderr = child.stderr.take().ok_or(RpcError::Unavailable)?;
        let (events_tx, events) = unbounded();
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));

        Self::spawn_stdout_reader(stdout, events_tx.clone(), pending.clone());
        Self::spawn_stderr_reader(stderr, events_tx.clone());

        let child = Arc::new(Mutex::new(Some(child)));
        let watched_child = child.clone();
        thread::spawn(move || {
            loop {
                let status = watched_child
                    .lock()
                    .as_mut()
                    .and_then(|process| process.try_wait().ok())
                    .flatten();
                if let Some(status) = status {
                    let _ = events_tx.send(PiRpcEvent::ProcessExited {
                        code: status.code(),
                    });
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
        });

        Ok(Self {
            stdin: Arc::new(Mutex::new(Some(stdin))),
            child,
            events,
            pending,
        })
    }

    fn spawn_stdout_reader(
        stdout: impl io::Read + Send + 'static,
        events: Sender<PiRpcEvent>,
        pending: PendingRequests,
    ) {
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) => break,
                    Ok(_) => match decode_jsonl_record(&raw) {
                        Ok(JsonlRecord(value))
                            if value.get("type").and_then(Value::as_str) == Some("response") =>
                        {
                            match serde_json::from_value::<PiResponse>(value) {
                                Ok(response) => {
                                    if let Some(id) = response.id.as_ref()
                                        && let Some(reply) = pending.lock().remove(id)
                                    {
                                        let _ = reply.send(response.clone());
                                    }
                                    let _ = events.send(PiRpcEvent::Response(response));
                                }
                                Err(error) => {
                                    let _ =
                                        events.send(PiRpcEvent::MalformedFrame(error.to_string()));
                                }
                            }
                        }
                        Ok(JsonlRecord(value)) => {
                            let _ = events.send(PiRpcEvent::Event(value));
                        }
                        Err(error) => {
                            let _ = events.send(PiRpcEvent::MalformedFrame(error.to_string()));
                        }
                    },
                    Err(error) => {
                        let _ = events.send(PiRpcEvent::MalformedFrame(error.to_string()));
                        break;
                    }
                }
            }
        });
    }

    fn spawn_stderr_reader(stderr: impl io::Read + Send + 'static, events: Sender<PiRpcEvent>) {
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = events.send(PiRpcEvent::Stderr(line));
            }
        });
    }

    pub fn events(&self) -> Receiver<PiRpcEvent> {
        self.events.clone()
    }

    pub fn send(&self, command: Value) -> Result<(), RpcError> {
        let record = encode_jsonl_record(&command)?;
        let mut guard = self.stdin.lock();
        let stdin = guard.as_mut().ok_or(RpcError::Unavailable)?;
        stdin.write_all(&record)?;
        stdin.flush()?;
        Ok(())
    }

    pub fn request(&self, mut command: Value, timeout: Duration) -> Result<PiResponse, RpcError> {
        let id = Uuid::new_v4().to_string();
        command["id"] = Value::String(id.clone());
        let (sender, receiver) = bounded(1);
        self.pending.lock().insert(id.clone(), sender);
        if let Err(error) = self.send(command) {
            self.pending.lock().remove(&id);
            return Err(error);
        }
        let response = wait_for_response(&self.pending, &id, receiver, timeout)?;
        if response.success {
            Ok(response)
        } else {
            Err(RpcError::Response(
                response.error.unwrap_or_else(|| "unknown Pi error".into()),
            ))
        }
    }

    pub fn stop(&self) -> Result<(), RpcError> {
        self.stdin.lock().take();
        if let Some(process) = self.child.lock().as_mut() {
            process.kill()?;
        }
        Ok(())
    }
}

fn wait_for_response(
    pending: &PendingRequests,
    id: &str,
    receiver: Receiver<PiResponse>,
    timeout: Duration,
) -> Result<PiResponse, RpcError> {
    receiver.recv_timeout(timeout).map_err(|_| {
        // A timed out request must not retain its sender forever. A late response
        // is still emitted on the event stream, but no longer has a waiting caller.
        pending.lock().remove(id);
        RpcError::Timeout
    })
}

impl Drop for PiRpcClient {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_is_lf_framed_and_accepts_crlf_input() {
        let value = serde_json::json!({"type":"prompt", "message":"A\nB"});
        let encoded = encode_jsonl_record(&value).unwrap();
        assert!(encoded.ends_with(b"\n"));
        assert_eq!(
            decode_jsonl_record(b"{\"type\":\"prompt\"}\r\n").unwrap().0["type"],
            "prompt"
        );
        assert!(matches!(
            decode_jsonl_record(b"{}"),
            Err(RpcError::InvalidFrame(_))
        ));
    }

    #[test]
    fn timed_out_request_is_removed_from_pending_map() {
        let pending: PendingRequests = Arc::new(Mutex::new(HashMap::new()));
        let (sender, receiver) = bounded(1);
        pending.lock().insert("late-response".into(), sender);

        assert!(matches!(
            wait_for_response(&pending, "late-response", receiver, Duration::ZERO),
            Err(RpcError::Timeout)
        ));
        assert!(!pending.lock().contains_key("late-response"));
    }
}
