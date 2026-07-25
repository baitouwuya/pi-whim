//! Entry point.
//!
//! Only the entry point: the window and the host live in [`app`], and
//! orchestration is reached through it.

mod app;
mod macos_paste;

use eframe::egui;
use pi_whim_ui::install_fonts;

use app::PiWhimApplication;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 620.0])
            .with_title("Pi-Whim"),
        hardware_acceleration: eframe::HardwareAcceleration::Required,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "Pi-Whim",
        native_options,
        Box::new(|creation_context| {
            install_fonts(&creation_context.egui_ctx);
            Ok(Box::<PiWhimApplication>::default())
        }),
    )
}
