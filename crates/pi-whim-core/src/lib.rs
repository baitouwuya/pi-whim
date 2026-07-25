//! Pure application state shared by the desktop UI and runtime adapters.

mod model_capabilities;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub use model_capabilities::{
    CapabilityMatch, CatalogModelCapability, ModelCapability, ModelCapabilitySource, ThinkingLevel,
    ThinkingLevelMap, normalize_provider_display_name, normalize_provider_name, provider_name_key,
    resolve_bundled_capability, resolve_bundled_capability_by_model_id, resolve_catalog_capability,
};

pub type ProjectId = Uuid;
pub type SessionId = Uuid;
pub type ProviderId = Uuid;
pub type SearchEngineId = Uuid;

/// Derive the same immutable identifier for a Pi JSONL session everywhere in the app.
/// The path is intentionally the input: it remains stable across reloads and renames.
pub fn stable_session_id(path: &str) -> SessionId {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_bytes())
}

pub const MAX_AGENT_DEPTH: u8 = 8;
pub const MAX_AGENTS_PER_LEVEL: u16 = 64;

/// The externally visible permission level for a spawned agent.  The level grants
/// a ceiling; the explicit tool/model lists in [`AgentPermissionPolicy`] can only
/// make that ceiling smaller.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPermissionLevel {
    ReadOnly,
    #[default]
    Controlled,
    Full,
}

impl AgentPermissionLevel {
    pub const fn rank(self) -> u8 {
        match self {
            Self::ReadOnly => 0,
            Self::Controlled => 1,
            Self::Full => 2,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::ReadOnly => "Read only",
            Self::Controlled => "Controlled",
            Self::Full => "Full",
        }
    }
}

/// A provider/model pair that may be handed to a child process. Provider is the
/// immutable Pi provider key, not the display name.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentModelSelection {
    pub provider: String,
    pub model: String,
}

/// Persisted policy for a child. Empty allowlists mean "use the level default",
/// except `allowed_models`, where empty intentionally means every configured
/// model the parent may delegate.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermissionPolicy {
    #[serde(default)]
    pub level: AgentPermissionLevel,
    #[serde(default)]
    pub enabled_tools: Vec<String>,
    #[serde(default)]
    pub command_allowlist: Vec<String>,
    #[serde(default)]
    pub allowed_models: Vec<AgentModelSelection>,
    #[serde(default)]
    pub trusted_extensions: Vec<String>,
}

impl Default for AgentPermissionPolicy {
    fn default() -> Self {
        Self {
            level: AgentPermissionLevel::Controlled,
            enabled_tools: Vec::new(),
            command_allowlist: Vec::new(),
            allowed_models: Vec::new(),
            trusted_extensions: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPermissionPreset {
    pub name: String,
    pub policy: AgentPermissionPolicy,
}

/// Limits a level-0 agent team. Level counts are global within one team, not per parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTeamConfig {
    pub max_depth: u8,
    /// Shared active-agent limit applied independently to every level above level 0.
    pub max_agents_per_level: u16,
    #[serde(default)]
    pub default_policy: AgentPermissionPolicy,
    #[serde(default)]
    pub presets: Vec<AgentPermissionPreset>,
}

impl Default for AgentTeamConfig {
    fn default() -> Self {
        Self {
            max_depth: 2,
            max_agents_per_level: 4,
            default_policy: AgentPermissionPolicy::default(),
            presets: Vec::new(),
        }
    }
}

impl AgentTeamConfig {
    pub fn normalized(mut self) -> Self {
        self.max_depth = self.max_depth.clamp(1, MAX_AGENT_DEPTH);
        self.max_agents_per_level = self.max_agents_per_level.clamp(1, MAX_AGENTS_PER_LEVEL);
        self.default_policy = normalize_agent_policy(self.default_policy);
        self.presets = self
            .presets
            .into_iter()
            .filter_map(|mut preset| {
                preset.name = preset.name.trim().to_owned();
                (!preset.name.is_empty()).then(|| AgentPermissionPreset {
                    name: preset.name,
                    policy: normalize_agent_policy(preset.policy),
                })
            })
            .collect();
        self
    }

    pub fn maximum_for_level(&self, level: u8) -> Option<u16> {
        if level == 0 || level > self.max_depth {
            return None;
        }
        Some(self.max_agents_per_level)
    }
}

pub fn normalize_agent_policy(mut policy: AgentPermissionPolicy) -> AgentPermissionPolicy {
    fn normalized_strings(values: Vec<String>) -> Vec<String> {
        let mut values = values
            .into_iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        values
    }
    policy.enabled_tools = normalized_strings(policy.enabled_tools);
    policy.command_allowlist = normalized_strings(policy.command_allowlist);
    policy.trusted_extensions = normalized_strings(policy.trusted_extensions);
    policy
        .allowed_models
        .retain(|model| !model.provider.trim().is_empty() && !model.model.trim().is_empty());
    policy.allowed_models.sort_by(|left, right| {
        left.provider
            .cmp(&right.provider)
            .then_with(|| left.model.cmp(&right.model))
    });
    policy.allowed_models.dedup();
    policy
}

/// The request shape spoken by a custom Pi provider.
/// These map directly to Pi's documented `models.json` API values.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderProtocol {
    #[default]
    OpenAiCompletions,
    OpenAiResponses,
    AnthropicMessages,
    GoogleGenerativeAi,
}

impl ProviderProtocol {
    pub const ALL: [Self; 4] = [
        Self::OpenAiCompletions,
        Self::OpenAiResponses,
        Self::AnthropicMessages,
        Self::GoogleGenerativeAi,
    ];

    pub fn pi_api(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerativeAi => "google-generative-ai",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "OpenAI Chat Completions",
            Self::OpenAiResponses => "OpenAI Responses",
            Self::AnthropicMessages => "Anthropic Messages",
            Self::GoogleGenerativeAi => "Google Generative AI",
        }
    }

    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAiCompletions | Self::OpenAiResponses => "https://api.openai.com/v1",
            Self::AnthropicMessages => "https://api.anthropic.com",
            Self::GoogleGenerativeAi => "https://generativelanguage.googleapis.com/v1beta",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderModel {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub reasoning: bool,
    #[serde(default)]
    pub supports_images: bool,
    #[serde(default)]
    pub thinking_level_map: ThinkingLevelMap,
    #[serde(default)]
    pub capability_source: ModelCapabilitySource,
}

impl ProviderModel {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            reasoning: false,
            supports_images: false,
            thinking_level_map: ThinkingLevelMap::default(),
            capability_source: ModelCapabilitySource::Unverified,
        }
    }

    pub fn apply_capability(&mut self, capability: ModelCapability) {
        if self.name == self.id || self.name.trim().is_empty() {
            self.name = capability.name;
        }
        self.reasoning = capability.reasoning;
        self.supports_images |= capability.supports_images;
        self.thinking_level_map = capability.thinking_level_map;
        self.capability_source = capability.source;
    }

    pub fn available_thinking_levels(&self) -> Vec<ThinkingLevel> {
        self.thinking_level_map.available_levels(self.reasoning)
    }
}

/// Metadata is persisted locally; its API key is always stored separately in Keychain.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    pub id: ProviderId,
    pub name: String,
    pub base_url: String,
    pub protocol: ProviderProtocol,
    pub models: Vec<ProviderModel>,
    pub updated_at_ms: i64,
    /// This is metadata only. The actual API key remains in the OS keychain.
    #[serde(default)]
    pub has_api_key: bool,
}

/// A user-configured web search backend. The enum is intentionally extensible
/// so provider-specific adapters can be added without changing tool callers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchEngineKind {
    #[default]
    Searxng,
}

impl SearchEngineKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Searxng => "SearXNG",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchEngineProfile {
    pub id: SearchEngineId,
    pub name: String,
    pub kind: SearchEngineKind,
    pub base_url: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    pub position: u32,
}

fn enabled_by_default() -> bool {
    true
}

impl SearchEngineProfile {
    pub fn new_searxng() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: "SearXNG".into(),
            kind: SearchEngineKind::Searxng,
            base_url: String::new(),
            enabled: true,
            position: 0,
        }
    }

    pub fn normalized(mut self) -> Self {
        self.name = self.name.trim().to_owned();
        self.base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Project {
    pub id: ProjectId,
    pub name: String,
    pub path: String,
    pub pinned: bool,
    pub last_opened_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub pi_path: String,
    pub title: String,
    pub preview: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    File,
    Directory,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub name: String,
    /// A canonical absolute path. This is the value supplied to the model.
    pub path: String,
    pub kind: AttachmentKind,
    /// Only application-created clipboard artifacts may be deleted when an
    /// unsent attachment is removed.
    #[serde(default, alias = "generated_pasted_text")]
    pub generated_by_app: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    /// Immutable provider key used by Pi RPC.
    pub provider: String,
    /// User-defined provider name shown in the interface.
    pub provider_name: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlashCommandInfo {
    pub name: String,
    pub description: String,
    pub source: String,
}

impl ModelOption {
    pub fn label(&self) -> String {
        self.name.clone()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// How a submitted prompt reaches a running agent.
///
/// This picks the Pi RPC to use, so it is protocol vocabulary rather than a
/// view concern: the session pool reads it to decide between starting a turn,
/// steering one in flight, and queueing for after it finishes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubmitMode {
    /// Start a new turn.
    #[default]
    Prompt,
    /// Redirect the turn already in flight.
    Steer,
    /// Queue for once the current turn finishes.
    FollowUp,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub total_messages: u64,
    pub user_messages: u64,
    pub assistant_messages: u64,
    pub tool_calls: u64,
    pub total_tokens: u64,
    pub cost_microusd: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum BashPolicy {
    #[default]
    Allow,
    Ask,
    Deny,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[default]
    English,
    SimplifiedChinese,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConversationRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConversationItem {
    pub id: String,
    pub role: ConversationRole,
    pub full_text: String,
    pub streaming: bool,
    pub tool_name: Option<String>,
    /// Human-readable tool activity and result shown in the first expansion level.
    pub tool_report: Option<String>,
    /// Raw tool event data, available only from the nested diagnostic expansion.
    pub tool_details: Option<String>,
    pub is_error: bool,
    /// Model id Pi reports for an assistant message; lets the UI confirm a
    /// model switch actually took effect per message.
    pub model: Option<String>,
    pub attachments: Vec<Attachment>,
}

pub fn grapheme_prefix(text: &str, count: usize) -> &str {
    if count == 0 {
        return "";
    }
    let mut boundaries = text
        .grapheme_indices(true)
        .map(|(index, _)| index)
        .skip(count);
    boundaries.next().map_or(text, |index| &text[..index])
}

#[derive(Clone, Debug, Default)]
pub struct AppState {
    pub projects: Vec<Project>,
    pub sessions: BTreeMap<ProjectId, Vec<SessionSummary>>,
    pub selected_project: Option<ProjectId>,
    pub selected_session: Option<SessionId>,
    /// Pi session file paths whose agent is currently streaming or compacting.
    /// Sessions run in parallel processes, so background sessions stay busy
    /// while another one is on screen; the sidebar marks these with a dot.
    pub running_sessions: std::collections::HashSet<String>,
    pub conversation: Vec<ConversationItem>,
    pub language: Language,
    pub bash_policy: BashPolicy,
    pub bash_blocked_patterns: Vec<String>,
    pub agent_team_config: AgentTeamConfig,
    pub session_status: SessionStatus,
    pub pending_steering: Vec<String>,
    pub pending_follow_up: Vec<String>,
    pub current_model: Option<ModelOption>,
    /// Model the user picked in the UI but hasn't taken effect yet. A model
    /// switch is deferred until the next prompt so the prior model can compact
    /// the existing conversation first (cache-friendly).
    pub pending_model: Option<ModelOption>,
    pub available_models: Vec<ModelOption>,
    pub thinking_level: ThinkingLevel,
    pub available_thinking_levels: Vec<ThinkingLevel>,
    pub available_commands: Vec<SlashCommandInfo>,
    pub auto_compaction_enabled: bool,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub session_metrics: Option<SessionMetrics>,
    pub provider_profiles: Vec<ProviderProfile>,
    pub search_engine_profiles: Vec<SearchEngineProfile>,
}

/// Where the visible session's Pi process stands.
///
/// Distinct from `pi_whim_agent_team::AgentStatus`, which tracks the lifecycle
/// of a single spawned sub-agent. This one describes a whole session, so a
/// session can be `Ready` while several of its sub-agents are still running.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum SessionStatus {
    /// No Pi process for this session yet.
    #[default]
    Offline,
    Starting,
    Ready,
    Streaming,
    Compacting,
    Failed(String),
}

#[derive(Clone, Debug)]
pub enum Action {
    ProjectsLoaded(Vec<Project>),
    SessionsLoaded {
        project_id: ProjectId,
        sessions: Vec<SessionSummary>,
    },
    SelectProject(ProjectId),
    SelectSession(SessionId),
    SessionRunning {
        path: String,
        running: bool,
    },
    SetLanguage(Language),
    SetBashPolicy(BashPolicy),
    SetBashBlockedPatterns(Vec<String>),
    SetAgentTeamConfig(AgentTeamConfig),
    ProviderProfilesLoaded(Vec<ProviderProfile>),
    SearchEngineProfilesLoaded(Vec<SearchEngineProfile>),
    RuntimeControlsUpdated {
        current_model: Option<ModelOption>,
        available_models: Vec<ModelOption>,
        thinking_level: ThinkingLevel,
        available_thinking_levels: Vec<ThinkingLevel>,
        auto_compaction_enabled: bool,
        steering_mode: QueueMode,
        follow_up_mode: QueueMode,
    },
    RuntimeCommandsUpdated(Vec<SlashCommandInfo>),
    SetPendingModel(Option<ModelOption>),
    SessionMetricsUpdated(SessionMetrics),
    SetSessionStatus(SessionStatus),
    UpsertConversation(ConversationItem),
    RekeyConversation {
        from: String,
        to: String,
    },
    AppendAssistantText {
        id: String,
        text: String,
    },
    FinishMessage(String),
    QueueUpdated {
        steering: Vec<String>,
        follow_up: Vec<String>,
    },
    ClearConversation,
}

impl AppState {
    pub fn dispatch(&mut self, action: Action) {
        match action {
            Action::ProjectsLoaded(mut projects) => {
                projects.sort_by_key(|project| (!project.pinned, -project.last_opened_ms));
                self.projects = projects;
            }
            Action::SessionsLoaded {
                project_id,
                mut sessions,
            } => {
                sessions.sort_by_key(|session| -session.updated_at_ms);
                self.sessions.insert(project_id, sessions);
            }
            Action::SelectProject(project_id) => {
                if self.selected_project != Some(project_id) {
                    self.selected_project = Some(project_id);
                    self.selected_session = None;
                    self.conversation.clear();
                }
            }
            Action::SelectSession(session_id) => self.selected_session = Some(session_id),
            Action::SessionRunning { path, running } => {
                if running {
                    self.running_sessions.insert(path);
                } else {
                    self.running_sessions.remove(&path);
                }
            }
            Action::SetLanguage(language) => self.language = language,
            Action::SetBashPolicy(policy) => self.bash_policy = policy,
            Action::SetBashBlockedPatterns(patterns) => {
                self.bash_blocked_patterns = normalize_bash_patterns(patterns)
            }
            Action::SetAgentTeamConfig(config) => {
                self.agent_team_config = config.normalized();
            }
            Action::ProviderProfilesLoaded(mut profiles) => {
                profiles.sort_by(|left, right| {
                    left.name.to_lowercase().cmp(&right.name.to_lowercase())
                });
                self.provider_profiles = profiles;
            }
            Action::SearchEngineProfilesLoaded(mut profiles) => {
                profiles.sort_by(|left, right| {
                    left.position
                        .cmp(&right.position)
                        .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
                });
                self.search_engine_profiles = profiles;
            }
            Action::RuntimeControlsUpdated {
                current_model,
                mut available_models,
                thinking_level,
                available_thinking_levels,
                auto_compaction_enabled,
                steering_mode,
                follow_up_mode,
            } => {
                available_models.sort_by_key(|model| model.label());
                available_models
                    .dedup_by(|left, right| left.provider == right.provider && left.id == right.id);
                self.current_model = current_model;
                self.available_models = available_models;
                self.thinking_level = thinking_level;
                self.available_thinking_levels = available_thinking_levels;
                self.auto_compaction_enabled = auto_compaction_enabled;
                self.steering_mode = steering_mode;
                self.follow_up_mode = follow_up_mode;
            }
            Action::RuntimeCommandsUpdated(mut commands) => {
                commands.sort_by(|left, right| left.name.cmp(&right.name));
                commands.dedup_by(|left, right| left.name == right.name);
                self.available_commands = commands;
            }
            Action::SetPendingModel(model) => self.pending_model = model,
            Action::SessionMetricsUpdated(metrics) => self.session_metrics = Some(metrics),
            Action::SetSessionStatus(status) => self.session_status = status,
            Action::UpsertConversation(item) => {
                if let Some(existing) = self
                    .conversation
                    .iter_mut()
                    .find(|message| message.id == item.id)
                {
                    *existing = item;
                } else {
                    self.conversation.push(item);
                }
            }
            Action::RekeyConversation { from, to } => {
                if let Some(message) = self
                    .conversation
                    .iter_mut()
                    .find(|message| message.id == from)
                {
                    message.id = to;
                }
            }
            Action::AppendAssistantText { id, text } => {
                if let Some(message) = self
                    .conversation
                    .iter_mut()
                    .find(|message| message.id == id)
                {
                    message.full_text.push_str(&text);
                    message.streaming = true;
                }
            }
            Action::FinishMessage(id) => {
                if let Some(message) = self
                    .conversation
                    .iter_mut()
                    .find(|message| message.id == id)
                {
                    message.streaming = false;
                }
            }
            Action::QueueUpdated {
                steering,
                follow_up,
            } => {
                self.pending_steering = steering;
                self.pending_follow_up = follow_up;
            }
            Action::ClearConversation => self.conversation.clear(),
        }
    }
}

/// Trim, deduplicate, and bound persisted command-filter patterns.
pub fn normalize_bash_patterns(patterns: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();
    for pattern in patterns {
        let pattern = pattern.trim();
        if pattern.is_empty()
            || pattern.len() > 512
            || normalized.iter().any(|item| item == pattern)
        {
            continue;
        }
        if normalized.len() == 32 {
            break;
        }
        normalized.push(pattern.to_owned());
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_never_splits_a_grapheme() {
        assert_eq!(grapheme_prefix("A👨‍👩‍👧‍👦B", 2), "A👨‍👩‍👧‍👦");
    }

    // Progressive reveal moved to pi_whim_engine::typewriter::Typewriter, and
    // attachment de-duplication to pi_whim_engine::composer::Composer; both
    // cover those behaviors directly.
    // which covers it directly.

    #[test]
    fn provider_protocol_maps_to_pi_models_json_api() {
        assert_eq!(
            ProviderProtocol::OpenAiCompletions.pi_api(),
            "openai-completions"
        );
        assert_eq!(
            ProviderProtocol::AnthropicMessages.pi_api(),
            "anthropic-messages"
        );
    }

    #[test]
    fn agent_team_config_normalizes_depth_and_level_limits() {
        let config = AgentTeamConfig {
            max_depth: 10,
            max_agents_per_level: 0,
            ..Default::default()
        }
        .normalized();
        assert_eq!(config.max_depth, MAX_AGENT_DEPTH);
        assert_eq!(config.maximum_for_level(1), Some(1));
        assert_eq!(config.maximum_for_level(2), Some(1));
        assert_eq!(config.maximum_for_level(0), None);
    }

    #[test]
    fn agent_policy_normalizes_and_deduplicates_configured_capabilities() {
        let policy = normalize_agent_policy(AgentPermissionPolicy {
            enabled_tools: vec![" bash ".into(), "bash".into(), String::new()],
            command_allowlist: vec!["git status **".into(), "git status **".into()],
            trusted_extensions: vec![" /tmp/ext.ts ".into(), "/tmp/ext.ts".into()],
            allowed_models: vec![
                AgentModelSelection {
                    provider: "p".into(),
                    model: "m".into(),
                },
                AgentModelSelection {
                    provider: "p".into(),
                    model: "m".into(),
                },
                AgentModelSelection {
                    provider: String::new(),
                    model: "invalid".into(),
                },
            ],
            ..AgentPermissionPolicy::default()
        });
        assert_eq!(policy.enabled_tools, vec!["bash"]);
        assert_eq!(policy.command_allowlist, vec!["git status **"]);
        assert_eq!(policy.trusted_extensions, vec!["/tmp/ext.ts"]);
        assert_eq!(policy.allowed_models.len(), 1);
    }
}
