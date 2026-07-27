//! Title row: what is running, and the controls that are always reachable.

use gpui::{
    App, ClickEvent, IntoElement, ParentElement, RenderOnce, Styled, Window, div,
    prelude::FluentBuilder, px, rems,
};
use gpui_component::button::{Button, ButtonVariants};
use pi_whim_core::{Language, SessionMetrics, SessionStatus, strings::text as translate};
use pi_whim_theme::{ThemeMode, Tokens, text};

use crate::{
    chrome::{SessionMeter, StatusPill},
    icons,
    theme::IntoHsla,
};

/// A click handler a caller can hand to a chrome control.
type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// The window's title row.
#[derive(IntoElement)]
pub struct TopBar {
    status: SessionStatus,
    mode: ThemeMode,
    /// Passed in rather than held: this is rebuilt on every render, so there is no
    /// state here to keep in step with the snapshot.
    language: Language,
    tokens: Tokens,
    /// What the visible session has cost, once it has reported anything.
    meter: Option<SessionMeter>,
    on_toggle_theme: Option<ClickHandler>,
    on_open_settings: Option<ClickHandler>,
}

impl TopBar {
    pub fn new(status: SessionStatus, mode: ThemeMode, language: Language, tokens: Tokens) -> Self {
        Self {
            status,
            mode,
            language,
            tokens,
            meter: None,
            on_toggle_theme: None,
            on_open_settings: None,
        }
    }

    /// Show the session's cost and token counts beside the status.
    ///
    /// These used to occupy a strip along the bottom of the window, which spent a
    /// whole row of chrome and a border on four short figures.
    pub fn metrics(mut self, metrics: Option<&SessionMetrics>) -> Self {
        self.meter = SessionMeter::from_metrics(metrics, self.tokens);
        self
    }

    pub fn on_toggle_theme(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_toggle_theme = Some(Box::new(handler));
        self
    }

    pub fn on_open_settings(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_open_settings = Some(Box::new(handler));
        self
    }

    /// Tooltip for the theme toggle, naming where it goes rather than where it is.
    fn theme_toggle_tooltip(&self) -> &'static str {
        let key = match self.mode {
            ThemeMode::Light => "switch-to-dark",
            ThemeMode::Dark => "switch-to-light",
        };
        translate(key, self.language)
    }
}

impl RenderOnce for TopBar {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tokens = self.tokens;
        // Icons rather than words: these controls are always present, and their
        // glyphs are recognizable enough that labels would only add width.
        let mut theme_button = Button::new("toggle-theme")
            .ghost()
            .icon(icons::theme_toggle(self.mode.is_dark()))
            .tooltip(self.theme_toggle_tooltip());
        if let Some(handler) = self.on_toggle_theme {
            theme_button = theme_button.on_click(handler);
        }
        let mut settings_button = Button::new("open-settings")
            .ghost()
            .icon(icons::settings())
            .tooltip(translate("settings", self.language));
        if let Some(handler) = self.on_open_settings {
            settings_button = settings_button.on_click(handler);
        }

        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .w_full()
            .px(px(14.0))
            .py(px(8.0))
            .bg(tokens.panel.hsla())
            .border_b_1()
            .border_color(tokens.line.hsla())
            .child(
                div()
                    .font_weight(gpui::FontWeight(text::BODY_WEIGHT as f32))
                    .text_size(rems(1.0))
                    .text_color(tokens.text.hsla())
                    .child("Pi-Whim"),
            )
            .child(StatusPill::new(self.status, self.language, tokens))
            // Push the controls to the trailing edge.
            .child(div().flex_1())
            .when_some(self.meter, |this, meter| this.child(meter))
            .child(theme_button)
            .child(settings_button)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_theme_toggle_tooltip_names_its_destination() {
        // The tooltip says what you get, not what you have.
        let light = TopBar::new(
            SessionStatus::Ready,
            ThemeMode::Light,
            Language::English,
            Tokens::light(),
        );
        assert_eq!(light.theme_toggle_tooltip(), "Switch to dark");

        let dark = TopBar::new(
            SessionStatus::Ready,
            ThemeMode::Dark,
            Language::English,
            Tokens::dark(),
        );
        assert_eq!(dark.theme_toggle_tooltip(), "Switch to light");

        // And it follows the language, like every other label.
        let chinese = TopBar::new(
            SessionStatus::Ready,
            ThemeMode::Light,
            Language::SimplifiedChinese,
            Tokens::light(),
        );
        assert_eq!(chinese.theme_toggle_tooltip(), "切换到深色");
    }

    #[test]
    fn handlers_are_optional() {
        // The preview harness renders the bar without wiring anything up.
        let bar = TopBar::new(
            SessionStatus::Offline,
            ThemeMode::Light,
            Language::English,
            Tokens::light(),
        );
        assert!(bar.on_toggle_theme.is_none());
        assert!(bar.on_open_settings.is_none());

        let wired = TopBar::new(
            SessionStatus::Offline,
            ThemeMode::Light,
            Language::English,
            Tokens::light(),
        )
        .on_toggle_theme(|_, _, _| {})
        .on_open_settings(|_, _, _| {});
        assert!(wired.on_toggle_theme.is_some());
        assert!(wired.on_open_settings.is_some());
    }
}
