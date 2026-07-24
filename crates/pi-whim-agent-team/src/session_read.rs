use std::collections::VecDeque;

use serde_json::{Map, Value, json};

use crate::model::{AgentOutcome, AgentSessionEntry, ReadSessionArguments};

const MAX_DETAIL_BYTES: usize = 64 * 1024;
const MAX_DETAIL_STRING_BYTES: usize = 16 * 1024;
const MAX_DETAIL_ITEMS: usize = 128;
const MAX_CONVERSATION_RESPONSE_BYTES: usize = 512 * 1024;
const DEFAULT_FULL_INCLUDE: &[&str] = &["peer_events"];
const VALID_INCLUDES: &[&str] = &[
    "thinking",
    "tool_calls",
    "tool_results",
    "usage",
    "metadata",
    "peer_events",
];

pub struct SessionSelection {
    pub entries: Vec<AgentSessionEntry>,
    pub include: Vec<String>,
    pub total_turns: u16,
    pub start_turn: Option<u16>,
    pub end_turn: Option<u16>,
    pub truncated: bool,
    pub next_entry_id: Option<String>,
}

pub fn select(
    entries: &[AgentSessionEntry],
    outcome: &AgentOutcome,
    arguments: &ReadSessionArguments,
) -> Result<SessionSelection, String> {
    if !matches!(arguments.detail.as_str(), "report" | "full") {
        return Err("detail must be report or full".into());
    }
    let include = normalized_include(arguments)?;
    if !matches!(arguments.range.as_str(), "all" | "last_turn") {
        return Err("range must be all or last_turn".into());
    }
    if arguments.range == "last_turn"
        && (arguments.start_turn.is_some() || arguments.end_turn.is_some())
    {
        return Err("start_turn and end_turn cannot be combined with range=last_turn".into());
    }
    if arguments.start_turn == Some(0) || arguments.end_turn == Some(0) {
        return Err("turn numbers start at 1".into());
    }
    if let (Some(start), Some(end)) = (arguments.start_turn, arguments.end_turn)
        && start > end
    {
        return Err("start_turn cannot be greater than end_turn".into());
    }

    let turn_starts: Vec<_> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| (entry.role == "user").then_some(index))
        .collect();
    let total_turns = u16::try_from(turn_starts.len()).unwrap_or(u16::MAX);
    let (start_turn, end_turn) = selected_turns(arguments, total_turns)?;
    let selected = match (start_turn, end_turn) {
        (Some(start), Some(end)) => select_turn_entries(
            entries,
            &turn_starts,
            start,
            end,
            arguments.detail.as_str(),
            &include,
            outcome,
        ),
        _ => Vec::new(),
    };
    let selected = select_entry_range(selected, arguments)?;
    let (entries, truncated, next_entry_id) = fit_response_budget(selected);

    Ok(SessionSelection {
        entries,
        include,
        total_turns,
        start_turn,
        end_turn,
        truncated,
        next_entry_id,
    })
}

fn normalized_include(arguments: &ReadSessionArguments) -> Result<Vec<String>, String> {
    let include: Vec<String> = if arguments.include.is_empty() && arguments.detail == "full" {
        DEFAULT_FULL_INCLUDE
            .iter()
            .map(|value| (*value).into())
            .collect()
    } else {
        arguments.include.clone()
    };
    for value in &include {
        if !VALID_INCLUDES.contains(&value.as_str()) {
            return Err(format!(
                "include contains unsupported value {value}; valid values are {}",
                VALID_INCLUDES.join(", ")
            ));
        }
    }
    Ok(include)
}

fn select_entry_range(
    entries: Vec<AgentSessionEntry>,
    arguments: &ReadSessionArguments,
) -> Result<Vec<AgentSessionEntry>, String> {
    if entries.is_empty() {
        if arguments.start_entry_id.is_some() || arguments.end_entry_id.is_some() {
            return Err("entry range cannot be selected from an empty conversation".into());
        }
        return Ok(entries);
    }
    let start = match arguments.start_entry_id.as_deref() {
        Some(id) => entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| format!("start_entry_id {id} is not in the selected turns"))?,
        None => 0,
    };
    let end = match arguments.end_entry_id.as_deref() {
        Some(id) => entries
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| format!("end_entry_id {id} is not in the selected turns"))?,
        None => entries.len() - 1,
    };
    if start > end {
        return Err("start_entry_id occurs after end_entry_id".into());
    }
    Ok(entries[start..=end].to_vec())
}

fn selected_turns(
    arguments: &ReadSessionArguments,
    total_turns: u16,
) -> Result<(Option<u16>, Option<u16>), String> {
    if total_turns == 0 {
        return Ok((None, None));
    }
    if arguments.range == "last_turn" {
        return Ok((Some(total_turns), Some(total_turns)));
    }
    let start = arguments.start_turn.unwrap_or(1);
    if start > total_turns {
        return Err(format!(
            "start_turn {start} exceeds the {total_turns} available turns"
        ));
    }
    let end = arguments.end_turn.unwrap_or(total_turns).min(total_turns);
    if arguments.end_turn.is_some_and(|end| end > total_turns) {
        return Err(format!(
            "end_turn {} exceeds the {total_turns} available turns",
            arguments.end_turn.unwrap_or_default()
        ));
    }
    if start > end {
        return Err("the selected turn range is empty".into());
    }
    Ok((Some(start), Some(end)))
}

fn select_turn_entries(
    entries: &[AgentSessionEntry],
    turn_starts: &[usize],
    start_turn: u16,
    end_turn: u16,
    detail: &str,
    include: &[String],
    outcome: &AgentOutcome,
) -> Vec<AgentSessionEntry> {
    let mut selected = Vec::new();
    for turn in start_turn..=end_turn {
        let turn_index = usize::from(turn - 1);
        let start = turn_starts[turn_index];
        let end = turn_starts
            .get(turn_index + 1)
            .copied()
            .unwrap_or(entries.len());
        let turn_entries = &entries[start..end];
        if detail == "full" {
            selected.extend(
                turn_entries
                    .iter()
                    .filter(|entry| include_peer_event(entry, include))
                    .cloned()
                    .map(|mut entry| {
                        entry.turn = Some(turn);
                        if let Some(details) = entry.details.as_ref() {
                            entry.details = Some(filter_details(details, include));
                        }
                        entry
                    }),
            );
            continue;
        }

        if let Some(user) = turn_entries.first() {
            selected.push(report_entry(user.clone(), turn));
        }
        if let Some(report) = turn_entries
            .iter()
            .rev()
            .find(|entry| is_final_report(entry))
        {
            selected.push(report_entry(report.clone(), turn));
        } else if turn == end_turn
            && !outcome.output.trim().is_empty()
            && !turn_entries
                .iter()
                .any(|entry| entry.role == "assistant" && entry.details.is_some())
        {
            selected.push(AgentSessionEntry {
                id: "agent-outcome".into(),
                role: "assistant".into(),
                turn: Some(turn),
                stop_reason: Some("stop".into()),
                peer_session_id: None,
                peer_name: None,
                content: crate::capture::truncate_utf8(
                    outcome.output.clone(),
                    crate::MAX_SESSION_CONTENT_BYTES,
                ),
                details: None,
                details_truncated: false,
            });
        }
    }
    selected
}

fn include_peer_event(entry: &AgentSessionEntry, include: &[String]) -> bool {
    !matches!(entry.role.as_str(), "incoming" | "outgoing")
        || include.iter().any(|value| value == "peer_events")
}

fn has_include(include: &[String], value: &str) -> bool {
    include.iter().any(|candidate| candidate == value)
}

fn filter_details(details: &Value, include: &[String]) -> Value {
    let Some(object) = details.as_object() else {
        return details.clone();
    };
    let mut filtered = Map::new();
    for key in [
        "role",
        "stopReason",
        "toolCallId",
        "toolName",
        "isError",
        "errorMessage",
    ] {
        if let Some(value) = object.get(key) {
            filtered.insert(key.into(), value.clone());
        }
    }
    let is_tool_result = object.get("role").and_then(Value::as_str) == Some("toolResult");
    if (has_include(include, "tool_calls")
        || has_include(include, "tool_results")
        || has_include(include, "thinking"))
        && (!is_tool_result || has_include(include, "tool_results"))
        && let Some(content) = object.get("content")
    {
        filtered.insert("content".into(), filter_content(content, include));
    }
    if has_include(include, "usage")
        && let Some(usage) = object.get("usage")
    {
        filtered.insert("usage".into(), usage.clone());
    }
    if has_include(include, "metadata") {
        for key in [
            "api",
            "provider",
            "model",
            "responseId",
            "thinkingSignature",
            "timestamp",
        ] {
            if let Some(value) = object.get(key) {
                filtered.insert(key.into(), value.clone());
            }
        }
    }
    Value::Object(filtered)
}

fn filter_content(content: &Value, include: &[String]) -> Value {
    let Some(parts) = content.as_array() else {
        return content.clone();
    };
    Value::Array(
        parts
            .iter()
            .filter(|part| {
                let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
                match kind {
                    "thinking" => has_include(include, "thinking"),
                    "toolCall" => has_include(include, "tool_calls"),
                    "toolResult" => has_include(include, "tool_results"),
                    _ => true,
                }
            })
            .cloned()
            .collect(),
    )
}

fn report_entry(mut entry: AgentSessionEntry, turn: u16) -> AgentSessionEntry {
    if let Some(details) = entry.details.as_ref() {
        entry.content = message_content(Some(details), false);
    }
    entry.turn = Some(turn);
    entry.details = None;
    entry
}

fn is_final_report(entry: &AgentSessionEntry) -> bool {
    if entry.role != "assistant" {
        return false;
    }
    if entry.stop_reason.as_deref() == Some("toolUse") {
        return false;
    }
    let content = entry
        .details
        .as_ref()
        .map(|details| message_content(Some(details), false))
        .unwrap_or_else(|| entry.content.clone());
    !content.trim().is_empty()
}

fn fit_response_budget(
    entries: Vec<AgentSessionEntry>,
) -> (Vec<AgentSessionEntry>, bool, Option<String>) {
    let mut retained = Vec::with_capacity(entries.len());
    let mut used = 0;
    for entry in entries {
        let size = serde_json::to_vec(&entry)
            .map(|value| value.len())
            .unwrap_or(0);
        if used + size > MAX_CONVERSATION_RESPONSE_BYTES {
            return (retained, true, Some(entry.id));
        }
        used += size;
        retained.push(entry);
    }
    (retained, false, None)
}

pub fn message_content(message: Option<&Value>, include_thinking: bool) -> String {
    let value = message.and_then(|message| message.get("content"));
    let content = match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text").and_then(Value::as_str).or_else(|| {
                    include_thinking
                        .then(|| part.get("thinking").and_then(Value::as_str))
                        .flatten()
                })
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    crate::capture::truncate_utf8(content, crate::MAX_SESSION_CONTENT_BYTES)
}

pub fn bounded_message_details(message: &Value) -> (Value, bool) {
    let mut budget = MAX_DETAIL_BYTES;
    let mut truncated = false;
    (
        bounded_value(message, &mut budget, &mut truncated),
        truncated,
    )
}

pub fn push_bounded_entry(
    entries: &mut VecDeque<AgentSessionEntry>,
    entry: AgentSessionEntry,
    maximum: usize,
) {
    if maximum == 0 {
        return;
    }
    if entries.len() >= maximum {
        let next_turn = entries
            .iter()
            .enumerate()
            .skip(1)
            .find_map(|(index, entry)| (entry.role == "user").then_some(index));
        if let Some(next_turn) = next_turn {
            entries.drain(..next_turn);
        } else if let Some(index) = entries.iter().position(|entry| entry.role != "user") {
            let _ = entries.remove(index);
        } else {
            entries.pop_front();
        }
    }
    entries.push_back(entry);
}

fn bounded_value(value: &Value, budget: &mut usize, truncated: &mut bool) -> Value {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => {
            *budget = budget.saturating_sub(16);
            value.clone()
        }
        Value::String(text) => {
            let maximum = (*budget).min(MAX_DETAIL_STRING_BYTES);
            let value = crate::capture::truncate_utf8(text.clone(), maximum);
            *truncated |= value.len() < text.len();
            *budget = budget.saturating_sub(value.len());
            Value::String(value)
        }
        Value::Array(items) => {
            let mut output = Vec::new();
            for item in items.iter().take(MAX_DETAIL_ITEMS) {
                if *budget == 0 {
                    break;
                }
                output.push(bounded_value(item, budget, truncated));
            }
            if output.len() < items.len() {
                *truncated = true;
                output.push(json!({ "truncated": true }));
            }
            Value::Array(output)
        }
        Value::Object(fields) => {
            let mut output = Map::new();
            for (key, value) in fields.iter().take(MAX_DETAIL_ITEMS) {
                if *budget == 0 {
                    break;
                }
                *budget = budget.saturating_sub(key.len());
                output.insert(key.clone(), bounded_value(value, budget, truncated));
            }
            if output.len() < fields.len() {
                *truncated = true;
                output.insert("_truncated".into(), Value::Bool(true));
            }
            Value::Object(output)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, role: &str, content: &str, details: Option<Value>) -> AgentSessionEntry {
        AgentSessionEntry {
            id: id.into(),
            role: role.into(),
            turn: None,
            stop_reason: None,
            peer_session_id: None,
            peer_name: None,
            content: content.into(),
            details,
            details_truncated: false,
        }
    }

    fn arguments(detail: &str, range: &str) -> ReadSessionArguments {
        ReadSessionArguments {
            session_id: uuid::Uuid::nil(),
            detail: detail.into(),
            range: range.into(),
            start_turn: None,
            end_turn: None,
            start_entry_id: None,
            end_entry_id: None,
            include: Vec::new(),
        }
    }

    #[test]
    fn reports_hide_thinking_tools_and_intermediate_assistant_messages() {
        let entries = vec![
            entry("u1", "user", "first", None),
            entry(
                "a1",
                "assistant",
                "hidden thought\nworking",
                Some(json!({
                    "role": "assistant",
                    "stopReason": "toolUse",
                    "content": [
                        { "type": "thinking", "thinking": "hidden thought" },
                        { "type": "text", "text": "working" },
                        { "type": "toolCall", "name": "read", "arguments": { "path": "secret" } }
                    ]
                })),
            ),
            entry("t1", "toolResult", "tool output", None),
            entry(
                "a2",
                "assistant",
                "hidden final thought\ndone",
                Some(json!({
                    "role": "assistant",
                    "stopReason": "stop",
                    "content": [
                        { "type": "thinking", "thinking": "hidden final thought" },
                        { "type": "text", "text": "done" }
                    ]
                })),
            ),
        ];
        let selected = select(
            &entries,
            &AgentOutcome::default(),
            &arguments("report", "all"),
        )
        .unwrap();
        assert_eq!(selected.entries.len(), 2);
        assert_eq!(selected.entries[0].content, "first");
        assert_eq!(selected.entries[1].content, "done");
        assert!(selected.entries.iter().all(|entry| entry.details.is_none()));
    }

    #[test]
    fn last_turn_and_explicit_turn_ranges_are_independent_of_detail() {
        let entries = vec![
            entry("u1", "user", "first", None),
            entry("a1", "assistant", "one", None),
            entry("u2", "user", "second", None),
            entry("a2", "assistant", "two", None),
            entry("u3", "user", "third", None),
            entry("a3", "assistant", "three", None),
        ];
        let last = select(
            &entries,
            &AgentOutcome::default(),
            &arguments("report", "last_turn"),
        )
        .unwrap();
        assert_eq!(last.start_turn, Some(3));
        assert_eq!(last.entries[0].content, "third");

        let mut explicit = arguments("full", "all");
        explicit.start_turn = Some(2);
        explicit.end_turn = Some(2);
        let middle = select(&entries, &AgentOutcome::default(), &explicit).unwrap();
        assert_eq!(middle.entries.len(), 2);
        assert!(middle.entries.iter().all(|entry| entry.turn == Some(2)));

        explicit.start_entry_id = Some("a2".into());
        let tail = select(&entries, &AgentOutcome::default(), &explicit).unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert_eq!(tail.entries[0].id, "a2");
    }

    #[test]
    fn response_budget_returns_a_resumable_entry_cursor() {
        let entries: Vec<_> = (0..40)
            .flat_map(|index| {
                [
                    entry(&format!("u{index}"), "user", "input", None),
                    entry(
                        &format!("a{index}"),
                        "assistant",
                        &"x".repeat(16 * 1024),
                        None,
                    ),
                ]
            })
            .collect();
        let selection = select(
            &entries,
            &AgentOutcome::default(),
            &arguments("full", "all"),
        )
        .unwrap();
        assert!(selection.truncated);
        let cursor = selection.next_entry_id.unwrap();

        let mut resumed = arguments("full", "all");
        resumed.start_entry_id = Some(cursor.clone());
        let resumed = select(&entries, &AgentOutcome::default(), &resumed).unwrap();
        assert_eq!(resumed.entries[0].id, cursor);
    }
}
