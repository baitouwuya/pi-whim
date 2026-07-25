//! One conversation entry.
//!
//! Four roles, four shapes. A user's prompt sits right-aligned in a narrower
//! block so the two sides of the conversation are distinguishable at a glance; an
//! assistant reply renders as markdown across the full measure; a tool call is a
//! card that can be expanded for its report; a system notice is mono text.
//!
//! Reasoning arrives inside the assistant's text wrapped in `<thinking>` tags,
//! and is rendered muted rather than shown as markup.

use gpui::{
    AnyElement, App, IntoElement, ParentElement, RenderOnce, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{Icon, text::TextView};
use pi_whim_core::{ConversationItem, ConversationRole};
use pi_whim_engine::thinking::{Segment, split_thinking_segments};
use pi_whim_theme::{Tokens, layout, text};

use crate::{icons, theme::IntoHsla};

/// A single conversation entry.
#[derive(IntoElement)]
pub struct MessageCard {
    index: usize,
    message: ConversationItem,
    /// The text to show, which for a streaming message is a prefix of the whole.
    visible_text: SharedString,
    expanded: bool,
    tokens: Tokens,
}

impl MessageCard {
    pub fn new(
        index: usize,
        message: ConversationItem,
        visible_text: impl Into<SharedString>,
        expanded: bool,
        tokens: Tokens,
    ) -> Self {
        Self {
            index,
            message,
            visible_text: visible_text.into(),
            expanded,
            tokens,
        }
    }

    /// How wide this entry's content may be.
    ///
    /// User prompts are held narrower than the column so the alternation between
    /// asking and answering is legible without reading the text.
    fn content_width(&self) -> f32 {
        match self.message.role {
            ConversationRole::User => layout::USER_MESSAGE_WIDTH,
            _ => layout::CHAT_CONTENT_WIDTH,
        }
    }

    /// Render assistant text, muting any reasoning it contains.
    fn assistant_body(&self) -> Vec<AnyElement> {
        let source = self.visible_text.as_ref();
        let tokens = self.tokens;

        split_thinking_segments(source)
            .into_iter()
            .enumerate()
            .filter_map(|(part, segment)| {
                let (range, is_thinking) = match segment {
                    Segment::Markdown(range) => (range, false),
                    Segment::Thinking(range) => (range, true),
                };
                let slice = source.get(range)?.trim();
                if slice.is_empty() {
                    return None;
                }
                Some(if is_thinking {
                    div()
                        .pl(px(10.0))
                        .border_l_2()
                        .border_color(tokens.line.hsla())
                        .text_color(tokens.muted.hsla())
                        .text_size(px(text::DETAIL_SIZE))
                        .child(slice.to_owned())
                        .into_any_element()
                } else {
                    TextView::markdown(("assistant-md", self.index * 32 + part), slice.to_owned())
                        .into_any_element()
                })
            })
            .collect()
    }
}

impl RenderOnce for MessageCard {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tokens = self.tokens;
        let width = self.content_width();
        let role = self.message.role.clone();

        let body = div().w(px(width)).child(match role {
            ConversationRole::User => div()
                .p(px(10.0))
                .bg(tokens.accent_surface_soft().hsla())
                .border_1()
                .border_color(tokens.accent_border_muted().hsla())
                .text_color(tokens.text.hsla())
                .child(self.visible_text.clone())
                .into_any_element(),

            ConversationRole::Assistant => div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .children(self.assistant_body())
                .into_any_element(),

            ConversationRole::Tool => {
                let name = self
                    .message
                    .tool_name
                    .clone()
                    .unwrap_or_else(|| "tool".to_owned());
                let accent = if self.message.is_error {
                    tokens.error
                } else {
                    tokens.accent
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .p(px(8.0))
                    .bg(tokens.surface_tint().hsla())
                    .border_1()
                    .border_color(accent.alpha(0.25).hsla())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(5.0))
                            .when_some(icons::role(&ConversationRole::Tool), |this, icon| {
                                this.child(Icon::new(icon).size(px(12.0)).text_color(accent.hsla()))
                            })
                            .child(
                                div()
                                    .text_size(px(text::LABEL_SIZE))
                                    .text_color(accent.hsla())
                                    .child(name),
                            ),
                    )
                    .when_some(self.message.tool_report.clone(), |this, report| {
                        this.child(
                            div()
                                .text_size(px(text::MONO_DETAIL_SIZE))
                                .text_color(tokens.muted.hsla())
                                .child(report),
                        )
                    })
                    // Raw event data is a second level of detail, shown only when
                    // asked for: it is diagnostic, not part of reading the
                    // conversation.
                    .when(self.expanded, |this| {
                        this.when_some(self.message.tool_details.clone(), |this, details| {
                            this.child(
                                div()
                                    .p(px(6.0))
                                    .bg(tokens.panel_soft.hsla())
                                    .text_size(px(text::MONO_DETAIL_SIZE))
                                    .text_color(tokens.muted.hsla())
                                    .child(details),
                            )
                        })
                    })
                    .into_any_element()
            }

            ConversationRole::System => div()
                .flex()
                .items_center()
                .gap(px(5.0))
                .when_some(icons::role(&ConversationRole::System), |this, icon| {
                    this.child(
                        Icon::new(icon)
                            .size(px(12.0))
                            .text_color(tokens.muted.hsla()),
                    )
                })
                .child(
                    div()
                        .text_size(px(text::MONO_DETAIL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(self.visible_text.clone()),
                )
                .into_any_element(),
        });

        div()
            .w_full()
            .flex()
            .px(px(16.0))
            .py(px(6.0))
            // A prompt hugs the trailing edge, everything else the leading one.
            .when(matches!(role, ConversationRole::User), |this| {
                this.justify_end()
            })
            .child(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: ConversationRole, text: &str) -> ConversationItem {
        ConversationItem {
            id: "m1".into(),
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

    fn card(role: ConversationRole, text: &str) -> MessageCard {
        MessageCard::new(0, message(role.clone(), text), text, false, Tokens::light())
    }

    #[test]
    fn prompts_are_narrower_than_replies() {
        // The width difference is what makes the two sides distinguishable
        // without reading them.
        assert!(
            card(ConversationRole::User, "hi").content_width()
                < card(ConversationRole::Assistant, "hi").content_width()
        );
    }

    #[test]
    fn tools_and_notices_use_the_full_measure() {
        let full = layout::CHAT_CONTENT_WIDTH;
        assert_eq!(card(ConversationRole::Tool, "x").content_width(), full);
        assert_eq!(card(ConversationRole::System, "x").content_width(), full);
    }

    #[test]
    fn assistant_text_splits_reasoning_from_the_reply() {
        let reply = card(
            ConversationRole::Assistant,
            "<thinking>weighing it up</thinking>Here is the answer.",
        );
        // Two blocks: the muted reasoning and the reply.
        assert_eq!(reply.assistant_body().len(), 2);
    }

    #[test]
    fn plain_replies_render_as_one_block() {
        assert_eq!(
            card(ConversationRole::Assistant, "Just an answer.")
                .assistant_body()
                .len(),
            1
        );
    }

    #[test]
    fn empty_segments_are_dropped() {
        // A reply that is nothing but reasoning should not leave a blank block
        // where the prose would be.
        let thinking_only = card(
            ConversationRole::Assistant,
            "<thinking>still working</thinking>",
        );
        assert_eq!(thinking_only.assistant_body().len(), 1);

        assert!(
            card(ConversationRole::Assistant, "   ")
                .assistant_body()
                .is_empty()
        );
    }

    #[test]
    fn a_streaming_prefix_is_what_gets_rendered() {
        // The card shows what the typewriter has revealed, not the full text.
        let full = message(ConversationRole::Assistant, "the whole answer");
        let partial = MessageCard::new(0, full, "the whole", false, Tokens::light());
        assert_eq!(partial.visible_text.as_ref(), "the whole");
    }
}
