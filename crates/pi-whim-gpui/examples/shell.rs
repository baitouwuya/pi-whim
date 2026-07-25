//! Opens the shell with the pi.dev theme applied.
//!
//! Run with `cargo run -p pi-whim-gpui --example shell`. This is the visual
//! check for the theme layer: gpui's test window does not rasterize, so
//! comparing against pi.dev means looking at a real window.

use gpui::{App, AppContext};
use gpui_component::Root;
use pi_whim_gpui::Workspace;
use pi_whim_theme::ThemePreference;

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(|cx: &mut App| {
        pi_whim_gpui::init(ThemePreference::default(), cx).expect("bundled fonts should load");

        let options = pi_whim_gpui::window_options(cx);
        cx.open_window(options, |window, cx| {
            let workspace = cx.new(|cx| Workspace::new(ThemePreference::default(), cx));
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("window should open");

        cx.activate(true);
    });
}
