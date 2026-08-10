//! Capability-secured external hook hosting for app and agent-supervisor callers.
//!
//! The crate keeps manifest validation, field authorization, one-shot v1 execution,
//! persistent v2 execution, scope lifecycle, and audit/health signals independent of
//! persistence and UI layers. Hook processes receive only the filtered payload for
//! the current request; authenticated invocation context is serialized separately.

#![deny(unsafe_code)]

mod audit;
mod error;
mod executor;
mod invocation;
mod manifest;
mod persistent;
mod protocol;
mod registry;
mod sandbox;
mod scope;

pub use audit::{HookAuditEvent, HookAuditOutcome, HookHealthStatus, HookHostHealth};
pub use error::{HookHostError, HookHostResult};
pub use invocation::{
    HookGateDecision, HookInvocation, HookInvocationContext, HookObserveReceipt, HookPayload,
    HookTransformResult,
};
pub use manifest::{
    DeliveryMode, HookDataClass, HookDefinition, HookDelivery, HookKind, HookManifest, HookMatcher,
    HookRestartPolicy, MAX_COMMAND_ARG_BYTES, MAX_COMMAND_ITEMS, MAX_COMMAND_TOTAL_BYTES,
    MAX_FIELDS, MAX_HOOKS, MAX_ID_BYTES, MAX_MATCHER_KEYS, MAX_STDOUT_BYTES, MAX_TIMEOUT_MS,
};
pub use protocol::{
    HookHello, HookRequest, HookResponse, HookResponseBody, HookWireError, HookWireMessage,
};
pub use registry::{EventRegistry, HookEventSpec, HookFieldSpec, HookKindSpec};
pub use scope::{
    ApprovedHookManifest, HookHostManager, HookScopeHandle, HookScopeKey, ReentrancyGuard,
    ReentrancyKind,
};

#[cfg(test)]
mod tests;
