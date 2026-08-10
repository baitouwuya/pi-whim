use crate::{Observer, Signal};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Behaviour when a finite buffer reaches capacity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverflowPolicy<E> {
    /// Remove the oldest buffered value and accept the new value.
    DropOldest,
    /// Drop the incoming value.
    DropNewest,
    /// Emit the current buffer before accepting the new value.
    Flush,
    /// Terminate the output with the supplied error.
    Error(E),
}

/// Capacity and overflow settings shared by buffer operators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BufferOptions<E> {
    /// Maximum number of values retained before overflow handling.
    pub capacity: usize,
    /// Action taken when `capacity` is reached.
    pub overflow: OverflowPolicy<E>,
}

impl<E> BufferOptions<E> {
    /// Creates finite buffer options.
    pub fn new(capacity: usize, overflow: OverflowPolicy<E>) -> Self {
        Self { capacity, overflow }
    }
}

impl<E> Default for BufferOptions<E>
where
    E: Default,
{
    fn default() -> Self {
        Self {
            capacity: 1024,
            overflow: OverflowPolicy::DropOldest,
        }
    }
}

impl<T, E> Signal<T, E> {
    /// Emits a vector every `count` values with finite overflow handling.
    pub fn buffer_count(&self, count: usize, options: BufferOptions<E>) -> Signal<Vec<T>, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let count = count.max(1);
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(VecDeque::<T>::new()));
        let options = Arc::new(options);
        let subscription = {
            let weak = output.downgrade();
            let state_for_value = state.clone();
            let options_for_value = options.clone();
            self.subscribe(Observer::with_callbacks(
                move |value| {
                    let (action, count_flush) = {
                        let mut buffer = state_for_value.lock();
                        let action = push_with_policy(&mut buffer, value, &options_for_value);
                        let count_flush = if matches!(action, BufferAction::Error(_)) {
                            None
                        } else if buffer.len() >= count {
                            Some(drain_buffer_inner(&mut buffer))
                        } else {
                            None
                        };
                        (action, count_flush)
                    };
                    handle_buffer_action(action, &weak, count);
                    if let Some(values) = count_flush
                        && !values.is_empty()
                    {
                        weak.emit(values);
                    }
                },
                {
                    let weak = output.downgrade();
                    let state = state.clone();
                    move |error| {
                        let pending = drain_buffer(&state);
                        if !pending.is_empty() {
                            weak.emit(pending);
                        }
                        weak.error(error);
                    }
                },
                {
                    let weak = output.downgrade();
                    let state = state.clone();
                    move || {
                        let pending = drain_buffer(&state);
                        if !pending.is_empty() {
                            weak.emit(pending);
                        }
                        weak.complete();
                    }
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }

    /// Emits the current finite buffer at fixed scheduler intervals.
    pub fn buffer_time<S>(
        &self,
        duration: Duration,
        scheduler: S,
        options: BufferOptions<E>,
    ) -> Signal<Vec<T>, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(TimeBufferState {
            buffer: VecDeque::new(),
            timer: None,
            done: false,
        }));
        let scheduler = Arc::new(scheduler);
        let options = Arc::new(options);
        if duration > Duration::ZERO && !scheduler.is_immediate() {
            schedule_time_buffer_flush(
                duration,
                scheduler.clone(),
                state.clone(),
                output.downgrade(),
                options.clone(),
            );
        }
        let subscription = {
            let weak = output.downgrade();
            let state_for_value = state.clone();
            let options_for_value = options.clone();
            let immediate = duration.is_zero() || scheduler.is_immediate();
            self.subscribe(Observer::with_callbacks(
                move |value| {
                    if immediate {
                        weak.emit(vec![value]);
                        return;
                    }
                    let action = {
                        let mut state = state_for_value.lock();
                        push_with_policy(&mut state.buffer, value, &options_for_value)
                    };
                    match action {
                        BufferAction::Error(error) => {
                            let timer = {
                                let mut state = state_for_value.lock();
                                state.done = true;
                                state.timer.take()
                            };
                            if let Some(timer) = timer {
                                timer.cancel();
                            }
                            weak.error(error);
                        }
                        action => handle_buffer_action(action, &weak, usize::MAX),
                    }
                },
                {
                    let weak = output.downgrade();
                    let state = state.clone();
                    move |error| {
                        let (timer, pending) = {
                            let mut state = state.lock();
                            state.done = true;
                            (state.timer.take(), drain_buffer_inner(&mut state.buffer))
                        };
                        if let Some(timer) = timer {
                            timer.cancel();
                        }
                        if !pending.is_empty() {
                            weak.emit(pending);
                        }
                        weak.error(error);
                    }
                },
                {
                    let weak = output.downgrade();
                    let state = state.clone();
                    move || {
                        let (timer, pending) = {
                            let mut state = state.lock();
                            state.done = true;
                            (state.timer.take(), drain_buffer_inner(&mut state.buffer))
                        };
                        if let Some(timer) = timer {
                            timer.cancel();
                        }
                        if !pending.is_empty() {
                            weak.emit(pending);
                        }
                        weak.complete();
                    }
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }
}

enum BufferAction<T, E> {
    None,
    Emit(Vec<T>),
    Error(E),
}

fn push_with_policy<T, E>(
    buffer: &mut VecDeque<T>,
    value: T,
    options: &BufferOptions<E>,
) -> BufferAction<T, E>
where
    E: Clone,
{
    if options.capacity == 0 {
        return match &options.overflow {
            OverflowPolicy::DropNewest => BufferAction::None,
            OverflowPolicy::DropOldest => BufferAction::None,
            OverflowPolicy::Flush => BufferAction::Emit(Vec::new()),
            OverflowPolicy::Error(error) => BufferAction::Error(error.clone()),
        };
    }
    if buffer.len() >= options.capacity {
        match &options.overflow {
            OverflowPolicy::DropOldest => {
                buffer.pop_front();
            }
            OverflowPolicy::DropNewest => return BufferAction::None,
            OverflowPolicy::Flush => {
                let pending = drain_buffer_inner(buffer);
                buffer.push_back(value);
                return BufferAction::Emit(pending);
            }
            OverflowPolicy::Error(error) => return BufferAction::Error(error.clone()),
        }
    }
    buffer.push_back(value);
    BufferAction::None
}

fn handle_buffer_action<T, E>(
    action: BufferAction<T, E>,
    weak: &crate::WeakSignal<Vec<T>, E>,
    count: usize,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    match action {
        BufferAction::None => {}
        BufferAction::Emit(values) => {
            if !values.is_empty() {
                weak.emit(values);
            }
        }
        BufferAction::Error(error) => {
            weak.error(error);
        }
    }
    if count != usize::MAX {
        // Count-based flushing is performed by the caller through this helper's
        // stateful companion below.
    }
}

fn drain_buffer<T>(state: &Arc<Mutex<VecDeque<T>>>) -> Vec<T> {
    let mut state = state.lock();
    drain_buffer_inner(&mut state)
}

fn drain_buffer_inner<T>(buffer: &mut VecDeque<T>) -> Vec<T> {
    buffer.drain(..).collect()
}

struct TimeBufferState<T> {
    buffer: VecDeque<T>,
    timer: Option<crate::ScheduledTask>,
    done: bool,
}

fn schedule_time_buffer_flush<T, E, S>(
    duration: Duration,
    scheduler: Arc<S>,
    state: Arc<Mutex<TimeBufferState<T>>>,
    weak: crate::WeakSignal<Vec<T>, E>,
    options: Arc<BufferOptions<E>>,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    S: crate::Scheduler + 'static,
{
    let task_state = state.clone();
    let task_weak = weak.clone();
    let task_scheduler = scheduler.clone();
    let task_options = options.clone();
    let handle = crate::scheduler::schedule(&*scheduler, duration, move || {
        let pending = {
            let mut state = task_state.lock();
            if state.done {
                None
            } else {
                Some(drain_buffer_inner(&mut state.buffer))
            }
        };
        if let Some(pending) = pending {
            if !pending.is_empty() {
                task_weak.emit(pending);
            }
            schedule_time_buffer_flush(
                duration,
                task_scheduler,
                task_state,
                task_weak,
                task_options,
            );
        }
    });
    let mut state = state.lock();
    if state.done {
        handle.cancel();
    } else {
        if let Some(previous) = state.timer.replace(handle) {
            previous.cancel();
        }
    }
}
