//! Pi-inspired egui presentation, deliberately isolated from persistence and Pi RPC.

mod markdown;

use std::{sync::Arc, time::Instant};

use eframe::egui::{
    self, Align, Button, Color32, FontData, FontDefinitions, FontFamily, FontId, Frame, Layout,
    Margin, RichText, ScrollArea, SidePanel, Stroke, TextEdit, TopBottomPanel, Ui, Vec2,
};
use pi_whim_core::{
    Action, AgentStatus, AppState, BashPolicy, ConversationRole, Language, ModelOption, ProjectId,
    ProviderId, ProviderModel, ProviderProfile, ProviderProtocol, QueueMode,
};

use markdown::MarkdownRenderer;

pub const PAPER: Color32 = Color32::from_rgb(245, 242, 236);
pub const INK: Color32 = Color32::from_rgb(37, 47, 61);
pub const MUTED_INK: Color32 = Color32::from_rgb(94, 101, 110);
pub const LINE: Color32 = Color32::from_rgb(193, 188, 180);
pub const BLUE: Color32 = Color32::from_rgb(98, 137, 182);
pub const SIDEBAR: Color32 = Color32::from_rgb(235, 233, 226);

const CJK_FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSansCJKsc-Regular.otf");

#[derive(Clone, Debug)]
pub enum UiIntent {
    AddProject,
    RemoveProject(ProjectId),
    RevealProject(ProjectId),
    StartProject(ProjectId),
    StartNewSession,
    SwitchSession(String),
    RenameSession {
        path: String,
        title: String,
    },
    CloneSession,
    ForkSession(String),
    DeleteSession(String),
    AddImageAttachment,
    SubmitPrompt {
        content: String,
        attachments: Vec<pi_whim_core::ImageAttachment>,
        mode: SubmitMode,
    },
    Stop,
    SetLanguage(Language),
    SetBashPolicy(BashPolicy),
    SetModel(ModelOption),
    SetThinkingLevel(String),
    SetQueueModes {
        steering: QueueMode,
        follow_up: QueueMode,
    },
    SaveProvider {
        profile: ProviderProfile,
        api_key: Option<String>,
    },
    DeleteProvider(ProviderId),
    DiscoverProviderModels {
        profile_id: Option<ProviderId>,
        base_url: String,
        protocol: ProviderProtocol,
        api_key: Option<String>,
    },
    RespondExtensionUi {
        request_id: String,
        confirmed: bool,
    },
}

#[derive(Clone, Copy, Debug)]
pub enum SubmitMode {
    Prompt,
    Steer,
    FollowUp,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum WorkbenchPage {
    #[default]
    Chat,
    Settings(SettingsSection),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SettingsSection {
    #[default]
    General,
    Providers,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProviderPreset {
    #[default]
    Custom,
    OpenAi,
    Anthropic,
    Google,
    OpenRouter,
}

impl ProviderPreset {
    const ALL: [Self; 5] = [
        Self::Custom,
        Self::OpenAi,
        Self::Anthropic,
        Self::Google,
        Self::OpenRouter,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Custom => "Custom",
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
            Self::Google => "Google / Gemini",
            Self::OpenRouter => "OpenRouter",
        }
    }

    fn apply(self, draft: &mut ProviderDraft) {
        let (name, base_url, protocol) = match self {
            Self::Custom => return,
            Self::OpenAi => (
                "OpenAI",
                "https://api.openai.com/v1",
                ProviderProtocol::OpenAiResponses,
            ),
            Self::Anthropic => (
                "Anthropic",
                "https://api.anthropic.com",
                ProviderProtocol::AnthropicMessages,
            ),
            Self::Google => (
                "Google / Gemini",
                "https://generativelanguage.googleapis.com/v1beta",
                ProviderProtocol::GoogleGenerativeAi,
            ),
            Self::OpenRouter => (
                "OpenRouter",
                "https://openrouter.ai/api/v1",
                ProviderProtocol::OpenAiCompletions,
            ),
        };
        draft.name = name.into();
        draft.base_url = base_url.into();
        draft.protocol = protocol;
    }
}

#[derive(Clone, Debug)]
struct ProviderDraft {
    id: Option<ProviderId>,
    name: String,
    base_url: String,
    protocol: ProviderProtocol,
    preset: ProviderPreset,
    api_key: String,
    has_api_key: bool,
    models: Vec<ProviderModel>,
    selected_model: Option<usize>,
    manual_model_id: String,
}

impl Default for ProviderDraft {
    fn default() -> Self {
        Self {
            id: None,
            name: "OpenAI-compatible".into(),
            base_url: ProviderProtocol::OpenAiCompletions
                .default_base_url()
                .into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            preset: ProviderPreset::Custom,
            api_key: String::new(),
            has_api_key: false,
            models: Vec::new(),
            selected_model: None,
            manual_model_id: String::new(),
        }
    }
}

impl ProviderDraft {
    fn from_profile(profile: &ProviderProfile) -> Self {
        Self {
            id: Some(profile.id),
            name: profile.name.clone(),
            base_url: profile.base_url.clone(),
            protocol: profile.protocol,
            preset: ProviderPreset::Custom,
            api_key: String::new(),
            has_api_key: profile.has_api_key,
            models: profile.models.clone(),
            selected_model: None,
            manual_model_id: String::new(),
        }
    }

    fn to_profile(&self) -> ProviderProfile {
        ProviderProfile {
            id: self.id.unwrap_or_else(uuid::Uuid::new_v4),
            name: self.name.trim().to_owned(),
            base_url: self.base_url.trim().trim_end_matches('/').to_owned(),
            protocol: self.protocol,
            models: self.models.clone(),
            updated_at_ms: now_ms(),
            has_api_key: self.has_api_key || !self.api_key.trim().is_empty(),
        }
    }
}

pub struct Workbench {
    pub state: AppState,
    intents: Vec<UiIntent>,
    page: WorkbenchPage,
    provider_draft: ProviderDraft,
    last_frame: Instant,
    rename_session_path: Option<String>,
    rename_session_title: String,
    markdown: MarkdownRenderer,
}

impl Default for Workbench {
    fn default() -> Self {
        Self {
            state: AppState::default(),
            intents: Vec::new(),
            page: WorkbenchPage::Chat,
            provider_draft: ProviderDraft::default(),
            last_frame: Instant::now(),
            rename_session_path: None,
            rename_session_title: String::new(),
            markdown: MarkdownRenderer::default(),
        }
    }
}

impl Workbench {
    pub fn take_intents(&mut self) -> Vec<UiIntent> {
        std::mem::take(&mut self.intents)
    }

    pub fn apply(&mut self, action: Action) {
        if let Action::ProviderProfilesLoaded(profiles) = action {
            self.sync_provider_draft(&profiles);
            self.state
                .dispatch(Action::ProviderProfilesLoaded(profiles));
        } else {
            self.state.dispatch(action);
        }
    }

    fn sync_provider_draft(&mut self, profiles: &[ProviderProfile]) {
        let selected = self
            .provider_draft
            .id
            .and_then(|id| profiles.iter().find(|profile| profile.id == id))
            .or_else(|| {
                profiles
                    .iter()
                    .filter(|profile| profile.has_api_key)
                    .max_by_key(|profile| profile.updated_at_ms)
            })
            .or_else(|| profiles.iter().max_by_key(|profile| profile.updated_at_ms));
        self.provider_draft = selected
            .map(ProviderDraft::from_profile)
            .unwrap_or_default();
    }

    /// The app only supplies discovered identifiers; credentials never flow back to the UI.
    pub fn set_discovered_models(&mut self, models: Vec<ProviderModel>) {
        self.provider_draft.models = models;
        self.provider_draft.selected_model = None;
    }

    /// Reflect a verified Keychain result rather than a value merely typed in
    /// the secure field.
    pub fn set_provider_key_status(&mut self, profile_id: ProviderId, saved: bool) {
        if self.provider_draft.id == Some(profile_id) {
            self.provider_draft.has_api_key = saved;
            if saved {
                self.provider_draft.api_key.clear();
            }
        }
    }

    fn save_provider_intent(&mut self) {
        let profile = self.provider_draft.to_profile();
        let api_key = (!self.provider_draft.api_key.trim().is_empty())
            .then(|| std::mem::take(&mut self.provider_draft.api_key));
        self.provider_draft.id = Some(profile.id);
        self.intents
            .push(UiIntent::SaveProvider { profile, api_key });
    }

    pub fn show(&mut self, context: &egui::Context) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        if self.state.tick_typewriter(elapsed) {
            context.request_repaint();
        }
        install_theme(context);
        self.top_bar(context);
        match self.page {
            WorkbenchPage::Chat => {
                self.sidebar(context);
                self.composer(context);
                self.conversation(context);
            }
            WorkbenchPage::Settings(section) => self.settings_page(context, section),
        }
        self.rename_session_dialog(context);
    }

    fn top_bar(&mut self, context: &egui::Context) {
        TopBottomPanel::top("top_bar")
            .frame(panel_frame())
            .show(context, |ui| {
                ui.set_height(44.0);
                ui.horizontal(|ui| {
                    ui.add_space(8.0);
                    ui.label(RichText::new("PI-WHIM").font(mono_font(13.0)).strong());
                    ui.separator();
                    ui.label(
                        RichText::new(status_text(&self.state.agent_status))
                            .color(MUTED_INK)
                            .font(mono_font(11.0)),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .button(RichText::new("⚙").font(mono_font(16.0)))
                            .on_hover_text(tr(&self.state, "settings"))
                            .clicked()
                        {
                            self.page = WorkbenchPage::Settings(SettingsSection::General);
                        }
                        let language = match self.state.language {
                            Language::English => "中文",
                            Language::SimplifiedChinese => "EN",
                        };
                        if ui
                            .button(RichText::new(language).font(mono_font(11.0)))
                            .clicked()
                        {
                            let next = match self.state.language {
                                Language::English => Language::SimplifiedChinese,
                                Language::SimplifiedChinese => Language::English,
                            };
                            self.state.dispatch(Action::SetLanguage(next));
                            self.intents.push(UiIntent::SetLanguage(next));
                        }
                    });
                });
            });
    }

    fn sidebar(&mut self, context: &egui::Context) {
        SidePanel::left("sidebar")
            .resizable(true)
            .min_width(220.0)
            .max_width(390.0)
            .frame(
                Frame::default()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::same(10))
                    .stroke(Stroke::new(1.0_f32, LINE)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new(tr(&self.state, "projects")).font(serif_font(23.0)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .button(RichText::new("+").font(mono_font(18.0)))
                            .on_hover_text(tr(&self.state, "add-project"))
                            .clicked()
                        {
                            self.intents.push(UiIntent::AddProject);
                        }
                    });
                });
                ui.add_space(8.0);
                let search_hint = tr(&self.state, "search").to_owned();
                ui.add(
                    TextEdit::singleline(&mut self.state.search)
                        .hint_text(search_hint)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                let project_ids: Vec<_> = self
                    .state
                    .projects
                    .iter()
                    .map(|project| project.id)
                    .collect();
                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        for project_id in project_ids {
                            self.project_row(ui, project_id);
                        }
                    });
            });
    }

    fn project_row(&mut self, ui: &mut Ui, project_id: ProjectId) {
        let Some(project) = self
            .state
            .projects
            .iter()
            .find(|project| project.id == project_id)
            .cloned()
        else {
            return;
        };
        if !self.state.search.is_empty()
            && !project
                .name
                .to_lowercase()
                .contains(&self.state.search.to_lowercase())
        {
            return;
        }
        let selected = self.state.selected_project == Some(project.id);
        let response = ui.add_sized(
            [ui.available_width(), 30.0],
            Button::new(RichText::new(format!("⌑  {}", project.name)).font(mono_font(13.0)))
                .selected(selected),
        );
        if response.clicked() {
            self.state.dispatch(Action::SelectProject(project.id));
            self.intents.push(UiIntent::StartProject(project.id));
        }
        response.context_menu(|ui| {
            if ui.button(tr(&self.state, "show-finder")).clicked() {
                self.intents.push(UiIntent::RevealProject(project.id));
                ui.close_menu();
            }
            if ui.button(tr(&self.state, "remove")).clicked() {
                self.intents.push(UiIntent::RemoveProject(project.id));
                ui.close_menu();
            }
        });
        if selected {
            let sessions = self
                .state
                .sessions
                .get(&project.id)
                .cloned()
                .unwrap_or_default();
            for session in sessions {
                let selected_session = self.state.selected_session == Some(session.id);
                let response = ui.add_sized(
                    [ui.available_width() - 12.0, 25.0],
                    Button::new(
                        RichText::new(format!("    {}", session.title))
                            .color(if selected_session { INK } else { MUTED_INK })
                            .font(serif_font(15.0)),
                    )
                    .selected(selected_session),
                );
                if response.clicked() {
                    self.state.dispatch(Action::SelectSession(session.id));
                    self.intents
                        .push(UiIntent::SwitchSession(session.pi_path.clone()));
                }
                response.context_menu(|ui| {
                    if ui.button(tr(&self.state, "rename")).clicked() {
                        self.rename_session_path = Some(session.pi_path.clone());
                        self.rename_session_title = session.title.clone();
                        ui.close_menu();
                    }
                    if ui.button(tr(&self.state, "clone")).clicked() {
                        self.intents.push(UiIntent::CloneSession);
                        ui.close_menu();
                    }
                    if ui.button(tr(&self.state, "delete")).clicked() {
                        self.intents
                            .push(UiIntent::DeleteSession(session.pi_path.clone()));
                        ui.close_menu();
                    }
                });
            }
        }
    }

    fn conversation(&mut self, context: &egui::Context) {
        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(PAPER)
                    .inner_margin(Margin::symmetric(28, 20)),
            )
            .show(context, |ui| {
                paint_paper_grid(ui.painter(), ui.max_rect());
                ui.horizontal(|ui| {
                    let label = self
                        .state
                        .selected_project
                        .and_then(|id| self.state.projects.iter().find(|project| project.id == id))
                        .map(|project| project.name.as_str())
                        .unwrap_or("pi-whim");
                    ui.heading(RichText::new(label).font(serif_font(26.0)));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.state.selected_project.is_some()
                            && ui
                                .button(
                                    RichText::new(tr(&self.state, "new-session"))
                                        .font(mono_font(11.0)),
                                )
                                .clicked()
                        {
                            self.intents.push(UiIntent::StartNewSession);
                        }
                    });
                });
                ui.separator();
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        if self.state.conversation.is_empty() {
                            ui.add_space(120.0);
                            ui.vertical_centered(|ui| {
                                ui.label(RichText::new("◰").size(38.0).color(MUTED_INK));
                                ui.label(
                                    RichText::new(tr(&self.state, "empty-heading"))
                                        .font(serif_font(34.0))
                                        .color(INK),
                                );
                                ui.label(
                                    RichText::new(tr(&self.state, "empty-detail")).color(MUTED_INK),
                                );
                            });
                        }
                        let message_ids: Vec<_> = self
                            .state
                            .conversation
                            .iter()
                            .map(|message| message.id.clone())
                            .collect();
                        for message_id in message_ids {
                            self.message_card(ui, &message_id);
                        }
                    });
            });
    }

    fn runtime_controls(&mut self, ui: &mut Ui) {
        if self.state.selected_project.is_none() {
            return;
        }
        if !self.state.available_models.is_empty() {
            let current = self
                .state
                .current_model
                .as_ref()
                .map(|model| model.id.clone())
                .unwrap_or_else(|| "Model".into());
            egui::ComboBox::from_id_salt("model-picker")
                .width(220.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    for model in self.state.available_models.clone() {
                        let selected = self.state.current_model.as_ref().is_some_and(|current| {
                            current.provider == model.provider && current.id == model.id
                        });
                        if ui.selectable_label(selected, model.label()).clicked() {
                            self.intents.push(UiIntent::SetModel(model));
                        }
                    }
                });
        } else if let AgentStatus::Failed(error) = &self.state.agent_status {
            ui.label(
                RichText::new(format!(
                    "{}: {error}",
                    tr(&self.state, "models-unavailable")
                ))
                .font(mono_font(10.0))
                .color(Color32::DARK_RED),
            );
        } else {
            ui.label(
                RichText::new(tr(&self.state, "models-unavailable"))
                    .font(mono_font(10.0))
                    .color(MUTED_INK),
            );
        }
        if !self.state.available_thinking_levels.is_empty() {
            let mut level = self.state.thinking_level.clone();
            egui::ComboBox::from_id_salt("thinking-picker")
                .width(92.0)
                .selected_text(format!("{}: {level}", tr(&self.state, "thinking")))
                .show_ui(ui, |ui| {
                    for candidate in &self.state.available_thinking_levels {
                        ui.selectable_value(&mut level, candidate.clone(), candidate);
                    }
                });
            if level != self.state.thinking_level {
                self.intents.push(UiIntent::SetThinkingLevel(level));
            }
        }
        if let Some(metrics) = &self.state.session_metrics {
            ui.label(
                RichText::new(format!(
                    "{} msg  {} tok  ${:.4}",
                    metrics.total_messages,
                    metrics.total_tokens,
                    metrics.cost_microusd as f64 / 1_000_000.0
                ))
                .font(mono_font(10.0))
                .color(MUTED_INK),
            );
        }
    }

    fn message_card(&mut self, ui: &mut Ui, message_id: &str) {
        let Some(message) = self
            .state
            .conversation
            .iter()
            .find(|message| message.id == message_id)
            .cloned()
        else {
            return;
        };
        ui.add_space(12.0);
        let fill = match message.role {
            ConversationRole::User => Color32::from_rgb(232, 237, 242),
            ConversationRole::Tool => Color32::from_rgb(238, 235, 227),
            _ => PAPER,
        };
        Frame::default()
            .fill(fill)
            .stroke(Stroke::new(1.0_f32, LINE))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                let role = match message.role {
                    ConversationRole::User => tr(&self.state, "you"),
                    ConversationRole::Assistant => "PI",
                    ConversationRole::Tool => message.tool_name.as_deref().unwrap_or("TOOL"),
                    ConversationRole::System => "SYSTEM",
                };
                ui.label(RichText::new(role).font(mono_font(10.0)).color(MUTED_INK));
                let content = message.text_for_display();
                if message.role == ConversationRole::Tool {
                    ui.label(RichText::new(content).font(mono_font(12.0)).color(
                        if message.is_error {
                            Color32::DARK_RED
                        } else {
                            INK
                        },
                    ));
                    if let Some(details) = message.tool_details.as_deref() {
                        ui.collapsing(tr(&self.state, "details"), |ui| {
                            ui.label(RichText::new(details).font(mono_font(11.0)));
                        });
                    }
                } else if message.role == ConversationRole::Assistant && !message.streaming {
                    self.markdown.show(ui, &message.id, content);
                } else {
                    ui.label(
                        RichText::new(content)
                            .font(if message.role == ConversationRole::Assistant {
                                serif_font(18.0)
                            } else {
                                serif_font(16.0)
                            })
                            .color(INK),
                    );
                }
                if message.streaming
                    && ui
                        .button(RichText::new(tr(&self.state, "show-all")).font(mono_font(10.0)))
                        .clicked()
                {
                    self.state
                        .dispatch(Action::SkipTypewriter(message.id.clone()));
                }
                if message.role == ConversationRole::User {
                    if !message.attachments.is_empty() {
                        ui.horizontal_wrapped(|ui| {
                            for attachment in &message.attachments {
                                ui.label(
                                    RichText::new(format!("[image] {}", attachment.name))
                                        .font(mono_font(10.0))
                                        .color(BLUE),
                                );
                            }
                        });
                    }
                    ui.separator();
                    if ui
                        .button(RichText::new(tr(&self.state, "fork-here")).font(mono_font(10.0)))
                        .clicked()
                    {
                        self.intents.push(UiIntent::ForkSession(message.id));
                    }
                }
            });
    }

    fn composer(&mut self, context: &egui::Context) {
        TopBottomPanel::bottom("composer")
            .frame(panel_frame())
            .show(context, |ui| {
                ui.add_space(4.0);
                let project_selected = self.state.selected_project.is_some();
                if !project_selected {
                    ui.label(
                        RichText::new(tr(&self.state, "select-project-to-chat"))
                            .font(mono_font(11.0))
                            .color(MUTED_INK),
                    );
                }
                ui.add_enabled_ui(project_selected, |ui| {
                    ui.horizontal(|ui| {
                        let composer_hint = tr(&self.state, "composer-placeholder").to_owned();
                        let input = ui.add(
                            TextEdit::multiline(&mut self.state.composer)
                                .hint_text(composer_hint)
                                .desired_rows(3)
                                .desired_width(ui.available_width() - 118.0),
                        );
                        let submit = ui.add_sized(
                            [88.0, 46.0],
                            Button::new(RichText::new("↑").font(mono_font(22.0))).fill(INK),
                        );
                        if submit.clicked()
                            && (!self.state.composer.trim().is_empty()
                                || !self.state.composer_attachments.is_empty())
                        {
                            let content = std::mem::take(&mut self.state.composer);
                            let attachments = std::mem::take(&mut self.state.composer_attachments);
                            self.intents.push(UiIntent::SubmitPrompt {
                                content,
                                attachments,
                                mode: SubmitMode::Prompt,
                            });
                        }
                        if input.has_focus()
                            && context.input(|input| input.key_pressed(egui::Key::Escape))
                        {
                            for message in &mut self.state.conversation {
                                message.reveal_all();
                            }
                        }
                    })
                });
                ui.add_enabled_ui(project_selected, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button("+")
                            .on_hover_text(tr(&self.state, "add-image"))
                            .clicked()
                        {
                            self.intents.push(UiIntent::AddImageAttachment);
                        }
                        ui.label(
                            RichText::new(status_text(&self.state.agent_status))
                                .font(mono_font(10.0))
                                .color(MUTED_INK),
                        );
                        self.runtime_controls(ui);
                        if !self.state.composer_attachments.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    self.state.composer_attachments.len(),
                                    tr(&self.state, "images")
                                ))
                                .font(mono_font(10.0))
                                .color(BLUE),
                            );
                        }
                        if !self.state.pending_steering.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    tr(&self.state, "queued"),
                                    self.state.pending_steering.len()
                                ))
                                .font(mono_font(10.0))
                                .color(BLUE),
                            );
                        }
                        if !self.state.pending_follow_up.is_empty() {
                            ui.label(
                                RichText::new(format!(
                                    "{} {}",
                                    tr(&self.state, "follow-ups"),
                                    self.state.pending_follow_up.len()
                                ))
                                .font(mono_font(10.0))
                                .color(BLUE),
                            );
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if matches!(
                                self.state.agent_status,
                                AgentStatus::Streaming | AgentStatus::Compacting
                            ) && ui.button(tr(&self.state, "stop")).clicked()
                            {
                                self.intents.push(UiIntent::Stop);
                            }
                        });
                    })
                });
            });
    }

    fn settings_page(&mut self, context: &egui::Context, section: SettingsSection) {
        SidePanel::left("settings-navigation")
            .exact_width(236.0)
            .frame(
                Frame::default()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::same(16))
                    .stroke(Stroke::new(1.0_f32, LINE)),
            )
            .show(context, |ui| {
                if ui
                    .button(RichText::new("←").font(mono_font(18.0)))
                    .clicked()
                {
                    self.page = WorkbenchPage::Chat;
                }
                ui.add_space(18.0);
                ui.heading(RichText::new(tr(&self.state, "settings")).font(serif_font(28.0)));
                ui.add_space(14.0);
                if ui
                    .selectable_label(
                        section == SettingsSection::General,
                        tr(&self.state, "general"),
                    )
                    .clicked()
                {
                    self.page = WorkbenchPage::Settings(SettingsSection::General);
                }
                if ui
                    .selectable_label(
                        section == SettingsSection::Providers,
                        tr(&self.state, "providers"),
                    )
                    .clicked()
                {
                    self.page = WorkbenchPage::Settings(SettingsSection::Providers);
                }
            });
        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(PAPER)
                    .inner_margin(Margin::symmetric(46, 30)),
            )
            .show(context, |ui| {
                paint_paper_grid(ui.painter(), ui.max_rect());
                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| match section {
                        SettingsSection::General => self.general_settings(ui),
                        SettingsSection::Providers => self.provider_settings(ui),
                    });
            });
    }

    fn general_settings(&mut self, ui: &mut Ui) {
        ui.heading(RichText::new(tr(&self.state, "general")).font(serif_font(32.0)));
        ui.add_space(10.0);
        ui.heading(RichText::new(tr(&self.state, "language")).font(serif_font(20.0)));
        ui.horizontal(|ui| {
            let previous_language = self.state.language;
            ui.selectable_value(&mut self.state.language, Language::English, "English");
            ui.selectable_value(
                &mut self.state.language,
                Language::SimplifiedChinese,
                "简体中文",
            );
            if self.state.language != previous_language {
                self.intents
                    .push(UiIntent::SetLanguage(self.state.language));
            }
        });
        ui.add_space(20.0);
        ui.heading(RichText::new(tr(&self.state, "bash-policy")).font(serif_font(20.0)));
        for (label, policy) in [
            ("Allow", BashPolicy::Allow),
            ("Ask", BashPolicy::Ask),
            ("Deny", BashPolicy::Deny),
        ] {
            if ui
                .selectable_label(self.state.bash_policy == policy, label)
                .clicked()
            {
                self.state.dispatch(Action::SetBashPolicy(policy));
                self.intents.push(UiIntent::SetBashPolicy(policy));
            }
        }
        ui.label(
            RichText::new(tr(&self.state, "bash-help"))
                .small()
                .color(MUTED_INK),
        );
        ui.add_space(20.0);
        ui.heading(RichText::new(tr(&self.state, "queue-mode")).font(serif_font(20.0)));
        let mut steering_mode = self.state.steering_mode;
        let mut follow_up_mode = self.state.follow_up_mode;
        ui.horizontal(|ui| {
            ui.label(tr(&self.state, "steer-mode"));
            ui.selectable_value(&mut steering_mode, QueueMode::OneAtATime, "one at a time");
            ui.selectable_value(&mut steering_mode, QueueMode::All, "all");
        });
        ui.horizontal(|ui| {
            ui.label(tr(&self.state, "follow-up-mode"));
            ui.selectable_value(&mut follow_up_mode, QueueMode::OneAtATime, "one at a time");
            ui.selectable_value(&mut follow_up_mode, QueueMode::All, "all");
        });
        if steering_mode != self.state.steering_mode || follow_up_mode != self.state.follow_up_mode
        {
            self.intents.push(UiIntent::SetQueueModes {
                steering: steering_mode,
                follow_up: follow_up_mode,
            });
        }
    }

    fn provider_settings(&mut self, ui: &mut Ui) {
        ui.heading(RichText::new(tr(&self.state, "providers")).font(serif_font(32.0)));
        ui.label(RichText::new(tr(&self.state, "providers-help")).color(MUTED_INK));
        ui.add_space(14.0);
        ui.horizontal_wrapped(|ui| {
            for profile in self.state.provider_profiles.clone() {
                if ui
                    .selectable_label(
                        self.provider_draft.id == Some(profile.id),
                        RichText::new(&profile.name).font(mono_font(11.0)),
                    )
                    .clicked()
                {
                    self.provider_draft = ProviderDraft::from_profile(&profile);
                }
            }
            if ui
                .button(RichText::new("+").font(mono_font(16.0)))
                .clicked()
            {
                self.provider_draft = ProviderDraft::default();
            }
        });
        ui.add_space(16.0);
        let mut preset = self.provider_draft.preset;
        ui.label(tr(&self.state, "preset"));
        egui::ComboBox::from_id_salt("provider-preset")
            .selected_text(preset.label())
            .show_ui(ui, |ui| {
                for option in ProviderPreset::ALL {
                    ui.selectable_value(&mut preset, option, option.label());
                }
            });
        if preset != self.provider_draft.preset {
            self.provider_draft.preset = preset;
            preset.apply(&mut self.provider_draft);
        }
        ui.add_space(8.0);
        ui.label(tr(&self.state, "provider-name"));
        ui.add(TextEdit::singleline(&mut self.provider_draft.name).desired_width(480.0));
        ui.add_space(8.0);
        ui.label("Base URL");
        ui.add(TextEdit::singleline(&mut self.provider_draft.base_url).desired_width(560.0));
        ui.add_space(8.0);
        ui.label(tr(&self.state, "protocol"));
        let old_protocol = self.provider_draft.protocol;
        egui::ComboBox::from_id_salt("provider-protocol")
            .selected_text(self.provider_draft.protocol.label())
            .show_ui(ui, |ui| {
                for protocol in ProviderProtocol::ALL {
                    ui.selectable_value(
                        &mut self.provider_draft.protocol,
                        protocol,
                        protocol.label(),
                    );
                }
            });
        if old_protocol != self.provider_draft.protocol
            && self.provider_draft.base_url.trim() == old_protocol.default_base_url()
        {
            self.provider_draft.base_url = self.provider_draft.protocol.default_base_url().into();
        }
        ui.add_space(8.0);
        ui.label("API Key");
        let key_hint = if self.provider_draft.has_api_key {
            tr(&self.state, "key-stored")
        } else {
            tr(&self.state, "key-required")
        };
        ui.add(
            TextEdit::singleline(&mut self.provider_draft.api_key)
                .password(true)
                .hint_text(key_hint)
                .desired_width(480.0),
        );
        ui.label(
            RichText::new(tr(&self.state, "provider-help"))
                .small()
                .color(MUTED_INK),
        );
        let can_save = !self.provider_draft.name.trim().is_empty()
            && !self.provider_draft.base_url.trim().is_empty()
            && !self.provider_draft.models.is_empty();
        if ui
            .add_enabled(can_save, Button::new(tr(&self.state, "save-and-apply")))
            .clicked()
        {
            self.save_provider_intent();
        }
        ui.add_space(16.0);
        ui.horizontal(|ui| {
            let can_discover = !self.provider_draft.base_url.trim().is_empty();
            if ui
                .add_enabled(
                    can_discover,
                    Button::new(tr(&self.state, "discover-models")),
                )
                .clicked()
            {
                self.intents.push(UiIntent::DiscoverProviderModels {
                    profile_id: self.provider_draft.id,
                    base_url: self.provider_draft.base_url.clone(),
                    protocol: self.provider_draft.protocol,
                    api_key: (!self.provider_draft.api_key.trim().is_empty())
                        .then(|| self.provider_draft.api_key.clone()),
                });
            }
            if ui.button(tr(&self.state, "add-model")).clicked() {
                let model_id = self.provider_draft.manual_model_id.trim().to_owned();
                if !model_id.is_empty()
                    && !self
                        .provider_draft
                        .models
                        .iter()
                        .any(|model| model.id == model_id)
                {
                    self.provider_draft
                        .models
                        .push(ProviderModel::new(model_id));
                }
                self.provider_draft.manual_model_id.clear();
            }
            ui.add(
                TextEdit::singleline(&mut self.provider_draft.manual_model_id)
                    .hint_text(tr(&self.state, "model-id"))
                    .desired_width(260.0),
            );
        });
        ui.add_space(10.0);
        if self.provider_draft.models.is_empty() {
            ui.label(RichText::new(tr(&self.state, "no-models")).color(MUTED_INK));
        } else {
            let mut remove = None;
            for (index, model) in self.provider_draft.models.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&model.name).font(mono_font(12.0)));
                    ui.label(RichText::new(&model.id).small().color(MUTED_INK));
                    if ui.small_button("×").clicked() {
                        remove = Some(index);
                    }
                });
            }
            if let Some(index) = remove {
                self.provider_draft.models.remove(index);
            }
        }
        ui.add_space(18.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(can_save, Button::new(tr(&self.state, "save-provider")))
                .clicked()
            {
                self.save_provider_intent();
            }
            if let Some(id) = self.provider_draft.id
                && ui.button(tr(&self.state, "delete-provider")).clicked()
            {
                self.intents.push(UiIntent::DeleteProvider(id));
                self.provider_draft = ProviderDraft::default();
            }
        });
    }

    fn rename_session_dialog(&mut self, context: &egui::Context) {
        let Some(path) = self.rename_session_path.clone() else {
            return;
        };
        let mut open = true;
        egui::Window::new(tr(&self.state, "rename-session"))
            .open(&mut open)
            .collapsible(false)
            .show(context, |ui| {
                ui.add(TextEdit::singleline(&mut self.rename_session_title).desired_width(300.0));
                if ui.button(tr(&self.state, "save")).clicked()
                    && !self.rename_session_title.trim().is_empty()
                {
                    let title = std::mem::take(&mut self.rename_session_title);
                    self.intents.push(UiIntent::RenameSession { path, title });
                    self.rename_session_path = None;
                }
            });
        if !open {
            self.rename_session_path = None;
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn panel_frame() -> Frame {
    Frame::default()
        .fill(PAPER)
        .inner_margin(Margin::symmetric(16, 8))
        .stroke(Stroke::new(1.0_f32, LINE))
}
fn mono_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
}
fn serif_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}

fn status_text(status: &AgentStatus) -> &'static str {
    match status {
        AgentStatus::Offline => "OFFLINE",
        AgentStatus::Starting => "STARTING",
        AgentStatus::Ready => "READY",
        AgentStatus::Streaming => "THINKING",
        AgentStatus::Compacting => "COMPACTING",
        AgentStatus::Failed(_) => "ERROR",
    }
}

fn tr<'a>(state: &AppState, key: &'a str) -> &'a str {
    let chinese = state.language == Language::SimplifiedChinese;
    match (chinese, key) {
        (_, "projects") => {
            if chinese {
                "项目"
            } else {
                "Projects"
            }
        }
        (_, "add-project") => {
            if chinese {
                "添加本地项目"
            } else {
                "Add local project"
            }
        }
        (_, "search") => {
            if chinese {
                "搜索项目"
            } else {
                "Search projects"
            }
        }
        (_, "show-finder") => {
            if chinese {
                "在 Finder 中显示"
            } else {
                "Show in Finder"
            }
        }
        (_, "remove") => {
            if chinese {
                "移除"
            } else {
                "Remove"
            }
        }
        (_, "rename") => {
            if chinese {
                "重命名"
            } else {
                "Rename"
            }
        }
        (_, "rename-session") => {
            if chinese {
                "重命名会话"
            } else {
                "Rename session"
            }
        }
        (_, "clone") => {
            if chinese {
                "克隆会话"
            } else {
                "Clone session"
            }
        }
        (_, "delete") => {
            if chinese {
                "移至废纸篓"
            } else {
                "Move to trash"
            }
        }
        (_, "save") => {
            if chinese {
                "保存"
            } else {
                "Save"
            }
        }
        (_, "new-session") => {
            if chinese {
                "新建会话"
            } else {
                "New session"
            }
        }
        (_, "empty-heading") => {
            if chinese {
                "我们应该在这里做些什么？"
            } else {
                "What should we make happen?"
            }
        }
        (_, "empty-detail") => {
            if chinese {
                "选择一个项目，然后告诉 Pi 你想完成什么。"
            } else {
                "Select a project, then tell Pi what you want to do."
            }
        }
        (_, "select-project-to-chat") => {
            if chinese {
                "先从左侧添加并选择一个项目，才能开始对话。"
            } else {
                "Add and select a project from the sidebar before starting a conversation."
            }
        }
        (_, "you") => {
            if chinese {
                "你"
            } else {
                "YOU"
            }
        }
        (_, "details") => {
            if chinese {
                "详细信息"
            } else {
                "Details"
            }
        }
        (_, "show-all") => {
            if chinese {
                "显示完整内容"
            } else {
                "Show all"
            }
        }
        (_, "fork-here") => {
            if chinese {
                "从这里分叉"
            } else {
                "Fork from here"
            }
        }
        (_, "composer-placeholder") => {
            if chinese {
                "告诉 Pi 你想完成什么..."
            } else {
                "Tell Pi what you want to do..."
            }
        }
        (_, "add-image") => {
            if chinese {
                "添加图片"
            } else {
                "Add image"
            }
        }
        (_, "images") => {
            if chinese {
                "张图片"
            } else {
                "images"
            }
        }
        (_, "queued") => {
            if chinese {
                "已排队"
            } else {
                "QUEUED"
            }
        }
        (_, "follow-ups") => {
            if chinese {
                "后续队列"
            } else {
                "FOLLOW-UPS"
            }
        }
        (_, "thinking") => {
            if chinese {
                "思考"
            } else {
                "Thinking"
            }
        }
        (_, "models-unavailable") => {
            if chinese {
                "没有可用模型。请在“设置 > 模型提供商”保存 API Key 和至少一个模型，然后重新打开项目。"
            } else {
                "No models are available. Save an API key and at least one model in Settings > Providers, then reopen the project."
            }
        }
        (_, "stop") => {
            if chinese {
                "停止"
            } else {
                "Stop"
            }
        }
        (_, "settings") => {
            if chinese {
                "设置"
            } else {
                "Settings"
            }
        }
        (_, "general") => {
            if chinese {
                "通用"
            } else {
                "General"
            }
        }
        (_, "providers") => {
            if chinese {
                "模型提供商"
            } else {
                "Providers"
            }
        }
        (_, "providers-help") => {
            if chinese {
                "使用 Base URL、请求协议和 API Key 配置兼容供应商。模型会写入 Pi 的独立配置目录。"
            } else {
                "Configure compatible providers with a base URL, request protocol, and API key. Models are written to Pi's isolated config directory."
            }
        }
        (_, "provider-name") => {
            if chinese {
                "名称"
            } else {
                "Name"
            }
        }
        (_, "preset") => {
            if chinese {
                "预设"
            } else {
                "Preset"
            }
        }
        (_, "protocol") => {
            if chinese {
                "请求协议"
            } else {
                "Request protocol"
            }
        }
        (_, "key-stored") => {
            if chinese {
                "已保存在 Keychain；留空可保持不变"
            } else {
                "Stored in Keychain; leave blank to keep it"
            }
        }
        (_, "key-required") => {
            if chinese {
                "输入 API Key（仅保存在 Keychain）"
            } else {
                "Enter API key (stored only in Keychain)"
            }
        }
        (_, "discover-models") => {
            if chinese {
                "发现模型"
            } else {
                "Discover models"
            }
        }
        (_, "add-model") => {
            if chinese {
                "添加模型"
            } else {
                "Add model"
            }
        }
        (_, "model-id") => {
            if chinese {
                "手动输入模型 ID"
            } else {
                "Manual model ID"
            }
        }
        (_, "no-models") => {
            if chinese {
                "尚未选择模型。请发现模型或手动添加模型 ID。"
            } else {
                "No models selected. Discover models or add a model ID manually."
            }
        }
        (_, "save-provider") => {
            if chinese {
                "保存提供商"
            } else {
                "Save provider"
            }
        }
        (_, "save-and-apply") => {
            if chinese {
                "保存并应用"
            } else {
                "Save and apply"
            }
        }
        (_, "delete-provider") => {
            if chinese {
                "删除提供商"
            } else {
                "Delete provider"
            }
        }
        (_, "language") => {
            if chinese {
                "语言"
            } else {
                "Language"
            }
        }
        (_, "bash-policy") => {
            if chinese {
                "Bash 命令"
            } else {
                "Bash commands"
            }
        }
        (_, "bash-help") => {
            if chinese {
                "此设置仅影响 Bash；Pi 的其他内置工具保持默认权限。"
            } else {
                "This affects Bash only; Pi's other built-in tools keep their default access."
            }
        }
        (_, "queue-mode") => {
            if chinese {
                "队列模式"
            } else {
                "Queue mode"
            }
        }
        (_, "steer-mode") => "Steer",
        (_, "follow-up-mode") => "Follow-up",
        (_, "provider-key") => {
            if chinese {
                "模型密钥"
            } else {
                "Model key"
            }
        }
        (_, "provider-help") => {
            if chinese {
                "密钥安全存储在 macOS Keychain，并只注入 Pi 子进程。"
            } else {
                "Keys are stored in macOS Keychain and injected only into Pi's child process."
            }
        }
        (_, "save-key") => {
            if chinese {
                "保存到 Keychain"
            } else {
                "Save to Keychain"
            }
        }
        _ => key,
    }
}

pub fn install_theme(context: &egui::Context) {
    let mut visuals = egui::Visuals::light();
    visuals.window_fill = PAPER;
    visuals.panel_fill = PAPER;
    visuals.faint_bg_color = Color32::from_rgba_unmultiplied(37, 47, 61, 7);
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, LINE);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, LINE);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, INK);
    visuals.widgets.active.bg_fill = Color32::from_rgb(220, 228, 234);
    visuals.selection.bg_fill = Color32::from_rgb(203, 220, 237);
    visuals.override_text_color = Some(INK);
    context.set_visuals(visuals);
    let mut style = (*context.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.visuals = context.style().visuals.clone();
    context.set_style(style);
}

/// Install a bundled Simplified Chinese font before first layout. System font files
/// cannot be relied upon in a distributed app, and eframe's default faces are Latin-only.
pub fn install_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let cjk = "pi-whim-cjk".to_owned();
    fonts
        .font_data
        .insert(cjk.clone(), Arc::new(FontData::from_static(CJK_FONT_BYTES)));
    // Keep the Pi-inspired Latin faces primary. The bundled CJK face is selected
    // only when those faces have no matching glyph.
    append_font(&mut fonts, FontFamily::Proportional, cjk.clone());
    append_font(&mut fonts, FontFamily::Monospace, cjk);
    context.set_fonts(fonts);
}

fn append_font(fonts: &mut FontDefinitions, family: FontFamily, font: String) {
    fonts.families.entry(family).or_default().push(font);
}

fn paint_paper_grid(painter: &egui::Painter, rect: egui::Rect) {
    draw_paper_grid(painter, rect, 9.0, 0.35, 11);
    draw_paper_grid(painter, rect, 36.0, 0.55, 27);
}

fn draw_paper_grid(painter: &egui::Painter, rect: egui::Rect, step: f32, width: f32, alpha: u8) {
    let start_x = (rect.left() / step).floor() * step;
    let start_y = (rect.top() / step).floor() * step;
    let stroke = Stroke::new(width, Color32::from_rgba_unmultiplied(95, 89, 82, alpha));
    let mut x = start_x;
    while x <= rect.right() {
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            stroke,
        );
        x += step;
    }
    let mut y = start_y;
    while y <= rect.bottom() {
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            stroke,
        );
        y += step;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_font_covers_simplified_chinese() {
        let context = egui::Context::default();
        install_fonts(&context);
        context.begin_pass(egui::RawInput::default());

        assert!(context.fonts(|fonts| {
            fonts.has_glyphs(&FontId::new(16.0, FontFamily::Proportional), "中文渲染项目")
        }));

        let _ = context.end_pass();
    }

    #[test]
    fn loaded_keyed_provider_is_reused_instead_of_creating_a_new_profile() {
        let missing_id = uuid::Uuid::new_v4();
        let keyed_id = uuid::Uuid::new_v4();
        let profile = |id, updated_at_ms, has_api_key| ProviderProfile {
            id,
            name: "OpenAI-compatible".into(),
            base_url: "https://gateway.example/v1".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("gpt-example")],
            updated_at_ms,
            has_api_key,
        };
        let mut workbench = Workbench::default();

        workbench.apply(Action::ProviderProfilesLoaded(vec![
            profile(missing_id, 20, false),
            profile(keyed_id, 10, true),
        ]));
        workbench.save_provider_intent();

        assert_eq!(workbench.provider_draft.id, Some(keyed_id));
        assert!(matches!(
            workbench.take_intents().as_slice(),
            [UiIntent::SaveProvider { profile, api_key: None }] if profile.id == keyed_id
        ));
    }
}
