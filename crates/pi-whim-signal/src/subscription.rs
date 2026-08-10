use crate::{Observer, Signal};
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

pub(crate) struct SubscriptionToken(Weak<SubscriptionState>);

struct SubscriptionState {
    active: AtomicBool,
    cancel: Mutex<Option<Box<dyn FnOnce() + Send + 'static>>>,
}

/// An RAII handle for one signal subscription.
///
/// Dropping the handle is equivalent to calling [`Subscription::unsubscribe`].
/// Unsubscription is idempotent and safe to call from a callback.
pub struct Subscription {
    state: Arc<SubscriptionState>,
}

impl Subscription {
    pub(crate) fn new<F>(cancel: F) -> Self
    where
        F: FnOnce() + Send + 'static,
    {
        Self::new_with_token(cancel).0
    }

    pub(crate) fn new_with_token<F>(cancel: F) -> (Self, SubscriptionToken)
    where
        F: FnOnce() + Send + 'static,
    {
        let state = Arc::new(SubscriptionState {
            active: AtomicBool::new(true),
            cancel: Mutex::new(Some(Box::new(cancel))),
        });
        (
            Self {
                state: state.clone(),
            },
            SubscriptionToken(Arc::downgrade(&state)),
        )
    }

    pub(crate) fn noop() -> Self {
        Self::new(|| {})
    }

    /// Unsubscribes immediately.  Repeated calls have no effect.
    pub fn unsubscribe(&self) {
        if !self.state.active.swap(false, Ordering::AcqRel) {
            return;
        }
        let cancel = self.state.cancel.lock().take();
        if let Some(cancel) = cancel {
            cancel();
        }
    }

    /// Returns whether this subscription is still active.
    pub fn is_active(&self) -> bool {
        self.state.active.load(Ordering::Acquire)
    }
}

impl SubscriptionToken {
    pub(crate) fn mark_inactive(&self) {
        if let Some(state) = self.0.upgrade() {
            state.active.store(false, Ordering::Release);
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.unsubscribe();
    }
}

struct ScopeState {
    closed: AtomicBool,
    subscriptions: Mutex<Vec<Subscription>>,
}

impl Drop for ScopeState {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        let subscriptions = self.subscriptions.get_mut().drain(..).collect::<Vec<_>>();
        drop(subscriptions);
    }
}

/// A shared RAII scope for a group of subscriptions.
#[derive(Clone)]
pub struct SubscriptionScope {
    state: Arc<ScopeState>,
}

impl SubscriptionScope {
    /// Creates an empty scope.
    pub fn new() -> Self {
        Self {
            state: Arc::new(ScopeState {
                closed: AtomicBool::new(false),
                subscriptions: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Adds a subscription to the scope.
    pub fn add(&self, subscription: Subscription) {
        if self.state.closed.load(Ordering::Acquire) {
            drop(subscription);
            return;
        }
        let mut subscriptions = self.state.subscriptions.lock();
        if self.state.closed.load(Ordering::Acquire) {
            drop(subscriptions);
            drop(subscription);
        } else {
            subscriptions.push(subscription);
        }
    }

    /// Subscribes and transfers ownership of the subscription to this scope.
    ///
    /// This method returns `()`: the scope owns the lifetime.  Call
    /// [`SubscriptionScope::add`] with [`Signal::subscribe`] when an independent
    /// cancellation handle is required.
    pub fn subscribe<T, E>(&self, signal: &Signal<T, E>, observer: Observer<T, E>)
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
    {
        let subscription = signal.subscribe(observer);
        self.add(subscription);
    }

    /// Unsubscribes all current members while keeping the scope usable.
    pub fn unsubscribe_all(&self) {
        let subscriptions = self
            .state
            .subscriptions
            .lock()
            .drain(..)
            .collect::<Vec<_>>();
        drop(subscriptions);
    }

    /// Returns the number of handles currently retained by the scope.
    pub fn len(&self) -> usize {
        self.state.subscriptions.lock().len()
    }

    /// Returns whether no handles are retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Default for SubscriptionScope {
    fn default() -> Self {
        Self::new()
    }
}
