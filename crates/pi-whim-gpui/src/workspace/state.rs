//! Topic-scoped projections for the Workspace domain cache.
//!
//! Each projection contains only the fields needed by one feature family. The
//! application owns the reducer; these values replay committed slices into the
//! GPUI shell without transporting a complete [`AppState`] snapshot.

use std::collections::{BTreeMap, HashSet};

use pi_whim_core::{
    AgentTeamConfig, AppState, BashPolicy, ConversationItem, HookAuditSummary, Language,
    ModelOption, OneShotAiConfig, Project, ProjectHookStatus, ProjectId, ProviderProfile,
    QueueMode, SearchEngineProfile, SessionId, SessionMetrics, SessionStatus, SessionSummary,
    SlashCommandInfo, ThinkingLevel,
};
use pi_whim_engine::{ChangeSet, ReplaySelection, StateSelector, StateTopic};
use pi_whim_signal::StateSignal;

#[derive(Clone, PartialEq)]
pub struct NavigationProjection {
    projects: Vec<Project>,
    sessions: BTreeMap<ProjectId, Vec<SessionSummary>>,
    selected_project: Option<ProjectId>,
    selected_session: Option<SessionId>,
    running_sessions: HashSet<String>,
    language: Language,
}

impl NavigationProjection {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            projects: state.projects.clone(),
            sessions: state.sessions.clone(),
            selected_project: state.selected_project,
            selected_session: state.selected_session,
            running_sessions: state.running_sessions.clone(),
            language: state.language,
        }
    }

    pub(super) fn apply_to(self, state: &mut AppState) {
        state.projects = self.projects;
        state.sessions = self.sessions;
        state.selected_project = self.selected_project;
        state.selected_session = self.selected_session;
        state.running_sessions = self.running_sessions;
        state.language = self.language;
    }
}

#[derive(Clone, PartialEq)]
pub struct ConversationProjection {
    selected_project: Option<ProjectId>,
    selected_session: Option<SessionId>,
    conversation: Vec<ConversationItem>,
    language: Language,
    session_status: SessionStatus,
    pending_steering: Vec<String>,
    pending_follow_up: Vec<String>,
}

impl ConversationProjection {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            selected_project: state.selected_project,
            selected_session: state.selected_session,
            conversation: state.conversation.clone(),
            language: state.language,
            session_status: state.session_status.clone(),
            pending_steering: state.pending_steering.clone(),
            pending_follow_up: state.pending_follow_up.clone(),
        }
    }

    pub(super) fn selected_session(&self) -> Option<SessionId> {
        self.selected_session
    }

    pub(super) fn conversation_is_empty(&self) -> bool {
        self.conversation.is_empty()
    }

    pub(super) fn session_status(&self) -> &SessionStatus {
        &self.session_status
    }

    pub(super) fn apply_to(self, state: &mut AppState) {
        state.selected_project = self.selected_project;
        state.selected_session = self.selected_session;
        state.conversation = self.conversation;
        state.language = self.language;
        state.session_status = self.session_status;
        state.pending_steering = self.pending_steering;
        state.pending_follow_up = self.pending_follow_up;
    }
}

#[derive(Clone, PartialEq)]
pub struct RuntimeProjection {
    selected_project: Option<ProjectId>,
    session_status: SessionStatus,
    language: Language,
    current_model: Option<ModelOption>,
    pending_model: Option<ModelOption>,
    available_models: Vec<ModelOption>,
    thinking_level: ThinkingLevel,
    available_thinking_levels: Vec<ThinkingLevel>,
    available_commands: Vec<SlashCommandInfo>,
    auto_compaction_enabled: bool,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    session_metrics: Option<SessionMetrics>,
}

impl RuntimeProjection {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            selected_project: state.selected_project,
            session_status: state.session_status.clone(),
            language: state.language,
            current_model: state.current_model.clone(),
            pending_model: state.pending_model.clone(),
            available_models: state.available_models.clone(),
            thinking_level: state.thinking_level,
            available_thinking_levels: state.available_thinking_levels.clone(),
            available_commands: state.available_commands.clone(),
            auto_compaction_enabled: state.auto_compaction_enabled,
            steering_mode: state.steering_mode,
            follow_up_mode: state.follow_up_mode,
            session_metrics: state.session_metrics.clone(),
        }
    }

    pub(super) fn apply_to(self, state: &mut AppState) {
        state.selected_project = self.selected_project;
        state.session_status = self.session_status;
        state.language = self.language;
        state.current_model = self.current_model;
        state.pending_model = self.pending_model;
        state.available_models = self.available_models;
        state.thinking_level = self.thinking_level;
        state.available_thinking_levels = self.available_thinking_levels;
        state.available_commands = self.available_commands;
        state.auto_compaction_enabled = self.auto_compaction_enabled;
        state.steering_mode = self.steering_mode;
        state.follow_up_mode = self.follow_up_mode;
        state.session_metrics = self.session_metrics;
    }
}

#[derive(Clone, PartialEq)]
pub struct SettingsProjection {
    selected_project: Option<ProjectId>,
    language: Language,
    bash_policy: BashPolicy,
    bash_blocked_patterns: Vec<String>,
    agent_team_config: AgentTeamConfig,
    one_shot_ai_config: OneShotAiConfig,
    auto_compaction_enabled: bool,
    steering_mode: QueueMode,
    follow_up_mode: QueueMode,
    provider_profiles: Vec<ProviderProfile>,
    search_engine_profiles: Vec<SearchEngineProfile>,
    project_hook_status: ProjectHookStatus,
    hook_audit: Vec<HookAuditSummary>,
}

impl SettingsProjection {
    pub fn from_state(state: &AppState) -> Self {
        Self {
            selected_project: state.selected_project,
            language: state.language,
            bash_policy: state.bash_policy,
            bash_blocked_patterns: state.bash_blocked_patterns.clone(),
            agent_team_config: state.agent_team_config.clone(),
            one_shot_ai_config: state.one_shot_ai_config.clone(),
            auto_compaction_enabled: state.auto_compaction_enabled,
            steering_mode: state.steering_mode,
            follow_up_mode: state.follow_up_mode,
            provider_profiles: state.provider_profiles.clone(),
            search_engine_profiles: state.search_engine_profiles.clone(),
            project_hook_status: state.project_hook_status.clone(),
            hook_audit: state.hook_audit.clone(),
        }
    }

    pub(super) fn apply_to(self, state: &mut AppState) {
        state.selected_project = self.selected_project;
        state.language = self.language;
        state.bash_policy = self.bash_policy;
        state.bash_blocked_patterns = self.bash_blocked_patterns;
        state.agent_team_config = self.agent_team_config;
        state.one_shot_ai_config = self.one_shot_ai_config;
        state.auto_compaction_enabled = self.auto_compaction_enabled;
        state.steering_mode = self.steering_mode;
        state.follow_up_mode = self.follow_up_mode;
        state.provider_profiles = self.provider_profiles;
        state.search_engine_profiles = self.search_engine_profiles;
        state.project_hook_status = self.project_hook_status;
        state.hook_audit = self.hook_audit;
    }
}

/// Replay-capable feature selections owned by the Host.
#[derive(Clone)]
pub struct WorkspaceStateSelections {
    navigation: ReplaySelection<NavigationProjection>,
    conversation: ReplaySelection<ConversationProjection>,
    runtime: ReplaySelection<RuntimeProjection>,
    settings: ReplaySelection<SettingsProjection>,
}

impl WorkspaceStateSelections {
    pub fn new(state: &AppState) -> Self {
        let navigation = StateSelector::new(
            [
                StateTopic::Projects,
                StateTopic::Sessions,
                StateTopic::Selection,
                StateTopic::SessionRuntime,
                StateTopic::Preferences,
            ],
            NavigationProjection::from_state,
        )
        .expect("navigation projection declares a fixed non-empty topic set");
        let conversation = StateSelector::new(
            [
                StateTopic::Selection,
                StateTopic::Conversation,
                StateTopic::Queue,
                StateTopic::SessionRuntime,
                StateTopic::Preferences,
            ],
            ConversationProjection::from_state,
        )
        .expect("conversation projection declares a fixed non-empty topic set");
        let runtime = StateSelector::new(
            [
                StateTopic::Selection,
                StateTopic::SessionRuntime,
                StateTopic::RuntimeControls,
                StateTopic::Preferences,
            ],
            RuntimeProjection::from_state,
        )
        .expect("runtime projection declares a fixed non-empty topic set");
        let settings = StateSelector::new(
            [
                StateTopic::Selection,
                StateTopic::Preferences,
                StateTopic::RuntimeControls,
                StateTopic::Providers,
                StateTopic::SearchEngines,
                StateTopic::Hooks,
            ],
            SettingsProjection::from_state,
        )
        .expect("settings projection declares a fixed non-empty topic set");

        Self {
            navigation: ReplaySelection::new(navigation, state),
            conversation: ReplaySelection::new(conversation, state),
            runtime: ReplaySelection::new(runtime, state),
            settings: ReplaySelection::new(settings, state),
        }
    }

    pub fn navigation_signal(&self) -> StateSignal<NavigationProjection> {
        self.navigation.signal()
    }

    pub fn conversation_signal(&self) -> StateSignal<ConversationProjection> {
        self.conversation.signal()
    }

    pub fn runtime_signal(&self) -> StateSignal<RuntimeProjection> {
        self.runtime.signal()
    }

    pub fn settings_signal(&self) -> StateSignal<SettingsProjection> {
        self.settings.signal()
    }

    /// Recompute only selections whose topic sets intersect this commit.
    pub fn publish(&self, change_set: &ChangeSet, state: &AppState) -> usize {
        [
            self.navigation.publish(change_set, state),
            self.conversation.publish(change_set, state),
            self.runtime.publish(change_set, state),
            self.settings.publish(change_set, state),
        ]
        .into_iter()
        .filter(|published| *published)
        .count()
    }

    #[cfg(test)]
    fn topics(&self) -> [&[StateTopic]; 4] {
        [
            self.navigation.topics(),
            self.conversation.topics(),
            self.runtime.topics(),
            self.settings.topics(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_engine::{CommitScope, CommitSource, TransactionRevision};

    fn change_set(topic: StateTopic) -> ChangeSet {
        ChangeSet {
            revision: TransactionRevision::new(1),
            scope: CommitScope::Global,
            source: CommitSource::InternalEffect,
            changed_topics: vec![topic],
            action_count: 1,
            coalesced: false,
        }
    }

    #[test]
    fn selectors_cover_feature_topics_without_full_state_projection() {
        let selections = WorkspaceStateSelections::new(&AppState::default());
        let topics = selections.topics();

        assert!(topics[0].contains(&StateTopic::Projects));
        assert!(topics[1].contains(&StateTopic::Conversation));
        assert!(topics[1].contains(&StateTopic::Queue));
        assert!(topics[2].contains(&StateTopic::RuntimeControls));
        assert!(topics[3].contains(&StateTopic::Providers));
        assert!(topics[3].contains(&StateTopic::SearchEngines));
        assert!(topics[3].contains(&StateTopic::Hooks));
    }

    #[test]
    fn one_commit_publishes_only_changed_matching_projections() {
        let initial = AppState::default();
        let selections = WorkspaceStateSelections::new(&initial);
        let mut changed = initial;
        changed.pending_steering.push("queued".into());

        assert_eq!(
            selections.publish(&change_set(StateTopic::Queue), &changed),
            1
        );
        assert_eq!(
            selections.publish(&change_set(StateTopic::Providers), &changed),
            0
        );
    }
}
