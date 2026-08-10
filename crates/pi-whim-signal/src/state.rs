use crate::{Observer, Signal, SignalEmitter, SignalEvent, Subscription};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;

struct StateValue<T> {
    value: T,
    terminated: bool,
}

/// A signal which retains and immediately exposes its latest value.
#[derive(Clone)]
pub struct StateSignal<T, E = Infallible> {
    state: Arc<Mutex<StateValue<T>>>,
    signal: Signal<T, E>,
    emitter: SignalEmitter<T, E>,
}

impl<T, E> StateSignal<T, E> {
    /// Creates a state signal with an initial value.
    pub fn new(initial: T) -> Self {
        let (signal, emitter) = Signal::channel();
        Self {
            state: Arc::new(Mutex::new(StateValue {
                value: initial,
                terminated: false,
            })),
            signal,
            emitter,
        }
    }

    /// Returns a read-only stream handle for future changes.
    pub fn signal(&self) -> Signal<T, E> {
        self.signal.clone()
    }

    /// Returns the current value.
    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.state.lock().value.clone()
    }

    /// Sets the current value and emits it to subscribers.
    pub fn set(&self, value: T) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let should_emit = {
            let mut state = self.state.lock();
            if state.terminated {
                false
            } else {
                state.value = value.clone();
                true
            }
        };
        should_emit && self.emitter.emit(value)
    }

    /// Updates the current value in place and emits the resulting value.
    pub fn update<F>(&self, update: F) -> bool
    where
        F: FnOnce(&mut T),
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let value = {
            let mut state = self.state.lock();
            if state.terminated {
                return false;
            }
            update(&mut state.value);
            state.value.clone()
        };
        self.emitter.emit(value)
    }

    /// Subscribes and synchronously receives the current value before changes.
    ///
    /// Registration and replay are ordered as one operation: a concurrent newer
    /// value is queued behind the initial replay, never delivered before it.
    /// User callbacks run without the state lock held, and replay bookkeeping is
    /// restored if a callback unwinds.
    pub fn subscribe(&self, observer: Observer<T, E>) -> Subscription
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let replay = {
            let state = self.state.lock();
            if state.terminated {
                None
            } else {
                let gate = Arc::new(Mutex::new(ReplayState {
                    replaying: true,
                    draining: false,
                    pending: VecDeque::new(),
                }));
                let next_gate = gate.clone();
                let error_gate = gate.clone();
                let complete_gate = gate.clone();
                let next_observer = observer.clone();
                let error_observer = observer.clone();
                let complete_observer = observer.clone();
                let forwarding = Observer::with_callbacks(
                    move |value| {
                        enqueue_replay(&next_gate, &next_observer, SignalEvent::Next(value))
                    },
                    move |error| {
                        enqueue_replay(&error_gate, &error_observer, SignalEvent::Error(error))
                    },
                    move || {
                        enqueue_replay(&complete_gate, &complete_observer, SignalEvent::Complete)
                    },
                );
                let subscription = self.signal.subscribe(forwarding);
                Some((state.value.clone(), gate, subscription))
            }
        };

        let Some((current, gate, subscription)) = replay else {
            return self.signal.subscribe(observer);
        };

        enqueue_replay(&gate, &observer, SignalEvent::Next(current));
        finish_replay(&gate, &observer);
        subscription
    }

    /// Subscribes a value callback and immediately sends the current value.
    pub fn subscribe_fn<F>(&self, on_next: F) -> Subscription
    where
        F: Fn(T) + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.subscribe(Observer::new(on_next))
    }

    /// Terminates the state signal with an error.
    pub fn error(&self, error: E) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let should_terminate = {
            let mut state = self.state.lock();
            if state.terminated {
                false
            } else {
                state.terminated = true;
                true
            }
        };
        should_terminate && self.emitter.error(error)
    }

    /// Completes the state signal.
    pub fn complete(&self) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let should_terminate = {
            let mut state = self.state.lock();
            if state.terminated {
                false
            } else {
                state.terminated = true;
                true
            }
        };
        should_terminate && self.emitter.complete()
    }
}

struct ReplayState<T, E> {
    replaying: bool,
    draining: bool,
    pending: VecDeque<SignalEvent<T, E>>,
}

struct ReplayDrainGuard<'a, T, E> {
    gate: &'a Mutex<ReplayState<T, E>>,
    armed: bool,
}

impl<T, E> ReplayDrainGuard<'_, T, E> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<T, E> Drop for ReplayDrainGuard<'_, T, E> {
    fn drop(&mut self) {
        if self.armed {
            self.gate.lock().draining = false;
        }
    }
}

fn enqueue_replay<T, E>(
    gate: &Arc<Mutex<ReplayState<T, E>>>,
    observer: &Observer<T, E>,
    event: SignalEvent<T, E>,
) where
    T: Send + 'static,
    E: Send + 'static,
{
    let should_drain = {
        let mut state = gate.lock();
        state.pending.push_back(event);
        if state.replaying || state.draining {
            false
        } else {
            state.draining = true;
            true
        }
    };
    if should_drain {
        drain_replay(gate, observer);
    }
}

fn finish_replay<T, E>(gate: &Arc<Mutex<ReplayState<T, E>>>, observer: &Observer<T, E>)
where
    T: Send + 'static,
    E: Send + 'static,
{
    let should_drain = {
        let mut state = gate.lock();
        state.replaying = false;
        if state.pending.is_empty() || state.draining {
            false
        } else {
            state.draining = true;
            true
        }
    };
    if should_drain {
        drain_replay(gate, observer);
    }
}

fn drain_replay<T, E>(gate: &Arc<Mutex<ReplayState<T, E>>>, observer: &Observer<T, E>)
where
    T: Send + 'static,
    E: Send + 'static,
{
    let mut drain_guard = ReplayDrainGuard { gate, armed: true };
    loop {
        let event = {
            let mut state = gate.lock();
            let Some(event) = state.pending.pop_front() else {
                state.draining = false;
                drain_guard.disarm();
                return;
            };
            event
        };
        match event {
            SignalEvent::Next(value) => observer.next(value),
            SignalEvent::Error(error) => observer.error(error),
            SignalEvent::Complete => observer.complete(),
        }
    }
}
