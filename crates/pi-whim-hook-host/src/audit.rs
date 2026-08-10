//! Metadata-only audit and health signal values.

use serde::{Deserialize, Serialize};

/// Outcome recorded for one hook operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookAuditOutcome {
    /// A gate allowed the current request.
    Allowed,
    /// A gate denied the current request.
    Denied,
    /// A transform changed an authorized payload field.
    Transformed,
    /// A transform failed and the previous payload was preserved.
    Preserved,
    /// An observe request was accepted for best-effort delivery.
    Observed,
    /// The hook failed without a timeout.
    Failed,
    /// The hook exceeded its deadline.
    TimedOut,
    /// The host dropped an observe request because a bounded queue was full.
    Dropped,
    /// The host restarted a persistent process.
    Restarted,
}

/// Metadata-only audit event emitted through [`pi_whim_signal::Signal`].
///
/// The value intentionally contains no payload, hook output, credential, or
/// environment value. `grants_hash` is an opaque caller-provided digest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookAuditEvent {
    /// Stable hook identifier.
    pub hook_id: String,
    /// Scope digest, not a filesystem path.
    pub scope_id: String,
    /// Canonical event name.
    pub event: String,
    /// Hook kind.
    pub kind: String,
    /// Operation outcome.
    pub outcome: HookAuditOutcome,
    /// Monotonic operation duration in milliseconds.
    pub duration_ms: u64,
    /// Scope/manifest revision.
    pub revision: String,
    /// Whether an observe request was dropped.
    pub dropped: bool,
    /// Cumulative restart count for the process.
    pub restart_count: u32,
    /// Cumulative observe drops for the process.
    pub drop_count: u64,
    /// Caller-provided grants digest, if any.
    pub grants_hash: Option<String>,
}

/// Health state for one hook process in one scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookHealthStatus {
    /// The process is being started or restarted.
    Starting,
    /// The v2 process completed the hello/ready handshake.
    Ready,
    /// The process is unavailable; control operations fail closed.
    Unhealthy,
    /// The scope was revoked or dropped.
    Stopped,
}

/// Replayable metadata-only health snapshot emitted through `StateSignal`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookHostHealth {
    /// Stable hook identifier.
    pub hook_id: String,
    /// Scope digest, not a filesystem path.
    pub scope_id: String,
    /// Canonical event name.
    pub event: String,
    /// Hook process health state.
    pub status: HookHealthStatus,
    /// Scope/manifest revision.
    pub revision: String,
    /// Number of restarts used.
    pub restart_count: u32,
    /// Number of observe requests dropped.
    pub drop_count: u64,
    /// Last bounded diagnostic, never stdout/payload.
    pub last_error: Option<String>,
}
