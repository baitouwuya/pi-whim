//! Pending steering and follow-up prompts shown beside the active draft.

use gpui::{
    App, InteractiveElement, IntoElement, MouseButton, MouseDownEvent, ParentElement, RenderOnce,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use pi_whim_core::{AppState, Language, strings::text as translate};
use pi_whim_theme::{Tokens, text};

use crate::{chat::message_card::opaque_over, theme::IntoHsla};

/// The clear chip's click handler, named so the field stays readable.
type OnClear = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

/// Compact queue chips for the prompt area.
#[derive(IntoElement)]
pub struct QueueStatus {
    steering: usize,
    follow_ups: usize,
    language: Language,
    tokens: Tokens,
    on_clear: Option<OnClear>,
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
            on_clear: None,
        })
    }

    /// What the trailing chip does. Pi clears both queues together, so the
    /// affordance is one button rather than one per kind.
    pub fn on_clear(
        mut self,
        listener: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_clear = Some(Box::new(listener));
        self
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
                        // Floating over the graph paper now: composite the fill
                        // or the grid runs through the chip.
                        .bg(opaque_over(tokens.accent_surface_soft(), tokens))
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
                        .bg(opaque_over(tokens.surface_tint(), tokens))
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
            .when_some(self.on_clear, |this, on_clear| {
                this.child(
                    div()
                        .id("queue-clear")
                        .px(px(6.0))
                        .py(px(2.0))
                        .bg(opaque_over(tokens.surface_tint(), tokens))
                        .border_1()
                        .border_color(tokens.line.hsla())
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .cursor_pointer()
                        .hover(|chip| {
                            chip.border_color(tokens.line_strong.hsla())
                                .text_color(tokens.text.hsla())
                        })
                        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                            on_clear(event, window, cx)
                        })
                        .child(translate("clear-queue", self.language)),
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
