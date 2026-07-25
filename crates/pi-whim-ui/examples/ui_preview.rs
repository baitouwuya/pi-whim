//! Headless design preview: renders the Workbench with a rich mock state and
//! writes a PNG screenshot so the UI can be reviewed without the full app.
//!
//! Usage: ui_preview <chat|settings|providers> <ready|streaming|compacting|failed> <out.png>

use std::sync::Arc;

use eframe::egui;
use pi_whim_core::{
    Action, ConversationItem, ConversationRole, Language, ModelOption, Project, ProjectId,
    QueueMode, SessionMetrics, SessionStatus, SessionSummary, ThinkingLevel,
};
use pi_whim_ui::{Workbench, install_fonts};

fn main() -> eframe::Result<()> {
    let page = std::env::args().nth(1).unwrap_or_else(|| "chat".into());
    let status = std::env::args().nth(2).unwrap_or_else(|| "ready".into());
    let out = std::env::args()
        .nth(3)
        .unwrap_or_else(|| "/tmp/pi-whim-preview.png".into());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_title("pi-whim preview"),
        ..Default::default()
    };
    eframe::run_native(
        "pi-whim-preview",
        options,
        Box::new(move |creation_context| {
            install_fonts(&creation_context.egui_ctx);
            let mut workbench = mock_workbench(&status);
            match page.as_str() {
                "settings" => workbench.preview_open_settings(false),
                "providers" => workbench.preview_open_settings(true),
                _ => {}
            }
            Ok(Box::new(PreviewApp {
                workbench,
                out,
                frames: 0,
            }))
        }),
    )
}

struct PreviewApp {
    workbench: Workbench,
    out: String,
    frames: u32,
}

impl eframe::App for PreviewApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let screenshot = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(image) = screenshot {
            save_png(&image, &self.out);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        self.frames += 1;
        self.workbench.show(ctx);
        let _ = self.workbench.take_intents();
        if self.frames == 40 {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        ctx.request_repaint();
    }
}

fn save_png(image: &egui::ColorImage, path: &str) {
    let mut bytes = Vec::with_capacity(image.width() * image.height() * 4);
    for pixel in &image.pixels {
        bytes.extend_from_slice(&pixel.to_array());
    }
    let buffer = image::RgbaImage::from_raw(image.width() as u32, image.height() as u32, bytes)
        .expect("screenshot buffer");
    buffer.save(path).expect("save screenshot");
    eprintln!("saved {path}");
}

fn mock_workbench(status: &str) -> Workbench {
    let mut workbench = Workbench::default();
    let language = std::env::args()
        .nth(4)
        .map(|arg| arg != "en")
        .unwrap_or(true);
    workbench.state.dispatch(Action::SetLanguage(if language {
        Language::SimplifiedChinese
    } else {
        Language::English
    }));

    let main_project = ProjectId::from_u128(0x11111111111111111111111111111111);
    let side_project = ProjectId::from_u128(0x22222222222222222222222222222222);
    workbench.apply(Action::ProjectsLoaded(vec![
        Project {
            id: main_project,
            name: "pi-whim".into(),
            path: "/Users/Shared/github-repos/pi-whim".into(),
            pinned: true,
            last_opened_ms: 3,
        },
        Project {
            id: side_project,
            name: "docs-site".into(),
            path: "/Users/baitouwuya/docs-site".into(),
            pinned: false,
            last_opened_ms: 1,
        },
    ]));
    let session_a = pi_whim_core::SessionId::from_u128(0x33333333333333333333333333333333);
    let session_b = pi_whim_core::SessionId::from_u128(0x44444444444444444444444444444444);
    workbench.apply(Action::SessionsLoaded {
        project_id: main_project,
        sessions: vec![
            SessionSummary {
                id: session_a,
                project_id: main_project,
                pi_path: "/tmp/session-a.jsonl".into(),
                title: "Rework the workbench layout".into(),
                preview: "Make the UI match pi.dev".into(),
                updated_at_ms: 2,
            },
            SessionSummary {
                id: session_b,
                project_id: main_project,
                pi_path: "/tmp/session-b.jsonl".into(),
                title: "Fix provider discovery".into(),
                preview: "Anthropic models missing".into(),
                updated_at_ms: 1,
            },
        ],
    });
    workbench
        .state
        .dispatch(Action::SelectProject(main_project));
    workbench.state.dispatch(Action::SelectSession(session_a));

    let model = ModelOption {
        provider: "openai".into(),
        provider_name: "OpenAI".into(),
        id: "gpt-5.1-codex".into(),
        name: "gpt-5.1-codex".into(),
    };
    workbench.state.dispatch(Action::RuntimeControlsUpdated {
        current_model: Some(model.clone()),
        available_models: vec![
            model,
            ModelOption {
                provider: "anthropic".into(),
                provider_name: "Anthropic".into(),
                id: "claude-opus-4.5".into(),
                name: "claude-opus-4.5".into(),
            },
        ],
        thinking_level: ThinkingLevel::Medium,
        available_thinking_levels: vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::Medium,
            ThinkingLevel::High,
        ],
        auto_compaction_enabled: true,
        steering_mode: QueueMode::OneAtATime,
        follow_up_mode: QueueMode::OneAtATime,
    });
    workbench
        .state
        .dispatch(Action::RuntimeCommandsUpdated(vec![
            pi_whim_core::SlashCommandInfo {
                name: "review".into(),
                description: "Review the current diff".into(),
                source: "prompt".into(),
            },
        ]));
    workbench
        .state
        .dispatch(Action::SessionMetricsUpdated(SessionMetrics {
            total_messages: 47,
            user_messages: 19,
            assistant_messages: 21,
            tool_calls: 7,
            total_tokens: 82_430,
            cost_microusd: 1_240_000,
        }));

    let session_status = match status {
        "streaming" => SessionStatus::Streaming,
        "compacting" => SessionStatus::Compacting,
        "failed" => SessionStatus::Failed(
            "POST https://api.openai.com/v1/responses: 429 Too Many Requests — rate limited, retry in 32s"
                .into(),
        ),
        _ => SessionStatus::Ready,
    };
    workbench
        .state
        .dispatch(Action::SetSessionStatus(session_status));

    if status == "bubble" {
        for item in [
            item(
                "b1",
                ConversationRole::User,
                "Rework the workbench layout so it follows pi.dev, with visible hints for compaction and request errors.",
            ),
            item("b2", ConversationRole::Assistant, "On it — reskinning now."),
        ] {
            workbench.state.dispatch(Action::UpsertConversation(item));
        }
    } else if status != "empty" {
        for item in mock_conversation() {
            workbench.state.dispatch(Action::UpsertConversation(item));
        }
    }
    if status == "slash" {
        workbench.composer_draft_mut().set_text("/");
    }
    workbench.state.dispatch(Action::QueueUpdated {
        steering: vec!["also check the runtime crate".into()],
        follow_up: vec![],
    });
    workbench
}

fn item(id: &str, role: ConversationRole, text: &str) -> ConversationItem {
    ConversationItem {
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
    }
}

fn mock_conversation() -> Vec<ConversationItem> {
    let mut tool_ok = item("m3", ConversationRole::Tool, "crates/pi-whim-ui/src/lib.rs");
    tool_ok.tool_name = Some("read".into());
    tool_ok.tool_report = Some("Read 2,729 lines (104 KB)".into());

    let mut tool_err = item("m5", ConversationRole::Tool, "cargo check -p pi-whim-ui");
    tool_err.tool_name = Some("bash".into());
    tool_err.tool_report =
        Some("error[E0061]: this function takes 4 arguments but 2 arguments were supplied".into());
    tool_err.tool_details = Some(
        "$ cargo check -p pi-whim-ui\n   Compiling pi-whim-ui v0.1.0\nerror[E0061]: ...".into(),
    );
    tool_err.is_error = true;

    let mut messages = vec![
        item(
            "m1",
            ConversationRole::User,
            "The GUI feels too plain. Rework the layout so it follows pi.dev, and add visible hints for compaction and request errors.",
        ),
        item(
            "m2",
            ConversationRole::Assistant,
            "I'll reskin the workbench around pi.dev's light theme.\n\n**Plan**\n\n- warm paper canvas, evening-blue ink, tidal-blue accents\n- a status pill for the agent state, a banner for failures\n- a bottom status line with cost, tokens and auto-compaction\n\nLet me read the current UI crate first.",
        ),
        tool_ok,
        item(
            "m4",
            ConversationRole::Assistant,
            "The layout lives in `lib.rs`. I found the composer, the message cards and the settings page. Now I'll restyle the tool cards and add the error banner.",
        ),
        tool_err,
        item(
            "m6",
            ConversationRole::Assistant,
            "That failure is a transient refactor in `pi-whim-agent-team`, not my change. Retrying the build after the tree settled.",
        ),
    ];
    for message in &mut messages {
        if message.role == ConversationRole::Assistant {
            message.model = Some("gpt-5.1-codex".into());
        }
    }
    messages
}

// Keep Arc import used even if egui changes its screenshot representation.
#[allow(dead_code)]
fn _assert_arc(image: Arc<egui::ColorImage>) -> Arc<egui::ColorImage> {
    image
}
