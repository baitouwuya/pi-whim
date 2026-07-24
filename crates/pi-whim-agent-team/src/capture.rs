use std::collections::VecDeque;

use serde_json::Value;

use crate::{
    model::AgentSessionEntry,
    session_read::{bounded_message_details, message_content},
};

pub const MAX_CAPTURE_BYTES: usize = 256 * 1024;

#[derive(Default)]
pub struct RunCapture {
    pub final_output: String,
    pub entries: VecDeque<AgentSessionEntry>,
}

impl RunCapture {
    pub fn ingest_line(&mut self, line: &str) {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            return;
        };
        if event.get("type").and_then(Value::as_str) != Some("message_end") {
            return;
        }
        let Some(message) = event.get("message") else {
            return;
        };
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            return;
        };
        if !matches!(role, "assistant" | "toolResult") {
            return;
        }
        let content = message_content(Some(message), true);
        let has_content = message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| !parts.is_empty());
        if content.is_empty() && !has_content {
            return;
        }
        let (details, details_truncated) = bounded_message_details(message);
        if self.entries.len() >= crate::MAX_SESSION_ENTRIES {
            self.entries.pop_front();
        }
        self.entries.push_back(AgentSessionEntry {
            id: message
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            role: role.to_owned(),
            turn: None,
            stop_reason: message
                .get("stopReason")
                .and_then(Value::as_str)
                .map(str::to_owned),
            peer_session_id: None,
            peer_name: None,
            content,
            details: Some(details),
            details_truncated,
        });
        if role == "assistant" {
            let output = message_content(Some(message), false);
            if !output.is_empty() {
                self.final_output = truncate_utf8(output, MAX_CAPTURE_BYTES);
            }
        }
    }
}

pub fn truncate_utf8(mut value: String, maximum_bytes: usize) -> String {
    if value.len() <= maximum_bytes {
        return value;
    }
    if maximum_bytes == 0 {
        return String::new();
    }
    const TRUNCATION_NOTICE: &str = "\n[output truncated by Pi-Whim]";
    if maximum_bytes <= TRUNCATION_NOTICE.len() {
        let mut boundary = maximum_bytes;
        while !value.is_char_boundary(boundary) {
            boundary -= 1;
        }
        value.truncate(boundary);
        return value;
    }
    let mut boundary = maximum_bytes - TRUNCATION_NOTICE.len();
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    value.truncate(boundary);
    value.push_str(TRUNCATION_NOTICE);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_the_last_assistant_message() {
        let mut capture = RunCapture::default();
        capture.ingest_line(
            r#"{"type":"message_end","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
        );
        assert_eq!(capture.final_output, "done");
        assert_eq!(capture.entries[0].content, "done");
    }

    #[test]
    fn truncation_including_its_notice_stays_within_the_limit() {
        let output = truncate_utf8("a".repeat(MAX_CAPTURE_BYTES + 1), MAX_CAPTURE_BYTES);
        assert_eq!(output.len(), MAX_CAPTURE_BYTES);
        assert!(output.ends_with("[output truncated by Pi-Whim]"));
    }
}
