//! Opens the shell with the pi.dev theme applied.
//!
//! Run with `cargo run -p pi-whim-gpui --example shell`. This is the visual
//! check for the theme layer: gpui's test window does not rasterize, so
//! comparing against pi.dev means looking at a real window.

use gpui::{App, AppContext, Context, Window};
use gpui_component::Root;
use pi_whim_core::{
    Action, ConversationItem, ConversationRole, ModelOption, Project, QueueMode, SessionStatus,
    SessionSummary, ThinkingLevel, stable_session_id,
};
use pi_whim_engine::dialogs::Prompt;
use pi_whim_gpui::Workspace;
use pi_whim_theme::ThemePreference;
use uuid::Uuid;

/// Populate the shell with a short conversation, so the preview shows the
/// layout rather than an empty window.
fn seed(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    let project = Project {
        id: Uuid::new_v4(),
        name: "pi-whim".into(),
        path: "/Users/example/pi-whim".into(),
        pinned: false,
        last_opened_ms: 1,
    };
    let project_id = project.id;
    let pi_path = "/Users/example/pi-whim/session.jsonl";

    workspace.apply(Action::ProjectsLoaded(vec![project]), window, cx);
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
        window,
        cx,
    );
    workspace.apply(Action::SelectProject(project_id), window, cx);
    workspace.apply(Action::SetSessionStatus(SessionStatus::Ready), window, cx);

    // One question waiting, so the chooser is on screen for the visual check.
    // Answering it or pressing Escape closes it; right-clicking a sidebar row
    // reaches the rest of the dialog layer.
    if let Some(prompt) = Prompt::from_interaction(
        pi_path,
        &serde_json::json!({
            "request_id": "int-1",
            "kind": "approval",
            "title": "Sub-agent wants to write",
            "message": "Write to crates/pi-whim-gpui/src/workspace.rs?",
            "options": ["approve", "deny"],
        }),
    ) {
        workspace.ask(prompt, cx);
    }
    workspace.report_info("Share URL: https://gist.github.com/example", cx);

    // Two providers, so the picker shows its grouping, and one model whose id
    // matches its name, so the reserved second line is visible in both states.
    let model = |provider: &str, provider_name: &str, id: &str, name: &str| ModelOption {
        provider: provider.into(),
        provider_name: provider_name.into(),
        id: id.into(),
        name: name.into(),
    };
    let models = vec![
        model("p1", "Anthropic", "claude-opus-4-8", "Opus 4.8"),
        model("p1", "Anthropic", "claude-sonnet-5", "Sonnet 5"),
        model("p2", "Ollama", "qwen3-coder", "qwen3-coder"),
    ];
    let current = models[0].clone();
    workspace.apply(
        Action::RuntimeControlsUpdated {
            current_model: Some(current),
            available_models: models,
            thinking_level: ThinkingLevel::Medium,
            available_thinking_levels: vec![
                ThinkingLevel::Off,
                ThinkingLevel::Low,
                ThinkingLevel::Medium,
                ThinkingLevel::High,
            ],
            auto_compaction_enabled: true,
            steering_mode: QueueMode::OneAtATime,
            follow_up_mode: QueueMode::All,
        },
        window,
        cx,
    );

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
            window,
            cx,
        );
    }
}

fn main() {
    let app = gpui_platform::application().with_assets(gpui_component_assets::Assets);

    app.run(|cx: &mut App| {
        pi_whim_gpui::init(ThemePreference::default(), cx).expect("bundled fonts should load");

        let options = pi_whim_gpui::window_options(cx);
        // Kept at the call site because `Root` erases what it wraps, and focusing
        // needs the workspace itself.
        let mut shell = None;
        let window = cx
            .open_window(options, |window, cx| {
                let workspace = cx.new(|cx| {
                    let mut workspace = Workspace::new(ThemePreference::default(), window, cx);
                    seed(&mut workspace, window, cx);
                    workspace
                });
                shell = Some(workspace.clone());
                cx.new(|cx| Root::new(workspace, window, cx))
            })
            .expect("window should open");

        // After open rather than during construction: focusing paints, and there
        // is nothing to paint into until the window exists. Without this the first
        // keystroke goes nowhere until the field is clicked.
        let shell = shell.expect("the window builder ran");
        window
            .update(cx, |_, window, cx| {
                shell.update(cx, |workspace, cx| workspace.focus_composer(window, cx));
            })
            .expect("the window is open");

        cx.activate(true);
    });
}
