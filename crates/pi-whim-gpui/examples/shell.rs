//! Opens the shell with the pi.dev theme applied.
//!
//! Run with `cargo run -p pi-whim-gpui --example shell`. This is the visual
//! check for the theme layer: gpui's test window does not rasterize, so
//! comparing against pi.dev means looking at a real window.

use gpui::{App, AppContext, Context};
use gpui_component::Root;
use pi_whim_core::{
    Action, ConversationItem, ConversationRole, Project, SessionStatus, SessionSummary,
    stable_session_id,
};
use pi_whim_gpui::Workspace;
use pi_whim_theme::ThemePreference;
use uuid::Uuid;

/// Populate the shell with a short conversation, so the preview shows the
/// layout rather than an empty window.
fn seed(workspace: &mut Workspace, cx: &mut Context<Workspace>) {
    let project = Project {
        id: Uuid::new_v4(),
        name: "pi-whim".into(),
        path: "/Users/example/pi-whim".into(),
        pinned: false,
        last_opened_ms: 1,
    };
    let project_id = project.id;
    let pi_path = "/Users/example/pi-whim/session.jsonl";

    workspace.apply(Action::ProjectsLoaded(vec![project]), cx);
    workspace.apply(
        Action::SessionsLoaded {
            project_id,
            sessions: vec![SessionSummary {
                id: stable_session_id(pi_path),
                project_id,
                pi_path: pi_path.into(),
                title: "Migrate the UI to gpui".into(),
                preview: "How should the views be split?".into(),
                updated_at_ms: 1,
            }],
        },
        cx,
    );
    workspace.apply(Action::SelectProject(project_id), cx);
    workspace.apply(Action::SetSessionStatus(SessionStatus::Ready), cx);

    for (id, role, text) in [
        (
            "m1",
            ConversationRole::User,
            "How should the views be split?",
        ),
        (
            "m2",
            ConversationRole::Assistant,
            "<thinking>The egui build put every view on one struct.</thinking>One type per view, each implementing `Render`.\n\n- `chrome` frames the window\n- `chat` holds the sidebar and conversation",
        ),
        ("m3", ConversationRole::System, "session ready"),
    ] {
        workspace.apply(
            Action::UpsertConversation(ConversationItem {
                id: id.into(),
                role,
                full_text: text.into(),
                streaming: false,
                tool_name: None,
                tool_report: None,
                tool_details: None,
                is_error: false,
                model: None,
                attachments: Vec::new(),
            }),
            cx,
        );
    }
}

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(|cx: &mut App| {
        pi_whim_gpui::init(ThemePreference::default(), cx).expect("bundled fonts should load");

        let options = pi_whim_gpui::window_options(cx);
        cx.open_window(options, |window, cx| {
            let workspace = cx.new(|cx| {
                let mut workspace = Workspace::new(ThemePreference::default(), cx);
                seed(&mut workspace, cx);
                workspace
            });
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("window should open");

        cx.activate(true);
    });
}
