//! Typed, synchronous reactive signals for pi-whim.
//!
//! The crate deliberately depends only on the Rust standard library.  A [`Signal`]
//! is a multicast stream handle; [`SignalEmitter`] is the optional writer handle
//! returned by [`Signal::channel`].  Operators build new signals and retain their
//! upstream subscriptions for the lifetime of the returned signal.

mod buffering;
mod chain;
mod combining;
mod event;
mod flattening;
mod observer;
mod resilience;
mod scheduler;
mod scheduling_ops;
mod signal;
mod state;
mod subscription;
mod transforming;

pub use buffering::{BufferOptions, OverflowPolicy};
pub use chain::{
    GateChain, GateDecision, GateFailurePolicy, TransformChain, TransformFailurePolicy,
};
pub use event::{SignalError, SignalEvent};
pub use flattening::FlatMapOptions;
pub use observer::Observer;
pub use resilience::{RetryBackoff, RetryPolicy};
pub use scheduler::{
    ImmediateScheduler, ScheduledTask, Scheduler, TestScheduler, ThreadScheduler, schedule,
};
pub use scheduling_ops::ThrottleOptions;
pub(crate) use signal::WeakSignal;
pub use signal::{Signal, SignalEmitter};
pub use state::StateSignal;
pub use subscription::{Subscription, SubscriptionScope};

/// A clonable type-erased scheduler handle.
pub type SchedulerRef = std::sync::Arc<dyn Scheduler>;

/// A context shared by signal operators and consumers.
#[derive(Clone)]
pub struct SignalContext<M> {
    model: std::sync::Arc<M>,
    scheduler: SchedulerRef,
    scope: SubscriptionScope,
}

impl<M> SignalContext<M> {
    /// Creates a context with a model, scheduler, and empty subscription scope.
    pub fn new<S>(model: M, scheduler: S) -> Self
    where
        S: Scheduler + 'static,
    {
        Self {
            model: std::sync::Arc::new(model),
            scheduler: std::sync::Arc::new(scheduler),
            scope: SubscriptionScope::new(),
        }
    }

    /// Creates a context using [`ImmediateScheduler`].
    pub fn immediate(model: M) -> Self {
        Self::new(model, ImmediateScheduler)
    }

    /// Returns the shared model.
    pub fn model(&self) -> std::sync::Arc<M> {
        self.model.clone()
    }

    /// Returns the scheduler used by this context.
    pub fn scheduler(&self) -> SchedulerRef {
        self.scheduler.clone()
    }

    /// Returns a clone of the context's RAII subscription scope.
    pub fn scope(&self) -> SubscriptionScope {
        self.scope.clone()
    }

    /// Adds a subscription to this context's scope.
    pub fn retain(&self, subscription: Subscription) {
        self.scope.add(subscription);
    }
}

#[cfg(test)]
mod tests;
