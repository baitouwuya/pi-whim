//! Structured errors returned by the hook host.

use std::fmt;

/// The result type used by the hook-host public API.
pub type HookHostResult<T> = Result<T, HookHostError>;

/// Errors which can be returned while loading, authorizing, or executing hooks.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HookHostError {
    /// A manifest or one of its nested values is invalid.
    #[error("invalid hook manifest: {0}")]
    InvalidManifest(String),
    /// A referenced event is not present in the registry.
    #[error("unknown hook event {event}")]
    UnknownEvent { event: String },
    /// A hook kind is not allowed for an event.
    #[error("hook kind {kind} is not allowed for event {event}")]
    DisallowedKind { event: String, kind: String },
    /// A field is not present in the event registry.
    #[error("field {field} is not allowed for event {event}")]
    UnauthorizedField { event: String, field: String },
    /// A field is permanently prohibited from crossing the hook boundary.
    #[error("field {field} is permanently prohibited")]
    ForbiddenField { field: String },
    /// A matcher key is not allowed for an event.
    #[error("matcher key {key} is not allowed for event {event}")]
    UnauthorizedMatcher { event: String, key: String },
    /// A scope key or approved manifest is inconsistent.
    #[error("invalid hook scope: {0}")]
    InvalidScope(String),
    /// The invocation did not contain an authenticated context for its scope.
    #[error("hook invocation context is not authenticated for this scope")]
    UnauthenticatedContext,
    /// A supplied invocation is malformed.
    #[error("invalid hook invocation: {0}")]
    InvalidInvocation(String),
    /// The hook host has been revoked or is shutting down.
    #[error("hook scope has been revoked")]
    ScopeRevoked,
    /// A nested invocation would re-enter an active hook event.
    #[error("recursive hook invocation is not allowed")]
    ReentrantInvocation,
    /// The platform does not provide the required sandbox launcher.
    #[error("sandbox-exec is unavailable")]
    SandboxUnavailable,
    /// A command entrypoint no longer matches its approval fingerprint.
    #[error("approved hook entrypoint changed")]
    FingerprintMismatch,
    /// A child process could not be started or used.
    #[error("hook process error: {0}")]
    Process(String),
    /// A hook did not finish within its configured deadline.
    #[error("hook {hook_id} timed out")]
    Timeout { hook_id: String },
    /// A response did not belong to the request currently in flight.
    #[error("unexpected response from hook {hook_id}: {reason}")]
    UnexpectedResponse { hook_id: String, reason: String },
    /// The bounded observe queue is full.
    #[error("hook observe queue is full")]
    QueueFull,
    /// A control request is already in flight.
    #[error("hook control request is busy")]
    Busy,
    /// The hook process has exceeded its restart budget.
    #[error("hook {hook_id} is unhealthy after restart budget was exhausted")]
    Unhealthy { hook_id: String },
    /// The host could not stop a child within the shutdown budget.
    #[error("hook host shutdown exceeded its bounded finalize budget")]
    ShutdownTimeout,
    /// JSON serialization or parsing failed at the process boundary.
    #[error("hook protocol JSON error: {0}")]
    Json(String),
    /// A generic I/O error represented without retaining platform-specific values.
    #[error("hook I/O error: {0}")]
    Io(String),
}

impl HookHostError {
    pub(crate) fn io(error: impl fmt::Display) -> Self {
        Self::Io(error.to_string())
    }

    pub(crate) fn process(error: impl fmt::Display) -> Self {
        Self::Process(error.to_string())
    }
}
