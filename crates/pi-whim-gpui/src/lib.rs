//! gpui presentation layer for Pi-Whim.
//!
//! Deliberately isolated from persistence and Pi RPC, the same way the egui
//! crate was: this crate depends on `pi-whim-core` for domain types and
//! `pi-whim-theme` for tokens, and nothing else from the workspace.
//!
//! Views are separate types implementing [`gpui::Render`] rather than methods
//! on one struct, which is what keeps this crate from growing into the single
//! 3.5k-line file the egui build ended up with.

pub mod chat;
pub mod chrome;
pub mod dialogs;
pub mod elements;
pub mod fonts;
pub mod icons;
pub mod pump;
pub mod settings;
pub mod theme;

mod workspace;

pub use workspace::{Request, RequestsRaised, Workspace};

use gpui::{App, Bounds, WindowBounds, WindowOptions, px, size};
use pi_whim_theme::ThemePreference;

/// Preferred window size, carried over from the egui build.
///
/// A preference, not a promise: [`window_options`] shrinks it to what the display
/// actually offers. On a 14" laptop the 860pt height lands the composer behind the
/// Dock, because the usable height there is under 900pt once the menu bar and Dock
/// are taken out.
pub const DEFAULT_WINDOW_SIZE: (f32, f32) = (1360.0, 860.0);
/// Below this the sidebar and composer stop being usable.
pub const MIN_WINDOW_SIZE: (f32, f32) = (900.0, 620.0);
/// Space to keep clear on each edge of the display, in points.
///
/// gpui reports the whole display rather than the working area, and
/// `Bounds::centered` centres on that, so the leftover height is split evenly
/// above and below. Clearing the Dock therefore costs *twice* its height. A Dock
/// with magnification runs to about 80pt, and the menu bar is shorter than that,
/// so one figure covers both edges.
const SCREEN_INSET: f32 = 80.0;

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
    let display = cx
        .primary_display()
        .map(|display| display.bounds().size)
        .map(|size| (f32::from(size.width), f32::from(size.height)));
    let (width, height) = fitted_window_size(DEFAULT_WINDOW_SIZE, display);
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

/// The preferred size, reduced to fit `display` if it does not.
///
/// Kept separate from [`window_options`] so the rule is testable without a
/// display: the test window reports none.
///
/// The floor wins over the display. A window below [`MIN_WINDOW_SIZE`] cannot show
/// the sidebar and the prompt at once, so on a display too small for even that,
/// something has to overflow — and an unusable window is worse than one the user
/// has to move.
fn fitted_window_size(preferred: (f32, f32), display: Option<(f32, f32)>) -> (f32, f32) {
    let Some((screen_width, screen_height)) = display else {
        return preferred;
    };
    // Twice the inset, because centring splits what is left over between the two
    // opposite edges.
    let fit = |preferred: f32, screen: f32, floor: f32| {
        preferred.min(screen - SCREEN_INSET * 2.0).max(floor)
    };
    (
        fit(preferred.0, screen_width, MIN_WINDOW_SIZE.0),
        fit(preferred.1, screen_height, MIN_WINDOW_SIZE.1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_display_that_fits_the_preferred_size_gets_it() {
        let size = fitted_window_size(DEFAULT_WINDOW_SIZE, Some((2560.0, 1440.0)));
        assert_eq!(size, DEFAULT_WINDOW_SIZE);
    }

    #[test]
    fn a_short_display_leaves_the_composer_clear_of_the_dock() {
        // A 14" MacBook at 2× is 982pt tall. The 860pt default left 61pt per side
        // once centred, so the composer sat behind the Dock — this is the case
        // that prompted the whole rule.
        let screen_height = 982.0;
        let (_, height) = fitted_window_size(DEFAULT_WINDOW_SIZE, Some((1512.0, screen_height)));

        // Centred on the display, so what matters is the margin, not the height.
        let margin = (screen_height - height) / 2.0;
        assert!(
            margin >= SCREEN_INSET,
            "margin {margin}pt does not clear a {SCREEN_INSET}pt Dock"
        );
        assert!(height >= MIN_WINDOW_SIZE.1);
    }

    #[test]
    fn the_usable_floor_outranks_a_tiny_display() {
        // Overflowing a very small display beats opening a window with no room
        // for the sidebar and the prompt together.
        let size = fitted_window_size(DEFAULT_WINDOW_SIZE, Some((640.0, 480.0)));
        assert_eq!(size, MIN_WINDOW_SIZE);
    }

    #[test]
    fn no_display_leaves_the_preference_alone() {
        assert_eq!(
            fitted_window_size(DEFAULT_WINDOW_SIZE, None),
            DEFAULT_WINDOW_SIZE
        );
    }

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
