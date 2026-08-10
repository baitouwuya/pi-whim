//! JSONL protocol boundary types for v2 persistent hook processes.

use crate::invocation::{HookInvocation, HookInvocationContext, HookPayload};
use crate::{HookHostError, HookHostResult, HookKind};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Host-to-hook hello message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookHello {
    /// Wire message discriminator.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Protocol version.
    pub protocol: u32,
    /// Hook identifier.
    pub hook_id: String,
    /// Canonical event name.
    pub event: String,
    /// Hook kind.
    pub kind: HookKind,
    /// Handshake nonce which a ready response must echo.
    pub hello_id: String,
}

impl HookHello {
    pub(crate) fn new(
        hook_id: impl Into<String>,
        event: impl Into<String>,
        kind: HookKind,
        hello_id: impl Into<String>,
    ) -> Self {
        Self {
            message_type: "hello".to_owned(),
            protocol: 2,
            hook_id: hook_id.into(),
            event: event.into(),
            kind,
            hello_id: hello_id.into(),
        }
    }
}

/// Host-to-hook invocation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookRequest {
    /// Wire message discriminator.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Request identifier.
    pub request_id: String,
    /// Hook identifier.
    pub hook_id: String,
    /// Canonical event name.
    pub event: String,
    /// Hook kind.
    pub kind: HookKind,
    /// Authenticated context, kept separate from mutable payload.
    pub context: HookInvocationContext,
    /// Filtered mutable payload.
    pub payload: HookPayload,
}

impl HookRequest {
    pub(crate) fn from_invocation(
        hook_id: &str,
        kind: HookKind,
        invocation: &HookInvocation,
    ) -> Self {
        Self {
            message_type: "request".to_owned(),
            request_id: invocation.request_id.clone(),
            hook_id: hook_id.to_owned(),
            event: invocation.event.clone(),
            kind,
            context: invocation.context.clone(),
            payload: invocation.payload.clone(),
        }
    }
}

/// Validated response body returned by a hook.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HookResponseBody {
    /// Gate response.
    Gate {
        /// `allow` or `deny`.
        decision: String,
        /// Optional bounded denial message.
        #[serde(default)]
        message: Option<String>,
    },
    /// Transform response with a payload delta.
    Transform {
        /// Authorized payload delta.
        payload: HookPayload,
    },
    /// Observe acknowledgement.
    Observe {
        /// Optional acknowledgement marker.
        #[serde(default)]
        accepted: Option<bool>,
    },
}

/// Hook-to-host response. The host validates all identity fields before use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookResponse {
    /// Wire message discriminator.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Request identifier echoed by the hook.
    pub request_id: String,
    /// Hook identifier echoed by the hook.
    pub hook_id: String,
    /// Event echoed by the hook.
    pub event: String,
    /// Response body.
    pub response: HookResponseBody,
}

/// Structured process-side error response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookWireError {
    /// Wire message discriminator.
    #[serde(rename = "type")]
    pub message_type: String,
    /// Request identifier, when known.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Hook identifier.
    pub hook_id: String,
    /// Bounded diagnostic.
    pub message: String,
}

/// Parsed v2 response/diagnostic frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookWireMessage {
    /// A ready response to hello.
    Ready {
        /// Echoed hook id.
        hook_id: String,
        /// Echoed event.
        event: String,
        /// Echoed hook kind when supplied by the process.
        kind: Option<HookKind>,
        /// Echoed hello nonce.
        hello_id: Option<String>,
    },
    /// A validated response frame.
    Response(HookResponse),
    /// A best-effort telemetry frame.
    Telemetry {
        /// Optional request id.
        request_id: Option<String>,
        /// Bounded event metadata.
        value: Value,
    },
    /// A structured error frame.
    Error(HookWireError),
    /// The reader encountered EOF or malformed data.
    Io(HookHostError),
}

impl HookWireMessage {
    pub(crate) fn parse_line(line: &[u8]) -> HookHostResult<Self> {
        let value: Value =
            serde_json::from_slice(line).map_err(|error| HookHostError::Json(error.to_string()))?;
        let object = value.as_object().ok_or_else(|| {
            HookHostError::Json("hook protocol frame must be an object".to_owned())
        })?;
        let message_type = object
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| HookHostError::Json("hook protocol frame has no type".to_owned()))?;
        match message_type {
            "ready" => {
                let allowed = ["type", "hook_id", "event", "kind", "hello_id"];
                reject_unknown(object, &allowed)?;
                let hook_id = required_string(object, "hook_id")?;
                let event = required_string(object, "event")?;
                let kind = object
                    .get("kind")
                    .map(|value| {
                        serde_json::from_value::<HookKind>(value.clone()).map_err(|error| {
                            HookHostError::Json(format!("invalid ready kind: {error}"))
                        })
                    })
                    .transpose()?;
                let hello_id = object
                    .get("hello_id")
                    .map(|value| string_value(value, "hello_id"))
                    .transpose()?;
                Ok(Self::Ready {
                    hook_id,
                    event,
                    kind,
                    hello_id,
                })
            }
            "response" => {
                let allowed = [
                    "type",
                    "request_id",
                    "hook_id",
                    "event",
                    "response",
                    "kind",
                    "decision",
                    "message",
                    "payload",
                    "accepted",
                ];
                reject_unknown(object, &allowed)?;
                let request_id = required_string(object, "request_id")?;
                let hook_id = required_string(object, "hook_id")?;
                let event = required_string(object, "event")?;
                let response_value = object.get("response").cloned().unwrap_or_else(|| {
                    let mut response = serde_json::Map::new();
                    for key in ["kind", "decision", "message", "payload", "accepted"] {
                        if let Some(value) = object.get(key) {
                            response.insert(key.to_owned(), value.clone());
                        }
                    }
                    Value::Object(response)
                });
                let response = parse_response_body(&response_value)?;
                Ok(Self::Response(HookResponse {
                    message_type: "response".to_owned(),
                    request_id,
                    hook_id,
                    event,
                    response,
                }))
            }
            "telemetry" | "observe" => {
                let allowed = ["type", "request_id", "value", "event", "message"];
                reject_unknown(object, &allowed)?;
                let request_id = object
                    .get("request_id")
                    .map(|value| string_value(value, "request_id"))
                    .transpose()?;
                let value = object
                    .get("value")
                    .or_else(|| object.get("message"))
                    .cloned()
                    .unwrap_or(Value::Null);
                if serde_json::to_vec(&value)
                    .map_err(|error| HookHostError::Json(error.to_string()))?
                    .len()
                    > 8 * 1024
                {
                    return Err(HookHostError::Json(
                        "telemetry frame exceeds diagnostic limit".to_owned(),
                    ));
                }
                Ok(Self::Telemetry { request_id, value })
            }
            "error" => {
                let allowed = ["type", "request_id", "hook_id", "message"];
                reject_unknown(object, &allowed)?;
                let request_id = object
                    .get("request_id")
                    .map(|value| string_value(value, "request_id"))
                    .transpose()?;
                let hook_id = required_string(object, "hook_id")?;
                let message = required_string(object, "message")?;
                if message.len() > 8 * 1024 {
                    return Err(HookHostError::Json(
                        "hook error diagnostic exceeds limit".to_owned(),
                    ));
                }
                Ok(Self::Error(HookWireError {
                    message_type: "error".to_owned(),
                    request_id,
                    hook_id,
                    message,
                }))
            }
            other => Err(HookHostError::Json(format!(
                "unknown hook protocol frame type {other}"
            ))),
        }
    }
}

fn parse_response_body(value: &Value) -> HookHostResult<HookResponseBody> {
    let object = value
        .as_object()
        .ok_or_else(|| HookHostError::Json("hook response body must be an object".to_owned()))?;
    let kind = object.get("kind").and_then(Value::as_str);
    match kind {
        Some("gate") => {
            reject_unknown(object, &["kind", "decision", "message"])?;
            let decision = required_string(object, "decision")?;
            let message = object
                .get("message")
                .map(|value| string_value(value, "message"))
                .transpose()?;
            Ok(HookResponseBody::Gate { decision, message })
        }
        Some("transform") => {
            reject_unknown(object, &["kind", "payload"])?;
            let payload = object.get("payload").cloned().ok_or_else(|| {
                HookHostError::Json("transform response has no payload".to_owned())
            })?;
            let payload = HookPayload::from_value(payload)?;
            Ok(HookResponseBody::Transform { payload })
        }
        Some("observe") => {
            reject_unknown(object, &["kind", "accepted"])?;
            let accepted = object
                .get("accepted")
                .map(|value| {
                    value.as_bool().ok_or_else(|| {
                        HookHostError::Json("observe accepted must be boolean".to_owned())
                    })
                })
                .transpose()?;
            Ok(HookResponseBody::Observe { accepted })
        }
        None => {
            if object.contains_key("decision") {
                let decision = required_string(object, "decision")?;
                let message = object
                    .get("message")
                    .map(|value| string_value(value, "message"))
                    .transpose()?;
                Ok(HookResponseBody::Gate { decision, message })
            } else if object.contains_key("payload") {
                let payload = HookPayload::from_value(
                    object
                        .get("payload")
                        .cloned()
                        .ok_or_else(|| HookHostError::Json("missing payload".to_owned()))?,
                )?;
                Ok(HookResponseBody::Transform { payload })
            } else {
                Ok(HookResponseBody::Observe { accepted: None })
            }
        }
        Some(other) => Err(HookHostError::Json(format!(
            "unknown hook response kind {other}"
        ))),
    }
}

fn reject_unknown(object: &serde_json::Map<String, Value>, allowed: &[&str]) -> HookHostResult<()> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(HookHostError::Json(format!("unknown protocol field {key}")));
    }
    Ok(())
}

fn required_string(object: &serde_json::Map<String, Value>, key: &str) -> HookHostResult<String> {
    let value = object
        .get(key)
        .ok_or_else(|| HookHostError::Json(format!("protocol field {key} is missing")))?;
    string_value(value, key)
}

fn string_value(value: &Value, key: &str) -> HookHostResult<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| HookHostError::Json(format!("protocol field {key} must be a string")))
}
