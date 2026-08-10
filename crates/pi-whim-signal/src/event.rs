use std::time::Duration;

/// A notification delivered by a [`Signal`](crate::Signal).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalEvent<T, E> {
    /// A normal value.
    Next(T),
    /// A terminal error.
    Error(E),
    /// A terminal completion.
    Complete,
}

/// Errors added by operators which need to introduce a failure value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignalError<E> {
    /// The upstream signal failed.
    Source(E),
    /// The timeout elapsed before the next value arrived.
    Timeout(Duration),
    /// A finite buffer rejected a value because its overflow policy requested an error.
    BufferOverflow,
}
