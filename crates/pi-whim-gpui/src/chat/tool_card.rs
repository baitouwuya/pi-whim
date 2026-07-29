//! A tool invocation with two independent levels of disclosure.

use gpui::{
    App, Entity, IntoElement, ParentElement, RenderOnce, ScrollHandle, Styled, Window, div,
    prelude::FluentBuilder, px,
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
    elements::isolated_manual_vertical_scroll_area,
    icons,
    theme::IntoHsla,
};

const TOOL_REPORT_MAX_HEIGHT: f32 = 320.0;
const RAW_DETAILS_MAX_HEIGHT: f32 = 280.0;

fn header_summary(message: &ConversationItem, name: &str) -> Option<String> {
    pi_whim_engine::protocol::tool_header_summary(name, message.tool_details.as_deref()).or_else(
        || {
            let summary = pi_whim_engine::protocol::compact_tool_text(&message.full_text);
            (!summary.is_empty()).then_some(summary)
        },
    )
}

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
        // Code-location keyed handles would be shared by every ToolCard because
        // they all render from this function. Key both disclosure levels by the
        // transcript item so each payload keeps its own offset and thumb.
        let report_scroll = window
            .use_keyed_state(format!("tool-report-scroll-{message_id}"), cx, |_, _| {
                ScrollHandle::new()
            })
            .read(cx)
            .clone();
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
        let summary = header_summary(&self.message, &name);
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
                        isolated_manual_vertical_scroll_area(
                            ("tool-report", self.index),
                            &report_scroll,
                        )
                        .max_h(px(TOOL_REPORT_MAX_HEIGHT))
                        .pr(px(18.0))
                        .text_size(px(text::MONO_DETAIL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(report)
                        .child(
                            Scrollbar::vertical(&report_scroll)
                                .id(("tool-report-scrollbar", self.index))
                                .scrollbar_show(ScrollbarShow::Always),
                        ),
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
                                    isolated_manual_vertical_scroll_area(
                                        ("raw-tool-details", self.index),
                                        &raw_scroll,
                                    )
                                    .max_h(px(RAW_DETAILS_MAX_HEIGHT))
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

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::ConversationRole;

    fn tool_message(full_text: &str, details: Option<&str>) -> ConversationItem {
        ConversationItem {
            id: "tool-1".into(),
            role: ConversationRole::Tool,
            full_text: full_text.into(),
            streaming: false,
            tool_name: Some("compact".into()),
            tool_report: None,
            tool_details: details.map(str::to_owned),
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn a_tool_without_argument_metadata_falls_back_to_its_result_summary() {
        let message = tool_message("Compacted context · 90,000 → 12,000 tokens", None);

        assert_eq!(
            header_summary(&message, "compact").as_deref(),
            Some("Compacted context · 90,000 → 12,000 tokens")
        );
    }

    #[test]
    fn the_header_fallback_is_single_line_and_bounded() {
        let message = tool_message(&format!("first\n{}", "word ".repeat(40)), None);
        let summary = header_summary(&message, "compact").expect("a summary");

        assert!(!summary.contains('\n'));
        assert!(summary.ends_with('…'));
    }

    const _: () = {
        assert!(TOOL_REPORT_MAX_HEIGHT > 0.0);
        assert!(RAW_DETAILS_MAX_HEIGHT > 0.0);
    };
}
