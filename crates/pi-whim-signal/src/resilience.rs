use crate::{Observer, ScheduledTask, Signal, Subscription};
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;

/// Backoff calculation for bounded retry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetryBackoff {
    /// Retry immediately through the supplied scheduler.
    Immediate,
    /// Wait the same duration before every retry.
    Fixed(Duration),
    /// Multiply the initial delay by `multiplier` for each attempt.
    Exponential {
        /// Delay before the first retry.
        initial: Duration,
        /// Integer multiplier applied after every retry.
        multiplier: u32,
        /// Maximum delay.
        max: Duration,
    },
}

impl RetryBackoff {
    /// Computes the delay for a one-based retry attempt.
    pub fn delay(self, attempt: usize) -> Duration {
        match self {
            Self::Immediate => Duration::ZERO,
            Self::Fixed(delay) => delay,
            Self::Exponential {
                initial,
                multiplier,
                max,
            } => {
                let mut delay = initial;
                for _ in 1..attempt {
                    delay = delay.saturating_mul(multiplier);
                    if delay >= max {
                        return max;
                    }
                }
                delay.min(max)
            }
        }
    }
}

/// A finite retry policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Maximum number of retries after the initial subscription.
    pub max_retries: usize,
    /// Delay strategy between attempts.
    pub backoff: RetryBackoff,
}

impl RetryPolicy {
    /// Creates a finite policy.  `max_retries` is capped to keep the policy bounded.
    pub fn new(max_retries: usize, backoff: RetryBackoff) -> Self {
        Self {
            max_retries: max_retries.min(1_000_000),
            backoff,
        }
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(0, RetryBackoff::Immediate)
    }
}

impl<T, E> Signal<T, E> {
    /// Retries a source at most `policy.max_retries` times.
    ///
    /// Signals are hot handles, so retrying a terminal signal is useful only when
    /// the source's subscription semantics can produce another attempt.  The
    /// operator still enforces a finite retry bound and never creates an unbounded
    /// retry loop.
    pub fn retry<S>(&self, policy: RetryPolicy, scheduler: S) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        S: crate::Scheduler + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(RetryState {
            attempt: 0,
            terminated: false,
            current: None,
            scheduled: None,
        }));
        start_retry_subscription(
            self.clone(),
            policy,
            Arc::new(scheduler),
            state,
            output.downgrade(),
        );
        output
    }

    /// Replaces the source after its first error with a handler-provided signal.
    pub fn catch<F>(&self, handler: F) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(E) -> Signal<T, E> + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(CatchState {
            switched: false,
            terminated: false,
            fallback: None,
        }));
        let handler = Arc::new(handler);
        let subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let handler = handler.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let weak = weak.clone();
                    move |value| {
                        weak.emit(value);
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |error| {
                        let should_switch = {
                            let mut state = state.lock();
                            if state.switched || state.terminated {
                                false
                            } else {
                                state.switched = true;
                                true
                            }
                        };
                        if should_switch {
                            let fallback = handler(error);
                            start_catch_fallback(fallback, state.clone(), weak.clone());
                        }
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move || {
                        let should_complete = {
                            let mut state = state.lock();
                            if state.terminated {
                                false
                            } else {
                                state.terminated = true;
                                true
                            }
                        };
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

    /// Replaces an upstream error with a fixed fallback signal.
    pub fn fallback(&self, fallback: Signal<T, E>) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.catch(move |_| fallback.clone())
    }
}

struct RetryState {
    attempt: usize,
    terminated: bool,
    current: Option<Subscription>,
    scheduled: Option<ScheduledTask>,
}

fn start_retry_subscription<T, E, S>(
    source: Signal<T, E>,
    policy: RetryPolicy,
    scheduler: Arc<S>,
    state: Arc<Mutex<RetryState>>,
    weak: crate::WeakSignal<T, E>,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    S: crate::Scheduler + 'static,
{
    let next_source = source.clone();
    let subscription = source.subscribe(Observer::with_callbacks(
        {
            let weak = weak.clone();
            move |value| {
                weak.emit(value);
            }
        },
        {
            let weak = weak.clone();
            let state = state.clone();
            let scheduler = scheduler.clone();
            move |error| {
                let action = {
                    let mut state = state.lock();
                    if state.terminated {
                        RetryAction::Ignore
                    } else if state.attempt < policy.max_retries {
                        state.attempt += 1;
                        RetryAction::Schedule(policy.backoff.delay(state.attempt))
                    } else {
                        state.terminated = true;
                        RetryAction::Fail
                    }
                };
                match action {
                    RetryAction::Ignore => {}
                    RetryAction::Fail => {
                        weak.error(error);
                        terminate_retry(&state);
                    }
                    RetryAction::Schedule(delay) => {
                        let source = next_source.clone();
                        let state_for_task = state.clone();
                        let weak_for_task = weak.clone();
                        let scheduler_for_task = scheduler.clone();
                        let handle = crate::scheduler::schedule(&*scheduler, delay, move || {
                            start_retry_subscription(
                                source,
                                policy,
                                scheduler_for_task,
                                state_for_task.clone(),
                                weak_for_task,
                            );
                        });
                        let mut state = state.lock();
                        if state.terminated {
                            handle.cancel();
                        } else {
                            state.scheduled = Some(handle);
                        }
                    }
                }
            }
        },
        {
            let weak = weak.clone();
            let state = state.clone();
            move || {
                let should_complete = {
                    let mut state = state.lock();
                    if state.terminated {
                        false
                    } else {
                        state.terminated = true;
                        true
                    }
                };
                if should_complete {
                    weak.complete();
                    terminate_retry(&state);
                }
            }
        },
    ));
    let mut state_guard = state.lock();
    if state_guard.terminated {
        drop(state_guard);
        drop(subscription);
    } else {
        state_guard.current = Some(subscription);
    }
}

enum RetryAction {
    Ignore,
    Fail,
    Schedule(Duration),
}

fn terminate_retry(state: &Arc<Mutex<RetryState>>) {
    let (current, scheduled) = {
        let mut state = state.lock();
        (state.current.take(), state.scheduled.take())
    };
    drop(current);
    if let Some(scheduled) = scheduled {
        scheduled.cancel();
    }
}

struct CatchState {
    switched: bool,
    terminated: bool,
    fallback: Option<Subscription>,
}

fn start_catch_fallback<T, E>(
    fallback: Signal<T, E>,
    state: Arc<Mutex<CatchState>>,
    weak: crate::WeakSignal<T, E>,
) where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let subscription = fallback.subscribe(Observer::with_callbacks(
        {
            let weak = weak.clone();
            move |value| {
                weak.emit(value);
            }
        },
        {
            let weak = weak.clone();
            let state = state.clone();
            move |error| {
                let should_error = {
                    let mut state = state.lock();
                    if state.terminated {
                        false
                    } else {
                        state.terminated = true;
                        true
                    }
                };
                if should_error {
                    weak.error(error);
                }
            }
        },
        {
            let weak = weak.clone();
            let state = state.clone();
            move || {
                let should_complete = {
                    let mut state = state.lock();
                    if state.terminated {
                        false
                    } else {
                        state.terminated = true;
                        true
                    }
                };
                if should_complete {
                    weak.complete();
                }
            }
        },
    ));
    let mut state = state.lock();
    if state.terminated {
        drop(state);
        drop(subscription);
    } else {
        state.fallback = Some(subscription);
    }
}
