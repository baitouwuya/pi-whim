//! Pi-inspired egui presentation, deliberately isolated from persistence and Pi RPC.

mod chat;
mod icons;
mod markdown;
mod settings;
mod slash_commands;

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::Instant,
};

use eframe::egui::{
    self, Align, Button, Checkbox, Color32, DragValue, FontData, FontDefinitions, FontFamily,
    FontId, Frame, Key, KeyboardShortcut, Layout, Margin, Modifiers, RichText, ScrollArea,
    SidePanel, Stroke, TextEdit, TopBottomPanel, Ui, Vec2,
};
use pi_whim_core::{
    Action, AgentTeamConfig, AppState, BashPolicy, ConversationItem, ConversationRole, Language,
    MAX_AGENT_DEPTH, MAX_AGENTS_PER_LEVEL, ModelOption, ProjectId, ProviderId, ProviderModel,
    ProviderProfile, ProviderProtocol, QueueMode, SearchEngineProfile, SessionStatus,
    ThinkingLevel, provider_name_key,
};
use pi_whim_engine::composer::Composer;

use markdown::MarkdownRenderer;

// Palette sampled from pi.dev's light theme: warm paper canvas, evening-blue
// ink, tidal-blue accents, terracotta for errors, sage/sunkissed for states.
pub const PAPER: Color32 = Color32::from_rgb(240, 237, 230);
pub const INK: Color32 = Color32::from_rgb(37, 47, 61);
pub const MUTED_INK: Color32 = Color32::from_rgb(117, 125, 137);
pub const LINE: Color32 = Color32::from_rgb(214, 209, 200);
pub const BLUE: Color32 = Color32::from_rgb(106, 159, 204);
pub const ACCENT_STRONG: Color32 = Color32::from_rgb(75, 96, 124);
pub const SIDEBAR: Color32 = Color32::from_rgb(236, 233, 226);
pub const CHAT_BACKGROUND: Color32 = Color32::from_rgb(250, 248, 244);
pub const USER_BUBBLE: Color32 = Color32::from_rgb(237, 240, 243);
pub const TOOL_BACKGROUND: Color32 = Color32::from_rgb(245, 243, 238);
pub const SUCCESS: Color32 = Color32::from_rgb(117, 138, 74);
pub const WARNING: Color32 = Color32::from_rgb(196, 143, 60);
pub const ERROR_RED: Color32 = Color32::from_rgb(184, 107, 82);
pub const ERROR_STRONG: Color32 = Color32::from_rgb(132, 79, 59);

fn composer_input_id() -> egui::Id {
    egui::Id::new("composer-input")
}

/// Semi-transparent tint of a palette color, used for banner and pill fills.
pub fn tint(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// Opaque blend of `color` over `base`. Top-level panels sit on eframe's dark
/// clear color, so translucent fills would read as mud there.
pub fn blend(base: Color32, color: Color32, amount: f32) -> Color32 {
    let t = amount.clamp(0.0, 1.0);
    Color32::from_rgb(
        (base.r() as f32 * (1.0 - t) + color.r() as f32 * t).round() as u8,
        (base.g() as f32 * (1.0 - t) + color.g() as f32 * t).round() as u8,
        (base.b() as f32 * (1.0 - t) + color.b() as f32 * t).round() as u8,
    )
}

const CHAT_CONTENT_WIDTH: f32 = 820.0;
const USER_MESSAGE_WIDTH: f32 = 620.0;
const SIDEBAR_WIDTH: f32 = 260.0;

const CJK_FONT_BYTES: &[u8] = include_bytes!("../assets/NotoSansCJKsc-Regular.otf");
const EMOJI_FONT_BYTES: &[u8] = include_bytes!("../assets/NotoEmoji.ttf");

#[derive(Clone, Debug)]
pub enum UiIntent {
    AddProject,
    RemoveProject(ProjectId),
    RevealProject(ProjectId),
    StartProject(ProjectId),
    StartNewSession(ProjectId),
    SwitchSession {
        project_id: ProjectId,
        path: String,
    },
    RenameSession {
        path: String,
        title: String,
    },
    CloneSession,
    ForkSession(String),
    DeleteSession(String),
    SetSessionName(String),
    ExportSession(Option<String>),
    ShareSession,
    AddFileAttachments,
    AddFolderAttachment,
    RemoveComposerAttachment(String),
    SubmitPrompt {
        content: String,
        attachments: Vec<pi_whim_core::Attachment>,
        mode: SubmitMode,
    },
    Compact,
    SetAutoCompaction(bool),
    Stop,
    SetLanguage(Language),
    SetBashPolicy(BashPolicy),
    SetBashBlockedPatterns(Vec<String>),
    SetAgentTeamConfig(AgentTeamConfig),
    SetModel(ModelOption),
    SetThinkingLevel(ThinkingLevel),
    SetQueueModes {
        steering: QueueMode,
        follow_up: QueueMode,
    },
    SaveProvider {
        profile: ProviderProfile,
        api_key: Option<String>,
    },
    DeleteProvider(ProviderId),
    SaveSearchEngines(Vec<SearchEngineProfile>),
    TestSearchEngine(SearchEngineProfile),
    DiscoverProviderModels {
        profile_id: Option<ProviderId>,
        provider_name: String,
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
    WebSearch,
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

#[derive(Clone, Debug)]
struct SearchEngineDraft {
    id: Option<pi_whim_core::SearchEngineId>,
    name: String,
    base_url: String,
    enabled: bool,
}

impl Default for SearchEngineDraft {
    fn default() -> Self {
        Self {
            id: None,
            name: "SearXNG".into(),
            base_url: String::new(),
            enabled: true,
        }
    }
}

impl SearchEngineDraft {
    fn from_profile(profile: &SearchEngineProfile) -> Self {
        Self {
            id: Some(profile.id),
            name: profile.name.clone(),
            base_url: profile.base_url.clone(),
            enabled: profile.enabled,
        }
    }

    fn to_profile(&self, position: u32) -> SearchEngineProfile {
        SearchEngineProfile {
            id: self.id.unwrap_or_else(uuid::Uuid::new_v4),
            name: self.name.trim().to_owned(),
            kind: pi_whim_core::SearchEngineKind::Searxng,
            base_url: self.base_url.trim().trim_end_matches('/').to_owned(),
            enabled: self.enabled,
            position,
        }
    }
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
    /// The prompt being drafted. View-local, so it is not in `state`.
    composer: Composer,
    intents: Vec<UiIntent>,
    page: WorkbenchPage,
    provider_draft: ProviderDraft,
    search_engine_draft: SearchEngineDraft,
    last_frame: Instant,
    rename_session_path: Option<String>,
    rename_session_title: String,
    expanded_projects: BTreeSet<ProjectId>,
    copied_message: Option<(String, Instant)>,
    composer_ime_composing: bool,
    slash_selection: usize,
    slash_query: Option<String>,
    slash_dismissed_query: Option<String>,
    model_search: String,
    bash_blocked_patterns_draft: Option<String>,
    dismissed_error: Option<String>,
    markdown: MarkdownRenderer,
    message_layouts: HashMap<String, CachedMessageLayout>,
}

#[derive(Clone, Copy)]
struct CachedMessageLayout {
    width_bits: u32,
    text_len: usize,
    text_marker: u64,
    revealed_graphemes: usize,
    attachments: usize,
    report_len: usize,
    details_len: usize,
    streaming: bool,
    height: f32,
}

impl CachedMessageLayout {
    fn for_message(message: &ConversationItem, width: f32) -> Self {
        Self {
            width_bits: width.to_bits(),
            text_len: message.full_text.len(),
            text_marker: text_marker(&message.full_text),
            revealed_graphemes: message.revealed_graphemes,
            attachments: message.attachments.len(),
            report_len: message.tool_report.as_deref().map_or(0, str::len),
            details_len: message.tool_details.as_deref().map_or(0, str::len),
            streaming: message.streaming,
            height: 0.0,
        }
    }

    fn matches(self, other: Self) -> bool {
        self.width_bits == other.width_bits
            && self.text_len == other.text_len
            && self.text_marker == other.text_marker
            && self.revealed_graphemes == other.revealed_graphemes
            && self.attachments == other.attachments
            && self.report_len == other.report_len
            && self.details_len == other.details_len
            && self.streaming == other.streaming
    }
}

fn text_marker(text: &str) -> u64 {
    let bytes = text.as_bytes();
    let mut marker = bytes.len() as u64;
    for byte in bytes.iter().take(16).chain(bytes.iter().rev().take(16)) {
        marker = marker
            .wrapping_mul(1_099_511_628_211)
            .wrapping_add(u64::from(*byte) + 1);
    }
    marker
}

impl Default for Workbench {
    fn default() -> Self {
        // Pi enables automatic compaction by default; refresh_runtime_controls
        // replaces this optimistic value with the session's actual setting.
        let state = AppState {
            auto_compaction_enabled: true,
            ..AppState::default()
        };
        Self {
            state,
            composer: Composer::new(),
            intents: Vec::new(),
            page: WorkbenchPage::Chat,
            provider_draft: ProviderDraft::default(),
            search_engine_draft: SearchEngineDraft::default(),
            last_frame: Instant::now(),
            rename_session_path: None,
            rename_session_title: String::new(),
            expanded_projects: BTreeSet::new(),
            copied_message: None,
            composer_ime_composing: false,
            slash_selection: 0,
            slash_query: None,
            slash_dismissed_query: None,
            model_search: String::new(),
            bash_blocked_patterns_draft: None,
            dismissed_error: None,
            markdown: MarkdownRenderer::default(),
            message_layouts: HashMap::new(),
        }
    }
}

impl Workbench {
    pub fn take_intents(&mut self) -> Vec<UiIntent> {
        std::mem::take(&mut self.intents)
    }

    pub fn composer_has_focus(&self, context: &egui::Context) -> bool {
        context.memory(|memory| memory.has_focus(composer_input_id()))
    }

    /// The prompt draft, for the app to stage attachments into.
    pub fn composer_draft(&self) -> &Composer {
        &self.composer
    }

    pub fn composer_draft_mut(&mut self) -> &mut Composer {
        &mut self.composer
    }

    /// Used by the ui_preview example to screenshot the settings page.
    #[doc(hidden)]
    pub fn preview_open_settings(&mut self, providers: bool) {
        self.page = WorkbenchPage::Settings(if providers {
            SettingsSection::Providers
        } else {
            SettingsSection::General
        });
    }

    pub fn apply(&mut self, action: Action) {
        match action {
            Action::ProviderProfilesLoaded(profiles) => {
                self.sync_provider_draft(&profiles);
                self.state
                    .dispatch(Action::ProviderProfilesLoaded(profiles));
            }
            Action::SearchEngineProfilesLoaded(profiles) => {
                self.sync_search_engine_draft(&profiles);
                self.state
                    .dispatch(Action::SearchEngineProfilesLoaded(profiles));
            }
            Action::ProjectsLoaded(projects) => {
                if self.state.projects.is_empty() && self.expanded_projects.is_empty() {
                    self.expanded_projects
                        .extend(projects.iter().map(|project| project.id));
                }
                self.state.dispatch(Action::ProjectsLoaded(projects));
            }
            Action::ClearConversation => {
                self.message_layouts.clear();
                self.state.dispatch(Action::ClearConversation);
            }
            action => self.state.dispatch(action),
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

    fn sync_search_engine_draft(&mut self, profiles: &[SearchEngineProfile]) {
        if let Some(id) = self.search_engine_draft.id
            && let Some(profile) = profiles.iter().find(|profile| profile.id == id)
        {
            self.search_engine_draft = SearchEngineDraft::from_profile(profile);
        } else if let Some(profile) = profiles.first() {
            self.search_engine_draft = SearchEngineDraft::from_profile(profile);
        } else {
            self.search_engine_draft = SearchEngineDraft::default();
        }
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

    fn submit_composer(&mut self) -> bool {
        if self.composer.is_empty() {
            return false;
        }
        let (content, attachments) = self.composer.take();
        self.intents.push(UiIntent::SubmitPrompt {
            content,
            attachments,
            mode: SubmitMode::Prompt,
        });
        true
    }

    fn activate_slash_command(
        &mut self,
        context: &egui::Context,
        command: slash_commands::SlashCommand,
    ) {
        use slash_commands::SlashCommand;

        match command {
            SlashCommand::NewSession => {
                if let Some(project_id) = self.state.selected_project {
                    self.intents.push(UiIntent::StartNewSession(project_id));
                }
                self.composer.clear_text();
            }
            SlashCommand::AddAttachment => {
                self.intents.push(UiIntent::AddFileAttachments);
                self.composer.clear_text();
            }
            SlashCommand::ChooseModel => self.composer.set_text("/model "),
            SlashCommand::ChooseThinkingLevel => self.composer.set_text("/thinking "),
            SlashCommand::SetModel(model) => {
                self.intents.push(UiIntent::SetModel(model));
                self.composer.clear_text();
            }
            SlashCommand::SetThinkingLevel(level) => {
                self.intents.push(UiIntent::SetThinkingLevel(level));
                self.composer.clear_text();
            }
            SlashCommand::Compact => {
                self.intents.push(UiIntent::Compact);
                self.composer.clear_text();
            }
            SlashCommand::CopyLastMessage => {
                if let Some(message) = self.state.conversation.iter().rev().find(|message| {
                    message.role == ConversationRole::Assistant
                        && !message.streaming
                        && !message.full_text.trim().is_empty()
                }) {
                    context.copy_text(message.full_text.clone());
                    self.copied_message = Some((message.id.clone(), Instant::now()));
                    self.composer.clear_text();
                }
            }
            SlashCommand::NameSession(name) => {
                if let Some(name) = name {
                    self.intents.push(UiIntent::SetSessionName(name));
                    self.composer.clear_text();
                } else {
                    self.composer.set_text("/name ");
                }
            }
            SlashCommand::Export(path) => {
                self.intents.push(UiIntent::ExportSession(path));
                self.composer.clear_text();
            }
            SlashCommand::Share => {
                self.intents.push(UiIntent::ShareSession);
                self.composer.clear_text();
            }
            SlashCommand::ShowSessionInfo => {
                let metrics = self.state.session_metrics.clone().unwrap_or_default();
                self.push_command_output(format!(
                    "Session info\n\nMessages: {} ({} user, {} assistant)\nTool calls: {}\nTokens: {}\nCost: ${:.4}",
                    metrics.total_messages,
                    metrics.user_messages,
                    metrics.assistant_messages,
                    metrics.tool_calls,
                    metrics.total_tokens,
                    metrics.cost_microusd as f64 / 1_000_000.0
                ));
                self.composer.clear_text();
            }
            SlashCommand::ShowHotkeys => {
                self.push_command_output(
                    "Keyboard shortcuts\n\nEnter: send\nShift+Enter: new line\n/: quick actions\nUp/Down: select action\nTab or Enter: confirm action\nEsc: close action menu or reveal streamed text".into(),
                );
                self.composer.clear_text();
            }
            SlashCommand::ShowChangelog => {
                self.push_command_output(
                    "Pi changelog: https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/CHANGELOG.md".into(),
                );
                self.composer.clear_text();
            }
            SlashCommand::ChooseFork => self.composer.set_text("/fork "),
            SlashCommand::Fork(entry_id) => {
                self.intents.push(UiIntent::ForkSession(entry_id));
                self.composer.clear_text();
            }
            SlashCommand::Clone => {
                self.intents.push(UiIntent::CloneSession);
                self.composer.clear_text();
            }
            SlashCommand::SubmitDynamic(command) => {
                self.composer.set_text(format!("/{command} "));
            }
            SlashCommand::Stop => {
                self.intents.push(UiIntent::Stop);
                self.composer.clear_text();
            }
        }
        self.slash_selection = 0;
        self.slash_query = None;
        self.slash_dismissed_query = None;
    }

    fn push_command_output(&mut self, text: String) {
        self.state
            .dispatch(Action::UpsertConversation(pi_whim_core::ConversationItem {
                id: format!("slash-command-{}", now_ms()),
                role: ConversationRole::System,
                full_text: text,
                revealed_graphemes: usize::MAX,
                reveal_credit: 0.0,
                streaming: false,
                tool_name: None,
                tool_report: None,
                tool_details: None,
                is_error: false,
                model: None,
                attachments: Vec::new(),
            }));
    }

    fn slash_toolbar(&mut self, context: &egui::Context) -> bool {
        let Some(options) = slash_commands::options(&self.state, self.composer.text()) else {
            self.slash_query = None;
            self.slash_dismissed_query = None;
            return false;
        };
        let query = self.composer.text().to_string();
        if self.slash_query.as_deref() != Some(&query) {
            self.slash_selection = 0;
            self.slash_query = Some(query.clone());
        }
        if self.slash_dismissed_query.as_deref() == Some(&query) {
            return false;
        }
        if options.is_empty() {
            return false;
        }

        self.slash_selection = self.slash_selection.min(options.len() - 1);
        let mut activate = None;
        let mut dismiss = false;
        let mut navigated = false;
        let pointer_moved = context.input(|input| input.pointer.delta() != Vec2::ZERO);
        {
            context.input_mut(|input| {
                if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                    self.slash_selection =
                        (self.slash_selection + options.len() - 1) % options.len();
                    navigated = true;
                }
                if input.consume_key(Modifiers::NONE, Key::ArrowDown) {
                    self.slash_selection = (self.slash_selection + 1) % options.len();
                    navigated = true;
                }
                if input.consume_key(Modifiers::NONE, Key::Enter)
                    || input.consume_key(Modifiers::NONE, Key::Tab)
                {
                    activate = Some(self.slash_selection);
                }
                if input.consume_key(Modifiers::NONE, Key::Escape) {
                    dismiss = true;
                }
            });
        }
        if dismiss {
            self.slash_dismissed_query = Some(query);
            return true;
        }
        if let Some(index) = activate {
            self.activate_slash_command(context, options[index].command.clone());
            return true;
        }

        let screen = context.screen_rect();
        // Anchor the menu's bottom edge just above the composer card, so it
        // grows upward and never overlaps the input, however tall it gets.
        // Center it over the chat column (the screen minus the sidebar), not
        // over the whole window.
        let chat_width = (screen.width() - SIDEBAR_WIDTH).max(0.0);
        let x_offset = (SIDEBAR_WIDTH + (chat_width - 700.0) * 0.5).max(12.0);
        egui::Area::new(egui::Id::new("slash-command-toolbar"))
            .order(egui::Order::Foreground)
            .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(x_offset, -200.0))
            .show(context, |ui| {
                Frame::popup(ui.style())
                    .fill(CHAT_BACKGROUND)
                    .stroke(Stroke::new(1.0_f32, LINE))
                    .corner_radius(0)
                    .shadow(egui::epaint::Shadow {
                        offset: [0, 8],
                        blur: 20,
                        spread: 0,
                        color: Color32::from_black_alpha(28),
                    })
                    .show(ui, |ui| {
                        ui.set_min_width(700.0_f32.min(screen.width() - 24.0));
                        ui.label(
                            RichText::new(tr(&self.state, "slash-commands"))
                                .font(mono_font(10.0))
                                .color(MUTED_INK),
                        );
                        ui.add_space(3.0);
                        ScrollArea::vertical().max_height(272.0).show(ui, |ui| {
                            for (index, option) in options.iter().enumerate() {
                                let selected = index == self.slash_selection;
                                let response = ui.allocate_response(
                                    Vec2::new(ui.available_width(), 48.0),
                                    egui::Sense::click(),
                                );
                                let rect = response.rect;
                                if response.hovered() && pointer_moved {
                                    self.slash_selection = index;
                                }
                                if selected && navigated {
                                    response.scroll_to_me(Some(Align::Center));
                                }
                                let response =
                                    response.on_hover_cursor(egui::CursorIcon::PointingHand);
                                if selected || response.hovered() {
                                    ui.painter().rect_filled(rect, 0, USER_BUBBLE);
                                }
                                let mut row = ui.new_child(
                                    egui::UiBuilder::new()
                                        .id_salt(("slash-command-row", index))
                                        .max_rect(rect.shrink2(Vec2::new(8.0, 4.0))),
                                );
                                row.horizontal(|ui| {
                                    ui.horizontal(|ui| {
                                        icons::display(
                                            ui,
                                            option.icon,
                                            Vec2::splat(20.0),
                                            MUTED_INK,
                                        );
                                        ui.add_space(4.0);
                                        ui.vertical(|ui| {
                                            ui.label(
                                                RichText::new(&option.title).size(15.0).color(INK),
                                            );
                                            ui.label(
                                                RichText::new(&option.detail)
                                                    .small()
                                                    .color(MUTED_INK),
                                            );
                                        });
                                        ui.with_layout(
                                            Layout::right_to_left(Align::Center),
                                            |ui| {
                                                ui.label(
                                                    RichText::new(&option.trigger)
                                                        .font(mono_font(10.0))
                                                        .color(MUTED_INK),
                                                );
                                            },
                                        );
                                    });
                                });
                                if response.clicked() {
                                    activate = Some(index);
                                }
                            }
                        });
                        ui.add_space(3.0);
                        ui.label(
                            RichText::new(tr(&self.state, "slash-help"))
                                .font(mono_font(10.0))
                                .color(MUTED_INK),
                        );
                    });
            });
        if let Some(index) = activate {
            self.activate_slash_command(context, options[index].command.clone());
        }
        true
    }

    pub fn show(&mut self, context: &egui::Context) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_frame).as_secs_f32();
        self.last_frame = now;
        if self.state.tick_typewriter(elapsed) {
            context.request_repaint_after(std::time::Duration::from_millis(33));
        }
        install_theme(context);
        self.top_bar(context);
        self.error_banner(context);
        self.compacting_banner(context);
        match self.page {
            WorkbenchPage::Chat => {
                self.sidebar(context);
                self.status_strip(context);
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
                    ui.add_space(10.0);
                    self.status_pill(ui);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icons::button(
                            ui,
                            icons::Icon::Settings,
                            tr(&self.state, "settings"),
                            Vec2::splat(28.0),
                            true,
                        )
                        .clicked()
                        {
                            self.page = WorkbenchPage::Settings(SettingsSection::General);
                        }
                        let language = match self.state.language {
                            Language::English => "中文",
                            Language::SimplifiedChinese => "EN",
                        };
                        if bracket_button(ui, language).clicked() {
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

    /// Codex-style status chip: a colored dot plus a mono label, tinted by the
    /// current agent state. Clicking a failed pill revives a dismissed banner.
    fn status_pill(&mut self, ui: &mut Ui) {
        let (color, label) = status_visual(&self.state);
        let busy = matches!(
            self.state.session_status,
            SessionStatus::Starting | SessionStatus::Streaming | SessionStatus::Compacting
        );
        let failed = matches!(self.state.session_status, SessionStatus::Failed(_));
        Frame::default()
            .fill(tint(color, 20))
            .stroke(Stroke::new(1.0_f32, tint(color, 110)))
            .corner_radius(0)
            .inner_margin(Margin::symmetric(10, 4))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), egui::Sense::hover());
                    let dot = if busy {
                        let phase = (ui.ctx().input(|input| input.time) * 2.4).sin() * 0.5 + 0.5;
                        tint(color, (110.0 + 145.0 * phase) as u8)
                    } else {
                        color
                    };
                    ui.painter().circle_filled(rect.center(), 3.5, dot);
                    let label = ui.label(RichText::new(label).font(mono_font(10.5)).color(color));
                    if failed && label.on_hover_text(tr(&self.state, "show-error")).clicked() {
                        self.dismissed_error = None;
                    }
                });
            });
    }

    /// Prominent request-failure banner under the top bar. The previous design
    /// only surfaced errors as a tiny "ERROR" label, easy to miss entirely.
    fn error_banner(&mut self, context: &egui::Context) {
        let error = match &self.state.session_status {
            SessionStatus::Failed(error) => error.clone(),
            _ => {
                self.dismissed_error = None;
                return;
            }
        };
        if self.dismissed_error.as_deref() == Some(error.as_str()) {
            return;
        }
        TopBottomPanel::top("error-banner")
            .frame(
                Frame::default()
                    .fill(blend(PAPER, ERROR_RED, 0.10))
                    .stroke(Stroke::new(1.0_f32, tint(ERROR_RED, 110)))
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    icons::display(ui, icons::Icon::Warning, Vec2::splat(20.0), ERROR_STRONG);
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new(tr(&self.state, "error-banner-title"))
                                .strong()
                                .color(ERROR_STRONG),
                        );
                        ui.label(RichText::new(&error).font(mono_font(11.0)).color(INK));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icons::button(
                            ui,
                            icons::Icon::Close,
                            tr(&self.state, "dismiss"),
                            Vec2::splat(26.0),
                            false,
                        )
                        .clicked()
                        {
                            self.dismissed_error = Some(error.clone());
                        }
                        if bracket_button(ui, tr(&self.state, "copy-error")).clicked() {
                            ui.ctx().copy_text(error.clone());
                        }
                    });
                });
            });
    }

    /// Context compaction hint: a slim accent banner shown while Pi summarizes
    /// older messages, so the pause never looks like a hang.
    fn compacting_banner(&mut self, context: &egui::Context) {
        if !matches!(self.state.session_status, SessionStatus::Compacting) {
            return;
        }
        TopBottomPanel::top("compacting-banner")
            .frame(
                Frame::default()
                    .fill(blend(PAPER, BLUE, 0.10))
                    .stroke(Stroke::new(1.0_f32, tint(BLUE, 100)))
                    .inner_margin(Margin::symmetric(16, 8)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    icons::display(ui, icons::Icon::Compress, Vec2::splat(18.0), ACCENT_STRONG);
                    ui.label(
                        RichText::new(tr(&self.state, "compacting-banner"))
                            .font(mono_font(11.0))
                            .strong()
                            .color(ACCENT_STRONG),
                    );
                    ui.label(
                        RichText::new(tr(&self.state, "compacting-detail"))
                            .small()
                            .color(MUTED_INK),
                    );
                });
            });
    }

    /// Pi-style terminal status line at the very bottom: current location on
    /// the left, spend / token usage / auto-compaction state on the right.
    fn status_strip(&mut self, context: &egui::Context) {
        TopBottomPanel::bottom("status-strip")
            .frame(
                Frame::default()
                    .fill(PAPER)
                    .stroke(Stroke::new(1.0_f32, LINE))
                    .inner_margin(Margin::symmetric(14, 4)),
            )
            .show(context, |ui| {
                // Mirror the top bar: a fixed-height, vertically centered row.
                // Without an explicit height the bottom panel mis-sizes its
                // content and the text slides half a line out of the window.
                ui.set_height(20.0);
                ui.horizontal(|ui| {
                    let mut location = self
                        .state
                        .selected_project
                        .and_then(|id| self.state.projects.iter().find(|project| project.id == id))
                        .map(|project| project.name.clone())
                        .unwrap_or_else(|| "pi-whim".into());
                    let session_title = self
                        .state
                        .selected_project
                        .and_then(|id| self.state.sessions.get(&id))
                        .and_then(|sessions| {
                            sessions
                                .iter()
                                .find(|session| Some(session.id) == self.state.selected_session)
                        })
                        .map(|session| session.title.clone());
                    if let Some(title) = session_title {
                        location = format!("{location} ({title})");
                    }
                    ui.label(
                        RichText::new(location)
                            .font(mono_font(10.0))
                            .color(MUTED_INK),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (auto_label, auto_color) = if self.state.auto_compaction_enabled {
                            (tr(&self.state, "auto-compact-on"), SUCCESS)
                        } else {
                            (tr(&self.state, "auto-compact-off"), MUTED_INK)
                        };
                        ui.label(
                            RichText::new(auto_label)
                                .font(mono_font(10.0))
                                .color(auto_color),
                        );
                        if let Some(metrics) = &self.state.session_metrics {
                            ui.label(
                                RichText::new(format!(
                                    "${:.4} · {} tok · {} msg",
                                    metrics.cost_microusd as f64 / 1_000_000.0,
                                    format_tokens(metrics.total_tokens),
                                    metrics.total_messages,
                                ))
                                .font(mono_font(10.0))
                                .color(BLUE),
                            );
                        }
                    });
                });
            });
    }

    fn sidebar(&mut self, context: &egui::Context) {
        SidePanel::left("sidebar")
            // A resizable sidebar shifts the centered chat column as its saved
            // width grows. Keep navigation stable so messages stay in place.
            .resizable(false)
            .exact_width(SIDEBAR_WIDTH)
            .frame(
                Frame::default()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::same(10))
                    .stroke(Stroke::new(1.0_f32, LINE)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(tr(&self.state, "projects"))
                            .font(serif_font(20.0))
                            .color(INK),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if icons::button(
                            ui,
                            icons::Icon::Plus,
                            tr(&self.state, "add-project"),
                            Vec2::splat(26.0),
                            false,
                        )
                        .clicked()
                        {
                            self.intents.push(UiIntent::AddProject);
                        }
                    });
                });
                ui.add_space(10.0);
                let search_hint = tr(&self.state, "search").to_owned();
                ui.add(
                    TextEdit::singleline(self.composer.search_mut())
                        .hint_text(search_hint)
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(10.0);
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
        if !self.composer.search().is_empty()
            && !project
                .name
                .to_lowercase()
                .contains(&self.composer.search().to_lowercase())
        {
            return;
        }
        let selected = self.state.selected_project == Some(project.id);
        let expanded = self.expanded_projects.contains(&project.id);
        let mut project_response = None;
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let name_width = (ui.available_width() - 26.0).max(40.0);
            let (row_rect, response) =
                ui.allocate_exact_size(Vec2::new(name_width, 30.0), egui::Sense::click());
            if response.hovered() {
                ui.painter().rect_filled(row_rect, 0, tint(INK, 9));
            }
            let chevron_rect = egui::Rect::from_min_size(
                row_rect.min + egui::vec2(3.0, 0.0),
                egui::vec2(20.0, 30.0),
            );
            let chevron_response = ui.interact(
                chevron_rect,
                ui.id().with(("project-chevron", project.id)),
                egui::Sense::click(),
            );
            icons::paint_icon(
                ui.painter(),
                chevron_rect.shrink(4.0),
                if expanded {
                    icons::Icon::ChevronDown
                } else {
                    icons::Icon::ChevronRight
                },
                MUTED_INK,
            );
            ui.painter().with_clip_rect(row_rect).text(
                row_rect.left_center() + egui::vec2(26.0, 0.0),
                egui::Align2::LEFT_CENTER,
                &project.name,
                serif_font(13.5),
                if selected { INK } else { MUTED_INK },
            );
            if chevron_response.clicked() {
                if expanded {
                    self.expanded_projects.remove(&project.id);
                } else {
                    self.expanded_projects.insert(project.id);
                }
            }
            if response.clicked() {
                self.expanded_projects.insert(project.id);
                self.state.dispatch(Action::SelectProject(project.id));
                self.intents.push(UiIntent::StartProject(project.id));
            }
            project_response = Some(response);
            if icons::button(
                ui,
                icons::Icon::Plus,
                tr(&self.state, "new-session"),
                Vec2::splat(26.0),
                false,
            )
            .clicked()
            {
                self.expanded_projects.insert(project.id);
                self.state.dispatch(Action::SelectProject(project.id));
                self.intents.push(UiIntent::StartNewSession(project.id));
            }
        });
        project_response
            .expect("project row response")
            .context_menu(|ui| {
                if ui.button(tr(&self.state, "show-finder")).clicked() {
                    self.intents.push(UiIntent::RevealProject(project.id));
                    ui.close_menu();
                }
                if ui.button(tr(&self.state, "remove")).clicked() {
                    self.intents.push(UiIntent::RemoveProject(project.id));
                    ui.close_menu();
                }
            });
        if self.expanded_projects.contains(&project.id) {
            let sessions = self
                .state
                .sessions
                .get(&project.id)
                .cloned()
                .unwrap_or_default();
            for session in sessions {
                let selected_session = self.state.selected_session == Some(session.id);
                let mut session_response = None;
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.add_space(16.0);
                    let (row_rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width().max(32.0), 26.0),
                        egui::Sense::click(),
                    );
                    if selected_session {
                        ui.painter()
                            .rect_filled(row_rect, 0, tint(ACCENT_STRONG, 22));
                    } else if response.hovered() {
                        ui.painter().rect_filled(row_rect, 0, tint(INK, 9));
                    }
                    let title: &str = if session.title.is_empty() {
                        tr(&self.state, "new-session")
                    } else {
                        &session.title
                    };
                    let session_running = self.state.running_sessions.contains(&session.pi_path);
                    if session_running {
                        ui.painter().circle_filled(
                            row_rect.left_center() + egui::vec2(6.0, 0.0),
                            3.0,
                            ACCENT_STRONG,
                        );
                    }
                    ui.painter().with_clip_rect(row_rect).text(
                        row_rect.left_center()
                            + egui::vec2(if session_running { 16.0 } else { 10.0 }, 0.0),
                        egui::Align2::LEFT_CENTER,
                        title,
                        serif_font(13.0),
                        if selected_session { INK } else { MUTED_INK },
                    );
                    session_response = Some(response);
                });
                let response = session_response.expect("session row response");
                if response.clicked() {
                    self.state.dispatch(Action::SelectSession(session.id));
                    self.intents.push(UiIntent::SwitchSession {
                        project_id: project.id,
                        path: session.pi_path.clone(),
                    });
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
                    if ui.button(tr(&self.state, "copy-session-id")).clicked() {
                        ui.ctx().copy_text(session.id.to_string());
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
                    .fill(CHAT_BACKGROUND)
                    .inner_margin(Margin::symmetric(24, 12)),
            )
            .show(context, |ui| {
                paint_graph_paper(ui.painter(), ui.clip_rect());
                let header_width = ui.available_width().min(CHAT_CONTENT_WIDTH);
                let header_padding = ((ui.available_width() - header_width) / 2.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(header_padding);
                    ui.allocate_ui_with_layout(
                        Vec2::new(header_width, 20.0),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            let project = self
                                .state
                                .selected_project
                                .and_then(|id| {
                                    self.state.projects.iter().find(|project| project.id == id)
                                })
                                .map(|project| project.name.as_str())
                                .unwrap_or("pi-whim");
                            ui.label(
                                RichText::new(project)
                                    .font(mono_font(11.0))
                                    .color(ACCENT_STRONG),
                            );
                            let session_title = self
                                .state
                                .selected_project
                                .and_then(|id| self.state.sessions.get(&id))
                                .and_then(|sessions| {
                                    sessions.iter().find(|session| {
                                        Some(session.id) == self.state.selected_session
                                    })
                                })
                                .map(|session| session.title.as_str());
                            if let Some(title) = session_title {
                                ui.label(
                                    RichText::new("/")
                                        .font(mono_font(11.0))
                                        .color(tint(MUTED_INK, 140)),
                                );
                                ui.label(
                                    RichText::new(title).font(mono_font(11.0)).color(MUTED_INK),
                                );
                            }
                        },
                    );
                });
                ui.add_space(6.0);
                ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .auto_shrink([false; 2])
                    .show_viewport(ui, |ui, viewport| {
                        ui.set_width(ui.clip_rect().width());
                        if self.state.conversation.is_empty() {
                            ui.add_space(120.0);
                            // A single centered column: icon, heading, detail
                            // and the hint chips all share one vertical axis.
                            let hero_width = ui.clip_rect().width().min(CHAT_CONTENT_WIDTH);
                            let hero_padding =
                                ((ui.clip_rect().width() - hero_width) / 2.0).max(0.0);
                            ui.horizontal(|ui| {
                                ui.add_space(hero_padding);
                                ui.allocate_ui_with_layout(
                                    Vec2::new(hero_width, 280.0),
                                    Layout::top_down(Align::Center),
                                    |ui| {
                                        icons::display(
                                            ui,
                                            icons::Icon::Message,
                                            Vec2::splat(38.0),
                                            tint(ACCENT_STRONG, 160),
                                        );
                                        ui.add_space(10.0);
                                        ui.label(
                                            RichText::new(tr(&self.state, "empty-heading"))
                                                .font(serif_font(34.0))
                                                .color(INK),
                                        );
                                        ui.add_space(4.0);
                                        ui.label(
                                            RichText::new(tr(&self.state, "empty-detail"))
                                                .color(MUTED_INK),
                                        );
                                        ui.add_space(20.0);
                                        ui.horizontal(|ui| {
                                            ui.spacing_mut().item_spacing.x = 8.0;
                                            for hint in [
                                                tr(&self.state, "hint-slash"),
                                                tr(&self.state, "hint-enter"),
                                                tr(&self.state, "hint-shift-enter"),
                                            ] {
                                                Frame::default()
                                                    .fill(tint(ACCENT_STRONG, 10))
                                                    .stroke(Stroke::new(1.0_f32, LINE))
                                                    .corner_radius(0)
                                                    .inner_margin(Margin::symmetric(10, 4))
                                                    .show(ui, |ui| {
                                                        ui.label(
                                                            RichText::new(hint)
                                                                .font(mono_font(10.0))
                                                                .color(MUTED_INK),
                                                        );
                                                    });
                                            }
                                        });
                                    },
                                );
                            });
                        }
                        ui.spacing_mut().item_spacing.y = 4.0;
                        // Strict vertical rhythm: tool cards pack tightly,
                        // prose gets air. Tool-to-tool stays at the 4pt item
                        // spacing; earlier every boundary cost 14pt + 8pt.
                        let message_width = ui.clip_rect().width().min(CHAT_CONTENT_WIDTH);
                        let content_origin = ui.max_rect().top();
                        let mut previous_role: Option<ConversationRole> = None;
                        for message_index in 0..self.state.conversation.len() {
                            let (message_id, role, cache_key) = {
                                let message = &self.state.conversation[message_index];
                                (
                                    message.id.clone(),
                                    message.role.clone(),
                                    CachedMessageLayout::for_message(message, message_width),
                                )
                            };
                            let gap = match (previous_role.as_ref(), &role) {
                                (None, _) => 0.0,
                                (Some(ConversationRole::Tool), ConversationRole::Tool) => 0.0,
                                (Some(_), ConversationRole::Tool) => 4.0,
                                (Some(ConversationRole::Tool), _) => 6.0,
                                _ => 12.0,
                            };
                            ui.add_space(gap);
                            let cached = self
                                .message_layouts
                                .get(&message_id)
                                .copied()
                                .filter(|cached| cached.matches(cache_key));
                            let top = ui.cursor().top();
                            let relative_top = top - content_origin;
                            let visible = cached.is_none_or(|cached| {
                                let bottom = relative_top + cached.height;
                                bottom >= viewport.min.y - viewport.height()
                                    && relative_top <= viewport.max.y + viewport.height()
                            });
                            if visible || cached.is_none() {
                                self.message_card(ui, message_index);
                                let height = (ui.cursor().top() - top).max(0.0);
                                self.message_layouts.insert(
                                    message_id,
                                    CachedMessageLayout {
                                        height,
                                        ..cache_key
                                    },
                                );
                            } else if let Some(cached) = cached {
                                ui.add_space(cached.height);
                            }
                            previous_role = Some(role);
                        }
                        if matches!(self.state.session_status, SessionStatus::Streaming) {
                            self.thinking_indicator(ui);
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
                .pending_model
                .clone()
                .or_else(|| self.state.current_model.clone())
                .map(|model| model.name.clone())
                .unwrap_or_else(|| "Model".into());
            egui::ComboBox::from_id_salt("model-picker")
                .width(190.0)
                .selected_text(current)
                .show_ui(ui, |ui| {
                    ui.set_min_width(340.0);
                    ui.add(
                        TextEdit::singleline(&mut self.model_search)
                            .hint_text(tr(&self.state, "search-models"))
                            .desired_width(f32::INFINITY),
                    );
                    let query = self.model_search.trim().to_lowercase();
                    let mut groups = BTreeMap::<String, Vec<ModelOption>>::new();
                    for model in self.state.available_models.clone() {
                        if !query.is_empty()
                            && !model.name.to_lowercase().contains(&query)
                            && !model.id.to_lowercase().contains(&query)
                            && !model.provider_name.to_lowercase().contains(&query)
                        {
                            continue;
                        }
                        groups
                            .entry(model.provider_name.clone())
                            .or_default()
                            .push(model);
                    }
                    ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                        for (provider_name, models) in groups {
                            ui.add_space(5.0);
                            ui.label(
                                RichText::new(provider_name)
                                    .font(mono_font(10.0))
                                    .strong()
                                    .color(MUTED_INK),
                            );
                            for model in models {
                                let active = self
                                    .state
                                    .pending_model
                                    .as_ref()
                                    .or(self.state.current_model.as_ref());
                                let selected = active.is_some_and(|current| {
                                    current.provider == model.provider && current.id == model.id
                                });
                                if ui
                                    .selectable_label(
                                        selected,
                                        RichText::new(&model.name).size(13.0),
                                    )
                                    .clicked()
                                {
                                    self.intents.push(UiIntent::SetModel(model.clone()));
                                }
                                if model.name != model.id {
                                    ui.label(
                                        RichText::new(&model.id)
                                            .font(mono_font(9.5))
                                            .color(MUTED_INK),
                                    );
                                }
                            }
                        }
                    });
                });
        } else if let SessionStatus::Failed(error) = &self.state.session_status {
            ui.label(
                RichText::new(format!(
                    "{}: {error}",
                    tr(&self.state, "models-unavailable")
                ))
                .font(mono_font(10.0))
                .color(ERROR_STRONG),
            );
        } else {
            ui.label(
                RichText::new(tr(&self.state, "models-unavailable"))
                    .font(mono_font(10.0))
                    .color(MUTED_INK),
            );
        }
        if !self.state.available_thinking_levels.is_empty() {
            let mut level = self.state.thinking_level;
            egui::ComboBox::from_id_salt("thinking-picker")
                .width(92.0)
                .selected_text(format!("{}: {level}", tr(&self.state, "thinking")))
                .show_ui(ui, |ui| {
                    for candidate in &self.state.available_thinking_levels {
                        ui.selectable_value(&mut level, *candidate, candidate.as_str());
                    }
                });
            if level != self.state.thinking_level {
                self.intents.push(UiIntent::SetThinkingLevel(level));
            }
        }
    }

    fn message_card(&mut self, ui: &mut Ui, message_index: usize) {
        let Some(message) = self.state.conversation.get(message_index).cloned() else {
            return;
        };
        let viewport_width = ui.clip_rect().width();
        let message_width = viewport_width.min(CHAT_CONTENT_WIDTH);
        let message_padding = ((viewport_width - message_width) / 2.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(message_padding);
            ui.allocate_ui_with_layout(
                Vec2::new(message_width, 0.0),
                Layout::top_down(Align::Min),
                |ui| {
                    let content = message.text_for_display();
                    match message.role {
                        ConversationRole::User => {
                            ui.with_layout(Layout::right_to_left(Align::TOP), |ui| {
                                chat::message_frame(ConversationRole::User).show(ui, |ui| {
                                    ui.set_max_width(USER_MESSAGE_WIDTH.min(ui.available_width()));
                                    ui.label(
                                        RichText::new(content).font(serif_font(16.0)).color(INK),
                                    );
                                    sent_attachment_cards(ui, &message.attachments);
                                });
                            });
                            // Action row sits under the bubble, not inside it:
                            // an in-bubble horizontal row overlaps the text in
                            // a right-to-left layout.
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button(tr(&self.state, "fork-here")).clicked() {
                                    self.intents.push(UiIntent::ForkSession(message.id.clone()));
                                }
                            });
                        }
                        ConversationRole::Assistant => {
                            // Render markdown progressively while streaming so
                            // headings, lists, tables, and thinking blocks take
                            // shape live instead of appearing as raw text.
                            if !content.trim().is_empty() {
                                self.markdown.show(ui, &message.id, content);
                            }
                            if !message.streaming && !message.full_text.trim().is_empty() {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    let copied =
                                        self.copied_message.as_ref().is_some_and(|(id, at)| {
                                            id == &message.id
                                                && chat::copy_feedback_active(*at, Instant::now())
                                        });
                                    if icons::button(
                                        ui,
                                        if copied {
                                            icons::Icon::Check
                                        } else {
                                            icons::Icon::Copy
                                        },
                                        tr(&self.state, "copy-report"),
                                        Vec2::new(28.0, 26.0),
                                        false,
                                    )
                                    .clicked()
                                    {
                                        ui.ctx().copy_text(message.full_text.clone());
                                        self.copied_message =
                                            Some((message.id.clone(), Instant::now()));
                                    }
                                });
                            }
                        }
                        ConversationRole::Tool => {
                            let (card_fill, card_line, tool_color) = if message.is_error {
                                (tint(ERROR_RED, 12), tint(ERROR_RED, 110), ERROR_RED)
                            } else {
                                (TOOL_BACKGROUND, LINE, ACCENT_STRONG)
                            };
                            Frame::default()
                                .fill(card_fill)
                                .stroke(Stroke::new(1.0_f32, card_line))
                                .corner_radius(0)
                                .inner_margin(Margin::symmetric(10, 6))
                                .show(ui, |ui| {
                                    let tool_name = message.tool_name.as_deref().unwrap_or("Tool");
                                    ui.horizontal(|ui| {
                                        icons::display(
                                            ui,
                                            if message.is_error {
                                                icons::Icon::Warning
                                            } else {
                                                icons::Icon::Settings
                                            },
                                            Vec2::splat(18.0),
                                            tool_color,
                                        );
                                        let header =
                                            RichText::new(format!("{tool_name} · {content}"))
                                                .font(mono_font(11.0))
                                                .color(if message.is_error {
                                                    ERROR_STRONG
                                                } else {
                                                    INK
                                                });
                                        ui.push_id(("tool-details", &message.id), |ui| {
                                            ui.collapsing(header, |ui| {
                                                if let Some(report) = message.tool_report.as_deref()
                                                {
                                                    ui.label(
                                                        RichText::new(report)
                                                            .font(mono_font(11.0))
                                                            .color(INK),
                                                    );
                                                }
                                                if let Some(details) =
                                                    message.tool_details.as_deref()
                                                {
                                                    ui.push_id("raw-tool-details", |ui| {
                                                        ui.collapsing(
                                                            tr(&self.state, "raw-tool-details"),
                                                            |ui| {
                                                                ui.label(
                                                                    RichText::new(details)
                                                                        .font(mono_font(11.0)),
                                                                );
                                                            },
                                                        );
                                                    });
                                                }
                                            });
                                        });
                                    });
                                });
                        }
                        ConversationRole::System => {
                            ui.label(
                                RichText::new(content)
                                    .font(mono_font(11.0))
                                    .color(MUTED_INK),
                            );
                        }
                    }
                    if message.streaming && ui.small_button(tr(&self.state, "show-all")).clicked() {
                        self.state.dispatch(Action::SkipTypewriter(message.id));
                    }
                },
            );
        });
    }

    /// Codex-style "thinking" row at the end of the stream: three pulsing dots
    /// plus a mono label, so a silent agent never looks frozen.
    fn thinking_indicator(&mut self, ui: &mut Ui) {
        ui.add_space(8.0);
        let viewport_width = ui.clip_rect().width();
        let width = viewport_width.min(CHAT_CONTENT_WIDTH);
        let padding = ((viewport_width - width) / 2.0).max(0.0);
        ui.horizontal(|ui| {
            ui.add_space(padding);
            let time = ui.ctx().input(|input| input.time);
            for index in 0..3 {
                let phase = ((time * 2.2 + index as f64 * 0.35).sin() * 0.5 + 0.5) as f32;
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
                ui.painter().circle_filled(
                    rect.center(),
                    2.6,
                    tint(ACCENT_STRONG, (70.0 + 160.0 * phase) as u8),
                );
            }
            ui.label(
                RichText::new(tr(&self.state, "generating"))
                    .font(mono_font(10.0))
                    .color(MUTED_INK),
            );
        });
    }

    fn composer(&mut self, context: &egui::Context) {
        TopBottomPanel::bottom("composer")
            .frame(
                Frame::default()
                    .fill(PAPER)
                    .inner_margin(Margin::symmetric(16, 10)),
            )
            .show(context, |ui| {
                let project_selected = self.state.selected_project.is_some();
                if !project_selected {
                    ui.horizontal(|ui| {
                        ui.add_space(composer_padding(ui));
                        ui.label(
                            RichText::new(tr(&self.state, "select-project-to-chat"))
                                .font(mono_font(11.0))
                                .color(MUTED_INK),
                        );
                    });
                }
                let card_width = ui.available_width().clamp(120.0, CHAT_CONTENT_WIDTH);
                let leading = composer_padding(ui);
                ui.horizontal(|ui| {
                    ui.add_space(leading);
                    ui.allocate_ui_with_layout(
                        Vec2::new(card_width, 0.0),
                        Layout::top_down(Align::Min),
                        |ui| {
                            ui.set_width(card_width);
                            Frame::default()
                                .fill(Color32::WHITE)
                                .stroke(Stroke::new(1.0_f32, LINE))
                                .corner_radius(0)
                                .inner_margin(Margin::symmetric(12, 8))
                                .show(ui, |ui| {
                                    ui.add_enabled_ui(project_selected, |ui| {
                                        self.composer_chips(ui);
                                        let ime_guard = chat::update_ime_composition(
                                            &mut self.composer_ime_composing,
                                            &context.input(|input| input.events.clone()),
                                        );
                                        let composer_id = composer_input_id();
                                        let composer_hint =
                                            tr(&self.state, "composer-placeholder").to_owned();
                                        // Consume toolbar navigation before TextEdit can turn Tab into focus traversal.
                                        let slash_open = self.slash_toolbar(context);
                                        let input = ui.add(
                                            TextEdit::multiline(self.composer.text_mut())
                                                .id(composer_id)
                                                .hint_text(composer_hint)
                                                .frame(false)
                                                .desired_rows(3)
                                                .return_key(KeyboardShortcut::new(
                                                    Modifiers::SHIFT,
                                                    Key::Enter,
                                                ))
                                                .desired_width(f32::INFINITY),
                                        );
                                        ui.add_space(2.0);
                                        ui.horizontal(|ui| {
                                            let attachment_menu = ui.menu_button(
                                                RichText::new("+").font(mono_font(18.0)),
                                                |ui| {
                                                    if ui
                                                        .button(tr(&self.state, "choose-files"))
                                                        .clicked()
                                                    {
                                                        self.intents
                                                            .push(UiIntent::AddFileAttachments);
                                                        ui.close_menu();
                                                    }
                                                    if ui
                                                        .button(tr(&self.state, "choose-folder"))
                                                        .clicked()
                                                    {
                                                        self.intents
                                                            .push(UiIntent::AddFolderAttachment);
                                                        ui.close_menu();
                                                    }
                                                },
                                            );
                                            attachment_menu
                                                .response
                                                .on_hover_text(tr(&self.state, "add-attachment"));
                                            self.runtime_controls(ui);
                                            ui.with_layout(
                                                Layout::right_to_left(Align::Center),
                                                |ui| {
                                                    if matches!(
                                                        self.state.session_status,
                                                        SessionStatus::Streaming
                                                            | SessionStatus::Compacting
                                                    ) && ui
                                                        .add_sized(
                                                            [72.0, 30.0],
                                                            Button::new(
                                                                RichText::new(tr(
                                                                    &self.state,
                                                                    "stop",
                                                                ))
                                                                .font(mono_font(11.0))
                                                                .color(Color32::WHITE),
                                                            )
                                                            .fill(ERROR_RED)
                                                            .corner_radius(0),
                                                        )
                                                        .clicked()
                                                    {
                                                        self.intents.push(UiIntent::Stop);
                                                    }
                                                    let submit = icons::filled_button(
                                                        ui,
                                                        icons::Icon::Send,
                                                        tr(&self.state, "send"),
                                                        Vec2::new(46.0, 30.0),
                                                        ACCENT_STRONG,
                                                    );
                                                    let enter_pressed = !slash_open
                                                        && context.input(|input| {
                                                            input.key_pressed(Key::Enter)
                                                                && !input.modifiers.shift
                                                        });
                                                    if (submit.clicked()
                                                        || chat::should_submit_from_keyboard(
                                                            input.has_focus(),
                                                            enter_pressed,
                                                            ime_guard,
                                                        ))
                                                        && self.submit_composer()
                                                    {
                                                        input.request_focus();
                                                    }
                                                    if input.has_focus()
                                                        && context.input(|input| {
                                                            input.key_pressed(egui::Key::Escape)
                                                        })
                                                    {
                                                        for message in &mut self.state.conversation
                                                        {
                                                            message.reveal_all();
                                                        }
                                                    }
                                                },
                                            );
                                        });
                                    });
                                });
                        },
                    );
                });
            });
    }

    /// Attachment and queue chips rendered inside the composer card, so pending
    /// context is visible where the user is typing.
    fn composer_chips(&mut self, ui: &mut Ui) {
        let has_attachments = !self.composer.attachments().is_empty();
        let steering = self.state.pending_steering.len();
        let follow_ups = self.state.pending_follow_up.len();
        if !has_attachments && steering == 0 && follow_ups == 0 {
            return;
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(6.0, 4.0);
            let attachments = self.composer.attachments().to_vec();
            for attachment in attachments {
                composer_attachment_card(ui, &attachment, &mut self.intents);
            }
            if steering > 0 {
                chip(
                    ui,
                    &format!("{} {}", tr(&self.state, "queued"), steering),
                    WARNING,
                );
            }
            if follow_ups > 0 {
                chip(
                    ui,
                    &format!("{} {}", tr(&self.state, "follow-ups"), follow_ups),
                    BLUE,
                );
            }
        });
        ui.add_space(2.0);
    }

    fn settings_page(&mut self, context: &egui::Context, section: SettingsSection) {
        SidePanel::left("settings-navigation")
            .exact_width(236.0)
            .resizable(false)
            .frame(
                Frame::default()
                    .fill(SIDEBAR)
                    .inner_margin(Margin::symmetric(14, 18))
                    .stroke(Stroke::new(1.0_f32, LINE)),
            )
            .show(context, |ui| {
                if icons::button(
                    ui,
                    icons::Icon::Back,
                    tr(&self.state, "back"),
                    Vec2::splat(30.0),
                    false,
                )
                .clicked()
                {
                    self.page = WorkbenchPage::Chat;
                }
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    ui.add_space(10.0);
                    ui.label(
                        RichText::new(tr(&self.state, "settings"))
                            .font(serif_font(22.0))
                            .color(INK),
                    );
                });
                ui.add_space(14.0);
                for (target, label) in [
                    (SettingsSection::General, tr(&self.state, "general")),
                    (SettingsSection::Providers, tr(&self.state, "providers")),
                    (SettingsSection::WebSearch, tr(&self.state, "web-search")),
                ] {
                    let (row_rect, response) = ui.allocate_exact_size(
                        Vec2::new(ui.available_width(), 32.0),
                        egui::Sense::click(),
                    );
                    let active = section == target;
                    if active {
                        ui.painter()
                            .rect_filled(row_rect, 0, tint(ACCENT_STRONG, 22));
                    } else if response.hovered() {
                        ui.painter().rect_filled(row_rect, 0, tint(INK, 9));
                    }
                    ui.painter().text(
                        row_rect.left_center() + egui::vec2(10.0, 0.0),
                        egui::Align2::LEFT_CENTER,
                        label,
                        serif_font(13.5),
                        if active { INK } else { MUTED_INK },
                    );
                    if response.clicked() {
                        self.page = WorkbenchPage::Settings(target);
                    }
                }
            });
        egui::CentralPanel::default()
            .frame(
                Frame::default()
                    .fill(CHAT_BACKGROUND)
                    .inner_margin(Margin::symmetric(0, 26)),
            )
            .show(context, |ui| {
                paint_graph_paper(ui.painter(), ui.clip_rect());
                ScrollArea::vertical()
                    .auto_shrink([false; 2])
                    .show(ui, |ui| {
                        settings::content(ui, |ui| match section {
                            SettingsSection::General => self.general_settings(ui),
                            SettingsSection::Providers => self.provider_settings(ui),
                            SettingsSection::WebSearch => self.web_search_settings(ui),
                        });
                    });
            });
    }

    fn general_settings(&mut self, ui: &mut Ui) {
        settings::page_header(ui, tr(&self.state, "general"), None);

        settings::section_header(ui, tr(&self.state, "appearance"), None);
        settings::form_row(ui, tr(&self.state, "language"), None, |ui| {
            let previous_language = self.state.language;
            settings::segmented(
                ui,
                &mut self.state.language,
                &[
                    (Language::English, "English"),
                    (Language::SimplifiedChinese, "简体中文"),
                ],
            );
            if self.state.language != previous_language {
                self.intents
                    .push(UiIntent::SetLanguage(self.state.language));
            }
        });

        settings::section_header(
            ui,
            tr(&self.state, "context"),
            Some(tr(&self.state, "context-help")),
        );
        settings::form_row(
            ui,
            tr(&self.state, "auto-compaction"),
            Some(tr(&self.state, "auto-compaction-help")),
            |ui| {
                let mut enabled = self.state.auto_compaction_enabled;
                if ui
                    .add_sized(
                        [settings::control_width(ui), settings::control_height()],
                        Checkbox::new(&mut enabled, tr(&self.state, "auto-compaction")),
                    )
                    .changed()
                {
                    self.intents.push(UiIntent::SetAutoCompaction(enabled));
                }
            },
        );

        settings::section_header(
            ui,
            tr(&self.state, "bash-policy"),
            Some(tr(&self.state, "bash-help")),
        );
        settings::form_row(ui, tr(&self.state, "command-policy"), None, |ui| {
            let mut policy = self.state.bash_policy;
            settings::segmented(
                ui,
                &mut policy,
                &[
                    (BashPolicy::Allow, tr(&self.state, "allow")),
                    (BashPolicy::Ask, tr(&self.state, "ask")),
                    (BashPolicy::Deny, tr(&self.state, "deny")),
                ],
            );
            if policy != self.state.bash_policy {
                self.state.dispatch(Action::SetBashPolicy(policy));
                self.intents.push(UiIntent::SetBashPolicy(policy));
            }
        });
        settings::form_row(
            ui,
            tr(&self.state, "blocked-patterns"),
            Some(tr(&self.state, "blocked-patterns-help")),
            |ui| {
                let blocked_patterns = self
                    .bash_blocked_patterns_draft
                    .get_or_insert_with(|| self.state.bash_blocked_patterns.join("\n"));
                ui.add_sized(
                    [settings::control_width(ui), 96.0],
                    TextEdit::multiline(blocked_patterns)
                        .hint_text("rm -rf\ncurl | sh")
                        .desired_rows(4)
                        .desired_width(settings::control_width(ui)),
                );
                ui.add_space(6.0);
                if settings::action_button(ui, tr(&self.state, "apply-command-filters")).clicked() {
                    self.intents.push(UiIntent::SetBashBlockedPatterns(
                        blocked_patterns.lines().map(str::to_owned).collect(),
                    ));
                }
            },
        );

        settings::section_header(
            ui,
            tr(&self.state, "agent-team"),
            Some(tr(&self.state, "agent-team-help")),
        );
        let mut team_config = self.state.agent_team_config.clone();
        settings::form_row(ui, tr(&self.state, "max-agent-depth"), None, |ui| {
            settings::compact_control(
                ui,
                DragValue::new(&mut team_config.max_depth).range(1..=MAX_AGENT_DEPTH),
            );
        });
        settings::form_row(ui, tr(&self.state, "max-agents-per-level"), None, |ui| {
            settings::compact_control(
                ui,
                DragValue::new(&mut team_config.max_agents_per_level)
                    .range(1..=MAX_AGENTS_PER_LEVEL),
            );
        });
        team_config = team_config.normalized();
        if team_config != self.state.agent_team_config {
            self.state
                .dispatch(Action::SetAgentTeamConfig(team_config.clone()));
            self.intents.push(UiIntent::SetAgentTeamConfig(team_config));
        }

        settings::section_header(ui, tr(&self.state, "queue-mode"), None);
        let mut steering_mode = self.state.steering_mode;
        let mut follow_up_mode = self.state.follow_up_mode;
        settings::form_row(ui, tr(&self.state, "steer-mode"), None, |ui| {
            settings::segmented(
                ui,
                &mut steering_mode,
                &[
                    (QueueMode::OneAtATime, tr(&self.state, "one-at-a-time")),
                    (QueueMode::All, tr(&self.state, "all")),
                ],
            );
        });
        settings::form_row(ui, tr(&self.state, "follow-up-mode"), None, |ui| {
            settings::segmented(
                ui,
                &mut follow_up_mode,
                &[
                    (QueueMode::OneAtATime, tr(&self.state, "one-at-a-time")),
                    (QueueMode::All, tr(&self.state, "all")),
                ],
            );
        });
        if steering_mode != self.state.steering_mode || follow_up_mode != self.state.follow_up_mode
        {
            self.intents.push(UiIntent::SetQueueModes {
                steering: steering_mode,
                follow_up: follow_up_mode,
            });
        }
        ui.add_space(24.0);
    }

    fn provider_settings(&mut self, ui: &mut Ui) {
        settings::page_header(
            ui,
            tr(&self.state, "providers"),
            Some(tr(&self.state, "providers-help")),
        );

        settings::section_header(ui, tr(&self.state, "configured-providers"), None);
        settings::control_row(ui, |ui| {
            let selector_width = settings::inline_leading_width(ui, settings::control_height());
            let selected_name = self
                .provider_draft
                .id
                .and_then(|id| {
                    self.state
                        .provider_profiles
                        .iter()
                        .find(|profile| profile.id == id)
                })
                .map(|profile| profile.name.as_str())
                .unwrap_or_else(|| tr(&self.state, "add-provider"));
            let mut selected_profile = None;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = settings::inline_gap();
                egui::ComboBox::from_id_salt("configured-provider")
                    .width(selector_width)
                    .selected_text(selected_name)
                    .show_ui(ui, |ui| {
                        for profile in self.state.provider_profiles.clone() {
                            let selected = self.provider_draft.id == Some(profile.id);
                            if ui.selectable_label(selected, &profile.name).clicked() {
                                selected_profile = Some(profile);
                            }
                        }
                    });
                if icons::button(
                    ui,
                    icons::Icon::Plus,
                    tr(&self.state, "add-provider"),
                    Vec2::splat(settings::control_height()),
                    true,
                )
                .clicked()
                {
                    self.provider_draft = ProviderDraft::default();
                }
            });
            if let Some(profile) = selected_profile {
                self.provider_draft = ProviderDraft::from_profile(&profile);
            }
        });

        settings::section_header(
            ui,
            tr(&self.state, "connection"),
            Some(tr(&self.state, "connection-help")),
        );
        settings::form_row(ui, tr(&self.state, "preset"), None, |ui| {
            let mut preset = self.provider_draft.preset;
            egui::ComboBox::from_id_salt("provider-preset")
                .width(settings::control_width(ui))
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
        });
        let draft_name_key = provider_name_key(&self.provider_draft.name);
        let duplicate_provider_name = !draft_name_key.is_empty()
            && self.state.provider_profiles.iter().any(|profile| {
                Some(profile.id) != self.provider_draft.id
                    && provider_name_key(&profile.name) == draft_name_key
            });
        settings::form_row(ui, tr(&self.state, "provider-name"), None, |ui| {
            settings::sized_control(
                ui,
                TextEdit::singleline(&mut self.provider_draft.name)
                    .desired_width(settings::control_width(ui)),
            );
            if duplicate_provider_name {
                ui.label(
                    RichText::new(tr(&self.state, "provider-name-duplicate"))
                        .small()
                        .color(ERROR_STRONG),
                );
            }
        });
        settings::form_row(ui, "Base URL", None, |ui| {
            settings::sized_control(
                ui,
                TextEdit::singleline(&mut self.provider_draft.base_url)
                    .desired_width(settings::control_width(ui)),
            );
        });
        settings::form_row(ui, tr(&self.state, "protocol"), None, |ui| {
            let old_protocol = self.provider_draft.protocol;
            egui::ComboBox::from_id_salt("provider-protocol")
                .width(settings::control_width(ui))
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
                self.provider_draft.base_url =
                    self.provider_draft.protocol.default_base_url().into();
            }
        });
        settings::form_row(
            ui,
            "API Key",
            Some(tr(&self.state, "provider-help")),
            |ui| {
                let key_hint = if self.provider_draft.has_api_key {
                    tr(&self.state, "key-stored")
                } else {
                    tr(&self.state, "key-required")
                };
                settings::sized_control(
                    ui,
                    TextEdit::singleline(&mut self.provider_draft.api_key)
                        .password(true)
                        .hint_text(key_hint)
                        .desired_width(settings::control_width(ui)),
                );
            },
        );

        let can_save = !self.provider_draft.name.trim().is_empty()
            && !self.provider_draft.base_url.trim().is_empty()
            && !self.provider_draft.models.is_empty()
            && !duplicate_provider_name;

        settings::section_header(
            ui,
            tr(&self.state, "models"),
            Some(tr(&self.state, "models-help")),
        );
        settings::control_row(ui, |ui| {
            let can_discover = !self.provider_draft.base_url.trim().is_empty();
            if ui
                .add_enabled_ui(can_discover, |ui| {
                    settings::action_button(ui, tr(&self.state, "discover-models"))
                })
                .inner
                .clicked()
            {
                self.intents.push(UiIntent::DiscoverProviderModels {
                    profile_id: self.provider_draft.id,
                    provider_name: self.provider_draft.name.trim().to_owned(),
                    base_url: self.provider_draft.base_url.clone(),
                    protocol: self.provider_draft.protocol,
                    api_key: (!self.provider_draft.api_key.trim().is_empty())
                        .then(|| self.provider_draft.api_key.clone()),
                });
            }
            ui.add_space(settings::inline_gap());
            let model_input_width =
                settings::inline_leading_width(ui, settings::action_button_width());
            let mut add_model_clicked = false;
            let model_id_input = ui
                .horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = settings::inline_gap();
                    let input = ui.add_sized(
                        [model_input_width, settings::control_height()],
                        TextEdit::singleline(&mut self.provider_draft.manual_model_id)
                            .hint_text(tr(&self.state, "model-id"))
                            .desired_width(model_input_width),
                    );
                    add_model_clicked =
                        settings::action_button(ui, tr(&self.state, "add-model")).clicked();
                    input
                })
                .inner;
            let add_model = add_model_clicked
                || (model_id_input.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)));
            if add_model {
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
        });
        if self.provider_draft.models.is_empty() {
            settings::control_row(ui, |ui| {
                ui.label(RichText::new(tr(&self.state, "no-models")).color(MUTED_INK));
            });
        } else {
            let mut remove = None;
            settings::control_row(ui, |ui| {
                let row_width = settings::control_width(ui);
                for (index, model) in self.provider_draft.models.iter().enumerate() {
                    if index > 0 {
                        ui.separator();
                    }
                    ui.allocate_ui_with_layout(
                        Vec2::new(row_width, settings::model_row_height()),
                        Layout::left_to_right(Align::Center),
                        |ui| {
                            ui.spacing_mut().item_spacing.x = settings::inline_gap();
                            let content_width =
                                row_width - settings::inline_gap() - settings::control_height();
                            ui.allocate_ui_with_layout(
                                Vec2::new(content_width, 40.0),
                                Layout::top_down(Align::Min),
                                |ui| {
                                    ui.spacing_mut().item_spacing.y = 4.0;
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&model.name)
                                                .font(mono_font(12.0))
                                                .strong(),
                                        )
                                        .truncate(),
                                    );
                                    let levels = model
                                        .available_thinking_levels()
                                        .iter()
                                        .map(|level| level.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", ");
                                    let capability =
                                        format!("{} · {}", levels, model.capability_source.label());
                                    let details = if model.name == model.id {
                                        capability
                                    } else {
                                        format!("{} · {}", model.id, capability)
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(details).small().color(MUTED_INK),
                                        )
                                        .truncate(),
                                    );
                                },
                            );
                            if icons::button(
                                ui,
                                icons::Icon::Close,
                                tr(&self.state, "remove-model"),
                                Vec2::splat(settings::control_height()),
                                false,
                            )
                            .clicked()
                            {
                                remove = Some(index);
                            }
                        },
                    );
                }
            });
            if let Some(index) = remove {
                self.provider_draft.models.remove(index);
            }
        }

        ui.separator();
        ui.add_space(12.0);
        settings::control_row(ui, |ui| {
            let width = settings::control_width(ui);
            let delete_width = if self.provider_draft.id.is_some() {
                settings::action_button_width()
            } else {
                0.0
            };
            let spacer = (width - delete_width - settings::action_button_width()).max(0.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                if let Some(id) = self.provider_draft.id
                    && settings::action_button(
                        ui,
                        RichText::new(tr(&self.state, "delete-provider")).color(ERROR_STRONG),
                    )
                    .clicked()
                {
                    self.intents.push(UiIntent::DeleteProvider(id));
                    self.provider_draft = ProviderDraft::default();
                }
                ui.add_space(spacer);
                if ui
                    .add_enabled_ui(can_save, |ui| {
                        settings::action_button(ui, tr(&self.state, "save-and-apply"))
                    })
                    .inner
                    .clicked()
                {
                    self.save_provider_intent();
                }
            });
        });
        ui.add_space(24.0);
    }

    fn web_search_settings(&mut self, ui: &mut Ui) {
        settings::page_header(
            ui,
            tr(&self.state, "web-search"),
            Some(tr(&self.state, "web-search-help")),
        );
        settings::section_header(ui, tr(&self.state, "search-engines"), None);

        let mut select = None;
        let mut remove = None;
        let mut move_engine = None;
        for (index, profile) in self.state.search_engine_profiles.clone().iter().enumerate() {
            settings::control_row(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = settings::inline_gap();
                    let selected = self.search_engine_draft.id == Some(profile.id);
                    if ui.selectable_label(selected, &profile.name).clicked() {
                        select = Some(profile.clone());
                    }
                    ui.label(
                        RichText::new(&profile.base_url)
                            .font(mono_font(10.0))
                            .color(MUTED_INK),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("x").clicked() {
                            remove = Some(index);
                        }
                        if ui.small_button("v").clicked()
                            && index + 1 < self.state.search_engine_profiles.len()
                        {
                            move_engine = Some((index, index + 1));
                        }
                        if ui.small_button("^").clicked() && index > 0 {
                            move_engine = Some((index, index - 1));
                        }
                    });
                });
            });
        }
        if let Some(profile) = select {
            self.search_engine_draft = SearchEngineDraft::from_profile(&profile);
        }
        if let Some(index) = remove {
            let mut profiles = self.state.search_engine_profiles.clone();
            profiles.remove(index);
            self.search_engine_draft = SearchEngineDraft::default();
            self.intents.push(UiIntent::SaveSearchEngines(profiles));
        }
        if let Some((from, to)) = move_engine {
            let mut profiles = self.state.search_engine_profiles.clone();
            profiles.swap(from, to);
            self.intents.push(UiIntent::SaveSearchEngines(profiles));
        }
        settings::control_row(ui, |ui| {
            if settings::action_button(ui, tr(&self.state, "add-search-engine")).clicked() {
                self.search_engine_draft = SearchEngineDraft::default();
            }
        });

        settings::section_header(ui, tr(&self.state, "search-engine-details"), None);
        settings::form_row(ui, tr(&self.state, "provider-name"), None, |ui| {
            settings::sized_control(
                ui,
                TextEdit::singleline(&mut self.search_engine_draft.name)
                    .desired_width(settings::control_width(ui)),
            );
        });
        settings::form_row(
            ui,
            "Base URL",
            Some(tr(&self.state, "searxng-url-help")),
            |ui| {
                settings::sized_control(
                    ui,
                    TextEdit::singleline(&mut self.search_engine_draft.base_url)
                        .hint_text("https://search.example")
                        .desired_width(settings::control_width(ui)),
                );
            },
        );
        settings::form_row(ui, tr(&self.state, "enabled"), None, |ui| {
            ui.checkbox(
                &mut self.search_engine_draft.enabled,
                tr(&self.state, "enabled"),
            );
        });

        settings::control_row(ui, |ui| {
            let mut profiles = self.state.search_engine_profiles.clone();
            let profile = self.search_engine_draft.to_profile(profiles.len() as u32);
            if settings::action_button(ui, tr(&self.state, "save-search-engine")).clicked() {
                if let Some(index) = profiles
                    .iter()
                    .position(|existing| existing.id == profile.id)
                {
                    profiles[index] = profile.clone();
                } else {
                    profiles.push(profile.clone());
                }
                self.intents.push(UiIntent::SaveSearchEngines(profiles));
            }
            ui.add_space(settings::inline_gap());
            if settings::action_button(ui, tr(&self.state, "test-search-engine")).clicked() {
                self.intents.push(UiIntent::TestSearchEngine(profile));
            }
        });
        ui.add_space(24.0);
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

fn status_visual(state: &AppState) -> (Color32, &'static str) {
    match state.session_status {
        SessionStatus::Offline => (MUTED_INK, "OFFLINE"),
        SessionStatus::Starting => (BLUE, "STARTING"),
        SessionStatus::Ready => (SUCCESS, "READY"),
        SessionStatus::Streaming => (BLUE, "WORKING"),
        SessionStatus::Compacting => (WARNING, "COMPACTING"),
        SessionStatus::Failed(_) => (ERROR_RED, "ERROR"),
    }
}

fn bracket_button(ui: &mut Ui, label: &str) -> egui::Response {
    ui.add_sized(
        [label.chars().count() as f32 * 7.5 + 18.0, 26.0],
        Button::new(RichText::new(label).font(mono_font(10.0))),
    )
}

fn chip(ui: &mut Ui, label: &str, color: Color32) {
    Frame::default()
        .fill(tint(color, 20))
        .stroke(Stroke::new(1.0_f32, tint(color, 80)))
        .corner_radius(0)
        .inner_margin(Margin::symmetric(7, 3))
        .show(ui, |ui| {
            ui.label(RichText::new(label).font(mono_font(10.0)).color(color));
        });
}

fn attachment_type(attachment: &pi_whim_core::Attachment) -> String {
    match attachment.kind {
        pi_whim_core::AttachmentKind::Directory => "FOLDER".into(),
        pi_whim_core::AttachmentKind::File => attachment
            .name
            .rsplit_once('.')
            .map(|(_, extension)| extension.to_ascii_uppercase())
            .filter(|extension| !extension.is_empty())
            .unwrap_or_else(|| "FILE".into()),
    }
}

fn attachment_icon(attachment: &pi_whim_core::Attachment) -> icons::Icon {
    match attachment.kind {
        pi_whim_core::AttachmentKind::File => icons::Icon::File,
        pi_whim_core::AttachmentKind::Directory => icons::Icon::Folder,
    }
}

fn composer_attachment_card(
    ui: &mut Ui,
    attachment: &pi_whim_core::Attachment,
    intents: &mut Vec<UiIntent>,
) {
    Frame::default()
        .fill(tint(BLUE, 16))
        .stroke(Stroke::new(1.0_f32, tint(BLUE, 72)))
        .corner_radius(4)
        .inner_margin(Margin::symmetric(7, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                icons::display(ui, attachment_icon(attachment), Vec2::splat(16.0), BLUE);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new(&attachment.name)
                            .font(mono_font(10.0))
                            .color(INK),
                    );
                    ui.label(
                        RichText::new(attachment_type(attachment))
                            .font(mono_font(8.0))
                            .color(MUTED_INK),
                    );
                });
                if icons::button(
                    ui,
                    icons::Icon::Close,
                    "Remove attachment",
                    Vec2::splat(18.0),
                    false,
                )
                .clicked()
                {
                    intents.push(UiIntent::RemoveComposerAttachment(attachment.path.clone()));
                }
            });
        });
}

fn sent_attachment_cards(ui: &mut Ui, attachments: &[pi_whim_core::Attachment]) {
    if attachments.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for attachment in attachments {
            Frame::default()
                .fill(tint(BLUE, 14))
                .stroke(Stroke::new(1.0_f32, tint(BLUE, 60)))
                .corner_radius(4)
                .inner_margin(Margin::symmetric(6, 4))
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        icons::display(ui, attachment_icon(attachment), Vec2::splat(14.0), BLUE);
                        ui.vertical(|ui| {
                            ui.label(
                                RichText::new(&attachment.name)
                                    .font(mono_font(9.0))
                                    .color(INK),
                            );
                            ui.label(
                                RichText::new(attachment_type(attachment))
                                    .font(mono_font(8.0))
                                    .color(MUTED_INK),
                            );
                        });
                    });
                });
        }
    });
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}K", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn composer_padding(ui: &Ui) -> f32 {
    ((ui.available_width() - CHAT_CONTENT_WIDTH) / 2.0).max(0.0)
}

fn paint_graph_paper(painter: &egui::Painter, rect: egui::Rect) {
    let step = 28.0;
    let start_x = (rect.left() / step).floor() * step;
    let start_y = (rect.top() / step).floor() * step;
    let stroke = Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(75, 96, 124, 10));
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

fn tr(state: &AppState, key: &str) -> &'static str {
    let zh = state.language == Language::SimplifiedChinese;
    match key {
        "projects" => {
            if zh {
                "项目"
            } else {
                "Projects"
            }
        }
        "add-project" => {
            if zh {
                "添加本地项目"
            } else {
                "Add local project"
            }
        }
        "search" => {
            if zh {
                "搜索项目"
            } else {
                "Search projects"
            }
        }
        "show-finder" => {
            if zh {
                "在 Finder 中显示"
            } else {
                "Show in Finder"
            }
        }
        "remove" => {
            if zh {
                "移除"
            } else {
                "Remove"
            }
        }
        "rename" => {
            if zh {
                "重命名"
            } else {
                "Rename"
            }
        }
        "rename-session" => {
            if zh {
                "重命名会话"
            } else {
                "Rename session"
            }
        }
        "clone" => {
            if zh {
                "克隆会话"
            } else {
                "Clone session"
            }
        }
        "delete" => {
            if zh {
                "移至废纸篓"
            } else {
                "Move to trash"
            }
        }
        "save" => {
            if zh {
                "保存"
            } else {
                "Save"
            }
        }
        "new-session" => {
            if zh {
                "新建会话"
            } else {
                "New session"
            }
        }
        "empty-heading" => {
            if zh {
                "我们应该在这里做些什么？"
            } else {
                "What should we make happen?"
            }
        }
        "empty-detail" => {
            if zh {
                "选择一个项目，然后告诉 Pi 你想完成什么。"
            } else {
                "Select a project, then tell Pi what you want to do."
            }
        }
        "select-project-to-chat" => {
            if zh {
                "先从左侧添加并选择一个项目，才能开始对话。"
            } else {
                "Add and select a project from the sidebar before starting a conversation."
            }
        }
        "fork-here" => {
            if zh {
                "从这里分叉"
            } else {
                "Fork from here"
            }
        }
        "composer-placeholder" => {
            if zh {
                "告诉 Pi 你想完成什么..."
            } else {
                "Tell Pi what you want to do..."
            }
        }
        "add-attachment" => {
            if zh {
                "添加附件"
            } else {
                "Add attachment"
            }
        }
        "choose-files" => {
            if zh {
                "选择文件..."
            } else {
                "Choose files..."
            }
        }
        "choose-folder" => {
            if zh {
                "选择文件夹..."
            } else {
                "Choose folder..."
            }
        }
        "queued" => {
            if zh {
                "已排队"
            } else {
                "QUEUED"
            }
        }
        "follow-ups" => {
            if zh {
                "后续队列"
            } else {
                "FOLLOW-UPS"
            }
        }
        "thinking" => {
            if zh {
                "思考"
            } else {
                "Thinking"
            }
        }
        "models-unavailable" => {
            if zh {
                "没有可用模型。请在设置中保存一个模型提供商。"
            } else {
                "No models are available. Save a provider in Settings."
            }
        }
        "stop" => {
            if zh {
                "停止"
            } else {
                "Stop"
            }
        }
        "settings" => {
            if zh {
                "设置"
            } else {
                "Settings"
            }
        }
        "general" => {
            if zh {
                "通用"
            } else {
                "General"
            }
        }
        "providers" => {
            if zh {
                "模型提供商"
            } else {
                "Providers"
            }
        }
        "web-search" => {
            if zh {
                "网页搜索"
            } else {
                "Web Search"
            }
        }
        "web-search-help" => {
            if zh {
                "配置按顺序尝试的网页搜索引擎。"
            } else {
                "Configure web search engines in fallback order."
            }
        }
        "search-engines" => {
            if zh {
                "搜索引擎"
            } else {
                "Search engines"
            }
        }
        "add-search-engine" => {
            if zh {
                "添加搜索引擎"
            } else {
                "Add search engine"
            }
        }
        "search-engine-details" => {
            if zh {
                "搜索引擎详情"
            } else {
                "Search engine details"
            }
        }
        "searxng-url-help" => {
            if zh {
                "SearXNG 实例根 URL。"
            } else {
                "Root URL of the SearXNG instance."
            }
        }
        "enabled" => {
            if zh {
                "启用"
            } else {
                "Enabled"
            }
        }
        "save-search-engine" => {
            if zh {
                "保存搜索引擎"
            } else {
                "Save search engine"
            }
        }
        "test-search-engine" => {
            if zh {
                "测试连接"
            } else {
                "Test connection"
            }
        }
        "providers-help" => {
            if zh {
                "配置 Pi 使用的模型提供商。"
            } else {
                "Configure model providers used by Pi."
            }
        }
        "provider-name" => {
            if zh {
                "名称"
            } else {
                "Name"
            }
        }
        "preset" => {
            if zh {
                "预设"
            } else {
                "Preset"
            }
        }
        "protocol" => {
            if zh {
                "请求协议"
            } else {
                "Request protocol"
            }
        }
        "key-stored" => {
            if zh {
                "已保存在 Keychain；留空可保持不变"
            } else {
                "Stored in Keychain; leave blank to keep it"
            }
        }
        "key-required" => {
            if zh {
                "输入 API Key"
            } else {
                "Enter API key"
            }
        }
        "discover-models" => {
            if zh {
                "发现模型"
            } else {
                "Discover models"
            }
        }
        "add-model" => {
            if zh {
                "添加模型"
            } else {
                "Add model"
            }
        }
        "model-id" => {
            if zh {
                "手动输入模型 ID"
            } else {
                "Manual model ID"
            }
        }
        "no-models" => {
            if zh {
                "尚未选择模型。"
            } else {
                "No models selected."
            }
        }
        "save-provider" => {
            if zh {
                "保存提供商"
            } else {
                "Save provider"
            }
        }
        "save-and-apply" => {
            if zh {
                "保存并应用"
            } else {
                "Save and apply"
            }
        }
        "delete-provider" => {
            if zh {
                "删除提供商"
            } else {
                "Delete provider"
            }
        }
        "language" => {
            if zh {
                "语言"
            } else {
                "Language"
            }
        }
        "bash-policy" => {
            if zh {
                "Bash 命令"
            } else {
                "Bash commands"
            }
        }
        "bash-help" => {
            if zh {
                "控制 Bash 工具的执行方式。"
            } else {
                "Control how the Bash tool executes."
            }
        }
        "queue-mode" => {
            if zh {
                "队列模式"
            } else {
                "Queue mode"
            }
        }
        "steer-mode" => "Steer",
        "follow-up-mode" => "Follow-up",
        "provider-help" => {
            if zh {
                "密钥安全存储在 macOS Keychain。"
            } else {
                "Keys are stored securely in macOS Keychain."
            }
        }
        "back" => {
            if zh {
                "返回"
            } else {
                "Back"
            }
        }
        "appearance" => {
            if zh {
                "外观"
            } else {
                "Appearance"
            }
        }
        "context" => {
            if zh {
                "上下文"
            } else {
                "Context"
            }
        }
        "context-help" => {
            if zh {
                "控制会话上下文管理。"
            } else {
                "Control conversation context management."
            }
        }
        "auto-compaction" => {
            if zh {
                "自动压缩上下文"
            } else {
                "Automatic compaction"
            }
        }
        "auto-compaction-help" => {
            if zh {
                "在上下文接近上限时自动压缩。"
            } else {
                "Automatically compact when context approaches its limit."
            }
        }
        "command-policy" => {
            if zh {
                "执行策略"
            } else {
                "Execution policy"
            }
        }
        "allow" => {
            if zh {
                "允许"
            } else {
                "Allow"
            }
        }
        "ask" => {
            if zh {
                "询问"
            } else {
                "Ask"
            }
        }
        "deny" => {
            if zh {
                "拒绝"
            } else {
                "Deny"
            }
        }
        "blocked-patterns" => {
            if zh {
                "阻止模式"
            } else {
                "Blocked patterns"
            }
        }
        "blocked-patterns-help" => {
            if zh {
                "每行一个 Bash 命令阻止模式。"
            } else {
                "One Bash command blocking pattern per line."
            }
        }
        "apply-command-filters" => {
            if zh {
                "应用命令过滤器"
            } else {
                "Apply command filters"
            }
        }
        "agent-team" => {
            if zh {
                "代理团队"
            } else {
                "Agent team"
            }
        }
        "agent-team-help" => {
            if zh {
                "限制委派代理的层级与数量。"
            } else {
                "Limit delegated agent depth and count."
            }
        }
        "max-agent-depth" => {
            if zh {
                "最大代理层级"
            } else {
                "Maximum agent depth"
            }
        }
        "max-agents-per-level" => {
            if zh {
                "每层最大代理数"
            } else {
                "Maximum agents per level"
            }
        }
        "one-at-a-time" => {
            if zh {
                "逐个"
            } else {
                "One at a time"
            }
        }
        "all" => {
            if zh {
                "全部"
            } else {
                "All"
            }
        }
        "configured-providers" => {
            if zh {
                "已配置的提供商"
            } else {
                "Configured providers"
            }
        }
        "add-provider" => {
            if zh {
                "添加提供商"
            } else {
                "Add provider"
            }
        }
        "connection" => {
            if zh {
                "连接"
            } else {
                "Connection"
            }
        }
        "connection-help" => {
            if zh {
                "保存前填写连接详情。"
            } else {
                "Fill in connection details before saving."
            }
        }
        "provider-name-duplicate" => {
            if zh {
                "名称已被使用。"
            } else {
                "This name is already in use."
            }
        }
        "models" => {
            if zh {
                "模型"
            } else {
                "Models"
            }
        }
        "models-help" => {
            if zh {
                "发现或手动添加可用模型。"
            } else {
                "Discover or manually add available models."
            }
        }
        "show-error" => {
            if zh {
                "显示错误"
            } else {
                "Show error"
            }
        }
        "error-banner-title" => {
            if zh {
                "请求失败"
            } else {
                "Request failed"
            }
        }
        "dismiss" => {
            if zh {
                "关闭"
            } else {
                "Dismiss"
            }
        }
        "copy-error" => {
            if zh {
                "复制错误"
            } else {
                "Copy error"
            }
        }
        "compacting-banner" => {
            if zh {
                "正在压缩上下文"
            } else {
                "Compacting context"
            }
        }
        "compacting-detail" => {
            if zh {
                "Pi 正在整理早期消息。"
            } else {
                "Pi is condensing earlier messages."
            }
        }
        "auto-compact-on" => {
            if zh {
                "自动压缩：开"
            } else {
                "AUTO-COMPACT: ON"
            }
        }
        "auto-compact-off" => {
            if zh {
                "自动压缩：关"
            } else {
                "AUTO-COMPACT: OFF"
            }
        }
        "copy-session-id" => {
            if zh {
                "复制会话 ID"
            } else {
                "Copy session ID"
            }
        }
        "hint-slash" => {
            if zh {
                "/ 查看快捷操作"
            } else {
                "/ for quick actions"
            }
        }
        "hint-enter" => {
            if zh {
                "Enter 发送"
            } else {
                "Enter to send"
            }
        }
        "hint-shift-enter" => {
            if zh {
                "Shift+Enter 换行"
            } else {
                "Shift+Enter for a new line"
            }
        }
        "search-models" => {
            if zh {
                "搜索模型"
            } else {
                "Search models"
            }
        }
        "copy-report" => {
            if zh {
                "复制回复"
            } else {
                "Copy reply"
            }
        }
        "raw-tool-details" => {
            if zh {
                "原始工具详情"
            } else {
                "Raw tool details"
            }
        }
        "show-all" => {
            if zh {
                "显示完整内容"
            } else {
                "Show all"
            }
        }
        "generating" => {
            if zh {
                "正在生成"
            } else {
                "Generating"
            }
        }
        "send" => {
            if zh {
                "发送"
            } else {
                "Send"
            }
        }
        "slash-commands" => {
            if zh {
                "快捷操作"
            } else {
                "Quick actions"
            }
        }
        "slash-help" => {
            if zh {
                "上下方向键选择，Enter 或 Tab 确认。"
            } else {
                "Use arrows to select; Enter or Tab to confirm."
            }
        }
        _ => "",
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
}

pub fn install_fonts(context: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let cjk = "pi-whim-cjk".to_owned();
    let emoji = "pi-whim-emoji".to_owned();
    fonts
        .font_data
        .insert(cjk.clone(), Arc::new(FontData::from_static(CJK_FONT_BYTES)));
    fonts.font_data.insert(
        emoji.clone(),
        Arc::new(FontData::from_static(EMOJI_FONT_BYTES)),
    );
    append_font(&mut fonts, FontFamily::Proportional, cjk.clone());
    append_font(&mut fonts, FontFamily::Proportional, emoji.clone());
    append_font(&mut fonts, FontFamily::Monospace, cjk);
    append_font(&mut fonts, FontFamily::Monospace, emoji);
    context.set_fonts(fonts);
}

fn append_font(fonts: &mut FontDefinitions, family: FontFamily, font: String) {
    fonts.families.entry(family).or_default().push(font);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_counts_use_compact_labels() {
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1_200), "1.2K");
        assert_eq!(format_tokens(2_000_000), "2.0M");
    }

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
}
