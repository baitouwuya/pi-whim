//! Pi's agent events, turned into domain actions.
//!
//! This is the widest part of the wire boundary: fourteen event types, each
//! carrying a different shape of JSON, collapsing onto a handful of
//! [`Action`]s. It used to sit on the egui app as a 270-line method reaching
//! into `self.workbench`, `self.sessions` and `self.store` as it went, which
//! made it both untestable and impossible to reuse from a second view.
//!
//! [`translate`] takes an event, a read-only [`Context`], and the [`Turn`] it
//! updates, and returns the actions to apply. What it cannot do itself — an RPC
//! back to the agent, a write to the session index, a prompt that was held back
//! for compaction — comes back as an [`Effect`] for the host to carry out. So
//! the translation stays pure enough to test against literal JSON, while each
//! host keeps its own answer for how to talk to its store and its window.

use pi_whim_core::{Action, ConversationItem, ConversationRole, SessionStatus};
use pi_whim_persistence::content_text;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    pool::{PendingPrompt, Turn},
    protocol::{
        assistant_text, tool_call_report, tool_event_details, tool_result_report,
        tool_result_summary,
    },
};

/// Work the translation cannot do itself.
///
/// Each of these needs something the translation deliberately has no handle on:
/// the agent process, the session index, or the composer. The host performs
/// them in order after applying the actions.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Re-read the transcript path from the agent, index it under the name the
    /// agent reports, and re-key the pool if Pi moved the file.
    ///
    /// A fresh session starts before Pi has written a transcript, and fork and
    /// clone move an existing one, so the key a session was pooled under is not
    /// always where its transcript ends up.
    SyncSessionFile,
    /// Index the transcript under a name Pi just reported for it.
    RenameSessionFile(Option<String>),
    /// Reload the visible conversation from the agent's own record of it.
    ReloadEntries,
    /// Re-read the agent's control state.
    RefreshControls,
    /// Send the prompt that was held back until compaction finished.
    SendPendingPrompt(PendingPrompt),
}

/// The actions to apply, and what to do afterwards.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    pub actions: Vec<Action>,
    pub effects: Vec<Effect>,
}

impl Outcome {
    fn act(&mut self, action: Action) {
        self.actions.push(action);
    }

    /// Only the visible session's events reach the conversation view; a
    /// background session's still have to move its busy dot and its title.
    fn act_if_active(&mut self, context: &Context<'_>, action: Action) {
        if context.is_active {
            self.actions.push(action);
        }
    }

    fn session_running(&mut self, context: &Context<'_>, running: bool) {
        self.act(Action::SessionRunning {
            path: context.key.to_owned(),
            running,
        });
    }

    fn effect(&mut self, effect: Effect) {
        self.effects.push(effect);
    }
}

/// What the session looks like from outside while one event is translated.
pub struct Context<'a> {
    /// The key the session is pooled under, which is its transcript path once
    /// Pi has written one.
    pub key: &'a str,
    /// Whether this is the session the conversation view is showing.
    pub is_active: bool,
    /// The conversation as it stands, for events that amend an existing entry.
    pub conversation: &'a [ConversationItem],
    /// Wall clock, for ids that have to be unique within a session.
    pub now_ms: i64,
}

/// Translate one agent event.
///
/// Unrecognized event types are ignored: Pi emits more than the workbench
/// shows, and a new one should not be an error.
pub fn translate(event: &Value, context: Context<'_>, turn: &mut Turn) -> Outcome {
    let mut outcome = Outcome::default();
    match event.get("type").and_then(Value::as_str) {
        Some("message_start" | "message_update") => {
            let message = event.get("message").cloned().unwrap_or(Value::Null);
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                return outcome;
            }
            turn.running = true;
            outcome.session_running(&context, true);
            if !context.is_active {
                return outcome;
            }
            // A new assistant reply means the conversation grew past the last
            // compaction; a later model switch should compact again.
            turn.conversation_compacted = false;
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    turn.assistant_message_id
                        .clone()
                        .unwrap_or_else(|| Uuid::new_v4().to_string())
                });
            // Pi assigns the real message id only once the model has answered,
            // so the entry started under a placeholder has to move with it.
            if let Some(previous_id) = turn.assistant_message_id.replace(id.clone())
                && previous_id != id
            {
                outcome.act(Action::RekeyConversation {
                    from: previous_id,
                    to: id.clone(),
                });
            }
            outcome.act(Action::UpsertConversation(ConversationItem {
                id,
                role: ConversationRole::Assistant,
                full_text: assistant_text(&message),
                streaming: true,
                tool_name: None,
                tool_report: None,
                tool_details: None,
                is_error: false,
                model: message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                attachments: Vec::new(),
            }));
        }

        Some("message_end")
            if event
                .get("message")
                .and_then(|message| message.get("role"))
                .and_then(Value::as_str)
                == Some("assistant") =>
        {
            // Left in place for a background session: whichever event ends its
            // turn will clear it, and until then the id is what a resumed
            // stream appends to.
            if context.is_active
                && let Some(id) = turn.assistant_message_id.take()
            {
                outcome.act(Action::FinishMessage(id));
            }
        }

        Some("tool_execution_start" | "tool_execution_end") => {
            outcome.act_if_active(&context, tool_event_action(event, context.conversation));
        }

        Some("queue_update") => {
            outcome.act_if_active(
                &context,
                Action::QueueUpdated {
                    steering: string_list(event.get("steering")),
                    follow_up: string_list(event.get("followUp")),
                },
            );
        }

        Some("agent_settled") => {
            turn.running = false;
            outcome.session_running(&context, false);
            outcome.effect(Effect::SyncSessionFile);
            if context.is_active {
                outcome.act(Action::SetSessionStatus(SessionStatus::Ready));
                // The agent's own transcript is the record of the turn; the
                // streamed entries were a preview of it.
                outcome.effect(Effect::ReloadEntries);
                outcome.effect(Effect::RefreshControls);
            }
        }

        Some("session_info_changed") => {
            outcome.effect(Effect::RenameSessionFile(
                event.get("name").and_then(Value::as_str).map(str::to_owned),
            ));
        }

        Some("thinking_level_changed") => {
            if context.is_active {
                outcome.effect(Effect::RefreshControls);
            }
        }

        Some("compaction_start") => {
            turn.running = true;
            outcome.session_running(&context, true);
            if !context.is_active {
                return outcome;
            }
            // Remembered so the result updates this card rather than adding a
            // second one below it.
            let item_id = format!("compaction-{}", context.now_ms);
            turn.compaction_item_id = Some(item_id.clone());
            outcome.act(Action::UpsertConversation(compaction_card(
                item_id,
                "Compacting…".to_owned(),
                false,
            )));
            outcome.act(Action::SetSessionStatus(SessionStatus::Compacting));
        }

        Some("compaction_end") => {
            let error = event.get("errorMessage").and_then(Value::as_str);
            turn.running = false;
            turn.conversation_compacted = error.is_none();
            let pending_prompt = turn.pending_prompt.take();
            let item_id = turn.compaction_item_id.take();
            outcome.session_running(&context, false);

            if context.is_active {
                // Pi reports "nothing to compact" as an error, but a session
                // too small to compact is not a failure the user should see as
                // one.
                let benign = error.is_some_and(|error| error.contains("Nothing to compact"));
                outcome.act(Action::SetSessionStatus(match error {
                    Some(error) if !benign => SessionStatus::Failed(error.to_owned()),
                    _ => SessionStatus::Ready,
                }));
                if let Some(item_id) = item_id {
                    let (text, is_error) = match error {
                        Some(_) if benign => {
                            ("Nothing to compact (session too small)".to_owned(), false)
                        }
                        Some(error) => (error.to_owned(), true),
                        None => (compaction_summary(event.get("result")), false),
                    };
                    outcome.act(Action::UpsertConversation(compaction_card(
                        item_id, text, is_error,
                    )));
                }
            }

            // The deferred prompt continues even with the session in the
            // background; only the visible status updates skip.
            if let Some(prompt) = pending_prompt {
                outcome.effect(Effect::SendPendingPrompt(prompt));
            }
        }

        Some("entry_appended") => {
            if context.is_active
                && let Some(entry) = event.get("entry")
                && let Some(action) = session_entry_action(entry)
            {
                outcome.act(action);
            }
        }

        _ => {}
    }
    outcome
}

/// A tool call or its result, as a conversation entry.
///
/// Start and end arrive as separate events under the same call id, so the end
/// amends the entry the start created. `conversation` is where the previous
/// report and details are read back from — a result report builds on the call
/// report rather than replacing it.
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
    let previous = conversation.iter().find(|message| message.id == id);
    let previous_report = previous.and_then(|message| message.tool_report.as_deref());
    let previous_details = previous.and_then(|message| message.tool_details.as_deref());

    let (content, tool_report) = match event.get("type").and_then(Value::as_str) {
        Some("tool_execution_end") => {
            let result = event.get("result").and_then(|result| result.get("content"));
            (
                tool_result_summary(Some(name), result, is_error),
                tool_result_report(Some(name), result, previous_report, is_error),
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

/// One entry from the agent's transcript, as a conversation entry.
///
/// Used both when an entry is appended live and when a session is reloaded
/// wholesale, so the two paths cannot drift. Roles the workbench does not
/// render return `None`.
pub fn session_entry_action(entry: &Value) -> Option<Action> {
    let message = entry.get("message")?;
    let role = match message.get("role").and_then(Value::as_str) {
        Some("user") => ConversationRole::User,
        Some("assistant") => ConversationRole::Assistant,
        Some("toolResult" | "bashExecution") => ConversationRole::Tool,
        _ => return None,
    };
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
        // Falling back to the raw JSON is deliberate: an entry shape this does
        // not know is better shown verbatim than silently dropped.
        _ => content_text(message.get("content")).unwrap_or_else(|| message.to_string()),
    };

    Some(Action::UpsertConversation(ConversationItem {
        id: entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("entry")
            .to_owned(),
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

/// The compaction card, which the start and the end of a compaction share.
fn compaction_card(id: String, text: String, is_error: bool) -> ConversationItem {
    ConversationItem {
        id,
        role: ConversationRole::Tool,
        full_text: text,
        streaming: false,
        tool_name: Some("compact".into()),
        tool_report: None,
        tool_details: None,
        is_error,
        model: None,
        attachments: Vec::new(),
    }
}

/// What a successful compaction saved, when Pi says.
fn compaction_summary(result: Option<&Value>) -> String {
    let number = |key: &str| {
        result
            .and_then(|result| result.get(key))
            .and_then(Value::as_i64)
    };
    match (number("tokensBefore"), number("estimatedTokensAfter")) {
        (Some(before), Some(after)) => {
            format!("Compacted context · {before} → {after} tokens")
        }
        _ => "Compacted context".to_owned(),
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::SubmitMode;
    use serde_json::json;

    const NOW: i64 = 1_700_000_000_000;

    /// The visible session, with nothing on screen yet.
    fn active() -> Context<'static> {
        Context {
            key: "/tmp/session.jsonl",
            is_active: true,
            conversation: &[],
            now_ms: NOW,
        }
    }

    fn background() -> Context<'static> {
        Context {
            is_active: false,
            ..active()
        }
    }

    fn assistant_message(text: &str) -> Value {
        json!({
            "type": "message_update",
            "message": {"role": "assistant", "id": "m1", "content": text},
        })
    }

    fn upserted(outcome: &Outcome) -> Vec<&ConversationItem> {
        outcome
            .actions
            .iter()
            .filter_map(|action| match action {
                Action::UpsertConversation(item) => Some(item),
                _ => None,
            })
            .collect()
    }

    fn running(outcome: &Outcome) -> Option<bool> {
        outcome.actions.iter().find_map(|action| match action {
            Action::SessionRunning { running, .. } => Some(*running),
            _ => None,
        })
    }

    fn status(outcome: &Outcome) -> Option<&SessionStatus> {
        outcome.actions.iter().find_map(|action| match action {
            Action::SetSessionStatus(status) => Some(status),
            _ => None,
        })
    }

    #[test]
    fn an_unknown_event_is_ignored() {
        // Pi emits more than the workbench renders; a new event type should be
        // inert, not an error.
        let mut turn = Turn::default();
        let outcome = translate(&json!({"type": "something_new"}), active(), &mut turn);

        assert_eq!(outcome, Outcome::default());
        assert_eq!(turn, Turn::default());
    }

    #[test]
    fn an_event_with_no_type_is_ignored() {
        let mut turn = Turn::default();
        assert_eq!(
            translate(&json!({}), active(), &mut turn),
            Outcome::default()
        );
    }

    #[test]
    fn a_streaming_reply_marks_the_session_running_and_shows_the_text() {
        let mut turn = Turn::default();
        let outcome = translate(&assistant_message("Hello"), active(), &mut turn);

        assert_eq!(running(&outcome), Some(true));
        assert!(turn.running);
        let items = upserted(&outcome);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].full_text, "Hello");
        assert!(items[0].streaming);
        assert_eq!(items[0].role, ConversationRole::Assistant);
    }

    #[test]
    fn a_user_message_event_is_not_treated_as_a_reply() {
        // Only the assistant streams; a user message arrives through the
        // transcript instead, and taking this path would double it.
        let mut turn = Turn::default();
        let outcome = translate(
            &json!({"type": "message_update", "message": {"role": "user", "content": "hi"}}),
            active(),
            &mut turn,
        );

        assert_eq!(outcome, Outcome::default());
        assert!(!turn.running);
    }

    #[test]
    fn a_background_reply_moves_its_busy_dot_but_not_the_conversation() {
        // This is what lets a session keep working while another is shown.
        let mut turn = Turn::default();
        let outcome = translate(&assistant_message("Hello"), background(), &mut turn);

        assert_eq!(running(&outcome), Some(true));
        assert!(turn.running);
        assert!(upserted(&outcome).is_empty());
    }

    #[test]
    fn a_reply_reopens_the_window_for_a_deferred_model_switch() {
        // The switch waits for a compaction, and a new reply means there is
        // once again something worth compacting.
        let mut turn = Turn {
            conversation_compacted: true,
            ..Turn::default()
        };
        translate(&assistant_message("Hello"), active(), &mut turn);

        assert!(!turn.conversation_compacted);
    }

    #[test]
    fn a_renamed_streaming_message_carries_its_entry_with_it() {
        // Pi assigns the real id once the model answers; without the rekey the
        // conversation would end up with both the placeholder and the reply.
        let mut turn = Turn {
            assistant_message_id: Some("placeholder".into()),
            ..Turn::default()
        };
        let outcome = translate(&assistant_message("Hello"), active(), &mut turn);

        assert!(outcome.actions.contains(&Action::RekeyConversation {
            from: "placeholder".into(),
            to: "m1".into(),
        }));
        assert_eq!(turn.assistant_message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn an_unchanged_id_is_not_rekeyed() {
        let mut turn = Turn {
            assistant_message_id: Some("m1".into()),
            ..Turn::default()
        };
        let outcome = translate(&assistant_message("more"), active(), &mut turn);

        assert!(
            !outcome
                .actions
                .iter()
                .any(|action| matches!(action, Action::RekeyConversation { .. }))
        );
    }

    #[test]
    fn a_reply_with_no_id_keeps_the_one_already_streaming() {
        // Otherwise every update would open a new entry and the reply would
        // arrive as a column of fragments.
        let mut turn = Turn {
            assistant_message_id: Some("m1".into()),
            ..Turn::default()
        };
        let outcome = translate(
            &json!({"type": "message_update", "message": {"role": "assistant", "content": "x"}}),
            active(),
            &mut turn,
        );

        assert_eq!(upserted(&outcome)[0].id, "m1");
        assert_eq!(turn.assistant_message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn a_first_reply_with_no_id_gets_one_so_it_can_be_amended() {
        let mut turn = Turn::default();
        let outcome = translate(
            &json!({"type": "message_update", "message": {"role": "assistant", "content": "x"}}),
            active(),
            &mut turn,
        );

        let id = &upserted(&outcome)[0].id;
        assert!(!id.is_empty());
        assert_eq!(turn.assistant_message_id.as_ref(), Some(id));
    }

    #[test]
    fn ending_a_reply_finishes_the_entry_and_releases_the_id() {
        let mut turn = Turn {
            assistant_message_id: Some("m1".into()),
            ..Turn::default()
        };
        let outcome = translate(
            &json!({"type": "message_end", "message": {"role": "assistant"}}),
            active(),
            &mut turn,
        );

        assert_eq!(outcome.actions, vec![Action::FinishMessage("m1".into())]);
        assert_eq!(turn.assistant_message_id, None);
    }

    #[test]
    fn a_background_reply_keeps_its_id_until_its_turn_ends() {
        // The entry is not on screen to finish, and a resumed stream still
        // needs somewhere to append.
        let mut turn = Turn {
            assistant_message_id: Some("m1".into()),
            ..Turn::default()
        };
        let outcome = translate(
            &json!({"type": "message_end", "message": {"role": "assistant"}}),
            background(),
            &mut turn,
        );

        assert_eq!(outcome, Outcome::default());
        assert_eq!(turn.assistant_message_id.as_deref(), Some("m1"));
    }

    #[test]
    fn a_tool_call_shows_as_running_then_takes_its_result() {
        let mut turn = Turn::default();
        let start = translate(
            &json!({"type": "tool_execution_start", "toolName": "read", "toolCallId": "t1"}),
            active(),
            &mut turn,
        );
        let items = upserted(&start);
        assert_eq!(items[0].id, "t1");
        assert_eq!(items[0].full_text, "Running…");
        assert!(!items[0].is_error);

        // The end amends the same entry, keyed by call id.
        let started = items[0].clone();
        let end = translate(
            &json!({
                "type": "tool_execution_end",
                "toolName": "read",
                "toolCallId": "t1",
                "result": {"content": "file contents"},
            }),
            Context {
                conversation: std::slice::from_ref(&started),
                ..active()
            },
            &mut turn,
        );
        let items = upserted(&end);
        assert_eq!(items[0].id, "t1");
        assert_ne!(items[0].full_text, "Running…");
    }

    #[test]
    fn a_failed_tool_is_marked_so_the_card_can_show_it() {
        let mut turn = Turn::default();
        let outcome = translate(
            &json!({
                "type": "tool_execution_end",
                "toolName": "bash",
                "toolCallId": "t1",
                "isError": true,
                "result": {"content": "command not found"},
            }),
            active(),
            &mut turn,
        );

        assert!(upserted(&outcome)[0].is_error);
    }

    #[test]
    fn an_unnamed_tool_call_is_still_keyed_uniquely_enough_to_amend() {
        // Falling back to the tool name for the id is what keeps start and end
        // on the same card when Pi omits the call id.
        let mut turn = Turn::default();
        let outcome = translate(
            &json!({"type": "tool_execution_start", "toolName": "read"}),
            active(),
            &mut turn,
        );

        assert_eq!(upserted(&outcome)[0].id, "read");
    }

    #[test]
    fn a_background_tool_call_does_not_reach_the_conversation() {
        let mut turn = Turn::default();
        let outcome = translate(
            &json!({"type": "tool_execution_start", "toolName": "read", "toolCallId": "t1"}),
            background(),
            &mut turn,
        );

        assert_eq!(outcome, Outcome::default());
    }

    #[test]
    fn queue_updates_carry_both_queues_and_default_to_empty() {
        let mut turn = Turn::default();
        let outcome = translate(
            &json!({"type": "queue_update", "steering": ["a"], "followUp": ["b", "c"]}),
            active(),
            &mut turn,
        );

        assert_eq!(
            outcome.actions,
            vec![Action::QueueUpdated {
                steering: vec!["a".into()],
                follow_up: vec!["b".into(), "c".into()],
            }]
        );

        // An absent queue is empty, not a reason to skip the update: the view
        // has to learn the queue drained.
        let outcome = translate(&json!({"type": "queue_update"}), active(), &mut turn);
        assert_eq!(
            outcome.actions,
            vec![Action::QueueUpdated {
                steering: Vec::new(),
                follow_up: Vec::new(),
            }]
        );
    }

    #[test]
    fn a_settled_agent_stops_reads_back_its_transcript_and_refreshes_controls() {
        let mut turn = Turn {
            running: true,
            ..Turn::default()
        };
        let outcome = translate(&json!({"type": "agent_settled"}), active(), &mut turn);

        assert!(!turn.running);
        assert_eq!(running(&outcome), Some(false));
        assert_eq!(status(&outcome), Some(&SessionStatus::Ready));
        // The transcript is the record of the turn; the streamed entries were a
        // preview, and the file has to be indexed under whatever Pi named it.
        assert_eq!(
            outcome.effects,
            vec![
                Effect::SyncSessionFile,
                Effect::ReloadEntries,
                Effect::RefreshControls,
            ]
        );
    }

    #[test]
    fn a_settled_background_agent_is_indexed_without_touching_the_view() {
        let mut turn = Turn {
            running: true,
            ..Turn::default()
        };
        let outcome = translate(&json!({"type": "agent_settled"}), background(), &mut turn);

        assert_eq!(running(&outcome), Some(false));
        assert_eq!(status(&outcome), None);
        assert_eq!(outcome.effects, vec![Effect::SyncSessionFile]);
    }

    #[test]
    fn a_renamed_session_is_reindexed_whether_or_not_it_is_visible() {
        // A background session's title still has to update in the sidebar.
        for context in [active(), background()] {
            let mut turn = Turn::default();
            let outcome = translate(
                &json!({"type": "session_info_changed", "name": "Refactor"}),
                context,
                &mut turn,
            );

            assert_eq!(
                outcome.effects,
                vec![Effect::RenameSessionFile(Some("Refactor".into()))]
            );
            assert!(outcome.actions.is_empty());
        }
    }

    #[test]
    fn a_thinking_level_change_only_refetches_for_the_visible_session() {
        // It is a control-bar change, and there is no control bar for a session
        // that is not on screen.
        let mut turn = Turn::default();
        assert_eq!(
            translate(
                &json!({"type": "thinking_level_changed"}),
                active(),
                &mut turn
            )
            .effects,
            vec![Effect::RefreshControls]
        );
        assert_eq!(
            translate(
                &json!({"type": "thinking_level_changed"}),
                background(),
                &mut turn
            ),
            Outcome::default()
        );
    }

    #[test]
    fn compaction_opens_one_card_that_its_result_replaces() {
        let mut turn = Turn::default();
        let start = translate(&json!({"type": "compaction_start"}), active(), &mut turn);

        assert!(turn.running);
        assert_eq!(status(&start), Some(&SessionStatus::Compacting));
        let card = upserted(&start)[0].clone();
        assert_eq!(card.tool_name.as_deref(), Some("compact"));
        let item_id = turn.compaction_item_id.clone().expect("a card to update");
        assert_eq!(card.id, item_id);

        let end = translate(
            &json!({
                "type": "compaction_end",
                "result": {"tokensBefore": 90_000, "estimatedTokensAfter": 12_000},
            }),
            active(),
            &mut turn,
        );

        // Same id, so the result lands on the card rather than under it.
        let items = upserted(&end);
        assert_eq!(items[0].id, item_id);
        assert!(items[0].full_text.contains("90000"));
        assert!(items[0].full_text.contains("12000"));
        assert!(!items[0].is_error);
        assert_eq!(turn.compaction_item_id, None);
        assert!(turn.conversation_compacted);
    }

    #[test]
    fn compaction_without_token_counts_still_reports_it_happened() {
        let mut turn = Turn::default();
        translate(&json!({"type": "compaction_start"}), active(), &mut turn);
        let end = translate(&json!({"type": "compaction_end"}), active(), &mut turn);

        assert_eq!(upserted(&end)[0].full_text, "Compacted context");
    }

    #[test]
    fn nothing_to_compact_is_not_shown_as_a_failure() {
        // Pi reports it as an error, but a session too small to compact is a
        // fact about the session, not a fault.
        let mut turn = Turn::default();
        translate(&json!({"type": "compaction_start"}), active(), &mut turn);
        let end = translate(
            &json!({"type": "compaction_end", "errorMessage": "Nothing to compact yet"}),
            active(),
            &mut turn,
        );

        assert_eq!(status(&end), Some(&SessionStatus::Ready));
        assert!(!upserted(&end)[0].is_error);
        assert!(!turn.conversation_compacted);
    }

    #[test]
    fn a_real_compaction_failure_surfaces_on_the_status_and_the_card() {
        let mut turn = Turn::default();
        translate(&json!({"type": "compaction_start"}), active(), &mut turn);
        let end = translate(
            &json!({"type": "compaction_end", "errorMessage": "model refused"}),
            active(),
            &mut turn,
        );

        assert_eq!(
            status(&end),
            Some(&SessionStatus::Failed("model refused".into()))
        );
        assert!(upserted(&end)[0].is_error);
        assert!(!turn.conversation_compacted);
    }

    #[test]
    fn a_prompt_held_for_compaction_is_sent_when_it_finishes() {
        let prompt: PendingPrompt = ("carry on".into(), Vec::new(), SubmitMode::Prompt);
        let mut turn = Turn {
            pending_prompt: Some(prompt.clone()),
            ..Turn::default()
        };
        let outcome = translate(&json!({"type": "compaction_end"}), active(), &mut turn);

        assert_eq!(outcome.effects, vec![Effect::SendPendingPrompt(prompt)]);
        assert_eq!(turn.pending_prompt, None);
    }

    #[test]
    fn a_held_prompt_is_sent_even_with_the_session_in_the_background() {
        // The user asked for it before switching away; dropping it would lose
        // the turn silently.
        let prompt: PendingPrompt = ("carry on".into(), Vec::new(), SubmitMode::Prompt);
        let mut turn = Turn {
            pending_prompt: Some(prompt.clone()),
            ..Turn::default()
        };
        let outcome = translate(&json!({"type": "compaction_end"}), background(), &mut turn);

        assert_eq!(outcome.effects, vec![Effect::SendPendingPrompt(prompt)]);
        assert_eq!(status(&outcome), None);
    }

    #[test]
    fn an_appended_entry_reaches_the_conversation_only_when_visible() {
        let event = json!({
            "type": "entry_appended",
            "entry": {"id": "e1", "message": {"role": "user", "content": "hello"}},
        });
        let mut turn = Turn::default();

        let outcome = translate(&event, active(), &mut turn);
        assert_eq!(upserted(&outcome)[0].id, "e1");

        assert_eq!(
            translate(&event, background(), &mut turn),
            Outcome::default()
        );
    }

    #[test]
    fn an_entry_the_workbench_cannot_render_is_dropped() {
        // Pi writes bookkeeping entries the conversation has no shape for.
        assert!(session_entry_action(&json!({"id": "e1"})).is_none());
        assert!(session_entry_action(&json!({})).is_none());
        assert!(
            session_entry_action(&json!({"message": {"role": "modelChange"}})).is_none(),
            "an unrenderable role should be skipped"
        );
    }

    #[test]
    fn replayed_roles_map_onto_conversation_roles() {
        for (reported, expected) in [
            ("user", ConversationRole::User),
            ("assistant", ConversationRole::Assistant),
            // Pi distinguishes these two; the conversation shows both as tool
            // activity, so the mapping is deliberately many-to-one.
            ("toolResult", ConversationRole::Tool),
            ("bashExecution", ConversationRole::Tool),
        ] {
            let Some(Action::UpsertConversation(item)) = session_entry_action(&json!({
                "id": "e1",
                "message": {"role": reported, "content": "hello"},
            })) else {
                panic!("expected an entry for role {reported}");
            };
            assert_eq!(item.role, expected, "for role {reported}");
        }
    }

    #[test]
    fn a_replayed_reply_keeps_the_model_that_produced_it() {
        // Which model answered is worth showing per message, since a switch
        // mid-session should be visible in the transcript.
        let Some(Action::UpsertConversation(item)) = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "assistant", "content": "hi", "model": "opus"},
        })) else {
            panic!("expected an entry");
        };

        assert_eq!(item.model.as_deref(), Some("opus"));
    }

    #[test]
    fn a_finishing_tool_carries_its_earlier_report_forward() {
        // The start event's report is the operation; the end event adds its
        // result. Losing the first half would leave "ran what?".
        let start = upserted(&Outcome {
            actions: vec![tool_event_action(
                &json!({"type": "tool_execution_start", "toolName": "bash", "toolCallId": "c1",
                        "args": {"command": "ls -la"}}),
                &[],
            )],
            effects: Vec::new(),
        })[0]
            .clone();

        let end = tool_event_action(
            &json!({"type": "tool_execution_end", "toolName": "bash", "toolCallId": "c1",
                    "result": {"content": [{"type": "text", "text": "a.txt"}]}}),
            std::slice::from_ref(&start),
        );
        let Action::UpsertConversation(item) = end else {
            panic!("expected an upsert");
        };

        let report = item.tool_report.expect("a report");
        assert!(report.contains("ls"), "operation was dropped: {report}");
    }

    #[test]
    fn a_transcript_entry_keeps_its_role_and_its_tool_report() {
        let Some(Action::UpsertConversation(item)) = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "toolResult", "toolName": "read", "content": "contents"},
        })) else {
            panic!("expected an entry");
        };

        assert_eq!(item.role, ConversationRole::Tool);
        assert_eq!(item.tool_name.as_deref(), Some("read"));
        // Both levels of detail are present: a tool card can be expanded.
        assert!(item.tool_report.is_some());
        assert!(item.tool_details.is_some());
        assert!(!item.streaming);
    }

    #[test]
    fn a_plain_entry_carries_no_tool_detail() {
        let Some(Action::UpsertConversation(item)) = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "user", "content": "hello"},
        })) else {
            panic!("expected an entry");
        };

        assert_eq!(item.full_text, "hello");
        assert_eq!(item.tool_report, None);
        assert_eq!(item.tool_details, None);
    }

    #[test]
    fn an_entry_with_content_this_cannot_read_is_shown_verbatim() {
        // Better the raw JSON than an entry that silently renders empty.
        let Some(Action::UpsertConversation(item)) = session_entry_action(&json!({
            "id": "e1",
            "message": {"role": "user", "content": {"unexpected": true}},
        })) else {
            panic!("expected an entry");
        };

        assert!(item.full_text.contains("unexpected"));
    }

    #[test]
    fn an_entry_with_no_id_still_renders() {
        let Some(Action::UpsertConversation(item)) =
            session_entry_action(&json!({"message": {"role": "user", "content": "hi"}}))
        else {
            panic!("expected an entry");
        };

        assert_eq!(item.id, "entry");
    }

    #[test]
    fn the_compaction_card_id_is_stable_within_one_compaction() {
        // It is what the result is matched against, so it cannot be re-derived.
        let mut turn = Turn::default();
        translate(&json!({"type": "compaction_start"}), active(), &mut turn);
        let first = turn.compaction_item_id.clone();

        let mut other = Turn::default();
        translate(
            &json!({"type": "compaction_start"}),
            Context {
                now_ms: NOW + 1,
                ..active()
            },
            &mut other,
        );

        assert_ne!(first, other.compaction_item_id);
    }
}
