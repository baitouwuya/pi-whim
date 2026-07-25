//! Root view.
//!
//! Currently a placeholder that proves the theme and fonts resolve. The chrome,
//! chat, and settings views land on top of this as separate modules.

use gpui::{Context, IntoElement, ParentElement, Render, Styled, Window, div, px};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, layout, text};

use crate::theme::IntoHsla;

/// The application shell.
pub struct Workspace {
    preference: ThemePreference,
    tokens: Tokens,
}

impl Workspace {
    pub fn new(preference: ThemePreference, cx: &mut Context<Self>) -> Self {
        let mode = if ComponentTheme::global(cx).is_dark() {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        Self {
            preference,
            tokens: Tokens::new(mode),
        }
    }

    pub fn mode(&self) -> ThemeMode {
        self.tokens.mode
    }

    /// Switch to the other theme, pinning the preference so the choice sticks
    /// rather than being overwritten on the next appearance change.
    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.tokens.mode.toggled();
        self.preference = ThemePreference::Fixed(next);
        self.tokens = Tokens::new(next);
        crate::theme::reapply(next, Some(window), cx);
        cx.notify();
    }

    /// Re-resolve after the system appearance changed. A pinned preference
    /// ignores it.
    pub fn system_appearance_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.preference, ThemePreference::System) {
            return;
        }
        // gpui-component already maps the platform appearance onto its own mode
        // enum, including the Vibrant variants, so defer to that.
        let system = match ComponentMode::from(window.appearance()) {
            ComponentMode::Dark => ThemeMode::Dark,
            ComponentMode::Light => ThemeMode::Light,
        };
        if system == self.tokens.mode {
            return;
        }
        self.tokens = Tokens::new(system);
        crate::theme::reapply(system, Some(window), cx);
        cx.notify();
    }
}

impl Render for Workspace {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(tokens.bg_canvas.hsla())
            .text_color(tokens.text.hsla())
            .text_size(px(text::BODY_SIZE))
            .child(
                div()
                    .w(px(layout::SIDEBAR_WIDTH))
                    .h_full()
                    .bg(tokens.panel_soft.hsla())
                    .border_r_1()
                    .border_color(tokens.line.hsla()),
            )
    }
}
