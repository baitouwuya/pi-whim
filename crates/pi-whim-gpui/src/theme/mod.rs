//! Applies Pi-Whim's pi.dev tokens on top of gpui-component's theme.
//!
//! gpui-component owns 140 `Hsla` slots that its widgets read from. Rather than
//! restyle each widget at the call site, we overwrite those slots once with the
//! matching pi.dev value, so stock components come out looking like pi.dev with
//! no per-widget work.
//!
//! Slots pi.dev has no opinion about keep gpui-component's own value.

mod convert;

pub use convert::{IntoHsla, to_gpui, to_hsla};

use gpui::{App, Window, px};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, font, radius};

/// Install the pi.dev palette and typography, resolving `preference` against
/// the current system appearance.
pub fn install(preference: ThemePreference, window: Option<&mut Window>, cx: &mut App) {
    let mode = match preference {
        // `sync_system_appearance` reads the window (or app) appearance and
        // calls `Theme::change` itself, so let it drive and then read back
        // which mode it settled on.
        ThemePreference::System => {
            ComponentTheme::sync_system_appearance(window, cx);
            if ComponentTheme::global(cx).is_dark() {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            }
        }
        ThemePreference::Fixed(mode) => {
            ComponentTheme::change(component_mode(mode), window, cx);
            mode
        }
    };

    ComponentTheme::sync_scrollbar_appearance(cx);
    apply_tokens(Tokens::new(mode), cx);
}

/// Re-apply after the system appearance changed.
///
/// gpui-component's `Theme::change` resets its slots from its own config, so
/// the pi.dev overrides have to be laid down again afterwards.
pub fn reapply(mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    ComponentTheme::change(component_mode(mode), window, cx);
    apply_tokens(Tokens::new(mode), cx);
}

fn component_mode(mode: ThemeMode) -> ComponentMode {
    match mode {
        ThemeMode::Light => ComponentMode::Light,
        ThemeMode::Dark => ComponentMode::Dark,
    }
}

/// Overwrite gpui-component's slots with the pi.dev equivalents.
fn apply_tokens(tokens: Tokens, cx: &mut App) {
    let theme = ComponentTheme::global_mut(cx);

    theme.font_family = font::MONO[0].into();
    theme.mono_font_family = font::MONO[0].into();

    // gpui-component's widgets read these, so squaring them here is what makes
    // stock buttons, inputs, and dialogs match pi.dev instead of each call site
    // having to override a radius.
    theme.radius = px(radius::NONE);
    theme.radius_lg = px(radius::NONE);

    let colors = &mut theme.colors;

    // Surfaces.
    colors.background = tokens.bg_canvas.hsla();
    colors.border = tokens.line.hsla();
    colors.popover = tokens.panel.hsla();
    colors.popover_foreground = tokens.text.hsla();
    colors.title_bar = tokens.panel.hsla();
    colors.title_bar_border = tokens.line.hsla();
    colors.status_bar = tokens.panel_soft.hsla();
    colors.status_bar_border = tokens.line.hsla();
    colors.window_border = tokens.line.hsla();
    colors.overlay = tokens.bg_deep.alpha(0.66).hsla();

    // Text.
    colors.foreground = tokens.text.hsla();
    colors.muted = tokens.panel_soft.hsla();
    colors.muted_foreground = tokens.muted.hsla();

    // Accent. Every one of these derives from the single accent slot.
    colors.accent = tokens.accent_surface_soft().hsla();
    colors.accent_foreground = tokens.text.hsla();
    colors.primary = tokens.accent.hsla();
    colors.primary_hover = tokens.accent.alpha(0.9).hsla();
    colors.primary_active = tokens.accent.alpha(0.8).hsla();
    colors.primary_foreground = tokens.panel_base.hsla();
    colors.ring = tokens.focus_ring().hsla();
    colors.selection = tokens.selection().hsla();
    colors.caret = tokens.accent.hsla();
    colors.link = tokens.accent.hsla();
    colors.link_hover = tokens.accent_border_hover().hsla();
    colors.link_active = tokens.accent_border_active().hsla();

    // Controls.
    colors.input = tokens.line.hsla();
    colors.button = tokens.control_background().hsla();
    colors.button_hover = tokens.control_background_hover().hsla();
    colors.button_foreground = tokens.text.hsla();
    colors.secondary = tokens.control_background().hsla();
    colors.secondary_hover = tokens.control_background_hover().hsla();
    colors.secondary_active = tokens.surface_tint().hsla();
    colors.secondary_foreground = tokens.text.hsla();
    colors.switch = tokens.line.hsla();
    colors.switch_thumb = tokens.panel_base.hsla();
    colors.slider_bar = tokens.accent.hsla();
    colors.slider_thumb = tokens.panel_base.hsla();
    colors.progress_bar = tokens.accent.hsla();
    colors.skeleton = tokens.surface_tint().hsla();

    // Sidebar.
    colors.sidebar = tokens.panel_soft.hsla();
    colors.sidebar_foreground = tokens.text.hsla();
    colors.sidebar_border = tokens.line.hsla();
    colors.sidebar_accent = tokens.accent_surface_soft().hsla();
    colors.sidebar_accent_foreground = tokens.text.hsla();
    colors.sidebar_primary = tokens.accent.hsla();
    colors.sidebar_primary_foreground = tokens.panel_base.hsla();

    // Lists and tables.
    colors.list = tokens.panel.hsla();
    colors.list_hover = tokens.control_background_hover().hsla();
    colors.list_active = tokens.accent_surface_strong().hsla();
    colors.list_active_border = tokens.accent_border_muted().hsla();
    colors.list_even = tokens.surface_tint().hsla();
    colors.list_head = tokens.table_heading_background().hsla();
    colors.table = tokens.panel.hsla();
    colors.table_hover = tokens.control_background_hover().hsla();
    colors.table_active = tokens.accent_surface_strong().hsla();
    colors.table_active_border = tokens.accent_border_muted().hsla();
    colors.table_even = tokens.surface_tint().hsla();
    colors.table_head = tokens.table_heading_background().hsla();
    colors.table_head_foreground = tokens.muted.hsla();
    colors.table_row_border = tokens.line.hsla();

    // Tabs.
    colors.tab = tokens.panel_soft.hsla();
    colors.tab_bar = tokens.panel_soft.hsla();
    colors.tab_active = tokens.panel.hsla();
    colors.tab_foreground = tokens.muted.hsla();
    colors.tab_active_foreground = tokens.text.hsla();

    // Scrollbars keep thread blue in both themes.
    colors.scrollbar = tokens.panel_soft.alpha(0.0).hsla();
    colors.scrollbar_thumb = tokens.scrollbar_thumb().hsla();
    colors.scrollbar_thumb_hover = tokens.scrollbar_thumb_hover().hsla();

    // States.
    colors.success = tokens.success.hsla();
    colors.success_foreground = tokens.panel_base.hsla();
    colors.warning = tokens.warning.hsla();
    colors.warning_foreground = tokens.panel_base.hsla();
    colors.danger = tokens.error.hsla();
    colors.danger_foreground = tokens.panel_base.hsla();
    colors.info = tokens.accent.hsla();
    colors.info_foreground = tokens.panel_base.hsla();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_resolution_matches_the_token_crate() {
        // `install` derives its mode from ThemePreference, so the mapping to
        // gpui-component's enum has to stay total.
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let component = component_mode(mode);
            assert_eq!(component.is_dark(), mode.is_dark());
        }
    }
}
