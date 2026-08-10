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

use crate::changes::{
    ChangeSet, CommitContext, CommitError, CommitSource, TransactionRevision,
    collect_changed_topics,
};

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
    revision: TransactionRevision,
}

impl EngineState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adopt an already-populated state, for callers that seed defaults before
    /// any action has been applied.
    pub fn from_state(state: AppState) -> Self {
        Self {
            state,
            revision: TransactionRevision::default(),
        }
    }

    /// Read access for rendering.
    pub fn get(&self) -> &AppState {
        &self.state
    }

    /// Read the revision of the most recently committed action batch.
    pub fn revision(&self) -> TransactionRevision {
        self.revision
    }

    /// Apply a complete action batch and publish one typed change set.
    ///
    /// A non-empty batch first preflights the next revision. If that checked
    /// increment overflows, no action is dispatched and the error is returned.
    /// Once preflight succeeds, every action is dispatched and the precomputed
    /// revision is assigned exactly once. The reducer itself has no fallible
    /// step, so no rollback is needed after preflight succeeds. An empty batch
    /// returns an explicit no-op change set at the current revision and does
    /// not advance it.
    pub fn apply_batch<I>(
        &mut self,
        actions: I,
        context: CommitContext,
    ) -> Result<ChangeSet, CommitError>
    where
        I: IntoIterator<Item = Action>,
    {
        let actions: Vec<Action> = actions.into_iter().collect();
        let action_count = actions.len();
        let changed_topics = collect_changed_topics(&actions);
        let next_revision = if action_count == 0 {
            None
        } else {
            Some(self.revision.checked_next()?)
        };

        for action in actions {
            self.state.dispatch(action);
        }

        let Some(revision) = next_revision else {
            return Ok(ChangeSet {
                revision: self.revision,
                scope: context.scope,
                source: context.source,
                changed_topics,
                action_count,
                coalesced: context.coalesced,
            });
        };
        self.revision = revision;

        Ok(ChangeSet {
            revision,
            scope: context.scope,
            source: context.source,
            changed_topics,
            action_count,
            coalesced: context.coalesced,
        })
    }

    /// Apply `action` through the reducer, returning any view-local follow-up.
    ///
    /// This legacy facade commits the action as one global
    /// [`CommitSource::InternalEffect`] batch. Callers that render should act
    /// on the returned effect; callers that do not can ignore it. The historic
    /// signature cannot expose [`CommitError`], so a failed legacy commit is
    /// represented by `None` and does not apply a view effect. Normal calls
    /// retain the previous reducer and `ViewEffect` behavior.
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
        match self.apply_batch(
            std::iter::once(action),
            CommitContext::global(CommitSource::InternalEffect),
        ) {
            Ok(_) => effect,
            Err(_error) => None,
        }
    }

    /// The project whose sessions are currently shown, if any.
    pub fn selected_project(&self) -> Option<ProjectId> {
        self.state.selected_project
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{CommitScope, SessionIdentity, StateTopic};
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

    #[test]
    fn a_batch_applies_every_action_and_allocates_one_revision() {
        let mut engine = EngineState::new();
        let change_set = engine
            .apply_batch(
                [
                    Action::SetSessionStatus(SessionStatus::Starting),
                    Action::SetSessionStatus(SessionStatus::Ready),
                    Action::QueueUpdated {
                        steering: vec!["steer".into()],
                        follow_up: vec!["follow".into()],
                    },
                ],
                CommitContext::global_coalesced(CommitSource::RuntimeEvent),
            )
            .expect("a normal revision should commit");

        assert_eq!(change_set.revision, TransactionRevision::new(1));
        assert_eq!(engine.revision(), TransactionRevision::new(1));
        assert_eq!(change_set.action_count, 3);
        assert_eq!(
            change_set.changed_topics,
            vec![StateTopic::SessionRuntime, StateTopic::Queue,]
        );
        assert!(change_set.coalesced);
        assert_eq!(engine.get().session_status, SessionStatus::Ready);
        assert_eq!(engine.get().pending_steering, vec!["steer"]);
        assert_eq!(engine.get().pending_follow_up, vec!["follow"]);
    }

    #[test]
    fn an_empty_batch_is_an_explicit_noop_without_revision_change() {
        let mut engine = EngineState::new();
        let context = CommitContext::global(CommitSource::Test);
        let change_set = engine
            .apply_batch(Vec::<Action>::new(), context)
            .expect("an empty batch cannot overflow");

        assert!(change_set.is_noop());
        assert_eq!(change_set.revision, TransactionRevision::ZERO);
        assert_eq!(engine.revision(), TransactionRevision::ZERO);
        assert_eq!(change_set.scope, CommitScope::Global);
        assert_eq!(change_set.source, CommitSource::Test);
        assert!(change_set.changed_topics.is_empty());
    }

    #[test]
    fn a_session_context_is_preserved_in_the_change_set() {
        let mut engine = EngineState::new();
        let identity = SessionIdentity::with_ids(
            crate::mailbox::SessionToken::next(),
            3,
            Some(Uuid::nil()),
            Some(Uuid::from_u128(1)),
        );
        let change_set = engine
            .apply_batch(
                [Action::SetSessionStatus(SessionStatus::Streaming)],
                CommitContext::session(identity, CommitSource::UserCommand),
            )
            .expect("a normal revision should commit");

        assert_eq!(change_set.scope, CommitScope::Session(identity));
        assert_eq!(change_set.source, CommitSource::UserCommand);
    }

    #[test]
    fn revision_overflow_returns_an_error_without_applying_actions() {
        let mut engine = EngineState::new();
        engine.revision = TransactionRevision::MAX;

        let result = engine.apply_batch(
            [Action::SetSessionStatus(SessionStatus::Ready)],
            CommitContext::global(CommitSource::Test),
        );

        assert_eq!(
            result,
            Err(CommitError::RevisionOverflow {
                current: TransactionRevision::MAX,
            })
        );
        assert_eq!(engine.revision(), TransactionRevision::MAX);
        assert_eq!(engine.get().session_status, SessionStatus::Offline);
    }

    #[test]
    fn legacy_apply_swallows_commit_error_without_applying_or_effect() {
        let mut engine = EngineState::new();
        engine.revision = TransactionRevision::MAX;
        let profile = provider("Overflow");

        let effect = engine.apply(Action::ProviderProfilesLoaded(vec![profile]));

        assert_eq!(effect, None);
        assert_eq!(engine.revision(), TransactionRevision::MAX);
        assert!(engine.get().provider_profiles.is_empty());
    }

    #[test]
    fn legacy_apply_still_runs_the_reducer_and_returns_view_effects() {
        let mut engine = EngineState::new();
        let profile = provider("Legacy");

        let effect = engine.apply(Action::ProviderProfilesLoaded(vec![profile.clone()]));

        assert_eq!(effect, Some(ViewEffect::ProvidersReloaded(vec![profile])));
        assert_eq!(engine.revision(), TransactionRevision::new(1));
        assert_eq!(engine.get().provider_profiles.len(), 1);
    }
}
