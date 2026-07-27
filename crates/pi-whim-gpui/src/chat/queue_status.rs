//! Pending steering and follow-up prompts shown beside the active draft.

use gpui::{
    IntoElement, ParentElement, RenderOnce, Styled, Window, div, prelude::FluentBuilder, px,
};
use pi_whim_core::{AppState, Language, strings::text as translate};
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// Compact queue chips for the prompt area.
#[derive(IntoElement)]
pub struct QueueStatus {
    steering: usize,
    follow_ups: usize,
    language: Language,
    tokens: Tokens,
}

impl QueueStatus {
    /// Return nothing when there is no queue, so the prompt keeps its spacing.
    pub fn from_state(state: &AppState, tokens: Tokens) -> Option<Self> {
        let steering = state.pending_steering.len();
        let follow_ups = state.pending_follow_up.len();
        (steering > 0 || follow_ups > 0).then_some(Self {
            steering,
            follow_ups,
            language: state.language,
            tokens,
        })
    }
}

impl RenderOnce for QueueStatus {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let tokens = self.tokens;
        div()
            .flex()
            .flex_wrap()
            .gap(px(5.0))
            .when(self.steering > 0, |this| {
                this.child(
                    div()
                        .px(px(6.0))
                        .py(px(2.0))
                        .bg(tokens.accent_surface_soft().hsla())
                        .border_1()
                        .border_color(tokens.accent_border_muted().hsla())
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(format!(
                            "{} {}",
                            translate("queued", self.language),
                            self.steering
                        )),
                )
            })
            .when(self.follow_ups > 0, |this| {
                this.child(
                    div()
                        .px(px(6.0))
                        .py(px(2.0))
                        .bg(tokens.surface_tint().hsla())
                        .border_1()
                        .border_color(tokens.line.hsla())
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(format!(
                            "{} {}",
                            translate("follow-ups", self.language),
                            self.follow_ups
                        )),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_queue_takes_no_prompt_space() {
        assert!(QueueStatus::from_state(&AppState::default(), Tokens::light()).is_none());
    }

    #[test]
    fn both_queue_counts_are_retained() {
        let state = AppState {
            pending_steering: vec!["one".into(), "two".into()],
            pending_follow_up: vec!["later".into()],
            ..Default::default()
        };
        let status = QueueStatus::from_state(&state, Tokens::light()).unwrap();

        assert_eq!(status.steering, 2);
        assert_eq!(status.follow_ups, 1);
    }
}
