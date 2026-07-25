//! Session status indicator.

use gpui::{
    IntoElement, ParentElement, RenderOnce, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::Icon;
use pi_whim_core::SessionStatus;
use pi_whim_theme::{Tokens, radius, text};

use crate::{icons, theme::IntoHsla};

/// A coloured dot and a label naming what the session is doing.
#[derive(IntoElement)]
pub struct StatusPill {
    status: SessionStatus,
    tokens: Tokens,
}

impl StatusPill {
    pub fn new(status: SessionStatus, tokens: Tokens) -> Self {
        Self { status, tokens }
    }

    /// The dot colour for a status.
    ///
    /// Failure reads as an error, work in progress as the accent, and an idle
    /// session as muted — so a glance at the colour alone tells you whether
    /// anything needs attention.
    fn dot(&self) -> pi_whim_theme::Rgba {
        match self.status {
            SessionStatus::Failed(_) => self.tokens.error,
            SessionStatus::Streaming | SessionStatus::Compacting => self.tokens.accent,
            SessionStatus::Starting => self.tokens.warning,
            SessionStatus::Ready => self.tokens.success,
            SessionStatus::Offline => self.tokens.muted,
        }
    }
}

/// The label for a status, in the app's mono voice.
pub fn status_label(status: &SessionStatus) -> &'static str {
    match status {
        SessionStatus::Offline => "offline",
        SessionStatus::Starting => "starting",
        SessionStatus::Ready => "ready",
        SessionStatus::Streaming => "streaming",
        SessionStatus::Compacting => "compacting",
        SessionStatus::Failed(_) => "failed",
    }
}

impl RenderOnce for StatusPill {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let tokens = self.tokens;
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(8.0))
            .py(px(3.0))
            .bg(tokens.accent_surface_subtle().hsla())
            .border_1()
            .border_color(tokens.accent_border_muted().hsla())
            .child(
                // The one round thing: pi.dev's own live indicator is a dot.
                div()
                    .w(px(6.0))
                    .h(px(6.0))
                    .rounded(px(radius::DOT))
                    .bg(self.dot().hsla()),
            )
            // States worth noticing carry a glyph as well as the dot; idle ones
            // do not, so the pill stays quiet when nothing needs attention.
            .when_some(icons::status(&self.status), |this, icon| {
                this.child(Icon::new(icon).size(px(11.0)).text_color(self.dot().hsla()))
            })
            .child(
                div()
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .child(status_label(&self.status)),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_status_has_a_label() {
        // A blank pill would read as a rendering bug, so the match is total.
        for status in [
            SessionStatus::Offline,
            SessionStatus::Starting,
            SessionStatus::Ready,
            SessionStatus::Streaming,
            SessionStatus::Compacting,
            SessionStatus::Failed("boom".into()),
        ] {
            assert!(!status_label(&status).is_empty());
        }
    }

    #[test]
    fn failure_and_progress_are_visually_distinct() {
        let tokens = Tokens::light();
        let failed = StatusPill::new(SessionStatus::Failed("boom".into()), tokens).dot();
        let streaming = StatusPill::new(SessionStatus::Streaming, tokens).dot();
        let idle = StatusPill::new(SessionStatus::Offline, tokens).dot();

        assert_ne!(failed, streaming);
        assert_ne!(streaming, idle);
        assert_eq!(failed, tokens.error);
    }

    #[test]
    fn compacting_reads_as_work_in_progress() {
        // Compaction is the agent doing something, not an idle state.
        let tokens = Tokens::light();
        assert_eq!(
            StatusPill::new(SessionStatus::Compacting, tokens).dot(),
            StatusPill::new(SessionStatus::Streaming, tokens).dot()
        );
    }
}
