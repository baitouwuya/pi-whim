use crate::coordinator::{
    DEFAULT_COMPLETED_RETENTION, DEFAULT_EVENT_RETENTION, DEFAULT_HUB_ACTIVE_LIMIT,
    DEFAULT_OWNER_ACTIVE_LIMIT, HubLimits, Shared, SourceState, TaskKind, TaskRecord,
    active_owner_task_count, active_task_count, all_selected_sources_closed, complete_task,
    coordinator, drain_updates, enqueue_update, ensure_open, find_matching_event, prepare_query,
    task_snapshots, validate_payload,
};
use crate::types::{duration_ms, wall_clock_ms};
use crate::{
    WaitError, WaitEvent, WaitOwnerId, WaitQuery, WaitSourceDescriptor, WaitSourceId, WaitStatus,
    WaitTaskId, WaitTaskMetadata, WaitTaskSnapshot,
};
use parking_lot::Mutex;
use pi_whim_signal::{Signal, StateSignal};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

struct HubLifecycle {
    shared: Weak<Shared>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for HubLifecycle {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.state.lock().shutdown = true;
            shared.changed.notify_all();
        }
        let worker = self.worker.lock().take();
        if let Some(worker) = worker
            && worker.thread().id() != std::thread::current().id()
        {
            let _ = worker.join();
        }
    }
}

/// A cloneable coordinator for bounded foreground and background one-shot waits.
#[derive(Clone)]
pub struct WaitHub {
    shared: Arc<Shared>,
    _lifecycle: Arc<HubLifecycle>,
    completion_signal: Signal<WaitTaskSnapshot>,
}

impl WaitHub {
    pub const MAX_ACTIVE_TASKS_PER_OWNER: usize = DEFAULT_OWNER_ACTIVE_LIMIT;
    pub const MAX_ACTIVE_TASKS: usize = DEFAULT_HUB_ACTIVE_LIMIT;
    pub const MAX_COMPLETED_SNAPSHOTS: usize = DEFAULT_COMPLETED_RETENTION;
    pub const MAX_RETAINED_EVENTS: usize = DEFAULT_EVENT_RETENTION;

    pub fn new() -> Result<Self, WaitError> {
        Self::with_limits(HubLimits::default())
    }

    fn with_limits(limits: HubLimits) -> Result<Self, WaitError> {
        let (completion_signal, completion_emitter) = Signal::channel();
        let shared = Shared::new(limits, completion_emitter);
        let worker_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("pi-whim-wait".into())
            .spawn(move || coordinator(worker_shared))
            .map_err(|error| WaitError::CoordinatorStart(error.to_string()))?;
        let lifecycle = Arc::new(HubLifecycle {
            shared: Arc::downgrade(&shared),
            worker: Mutex::new(Some(worker)),
        });
        Ok(Self {
            shared,
            _lifecycle: lifecycle,
            completion_signal,
        })
    }

    pub fn register_source(
        &self,
        descriptor: WaitSourceDescriptor,
    ) -> Result<WaitSourceHandle, WaitError> {
        let source_id = descriptor.source_id().clone();
        let generation = {
            let mut state = self.shared.state.lock();
            ensure_open(&state)?;
            if state
                .sources
                .get(&source_id)
                .is_some_and(|source| source.open)
            {
                return Err(WaitError::DuplicateSource(source_id));
            }
            state.next_source_generation = state
                .next_source_generation
                .checked_add(1)
                .ok_or(WaitError::SequenceOverflow)?;
            let generation = state.next_source_generation;
            state.sources.insert(
                source_id.clone(),
                SourceState {
                    descriptor,
                    generation,
                    open: true,
                },
            );
            generation
        };
        self.shared.changed.notify_all();
        Ok(WaitSourceHandle {
            lease: Arc::new(SourceLease {
                shared: Arc::downgrade(&self.shared),
                source_id,
                generation,
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub fn current_sequence(&self) -> u64 {
        self.shared.state.lock().next_sequence
    }

    /// Returns whether two handles refer to the same coordinator instance.
    pub fn same_instance(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }

    pub fn wait(&self, query: WaitQuery, timeout: Duration) -> Result<WaitStatus, WaitError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(WaitError::TimeoutTooLarge)?;
        let mut state = self.shared.state.lock();
        ensure_open(&state)?;
        let query = prepare_query(&state, query)?;
        loop {
            if let Some(event) = find_matching_event(&state, &query) {
                return Ok(WaitStatus::Matched { event });
            }
            if all_selected_sources_closed(&state, &query) {
                return Ok(WaitStatus::SourceClosed);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitStatus::TimedOut);
            }
            self.shared.changed.wait_for(&mut state, deadline - now);
            ensure_open(&state)?;
        }
    }

    pub fn wait_timer(&self, duration: Duration) -> Result<WaitStatus, WaitError> {
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or(WaitError::TimeoutTooLarge)?;
        let mut state = self.shared.state.lock();
        ensure_open(&state)?;
        loop {
            let now = Instant::now();
            if now >= deadline {
                return Ok(WaitStatus::Elapsed);
            }
            self.shared.changed.wait_for(&mut state, deadline - now);
            ensure_open(&state)?;
        }
    }

    pub fn start_background_timer(
        &self,
        owner_id: WaitOwnerId,
        duration: Duration,
    ) -> Result<WaitTaskId, WaitError> {
        self.start_background_timer_with_metadata(owner_id, duration, None)
    }

    pub fn start_background_timer_with_metadata(
        &self,
        owner_id: WaitOwnerId,
        duration: Duration,
        metadata: Option<WaitTaskMetadata>,
    ) -> Result<WaitTaskId, WaitError> {
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or(WaitError::TimeoutTooLarge)?;
        let started_at_ms = wall_clock_ms();
        let deadline_at_ms = started_at_ms.saturating_add(duration_ms(duration));
        let (task_id, should_drain) = {
            let mut state = self.shared.state.lock();
            ensure_open(&state)?;
            enforce_task_limits(&state, &owner_id, self.shared.limits)?;
            let task_id = WaitTaskId::new();
            let snapshot = WaitTaskSnapshot {
                task_id,
                owner_id,
                metadata,
                status: WaitStatus::Pending,
                started_after_sequence: state.next_sequence,
                started_at_ms,
                deadline_at_ms,
                completed_at_ms: None,
            };
            state.tasks.insert(
                task_id,
                TaskRecord {
                    snapshot,
                    kind: TaskKind::Timer,
                    deadline,
                },
            );
            state.task_order.push_back(task_id);
            let should_drain = enqueue_update(&self.shared, task_snapshots(&state), Vec::new());
            (task_id, should_drain)
        };
        if should_drain {
            drain_updates(&self.shared);
        }
        self.shared.changed.notify_all();
        Ok(task_id)
    }

    pub fn start_background(
        &self,
        owner_id: WaitOwnerId,
        query: WaitQuery,
        timeout: Duration,
    ) -> Result<WaitTaskId, WaitError> {
        self.start_background_with_metadata(owner_id, query, timeout, None)
    }

    pub fn start_background_with_metadata(
        &self,
        owner_id: WaitOwnerId,
        query: WaitQuery,
        timeout: Duration,
        metadata: Option<WaitTaskMetadata>,
    ) -> Result<WaitTaskId, WaitError> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(WaitError::TimeoutTooLarge)?;
        let started_at_ms = wall_clock_ms();
        let deadline_at_ms = started_at_ms.saturating_add(duration_ms(timeout));
        let (task_id, should_drain) = {
            let mut state = self.shared.state.lock();
            ensure_open(&state)?;
            enforce_task_limits(&state, &owner_id, self.shared.limits)?;
            let query = prepare_query(&state, query)?;
            let task_id = WaitTaskId::new();
            let snapshot = WaitTaskSnapshot {
                task_id,
                owner_id,
                metadata,
                status: WaitStatus::Pending,
                started_after_sequence: query.after_sequence,
                started_at_ms,
                deadline_at_ms,
                completed_at_ms: None,
            };
            state.tasks.insert(
                task_id,
                TaskRecord {
                    snapshot,
                    kind: TaskKind::Monitor(query),
                    deadline,
                },
            );
            state.task_order.push_back(task_id);
            let should_drain = enqueue_update(&self.shared, task_snapshots(&state), Vec::new());
            (task_id, should_drain)
        };
        if should_drain {
            drain_updates(&self.shared);
        }
        self.shared.changed.notify_all();
        Ok(task_id)
    }

    pub fn task_status(
        &self,
        owner_id: &WaitOwnerId,
        task_id: WaitTaskId,
    ) -> Result<WaitTaskSnapshot, WaitError> {
        let state = self.shared.state.lock();
        state
            .tasks
            .get(&task_id)
            .filter(|task| &task.snapshot.owner_id == owner_id)
            .map(|task| task.snapshot.clone())
            .ok_or(WaitError::TaskNotFound)
    }

    pub fn cancel(
        &self,
        owner_id: &WaitOwnerId,
        task_id: WaitTaskId,
    ) -> Result<WaitTaskSnapshot, WaitError> {
        self.cancel_task(Some(owner_id), task_id)
    }

    pub fn cancel_any(&self, task_id: WaitTaskId) -> Result<WaitTaskSnapshot, WaitError> {
        self.cancel_task(None, task_id)
    }

    fn cancel_task(
        &self,
        owner_id: Option<&WaitOwnerId>,
        task_id: WaitTaskId,
    ) -> Result<WaitTaskSnapshot, WaitError> {
        let (completed, should_drain) = {
            let mut state = self.shared.state.lock();
            let task = state
                .tasks
                .get(&task_id)
                .filter(|task| owner_id.is_none_or(|owner| &task.snapshot.owner_id == owner))
                .ok_or(WaitError::TaskNotFound)?;
            if task.snapshot.status.is_terminal() {
                return Ok(task.snapshot.clone());
            }
            let completed = complete_task(
                &mut state,
                task_id,
                WaitStatus::Cancelled,
                self.shared.limits.completed_retention,
            )
            .ok_or(WaitError::TaskNotFound)?;
            let should_drain = enqueue_update(
                &self.shared,
                task_snapshots(&state),
                vec![completed.clone()],
            );
            (completed, should_drain)
        };
        if should_drain {
            drain_updates(&self.shared);
        }
        self.shared.changed.notify_all();
        Ok(completed)
    }

    pub fn completion_signal(&self) -> Signal<WaitTaskSnapshot> {
        self.completion_signal.clone()
    }

    pub fn task_state_signal(&self) -> StateSignal<Vec<WaitTaskSnapshot>> {
        self.shared.task_state.clone()
    }

    #[cfg(test)]
    pub(crate) fn test_with_limits(
        event_retention: usize,
        completed_retention: usize,
        owner_active: usize,
        hub_active: usize,
    ) -> Result<Self, WaitError> {
        Self::with_limits(HubLimits {
            event_retention,
            completed_retention,
            owner_active,
            hub_active,
        })
    }
}

fn enforce_task_limits(
    state: &crate::coordinator::HubState,
    owner_id: &WaitOwnerId,
    limits: HubLimits,
) -> Result<(), WaitError> {
    if active_task_count(state) >= limits.hub_active {
        return Err(WaitError::HubTaskLimit);
    }
    if active_owner_task_count(state, owner_id) >= limits.owner_active {
        return Err(WaitError::OwnerTaskLimit);
    }
    Ok(())
}

/// A cloneable producer lease for one registered source.
#[derive(Clone)]
pub struct WaitSourceHandle {
    lease: Arc<SourceLease>,
}

impl WaitSourceHandle {
    pub fn source_id(&self) -> &WaitSourceId {
        &self.lease.source_id
    }

    pub fn publish(&self, payload: Value) -> Result<WaitEvent, WaitError> {
        let shared = self.lease.shared.upgrade().ok_or(WaitError::HubClosed)?;
        let event = {
            let mut state = shared.state.lock();
            ensure_open(&state)?;
            let source = state
                .sources
                .get(&self.lease.source_id)
                .filter(|source| source.generation == self.lease.generation && source.open)
                .ok_or_else(|| WaitError::SourceClosed(self.lease.source_id.clone()))?;
            validate_payload(&source.descriptor, &payload)?;
            let sequence = state
                .next_sequence
                .checked_add(1)
                .ok_or(WaitError::SequenceOverflow)?;
            state.next_sequence = sequence;
            let event = WaitEvent {
                sequence,
                emitted_at_ms: wall_clock_ms(),
                source_id: self.lease.source_id.clone(),
                payload,
            };
            state.events.push_back(event.clone());
            while state.events.len() > shared.limits.event_retention {
                state.events.pop_front();
            }
            event
        };
        shared.changed.notify_all();
        Ok(event)
    }

    pub fn close(&self) {
        self.lease.close();
    }
}

struct SourceLease {
    shared: Weak<Shared>,
    source_id: WaitSourceId,
    generation: u64,
    closed: AtomicBool,
}

impl SourceLease {
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        let changed = {
            let mut state = shared.state.lock();
            if let Some(source) = state.sources.get_mut(&self.source_id) {
                if source.generation == self.generation && source.open {
                    source.open = false;
                    true
                } else {
                    false
                }
            } else {
                false
            }
        };
        if changed {
            shared.changed.notify_all();
        }
    }
}

impl Drop for SourceLease {
    fn drop(&mut self) {
        self.close();
    }
}
