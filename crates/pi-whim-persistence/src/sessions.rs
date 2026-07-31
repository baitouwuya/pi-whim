//! Reading Pi's JSONL session files.
//!
//! Pi owns the conversation transcript; SQLite only indexes it. These helpers
//! derive the index entries — title, preview, timestamp — by reading a
//! transcript, so the parsing lives next to the store that caches its results
//! rather than in the UI process.

use std::{
    collections::{HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::Path,
    time::UNIX_EPOCH,
};

use chrono::{SecondsFormat, Utc};
use pi_whim_core::{ProjectId, SessionSummary, stable_session_id};
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_JSONL_ENTRY_BYTES: usize = 1024 * 1024;

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

/// Build a bounded transcript for an explicit AI rename request.
///
/// Only plain user text and completed assistant text are retained. Thinking,
/// tool calls, tool results, attachments, metadata, and malformed or oversized
/// JSONL records never enter the returned value.
pub fn session_title_context_from_jsonl(
    path: &Path,
    max_bytes: usize,
) -> io::Result<Option<String>> {
    if max_bytes < 16 {
        return Ok(None);
    }

    let mut records = VecDeque::new();
    let per_record_limit = max_bytes.saturating_sub(2) / 2;
    visit_bounded_jsonl_lines(path, |line| {
        append_title_record(line, per_record_limit, max_bytes, &mut records);
    })?;

    let has_user = records
        .iter()
        .any(|record: &String| record.starts_with("User:\n"));
    if !has_user {
        return Ok(None);
    }
    Ok(Some(records.into_iter().collect::<Vec<_>>().join("\n\n")))
}

/// Persist a title in Pi's source-of-truth JSONL without starting a Pi process.
///
/// Callers must only use this for sessions that are not live in this app. The
/// append is synced before the SQLite index is updated, so loading the session
/// later cannot restore the previous title from disk.
pub fn persist_session_title_to_jsonl(path: &Path, title: &str) -> io::Result<()> {
    let title = title.replace(['\r', '\n'], " ").trim().to_owned();
    if title.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session title cannot be empty",
        ));
    }

    let mut ids = HashSet::new();
    let mut parent_id = None;
    visit_bounded_jsonl_lines(path, |line| {
        let Ok(entry) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        let Some(id) = entry.get("id").and_then(Value::as_str) else {
            return;
        };
        ids.insert(id.to_owned());
        if entry.get("type").and_then(Value::as_str) != Some("session") {
            parent_id = Some(id.to_owned());
        }
    })?;

    let id = (0..100)
        .map(|_| Uuid::new_v4().to_string()[..8].to_owned())
        .find(|id| !ids.contains(id))
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let entry = json!({
        "type": "session_info",
        "id": id,
        "parentId": parent_id,
        "timestamp": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "name": title,
    });
    let mut encoded = serde_json::to_vec(&entry).map_err(io::Error::other)?;
    encoded.push(b'\n');

    let mut reader = File::open(path)?;
    let needs_newline = if reader.seek(SeekFrom::End(-1)).is_ok() {
        let mut last = [0_u8; 1];
        reader.read_exact(&mut last)?;
        last[0] != b'\n'
    } else {
        false
    };
    let mut writer = OpenOptions::new().append(true).open(path)?;
    if needs_newline {
        writer.write_all(b"\n")?;
    }
    writer.write_all(&encoded)?;
    writer.sync_data()
}

fn visit_bounded_jsonl_lines(path: &Path, mut visit: impl FnMut(&[u8])) -> io::Result<()> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut oversized = false;

    loop {
        let (consumed, ended, finished) = {
            let buffer = reader.fill_buf()?;
            if buffer.is_empty() {
                (0, false, true)
            } else {
                let newline = buffer.iter().position(|byte| *byte == b'\n');
                let consumed = newline.map_or(buffer.len(), |index| index + 1);
                let content_end = newline.unwrap_or(buffer.len());
                if !oversized {
                    if line.len().saturating_add(content_end) <= MAX_JSONL_ENTRY_BYTES {
                        line.extend_from_slice(&buffer[..content_end]);
                    } else {
                        line.clear();
                        oversized = true;
                    }
                }
                (consumed, newline.is_some(), false)
            }
        };

        if finished {
            if !line.is_empty() && !oversized {
                visit(&line);
            }
            break;
        }
        reader.consume(consumed);
        if ended {
            if !oversized {
                visit(&line);
            }
            line.clear();
            oversized = false;
        }
    }
    Ok(())
}

fn append_title_record(
    line: &[u8],
    per_record_limit: usize,
    max_bytes: usize,
    records: &mut VecDeque<String>,
) {
    let Ok(entry) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    if entry.get("type").and_then(Value::as_str) != Some("message") {
        return;
    }
    let Some(message) = entry.get("message") else {
        return;
    };
    let role = message.get("role").and_then(Value::as_str);
    let text = match role {
        Some("user") => plain_content_text(message.get("content")),
        Some("assistant") if !has_tool_call(message.get("content")) => {
            plain_content_text(message.get("content"))
        }
        _ => None,
    };
    let Some(text) = text
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty())
    else {
        return;
    };
    let label = if role == Some("user") {
        "User"
    } else {
        "Assistant"
    };
    let record = bounded_record(label, &text, per_record_limit);
    if record.is_empty() {
        return;
    }
    records.push_back(record);
    while context_bytes(records) > max_bytes {
        records.pop_front();
    }
}

fn plain_content_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

fn has_tool_call(content: Option<&Value>) -> bool {
    content.and_then(Value::as_array).is_some_and(|parts| {
        parts.iter().any(|part| {
            matches!(
                part.get("type").and_then(Value::as_str),
                Some("toolCall" | "tool_call" | "toolUse" | "function_call")
            )
        })
    })
}

fn bounded_record(label: &str, text: &str, limit: usize) -> String {
    let prefix = format!("{label}:\n");
    if prefix.len().saturating_add(text.len()) <= limit {
        return format!("{prefix}{text}");
    }
    let marker = "[...]\n";
    let available = limit.saturating_sub(prefix.len() + marker.len());
    if available == 0 {
        return String::new();
    }
    let mut start = text.len().saturating_sub(available);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    format!("{prefix}{marker}{}", &text[start..])
}

fn context_bytes(records: &VecDeque<String>) -> usize {
    records.iter().map(String::len).sum::<usize>()
        + records.len().saturating_sub(1).saturating_mul(2)
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

    #[test]
    fn smart_title_context_keeps_only_user_and_final_assistant_text() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Fix the parser\"},{\"type\":\"image\",\"path\":\"/secret.png\"}]}}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"private reasoning\"},{\"type\":\"text\",\"text\":\"I will inspect it\"},{\"type\":\"toolCall\",\"name\":\"read\",\"arguments\":{\"path\":\"/secret.rs\"}}]}}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"toolResult\",\"content\":\"tool output\"}}\n",
                "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hidden final thought\"},{\"type\":\"text\",\"text\":\"Parser fixed and tested\"}]}}\n"
            ),
        )
        .unwrap();

        let context = session_title_context_from_jsonl(&path, 8 * 1024)
            .unwrap()
            .unwrap();

        assert_eq!(
            context,
            "User:\nFix the parser\n\nAssistant:\nParser fixed and tested"
        );
        for secret in [
            "private reasoning",
            "hidden final thought",
            "I will inspect it",
            "tool output",
            "/secret.png",
            "/secret.rs",
        ] {
            assert!(!context.contains(secret), "leaked {secret}");
        }
    }

    #[test]
    fn smart_title_context_is_bounded_and_prefers_recent_turns() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        let history = (0..20)
            .map(|index| {
                format!(
                    "{{\"type\":\"message\",\"message\":{{\"role\":\"user\",\"content\":\"request {index} {}\"}}}}\n{{\"type\":\"message\",\"message\":{{\"role\":\"assistant\",\"content\":\"answer {index} {}\"}}}}\n",
                    "x".repeat(40),
                    "y".repeat(40)
                )
            })
            .collect::<String>();
        fs::write(&path, history).unwrap();

        let context = session_title_context_from_jsonl(&path, 256)
            .unwrap()
            .unwrap();

        assert!(context.len() <= 256);
        assert!(context.contains("request 19"));
        assert!(context.contains("answer 19"));
        assert!(!context.contains("request 0 "));
    }

    #[test]
    fn smart_title_context_requires_a_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(
            &path,
            "{\"type\":\"message\",\"message\":{\"role\":\"assistant\",\"content\":\"hello\"}}\n",
        )
        .unwrap();

        assert_eq!(
            session_title_context_from_jsonl(&path, 8 * 1024).unwrap(),
            None
        );
    }

    #[test]
    fn persisted_title_is_the_latest_valid_pi_session_info_entry() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(
            &path,
            concat!(
                "{\"type\":\"session\",\"version\":3,\"id\":\"session-id\",\"timestamp\":\"2026-07-31T00:00:00.000Z\",\"cwd\":\"/tmp\"}\n",
                "{\"type\":\"message\",\"id\":\"last-id\",\"parentId\":null,\"timestamp\":\"2026-07-31T00:00:01.000Z\",\"message\":{\"role\":\"user\",\"content\":\"Prompt\"}}"
            ),
        )
        .unwrap();

        persist_session_title_to_jsonl(&path, "  AI title\ncontinued  ").unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        let entry = serde_json::from_str::<Value>(contents.lines().last().unwrap()).unwrap();
        assert_eq!(entry["type"], "session_info");
        assert_eq!(entry["name"], "AI title continued");
        assert_eq!(entry["parentId"], "last-id");
        assert_eq!(entry["id"].as_str().unwrap().len(), 8);
        assert!(entry["timestamp"].as_str().unwrap().ends_with('Z'));
        assert_eq!(
            session_summary_from_jsonl(Uuid::new_v4(), &path)
                .unwrap()
                .title,
            "AI title continued"
        );
    }
}
