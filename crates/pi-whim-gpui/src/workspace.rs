//! Root view.
//!
//! Owns the domain state and the resolved theme, and arranges the chrome around
//! the space the conversation and sidebar will fill. Those two, along with the
//! settings page, land as their own modules.

use gpui::{
    Context, IntoElement, ParentElement, Render, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_core::{Action, AppState, SessionStatus};
use pi_whim_engine::state::EngineState;
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, layout, text};

use crate::{
    chrome::{Banner, Severity, StatusStrip, TopBar},
    theme::IntoHsla,
};

/// The application shell.
pub struct Workspace {
    preference: ThemePreference,
    tokens: Tokens,
    engine: EngineState,
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
            engine: EngineState::new(),
        }
    }

    /// Read-only domain state, for rendering.
    pub fn state(&self) -> &AppState {
        self.engine.get()
    }

    /// Apply `action` through the reducer.
    ///
    /// View-local follow-ups arrive as a [`ViewEffect`]; the shell currently
    /// caches nothing per message, so there is nothing to invalidate yet.
    pub fn apply(&mut self, action: Action, cx: &mut Context<Self>) {
        let _effect = self.engine.apply(action);
        cx.notify();
    }

    /// The banner to show above the conversation, if any.
    ///
    /// Failure takes precedence: if the session has broken, that matters more
    /// than reporting that it is busy.
    fn banner(&self) -> Option<Banner> {
        match &self.engine.get().session_status {
            SessionStatus::Failed(error) => Some(Banner::error(error.clone(), self.tokens)),
            SessionStatus::Compacting => Some(Banner::progress(
                "Compacting the conversation…",
                self.tokens,
            )),
            _ => None,
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        let state = self.engine.get();
        let status = state.session_status.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(tokens.bg_canvas.hsla())
            .text_color(tokens.text.hsla())
            .text_size(px(text::BODY_SIZE))
            .child(
                TopBar::new(status.clone(), tokens.mode, tokens)
                    .on_toggle_theme(cx.listener(|workspace, _, window, cx| {
                        workspace.toggle_theme(window, cx);
                    }))
                    .on_open_settings(cx.listener(|_, _, _, _| {
                        // The settings page lands in a later change.
                    })),
            )
            .when_some(self.banner(), |this, banner| this.child(banner))
            // The conversation and sidebar fill whatever the chrome leaves.
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h(px(0.0))
                    .child(
                        div()
                            .w(px(layout::SIDEBAR_WIDTH))
                            .h_full()
                            .bg(tokens.panel_soft.hsla())
                            .border_r_1()
                            .border_color(tokens.line.hsla()),
                    )
                    .child(div().flex_1().h_full()),
            )
            .child(StatusStrip::from_state(self.engine.get(), tokens))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A workspace built without a window, for testing the parts that do not
    /// need one. `Workspace::new` reads the component theme from the app
    /// context, which a bare unit test has no access to.
    fn workspace(mode: ThemeMode) -> Workspace {
        Workspace {
            preference: ThemePreference::Fixed(mode),
            tokens: Tokens::new(mode),
            engine: EngineState::new(),
        }
    }

    #[test]
    fn an_idle_session_shows_no_banner() {
        let mut shell = workspace(ThemeMode::Light);
        assert!(shell.banner().is_none());

        shell
            .engine
            .apply(Action::SetSessionStatus(SessionStatus::Ready));
        assert!(shell.banner().is_none());
    }

    #[test]
    fn compaction_shows_a_progress_banner() {
        let mut shell = workspace(ThemeMode::Light);
        shell
            .engine
            .apply(Action::SetSessionStatus(SessionStatus::Compacting));

        let banner = shell.banner().expect("a banner while compacting");
        assert_eq!(banner.severity(), Severity::Progress);
    }

    #[test]
    fn failure_takes_precedence_over_progress() {
        // A broken session matters more than reporting that it is busy.
        let mut shell = workspace(ThemeMode::Light);
        shell
            .engine
            .apply(Action::SetSessionStatus(SessionStatus::Compacting));
        shell
            .engine
            .apply(Action::SetSessionStatus(SessionStatus::Failed(
                "boom".into(),
            )));

        let banner = shell.banner().expect("a banner after failure");
        assert_eq!(banner.severity(), Severity::Error);
    }

    #[test]
    fn the_shell_starts_in_the_requested_mode() {
        assert_eq!(workspace(ThemeMode::Dark).mode(), ThemeMode::Dark);
        assert_eq!(workspace(ThemeMode::Light).mode(), ThemeMode::Light);
    }
}
