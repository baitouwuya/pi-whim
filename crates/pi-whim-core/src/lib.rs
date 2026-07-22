//! Pure application state shared by the desktop UI and runtime adapters.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use unicode_segmentation::UnicodeSegmentation;
use uuid::Uuid;

pub type ProjectId = Uuid;
pub type SessionId = Uuid;
pub type ProviderId = Uuid;

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
    pub reasoning: bool,
    pub supports_images: bool,
}

impl ProviderModel {
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            name: id.clone(),
            id,
            reasoning: false,
            supports_images: false,
        }
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageAttachment {
    pub name: String,
    pub mime_type: String,
    pub base64_data: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelOption {
    pub provider: String,
    pub id: String,
    pub name: String,
}

impl ModelOption {
    pub fn label(&self) -> String {
        format!("{} / {}", self.provider, self.name)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
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
    pub revealed_graphemes: usize,
    pub reveal_credit: f32,
    pub streaming: bool,
    pub tool_name: Option<String>,
    pub tool_details: Option<String>,
    pub is_error: bool,
    pub attachments: Vec<ImageAttachment>,
}

impl ConversationItem {
    pub fn text_for_display(&self) -> &str {
        if !self.streaming {
            return &self.full_text;
        }
        grapheme_prefix(&self.full_text, self.revealed_graphemes)
    }

    pub fn reveal_all(&mut self) {
        self.revealed_graphemes = self.full_text.graphemes(true).count();
        self.reveal_credit = 0.0;
    }

    pub fn advance_typewriter(&mut self, elapsed_seconds: f32) -> bool {
        if !self.streaming {
            return false;
        }
        let total = self.full_text.graphemes(true).count();
        let backlog = total.saturating_sub(self.revealed_graphemes);
        let speed = if backlog > 180 {
            240.0
        } else {
            45.0 + (backlog as f32 * 0.45).min(95.0)
        };
        self.reveal_credit += elapsed_seconds * speed;
        let advance = self.reveal_credit.floor() as usize;
        self.reveal_credit -= advance as f32;
        let next = (self.revealed_graphemes + advance).min(total);
        let changed = next != self.revealed_graphemes;
        self.revealed_graphemes = next;
        changed
    }
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
    pub conversation: Vec<ConversationItem>,
    pub composer: String,
    pub composer_attachments: Vec<ImageAttachment>,
    pub search: String,
    pub language: Language,
    pub bash_policy: BashPolicy,
    pub agent_status: AgentStatus,
    pub pending_steering: Vec<String>,
    pub pending_follow_up: Vec<String>,
    pub current_model: Option<ModelOption>,
    pub available_models: Vec<ModelOption>,
    pub thinking_level: String,
    pub available_thinking_levels: Vec<String>,
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
    pub session_metrics: Option<SessionMetrics>,
    pub provider_profiles: Vec<ProviderProfile>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum AgentStatus {
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
    SetComposer(String),
    AddComposerAttachment(ImageAttachment),
    ClearComposerAttachments,
    SetSearch(String),
    SetLanguage(Language),
    SetBashPolicy(BashPolicy),
    ProviderProfilesLoaded(Vec<ProviderProfile>),
    RuntimeControlsUpdated {
        current_model: Option<ModelOption>,
        available_models: Vec<ModelOption>,
        thinking_level: String,
        available_thinking_levels: Vec<String>,
        steering_mode: QueueMode,
        follow_up_mode: QueueMode,
    },
    SessionMetricsUpdated(SessionMetrics),
    SetAgentStatus(AgentStatus),
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
    SkipTypewriter(String),
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
                self.selected_project = Some(project_id);
                self.selected_session = None;
                self.conversation.clear();
            }
            Action::SelectSession(session_id) => self.selected_session = Some(session_id),
            Action::SetComposer(value) => self.composer = value,
            Action::AddComposerAttachment(attachment) => self.composer_attachments.push(attachment),
            Action::ClearComposerAttachments => self.composer_attachments.clear(),
            Action::SetSearch(value) => self.search = value,
            Action::SetLanguage(language) => self.language = language,
            Action::SetBashPolicy(policy) => self.bash_policy = policy,
            Action::ProviderProfilesLoaded(mut profiles) => {
                profiles.sort_by(|left, right| {
                    left.name.to_lowercase().cmp(&right.name.to_lowercase())
                });
                self.provider_profiles = profiles;
            }
            Action::RuntimeControlsUpdated {
                current_model,
                mut available_models,
                thinking_level,
                available_thinking_levels,
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
                self.steering_mode = steering_mode;
                self.follow_up_mode = follow_up_mode;
            }
            Action::SessionMetricsUpdated(metrics) => self.session_metrics = Some(metrics),
            Action::SetAgentStatus(status) => self.agent_status = status,
            Action::UpsertConversation(item) => {
                if let Some(existing) = self
                    .conversation
                    .iter_mut()
                    .find(|message| message.id == item.id)
                {
                    let revealed_graphemes = existing
                        .revealed_graphemes
                        .min(item.full_text.graphemes(true).count());
                    let reveal_credit = existing.reveal_credit;
                    *existing = item;
                    existing.revealed_graphemes = revealed_graphemes;
                    existing.reveal_credit = reveal_credit;
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
                    message.reveal_all();
                }
            }
            Action::SkipTypewriter(id) => {
                if let Some(message) = self
                    .conversation
                    .iter_mut()
                    .find(|message| message.id == id)
                {
                    message.reveal_all();
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

    pub fn tick_typewriter(&mut self, elapsed_seconds: f32) -> bool {
        let mut changed = false;
        for message in self
            .conversation
            .iter_mut()
            .filter(|message| message.role == ConversationRole::Assistant)
        {
            changed |= message.advance_typewriter(elapsed_seconds);
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_never_splits_a_grapheme() {
        assert_eq!(grapheme_prefix("A👨‍👩‍👧‍👦B", 2), "A👨‍👩‍👧‍👦");
    }

    #[test]
    fn typewriter_catches_up_and_can_skip() {
        let mut item = ConversationItem {
            id: "a".into(),
            role: ConversationRole::Assistant,
            full_text: "hello".into(),
            revealed_graphemes: 0,
            reveal_credit: 0.0,
            streaming: true,
            tool_name: None,
            tool_details: None,
            is_error: false,
            attachments: Vec::new(),
        };
        assert!(item.advance_typewriter(0.1));
        item.reveal_all();
        assert_eq!(item.text_for_display(), "hello");
    }

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
}
