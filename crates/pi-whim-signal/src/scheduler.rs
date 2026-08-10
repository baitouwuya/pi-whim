use parking_lot::{Condvar, Mutex};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::io;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::{Arc, Weak};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

/// A cancellation handle returned by a scheduler.
#[derive(Clone)]
pub struct ScheduledTask {
    cancelled: Arc<AtomicBool>,
}

impl ScheduledTask {
    fn new() -> (Self, Arc<AtomicBool>) {
        let cancelled = Arc::new(AtomicBool::new(false));
        (
            Self {
                cancelled: cancelled.clone(),
            },
            cancelled,
        )
    }

    /// Cancels the task if it has not run yet.
    pub fn cancel(&self) {
        self.cancelled.store(true, AtomicOrdering::Release);
    }

    /// Returns whether the task has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(AtomicOrdering::Acquire)
    }
}

/// A scheduler for delayed callbacks.
pub trait Scheduler: Send + Sync {
    /// Returns scheduler-relative monotonic time.
    fn now(&self) -> Duration;

    /// Schedules a callback after `delay`.
    fn schedule_boxed(
        &self,
        delay: Duration,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> ScheduledTask;

    /// Returns whether this scheduler executes delayed work synchronously.
    ///
    /// Operators with recurring timers use this to avoid recursively scheduling
    /// an unbounded sequence on a scheduler that has no passage of time.
    fn is_immediate(&self) -> bool {
        false
    }
}

impl<S> Scheduler for Arc<S>
where
    S: Scheduler + ?Sized,
{
    fn now(&self) -> Duration {
        (**self).now()
    }

    fn schedule_boxed(
        &self,
        delay: Duration,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> ScheduledTask {
        (**self).schedule_boxed(delay, task)
    }

    fn is_immediate(&self) -> bool {
        (**self).is_immediate()
    }
}

/// A small helper for scheduling a typed closure without manually boxing it.
pub fn schedule<S, F>(scheduler: &S, delay: Duration, task: F) -> ScheduledTask
where
    S: Scheduler + ?Sized,
    F: FnOnce() + Send + 'static,
{
    scheduler.schedule_boxed(delay, Box::new(task))
}

/// A scheduler that executes every callback synchronously.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImmediateScheduler;

impl Scheduler for ImmediateScheduler {
    fn is_immediate(&self) -> bool {
        true
    }

    fn now(&self) -> Duration {
        Duration::ZERO
    }

    fn schedule_boxed(
        &self,
        _delay: Duration,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> ScheduledTask {
        let (handle, cancelled) = ScheduledTask::new();
        if !cancelled.load(AtomicOrdering::Acquire) {
            task();
        }
        handle
    }
}

struct TimedTask {
    when: Instant,
    sequence: u64,
    cancelled: Arc<AtomicBool>,
    task: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl PartialEq for TimedTask {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when && self.sequence == other.sequence
    }
}

impl Eq for TimedTask {}

impl Ord for TimedTask {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .when
            .cmp(&self.when)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for TimedTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct ThreadState {
    next_sequence: u64,
    shutting_down: bool,
    tasks: BinaryHeap<TimedTask>,
}

struct ThreadShared {
    start: Instant,
    state: Mutex<ThreadState>,
    wake: Condvar,
}

/// A scheduler backed by one worker thread and a timer heap.
///
/// Callbacks execute in registration order when their deadlines are equal.  The
/// worker never invokes callbacks while holding the scheduler mutex. Task
/// panics run the normal panic hook but are isolated so the worker can execute
/// later tasks. Clones keep the worker alive until the last scheduler handle is
/// dropped.
pub struct ThreadScheduler {
    inner: Arc<ThreadSchedulerInner>,
}

struct ThreadSchedulerInner {
    shared: Arc<ThreadShared>,
    worker: Mutex<Option<JoinHandle<()>>>,
    worker_id: ThreadId,
}

impl ThreadScheduler {
    /// Starts a scheduler worker with the default thread name.
    pub fn try_new() -> io::Result<Self> {
        Self::try_with_name("pi-whim-signal")
    }

    /// Starts a scheduler worker with a custom thread name.
    pub fn try_with_name(name: impl Into<String>) -> io::Result<Self> {
        let shared = Arc::new(ThreadShared {
            start: Instant::now(),
            state: Mutex::new(ThreadState {
                next_sequence: 0,
                shutting_down: false,
                tasks: BinaryHeap::new(),
            }),
            wake: Condvar::new(),
        });
        let weak = Arc::downgrade(&shared);
        let worker = thread::Builder::new()
            .name(name.into())
            .spawn(move || thread_worker(weak))?;
        let worker_id = worker.thread().id();
        Ok(Self {
            inner: Arc::new(ThreadSchedulerInner {
                shared,
                worker: Mutex::new(Some(worker)),
                worker_id,
            }),
        })
    }

    /// Requests worker shutdown and waits for it to finish.
    pub fn shutdown(&mut self) {
        self.inner.shutdown();
    }
}

impl ThreadSchedulerInner {
    fn shutdown(&self) {
        {
            let mut state = self.shared.state.lock();
            state.shutting_down = true;
            self.shared.wake.notify_all();
        }
        let worker = self.worker.lock().take();
        if let Some(worker) = worker
            && thread::current().id() != self.worker_id
        {
            let _ = worker.join();
        }
    }
}

impl Clone for ThreadScheduler {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl Drop for ThreadScheduler {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            self.inner.shutdown();
        }
    }
}

impl Scheduler for ThreadScheduler {
    fn now(&self) -> Duration {
        self.inner.shared.start.elapsed()
    }

    fn schedule_boxed(
        &self,
        delay: Duration,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> ScheduledTask {
        let (handle, cancelled) = ScheduledTask::new();
        let mut state = self.inner.shared.state.lock();
        if state.shutting_down {
            handle.cancel();
            return handle;
        }
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        state.tasks.push(TimedTask {
            when: match Instant::now().checked_add(delay) {
                Some(when) => when,
                None => Instant::now(),
            },
            sequence,
            cancelled,
            task: Some(task),
        });
        self.inner.shared.wake.notify_one();
        handle
    }
}

fn thread_worker(shared: Weak<ThreadShared>) {
    loop {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let task = {
            let mut state = shared.state.lock();
            loop {
                if state.shutting_down {
                    return;
                }
                let Some(top) = state.tasks.peek() else {
                    shared.wake.wait(&mut state);
                    continue;
                };
                if top.cancelled.load(AtomicOrdering::Acquire) {
                    state.tasks.pop();
                    continue;
                }
                let now = Instant::now();
                if top.when <= now {
                    break state.tasks.pop().and_then(|entry| entry.task);
                }
                let timeout = top.when.saturating_duration_since(now);
                shared.wake.wait_for(&mut state, timeout);
            }
        };
        if let Some(task) = task {
            let _ = catch_unwind(AssertUnwindSafe(task));
        }
    }
}

struct VirtualTask {
    when: Duration,
    sequence: u64,
    cancelled: Arc<AtomicBool>,
    task: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl PartialEq for VirtualTask {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when && self.sequence == other.sequence
    }
}

impl Eq for VirtualTask {}

impl Ord for VirtualTask {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .when
            .cmp(&self.when)
            .then_with(|| other.sequence.cmp(&self.sequence))
    }
}

impl PartialOrd for VirtualTask {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

struct VirtualState {
    now: Duration,
    next_sequence: u64,
    tasks: BinaryHeap<VirtualTask>,
}

/// A deterministic virtual-time scheduler for unit tests.
#[derive(Clone)]
pub struct TestScheduler {
    state: Arc<Mutex<VirtualState>>,
}

impl TestScheduler {
    /// Creates a scheduler whose clock starts at zero.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(VirtualState {
                now: Duration::ZERO,
                next_sequence: 0,
                tasks: BinaryHeap::new(),
            })),
        }
    }

    /// Advances virtual time and runs every task due by the target time.
    pub fn advance(&self, duration: Duration) {
        let target = {
            let state = self.state.lock();
            state.now.saturating_add(duration)
        };
        self.advance_to(target);
    }

    /// Advances virtual time to an absolute scheduler-relative instant.
    pub fn advance_to(&self, target: Duration) {
        let target = {
            let state = self.state.lock();
            target.max(state.now)
        };
        loop {
            let task = {
                let mut state = self.state.lock();
                loop {
                    let Some(top) = state.tasks.peek() else {
                        state.now = target;
                        break None;
                    };
                    if top.cancelled.load(AtomicOrdering::Acquire) {
                        state.tasks.pop();
                        continue;
                    }
                    if top.when > target {
                        state.now = target;
                        break None;
                    }
                    let Some(entry) = state.tasks.pop() else {
                        state.now = target;
                        break None;
                    };
                    state.now = entry.when;
                    break entry.task;
                }
            };
            let Some(task) = task else {
                return;
            };
            task();
        }
    }

    /// Runs all currently queued tasks, advancing virtual time as needed.
    pub fn run_until_idle(&self) {
        loop {
            let next = {
                let state = self.state.lock();
                state.tasks.peek().map(|task| task.when)
            };
            let Some(next) = next else {
                return;
            };
            let now = self.now();
            self.advance_to(next.max(now));
        }
    }

    /// Returns the number of queued, not-yet-cancelled tasks.
    pub fn pending_tasks(&self) -> usize {
        self.state
            .lock()
            .tasks
            .iter()
            .filter(|task| !task.cancelled.load(AtomicOrdering::Acquire))
            .count()
    }
}

impl Default for TestScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Scheduler for TestScheduler {
    fn now(&self) -> Duration {
        self.state.lock().now
    }

    fn schedule_boxed(
        &self,
        delay: Duration,
        task: Box<dyn FnOnce() + Send + 'static>,
    ) -> ScheduledTask {
        let (handle, cancelled) = ScheduledTask::new();
        let mut state = self.state.lock();
        let sequence = state.next_sequence;
        state.next_sequence = state.next_sequence.wrapping_add(1);
        let when = state.now.saturating_add(delay);
        state.tasks.push(VirtualTask {
            when,
            sequence,
            cancelled,
            task: Some(task),
        });
        handle
    }
}
