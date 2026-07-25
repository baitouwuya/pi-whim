//! Turning Pi's event stream into domain actions.
//!
//! Pi reports tool activity and replayed transcript entries as JSON. Deciding
//! what each becomes in the conversation is pure translation — JSON in, one
//! [`Action`] out — so it belongs beside the rest of the protocol handling rather
//! than in whichever view is on screen.
//!
//! These return the action instead of applying it, which is what lets them be
//! tested directly and keeps the reducer the only thing that mutates state.

use pi_whim_core::{Action, ConversationItem, ConversationRole};
use pi_whim_persistence::content_text;
use serde_json::Value;

use crate::protocol::{
    assistant_text, tool_call_report, tool_event_details, tool_result_report, tool_result_summary,
};

/// The action for a tool starting or finishing.
///
/// `conversation` is consulted for the entry this event updates: a tool's report
/// accumulates across its start and end events, so the previous text has to be
/// carried forward rather than replaced.
pub fn tool_event_action(event: &Value, conversation: &[ConversationItem]) -> Action {
    let name = event
        .get("toolName")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let id = event
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or(name)
        .to_owned();
    let is_error = event
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let previous = conversation
        .iter()
        .find(|message| message.id == id.as_str())
        .map(|message| (message.tool_report.clone(), message.tool_details.clone()));
    let previous_report = previous.as_ref().and_then(|(report, _)| report.as_deref());
    let previous_details = previous
        .as_ref()
        .and_then(|(_, details)| details.as_deref());

    let (content, tool_report) = match event.get("type").and_then(Value::as_str) {
        Some("tool_execution_end") => {
            let result_content = event.get("result").and_then(|result| result.get("content"));
            (
                tool_result_summary(Some(name), result_content, is_error),
                tool_result_report(Some(name), result_content, previous_report, is_error),
            )
        }
        _ => ("Running…".into(), tool_call_report(name, event.get("args"))),
    };

    Action::UpsertConversation(ConversationItem {
        id,
        role: ConversationRole::Tool,
        full_text: content,
        streaming: false,
        tool_name: Some(name.into()),
        tool_report: Some(tool_report),
        tool_details: Some(tool_event_details(event, previous_details)),
        is_error,
        model: None,
        attachments: Vec::new(),
    })
}

/// The action for a transcript entry Pi replayed, if it is one worth showing.
///
/// Returns `None` for entries with no message, or with a role the conversation
/// does not render — the transcript also carries bookkeeping the reader has no
/// use for.
pub fn session_entry_action(entry: &Value) -> Option<Action> {
    let message = entry.get("message")?;
    let role = match message.get("role").and_then(Value::as_str) {
        Some("user") => ConversationRole::User,
        Some("assistant") => ConversationRole::Assistant,
        Some("toolResult") | Some("bashExecution") => ConversationRole::Tool,
        _ => return None,
    };

    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("entry")
        .to_owned();
    let is_tool = role == ConversationRole::Tool;
    let tool_name = message
        .get("toolName")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let is_error = message
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let text = match role {
        ConversationRole::Assistant => assistant_text(message),
        ConversationRole::Tool => {
            tool_result_summary(tool_name.as_deref(), message.get("content"), is_error)
        }
        // A user entry can carry structured content; fall back to the raw JSON
        // rather than showing nothing.
        _ => content_text(message.get("content")).unwrap_or_else(|| message.to_string()),
    };

    Some(Action::UpsertConversation(ConversationItem {
        id,
        role,
        full_text: text,
        streaming: false,
        tool_name,
        tool_report: is_tool.then(|| {
            tool_result_report(
                message.get("toolName").and_then(Value::as_str),
                message.get("content"),
                None,
                is_error,
            )
        }),
        tool_details: is_tool.then(|| message.to_string()),
        is_error,
        model: message
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        attachments: Vec::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn upserted(action: Action) -> ConversationItem {
        match action {
            Action::UpsertConversation(item) => item,
            other => panic!("expected an upsert, got {other:?}"),
        }
    }

    #[test]
    fn a_starting_tool_reads_as_running() {
        let action = tool_event_action(
            &json!({"type": "tool_execution_start", "toolName": "bash", "toolCallId": "c1",
                    "args": {"command": "ls -la"}}),
            &[],
        );
        let item = upserted(action);

        assert_eq!(item.id, "c1");
        assert_eq!(item.role, ConversationRole::Tool);
        assert_eq!(item.tool_name.as_deref(), Some("bash"));
        assert!(!item.is_error);
        // The report names the operation rather than dumping the arguments.
        assert!(item.tool_report.is_some_and(|report| report.contains("ls")));
    }

    #[test]
    fn a_finishing_tool_carries_its_earlier_report_forward() {
        // The start event's report is the operation; the end event adds its
        // result. Losing the first half would leave "ran what?".
        let start = upserted(tool_event_action(
            &json!({"type": "tool_execution_start", "toolName": "bash", "toolCallId": "c1",
                    "args": {"command": "ls -la"}}),
            &[],
        ));
        let end = upserted(tool_event_action(
            &json!({"type": "tool_execution_end", "toolName": "bash", "toolCallId": "c1",
                    "result": {"content": [{"type": "text", "text": "a.txt"}]}}),
            std::slice::from_ref(&start),
        ));

        let report = end.tool_report.expect("a report");
        assert!(report.contains("ls"), "operation was dropped: {report}");
    }

    #[test]
    fn a_failing_tool_is_marked_as_an_error() {
        let item = upserted(tool_event_action(
            &json!({"type": "tool_execution_end", "toolName": "bash", "toolCallId": "c1",
                    "isError": true,
                    "result": {"content": [{"type": "text", "text": "not found"}]}}),
            &[],
        ));
        assert!(item.is_error);
    }

    #[test]
    fn a_tool_without_a_call_id_falls_back_to_its_name() {
        let item = upserted(tool_event_action(
            &json!({"type": "tool_execution_start", "toolName": "read"}),
            &[],
        ));
        assert_eq!(item.id, "read");
    }

    #[test]
    fn replayed_roles_map_onto_conversation_roles() {
        for (reported, expected) in [
            ("user", ConversationRole::User),
            ("assistant", ConversationRole::Assistant),
            ("toolResult", ConversationRole::Tool),
            ("bashExecution", ConversationRole::Tool),
        ] {
            let action = session_entry_action(&json!({
                "id": "e1",
                "message": {"role": reported, "content": "hello"}
            }))
            .expect("a renderable entry");
            assert_eq!(upserted(action).role, expected, "for role {reported}");
        }
    }

    #[test]
    fn transcript_bookkeeping_is_skipped() {
        // The transcript carries entries the reader has no use for.
        assert!(session_entry_action(&json!({"id": "e1"})).is_none());
        assert!(
            session_entry_action(&json!({"message": {"role": "modelChange"}})).is_none(),
            "an unrenderable role should be skipped"
        );
    }

    #[test]
    fn a_replayed_reply_keeps_the_model_that_produced_it() {
        // Which model answered is worth showing per message, since a switch
        // mid-session should be visible.
        let action = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "assistant", "content": "hi", "model": "opus"}
        }))
        .expect("a renderable entry");
        assert_eq!(upserted(action).model.as_deref(), Some("opus"));
    }

    #[test]
    fn a_replayed_tool_result_carries_a_report_and_its_raw_detail() {
        let action = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "toolResult", "toolName": "read",
                        "content": [{"type": "text", "text": "file body"}]}
        }))
        .expect("a renderable entry");
        let item = upserted(action);

        assert!(item.tool_report.is_some());
        assert!(
            item.tool_details.is_some(),
            "raw detail should be available"
        );
    }

    #[test]
    fn a_replayed_prompt_with_structured_content_still_shows_text() {
        let action = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "user", "content": [{"type": "text", "text": "the prompt"}]}
        }))
        .expect("a renderable entry");
        assert_eq!(upserted(action).full_text, "the prompt");
    }
}
