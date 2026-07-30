use std::{
    collections::HashSet,
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender, TrySendError};
use pi_whim_core::{
    MAX_ONE_SHOT_AI_CONCURRENCY, MAX_ONE_SHOT_AI_QUEUE_CAPACITY, MAX_ONE_SHOT_AI_TIMEOUT_SECS,
    MIN_ONE_SHOT_AI_TIMEOUT_SECS, OneShotAiConfig, ProviderModel, ProviderProfile,
    ProviderProtocol, ThinkingLevel,
};
use uuid::Uuid;

use crate::{MAX_ONE_SHOT_INPUT_BYTES, OneShotTask, protocol};

pub type OneShotRequestId = Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum OneShotErrorKind {
    Disabled,
    InvalidConfiguration,
    InputTooLarge,
    InvalidInput,
    Network,
    Unauthorized,
    RateLimited,
    ProviderRejected,
    InvalidResponse,
    ResponseTooLarge,
    InvalidOutput,
    TimedOut,
    Cancelled,
    ShuttingDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneShotSubmitError {
    Overloaded,
    ShuttingDown,
    InvalidInput,
    InputTooLarge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneShotServiceError {
    Disabled,
    MissingProvider,
    MissingModel,
    MissingApiKey,
    InvalidConcurrency,
    InvalidQueueCapacity,
    InvalidTimeout,
    InvalidBaseUrl,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OneShotCompletion {
    pub request_id: OneShotRequestId,
    pub generation: u64,
    pub task_kind: String,
    pub result: Result<String, OneShotErrorKind>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OneShotStats {
    pub submitted: u64,
    pub running: usize,
    pub succeeded: u64,
    pub failed: u64,
    pub rejected: u64,
    pub cancelled: u64,
    pub timed_out: u64,
}

pub(crate) struct Secret(Box<[u8]>);

impl Secret {
    pub(crate) fn new(value: String) -> Self {
        Self(value.into_bytes().into_boxed_slice())
    }

    pub(crate) fn expose(&self) -> &str {
        // Constructed from a valid String and never mutated before Drop.
        std::str::from_utf8(&self.0).expect("secret originated as UTF-8")
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

pub(crate) struct ProviderRuntime {
    pub(crate) base_url: String,
    pub(crate) protocol: ProviderProtocol,
    pub(crate) model: ProviderModel,
    pub(crate) thinking_level: ThinkingLevel,
    pub(crate) api_key: Secret,
}

impl fmt::Debug for ProviderRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProviderRuntime")
            .field("base_url", &self.base_url)
            .field("protocol", &self.protocol)
            .field("model", &self.model)
            .field("thinking_level", &self.thinking_level)
            .field("api_key", &self.api_key)
            .finish()
    }
}

/// Fully resolved, immutable runtime configuration. It owns exactly one API key.
pub struct ResolvedOneShotAiConfig {
    generation: u64,
    provider: ProviderRuntime,
    max_concurrency: usize,
    queue_capacity: usize,
    timeout: Duration,
}

impl fmt::Debug for ResolvedOneShotAiConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResolvedOneShotAiConfig")
            .field("generation", &self.generation)
            .field("provider", &self.provider)
            .field("max_concurrency", &self.max_concurrency)
            .field("queue_capacity", &self.queue_capacity)
            .field("timeout", &self.timeout)
            .finish()
    }
}

impl ResolvedOneShotAiConfig {
    pub fn new(
        generation: u64,
        config: &OneShotAiConfig,
        task_kind: &str,
        profile: &ProviderProfile,
        api_key: String,
    ) -> Result<Self, OneShotServiceError> {
        let task = config.task(task_kind);
        if !task.enabled {
            return Err(OneShotServiceError::Disabled);
        }
        if task.provider_id != Some(profile.id) {
            return Err(OneShotServiceError::MissingProvider);
        }
        if !(1..=MAX_ONE_SHOT_AI_CONCURRENCY).contains(&config.max_concurrency) {
            return Err(OneShotServiceError::InvalidConcurrency);
        }
        if config.queue_capacity > MAX_ONE_SHOT_AI_QUEUE_CAPACITY {
            return Err(OneShotServiceError::InvalidQueueCapacity);
        }
        if !(MIN_ONE_SHOT_AI_TIMEOUT_SECS..=MAX_ONE_SHOT_AI_TIMEOUT_SECS)
            .contains(&config.timeout_secs)
        {
            return Err(OneShotServiceError::InvalidTimeout);
        }
        let model_id = task
            .model_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or(OneShotServiceError::MissingModel)?;
        let model = profile
            .models
            .iter()
            .find(|model| model.id == model_id)
            .cloned()
            .ok_or(OneShotServiceError::MissingModel)?;
        if api_key.trim().is_empty() {
            return Err(OneShotServiceError::MissingApiKey);
        }
        let base_url = profile.base_url.trim().trim_end_matches('/').to_owned();
        let valid_base_url = url::Url::parse(&base_url).is_ok_and(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some()
                && url.username().is_empty()
                && url.password().is_none()
                && url.query().is_none()
                && url.fragment().is_none()
        });
        if !valid_base_url {
            return Err(OneShotServiceError::InvalidBaseUrl);
        }
        Ok(Self {
            generation,
            provider: ProviderRuntime {
                base_url,
                protocol: profile.protocol,
                model,
                thinking_level: task.thinking_level,
                // Move the Keychain allocation directly into the redacted owner;
                // avoid making a plaintext trim copy that could outlive it.
                api_key: Secret::new(api_key),
            },
            max_concurrency: config.max_concurrency.into(),
            queue_capacity: config.queue_capacity.into(),
            timeout: Duration::from_secs(config.timeout_secs.into()),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }
}

struct WorkItem {
    request_id: OneShotRequestId,
    submitted_at: Instant,
    task: Box<dyn OneShotTask>,
}

#[derive(Default)]
struct AtomicStats {
    submitted: AtomicU64,
    running: AtomicUsize,
    succeeded: AtomicU64,
    failed: AtomicU64,
    rejected: AtomicU64,
    cancelled: AtomicU64,
    timed_out: AtomicU64,
}

impl AtomicStats {
    fn snapshot(&self) -> OneShotStats {
        OneShotStats {
            submitted: self.submitted.load(Ordering::Relaxed),
            running: self.running.load(Ordering::Relaxed),
            succeeded: self.succeeded.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            cancelled: self.cancelled.load(Ordering::Relaxed),
            timed_out: self.timed_out.load(Ordering::Relaxed),
        }
    }
}

struct WorkerShared {
    generation: u64,
    provider: ProviderRuntime,
    timeout: Duration,
    agent: ureq::Agent,
    completions: Sender<OneShotCompletion>,
    cancelled: Mutex<HashSet<OneShotRequestId>>,
    active: Mutex<HashSet<OneShotRequestId>>,
    stats: AtomicStats,
    admission_slots: AtomicUsize,
}

struct ServiceInner {
    generation: u64,
    sender: Mutex<Option<Sender<WorkItem>>>,
    completions: Receiver<OneShotCompletion>,
    shared: Arc<WorkerShared>,
    workers: Mutex<Vec<JoinHandle<()>>>,
    shutting_down: AtomicBool,
}

impl Drop for ServiceInner {
    fn drop(&mut self) {
        self.shutting_down.store(true, Ordering::Release);
        self.sender.get_mut().expect("sender mutex poisoned").take();
        // Dropping JoinHandles detaches the bounded workers. They finish accepted
        // work within its deadline, then release the shared credential normally.
        self.workers
            .get_mut()
            .expect("worker mutex poisoned")
            .clear();
    }
}

#[derive(Clone)]
pub struct OneShotAiService {
    inner: Arc<ServiceInner>,
}

impl fmt::Debug for OneShotAiService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OneShotAiService")
            .field("generation", &self.generation())
            .field("stats", &self.stats())
            .field(
                "shutting_down",
                &self.inner.shutting_down.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl OneShotAiService {
    pub fn new(config: ResolvedOneShotAiConfig) -> Self {
        // The configured queue counts waiting work, not requests already owned
        // by workers. Admission slots make that independent of thread scheduling
        // and also give a zero-length queue reliable immediate handoff semantics.
        let channel_capacity = config.max_concurrency + config.queue_capacity;
        let (sender, receiver) = crossbeam_channel::bounded(channel_capacity);
        let (completion_sender, completions) = crossbeam_channel::unbounded();
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(config.timeout))
            .max_redirects(0)
            .build()
            .new_agent();
        let shared = Arc::new(WorkerShared {
            generation: config.generation,
            provider: config.provider,
            timeout: config.timeout,
            agent,
            completions: completion_sender,
            cancelled: Mutex::new(HashSet::new()),
            active: Mutex::new(HashSet::new()),
            stats: AtomicStats::default(),
            admission_slots: AtomicUsize::new(channel_capacity),
        });
        let mut workers = Vec::with_capacity(config.max_concurrency);
        for index in 0..config.max_concurrency {
            let receiver = receiver.clone();
            let shared = Arc::clone(&shared);
            workers.push(
                thread::Builder::new()
                    .name(format!("one-shot-ai-{index}"))
                    .spawn(move || worker_loop(receiver, shared))
                    .expect("failed to create one-shot AI worker"),
            );
        }
        Self {
            inner: Arc::new(ServiceInner {
                generation: config.generation,
                sender: Mutex::new(Some(sender)),
                completions,
                shared,
                workers: Mutex::new(workers),
                shutting_down: AtomicBool::new(false),
            }),
        }
    }

    pub fn try_submit<T: OneShotTask>(
        &self,
        task: T,
    ) -> Result<OneShotRequestId, OneShotSubmitError> {
        let input = task.input();
        if input.trim().is_empty() {
            self.inner
                .shared
                .stats
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(OneShotSubmitError::InvalidInput);
        }
        if input.len() > MAX_ONE_SHOT_INPUT_BYTES {
            self.inner
                .shared
                .stats
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(OneShotSubmitError::InputTooLarge);
        }
        if self.inner.shutting_down.load(Ordering::Acquire) {
            self.inner
                .shared
                .stats
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(OneShotSubmitError::ShuttingDown);
        }
        let request_id = Uuid::new_v4();
        let claimed = self
            .inner
            .shared
            .admission_slots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |available| {
                available.checked_sub(1)
            })
            .is_ok();
        if !claimed {
            self.inner
                .shared
                .stats
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(OneShotSubmitError::Overloaded);
        }
        let item = WorkItem {
            request_id,
            submitted_at: Instant::now(),
            task: Box::new(task),
        };
        let sender = self.inner.sender.lock().expect("sender mutex poisoned");
        let Some(sender) = sender.as_ref() else {
            release_admission_slot(&self.inner.shared);
            self.inner
                .shared
                .stats
                .rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(OneShotSubmitError::ShuttingDown);
        };
        self.inner
            .shared
            .active
            .lock()
            .expect("active mutex poisoned")
            .insert(request_id);
        match sender.try_send(item) {
            Ok(()) => {
                self.inner
                    .shared
                    .stats
                    .submitted
                    .fetch_add(1, Ordering::Relaxed);
                Ok(request_id)
            }
            Err(TrySendError::Full(_)) => {
                release_admission_slot(&self.inner.shared);
                self.inner
                    .shared
                    .active
                    .lock()
                    .expect("active mutex poisoned")
                    .remove(&request_id);
                self.inner
                    .shared
                    .stats
                    .rejected
                    .fetch_add(1, Ordering::Relaxed);
                Err(OneShotSubmitError::Overloaded)
            }
            Err(TrySendError::Disconnected(_)) => {
                release_admission_slot(&self.inner.shared);
                self.inner
                    .shared
                    .active
                    .lock()
                    .expect("active mutex poisoned")
                    .remove(&request_id);
                self.inner
                    .shared
                    .stats
                    .rejected
                    .fetch_add(1, Ordering::Relaxed);
                Err(OneShotSubmitError::ShuttingDown)
            }
        }
    }

    pub fn cancel(&self, request_id: OneShotRequestId) -> bool {
        if !self
            .inner
            .shared
            .active
            .lock()
            .expect("active mutex poisoned")
            .contains(&request_id)
        {
            return false;
        }
        self.inner
            .shared
            .cancelled
            .lock()
            .expect("cancel mutex poisoned")
            .insert(request_id)
    }

    pub fn completion_receiver(&self) -> Receiver<OneShotCompletion> {
        self.inner.completions.clone()
    }

    pub fn stats(&self) -> OneShotStats {
        self.inner.shared.stats.snapshot()
    }

    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Stops accepting work. Accepted requests remain bounded by their deadlines.
    pub fn shutdown(&self) {
        if !self.inner.shutting_down.swap(true, Ordering::AcqRel) {
            self.inner
                .sender
                .lock()
                .expect("sender mutex poisoned")
                .take();
        }
    }
}

fn worker_loop(receiver: Receiver<WorkItem>, shared: Arc<WorkerShared>) {
    while let Ok(item) = receiver.recv() {
        let result = process(&item, &shared);
        release_admission_slot(&shared);
        shared
            .active
            .lock()
            .expect("active mutex poisoned")
            .remove(&item.request_id);
        shared
            .cancelled
            .lock()
            .expect("cancel mutex poisoned")
            .remove(&item.request_id);
        let completion = OneShotCompletion {
            request_id: item.request_id,
            generation: shared.generation,
            task_kind: item.task.kind().to_owned(),
            result,
        };
        let _ = shared.completions.send(completion);
    }
}

fn release_admission_slot(shared: &WorkerShared) {
    shared.admission_slots.fetch_add(1, Ordering::Release);
}

fn process(item: &WorkItem, shared: &WorkerShared) -> Result<String, OneShotErrorKind> {
    if is_cancelled(shared, item.request_id) {
        shared.stats.cancelled.fetch_add(1, Ordering::Relaxed);
        return Err(OneShotErrorKind::Cancelled);
    }
    if item.submitted_at.elapsed() >= shared.timeout {
        shared.stats.timed_out.fetch_add(1, Ordering::Relaxed);
        return Err(OneShotErrorKind::TimedOut);
    }
    shared.stats.running.fetch_add(1, Ordering::Relaxed);
    let remaining = shared.timeout.saturating_sub(item.submitted_at.elapsed());
    let output = protocol::execute(
        &shared.agent,
        &shared.provider,
        &item.task.system_prompt(),
        item.task.input(),
        item.task.max_output_tokens(),
        remaining,
    )
    .and_then(|value| item.task.normalize_output(&value));
    shared.stats.running.fetch_sub(1, Ordering::Relaxed);

    let result = if is_cancelled(shared, item.request_id) {
        Err(OneShotErrorKind::Cancelled)
    } else if item.submitted_at.elapsed() >= shared.timeout
        || output == Err(OneShotErrorKind::TimedOut)
    {
        Err(OneShotErrorKind::TimedOut)
    } else {
        output
    };
    match result {
        Ok(_) => {
            shared.stats.succeeded.fetch_add(1, Ordering::Relaxed);
        }
        Err(OneShotErrorKind::Cancelled) => {
            shared.stats.cancelled.fetch_add(1, Ordering::Relaxed);
        }
        Err(OneShotErrorKind::TimedOut) => {
            shared.stats.timed_out.fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            shared.stats.failed.fetch_add(1, Ordering::Relaxed);
        }
    }
    result
}

fn is_cancelled(shared: &WorkerShared, request_id: OneShotRequestId) -> bool {
    shared
        .cancelled
        .lock()
        .expect("cancel mutex poisoned")
        .contains(&request_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{OneShotAiTaskConfig, ProviderId, ProviderProfile, SESSION_TITLE_TASK_KIND};
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    use crate::SessionTitleTask;

    fn service(
        protocol: ProviderProtocol,
        response: serde_json::Value,
    ) -> Option<(OneShotAiService, Receiver<String>)> {
        let listener = match TcpListener::bind("127.0.0.1:0") {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => return None,
            Err(error) => panic!("mock server binds: {error}"),
        };
        let address = listener.local_addr().unwrap();
        let (request_tx, request_rx) = crossbeam_channel::bounded(1);
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                if count == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(split) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&bytes[..split + 4]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= split + 4 + length {
                        break;
                    }
                }
            }
            request_tx.send(String::from_utf8(bytes).unwrap()).unwrap();
            let body = response.to_string();
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).unwrap();
        });
        let provider_id = ProviderId::new_v4();
        let model = ProviderModel::new("test/model");
        let profile = ProviderProfile {
            id: provider_id,
            name: "Test".into(),
            base_url: format!("http://{address}/v1"),
            protocol,
            models: vec![model],
            updated_at_ms: 0,
            has_api_key: true,
        };
        let mut config = OneShotAiConfig {
            max_concurrency: 1,
            queue_capacity: 1,
            timeout_secs: 3,
            ..Default::default()
        };
        config.set_task(
            SESSION_TITLE_TASK_KIND,
            OneShotAiTaskConfig {
                enabled: true,
                provider_id: Some(provider_id),
                model_id: Some("test/model".into()),
                ..Default::default()
            },
        );
        Some((
            OneShotAiService::new(
                ResolvedOneShotAiConfig::new(
                    7,
                    &config,
                    SESSION_TITLE_TASK_KIND,
                    &profile,
                    "top-secret".into(),
                )
                .unwrap(),
            ),
            request_rx,
        ))
    }

    #[test]
    fn chat_completion_is_tool_free_and_normalized() {
        let Some((service, request_rx)) = service(
            ProviderProtocol::OpenAiCompletions,
            json!({"choices":[{"message":{"content":"\"A title\""}}]}),
        ) else {
            return;
        };
        let id = service
            .try_submit(SessionTitleTask::new("Explain Rust"))
            .unwrap();
        let completion = service
            .completion_receiver()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert_eq!(completion.request_id, id);
        assert_eq!(completion.generation, 7);
        assert_eq!(completion.result, Ok("A title".into()));
        let request = request_rx.recv().unwrap();
        let (_, body) = request.split_once("\r\n\r\n").unwrap();
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["stream"], false);
        assert!(body.get("tools").is_none());
        assert!(body.get("functions").is_none());
        assert!(!body.to_string().contains("top-secret"));
    }

    #[test]
    fn debug_output_redacts_api_key() {
        let Some((service, _)) = service(
            ProviderProtocol::OpenAiResponses,
            json!({"output_text":"Title"}),
        ) else {
            return;
        };
        let debug = format!("{service:?} {:?}", service.inner.shared.provider);
        assert!(!debug.contains("top-secret"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn oversized_and_empty_input_are_rejected_without_queueing() {
        let Some((service, _)) = service(
            ProviderProtocol::AnthropicMessages,
            json!({"content":[{"type":"text","text":"Title"}]}),
        ) else {
            return;
        };
        assert_eq!(
            service.try_submit(SessionTitleTask::new(" ")),
            Err(OneShotSubmitError::InvalidInput)
        );
        assert_eq!(
            service.try_submit(SessionTitleTask::new(
                "x".repeat(MAX_ONE_SHOT_INPUT_BYTES + 1)
            )),
            Err(OneShotSubmitError::InputTooLarge)
        );
        assert_eq!(service.stats().rejected, 2);
        service.shutdown();
    }
}
