//! gpui presentation layer for Pi-Whim.
//!
//! Deliberately isolated from persistence and Pi RPC, the same way the egui
//! crate was: this crate depends on `pi-whim-core` for domain types and
//! `pi-whim-theme` for tokens, and nothing else from the workspace.
//!
//! Views are separate types implementing [`gpui::Render`] rather than methods
//! on one struct, which is what keeps this crate from growing into the single
//! 3.5k-line file the egui build ended up with.

pub mod fonts;
pub mod theme;

mod workspace;

pub use workspace::Workspace;

use gpui::{App, Bounds, WindowBounds, WindowOptions, px, size};
use pi_whim_theme::ThemePreference;

/// Default window size, carried over from the egui build.
pub const DEFAULT_WINDOW_SIZE: (f32, f32) = (1360.0, 860.0);
/// Below this the sidebar and composer stop being usable.
pub const MIN_WINDOW_SIZE: (f32, f32) = (900.0, 620.0);

/// Initialize the component library, fonts, and theme.
///
/// Call once after the application starts, before opening a window.
pub fn init(preference: ThemePreference, cx: &mut App) -> anyhow::Result<()> {
    gpui_component::init(cx);
    fonts::install(cx)?;
    theme::install(preference, None, cx);
    Ok(())
}

/// Window options for the main window.
pub fn window_options(cx: &mut App) -> WindowOptions {
    let (width, height) = DEFAULT_WINDOW_SIZE;
    let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_min_size: Some(size(px(MIN_WINDOW_SIZE.0), px(MIN_WINDOW_SIZE.1))),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some("Pi-Whim".into()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimum_window_fits_the_fixed_chrome() {
        use pi_whim_theme::layout;

        // The sidebar is fixed-width and the chat column has a minimum useful
        // measure; the floor has to leave room for both.
        assert!(MIN_WINDOW_SIZE.0 > layout::SIDEBAR_WIDTH);
        assert!(MIN_WINDOW_SIZE.0 < DEFAULT_WINDOW_SIZE.0);
        assert!(MIN_WINDOW_SIZE.1 < DEFAULT_WINDOW_SIZE.1);
    }

    #[test]
    fn default_window_can_show_the_full_chat_measure() {
        use pi_whim_theme::layout;

        assert!(DEFAULT_WINDOW_SIZE.0 >= layout::SIDEBAR_WIDTH + layout::CHAT_CONTENT_WIDTH);
    }
}
