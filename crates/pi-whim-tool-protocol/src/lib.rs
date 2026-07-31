//! Versioned JSONL protocol between Pi extensions and the local Rust tool host.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;
pub const SPAWN_AGENT_TOOL: &str = "spawn_agent";
pub const SEND_MESSAGE_TOOL: &str = "send_message";
pub const LIST_AGENTS_TOOL: &str = "list_agents";
pub const READ_MESSAGES_TOOL: &str = "read_messages";
pub const WAIT_AGENT_TOOL: &str = "wait_agent";
pub const INTERRUPT_AGENT_TOOL: &str = "interrupt_agent";
pub const READ_SESSION_TOOL: &str = "read_session";
pub const RESOLVE_SESSION_TOOL: &str = "resolve_session";
pub const LIST_SESSIONS_TOOL: &str = "list_sessions";
pub const SEARCH_SESSIONS_TOOL: &str = "search_sessions";
pub const READ_FILE_TOOL: &str = "read";
pub const WRITE_FILE_TOOL: &str = "write";
pub const EDIT_FILE_TOOL: &str = "edit";
pub const BASH_TOOL: &str = "bash";
pub const FETCH_TOOL: &str = "fetch";
pub const WEB_SEARCH_TOOL: &str = "web_search";
pub const LIST_PROCESSES_TOOL: &str = "list_processes";
pub const READ_PROCESS_TOOL: &str = "read_process";
pub const STOP_PROCESS_TOOL: &str = "stop_process";
pub const LIST_AVAILABLE_MODELS_TOOL: &str = "list_available_models";
pub const LIST_PENDING_REQUESTS_TOOL: &str = "list_pending_requests";
pub const RESOLVE_INTERACTION_TOOL: &str = "resolve_interaction";
pub const ASK_USER_TOOL: &str = "ask_user";
pub const RESET_TEAM_TOOL: &str = "_reset_team";

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolRequest {
    pub version: u32,
    pub request_id: String,
    pub capability: String,
    pub tool_name: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolProgress {
    pub version: u32,
    pub request_id: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolResponse {
    pub version: u32,
    pub request_id: String,
    pub content: Value,
    pub details: Value,
    pub is_error: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub error_details: Value,
}

impl ToolResponse {
    pub fn success(request_id: impl Into<String>, content: Value) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            content,
            details: Value::Null,
            is_error: false,
            error_code: None,
            error_details: Value::Null,
        }
    }

    pub fn error(
        request_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        Self {
            version: PROTOCOL_VERSION,
            request_id: request_id.into(),
            content: serde_json::json!({ "message": message }),
            details: Value::Null,
            is_error: true,
            error_code: Some(code.into()),
            error_details: Value::Null,
        }
    }

    pub fn error_with_details(
        request_id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        let mut response = Self::error(request_id, code, message);
        response.error_details = details;
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_has_a_stable_code() {
        let response = ToolResponse::error("request", "forbidden", "not allowed");
        assert!(response.is_error);
        assert_eq!(response.error_code.as_deref(), Some("forbidden"));
        assert_eq!(response.content["message"], "not allowed");
    }
}
