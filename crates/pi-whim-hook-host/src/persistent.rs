//! v2 JSONL persistent process hosting with bounded restart and observe queues.

use crate::audit::{HookAuditEvent, HookAuditOutcome, HookHealthStatus, HookHostHealth};
use crate::invocation::HookInvocation;
use crate::manifest::{DeliveryMode, HookDefinition, HookKind, MAX_STDOUT_BYTES};
use crate::protocol::{HookHello, HookRequest, HookResponseBody, HookWireMessage};
use crate::sandbox;
use crate::{HookHostError, HookHostResult};
use parking_lot::{Condvar, Mutex};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{BufReader, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::thread;
use std::time::{Duration, Instant};

const DIAGNOSTIC_LIMIT: usize = 8 * 1024;
const PROTOCOL_QUEUE_CAPACITY: usize = 128;

pub(crate) type HealthReporter = Arc<dyn Fn(HookHostHealth) + Send + Sync>;
pub(crate) type AuditReporter = Arc<dyn Fn(HookAuditEvent) + Send + Sync>;

pub(crate) struct PersistentHookConfig {
    pub definition: HookDefinition,
    pub event: String,
    pub project_root: PathBuf,
    pub scope_id: String,
    pub revision: String,
    pub health_reporter: HealthReporter,
    pub audit_reporter: AuditReporter,
}

#[derive(Debug)]
struct ProcessState {
    restart_count: u32,
    drop_count: u64,
    status: HookHealthStatus,
    last_error: Option<String>,
}

struct PersistentConnection {
    child: Child,
    stdin: ChildStdin,
    responses: Receiver<HookHostResult<HookWireMessage>>,
    temporary_directory: PathBuf,
}

impl Drop for PersistentConnection {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let started = Instant::now();
        while started.elapsed() < sandbox::FINALIZE_BUDGET {
            match self.child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => break,
            }
        }
        let _ = std::fs::remove_dir_all(&self.temporary_directory);
    }
}

/// A v2 definition's per-scope resident process.
pub(crate) struct PersistentHook {
    definition: HookDefinition,
    event: String,
    project_root: PathBuf,
    scope_id: String,
    revision: String,
    connection: Mutex<Option<PersistentConnection>>,
    control_lock: Mutex<()>,
    state: Mutex<ProcessState>,
    observe_queue: Arc<ObserveQueue>,
    stopping: AtomicBool,
    health_reporter: HealthReporter,
    audit_reporter: AuditReporter,
}

impl PersistentHook {
    pub(crate) fn new(config: PersistentHookConfig) -> HookHostResult<Arc<Self>> {
        let queue = Arc::new(ObserveQueue::new(
            config.definition.delivery.mode,
            config.definition.delivery.capacity,
        ));
        let hook = Arc::new(Self {
            definition: config.definition,
            event: config.event,
            project_root: config.project_root,
            scope_id: config.scope_id,
            revision: config.revision,
            connection: Mutex::new(None),
            control_lock: Mutex::new(()),
            state: Mutex::new(ProcessState {
                restart_count: 0,
                drop_count: 0,
                status: HookHealthStatus::Starting,
                last_error: None,
            }),
            observe_queue: queue,
            stopping: AtomicBool::new(false),
            health_reporter: config.health_reporter,
            audit_reporter: config.audit_reporter,
        });
        hook.start_initial();
        let weak = Arc::downgrade(&hook);
        thread::Builder::new()
            .name(format!("pi-whim-hook-observe-{}", hook.definition.id))
            .spawn(move || {
                loop {
                    let Some(hook) = weak.upgrade() else {
                        break;
                    };
                    let Some(item) = hook.observe_queue.pop_wait() else {
                        break;
                    };
                    if hook.stopping.load(Ordering::Acquire) {
                        break;
                    }
                    if let Err(error) = hook.send_observe(&item.invocation) {
                        hook.report_audit(
                            &item.invocation,
                            HookAuditOutcome::Failed,
                            error_is_timeout(&error),
                            false,
                        );
                    }
                }
            })
            .map_err(HookHostError::io)?;
        Ok(hook)
    }

    pub(crate) fn health(&self) -> HookHostHealth {
        let state = self.state.lock();
        HookHostHealth {
            hook_id: self.definition.id.clone(),
            scope_id: self.scope_id.clone(),
            event: self.event.clone(),
            status: state.status,
            revision: self.revision.clone(),
            restart_count: state.restart_count,
            drop_count: state.drop_count,
            last_error: state.last_error.clone(),
        }
    }

    pub(crate) fn call(&self, invocation: &HookInvocation) -> HookHostResult<Value> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(HookHostError::ScopeRevoked);
        }
        let Some(_control) = self.control_lock.try_lock() else {
            return Err(HookHostError::Busy);
        };
        let mut connection = self.connection.lock();
        if connection.is_none() {
            self.restart_locked(&mut connection)?;
        }
        let Some(process) = connection.as_mut() else {
            return Err(HookHostError::Unhealthy {
                hook_id: self.definition.id.clone(),
            });
        };
        let result = process.request(
            &self.definition.id,
            &self.event,
            self.definition.kind,
            invocation,
            self.definition.effective_timeout(),
        );
        if let Err(error) = &result {
            let detail = error.to_string();
            connection.take();
            self.set_health(HookHealthStatus::Unhealthy, Some(detail));
            let _ = self.restart_locked(&mut connection);
        }
        result
    }

    pub(crate) fn submit_observe(
        &self,
        invocation: HookInvocation,
    ) -> HookHostResult<ObserveSubmit> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(HookHostError::ScopeRevoked);
        }
        let result = self.observe_queue.push(ObserveItem { invocation });
        if result.dropped > 0 {
            let mut state = self.state.lock();
            state.drop_count = state.drop_count.saturating_add(result.dropped as u64);
            let health = self.health_locked(&state);
            drop(state);
            (self.health_reporter)(health);
        }
        Ok(result)
    }

    pub(crate) fn stop(&self) {
        if self.stopping.swap(true, Ordering::AcqRel) {
            return;
        }
        self.observe_queue.stop();
        self.connection.lock().take();
        self.set_health(HookHealthStatus::Stopped, None);
    }

    fn start_initial(&self) {
        let mut connection = self.connection.lock();
        match self.start_connection() {
            Ok(process) => {
                *connection = Some(process);
                self.set_health(HookHealthStatus::Ready, None);
            }
            Err(error) => {
                self.set_health(HookHealthStatus::Unhealthy, Some(error.to_string()));
            }
        }
    }

    fn restart_locked(&self, connection: &mut Option<PersistentConnection>) -> HookHostResult<()> {
        if self.stopping.load(Ordering::Acquire) {
            return Err(HookHostError::ScopeRevoked);
        }
        let restart_number = self.state.lock().restart_count;
        if restart_number >= self.definition.restart.max_restarts {
            self.set_health(
                HookHealthStatus::Unhealthy,
                Some("restart budget exhausted".to_owned()),
            );
            return Err(HookHostError::Unhealthy {
                hook_id: self.definition.id.clone(),
            });
        }
        thread::sleep(self.definition.restart.delay_for(restart_number));
        {
            let mut state = self.state.lock();
            state.restart_count = state.restart_count.saturating_add(1);
            state.status = HookHealthStatus::Starting;
            state.last_error = None;
            let health = self.health_locked(&state);
            drop(state);
            (self.health_reporter)(health);
        }
        match self.start_connection() {
            Ok(process) => {
                *connection = Some(process);
                self.set_health(HookHealthStatus::Ready, None);
                Ok(())
            }
            Err(error) => {
                self.set_health(HookHealthStatus::Unhealthy, Some(error.to_string()));
                Err(error)
            }
        }
    }

    fn start_connection(&self) -> HookHostResult<PersistentConnection> {
        let (mut child, temporary_directory) =
            sandbox::launch_persistent(&self.definition, &self.project_root)?;
        let stdin = match child.stdin.take() {
            Some(stdin) => stdin,
            None => {
                cleanup_child(&mut child, &temporary_directory);
                return Err(HookHostError::process("hook stdin unavailable"));
            }
        };
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                cleanup_child(&mut child, &temporary_directory);
                return Err(HookHostError::process("hook stdout unavailable"));
            }
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => {
                cleanup_child(&mut child, &temporary_directory);
                return Err(HookHostError::process("hook stderr unavailable"));
            }
        };
        let (sender, receiver) = mpsc::sync_channel(PROTOCOL_QUEUE_CAPACITY);
        if let Err(error) = thread::Builder::new()
            .name(format!("pi-whim-hook-read-{}", self.definition.id))
            .spawn(move || read_process_output(stdout, sender))
        {
            cleanup_child(&mut child, &temporary_directory);
            return Err(HookHostError::io(error));
        }
        if let Err(error) = thread::Builder::new()
            .name(format!("pi-whim-hook-stderr-{}", self.definition.id))
            .spawn(move || drain_stderr(stderr))
        {
            cleanup_child(&mut child, &temporary_directory);
            return Err(HookHostError::io(error));
        }
        let mut connection = PersistentConnection {
            child,
            stdin,
            responses: receiver,
            temporary_directory,
        };
        let hello_id = uuid::Uuid::new_v4().simple().to_string();
        let hello = HookHello::new(
            &self.definition.id,
            &self.event,
            self.definition.kind,
            hello_id.clone(),
        );
        write_json_line(&mut connection.stdin, &hello)?;
        let message = connection
            .responses
            .recv_timeout(self.definition.effective_timeout())
            .map_err(|_| HookHostError::Timeout {
                hook_id: self.definition.id.clone(),
            })??;
        match message {
            HookWireMessage::Ready {
                hook_id,
                event,
                kind,
                hello_id: response_hello_id,
            } if hook_id == self.definition.id
                && event == self.event
                && kind.is_none_or(|ready_kind| ready_kind == self.definition.kind)
                && response_hello_id.as_deref() == Some(hello_id.as_str()) =>
            {
                Ok(connection)
            }
            HookWireMessage::Ready { .. } => Err(HookHostError::UnexpectedResponse {
                hook_id: self.definition.id.clone(),
                reason: "hello ready identity mismatch".to_owned(),
            }),
            HookWireMessage::Error(error) => Err(HookHostError::process(error.message)),
            HookWireMessage::Io(error) => Err(error),
            other => Err(HookHostError::UnexpectedResponse {
                hook_id: self.definition.id.clone(),
                reason: format!("expected ready, received {other:?}"),
            }),
        }
    }

    fn send_observe(&self, invocation: &HookInvocation) -> HookHostResult<()> {
        let Some(_control) = self.control_lock.try_lock() else {
            return Err(HookHostError::Busy);
        };
        let mut connection = self.connection.lock();
        if connection.is_none() {
            self.restart_locked(&mut connection)?;
        }
        let Some(process) = connection.as_mut() else {
            return Err(HookHostError::Unhealthy {
                hook_id: self.definition.id.clone(),
            });
        };
        let result = process.request(
            &self.definition.id,
            &self.event,
            HookKind::Observe,
            invocation,
            self.definition.effective_timeout(),
        );
        if let Err(error) = &result {
            connection.take();
            self.set_health(HookHealthStatus::Unhealthy, Some(error.to_string()));
            let _ = self.restart_locked(&mut connection);
        }
        result.map(|_| ())
    }

    fn set_health(&self, status: HookHealthStatus, last_error: Option<String>) {
        let mut state = self.state.lock();
        state.status = status;
        state.last_error = last_error;
        let health = self.health_locked(&state);
        drop(state);
        (self.health_reporter)(health);
    }

    fn health_locked(&self, state: &ProcessState) -> HookHostHealth {
        HookHostHealth {
            hook_id: self.definition.id.clone(),
            scope_id: self.scope_id.clone(),
            event: self.event.clone(),
            status: state.status,
            revision: self.revision.clone(),
            restart_count: state.restart_count,
            drop_count: state.drop_count,
            last_error: state.last_error.clone(),
        }
    }

    fn report_audit(
        &self,
        invocation: &HookInvocation,
        outcome: HookAuditOutcome,
        timed_out: bool,
        dropped: bool,
    ) {
        let state = self.state.lock();
        let event = HookAuditEvent {
            hook_id: self.definition.id.clone(),
            scope_id: self.scope_id.clone(),
            event: self.event.clone(),
            kind: kind_name(self.definition.kind).to_owned(),
            outcome: if timed_out {
                HookAuditOutcome::TimedOut
            } else {
                outcome
            },
            duration_ms: 0,
            revision: self.revision.clone(),
            dropped,
            restart_count: state.restart_count,
            drop_count: state.drop_count,
            grants_hash: invocation.context.grants_hash.clone(),
        };
        drop(state);
        (self.audit_reporter)(event);
    }
}

impl PersistentConnection {
    fn request(
        &mut self,
        hook_id: &str,
        event: &str,
        kind: HookKind,
        invocation: &HookInvocation,
        timeout: Duration,
    ) -> HookHostResult<Value> {
        let request = HookRequest::from_invocation(hook_id, kind, invocation);
        write_json_line(&mut self.stdin, &request)?;
        let started = Instant::now();
        loop {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(HookHostError::Timeout {
                    hook_id: hook_id.to_owned(),
                });
            }
            let message =
                self.responses
                    .recv_timeout(remaining)
                    .map_err(|_| HookHostError::Timeout {
                        hook_id: hook_id.to_owned(),
                    })?;
            match message {
                Ok(HookWireMessage::Response(response)) => {
                    if response.hook_id != hook_id || response.event != event {
                        return Err(HookHostError::UnexpectedResponse {
                            hook_id: hook_id.to_owned(),
                            reason: "hook or event mismatch".to_owned(),
                        });
                    }
                    if response.request_id != invocation.request_id {
                        return Err(HookHostError::UnexpectedResponse {
                            hook_id: hook_id.to_owned(),
                            reason: "request_id mismatch".to_owned(),
                        });
                    }
                    return response_value(kind, response.response);
                }
                Ok(HookWireMessage::Error(error)) => {
                    if error.hook_id != hook_id
                        || error.request_id.as_deref() != Some(invocation.request_id.as_str())
                    {
                        return Err(HookHostError::UnexpectedResponse {
                            hook_id: hook_id.to_owned(),
                            reason: "error response identity mismatch".to_owned(),
                        });
                    }
                    return Err(HookHostError::process(error.message));
                }
                Ok(HookWireMessage::Telemetry { .. }) => continue,
                Ok(HookWireMessage::Ready { .. }) => {
                    return Err(HookHostError::UnexpectedResponse {
                        hook_id: hook_id.to_owned(),
                        reason: "ready received while request was in flight".to_owned(),
                    });
                }
                Ok(HookWireMessage::Io(error)) => return Err(error),
                Err(error) => return Err(error),
            }
        }
    }
}

fn response_value(kind: HookKind, body: HookResponseBody) -> HookHostResult<Value> {
    match (kind, body) {
        (HookKind::Gate, HookResponseBody::Gate { decision, message }) => {
            if !matches!(decision.as_str(), "allow" | "deny") {
                return Err(HookHostError::InvalidInvocation(
                    "gate decision must be allow or deny".to_owned(),
                ));
            }
            let mut object = serde_json::Map::new();
            object.insert("decision".to_owned(), Value::String(decision));
            if let Some(message) = message {
                if message.len() > 4 * 1024 {
                    return Err(HookHostError::InvalidInvocation(
                        "gate message exceeds limits".to_owned(),
                    ));
                }
                object.insert("message".to_owned(), Value::String(message));
            }
            Ok(Value::Object(object))
        }
        (HookKind::Transform, HookResponseBody::Transform { payload }) => {
            Ok(json!({"payload": payload}))
        }
        (HookKind::Observe, HookResponseBody::Observe { accepted }) => {
            Ok(json!({"accepted": accepted}))
        }
        (_, body) => Err(HookHostError::UnexpectedResponse {
            hook_id: "unknown".to_owned(),
            reason: format!("response kind mismatch: {body:?}"),
        }),
    }
}

fn write_json_line<T: serde::Serialize>(writer: &mut ChildStdin, value: &T) -> HookHostResult<()> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| HookHostError::Json(error.to_string()))?;
    if bytes.len() > MAX_STDOUT_BYTES {
        return Err(HookHostError::InvalidInvocation(
            "hook request exceeds 64 KiB".to_owned(),
        ));
    }
    writer.write_all(&bytes).map_err(HookHostError::io)?;
    writer.write_all(b"\n").map_err(HookHostError::io)
}

fn cleanup_child(child: &mut Child, temporary_directory: &std::path::Path) {
    let _ = child.kill();
    let started = Instant::now();
    while started.elapsed() < sandbox::FINALIZE_BUDGET {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => thread::sleep(Duration::from_millis(5)),
        }
    }
    let _ = std::fs::remove_dir_all(temporary_directory);
}

fn read_process_output<R: Read + Send + 'static>(
    reader: R,
    sender: SyncSender<HookHostResult<HookWireMessage>>,
) {
    let mut reader = BufReader::new(reader);
    loop {
        match read_line_bounded(&mut reader) {
            Ok(Some(line)) => {
                let result = HookWireMessage::parse_line(&line);
                if sender.send(result).is_err() {
                    break;
                }
            }
            Ok(None) => {
                let _ = sender.send(Err(HookHostError::process("hook stdout closed")));
                break;
            }
            Err(error) => {
                let _ = sender.send(Err(HookHostError::io(error)));
                break;
            }
        }
    }
}

fn read_line_bounded<R: Read>(reader: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut byte = [0_u8; 1];
    loop {
        let read = reader.read(&mut byte)?;
        if read == 0 {
            return if line.is_empty() {
                Ok(None)
            } else {
                Ok(Some(line))
            };
        }
        line.push(byte[0]);
        if line.len() > MAX_STDOUT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "hook protocol line exceeds limit",
            ));
        }
        if byte[0] == b'\n' {
            return Ok(Some(line));
        }
    }
}

fn drain_stderr<R: Read>(mut reader: R) {
    let mut buffer = [0_u8; 4096];
    let mut retained = 0_usize;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                retained = retained.saturating_add(read).min(DIAGNOSTIC_LIMIT);
            }
        }
    }
    let _ = retained;
}

fn error_is_timeout(error: &HookHostError) -> bool {
    matches!(error, HookHostError::Timeout { .. })
}

fn kind_name(kind: HookKind) -> &'static str {
    match kind {
        HookKind::Gate => "gate",
        HookKind::Transform => "transform",
        HookKind::Observe => "observe",
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObserveItem {
    pub invocation: HookInvocation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ObserveSubmit {
    pub accepted: bool,
    pub dropped: usize,
}

struct ObserveQueue {
    mode: DeliveryMode,
    capacity: usize,
    state: Mutex<ObserveQueueState>,
    wake: Condvar,
}

struct ObserveQueueState {
    items: VecDeque<ObserveItem>,
    stopped: bool,
}

impl ObserveQueue {
    fn new(mode: DeliveryMode, capacity: usize) -> Self {
        Self {
            mode,
            capacity: capacity.max(1),
            state: Mutex::new(ObserveQueueState {
                items: VecDeque::new(),
                stopped: false,
            }),
            wake: Condvar::new(),
        }
    }

    fn push(&self, item: ObserveItem) -> ObserveSubmit {
        let mut state = self.state.lock();
        if state.stopped {
            return ObserveSubmit {
                accepted: false,
                dropped: 1,
            };
        }
        match self.mode {
            DeliveryMode::StateLatest => {
                let dropped = usize::from(!state.items.is_empty());
                state.items.clear();
                state.items.push_back(item);
                self.wake.notify_one();
                ObserveSubmit {
                    accepted: true,
                    dropped,
                }
            }
            DeliveryMode::Telemetry | DeliveryMode::RequestResponse => {
                if state.items.len() >= self.capacity {
                    ObserveSubmit {
                        accepted: false,
                        dropped: 1,
                    }
                } else {
                    state.items.push_back(item);
                    self.wake.notify_one();
                    ObserveSubmit {
                        accepted: true,
                        dropped: 0,
                    }
                }
            }
        }
    }

    fn pop_wait(&self) -> Option<ObserveItem> {
        let mut state = self.state.lock();
        loop {
            if let Some(item) = state.items.pop_front() {
                return Some(item);
            }
            if state.stopped {
                return None;
            }
            self.wake.wait(&mut state);
        }
    }

    fn stop(&self) {
        let mut state = self.state.lock();
        state.stopped = true;
        state.items.clear();
        self.wake.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HookInvocationContext, HookPayload};
    use serde_json::json;

    fn invocation(request_id: &str) -> HookInvocation {
        HookInvocation::new(
            request_id,
            "pi.tool.completed",
            HookKind::Observe,
            HookInvocationContext::app("scope", "revision"),
            HookPayload::from_value(json!({"status": request_id}))
                .unwrap_or_else(|error| panic!("test payload must be valid: {error}")),
        )
        .unwrap_or_else(|error| panic!("test invocation must be valid: {error}"))
    }

    #[test]
    fn telemetry_delivery_drops_at_bounded_capacity() {
        let queue = ObserveQueue::new(DeliveryMode::Telemetry, 2);
        assert_eq!(
            queue.push(ObserveItem {
                invocation: invocation("1")
            }),
            ObserveSubmit {
                accepted: true,
                dropped: 0,
            }
        );
        assert_eq!(
            queue.push(ObserveItem {
                invocation: invocation("2")
            }),
            ObserveSubmit {
                accepted: true,
                dropped: 0,
            }
        );
        assert_eq!(
            queue.push(ObserveItem {
                invocation: invocation("3")
            }),
            ObserveSubmit {
                accepted: false,
                dropped: 1,
            }
        );
        queue.stop();
    }

    #[test]
    fn state_latest_coalesces_and_keeps_the_newest_invocation() {
        let queue = ObserveQueue::new(DeliveryMode::StateLatest, 8);
        assert_eq!(
            queue
                .push(ObserveItem {
                    invocation: invocation("old")
                })
                .dropped,
            0
        );
        assert_eq!(
            queue
                .push(ObserveItem {
                    invocation: invocation("new")
                })
                .dropped,
            1
        );
        let latest = queue
            .pop_wait()
            .unwrap_or_else(|| panic!("latest queue item must be available"));
        assert_eq!(latest.invocation.request_id, "new");
        queue.stop();
        assert!(queue.pop_wait().is_none());
    }
}
