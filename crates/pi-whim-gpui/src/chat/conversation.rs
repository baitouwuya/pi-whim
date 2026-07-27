//! The conversation itself.
//!
//! Entries vary in height — a one-line prompt, a long markdown reply, a tool card
//! — so this uses `list` rather than `uniform_list`.
//!
//! Aligned to the top, not the bottom. `ListAlignment::Bottom` pins the content to
//! the foot of the viewport, so a two-message conversation sat down by the prompt
//! with the empty space above it, and a reply appeared to grow upward out of the
//! input. Reading starts at the top. Following a stream is [`Conversation::
//! scroll_to_latest`]'s job instead, which is explicit about when the view moves.
//!
//! `overdraw` is what the egui build maintained by hand: it rendered a viewport's
//! worth above and below the visible span so scrolling did not reveal blank space.
//! Here it is a constructor argument, and `CachedMessageLayout` has no
//! counterpart — gpui measures and caches item heights itself.

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

use gpui::{
    AnyElement, Context, EventEmitter, FollowMode, IntoElement, ListAlignment, ListState,
    ParentElement, Render, Styled, Window, div, list, prelude::FluentBuilder, px,
};
use pi_whim_core::{
    AppState, ConversationItem, ConversationRole, Language, strings::text as translate,
};
use pi_whim_engine::typewriter::Typewriter;
use pi_whim_theme::{Tokens, text};

use crate::{
    chat::{MessageCard, message_card::MessageExpansion, reading_lane},
    theme::IntoHsla,
};

/// How far beyond the visible span to render, so scrolling does not flash blank.
const OVERDRAW: f32 = 400.0;
/// A retained-mode window has no frame callback, so the reveal drives itself.
const TYPEWRITER_FRAME: Duration = Duration::from_millis(16);

/// What the conversation asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEvent {
    /// Show or hide a tool card's normal output.
    ToggleToolReport(String),
    /// Show or hide a tool card's raw event data.
    ToggleToolDetails(String),
    /// Show or hide one reasoning section inside an assistant message.
    ToggleThinking { id: String, segment: usize },
    /// Reveal the rest of one streaming reply immediately.
    RevealAll(String),
    /// Fork the session at a user message.
    ForkAt(String),
    /// Copy a completed assistant reply.
    CopyAssistant(String),
}

/// The scrolling list of conversation entries.
pub struct Conversation {
    messages: Vec<ConversationItem>,
    /// Independently expandable content, grouped by message so lifecycle changes
    /// cannot leave one disclosure state behind another.
    expansions: HashMap<String, MessageExpansion>,
    typewriter: Typewriter,
    /// Whether a project is selected, which is what the empty state turns on.
    ///
    /// Without one there is nothing to talk to, so the empty transcript says how
    /// to get started rather than sitting blank.
    has_project: bool,
    /// The language the empty state is read in.
    language: Language,
    /// Whether the local transcript should show that Pi is generating.
    generating: bool,
    /// Prevents more than one reveal loop from running for this conversation.
    typewriter_running: bool,
    tokens: Tokens,
    list: ListState,
}

impl EventEmitter<ConversationEvent> for Conversation {}

impl Conversation {
    pub fn new(tokens: Tokens) -> Self {
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW));
        // Follow new output until the reader scrolls upward. GPUI disengages and
        // re-engages this mode from real scroll input, so streaming never fights
        // someone who is reading earlier messages.
        list.set_follow_mode(FollowMode::Tail);
        Self {
            messages: Vec::new(),
            expansions: HashMap::new(),
            typewriter: Typewriter::new(),
            has_project: false,
            language: Language::default(),
            generating: false,
            typewriter_running: false,
            tokens,
            list,
        }
    }

    /// Replace the entries from state.
    ///
    /// A changed count is spliced rather than reset so gpui keeps the measured
    /// heights of entries that did not move — appending a message should not
    /// re-measure the whole transcript.
    pub fn set_messages(&mut self, messages: Vec<ConversationItem>, cx: &mut Context<Self>) {
        if self.messages == messages {
            return;
        }
        let previous = self.messages.len();
        let next = messages.len();
        let changed_existing = changed_message_indices(&self.messages, &messages);

        // Pi replaces a draft streaming id with its real entry id in place.
        // Carry reveal progress across that rename, then discard progress for
        // entries that genuinely left the transcript.
        for (old, new) in self.messages.iter().zip(&messages) {
            let same_stream = old.role == new.role
                && old.streaming
                && new.streaming
                && (old.full_text.starts_with(&new.full_text)
                    || new.full_text.starts_with(&old.full_text));
            if old.id != new.id && same_stream {
                self.typewriter.rekey(&old.id, &new.id);
                rekey_expansion(&mut self.expansions, &old.id, &new.id);
            }
        }
        let next_ids: HashSet<_> = messages.iter().map(|message| message.id.as_str()).collect();
        for old in &self.messages {
            if !next_ids.contains(old.id.as_str()) {
                self.typewriter.forget(&old.id);
            }
        }
        self.expansions
            .retain(|id, _| next_ids.contains(id.as_str()));
        self.messages = messages;

        if next >= previous {
            // Appended, or the tail changed in place.
            self.list.splice(previous..previous, next - previous);
            // Tool output and streaming text can update any existing entry, not
            // only the tail (parallel tools are the common case).
            for index in changed_existing {
                self.list.remeasure_items(index..index + 1);
            }
        } else {
            // Entries went away, which only happens on a reset.
            self.list.reset(next);
        }
        self.start_typewriter(cx);
        cx.notify();
    }

    /// Drop everything, for switching sessions or clearing the conversation.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.messages.clear();
        self.expansions.clear();
        self.typewriter.clear();
        self.list.reset(0);
        self.list.set_follow_mode(FollowMode::Tail);
        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        if self.language != language {
            self.language = language;
            cx.notify();
        }
    }

    /// Say whether a project is selected.
    pub fn set_has_project(&mut self, has_project: bool, cx: &mut Context<Self>) {
        if self.has_project != has_project {
            self.has_project = has_project;
            cx.notify();
        }
    }

    pub fn set_generating(&mut self, generating: bool, cx: &mut Context<Self>) {
        if self.generating != generating {
            self.generating = generating;
            cx.notify();
        }
    }

    /// Advance the typewriter, reporting whether anything became visible.
    pub fn advance_typewriter(&mut self, elapsed_seconds: f32, cx: &mut Context<Self>) -> bool {
        let changed = self.typewriter.advance(&self.messages, elapsed_seconds);
        if changed {
            // The visible prefix can wrap onto another line, so its cached height
            // is no longer valid even though the domain message did not change.
            for (index, message) in self.messages.iter().enumerate() {
                if message.streaming {
                    self.list.remeasure_items(index..index + 1);
                }
            }
            cx.notify();
        }
        changed
    }

    /// Start the reveal loop when a stream has text the reader cannot see yet.
    fn start_typewriter(&mut self, cx: &mut Context<Self>) {
        if self.typewriter_running || !self.has_unrevealed_streaming() {
            return;
        }
        self.typewriter_running = true;
        cx.spawn(async move |conversation, cx| {
            loop {
                cx.background_executor().timer(TYPEWRITER_FRAME).await;
                let Ok(keep_running) = conversation.update(cx, |conversation, cx| {
                    conversation.advance_typewriter(TYPEWRITER_FRAME.as_secs_f32(), cx);
                    let keep_running = conversation.has_unrevealed_streaming();
                    if !keep_running {
                        conversation.typewriter_running = false;
                    }
                    keep_running
                }) else {
                    return;
                };
                if !keep_running {
                    return;
                }
            }
        })
        .detach();
    }

    fn has_unrevealed_streaming(&self) -> bool {
        self.messages.iter().any(|message| {
            message.streaming && self.typewriter.visible_text(message) != message.full_text
        })
    }

    /// Reveal a streaming entry in full.
    pub fn reveal_all(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some((index, message)) = self
            .messages
            .iter()
            .enumerate()
            .find(|(_, message)| message.id == id)
        {
            self.typewriter.reveal_all(message);
            self.list.remeasure_items(index..index + 1);
            self.scroll_to_latest();
            cx.notify();
        }
    }

    /// Reveal the newest streaming reply, which is what Escape acts on.
    pub fn reveal_latest(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(id) = self
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.streaming && self.typewriter.visible_text(message) != message.full_text
            })
            .map(|message| message.id.clone())
        else {
            return false;
        };
        self.reveal_all(&id, cx);
        true
    }

    pub fn is_tool_report_expanded(&self, id: &str) -> bool {
        self.expansions
            .get(id)
            .is_some_and(|expansion| expansion.tool_report)
    }

    pub fn is_tool_details_expanded(&self, id: &str) -> bool {
        self.expansions
            .get(id)
            .is_some_and(|expansion| expansion.tool_details)
    }

    pub fn is_thinking_expanded(&self, id: &str, segment: usize) -> bool {
        self.expansions
            .get(id)
            .is_some_and(|expansion| expansion.thinking.contains(&segment))
    }

    /// Show or hide the normal output of a tool entry.
    pub fn toggle_tool_report(&mut self, id: &str, cx: &mut Context<Self>) {
        let expansion = self.expansions.entry(id.to_owned()).or_default();
        expansion.tool_report = !expansion.tool_report;
        self.drop_empty_expansion(id);
        self.remeasure(id);
        cx.notify();
    }

    /// Show or hide a tool entry's raw diagnostic data.
    pub fn toggle_tool_details(&mut self, id: &str, cx: &mut Context<Self>) {
        let expansion = self.expansions.entry(id.to_owned()).or_default();
        expansion.tool_details = !expansion.tool_details;
        self.drop_empty_expansion(id);
        self.remeasure(id);
        cx.notify();
    }

    /// Show or hide one reasoning section in an assistant message.
    pub fn toggle_thinking(&mut self, id: &str, segment: usize, cx: &mut Context<Self>) {
        let expansion = self.expansions.entry(id.to_owned()).or_default();
        if !expansion.thinking.remove(&segment) {
            expansion.thinking.insert(segment);
        }
        self.drop_empty_expansion(id);
        self.remeasure(id);
        cx.notify();
    }

    fn drop_empty_expansion(&mut self, id: &str) {
        if self
            .expansions
            .get(id)
            .is_some_and(MessageExpansion::is_empty)
        {
            self.expansions.remove(id);
        }
    }

    fn remeasure(&mut self, id: &str) {
        if let Some(index) = self.messages.iter().position(|message| message.id == id) {
            self.list.remeasure_items(index..index + 1);
        }
    }

    /// Scroll so the newest entry is in view.
    pub fn scroll_to_latest(&mut self) {
        if !self.messages.is_empty() {
            self.list.set_follow_mode(FollowMode::Tail);
        }
    }

    pub fn messages(&self) -> &[ConversationItem] {
        &self.messages
    }

    /// What an empty transcript says instead of nothing.
    ///
    /// The heading always shows; the line under it only when there is no project,
    /// because that is the one case where the reader has something to do first.
    fn render_empty_state(&self) -> AnyElement {
        let tokens = self.tokens;
        div()
            .flex_1()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(6.0))
            .child(
                div()
                    .text_size(px(text::BODY_SIZE))
                    .text_color(tokens.text.hsla())
                    .child(translate("empty-heading", self.language)),
            )
            .when(!self.has_project, |this| {
                this.child(
                    div()
                        .text_size(px(text::DETAIL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(translate("empty-detail", self.language)),
                )
            })
            .into_any_element()
    }

    fn render_entry(&self, index: usize, owner: gpui::Entity<Self>) -> AnyElement {
        let Some(message) = self.messages.get(index) else {
            return div().into_any_element();
        };
        MessageCard::with_expansion(
            index,
            message.clone(),
            self.typewriter.visible_text(message).to_owned(),
            self.expansions
                .get(&message.id)
                .cloned()
                .unwrap_or_default(),
            self.tokens,
        )
        .language(self.language)
        .events(owner)
        .into_any_element()
    }

    fn render_generating(&self) -> AnyElement {
        let tokens = self.tokens;
        reading_lane(
            div()
                .flex()
                .items_center()
                .gap(px(7.0))
                .text_size(px(text::LABEL_SIZE))
                .text_color(tokens.muted.hsla())
                .child(div().w(px(6.0)).h(px(6.0)).bg(tokens.accent.hsla()))
                .child(translate("generating", self.language)),
        )
        .py(px(5.0))
        .into_any_element()
    }
}

fn rekey_expansion(expansions: &mut HashMap<String, MessageExpansion>, from: &str, to: &str) {
    if let Some(expansion) = expansions.remove(from) {
        expansions
            .entry(to.to_owned())
            .or_default()
            .merge(expansion);
    }
}

fn changed_message_indices(before: &[ConversationItem], after: &[ConversationItem]) -> Vec<usize> {
    before
        .iter()
        .zip(after)
        .enumerate()
        .filter_map(|(index, (old, new))| (old != new).then_some(index))
        .collect()
}

/// The entries worth rendering from state.
///
/// Empty entries that are not carrying a tool report would render as blank rows,
/// which reads as a glitch rather than as a message.
pub fn visible_messages(state: &AppState) -> Vec<ConversationItem> {
    state
        .conversation
        .iter()
        .filter(|message| {
            !message.full_text.trim().is_empty()
                || message.tool_report.is_some()
                || !message.attachments.is_empty()
                || matches!(message.role, ConversationRole::Tool)
        })
        .cloned()
        .collect()
}

impl Render for Conversation {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let entity = cx.entity();

        // No background: the graph paper the shell lays down behind everything
        // shows through here, which is the whole point of it being at the root.
        //
        // The container has to be a flex box: `list` measures nothing itself, so
        // outside a flex context `flex_1` does not apply and it lays out at zero
        // height — a blank conversation with no error anywhere.
        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .min_h(px(0.0))
            // A blank canvas reads as a broken view rather than as an empty one,
            // so the transcript says what to do while there is nothing in it.
            .when(self.messages.is_empty(), |this| {
                this.child(self.render_empty_state())
            })
            .when(!self.messages.is_empty(), |this| {
                this.child(
                    list(self.list.clone(), move |index, _window, cx| {
                        entity.read(cx).render_entry(index, entity.clone())
                    })
                    .flex_1(),
                )
                .when(self.generating, |this| this.child(self.render_generating()))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;
    use pi_whim_core::Action;

    fn message(id: &str, role: ConversationRole, text: &str) -> ConversationItem {
        ConversationItem {
            id: id.into(),
            role,
            full_text: text.into(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn blank_entries_are_not_rendered_as_rows() {
        let mut state = AppState::default();
        state.dispatch(Action::UpsertConversation(message(
            "a",
            ConversationRole::Assistant,
            "an answer",
        )));
        state.dispatch(Action::UpsertConversation(message(
            "b",
            ConversationRole::Assistant,
            "   ",
        )));

        let visible = visible_messages(&state);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].id, "a");
    }

    #[test]
    fn a_tool_entry_shows_even_before_it_has_text() {
        // A tool card with no text yet is still a card: it names what is running.
        let mut state = AppState::default();
        state.dispatch(Action::UpsertConversation(message(
            "t",
            ConversationRole::Tool,
            "",
        )));

        assert_eq!(visible_messages(&state).len(), 1);
    }

    #[test]
    fn an_entry_carrying_only_a_report_still_shows() {
        let mut state = AppState::default();
        let mut item = message("r", ConversationRole::Assistant, "");
        item.tool_report = Some("ran ls -la".into());
        state.dispatch(Action::UpsertConversation(item));

        assert_eq!(visible_messages(&state).len(), 1);
    }

    #[test]
    fn an_attachment_only_prompt_still_shows() {
        use pi_whim_core::{Attachment, AttachmentKind};

        let mut state = AppState::default();
        let mut item = message("a", ConversationRole::User, "");
        item.attachments.push(Attachment {
            name: "notes.txt".into(),
            path: "/tmp/notes.txt".into(),
            kind: AttachmentKind::File,
            generated_by_app: false,
        });
        state.dispatch(Action::UpsertConversation(item));

        assert_eq!(visible_messages(&state).len(), 1);
    }

    #[test]
    fn a_changed_non_tail_entry_is_remeasured() {
        let before = vec![
            message("tool-a", ConversationRole::Tool, "running"),
            message("tool-b", ConversationRole::Tool, "running"),
        ];
        let mut after = before.clone();
        after[0].tool_report = Some("a much taller report".into());
        after.push(message("tail", ConversationRole::Assistant, "answer"));

        assert_eq!(changed_message_indices(&before, &after), vec![0]);
    }

    #[gpui::test]
    async fn streaming_text_is_advanced_without_a_window_frame_loop(cx: &mut gpui::TestAppContext) {
        let conversation = cx.update(|cx| cx.new(|_| Conversation::new(Tokens::light())));
        let mut item = message("stream", ConversationRole::Assistant, "hello");
        item.streaming = true;
        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.set_messages(vec![item], cx);
            });
        });

        let visible = |cx: &gpui::App| {
            let conversation = conversation.read(cx);
            conversation
                .typewriter
                .visible_text(&conversation.messages[0])
                .to_owned()
        };
        assert!(cx.read(visible).is_empty());

        // Poll once to arm the timer, then move the test clock through one tick.
        cx.run_until_parked();
        for _ in 0..4 {
            cx.executor().advance_clock(TYPEWRITER_FRAME);
            cx.run_until_parked();
        }

        assert!(!cx.read(visible).is_empty());
    }

    #[gpui::test]
    async fn a_stream_rekey_keeps_the_visible_prefix(cx: &mut gpui::TestAppContext) {
        let conversation = cx.update(|cx| cx.new(|_| Conversation::new(Tokens::light())));
        let mut draft = message("draft", ConversationRole::Assistant, "hello");
        draft.streaming = true;
        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.set_messages(vec![draft], cx);
                conversation.reveal_all("draft", cx);
                conversation.toggle_tool_report("draft", cx);
                conversation.toggle_tool_details("draft", cx);
                conversation.toggle_thinking("draft", 1, cx);
            });
        });

        let mut persisted = message("entry-1", ConversationRole::Assistant, "hello world");
        persisted.streaming = true;
        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.set_messages(vec![persisted], cx);
                assert_eq!(
                    conversation
                        .typewriter
                        .visible_text(&conversation.messages[0]),
                    "hello"
                );
                assert!(conversation.is_tool_report_expanded("entry-1"));
                assert!(conversation.is_tool_details_expanded("entry-1"));
                assert!(conversation.is_thinking_expanded("entry-1", 1));
                assert!(!conversation.expansions.contains_key("draft"));
            });
        });
    }

    #[gpui::test]
    async fn disclosures_default_closed_and_toggle_independently(cx: &mut gpui::TestAppContext) {
        let conversation = cx.update(|cx| cx.new(|_| Conversation::new(Tokens::light())));
        let mut tool = message("tool", ConversationRole::Tool, "running");
        tool.tool_report = Some("report".into());
        tool.tool_details = Some("{\"raw\":true}".into());

        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.set_messages(vec![tool], cx);
                assert!(!conversation.is_tool_report_expanded("tool"));
                assert!(!conversation.is_tool_details_expanded("tool"));
                assert!(!conversation.is_thinking_expanded("tool", 2));

                conversation.toggle_tool_report("tool", cx);
                assert!(conversation.is_tool_report_expanded("tool"));
                assert!(!conversation.is_tool_details_expanded("tool"));

                conversation.toggle_tool_details("tool", cx);
                conversation.toggle_thinking("tool", 2, cx);
                assert!(conversation.is_tool_report_expanded("tool"));
                assert!(conversation.is_tool_details_expanded("tool"));
                assert!(conversation.is_thinking_expanded("tool", 2));

                conversation.toggle_tool_report("tool", cx);
                assert!(!conversation.is_tool_report_expanded("tool"));
                assert!(conversation.is_tool_details_expanded("tool"));
                assert!(conversation.is_thinking_expanded("tool", 2));

                conversation.toggle_tool_details("tool", cx);
                conversation.toggle_thinking("tool", 2, cx);
                assert!(!conversation.expansions.contains_key("tool"));
            });
        });
    }

    #[gpui::test]
    async fn removing_a_message_drops_its_disclosure_state(cx: &mut gpui::TestAppContext) {
        let conversation = cx.update(|cx| cx.new(|_| Conversation::new(Tokens::light())));
        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.set_messages(
                    vec![message("gone", ConversationRole::Assistant, "answer")],
                    cx,
                );
                conversation.toggle_thinking("gone", 0, cx);
                assert!(conversation.expansions.contains_key("gone"));

                conversation.set_messages(Vec::new(), cx);
                assert!(conversation.expansions.is_empty());
            });
        });
    }

    #[gpui::test]
    async fn streaming_does_not_reengage_tail_after_the_reader_scrolls_up(
        cx: &mut gpui::TestAppContext,
    ) {
        let conversation = cx.update(|cx| cx.new(|_| Conversation::new(Tokens::light())));
        let mut stream = message("stream", ConversationRole::Assistant, "first");
        stream.streaming = true;
        cx.update(|cx| {
            conversation.update(cx, |conversation, cx| {
                conversation.set_messages(vec![stream.clone()], cx);
                conversation.list.scroll_by(px(-1.0));
                assert!(!conversation.list.is_following_tail());

                stream.full_text.push_str(" second");
                conversation.set_messages(vec![stream], cx);
                conversation.advance_typewriter(TYPEWRITER_FRAME.as_secs_f32(), cx);

                assert!(!conversation.list.is_following_tail());
            });
        });
    }

    // Zero overdraw would mean scrolling reveals blank space before it fills in.
    const _: () = assert!(OVERDRAW > 0.0);
}
