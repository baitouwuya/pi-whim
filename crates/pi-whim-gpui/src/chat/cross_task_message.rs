//! A durable message injected by another running task.

use gpui::{
    InteractiveElement, IntoElement, ParentElement, RenderOnce, StatefulInteractiveElement, Styled,
    Window, div, px,
};
use gpui_component::{Icon, IconName, text::TextView, tooltip::Tooltip};
use pi_whim_core::Language;
use pi_whim_theme::{Tokens, layout, text};

use crate::{chat::message_card::opaque_over, theme::IntoHsla};

#[derive(IntoElement)]
pub struct CrossTaskMessage {
    index: usize,
    sender_session: String,
    content: String,
    language: Language,
    tokens: Tokens,
}

impl CrossTaskMessage {
    pub fn new(
        index: usize,
        sender_session: String,
        content: String,
        language: Language,
        tokens: Tokens,
    ) -> Self {
        Self {
            index,
            sender_session,
            content,
            language,
            tokens,
        }
    }
}

impl RenderOnce for CrossTaskMessage {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let tokens = self.tokens;
        let sender_session = self.sender_session;
        let index = self.index;
        let content = self.content;
        let source = match self.language {
            Language::SimplifiedChinese => "由另一 agent 发起的任务",
            Language::English => "Task started by another agent",
        };

        div()
            .w_full()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(6.0))
            .child(
                div()
                    .id(("cross-task-source", index))
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap(px(5.0))
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .tooltip(move |window, cx| {
                        Tooltip::new(sender_session.clone()).build(window, cx)
                    })
                    .child(Icon::new(IconName::Bot).size(px(12.0)))
                    .child(source),
            )
            .child(
                div()
                    .max_w(px(layout::CHAT_CONTENT_WIDTH))
                    .flex_none()
                    .p(px(11.0))
                    .bg(opaque_over(tokens.surface_tint(), tokens))
                    .border_1()
                    .border_color(tokens.line.hsla())
                    .text_color(tokens.text.hsla())
                    .child(
                        TextView::markdown(("cross-task-message", index), content).selectable(true),
                    ),
            )
    }
}
