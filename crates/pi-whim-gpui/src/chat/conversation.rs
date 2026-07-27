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

use std::collections::HashSet;

use gpui::{
    AnyElement, Context, EventEmitter, IntoElement, ListAlignment, ListState, ParentElement,
    Render, Styled, Window, div, list, prelude::FluentBuilder, px,
};
use pi_whim_core::{
    AppState, ConversationItem, ConversationRole, Language, strings::text as translate,
};
use pi_whim_engine::typewriter::Typewriter;
use pi_whim_theme::{Tokens, text};

use crate::{chat::MessageCard, theme::IntoHsla};

/// How far beyond the visible span to render, so scrolling does not flash blank.
const OVERDRAW: f32 = 400.0;

/// What the conversation asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversationEvent {
    /// Show or hide a tool card's raw event data.
    ToggleToolDetails(String),
}

/// The scrolling list of conversation entries.
pub struct Conversation {
    messages: Vec<ConversationItem>,
    /// Tool entries whose diagnostic detail is showing, by message id.
    expanded: HashSet<String>,
    typewriter: Typewriter,
    /// Whether a project is selected, which is what the empty state turns on.
    ///
    /// Without one there is nothing to talk to, so the empty transcript says how
    /// to get started rather than sitting blank.
    has_project: bool,
    /// The language the empty state is read in.
    language: Language,
    tokens: Tokens,
    list: ListState,
}

impl EventEmitter<ConversationEvent> for Conversation {}

impl Conversation {
    pub fn new(tokens: Tokens) -> Self {
        Self {
            messages: Vec::new(),
            expanded: HashSet::new(),
            typewriter: Typewriter::new(),
            has_project: false,
            language: Language::default(),
            tokens,
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
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
        self.messages = messages;

        if next >= previous {
            // Appended, or the tail changed in place.
            self.list.splice(previous..previous, next - previous);
            if previous > 0 {
                // The last existing entry may have grown while streaming.
                self.list.splice(previous - 1..previous, 1);
            }
        } else {
            // Entries went away, which only happens on a reset.
            self.list.reset(next);
        }
        cx.notify();
    }

    /// Drop everything, for switching sessions or clearing the conversation.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.messages.clear();
        self.expanded.clear();
        self.typewriter.clear();
        self.list.reset(0);
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

    /// Advance the typewriter, reporting whether anything became visible.
    pub fn advance_typewriter(&mut self, elapsed_seconds: f32, cx: &mut Context<Self>) -> bool {
        let changed = self.typewriter.advance(&self.messages, elapsed_seconds);
        if changed {
            cx.notify();
        }
        changed
    }

    /// Reveal a streaming entry in full.
    pub fn reveal_all(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some(message) = self.messages.iter().find(|message| message.id == id) {
            self.typewriter.reveal_all(message);
            cx.notify();
        }
    }

    /// Whether a tool entry's diagnostic detail is showing.
    pub fn is_expanded(&self, id: &str) -> bool {
        self.expanded.contains(id)
    }

    /// Show or hide a tool entry's diagnostic detail.
    pub fn toggle_details(&mut self, id: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(id) {
            self.expanded.insert(id.to_owned());
        }
        cx.notify();
    }

    /// Scroll so the newest entry is in view.
    pub fn scroll_to_latest(&mut self) {
        if let Some(last) = self.messages.len().checked_sub(1) {
            self.list.scroll_to_reveal_item(last);
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

    fn render_entry(&self, index: usize) -> AnyElement {
        let Some(message) = self.messages.get(index) else {
            return div().into_any_element();
        };
        MessageCard::new(
            index,
            message.clone(),
            self.typewriter.visible_text(message).to_owned(),
            self.expanded.contains(&message.id),
            self.tokens,
        )
        .into_any_element()
    }
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
                        entity.read(cx).render_entry(index)
                    })
                    .flex_1(),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    // Zero overdraw would mean scrolling reveals blank space before it fills in.
    const _: () = assert!(OVERDRAW > 0.0);
}
