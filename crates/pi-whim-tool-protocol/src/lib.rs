//! Reserved, versioned wire protocol for future Rust implementations of Pi tools.
//! This crate is intentionally unused by the first release.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolRequest {
    pub version: u32,
    pub request_id: String,
    pub tool_name: String,
    pub project_path: String,
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
}
