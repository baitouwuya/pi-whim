use crate::types::wall_clock_ms;
use crate::{
    MAX_WAIT_CLAUSES, WaitClause, WaitError, WaitEvent, WaitMatcher, WaitOwnerId, WaitQuery,
    WaitSourceDescriptor, WaitSourceId, WaitSourceSelection, WaitStatus, WaitTaskId,
    WaitTaskSnapshot,
};
use parking_lot::{Condvar, Mutex};
use pi_whim_signal::{SignalEmitter, StateSignal};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

pub(crate) const DEFAULT_EVENT_RETENTION: usize = 1_024;
pub(crate) const DEFAULT_COMPLETED_RETENTION: usize = 128;
pub(crate) const DEFAULT_OWNER_ACTIVE_LIMIT: usize = 32;
pub(crate) const DEFAULT_HUB_ACTIVE_LIMIT: usize = 256;

#[derive(Clone, Copy)]
pub(crate) struct HubLimits {
    pub(crate) event_retention: usize,
    pub(crate) completed_retention: usize,
    pub(crate) owner_active: usize,
    pub(crate) hub_active: usize,
}

impl Default for HubLimits {
    fn default() -> Self {
        Self {
            event_retention: DEFAULT_EVENT_RETENTION,
            completed_retention: DEFAULT_COMPLETED_RETENTION,
            owner_active: DEFAULT_OWNER_ACTIVE_LIMIT,
            hub_active: DEFAULT_HUB_ACTIVE_LIMIT,
        }
    }
}

pub(crate) struct SourceState {
    pub(crate) descriptor: WaitSourceDescriptor,
    pub(crate) generation: u64,
    pub(crate) open: bool,
}

pub(crate) struct PreparedClause {
    selection: WaitSourceSelection,
    matcher: WaitMatcher,
}

pub(crate) struct PreparedQuery {
    clauses: Vec<PreparedClause>,
    pub(crate) after_sequence: u64,
}

pub(crate) enum TaskKind {
    Monitor(PreparedQuery),
    Timer,
}

pub(crate) struct TaskRecord {
    pub(crate) snapshot: WaitTaskSnapshot,
    pub(crate) kind: TaskKind,
    pub(crate) deadline: Instant,
}

pub(crate) struct HubState {
    pub(crate) sources: BTreeMap<WaitSourceId, SourceState>,
    pub(crate) events: VecDeque<WaitEvent>,
    pub(crate) next_sequence: u64,
    pub(crate) next_source_generation: u64,
    pub(crate) tasks: BTreeMap<WaitTaskId, TaskRecord>,
    pub(crate) task_order: VecDeque<WaitTaskId>,
    completed_order: VecDeque<WaitTaskId>,
    pub(crate) shutdown: bool,
}

struct HubUpdate {
    task_state: Vec<WaitTaskSnapshot>,
    completions: Vec<WaitTaskSnapshot>,
}

struct UpdateQueue {
    pending: VecDeque<HubUpdate>,
    draining: bool,
}

pub(crate) struct Shared {
    pub(crate) state: Mutex<HubState>,
    pub(crate) changed: Condvar,
    completion_emitter: SignalEmitter<WaitTaskSnapshot>,
    pub(crate) task_state: StateSignal<Vec<WaitTaskSnapshot>>,
    updates: Mutex<UpdateQueue>,
    pub(crate) limits: HubLimits,
}

impl Shared {
    pub(crate) fn new(
        limits: HubLimits,
        completion_emitter: SignalEmitter<WaitTaskSnapshot>,
    ) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HubState {
                sources: BTreeMap::new(),
                events: VecDeque::new(),
                next_sequence: 0,
                next_source_generation: 0,
                tasks: BTreeMap::new(),
                task_order: VecDeque::new(),
                completed_order: VecDeque::new(),
                shutdown: false,
            }),
            changed: Condvar::new(),
            completion_emitter,
            task_state: StateSignal::new(Vec::new()),
            updates: Mutex::new(UpdateQueue {
                pending: VecDeque::new(),
                draining: false,
            }),
            limits,
        })
    }
}

pub(crate) fn coordinator(shared: Arc<Shared>) {
    loop {
        let should_drain = {
            let mut state = shared.state.lock();
            loop {
                if state.shutdown {
                    return;
                }
                let completed = settle_tasks(
                    &mut state,
                    Instant::now(),
                    shared.limits.completed_retention,
                );
                if !completed.is_empty() {
                    break enqueue_update(&shared, task_snapshots(&state), completed);
                }
                if let Some(deadline) = earliest_deadline(&state) {
                    let now = Instant::now();
                    if deadline > now {
                        shared.changed.wait_for(&mut state, deadline - now);
                    }
                } else {
                    shared.changed.wait(&mut state);
                }
            }
        };
        if should_drain {
            drain_updates(&shared);
        }
    }
}

pub(crate) fn enqueue_update(
    shared: &Shared,
    task_state: Vec<WaitTaskSnapshot>,
    completions: Vec<WaitTaskSnapshot>,
) -> bool {
    let mut updates = shared.updates.lock();
    updates.pending.push_back(HubUpdate {
        task_state,
        completions,
    });
    if updates.draining {
        false
    } else {
        updates.draining = true;
        true
    }
}

pub(crate) fn drain_updates(shared: &Shared) {
    loop {
        let update = {
            let mut updates = shared.updates.lock();
            let Some(update) = updates.pending.pop_front() else {
                updates.draining = false;
                return;
            };
            update
        };
        shared.task_state.set(update.task_state);
        for snapshot in update.completions {
            shared.completion_emitter.emit(snapshot);
        }
    }
}

fn settle_tasks(
    state: &mut HubState,
    now: Instant,
    completed_retention: usize,
) -> Vec<WaitTaskSnapshot> {
    let pending_ids = state
        .task_order
        .iter()
        .copied()
        .filter(|task_id| {
            state
                .tasks
                .get(task_id)
                .is_some_and(|task| !task.snapshot.status.is_terminal())
        })
        .collect::<Vec<_>>();
    let mut decisions = Vec::new();
    for task_id in pending_ids {
        let Some(task) = state.tasks.get(&task_id) else {
            continue;
        };
        let status = match &task.kind {
            TaskKind::Monitor(query) => {
                if let Some(event) = find_matching_event(state, query) {
                    Some(WaitStatus::Matched { event })
                } else if all_selected_sources_closed(state, query) {
                    Some(WaitStatus::SourceClosed)
                } else if now >= task.deadline {
                    Some(WaitStatus::TimedOut)
                } else {
                    None
                }
            }
            TaskKind::Timer => (now >= task.deadline).then_some(WaitStatus::Elapsed),
        };
        if let Some(status) = status {
            decisions.push((task_id, status));
        }
    }
    decisions
        .into_iter()
        .filter_map(|(task_id, status)| complete_task(state, task_id, status, completed_retention))
        .collect()
}

pub(crate) fn complete_task(
    state: &mut HubState,
    task_id: WaitTaskId,
    status: WaitStatus,
    completed_retention: usize,
) -> Option<WaitTaskSnapshot> {
    let task = state.tasks.get_mut(&task_id)?;
    if task.snapshot.status.is_terminal() {
        return None;
    }
    task.snapshot.status = status;
    task.snapshot.completed_at_ms = Some(wall_clock_ms().max(task.snapshot.started_at_ms));
    let snapshot = task.snapshot.clone();
    state.completed_order.push_back(task_id);
    while state.completed_order.len() > completed_retention {
        if let Some(evicted) = state.completed_order.pop_front() {
            state.tasks.remove(&evicted);
            state.task_order.retain(|candidate| *candidate != evicted);
        }
    }
    Some(snapshot)
}

pub(crate) fn prepare_query(
    state: &HubState,
    query: WaitQuery,
) -> Result<PreparedQuery, WaitError> {
    if query.clauses().is_empty() {
        return Err(WaitError::EmptyClauses);
    }
    if query.clauses().len() > MAX_WAIT_CLAUSES {
        return Err(WaitError::TooManyClauses {
            max: MAX_WAIT_CLAUSES,
        });
    }
    let clauses = query
        .clauses()
        .iter()
        .map(|clause| prepare_clause(state, clause))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PreparedQuery {
        clauses,
        after_sequence: query.after_sequence().unwrap_or(state.next_sequence),
    })
}

fn prepare_clause(state: &HubState, clause: &WaitClause) -> Result<PreparedClause, WaitError> {
    for (field, value) in clause.matcher().fields() {
        if !crate::types::is_scalar(value) {
            return Err(WaitError::NonScalarMatcher(field.clone()));
        }
    }
    match clause.selection() {
        WaitSourceSelection::Sources { source_ids } => {
            if source_ids.is_empty() {
                return Err(WaitError::EmptySourceSelection);
            }
            for source_id in source_ids {
                let source = state
                    .sources
                    .get(source_id)
                    .ok_or_else(|| WaitError::UnknownSource(source_id.clone()))?;
                if let Some(field) = clause
                    .matcher()
                    .fields()
                    .keys()
                    .find(|field| !source.descriptor.matcher_fields().contains(*field))
                {
                    return Err(WaitError::UnknownMatcherField(field.clone()));
                }
            }
        }
        WaitSourceSelection::Any => {
            if state.sources.is_empty() {
                return Err(WaitError::EmptySourceSelection);
            }
            let compatible = state.sources.values().any(|source| {
                clause
                    .matcher()
                    .fields()
                    .keys()
                    .all(|field| source.descriptor.matcher_fields().contains(field))
            });
            if !compatible {
                let field = clause
                    .matcher()
                    .fields()
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_default();
                return Err(WaitError::UnknownMatcherField(field));
            }
        }
    }
    Ok(PreparedClause {
        selection: clause.selection().clone(),
        matcher: clause.matcher().clone(),
    })
}

pub(crate) fn find_matching_event(state: &HubState, query: &PreparedQuery) -> Option<WaitEvent> {
    state
        .events
        .iter()
        .find(|event| {
            event.sequence > query.after_sequence
                && query
                    .clauses
                    .iter()
                    .any(|clause| event_matches_clause(state, clause, event))
        })
        .cloned()
}

fn event_matches_clause(state: &HubState, clause: &PreparedClause, event: &WaitEvent) -> bool {
    source_matches_clause(state, clause, &event.source_id)
        && event
            .payload
            .as_object()
            .is_some_and(|payload| clause.matcher.matches(payload))
}

fn source_matches_clause(
    state: &HubState,
    clause: &PreparedClause,
    source_id: &WaitSourceId,
) -> bool {
    match &clause.selection {
        WaitSourceSelection::Any => state.sources.get(source_id).is_some_and(|source| {
            clause
                .matcher
                .fields()
                .keys()
                .all(|field| source.descriptor.matcher_fields().contains(field))
        }),
        WaitSourceSelection::Sources { source_ids } => source_ids.contains(source_id),
    }
}

pub(crate) fn all_selected_sources_closed(state: &HubState, query: &PreparedQuery) -> bool {
    query
        .clauses
        .iter()
        .all(|clause| clause_sources_closed(state, clause))
}

fn clause_sources_closed(state: &HubState, clause: &PreparedClause) -> bool {
    match &clause.selection {
        WaitSourceSelection::Sources { source_ids } => source_ids.iter().all(|source_id| {
            state
                .sources
                .get(source_id)
                .is_none_or(|source| !source.open)
        }),
        WaitSourceSelection::Any => {
            let compatible = state.sources.values().filter(|source| {
                clause
                    .matcher
                    .fields()
                    .keys()
                    .all(|field| source.descriptor.matcher_fields().contains(field))
            });
            let mut found = false;
            for source in compatible {
                found = true;
                if source.open {
                    return false;
                }
            }
            found
        }
    }
}

pub(crate) fn validate_payload(
    descriptor: &WaitSourceDescriptor,
    payload: &Value,
) -> Result<(), WaitError> {
    let payload = payload.as_object().ok_or(WaitError::PayloadMustBeObject)?;
    for (field, value) in payload {
        if !descriptor.public_fields().contains(field) {
            return Err(WaitError::UnknownPayloadField(field.clone()));
        }
        if descriptor.matcher_fields().contains(field) && !crate::types::is_scalar(value) {
            return Err(WaitError::NonScalarPublishedMatcherField(field.clone()));
        }
    }
    Ok(())
}

pub(crate) fn active_task_count(state: &HubState) -> usize {
    state
        .tasks
        .values()
        .filter(|task| !task.snapshot.status.is_terminal())
        .count()
}

pub(crate) fn active_owner_task_count(state: &HubState, owner_id: &WaitOwnerId) -> usize {
    state
        .tasks
        .values()
        .filter(|task| &task.snapshot.owner_id == owner_id && !task.snapshot.status.is_terminal())
        .count()
}

fn earliest_deadline(state: &HubState) -> Option<Instant> {
    state
        .tasks
        .values()
        .filter(|task| !task.snapshot.status.is_terminal())
        .map(|task| task.deadline)
        .min()
}

pub(crate) fn task_snapshots(state: &HubState) -> Vec<WaitTaskSnapshot> {
    state
        .task_order
        .iter()
        .filter_map(|task_id| state.tasks.get(task_id))
        .map(|task| task.snapshot.clone())
        .collect()
}

pub(crate) fn ensure_open(state: &HubState) -> Result<(), WaitError> {
    if state.shutdown {
        Err(WaitError::HubClosed)
    } else {
        Ok(())
    }
}
