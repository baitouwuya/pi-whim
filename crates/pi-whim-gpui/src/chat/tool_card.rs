//! A tool invocation with two independent levels of disclosure.

use gpui::{
    App, Entity, InteractiveElement, IntoElement, ParentElement, RenderOnce, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Icon,
    scroll::{Scrollbar, ScrollbarShow},
};
use pi_whim_core::{ConversationItem, Language, strings::text as translate};
use pi_whim_theme::{Tokens, font, text};

use crate::{
    chat::{
        Conversation, ConversationEvent, message_card::opaque_over,
        message_disclosure::disclosure_button,
    },
    icons,
    theme::IntoHsla,
};

const RAW_DETAILS_MAX_HEIGHT: f32 = 280.0;

/// Tool output is collapsed once at the report and again at the raw event data.
#[derive(IntoElement)]
pub(crate) struct ToolCard {
    index: usize,
    message: ConversationItem,
    report_expanded: bool,
    details_expanded: bool,
    language: Language,
    events: Option<Entity<Conversation>>,
    tokens: Tokens,
}

impl ToolCard {
    pub(crate) fn new(
        index: usize,
        message: ConversationItem,
        report_expanded: bool,
        details_expanded: bool,
        language: Language,
        events: Option<Entity<Conversation>>,
        tokens: Tokens,
    ) -> Self {
        Self {
            index,
            message,
            report_expanded,
            details_expanded,
            language,
            events,
            tokens,
        }
    }
}

impl RenderOnce for ToolCard {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let tokens = self.tokens;
        let message_id = self.message.id.clone();
        // A code-location keyed handle is shared by every ToolCard because they
        // all render from this function. Key it by the transcript item instead,
        // so each expanded payload keeps an independent offset and thumb.
        let raw_scroll = window
            .use_keyed_state(format!("raw-tool-scroll-{message_id}"), cx, |_, _| {
                ScrollHandle::new()
            })
            .read(cx)
            .clone();
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
        let has_output = self.message.tool_report.is_some() || self.message.tool_details.is_some();
        let summary = pi_whim_engine::protocol::tool_header_summary(
            &name,
            self.message.tool_details.as_deref(),
        );
        let tool_icon = icons::tool(&name);

        let title = if has_output {
            disclosure_button(
                format!("tool-report-{}", self.index),
                name.clone(),
                self.report_expanded,
                accent.hsla(),
                ConversationEvent::ToggleToolReport(message_id.clone()),
                self.events.clone(),
            )
        } else {
            div()
                .text_size(px(text::LABEL_SIZE))
                .text_color(accent.hsla())
                .child(name.clone())
                .into_any_element()
        };

        div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .p(px(8.0))
            .bg(opaque_over(tokens.surface_tint(), tokens))
            .border_1()
            .border_color(accent.alpha(0.25).hsla())
            .child(
                div()
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .child(
                        Icon::new(tool_icon)
                            .size(px(12.0))
                            .text_color(accent.hsla()),
                    )
                    .child(div().flex_none().child(title))
                    .when_some(summary, |this, summary| {
                        this.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_size(px(text::LABEL_SIZE))
                                .text_color(tokens.muted.hsla())
                                .child(summary),
                        )
                    }),
            )
            .when(self.report_expanded, |this| {
                this.when_some(self.message.tool_report, |this, report| {
                    this.child(
                        div()
                            .text_size(px(text::MONO_DETAIL_SIZE))
                            .text_color(tokens.muted.hsla())
                            .child(report),
                    )
                })
                .when_some(self.message.tool_details, |this, details| {
                    this.child(
                        div()
                            .flex()
                            .flex_col()
                            .items_start()
                            .gap(px(4.0))
                            .pt(px(2.0))
                            .child(disclosure_button(
                                format!("tool-details-{}", self.index),
                                translate("raw-tool-details", self.language),
                                self.details_expanded,
                                tokens.muted.hsla(),
                                ConversationEvent::ToggleToolDetails(message_id),
                                self.events,
                            ))
                            .when(self.details_expanded, |this| {
                                this.child(
                                    div()
                                        .id(("raw-tool-details", self.index))
                                        .relative()
                                        .w_full()
                                        .max_h(px(RAW_DETAILS_MAX_HEIGHT))
                                        .overflow_y_scroll()
                                        .overflow_x_hidden()
                                        .track_scroll(&raw_scroll)
                                        .pl(px(6.0))
                                        .pr(px(18.0))
                                        .py(px(6.0))
                                        .bg(opaque_over(tokens.panel_soft, tokens))
                                        .font_family(font::MONO)
                                        .text_size(px(text::MONO_DETAIL_SIZE))
                                        .text_color(tokens.muted.hsla())
                                        .child(details)
                                        // Raw JSON is diagnostic content: its
                                        // bounded height is otherwise mistaken
                                        // for truncation. Keep the thumb visible
                                        // even when macOS hides ordinary system
                                        // scrollbars until scrolling begins.
                                        .child(
                                            Scrollbar::vertical(&raw_scroll)
                                                .id(("raw-tool-scrollbar", self.index))
                                                .scrollbar_show(ScrollbarShow::Always),
                                        ),
                                )
                            }),
                    )
                })
            })
    }
}
