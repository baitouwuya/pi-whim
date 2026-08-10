use crate::{Observer, Signal};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;

impl<T, E> Signal<T, E> {
    /// Merges all sources in stable subscription order.
    pub fn merge<I>(sources: I) -> Signal<T, E>
    where
        I: IntoIterator<Item = Signal<T, E>>,
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let sources = sources.into_iter().collect::<Vec<_>>();
        if sources.is_empty() {
            output.complete();
            return output;
        }
        let state = Arc::new(Mutex::new(MergeState {
            completed: 0,
            total: sources.len(),
            terminated: false,
        }));
        for source in sources {
            let subscription = {
                let weak = output.downgrade();
                let state = state.clone();
                source.subscribe(Observer::with_callbacks(
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
                    {
                        let weak = weak.clone();
                        move || {
                            let should_complete = {
                                let mut state = state.lock();
                                state.completed += 1;
                                if state.completed == state.total && !state.terminated {
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
            output.keep_subscription(subscription);
        }
        output
    }

    /// Merges two sources and completes after both have completed.
    pub fn merge_two(&self, other: &Signal<T, E>) -> Signal<T, E>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        Signal::merge(vec![self.clone(), other.clone()])
    }

    /// Zips two sources, emitting one pair for each pair of values.
    pub fn zip<U>(&self, other: &Signal<U, E>) -> Signal<(T, U), E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(ZipState {
            left: VecDeque::new(),
            right: VecDeque::new(),
            left_done: false,
            right_done: false,
            terminated: false,
        }));
        let left_subscription = subscribe_zip_left(self, other, &output, &state);
        let right_subscription = subscribe_zip_right(self, other, &output, &state);
        output.keep_subscription(left_subscription);
        output.keep_subscription(right_subscription);
        output
    }

    /// Combines the latest value from two sources after both have emitted once.
    pub fn combine_latest<U>(&self, other: &Signal<U, E>) -> Signal<(T, U), E>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (output, _) = Signal::channel();
        let state = Arc::new(Mutex::new(CombineState::<T, U> {
            left: None,
            right: None,
            left_done: false,
            right_done: false,
            terminated: false,
        }));
        let left_subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let state_for_value = state.clone();
            self.subscribe(Observer::with_callbacks(
                move |value: T| {
                    let pair = {
                        let mut state = state_for_value.lock();
                        state.left = Some(value);
                        state
                            .left
                            .as_ref()
                            .zip(state.right.as_ref())
                            .map(|(left, right)| (left.clone(), right.clone()))
                    };
                    if let Some(pair) = pair {
                        weak.emit(pair);
                    }
                },
                {
                    let weak = output.downgrade();
                    move |error| {
                        weak.error(error);
                    }
                },
                {
                    let weak = output.downgrade();
                    let state = state.clone();
                    move || {
                        let should_complete = {
                            let mut state = state.lock();
                            state.left_done = true;
                            if (state.left.is_none() || state.right_done) && !state.terminated {
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
        let right_subscription = {
            let weak = output.downgrade();
            let state = state.clone();
            let state_for_value = state.clone();
            other.subscribe(Observer::with_callbacks(
                move |value: U| {
                    let pair = {
                        let mut state = state_for_value.lock();
                        state.right = Some(value);
                        state
                            .left
                            .as_ref()
                            .zip(state.right.as_ref())
                            .map(|(left, right)| (left.clone(), right.clone()))
                    };
                    if let Some(pair) = pair {
                        weak.emit(pair);
                    }
                },
                {
                    let weak = output.downgrade();
                    move |error| {
                        weak.error(error);
                    }
                },
                {
                    let weak = output.downgrade();
                    let state = state.clone();
                    move || {
                        let should_complete = {
                            let mut state = state.lock();
                            state.right_done = true;
                            if (state.right.is_none() || state.left_done) && !state.terminated {
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
        output.keep_subscription(left_subscription);
        output.keep_subscription(right_subscription);
        output
    }
}

struct MergeState {
    completed: usize,
    total: usize,
    terminated: bool,
}

struct ZipState<T, U> {
    left: VecDeque<T>,
    right: VecDeque<U>,
    left_done: bool,
    right_done: bool,
    terminated: bool,
}

struct CombineState<T, U> {
    left: Option<T>,
    right: Option<U>,
    left_done: bool,
    right_done: bool,
    terminated: bool,
}

fn subscribe_zip_left<T, U, E>(
    left: &Signal<T, E>,
    _right: &Signal<U, E>,
    output: &Signal<(T, U), E>,
    state: &Arc<Mutex<ZipState<T, U>>>,
) -> crate::Subscription
where
    T: Clone + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let weak = output.downgrade();
    let state_for_next = state.clone();
    left.subscribe(Observer::with_callbacks(
        move |value| {
            let (pair, complete) = {
                let mut state = state_for_next.lock();
                state.left.push_back(value);
                let pair = pop_pair_state(&mut state);
                let complete = !state.terminated
                    && ((state.left_done && state.left.is_empty())
                        || (state.right_done && state.right.is_empty()));
                if complete {
                    state.terminated = true;
                }
                (pair, complete)
            };
            if let Some(pair) = pair {
                weak.emit(pair);
            }
            if complete {
                weak.complete();
            }
        },
        {
            let weak = output.downgrade();
            move |error| {
                weak.error(error);
            }
        },
        {
            let weak = output.downgrade();
            let state = state.clone();
            move || {
                let should_complete = {
                    let mut state = state.lock();
                    state.left_done = true;
                    if !state.terminated && state.left.is_empty() {
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
}

fn subscribe_zip_right<T, U, E>(
    _left: &Signal<T, E>,
    right: &Signal<U, E>,
    output: &Signal<(T, U), E>,
    state: &Arc<Mutex<ZipState<T, U>>>,
) -> crate::Subscription
where
    T: Clone + Send + Sync + 'static,
    U: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
{
    let weak = output.downgrade();
    let state_for_next = state.clone();
    right.subscribe(Observer::with_callbacks(
        move |value| {
            let (pair, complete) = {
                let mut state = state_for_next.lock();
                state.right.push_back(value);
                let pair = pop_pair_state(&mut state);
                let complete = !state.terminated
                    && ((state.left_done && state.left.is_empty())
                        || (state.right_done && state.right.is_empty()));
                if complete {
                    state.terminated = true;
                }
                (pair, complete)
            };
            if let Some(pair) = pair {
                weak.emit(pair);
            }
            if complete {
                weak.complete();
            }
        },
        {
            let weak = output.downgrade();
            move |error| {
                weak.error(error);
            }
        },
        {
            let weak = output.downgrade();
            let state = state.clone();
            move || {
                let should_complete = {
                    let mut state = state.lock();
                    state.right_done = true;
                    if !state.terminated && state.right.is_empty() {
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
}

fn pop_pair_state<T, U>(state: &mut ZipState<T, U>) -> Option<(T, U)> {
    pop_pair(&mut state.left, &mut state.right)
}

fn pop_pair<T, U>(left: &mut VecDeque<T>, right: &mut VecDeque<U>) -> Option<(T, U)> {
    if left.is_empty() || right.is_empty() {
        return None;
    }
    let left_value = left.pop_front();
    let right_value = right.pop_front();
    match (left_value, right_value) {
        (Some(left_value), Some(right_value)) => Some((left_value, right_value)),
        (left_value, right_value) => {
            if let Some(left_value) = left_value {
                left.push_front(left_value);
            }
            if let Some(right_value) = right_value {
                right.push_front(right_value);
            }
            None
        }
    }
}
