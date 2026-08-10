use crate::SignalEvent;
use std::sync::Arc;

/// A set of callbacks for a signal subscription.
///
/// Callbacks are required to be `Send + Sync` so that one observer can safely be
/// used with [`ThreadScheduler`](crate::ThreadScheduler) and with signals emitted
/// from multiple threads.  A missing error or completion callback is a no-op.
pub struct Observer<T, E> {
    on_next: Arc<dyn Fn(T) + Send + Sync>,
    on_error: Arc<dyn Fn(E) + Send + Sync>,
    on_complete: Arc<dyn Fn() + Send + Sync>,
}

impl<T, E> Clone for Observer<T, E> {
    fn clone(&self) -> Self {
        Self {
            on_next: self.on_next.clone(),
            on_error: self.on_error.clone(),
            on_complete: self.on_complete.clone(),
        }
    }
}

impl<T, E> Observer<T, E> {
    /// Creates an observer with a value callback and no-op terminal callbacks.
    pub fn new<F>(on_next: F) -> Self
    where
        F: Fn(T) + Send + Sync + 'static,
    {
        Self {
            on_next: Arc::new(on_next),
            on_error: Arc::new(|_| {}),
            on_complete: Arc::new(|| {}),
        }
    }

    /// Creates an observer with all three callbacks.
    pub fn with_callbacks<F, G, H>(on_next: F, on_error: G, on_complete: H) -> Self
    where
        F: Fn(T) + Send + Sync + 'static,
        G: Fn(E) + Send + Sync + 'static,
        H: Fn() + Send + Sync + 'static,
    {
        Self {
            on_next: Arc::new(on_next),
            on_error: Arc::new(on_error),
            on_complete: Arc::new(on_complete),
        }
    }

    /// Creates an observer from one callback receiving every notification.
    pub fn from_event<F>(callback: F) -> Self
    where
        F: Fn(SignalEvent<T, E>) + Send + Sync + 'static,
        T: 'static,
        E: 'static,
    {
        let callback = Arc::new(callback);
        let next_callback = callback.clone();
        let error_callback = callback.clone();
        let complete_callback = callback;
        Self::with_callbacks(
            move |value| next_callback(SignalEvent::Next(value)),
            move |error| error_callback(SignalEvent::Error(error)),
            move || complete_callback(SignalEvent::Complete),
        )
    }

    pub(crate) fn next(&self, value: T) {
        (self.on_next)(value);
    }

    pub(crate) fn error(&self, error: E) {
        (self.on_error)(error);
    }

    pub(crate) fn complete(&self) {
        (self.on_complete)();
    }
}
