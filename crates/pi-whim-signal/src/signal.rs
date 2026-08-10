use crate::subscription::SubscriptionToken;
use crate::{Observer, SignalEvent, Subscription, SubscriptionScope};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

struct Listener<T, E> {
    id: u64,
    observer: Observer<T, E>,
    subscription: SubscriptionToken,
}

enum Terminal<E> {
    Error(E),
    Complete,
}

struct SignalState<T, E> {
    listeners: Vec<Listener<T, E>>,
    next_listener: AtomicU64,
    queue: VecDeque<SignalEvent<T, E>>,
    draining: bool,
    terminal: Option<Terminal<E>>,
    resources: Vec<Subscription>,
}

struct SignalInner<T, E> {
    state: Mutex<SignalState<T, E>>,
}

struct DrainGuard<'a, T, E> {
    state: &'a Mutex<SignalState<T, E>>,
    armed: bool,
}

impl<T, E> DrainGuard<'_, T, E> {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<T, E> Drop for DrainGuard<'_, T, E> {
    fn drop(&mut self) {
        if self.armed {
            self.state.lock().draining = false;
        }
    }
}

/// A cloneable multicast stream handle.
///
/// A signal serializes recursive emissions: an emission made from inside a
/// callback is queued and delivered after the current notification.  Listener
/// snapshots are taken before callbacks begin, and callbacks never run while the
/// signal mutex is held. Observer panics propagate to the synchronous emitter,
/// while internal drain bookkeeping is restored during unwind.
/// [`Signal::channel`] can be used when a caller wants a separate writer handle.
pub struct Signal<T, E = Infallible> {
    inner: Arc<SignalInner<T, E>>,
}

/// A writer handle paired with a [`Signal`].
pub struct SignalEmitter<T, E = Infallible> {
    signal: Signal<T, E>,
}

impl<T, E> Clone for Signal<T, E> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T, E> Clone for SignalEmitter<T, E> {
    fn clone(&self) -> Self {
        Self {
            signal: self.signal.clone(),
        }
    }
}

#[derive(Clone)]
pub(crate) struct WeakSignal<T, E> {
    inner: Weak<SignalInner<T, E>>,
}

impl<T, E> Signal<T, E> {
    fn new() -> Self {
        Self {
            inner: Arc::new(SignalInner {
                state: Mutex::new(SignalState {
                    listeners: Vec::new(),
                    next_listener: AtomicU64::new(1),
                    queue: VecDeque::new(),
                    draining: false,
                    terminal: None,
                    resources: Vec::new(),
                }),
            }),
        }
    }

    /// Creates a signal and a separate writer handle.
    pub fn channel() -> (Self, SignalEmitter<T, E>) {
        let signal = Self::new();
        let emitter = SignalEmitter {
            signal: signal.clone(),
        };
        (signal, emitter)
    }

    /// Subscribes an observer and returns its RAII lifetime handle.
    pub fn subscribe(&self, observer: Observer<T, E>) -> Subscription
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let id = self
            .inner
            .state
            .lock()
            .next_listener
            .fetch_add(1, Ordering::Relaxed);
        let weak = self.downgrade();
        let (subscription, token) = Subscription::new_with_token(move || {
            if let Some(signal) = weak.inner.upgrade() {
                let mut state = signal.state.lock();
                state.listeners.retain(|listener| listener.id != id);
            }
        });
        let terminal = {
            let mut state = self.inner.state.lock();
            if state.terminal.is_some() {
                state.terminal.as_ref().map(|terminal| match terminal {
                    Terminal::Error(error) => SignalEvent::Error(error.clone()),
                    Terminal::Complete => SignalEvent::Complete,
                })
            } else {
                state.listeners.push(Listener {
                    id,
                    observer: observer.clone(),
                    subscription: token,
                });
                None
            }
        };
        if let Some(event) = terminal {
            drop(subscription);
            dispatch_to_observer(&observer, event);
            return Subscription::noop();
        }
        subscription
    }

    /// Subscribes a value callback with no-op terminal callbacks.
    pub fn subscribe_fn<F>(&self, on_next: F) -> Subscription
    where
        F: Fn(T) + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.subscribe(Observer::new(on_next))
    }

    /// Subscribes one callback receiving every notification.
    pub fn subscribe_event<F>(&self, callback: F) -> Subscription
    where
        F: Fn(SignalEvent<T, E>) + Send + Sync + 'static,
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.subscribe(Observer::from_event(callback))
    }

    /// Subscribes and transfers ownership of the subscription to a scope.
    ///
    /// The scope owns the returned lifetime; use [`SubscriptionScope::add`] when
    /// an independently cancellable [`Subscription`] is needed.
    pub fn subscribe_in_scope(&self, scope: &SubscriptionScope, observer: Observer<T, E>)
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let subscription = self.subscribe(observer);
        scope.add(subscription);
    }

    /// Emits one value for internal operator implementations.
    pub(crate) fn emit(&self, value: T) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.enqueue(SignalEvent::Next(value))
    }

    /// Emits a terminal error for internal operator implementations.
    pub(crate) fn error(&self, error: E) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.enqueue(SignalEvent::Error(error))
    }

    /// Completes the signal for internal operator implementations.
    pub(crate) fn complete(&self) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.enqueue(SignalEvent::Complete)
    }

    /// Returns whether the signal has received an error or completion.
    pub fn is_terminated(&self) -> bool {
        self.inner.state.lock().terminal.is_some()
    }

    /// Returns the number of listeners currently registered.
    pub fn listener_count(&self) -> usize {
        self.inner.state.lock().listeners.len()
    }

    pub(crate) fn keep_subscription(&self, subscription: Subscription) {
        let mut state = self.inner.state.lock();
        if state.terminal.is_some() {
            drop(state);
            drop(subscription);
        } else {
            state.resources.push(subscription);
        }
    }

    pub(crate) fn downgrade(&self) -> WeakSignal<T, E> {
        WeakSignal {
            inner: Arc::downgrade(&self.inner),
        }
    }

    fn enqueue(&self, event: SignalEvent<T, E>) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let should_drain = {
            let mut state = self.inner.state.lock();
            if state.terminal.is_some() {
                return false;
            }
            match &event {
                SignalEvent::Error(error) => {
                    state.terminal = Some(Terminal::Error(error.clone()));
                }
                SignalEvent::Complete => {
                    state.terminal = Some(Terminal::Complete);
                }
                SignalEvent::Next(_) => {}
            }
            state.queue.push_back(event);
            if state.draining {
                false
            } else {
                state.draining = true;
                true
            }
        };
        if should_drain {
            self.drain();
        }
        true
    }

    fn drain(&self)
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let mut drain_guard = DrainGuard {
            state: &self.inner.state,
            armed: true,
        };
        loop {
            let event = {
                let mut state = self.inner.state.lock();
                let Some(event) = state.queue.pop_front() else {
                    state.draining = false;
                    drain_guard.disarm();
                    return;
                };
                event
            };
            self.dispatch(event);
        }
    }

    fn dispatch(&self, event: SignalEvent<T, E>)
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let (listeners, resources) = {
            let mut state = self.inner.state.lock();
            let listeners = state
                .listeners
                .iter()
                .map(|listener| listener.observer.clone())
                .collect::<Vec<_>>();
            let resources = if matches!(event, SignalEvent::Error(_) | SignalEvent::Complete) {
                for listener in &state.listeners {
                    listener.subscription.mark_inactive();
                }
                state.listeners.clear();
                std::mem::take(&mut state.resources)
            } else {
                Vec::new()
            };
            (listeners, resources)
        };
        for observer in listeners {
            dispatch_to_observer(&observer, event.clone());
        }
        drop(resources);
    }
}

impl<T, E> SignalEmitter<T, E> {
    /// Emits one value through the paired signal.
    pub fn emit(&self, value: T) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.signal.emit(value)
    }

    /// Emits a terminal error through the paired signal.
    pub fn error(&self, error: E) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.signal.error(error)
    }

    /// Completes the paired signal.
    pub fn complete(&self) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.signal.complete()
    }

    /// Returns the read-only stream handle.
    pub fn signal(&self) -> Signal<T, E> {
        self.signal.clone()
    }
}

impl<T, E> WeakSignal<T, E> {
    pub(crate) fn emit(&self, value: T) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.inner
            .upgrade()
            .is_some_and(|inner| Signal { inner }.emit(value))
    }

    pub(crate) fn error(&self, error: E) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.inner
            .upgrade()
            .is_some_and(|inner| Signal { inner }.error(error))
    }

    pub(crate) fn complete(&self) -> bool
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        self.inner
            .upgrade()
            .is_some_and(|inner| Signal { inner }.complete())
    }
}

fn dispatch_to_observer<T, E>(observer: &Observer<T, E>, event: SignalEvent<T, E>) {
    match event {
        SignalEvent::Next(value) => observer.next(value),
        SignalEvent::Error(error) => observer.error(error),
        SignalEvent::Complete => observer.complete(),
    }
}
