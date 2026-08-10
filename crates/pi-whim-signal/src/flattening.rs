use crate::{Observer, Signal, Subscription, WeakSignal};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

/// Bounds the number of active and queued inner streams for `flat_map`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlatMapOptions {
    /// Maximum number of concurrently subscribed inner streams.
    pub max_concurrency: usize,
    /// Maximum number of outer values waiting for an inner slot.
    pub max_pending: usize,
}

impl FlatMapOptions {
    /// Creates options with a bounded pending queue.
    pub fn new(max_concurrency: usize, max_pending: usize) -> Self {
        Self {
            max_concurrency: max_concurrency.max(1),
            max_pending,
        }
    }
}

impl Default for FlatMapOptions {
    fn default() -> Self {
        Self::new(1, 1024)
    }
}

impl<T, E> Signal<T, E> {
    /// Maps each outer value to an inner signal with bounded concurrency.
    ///
    /// When all inner slots are full, the oldest pending outer value is dropped
    /// once `max_pending` is reached.  This keeps the operator bounded without
    /// introducing an implicit unbounded queue.
    pub fn flat_map<U, F>(&self, max_concurrency: usize, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> Signal<U, E> + Send + Sync + 'static,
    {
        self.flat_map_with_options(
            FlatMapOptions::new(max_concurrency, max_concurrency),
            mapper,
        )
    }

    /// Maps each outer value to an inner signal using explicit finite bounds.
    pub fn flat_map_with_options<U, F>(&self, options: FlatMapOptions, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> Signal<U, E> + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(FlatState {
            active: 0,
            pending: VecDeque::new(),
            outer_done: false,
            terminated: false,
            subscriptions: Vec::new(),
        }));
        let mapper = Arc::new(mapper);
        let outer_subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let mapper = mapper.clone();
            let options = FlatMapOptions::new(options.max_concurrency, options.max_pending);
            self.subscribe(Observer::with_callbacks(
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |value| {
                        let start = {
                            let mut state = state.lock();
                            if state.terminated {
                                None
                            } else if state.active < options.max_concurrency {
                                state.active += 1;
                                Some(value)
                            } else {
                                if options.max_pending > 0 {
                                    if state.pending.len() >= options.max_pending {
                                        state.pending.pop_front();
                                    }
                                    state.pending.push_back(value);
                                }
                                None
                            }
                        };
                        if let Some(value) = start {
                            start_flat_inner(value, &mapper, state.clone(), weak.clone(), options);
                        }
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |error| {
                        terminate_flat(&state, &weak, Some(error));
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move || {
                        let should_complete = {
                            let mut state = state.lock();
                            state.outer_done = true;
                            if state.active == 0 && !state.terminated {
                                state.terminated = true;
                                true
                            } else {
                                false
                            }
                        };
                        if should_complete {
                            weak.complete();
                            drop_flat_subscriptions(&state);
                        }
                    }
                },
            ))
        };
        output.keep_subscription(outer_subscription);
        output
    }

    /// Concatenates mapped inner signals in outer registration order.
    pub fn concat_map<U, F>(&self, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> Signal<U, E> + Send + Sync + 'static,
    {
        self.flat_map_with_options(FlatMapOptions::new(1, 1024), mapper)
    }

    /// Concatenates mapped inner signals with an explicit finite pending bound.
    pub fn concat_map_with_capacity<U, F>(&self, capacity: usize, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> Signal<U, E> + Send + Sync + 'static,
    {
        self.flat_map_with_options(FlatMapOptions::new(1, capacity), mapper)
    }

    /// Switches to the newest mapped inner signal and unsubscribes the previous one.
    pub fn switch_map<U, F>(&self, mapper: F) -> Signal<U, E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(T) -> Signal<U, E> + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(SwitchState {
            generation: 0,
            outer_done: false,
            terminated: false,
            current: None,
        }));
        let mapper = Arc::new(mapper);
        let outer_subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let mapper = mapper.clone();
            self.subscribe(Observer::with_callbacks(
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |value| {
                        let inner = mapper(value);
                        let (generation, previous) = {
                            let mut state = state.lock();
                            if state.terminated {
                                return;
                            }
                            state.generation = state.generation.wrapping_add(1);
                            (state.generation, state.current.take())
                        };
                        drop(previous);
                        start_switch_inner(inner, generation, state.clone(), weak.clone());
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move |error| {
                        let previous = terminate_switch(&state);
                        weak.error(error);
                        drop(previous);
                    }
                },
                {
                    let state = state.clone();
                    let weak = weak.clone();
                    move || {
                        let should_complete = {
                            let mut state = state.lock();
                            state.outer_done = true;
                            if state.current.is_none() && !state.terminated {
                                state.terminated = true;
                                true
                            } else {
                                false
                            }
                        };
                        if should_complete {
                            weak.complete();
                        }
                    }
                },
            ))
        };
        output.keep_subscription(outer_subscription);
        output
    }
}

struct FlatState<T> {
    active: usize,
    pending: VecDeque<T>,
    outer_done: bool,
    terminated: bool,
    subscriptions: Vec<Subscription>,
}

struct SwitchState {
    generation: u64,
    outer_done: bool,
    terminated: bool,
    current: Option<Subscription>,
}

#[allow(clippy::only_used_in_recursion)]
fn start_flat_inner<T, U, E, F>(
    value: T,
    mapper: &Arc<F>,
    state: Arc<Mutex<FlatState<T>>>,
    weak: WeakSignal<U, E>,
    options: FlatMapOptions,
) where
    T: Clone + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    F: Fn(T) -> Signal<U, E> + Send + Sync + 'static,
{
    let inner = mapper(value);
    let subscription = {
        let weak = weak.clone();
        let state_for_next = state.clone();
        let state_for_error = state.clone();
        let mapper = mapper.clone();
        inner.subscribe(Observer::with_callbacks(
            {
                let weak = weak.clone();
                move |value| {
                    weak.emit(value);
                }
            },
            {
                let weak = weak.clone();
                let state = state_for_error.clone();
                move |error| {
                    terminate_flat(&state, &weak, Some(error));
                }
            },
            move || {
                let next = {
                    let mut state = state_for_next.lock();
                    state.subscriptions.retain(Subscription::is_active);
                    if state.terminated {
                        None
                    } else {
                        state.active = state.active.saturating_sub(1);
                        if let Some(value) = state.pending.pop_front() {
                            state.active += 1;
                            Some(value)
                        } else {
                            None
                        }
                    }
                };
                if let Some(value) = next {
                    start_flat_inner(
                        value,
                        &mapper,
                        state_for_next.clone(),
                        weak.clone(),
                        options,
                    );
                    return;
                }
                let should_complete = {
                    let mut state = state_for_next.lock();
                    if state.outer_done && state.active == 0 && !state.terminated {
                        state.terminated = true;
                        true
                    } else {
                        false
                    }
                };
                if should_complete {
                    weak.complete();
                    drop_flat_subscriptions(&state_for_next);
                }
            },
        ))
    };
    if inner.is_terminated() {
        drop(subscription);
        return;
    }
    let drop_now = {
        let mut state = state.lock();
        if state.terminated {
            true
        } else {
            state.subscriptions.retain(Subscription::is_active);
            state.subscriptions.push(subscription);
            false
        }
    };
    if drop_now {
        // `subscription` is dropped at the end of this branch.
    }
}

fn terminate_flat<T, U, E>(
    state: &Arc<Mutex<FlatState<T>>>,
    weak: &WeakSignal<U, E>,
    error: Option<E>,
) where
    T: Clone + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let subscriptions = {
        let mut state = state.lock();
        if state.terminated {
            Vec::new()
        } else {
            state.terminated = true;
            std::mem::take(&mut state.subscriptions)
        }
    };
    if let Some(error) = error {
        weak.error(error);
    } else {
        weak.complete();
    }
    drop(subscriptions);
}

fn drop_flat_subscriptions<T>(state: &Arc<Mutex<FlatState<T>>>) {
    let subscriptions = {
        let mut state = state.lock();
        std::mem::take(&mut state.subscriptions)
    };
    drop(subscriptions);
}

fn start_switch_inner<U, E>(
    inner: Signal<U, E>,
    generation: u64,
    state: Arc<Mutex<SwitchState>>,
    weak: WeakSignal<U, E>,
) where
    U: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let subscription = {
        let state_for_callbacks = state.clone();
        let weak_for_next = weak.clone();
        inner.subscribe(Observer::with_callbacks(
            move |value| {
                weak_for_next.emit(value);
            },
            {
                let state = state.clone();
                let weak = weak.clone();
                move |error| {
                    let should_error = {
                        let mut state = state.lock();
                        if state.generation == generation && !state.terminated {
                            state.terminated = true;
                            state.current.take();
                            true
                        } else {
                            false
                        }
                    };
                    if should_error {
                        weak.error(error);
                    }
                }
            },
            {
                let state = state_for_callbacks.clone();
                let weak = weak.clone();
                move || {
                    let should_complete = {
                        let mut state = state.lock();
                        if state.generation != generation || state.terminated {
                            false
                        } else {
                            state.current = None;
                            if state.outer_done {
                                state.terminated = true;
                                true
                            } else {
                                false
                            }
                        }
                    };
                    if should_complete {
                        weak.complete();
                    }
                }
            },
        ))
    };
    if inner.is_terminated() {
        drop(subscription);
        return;
    }
    let previous = {
        let mut state = state.lock();
        if state.generation == generation && !state.terminated {
            state.current.replace(subscription)
        } else {
            Some(subscription)
        }
    };
    drop(previous);
}

fn terminate_switch(state: &Arc<Mutex<SwitchState>>) -> Option<Subscription> {
    let mut state = state.lock();
    state.terminated = true;
    state.current.take()
}
