//! Banners for conditions the user needs to see without hunting for them.
//!
//! The egui build surfaced failures as a small "ERROR" label that was easy to
//! miss entirely; these take a full row.

use gpui::{
    App, ClickEvent, IntoElement, ParentElement, RenderOnce, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
};
use pi_whim_theme::{Rgba, Tokens, text};

use crate::theme::IntoHsla;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

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
    /// A second, quieter line under the message.
    ///
    /// Used where the headline names the condition but not what it means for the
    /// reader — "compacting context" says nothing about the conversation still
    /// being there afterwards.
    detail: Option<String>,
    copy_label: Option<String>,
    dismiss_label: Option<String>,
    on_copy: Option<ClickHandler>,
    on_dismiss: Option<ClickHandler>,
    tokens: Tokens,
}

impl Banner {
    pub fn error(message: impl Into<String>, tokens: Tokens) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            detail: None,
            copy_label: None,
            dismiss_label: None,
            on_copy: None,
            on_dismiss: None,
            tokens,
        }
    }

    pub fn progress(message: impl Into<String>, tokens: Tokens) -> Self {
        Self {
            severity: Severity::Progress,
            message: message.into(),
            detail: None,
            copy_label: None,
            dismiss_label: None,
            on_copy: None,
            on_dismiss: None,
            tokens,
        }
    }

    /// Add the quieter second line.
    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn on_copy(
        mut self,
        label: impl Into<String>,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.copy_label = Some(label.into());
        self.on_copy = Some(Box::new(handler));
        self
    }

    pub fn on_dismiss(
        mut self,
        label: impl Into<String>,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.dismiss_label = Some(label.into());
        self.on_dismiss = Some(Box::new(handler));
        self
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
        let copy_label = self.copy_label;
        let dismiss_label = self.dismiss_label;
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
                    .flex()
                    .flex_col()
                    .gap(px(1.0))
                    .child(
                        div()
                            .text_size(px(text::DETAIL_SIZE))
                            .text_color(self.tokens.text.hsla())
                            .child(self.message),
                    )
                    .when_some(self.detail, |this, detail| {
                        this.child(
                            div()
                                .text_size(px(text::LABEL_SIZE))
                                .text_color(self.tokens.muted.hsla())
                                .child(detail),
                        )
                    }),
            )
            .when_some(self.on_copy, |this, handler| {
                this.child(
                    Button::new("copy-banner-error")
                        .ghost()
                        .xsmall()
                        .icon(crate::icons::copy())
                        .tooltip(copy_label.unwrap_or_default())
                        .on_click(handler),
                )
            })
            .when_some(self.on_dismiss, |this, handler| {
                this.child(
                    Button::new("dismiss-banner-error")
                        .ghost()
                        .xsmall()
                        .icon(crate::icons::close())
                        .tooltip(dismiss_label.unwrap_or_default())
                        .on_click(handler),
                )
            })
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
