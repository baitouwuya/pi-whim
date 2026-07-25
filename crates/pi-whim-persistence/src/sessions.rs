//! Reading Pi's JSONL session files.
//!
//! Pi owns the conversation transcript; SQLite only indexes it. These helpers
//! derive the index entries — title, preview, timestamp — by reading a
//! transcript, so the parsing lives next to the store that caches its results
//! rather than in the UI process.

use std::{fs, path::Path, time::UNIX_EPOCH};

use pi_whim_core::{ProjectId, SessionSummary, stable_session_id};
use serde_json::Value;

pub fn content_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<String>(),
        ),
        _ => None,
    }
}

pub fn session_summary_from_jsonl(project_id: ProjectId, path: &Path) -> Option<SessionSummary> {
    let contents = fs::read_to_string(path).ok()?;
    let (title, preview, has_user_message) = session_title_and_preview(&contents);
    if !has_user_message {
        return None;
    }
    let pi_path = path.to_string_lossy().into_owned();
    let title = title.unwrap_or_else(|| {
        if preview.trim().is_empty() {
            "Image conversation".into()
        } else {
            preview.chars().take(52).collect()
        }
    });
    let updated_at_ms = path
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(SessionSummary {
        id: stable_session_id(&pi_path),
        project_id,
        pi_path,
        title,
        preview,
        updated_at_ms,
    })
}

fn session_title_and_preview(contents: &str) -> (Option<String>, String, bool) {
    let mut title = None;
    let mut preview = String::new();
    let mut has_user_message = false;
    for line in contents.lines() {
        // A Pi process can be interrupted while appending JSONL. Preserve the
        // usable history instead of hiding the entire session because of one tail line.
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("session_info") {
            title = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
        }
        let is_user_message = entry
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("user");
        has_user_message |= is_user_message;
        if preview.is_empty() && is_user_message {
            preview = content_text(
                entry
                    .get("message")
                    .and_then(|message| message.get("content")),
            )
            .unwrap_or_default();
        }
    }
    (title, preview, has_user_message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_uses_the_latest_pi_session_info_title() {
        let history = r#"
{"type":"message","message":{"role":"user","content":"first prompt"}}
{"type":"session_info","name":"Initial title"}
{"type":"message","message":{"role":"assistant","content":"reply"}}
{"type":"session_info","name":"  中文会话标题  "}
"#;

        let (title, preview, has_user_message) = session_title_and_preview(history);

        assert_eq!(title.as_deref(), Some("中文会话标题"));
        assert_eq!(preview, "first prompt");
        assert!(has_user_message);
    }

    #[test]
    fn history_skips_an_incomplete_jsonl_tail() {
        let history = r#"
{"type":"message","message":{"role":"user","content":"hello"}}
{"type":"session_info","name":"Named session"}
{"type":"message"
"#;

        let (title, preview, has_user_message) = session_title_and_preview(history);

        assert_eq!(title.as_deref(), Some("Named session"));
        assert_eq!(preview, "hello");
        assert!(has_user_message);
    }

    #[test]
    fn metadata_only_history_is_not_a_persisted_conversation() {
        let history = r#"
{"type":"session","id":"session-id","timestamp":"2026-07-22T12:00:00Z"}
{"type":"session_info","name":"New session"}
{"type":"model_change","provider":"example","modelId":"gpt-example"}
"#;

        let (title, preview, has_user_message) = session_title_and_preview(history);

        assert_eq!(title.as_deref(), Some("New session"));
        assert!(preview.is_empty());
        assert!(!has_user_message);
    }
}
