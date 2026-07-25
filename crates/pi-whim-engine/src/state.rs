//! Ownership of the domain state, and the notifications a view needs.
//!
//! [`AppState`] used to live on the egui `Workbench`, which made the view the
//! holder of record: the app read `workbench.state.bash_policy` to launch a
//! process, and preferences were saved by reading back out of the widget tree.
//! That only worked because there was exactly one view.
//!
//! Here the engine owns the state and applies every [`Action`] through the
//! reducer. Views observe. What a view still needs is a way to react to
//! specific actions — a conversation reset should drop cached layouts, loading
//! provider profiles should reseed an edit draft — so [`EngineState::apply`]
//! reports the action back to the caller as a [`ViewEffect`] instead of the
//! view intercepting actions on the way past.

use pi_whim_core::{Action, AppState, Project, ProjectId, ProviderProfile, SearchEngineProfile};

/// Something a view may need to do in response to an applied action.
///
/// The reducer has already run by the time one of these is handed back; it
/// describes view-local bookkeeping, never domain changes.
#[derive(Clone, Debug, PartialEq)]
pub enum ViewEffect {
    /// Provider profiles were replaced; reseed any provider edit draft.
    ProvidersReloaded(Vec<ProviderProfile>),
    /// Search engine profiles were replaced; reseed any search engine draft.
    SearchEnginesReloaded(Vec<SearchEngineProfile>),
    /// Projects were loaded; a view may want to expand them on first load.
    ProjectsLoaded(Vec<Project>),
    /// The conversation was cleared; drop anything cached per message.
    ConversationCleared,
}

/// The domain state, plus the reducer that is the only way to change it.
#[derive(Default)]
pub struct EngineState {
    state: AppState,
}

impl EngineState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read access for rendering.
    pub fn get(&self) -> &AppState {
        &self.state
    }

    /// Apply `action` through the reducer, returning any view-local follow-up.
    ///
    /// Callers that render should act on the returned effect; callers that do
    /// not can ignore it.
    pub fn apply(&mut self, action: Action) -> Option<ViewEffect> {
        let effect = match &action {
            Action::ProviderProfilesLoaded(profiles) => {
                Some(ViewEffect::ProvidersReloaded(profiles.clone()))
            }
            Action::SearchEngineProfilesLoaded(profiles) => {
                Some(ViewEffect::SearchEnginesReloaded(profiles.clone()))
            }
            Action::ProjectsLoaded(projects) => Some(ViewEffect::ProjectsLoaded(projects.clone())),
            Action::ClearConversation => Some(ViewEffect::ConversationCleared),
            _ => None,
        };
        self.state.dispatch(action);
        effect
    }

    /// The project whose sessions are currently shown, if any.
    pub fn selected_project(&self) -> Option<ProjectId> {
        self.state.selected_project
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{
        ConversationItem, ConversationRole, ProviderProtocol, SessionStatus, SessionSummary,
        stable_session_id,
    };
    use uuid::Uuid;

    fn project(name: &str) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: format!("/tmp/{name}"),
            pinned: false,
            last_opened_ms: 1,
        }
    }

    fn provider(name: &str) -> ProviderProfile {
        ProviderProfile {
            id: Uuid::new_v4(),
            name: name.into(),
            base_url: "https://example.test".into(),
            protocol: ProviderProtocol::default(),
            models: Vec::new(),
            updated_at_ms: 1,
            has_api_key: false,
        }
    }

    fn message(id: &str) -> ConversationItem {
        ConversationItem {
            id: id.into(),
            role: ConversationRole::Assistant,
            full_text: "hello".into(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn actions_reach_the_reducer() {
        let mut engine = EngineState::new();
        engine.apply(Action::SetSessionStatus(SessionStatus::Ready));
        assert_eq!(engine.get().session_status, SessionStatus::Ready);
    }

    #[test]
    fn most_actions_need_no_view_follow_up() {
        let mut engine = EngineState::new();
        assert_eq!(
            engine.apply(Action::SetSessionStatus(SessionStatus::Ready)),
            None
        );
        assert_eq!(engine.apply(Action::UpsertConversation(message("a"))), None);
    }

    #[test]
    fn reloading_providers_asks_the_view_to_reseed_its_draft() {
        let mut engine = EngineState::new();
        let profile = provider("Example");
        let effect = engine.apply(Action::ProviderProfilesLoaded(vec![profile.clone()]));

        // The reducer ran, and the view was told to reseed.
        assert_eq!(engine.get().provider_profiles.len(), 1);
        assert_eq!(effect, Some(ViewEffect::ProvidersReloaded(vec![profile])));
    }

    #[test]
    fn clearing_the_conversation_asks_the_view_to_drop_caches() {
        let mut engine = EngineState::new();
        engine.apply(Action::UpsertConversation(message("a")));
        assert!(!engine.get().conversation.is_empty());

        let effect = engine.apply(Action::ClearConversation);

        assert!(engine.get().conversation.is_empty());
        assert_eq!(effect, Some(ViewEffect::ConversationCleared));
    }

    #[test]
    fn loading_projects_reports_them_for_first_time_expansion() {
        let mut engine = EngineState::new();
        let project = project("example");
        let effect = engine.apply(Action::ProjectsLoaded(vec![project.clone()]));

        assert_eq!(engine.get().projects.len(), 1);
        assert_eq!(effect, Some(ViewEffect::ProjectsLoaded(vec![project])));
    }

    #[test]
    fn selected_project_reads_through_to_state() {
        let mut engine = EngineState::new();
        let project = project("example");
        let project_id = project.id;
        engine.apply(Action::ProjectsLoaded(vec![project]));
        engine.apply(Action::SelectProject(project_id));

        assert_eq!(engine.selected_project(), Some(project_id));
    }

    #[test]
    fn sessions_loaded_needs_no_view_follow_up() {
        // Session lists render straight from state with nothing cached
        // alongside them, so this variant deliberately has no effect.
        let mut engine = EngineState::new();
        let project = project("example");
        let project_id = project.id;
        engine.apply(Action::ProjectsLoaded(vec![project]));

        let pi_path = "/tmp/example/session.jsonl";
        let summary = SessionSummary {
            id: stable_session_id(pi_path),
            project_id,
            pi_path: pi_path.into(),
            title: "Example".into(),
            preview: String::new(),
            updated_at_ms: 0,
        };
        assert_eq!(
            engine.apply(Action::SessionsLoaded {
                project_id,
                sessions: vec![summary],
            }),
            None
        );
    }
}
