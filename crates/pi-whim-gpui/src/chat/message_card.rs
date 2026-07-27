//! One conversation entry.
//!
//! Four roles, four shapes. A user's prompt sits right-aligned in a narrower
//! block so the two sides of the conversation are distinguishable at a glance; an
//! assistant reply renders as markdown across the full measure; a tool call is a
//! card that can be expanded for its report; a system notice is mono text.
//!
//! Reasoning arrives inside the assistant's text wrapped in `<thinking>` tags,
//! and is presented as a collapsed section rather than shown as markup.

use std::collections::HashSet;

use gpui::{
    AnyElement, App, Entity, Hsla, InteractiveElement, IntoElement, ParentElement, RenderOnce,
    SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Icon, IconName, Sizable,
    button::{Button, ButtonVariants},
    text::TextView,
};
use pi_whim_core::{ConversationItem, ConversationRole, Language, strings::text as translate};
use pi_whim_engine::thinking::{Segment, split_thinking_segments};
use pi_whim_theme::{Rgba, Tokens, layout, text};

use crate::{
    chat::{
        Conversation, ConversationEvent, ToolCard, message_disclosure::disclosure_button,
        reading_lane,
    },
    icons,
    theme::IntoHsla,
};

/// The independently expandable sections belonging to one message.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct MessageExpansion {
    pub(crate) tool_report: bool,
    pub(crate) tool_details: bool,
    pub(crate) thinking: HashSet<usize>,
}

impl MessageExpansion {
    pub(crate) fn merge(&mut self, other: Self) {
        self.tool_report |= other.tool_report;
        self.tool_details |= other.tool_details;
        self.thinking.extend(other.thinking);
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.tool_report && !self.tool_details && self.thinking.is_empty()
    }
}

/// A single conversation entry.
#[derive(IntoElement)]
pub struct MessageCard {
    index: usize,
    message: ConversationItem,
    /// The text to show, which for a streaming message is a prefix of the whole.
    visible_text: SharedString,
    expansion: MessageExpansion,
    language: Language,
    /// The owning transcript receives action events so cards remain stateless.
    events: Option<Entity<Conversation>>,
    tokens: Tokens,
}

impl MessageCard {
    pub fn new(
        index: usize,
        message: ConversationItem,
        visible_text: impl Into<SharedString>,
        details_expanded: bool,
        tokens: Tokens,
    ) -> Self {
        Self::with_expansion(
            index,
            message,
            visible_text,
            MessageExpansion {
                // Before double disclosure was restored, the normal report was
                // always visible and the public flag controlled raw details.
                tool_report: true,
                tool_details: details_expanded,
                thinking: HashSet::new(),
            },
            tokens,
        )
    }

    pub(crate) fn with_expansion(
        index: usize,
        message: ConversationItem,
        visible_text: impl Into<SharedString>,
        expansion: MessageExpansion,
        tokens: Tokens,
    ) -> Self {
        Self {
            index,
            message,
            visible_text: visible_text.into(),
            expansion,
            language: Language::default(),
            events: None,
            tokens,
        }
    }

    pub fn language(mut self, language: Language) -> Self {
        self.language = language;
        self
    }

    pub fn events(mut self, owner: Entity<Conversation>) -> Self {
        self.events = Some(owner);
        self
    }

    /// The most this entry's content may span.
    ///
    /// A ceiling, not a width: a short prompt hugs its text. Held narrower than
    /// the column for a prompt so the alternation between asking and answering is
    /// legible without reading the text.
    fn content_width(&self) -> f32 {
        match self.message.role {
            ConversationRole::User => layout::USER_MESSAGE_WIDTH,
            _ => layout::CHAT_CONTENT_WIDTH,
        }
    }

    /// Whether this entry shrinks to its text rather than filling the measure.
    fn hugs_its_content(&self) -> bool {
        matches!(self.message.role, ConversationRole::User)
    }

    /// Render assistant text, folding any reasoning it contains.
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
                    let expanded = self.expansion.thinking.contains(&part);
                    div()
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(px(3.0))
                        .child(disclosure_button(
                            format!("thinking-{}-{part}", self.index),
                            translate("thinking-process", self.language),
                            expanded,
                            tokens.muted.hsla(),
                            ConversationEvent::ToggleThinking {
                                id: self.message.id.clone(),
                                segment: part,
                            },
                            self.events.clone(),
                        ))
                        .when(expanded, |this| {
                            this.child(
                                div()
                                    .w_full()
                                    .pl(px(10.0))
                                    .border_l_2()
                                    .border_color(tokens.line.hsla())
                                    .text_color(tokens.muted.hsla())
                                    .text_size(px(text::DETAIL_SIZE))
                                    .child(slice.to_owned()),
                            )
                        })
                        .into_any_element()
                } else {
                    TextView::markdown(("assistant-md", self.index * 32 + part), slice.to_owned())
                        .selectable(true)
                        .into_any_element()
                })
            })
            .collect()
    }

    /// Compact chips for files that travelled with a sent user message.
    fn attachment_cards(&self) -> AnyElement {
        let tokens = self.tokens;
        div()
            .flex()
            .flex_wrap()
            .gap(px(4.0))
            .pt(px(6.0))
            .children(self.message.attachments.iter().map(|attachment| {
                div()
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .px(px(5.0))
                    .py(px(2.0))
                    .bg(opaque_over(tokens.accent_surface_soft(), tokens))
                    .border_1()
                    .border_color(tokens.accent_border_muted().hsla())
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .child(Icon::new(icons::attachment()).size(px(10.0)))
                    .child(attachment.name.clone())
            }))
            .into_any_element()
    }

    fn action_button(
        &self,
        id: String,
        label: &'static str,
        icon: Option<IconName>,
        event: ConversationEvent,
    ) -> Option<AnyElement> {
        let owner = self.events.clone()?;
        let button = Button::new(id).xsmall().tooltip(label);
        let button = if let Some(icon) = icon {
            button.ghost().icon(icon)
        } else {
            // Text actions should not reserve the component library's fixed 20px
            // xsmall button box below a message bubble.
            button.text().label(label)
        };
        Some(
            button
                .on_click(move |_, _, cx| {
                    owner.update(cx, |_, cx| cx.emit(event.clone()));
                })
                .into_any_element(),
        )
    }
}

/// A card fill, flattened against the canvas it sits on.
///
/// pi.dev's surfaces are alpha steps over the page, which works there because the
/// page behind them is flat. Here the canvas carries the graph paper, so a
/// translucent fill lets the grid run straight through the text. Compositing
/// against `bg_canvas` keeps the intended colour and stops the paper at the card's
/// edge.
pub(crate) fn opaque_over(fill: Rgba, tokens: Tokens) -> Hsla {
    fill.over(tokens.bg_canvas).hsla()
}

/// Keep stored prompts byte-for-byte intact while suppressing display-only
/// whitespace that would otherwise make a bubble look one line taller.
fn user_text_for_display(text: &str) -> &str {
    text.trim_end()
}

impl RenderOnce for MessageCard {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tokens = self.tokens;
        let width = self.content_width();
        let role = self.message.role.clone();
        let language = self.language;
        let message_id = self.message.id.clone();
        let has_attachments = !self.message.attachments.is_empty();
        let hugs_its_content = self.hugs_its_content();
        let visible_user_text = user_text_for_display(self.visible_text.as_ref()).to_owned();

        // A prompt is a bubble, so it shrinks to its text; everything else fills
        // the measure, because markdown and tool reports need a stable column to
        // wrap against. `w(width)` for both would pad a three-word question out to
        // 620px of empty fill.
        let content = match role {
            ConversationRole::User => div()
                .flex()
                .flex_col()
                .items_end()
                .gap(px(2.0))
                .child(
                    div()
                        .p(px(10.0))
                        // Composited rather than translucent: a card sitting on the graph
                        // paper would otherwise have the grid running through the text
                        // behind it.
                        .bg(opaque_over(tokens.accent_surface_soft(), tokens))
                        .border_1()
                        .border_color(tokens.accent_border_muted().hsla())
                        .text_color(tokens.text.hsla())
                        .when(!visible_user_text.is_empty(), |this| {
                            this.child(visible_user_text)
                        })
                        .when(has_attachments, |this| this.child(self.attachment_cards())),
                )
                .when_some(
                    self.action_button(
                        format!("fork-message-{}", self.index),
                        translate("fork-here", language),
                        None,
                        ConversationEvent::ForkAt(message_id.clone()),
                    ),
                    |this, button| this.child(button),
                )
                .into_any_element(),

            ConversationRole::Assistant => {
                let reveal_pending =
                    self.message.streaming && self.visible_text.as_ref() != self.message.full_text;
                let action_group = SharedString::from(format!("assistant-actions-{}", self.index));
                let action = if reveal_pending {
                    self.action_button(
                        format!("reveal-message-{}", self.index),
                        translate("show-all", language),
                        Some(icons::details(false)),
                        ConversationEvent::RevealAll(message_id.clone()),
                    )
                } else if !self.message.streaming && !self.message.full_text.trim().is_empty() {
                    self.action_button(
                        format!("copy-message-{}", self.index),
                        translate("copy-report", language),
                        Some(icons::copy()),
                        ConversationEvent::CopyAssistant(message_id.clone()),
                    )
                } else {
                    None
                };

                div()
                    .group(action_group.clone())
                    .relative()
                    .w_full()
                    .flex()
                    .flex_col()
                    // The action floats over this reserved edge instead of
                    // becoming a trailing flex row that makes every reply look
                    // one line taller.
                    .pr(px(28.0))
                    .gap(px(8.0))
                    .children(self.assistant_body())
                    .when_some(action, |this, button| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .right_0()
                                .when(!reveal_pending, |this| {
                                    this.invisible()
                                        .group_hover(action_group, |this| this.visible())
                                })
                                .child(button),
                        )
                    })
                    .into_any_element()
            }

            ConversationRole::Tool => ToolCard::new(
                self.index,
                self.message.clone(),
                self.expansion.tool_report,
                self.expansion.tool_details,
                language,
                self.events.clone(),
                tokens,
            )
            .into_any_element(),

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
        };

        // Every role lives in the same centred reading lane. Assistant, tool, and
        // system content fill it from the left; a user bubble hugs the lane's right
        // edge instead of the window's right edge.
        let body = div()
            .w_full()
            .when(matches!(role, ConversationRole::User), |this| {
                this.flex().justify_end()
            })
            .child(
                div()
                    .max_w(px(width))
                    .when(!hugs_its_content, |this| this.w_full())
                    .child(content),
            );

        reading_lane(body).py(px(6.0))
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
    fn a_prompt_hugs_its_text_while_a_reply_fills_the_measure() {
        // A prompt bubble sized to the full 620px would pad a three-word question
        // out with empty fill; a reply needs the stable column to wrap against.
        assert!(card(ConversationRole::User, "hi").hugs_its_content());
        assert!(!card(ConversationRole::Assistant, "hi").hugs_its_content());
        assert!(!card(ConversationRole::Tool, "hi").hugs_its_content());
        assert!(!card(ConversationRole::System, "hi").hugs_its_content());
    }

    #[test]
    fn card_fills_are_opaque_so_the_grid_stops_at_their_edge() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            for fill in [
                tokens.accent_surface_soft(),
                tokens.surface_tint(),
                tokens.panel_soft,
            ] {
                assert_eq!(opaque_over(fill, tokens).a, 1.0);
            }
        }
    }

    #[test]
    fn compositing_preserves_the_intended_tint() {
        // Flattening must not turn the accent wash into plain canvas: the card
        // still has to read as tinted, just without the paper showing through.
        let tokens = Tokens::light();
        let flattened = tokens.accent_surface_soft().over(tokens.bg_canvas);
        assert_ne!(flattened.to_hexa(), tokens.bg_canvas.to_hexa());
    }

    #[test]
    fn a_streaming_prefix_is_what_gets_rendered() {
        // The card shows what the typewriter has revealed, not the full text.
        let full = message(ConversationRole::Assistant, "the whole answer");
        let partial = MessageCard::new(0, full, "the whole", false, Tokens::light());
        assert_eq!(partial.visible_text.as_ref(), "the whole");
    }

    #[test]
    fn prompt_display_drops_only_trailing_whitespace() {
        assert_eq!(user_text_for_display("question\n"), "question");
        assert_eq!(user_text_for_display("question  \n\n"), "question");
        assert_eq!(user_text_for_display("  question"), "  question");
    }
}
