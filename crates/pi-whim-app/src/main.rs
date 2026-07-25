//! Entry point.
//!
//! Only the entry point: the window and the wiring live in [`host`], and
//! orchestration in [`app`].

mod app;
mod host;

use gpui::AppContext;
use gpui_component::Root;
use pi_whim_gpui::Workspace;
use pi_whim_theme::ThemePreference;

use app::PiWhimApplication;
use host::Host;

fn main() {
    let application = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    application.run(|cx: &mut gpui::App| {
        let preference = ThemePreference::default();
        pi_whim_gpui::init(preference, cx).expect("the bundled fonts load");

        let options = pi_whim_gpui::window_options(cx);
        // Kept at the call site because `Root` erases what it wraps, and focusing
        // the prompt needs the shell itself.
        let mut shell = None;
        let window = cx
            .open_window(options, |window, cx| {
                let workspace = cx.new(|cx| Workspace::new(preference, window, cx));
                // Built here rather than before the window: the pumps it starts
                // are window tasks, and the state it seeds ends in a snapshot that
                // has to reach a view that already exists.
                let host = cx.new(|cx| {
                    Host::new(PiWhimApplication::default(), workspace.clone(), window, cx)
                });
                shell = Some(workspace);
                cx.new(|cx| Root::new(host, window, cx))
            })
            .expect("the window opens");

        // After open rather than during construction: focusing paints, and there is
        // nothing to paint into until the window exists.
        let shell = shell.expect("the window builder ran");
        window
            .update(cx, |_, window, cx| {
                shell.update(cx, |workspace, cx| workspace.focus_composer(window, cx));
            })
            .expect("the window is open");

        cx.activate(true);
    });
}
