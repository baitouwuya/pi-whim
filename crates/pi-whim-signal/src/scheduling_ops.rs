use crate::{Observer, ScheduledTask, Signal, SignalError};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// Controls which edges a throttle emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottleOptions {
    /// Emit the first value in each window immediately.
    pub leading: bool,
    /// Emit the latest value at the end of each window.
    pub trailing: bool,
}

impl ThrottleOptions {
    /// Enables both leading and trailing emissions.
    pub const fn leading_and_trailing() -> Self {
        Self {
            leading: true,
            trailing: true,
        }
    }

    /// Enables leading emissions only.
    pub const fn leading_only() -> Self {
        Self {
            leading: true,
            trailing: false,
        }
    }

    /// Enables trailing emissions only.
    pub const fn trailing_only() -> Self {
        Self {
            leading: false,
            trailing: true,
        }
    }
}

impl Default for ThrottleOptions {
    fn default() -> Self {
        Self::leading_and_trailing()
    }
}

impl<T, E> Signal<T, E> {
    /// Emits at most `count` values, then completes.
    pub fn take(&self, count: usize) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        if count == 0 {
            output.complete();
            return output;
        }
        let state = Arc::new(Mutex::new(count));
        let subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let weak = weak.clone();
                    move |value: T| {
                        let complete = {
                            let mut remaining = state.lock();
                            if *remaining == 0 {
                                return;
                            }
                            *remaining -= 1;
                            *remaining == 0
                        };
                        weak.emit(value);
                        if complete {
                            weak.complete();
                        }
                    }
                },
                {
                    let weak = weak.clone();
                    move |error| {
                        weak.error(error);
                    }
                },
                move || {
                    weak.complete();
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }

    /// Emits one value, then completes.
    pub fn once(&self) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.take(1)
    }

    /// Completes when `notifier` emits its first value.
    pub fn take_until<U>(&self, notifier: &Signal<U, E>) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let source_subscription = {
            let weak = output.downgrade();
            self.subscribe(Observer::with_callbacks(
                {
                    let weak = weak.clone();
                    move |value| {
                        weak.emit(value);
                    }
                },
                {
                    let weak = weak.clone();
                    move |error| {
                        weak.error(error);
                    }
                },
                move || {
                    weak.complete();
                },
            ))
        };
        let notifier_subscription = {
            let weak = output.downgrade();
            notifier.subscribe(Observer::with_callbacks(
                {
                    let weak = weak.clone();
                    move |_| {
                        weak.complete();
                    }
                },
                move |error| {
                    weak.error(error);
                },
                || {},
            ))
        };
        output.keep_subscription(source_subscription);
        output.keep_subscription(notifier_subscription);
        output
    }

    /// Delays every notification by `duration` on `scheduler`.
    pub fn delay<S>(&self, duration: Duration, scheduler: S) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        let (output, _) = Signal::channel();
        let scheduler = Arc::new(scheduler);
        let subscription = {
            let weak = output.downgrade();
            let scheduler = scheduler.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let weak = weak.clone();
                    let scheduler = scheduler.clone();
                    move |value| {
                        let weak = weak.clone();
                        crate::scheduler::schedule(&*scheduler, duration, move || {
                            weak.emit(value);
                        });
                    }
                },
                {
                    let weak = weak.clone();
                    let scheduler = scheduler.clone();
                    move |error| {
                        let weak = weak.clone();
                        crate::scheduler::schedule(&*scheduler, duration, move || {
                            weak.error(error);
                        });
                    }
                },
                move || {
                    let weak = weak.clone();
                    crate::scheduler::schedule(&*scheduler, duration, move || {
                        weak.complete();
                    });
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }

    /// Emits only the latest value after a quiet period.
    pub fn debounce<S>(&self, duration: Duration, scheduler: S) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(DebounceState {
            generation: 0,
            pending: None,
            timer: None,
            done: false,
        }));
        let scheduler = Arc::new(scheduler);
        let subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let scheduler = scheduler.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let state = state.clone();
                    let scheduler = scheduler.clone();
                    let weak = weak.clone();
                    move |value| {
                        let (generation, previous) = {
                            let mut state = state.lock();
                            if state.done {
                                return;
                            }
                            state.generation = state.generation.wrapping_add(1);
                            state.pending = Some(value);
                            (state.generation, state.timer.take())
                        };
                        if let Some(previous) = previous {
                            previous.cancel();
                        }
                        let state_for_task = state.clone();
                        let weak_for_task = weak.clone();
                        let handle = crate::scheduler::schedule(&*scheduler, duration, move || {
                            let (pending, complete) = {
                                let mut state = state_for_task.lock();
                                if state.generation != generation {
                                    (None, false)
                                } else {
                                    let pending = state.pending.take();
                                    let complete = state.done;
                                    state.timer = None;
                                    (pending, complete)
                                }
                            };
                            if let Some(pending) = pending {
                                weak_for_task.emit(pending);
                            }
                            if complete {
                                weak_for_task.complete();
                            }
                        });
                        let mut state = state.lock();
                        if state.generation == generation && !state.done {
                            state.timer = Some(handle);
                        } else {
                            handle.cancel();
                        }
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |error| {
                        let timer = {
                            let mut state = state.lock();
                            state.done = true;
                            state.pending = None;
                            state.timer.take()
                        };
                        if let Some(timer) = timer {
                            timer.cancel();
                        }
                        weak.error(error);
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move || {
                        let (timer, has_pending) = {
                            let mut state = state.lock();
                            state.done = true;
                            (state.timer.clone(), state.pending.is_some())
                        };
                        if !has_pending {
                            if let Some(timer) = timer {
                                timer.cancel();
                            }
                            weak.complete();
                        }
                    }
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }

    /// Throttles values with configurable leading and trailing edges.
    pub fn throttle<S>(
        &self,
        duration: Duration,
        scheduler: S,
        options: ThrottleOptions,
    ) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(ThrottleState {
            open: false,
            pending: None,
            timer: None,
            done: false,
        }));
        let scheduler = Arc::new(scheduler);
        let subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let scheduler = scheduler.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    let scheduler = scheduler.clone();
                    move |value: T| {
                        let (emit, start_timer) = {
                            let mut state = state.lock();
                            if state.done {
                                return;
                            }
                            if !state.open {
                                state.open = true;
                                let emit = options.leading.then_some(value.clone());
                                if !options.leading && options.trailing {
                                    state.pending = Some(value);
                                }
                                (emit, true)
                            } else if options.trailing {
                                state.pending = Some(value);
                                (None, false)
                            } else {
                                (None, false)
                            }
                        };
                        if let Some(value) = emit {
                            weak.emit(value);
                        }
                        if start_timer {
                            schedule_throttle_close(
                                duration,
                                scheduler.clone(),
                                state.clone(),
                                weak.clone(),
                                options,
                            );
                        }
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |error| {
                        let timer = {
                            let mut state = state.lock();
                            state.done = true;
                            state.pending = None;
                            state.timer.take()
                        };
                        if let Some(timer) = timer {
                            timer.cancel();
                        }
                        weak.error(error);
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move || {
                        let (timer, pending) = {
                            let mut state = state.lock();
                            state.done = true;
                            state.open = false;
                            (state.timer.take(), state.pending.take())
                        };
                        if let Some(timer) = timer {
                            timer.cancel();
                        }
                        if let Some(pending) = pending {
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

    /// Throttles with both leading and trailing edges enabled.
    pub fn throttle_leading_trailing<S>(&self, duration: Duration, scheduler: S) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        self.throttle(duration, scheduler, ThrottleOptions::default())
    }

    /// Adds a timeout error when no value arrives within `duration`.
    pub fn timeout<S>(&self, duration: Duration, scheduler: S) -> Signal<T, SignalError<E>>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        timeout_impl(
            self,
            duration,
            scheduler,
            move || SignalError::Timeout(duration),
            SignalError::Source,
        )
    }

    /// Adds a timeout using a caller-provided error value.
    pub fn timeout_with<S, F>(
        &self,
        duration: Duration,
        scheduler: S,
        timeout_error: F,
    ) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
        F: Fn() -> E + Send + Sync + 'static,
    {
        timeout_impl(self, duration, scheduler, timeout_error, |error: E| error)
    }

    /// Drops consecutive values equal to the previous value.
    pub fn distinct_until_changed(&self) -> Signal<T, E>
    where
        T: Clone + PartialEq + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let last = Arc::new(Mutex::new(None::<T>));
        let subscription = {
            let weak = output.downgrade();
            let last = last.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let weak = weak.clone();
                    move |value| {
                        let emit = {
                            let mut last = last.lock();
                            if last.as_ref().is_some_and(|previous| previous == &value) {
                                false
                            } else {
                                *last = Some(value.clone());
                                true
                            }
                        };
                        if emit {
                            weak.emit(value);
                        }
                    }
                },
                {
                    let weak = weak.clone();
                    move |error| {
                        weak.error(error);
                    }
                },
                move || {
                    weak.complete();
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }

    /// Coalesces concurrent or recursive values to the latest pending value.
    pub fn conflate_latest(&self) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(ConflateState {
            busy: false,
            pending: None,
            done: false,
            error: None,
        }));
        let subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |value| {
                        let (start, current) = {
                            let mut state = state.lock();
                            if state.done {
                                (false, None)
                            } else if state.busy {
                                state.pending = Some(value);
                                (false, None)
                            } else {
                                state.busy = true;
                                (true, Some(value))
                            }
                        };
                        if start && let Some(current) = current {
                            drain_conflate(current, state.clone(), weak.clone());
                        }
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |error| {
                        let start = {
                            let mut state = state.lock();
                            if state.done {
                                false
                            } else {
                                state.done = true;
                                state.error = Some(error);
                                !state.busy && {
                                    state.busy = true;
                                    true
                                }
                            }
                        };
                        if start {
                            finish_conflate(state.clone(), weak.clone());
                        }
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move || {
                        let start = {
                            let mut state = state.lock();
                            if state.done {
                                false
                            } else {
                                state.done = true;
                                !state.busy && {
                                    state.busy = true;
                                    true
                                }
                            }
                        };
                        if start {
                            finish_conflate(state.clone(), weak.clone());
                        }
                    }
                },
            ))
        };
        output.keep_subscription(subscription);
        output
    }
}

fn timeout_impl<T, E, O, S, F, M>(
    source: &Signal<T, E>,
    duration: Duration,
    scheduler: S,
    timeout_error: F,
    map_error: M,
) -> Signal<T, O>
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    O: Clone + Send + Sync + 'static,
    S: crate::Scheduler + 'static,
    F: Fn() -> O + Send + Sync + 'static,
    M: Fn(E) -> O + Send + Sync + 'static,
{
    let (output, _) = Signal::channel();
    let state = Arc::new(Mutex::new(TimeoutState {
        generation: 0,
        timer: None,
        terminated: false,
    }));
    let scheduler = Arc::new(scheduler);
    let timeout_error = Arc::new(timeout_error);
    let map_error = Arc::new(map_error);
    schedule_timeout(
        duration,
        scheduler.clone(),
        state.clone(),
        output.downgrade(),
        timeout_error.clone(),
    );
    let subscription = {
        let state = state.clone();
        let scheduler = scheduler.clone();
        let timeout_error = timeout_error.clone();
        let map_error = map_error.clone();
        let weak = output.downgrade();
        source.subscribe(Observer::with_callbacks(
            {
                let state = state.clone();
                let scheduler = scheduler.clone();
                let timeout_error = timeout_error.clone();
                let weak = weak.clone();
                move |value| {
                    if weak.emit(value) {
                        schedule_timeout(
                            duration,
                            scheduler.clone(),
                            state.clone(),
                            weak.clone(),
                            timeout_error.clone(),
                        );
                    }
                }
            },
            {
                let state = state.clone();
                let map_error = map_error.clone();
                let weak = weak.clone();
                move |error| {
                    let (timer, should_error) = {
                        let mut state = state.lock();
                        if state.terminated {
                            (None, false)
                        } else {
                            state.terminated = true;
                            (state.timer.take(), true)
                        }
                    };
                    if let Some(timer) = timer {
                        timer.cancel();
                    }
                    if should_error {
                        weak.error(map_error(error));
                    }
                }
            },
            {
                let state = state.clone();
                let weak = weak.clone();
                move || {
                    let (timer, should_complete) = {
                        let mut state = state.lock();
                        if state.terminated {
                            (None, false)
                        } else {
                            state.terminated = true;
                            (state.timer.take(), true)
                        }
                    };
                    if let Some(timer) = timer {
                        timer.cancel();
                    }
                    if should_complete {
                        weak.complete();
                    }
                }
            },
        ))
    };
    output.keep_subscription(subscription);
    output
}

struct DebounceState<T> {
    generation: u64,
    pending: Option<T>,
    timer: Option<ScheduledTask>,
    done: bool,
}

struct ThrottleState<T> {
    open: bool,
    pending: Option<T>,
    timer: Option<ScheduledTask>,
    done: bool,
}

struct TimeoutState {
    generation: u64,
    timer: Option<ScheduledTask>,
    terminated: bool,
}

struct ConflateState<T, E> {
    busy: bool,
    pending: Option<T>,
    done: bool,
    error: Option<E>,
}

fn schedule_throttle_close<T, E, S>(
    duration: Duration,
    scheduler: Arc<S>,
    state: Arc<Mutex<ThrottleState<T>>>,
    weak: crate::WeakSignal<T, E>,
    options: ThrottleOptions,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    S: crate::Scheduler + 'static,
{
    let state_for_task = state.clone();
    let weak_for_task = weak.clone();
    let handle = crate::scheduler::schedule(&*scheduler, duration, move || {
        let pending = {
            let mut state = state_for_task.lock();
            state.timer = None;
            if state.done {
                None
            } else {
                state.open = false;
                if options.trailing {
                    state.pending.take()
                } else {
                    state.pending = None;
                    None
                }
            }
        };
        if let Some(pending) = pending {
            weak_for_task.emit(pending);
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

fn schedule_timeout<T, E, S, F>(
    duration: Duration,
    scheduler: Arc<S>,
    state: Arc<Mutex<TimeoutState>>,
    weak: crate::WeakSignal<T, E>,
    timeout_error: Arc<F>,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    S: crate::Scheduler + 'static,
    F: Fn() -> E + Send + Sync + 'static,
{
    let (generation, previous) = {
        let mut state = state.lock();
        if state.terminated {
            return;
        }
        state.generation = state.generation.wrapping_add(1);
        (state.generation, state.timer.take())
    };
    if let Some(previous) = previous {
        previous.cancel();
    }
    let state_for_task = state.clone();
    let weak_for_task = weak.clone();
    let timeout_error_for_task = timeout_error.clone();
    let handle = crate::scheduler::schedule(&*scheduler, duration, move || {
        let should_timeout = {
            let mut state = state_for_task.lock();
            if state.generation == generation && !state.terminated {
                state.terminated = true;
                state.timer = None;
                true
            } else {
                false
            }
        };
        if should_timeout {
            weak_for_task.error(timeout_error_for_task());
        }
    });
    let mut state = state.lock();
    if state.generation == generation && !state.terminated {
        state.timer = Some(handle);
    } else {
        handle.cancel();
    }
}

fn drain_conflate<T, E>(
    mut current: T,
    state: Arc<Mutex<ConflateState<T, E>>>,
    weak: crate::WeakSignal<T, E>,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    loop {
        weak.emit(current);
        let next = {
            let mut state = state.lock();
            if let Some(error) = state.error.take() {
                state.busy = false;
                weak.error(error);
                return;
            }
            if let Some(next) = state.pending.take() {
                Some(next)
            } else {
                state.busy = false;
                if state.done {
                    weak.complete();
                }
                None
            }
        };
        let Some(next) = next else {
            return;
        };
        current = next;
    }
}

fn finish_conflate<T, E>(state: Arc<Mutex<ConflateState<T, E>>>, weak: crate::WeakSignal<T, E>)
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let error = {
        let mut state = state.lock();
        state.busy = false;
        state.error.take()
    };
    if let Some(error) = error {
        weak.error(error);
    } else {
        weak.complete();
    }
}
