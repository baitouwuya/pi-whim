//! Banners for conditions the user needs to see without hunting for them.
//!
//! The egui build surfaced failures as a small "ERROR" label that was easy to
//! miss entirely; these take a full row.

use gpui::{IntoElement, ParentElement, RenderOnce, Styled, Window, div, px};
use pi_whim_theme::{Rgba, Tokens, text};

use crate::theme::IntoHsla;

/// How prominent a banner should be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// Something is wrong and the user likely has to act.
    Error,
    /// Something is happening and will resolve on its own.
    Progress,
}

/// A full-width strip explaining the current condition.
#[derive(IntoElement)]
pub struct Banner {
    severity: Severity,
    message: String,
    tokens: Tokens,
}

impl Banner {
    pub fn error(message: impl Into<String>, tokens: Tokens) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            tokens,
        }
    }

    pub fn progress(message: impl Into<String>, tokens: Tokens) -> Self {
        Self {
            severity: Severity::Progress,
            message: message.into(),
            tokens,
        }
    }

    /// How prominent this banner is.
    pub fn severity(&self) -> Severity {
        self.severity
    }

    /// The hue carrying the banner's meaning.
    fn accent(&self) -> Rgba {
        match self.severity {
            Severity::Error => self.tokens.error,
            Severity::Progress => self.tokens.accent,
        }
    }

    /// Progress banners are slimmer: they are informational, and a tall strip
    /// would shove the conversation down every time compaction ran.
    fn vertical_padding(&self) -> f32 {
        match self.severity {
            Severity::Error => 10.0,
            Severity::Progress => 6.0,
        }
    }
}

impl RenderOnce for Banner {
    fn render(self, _window: &mut Window, _cx: &mut gpui::App) -> impl IntoElement {
        let accent = self.accent();
        let padding = self.vertical_padding();
        div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .w_full()
            .px(px(16.0))
            .py(px(padding))
            .bg(accent.alpha(0.08).hsla())
            .border_b_1()
            .border_color(accent.alpha(0.25).hsla())
            .child(div().w(px(3.0)).h(px(14.0)).bg(accent.hsla()))
            .child(
                div()
                    .flex_1()
                    .text_size(px(text::DETAIL_SIZE))
                    .text_color(self.tokens.text.hsla())
                    .child(self.message),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_selects_the_accent() {
        let tokens = Tokens::light();
        assert_eq!(Banner::error("boom", tokens).accent(), tokens.error);
        assert_eq!(Banner::progress("working", tokens).accent(), tokens.accent);
    }

    #[test]
    fn progress_banners_are_slimmer_than_errors() {
        // Compaction runs often; a tall strip would shove the conversation
        // down every time.
        let tokens = Tokens::light();
        assert!(
            Banner::progress("working", tokens).vertical_padding()
                < Banner::error("boom", tokens).vertical_padding()
        );
    }

    #[test]
    fn both_severities_work_in_dark_mode() {
        let tokens = Tokens::dark();
        assert_eq!(Banner::error("boom", tokens).accent(), tokens.error);
        assert_ne!(
            Banner::error("boom", tokens).accent(),
            Banner::progress("working", tokens).accent()
        );
    }
}
