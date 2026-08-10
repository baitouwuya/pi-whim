//! JSON-boundary invocation values and operation results.

use crate::{HookHostError, HookHostResult};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Mutable event payload after registry filtering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookPayload(Value);

impl HookPayload {
    /// Creates a bounded object payload.
    pub fn from_value(value: Value) -> HookHostResult<Self> {
        if !value.is_object() {
            return Err(HookHostError::InvalidInvocation(
                "hook payload must be a JSON object".to_owned(),
            ));
        }
        let bytes = serde_json::to_vec(&value)
            .map_err(|error| HookHostError::Json(error.to_string()))?
            .len();
        if bytes > crate::MAX_STDOUT_BYTES {
            return Err(HookHostError::InvalidInvocation(
                "hook payload exceeds 64 KiB".to_owned(),
            ));
        }
        Ok(Self(value))
    }

    /// Returns the underlying JSON value by reference.
    pub fn as_value(&self) -> &Value {
        &self.0
    }

    /// Consumes the wrapper and returns the JSON value.
    pub fn into_value(self) -> Value {
        self.0
    }
}

impl Serialize for HookPayload {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HookPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(value).map_err(serde::de::Error::custom)
    }
}

/// Authenticated invocation metadata. It is never part of the mutable payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookInvocationContext {
    /// Host-generated authentication marker.
    pub authenticated: bool,
    /// Scope digest associated with the request.
    pub scope_id: String,
    /// Approved manifest revision.
    pub revision: String,
    /// Canonical project root when the scope is project-bound.
    pub project_root: Option<String>,
    /// Opaque digest of grants, never the grants themselves.
    pub grants_hash: Option<String>,
}

impl HookInvocationContext {
    /// Creates authenticated context for an app scope.
    pub fn app(scope_id: impl Into<String>, revision: impl Into<String>) -> Self {
        Self {
            authenticated: true,
            scope_id: scope_id.into(),
            revision: revision.into(),
            project_root: None,
            grants_hash: None,
        }
    }

    /// Creates authenticated context for a project scope.
    pub fn project(
        scope_id: impl Into<String>,
        revision: impl Into<String>,
        project_root: impl Into<String>,
    ) -> Self {
        Self {
            authenticated: true,
            scope_id: scope_id.into(),
            revision: revision.into(),
            project_root: Some(project_root.into()),
            grants_hash: None,
        }
    }

    /// Attaches a digest without exposing the underlying grant set.
    pub fn with_grants_hash(mut self, grants_hash: impl Into<String>) -> Self {
        self.grants_hash = Some(grants_hash.into());
        self
    }

    /// Returns a copy intentionally marked unauthenticated for negative tests.
    pub fn unauthenticated(mut self) -> Self {
        self.authenticated = false;
        self
    }
}

/// One authenticated request sent to a matching hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookInvocation {
    /// Host-generated request identifier.
    pub request_id: String,
    /// Canonical event name.
    pub event: String,
    /// Definition kind.
    pub kind: crate::HookKind,
    /// Immutable authenticated context.
    pub context: HookInvocationContext,
    /// Mutable registry-filtered payload.
    pub payload: HookPayload,
}

impl HookInvocation {
    /// Creates an invocation with a caller-supplied request id.
    pub fn new(
        request_id: impl Into<String>,
        event: impl Into<String>,
        kind: crate::HookKind,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<Self> {
        let request_id = request_id.into();
        if request_id.is_empty() || request_id.len() > 128 {
            return Err(HookHostError::InvalidInvocation(
                "request_id must be 1..=128 bytes".to_owned(),
            ));
        }
        let event = event.into();
        if event.is_empty() || event.len() > 128 {
            return Err(HookHostError::InvalidInvocation(
                "event must be 1..=128 bytes".to_owned(),
            ));
        }
        Ok(Self {
            request_id,
            event,
            kind,
            context,
            payload,
        })
    }
}

/// Result of a gate chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookGateDecision {
    /// Every matching gate allowed the request.
    Allow,
    /// A gate explicitly denied the request.
    Deny {
        /// Hook which denied the request.
        hook_id: String,
        /// Bounded denial message.
        message: String,
    },
    /// A gate failed or timed out, so the host denied by default.
    FailedClosed {
        /// Hook which failed.
        hook_id: String,
        /// Structured failure.
        error: HookHostError,
    },
}

/// Result of a transform chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookTransformResult {
    /// The chain produced a changed payload.
    Transformed(HookPayload),
    /// A transform failed; the previous payload is returned unchanged.
    Preserved {
        /// Hook which failed, if one was reached.
        hook_id: Option<String>,
        /// Structured failure, if one was recorded.
        error: Option<HookHostError>,
        /// The unchanged payload.
        payload: HookPayload,
    },
}

/// Best-effort observe submission result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookObserveReceipt {
    /// Number of definitions accepted into delivery queues.
    pub accepted: usize,
    /// Number of definitions dropped because a queue was bounded/full.
    pub dropped: usize,
}
