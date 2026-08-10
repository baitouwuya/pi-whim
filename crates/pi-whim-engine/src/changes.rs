//! Typed descriptions of reducer commits.
//!
//! A [`ChangeSet`] is the engine's transaction boundary: it reports which
//! stable state topics were touched after a reducer batch has completed without
//! exposing the actions, effects, full state snapshot, or Pi protocol payload.

use std::fmt;

use pi_whim_core::{Action, ProjectId, SessionId};

pub use crate::mailbox::SessionToken;

/// The monotonically increasing revision assigned to a committed action batch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TransactionRevision(u64);

/// An error raised while committing a reducer batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// The next revision cannot be represented by the `u64` revision counter.
    RevisionOverflow {
        /// The last successfully committed revision.
        current: TransactionRevision,
    },
}

impl TransactionRevision {
    /// The initial revision of a newly created engine state.
    pub const ZERO: Self = Self(0);

    /// The highest representable revision.
    pub const MAX: Self = Self(u64::MAX);

    /// Construct a revision from its persisted numeric representation.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the numeric representation of this revision.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Increment this revision without wrapping.
    pub fn checked_next(self) -> Result<Self, CommitError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(CommitError::RevisionOverflow { current: self })
    }
}

impl fmt::Display for TransactionRevision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RevisionOverflow { current } => {
                write!(formatter, "transaction revision overflow at {current}")
            }
        }
    }
}

impl std::error::Error for CommitError {}

/// The stable identity of a live session used to scope a commit.
///
/// The token identifies the process lifetime and does not change when the Pi
/// transcript path is re-keyed. The generation distinguishes later lifetimes
/// that may reuse a project or transcript identity. Project and session IDs
/// are optional because a session can be observed before persistence has
/// supplied them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SessionIdentity {
    /// The process-lifetime token assigned by the session pool.
    pub token: SessionToken,
    /// The generation of this session identity.
    pub generation: u64,
    /// The project associated with the session, when known.
    pub project_id: Option<ProjectId>,
    /// The persisted session identity, when known.
    pub session_id: Option<SessionId>,
}

impl SessionIdentity {
    /// Construct an identity before optional persisted IDs are available.
    pub const fn new(token: SessionToken, generation: u64) -> Self {
        Self {
            token,
            generation,
            project_id: None,
            session_id: None,
        }
    }

    /// Construct an identity with all currently known stable identifiers.
    pub const fn with_ids(
        token: SessionToken,
        generation: u64,
        project_id: Option<ProjectId>,
        session_id: Option<SessionId>,
    ) -> Self {
        Self {
            token,
            generation,
            project_id,
            session_id,
        }
    }

    /// Return the process-lifetime token.
    pub const fn session_token(self) -> SessionToken {
        self.token
    }
}

/// The state scope affected by a reducer commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommitScope {
    /// The commit changes state shared by all sessions.
    Global,
    /// The commit is associated with one live session identity.
    Session(SessionIdentity),
}

/// The producer that caused a reducer commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CommitSource {
    /// A state update translated from a runtime event.
    RuntimeEvent,
    /// A state update caused by a user command.
    UserCommand,
    /// A refresh of controls or other runtime-derived metadata.
    ControlRefresh,
    /// State loaded from persistence.
    PersistenceLoad,
    /// A state update produced by an internal engine effect.
    InternalEffect,
    /// A test-only commit source.
    Test,
}

/// The stable state topics exposed by reducer commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum StateTopic {
    Projects,
    Sessions,
    Selection,
    Conversation,
    SessionRuntime,
    RuntimeControls,
    Queue,
    Preferences,
    Providers,
    SearchEngines,
    Hooks,
    AgentsMd,
}

impl StateTopic {
    /// All topics in the contract's fixed declaration order.
    pub const ALL: [Self; 12] = [
        Self::Projects,
        Self::Sessions,
        Self::Selection,
        Self::Conversation,
        Self::SessionRuntime,
        Self::RuntimeControls,
        Self::Queue,
        Self::Preferences,
        Self::Providers,
        Self::SearchEngines,
        Self::Hooks,
        Self::AgentsMd,
    ];

    /// Return the stable snake_case name of this topic.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Projects => "projects",
            Self::Sessions => "sessions",
            Self::Selection => "selection",
            Self::Conversation => "conversation",
            Self::SessionRuntime => "session_runtime",
            Self::RuntimeControls => "runtime_controls",
            Self::Queue => "queue",
            Self::Preferences => "preferences",
            Self::Providers => "providers",
            Self::SearchEngines => "search_engines",
            Self::Hooks => "hooks",
            Self::AgentsMd => "agents_md",
        }
    }
}

impl fmt::Display for StateTopic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

const PROJECTS: &[StateTopic] = &[StateTopic::Projects];
const SESSIONS: &[StateTopic] = &[StateTopic::Sessions];
const SELECTION: &[StateTopic] = &[StateTopic::Selection];
const SELECTION_AND_CONVERSATION: &[StateTopic] =
    &[StateTopic::Selection, StateTopic::Conversation];
const SESSION_RUNTIME: &[StateTopic] = &[StateTopic::SessionRuntime];
const RUNTIME_CONTROLS: &[StateTopic] = &[StateTopic::RuntimeControls];
const QUEUE: &[StateTopic] = &[StateTopic::Queue];
const PREFERENCES: &[StateTopic] = &[StateTopic::Preferences];
const PROVIDERS: &[StateTopic] = &[StateTopic::Providers];
const SEARCH_ENGINES: &[StateTopic] = &[StateTopic::SearchEngines];
const HOOKS: &[StateTopic] = &[StateTopic::Hooks];
const AGENTS_MD: &[StateTopic] = &[StateTopic::AgentsMd];
const CONVERSATION: &[StateTopic] = &[StateTopic::Conversation];

/// Map one core action to the topics that its reducer can affect.
///
/// This is deliberately the single action-to-topic mapping seam. Consumers of
/// [`ChangeSet`] should use the topics here instead of matching [`Action`]
/// themselves. The order within a mapping follows [`StateTopic::ALL`].
pub fn topics_for_action(action: &Action) -> &'static [StateTopic] {
    match action {
        Action::ProjectsLoaded(_) => PROJECTS,
        Action::SessionsLoaded { .. } => SESSIONS,
        Action::SelectProject(_) => SELECTION_AND_CONVERSATION,
        Action::SelectSession(_) => SELECTION,
        Action::SessionRunning { .. } => SESSION_RUNTIME,
        Action::SetLanguage(_)
        | Action::SetBashPolicy(_)
        | Action::SetBashBlockedPatterns(_)
        | Action::SetAgentTeamConfig(_)
        | Action::SetOneShotAiConfig(_) => PREFERENCES,
        Action::SetProjectHookStatus(_) | Action::SetHookAudit(_) => HOOKS,
        Action::AgentsMdFilesLoaded(_) => AGENTS_MD,
        Action::ProviderProfilesLoaded(_) | Action::ProviderKeyStatusLoaded(_) => PROVIDERS,
        Action::SearchEngineProfilesLoaded(_) | Action::SearchEngineKeyStatusLoaded(_) => {
            SEARCH_ENGINES
        }
        Action::RuntimeControlsUpdated { .. }
        | Action::RuntimeCommandsUpdated(_)
        | Action::SetPendingModel(_) => RUNTIME_CONTROLS,
        Action::SessionMetricsUpdated(_) | Action::SetSessionStatus(_) => SESSION_RUNTIME,
        Action::UpsertConversation(_)
        | Action::RekeyConversation { .. }
        | Action::AppendAssistantText { .. }
        | Action::FinishMessage(_)
        | Action::ClearConversation => CONVERSATION,
        Action::QueueUpdated { .. } => QUEUE,
    }
}

/// Collect changed topics in first-seen order, removing duplicates.
///
/// First-seen order is deterministic for a given batch and preserves the
/// reducer's action order. No hash-based collection is used, so the result is
/// stable across processes and platforms.
pub(crate) fn collect_changed_topics(actions: &[Action]) -> Vec<StateTopic> {
    let mut topics = Vec::new();
    for action in actions {
        for topic in topics_for_action(action) {
            if !topics.contains(topic) {
                topics.push(*topic);
            }
        }
    }
    topics
}

/// Metadata supplied to a reducer commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CommitContext {
    /// The state scope associated with the commit.
    pub scope: CommitScope,
    /// The producer of the commit.
    pub source: CommitSource,
    /// Whether this batch was coalesced from multiple upstream updates.
    pub coalesced: bool,
}

impl CommitContext {
    /// Construct a context from all commit dimensions explicitly.
    pub const fn new(scope: CommitScope, source: CommitSource, coalesced: bool) -> Self {
        Self {
            scope,
            source,
            coalesced,
        }
    }

    /// Construct a non-coalesced global commit context.
    pub const fn global(source: CommitSource) -> Self {
        Self::new(CommitScope::Global, source, false)
    }

    /// Construct a coalesced global commit context.
    pub const fn global_coalesced(source: CommitSource) -> Self {
        Self::new(CommitScope::Global, source, true)
    }

    /// Construct a non-coalesced session commit context.
    pub const fn session(identity: SessionIdentity, source: CommitSource) -> Self {
        Self::new(CommitScope::Session(identity), source, false)
    }

    /// Construct a coalesced session commit context.
    pub const fn session_coalesced(identity: SessionIdentity, source: CommitSource) -> Self {
        Self::new(CommitScope::Session(identity), source, true)
    }

    /// Return a copy of this context with an explicit coalesced flag.
    pub const fn with_coalesced(self, coalesced: bool) -> Self {
        Self { coalesced, ..self }
    }
}

/// The typed result of one completed reducer batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangeSet {
    /// The revision assigned to this batch, or the unchanged revision for a
    /// no-op empty batch.
    pub revision: TransactionRevision,
    /// The scope supplied by the commit context.
    pub scope: CommitScope,
    /// The source supplied by the commit context.
    pub source: CommitSource,
    /// Topics touched by the batch in deterministic first-seen order.
    pub changed_topics: Vec<StateTopic>,
    /// The number of actions applied by the reducer.
    pub action_count: usize,
    /// Whether the upstream producer coalesced this batch.
    pub coalesced: bool,
}

impl ChangeSet {
    /// Whether this is the explicit no-op result returned for an empty batch.
    pub fn is_noop(&self) -> bool {
        self.action_count == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{
        Action, AgentTeamConfig, AgentsMdFiles, ConversationItem, ConversationRole, Language,
        ModelOption, ProjectHookStatus, QueueMode, SearchEngineProfile, SessionMetrics,
        SessionStatus, ThinkingLevel,
    };
    use uuid::Uuid;

    fn conversation_item() -> ConversationItem {
        ConversationItem {
            id: "message".into(),
            role: ConversationRole::Assistant,
            full_text: String::new(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }
    }

    fn all_actions() -> Vec<Action> {
        let project_id = Uuid::nil();
        let session_id = Uuid::from_u128(1);
        vec![
            Action::ProjectsLoaded(Vec::new()),
            Action::SessionsLoaded {
                project_id,
                sessions: Vec::new(),
            },
            Action::SelectProject(project_id),
            Action::SelectSession(session_id),
            Action::SessionRunning {
                path: String::new(),
                running: false,
            },
            Action::SetLanguage(Language::default()),
            Action::SetBashPolicy(Default::default()),
            Action::SetBashBlockedPatterns(Vec::new()),
            Action::SetAgentTeamConfig(AgentTeamConfig::default()),
            Action::SetOneShotAiConfig(Default::default()),
            Action::SetProjectHookStatus(ProjectHookStatus::default()),
            Action::SetHookAudit(Vec::new()),
            Action::AgentsMdFilesLoaded(AgentsMdFiles::default()),
            Action::ProviderProfilesLoaded(Vec::new()),
            Action::ProviderKeyStatusLoaded(Vec::new()),
            Action::SearchEngineProfilesLoaded(Vec::<SearchEngineProfile>::new()),
            Action::SearchEngineKeyStatusLoaded(Vec::new()),
            Action::RuntimeControlsUpdated {
                current_model: None,
                available_models: Vec::<ModelOption>::new(),
                thinking_level: ThinkingLevel::default(),
                available_thinking_levels: Vec::new(),
                auto_compaction_enabled: false,
                steering_mode: QueueMode::default(),
                follow_up_mode: QueueMode::default(),
            },
            Action::RuntimeCommandsUpdated(Vec::new()),
            Action::SetPendingModel(None),
            Action::SessionMetricsUpdated(SessionMetrics::default()),
            Action::SetSessionStatus(SessionStatus::default()),
            Action::UpsertConversation(conversation_item()),
            Action::RekeyConversation {
                from: "from".into(),
                to: "to".into(),
            },
            Action::AppendAssistantText {
                id: "message".into(),
                text: "text".into(),
            },
            Action::FinishMessage("message".into()),
            Action::QueueUpdated {
                steering: Vec::new(),
                follow_up: Vec::new(),
            },
            Action::ClearConversation,
        ]
    }

    #[test]
    fn every_action_maps_to_at_least_one_topic() {
        for action in all_actions() {
            assert!(!topics_for_action(&action).is_empty());
        }
    }

    #[test]
    fn mappings_cover_multiple_topics_when_selection_can_clear_conversation() {
        let topics = topics_for_action(&Action::SelectProject(Uuid::nil()));
        assert_eq!(topics, &[StateTopic::Selection, StateTopic::Conversation]);
    }

    #[test]
    fn checked_revision_increment_reports_overflow() {
        assert_eq!(
            TransactionRevision::new(41).checked_next(),
            Ok(TransactionRevision::new(42))
        );
        assert_eq!(
            TransactionRevision::MAX.checked_next(),
            Err(CommitError::RevisionOverflow {
                current: TransactionRevision::MAX,
            })
        );
    }

    #[test]
    fn context_constructors_preserve_scope_source_and_coalesced_flag() {
        let token = SessionToken::next();
        let identity =
            SessionIdentity::with_ids(token, 7, Some(Uuid::nil()), Some(Uuid::from_u128(1)));
        let session = CommitContext::session_coalesced(identity, CommitSource::RuntimeEvent);
        assert_eq!(session.scope, CommitScope::Session(identity));
        assert_eq!(session.source, CommitSource::RuntimeEvent);
        assert!(session.coalesced);
        assert_eq!(identity.session_token(), token);

        let global = CommitContext::global(CommitSource::PersistenceLoad);
        assert_eq!(global.scope, CommitScope::Global);
        assert_eq!(global.source, CommitSource::PersistenceLoad);
        assert!(!global.coalesced);
        assert!(global.with_coalesced(true).coalesced);
    }

    #[test]
    fn all_topics_are_fixed_and_unique() {
        let topics = StateTopic::ALL;
        for (index, topic) in topics.iter().enumerate() {
            assert!(!topics[..index].contains(topic));
        }
        assert_eq!(topics.len(), 12);
    }

    #[test]
    fn every_state_topic_has_a_stable_snake_case_name_and_display() {
        let expected = [
            (StateTopic::Projects, "projects"),
            (StateTopic::Sessions, "sessions"),
            (StateTopic::Selection, "selection"),
            (StateTopic::Conversation, "conversation"),
            (StateTopic::SessionRuntime, "session_runtime"),
            (StateTopic::RuntimeControls, "runtime_controls"),
            (StateTopic::Queue, "queue"),
            (StateTopic::Preferences, "preferences"),
            (StateTopic::Providers, "providers"),
            (StateTopic::SearchEngines, "search_engines"),
            (StateTopic::Hooks, "hooks"),
            (StateTopic::AgentsMd, "agents_md"),
        ];

        assert_eq!(expected.len(), StateTopic::ALL.len());
        for (topic, name) in expected {
            assert_eq!(topic.as_str(), name);
            assert_eq!(topic.to_string(), name);
        }
    }
}
