//! Orchestration: the session pool, the store, the keychain, and Pi.
//!
//! Split from `main.rs` so the entry point is only the entry point. Nothing here
//! knows which UI is mounted — typed command, state, and framework-effect signals
//! are what let the host be swapped without touching the orchestration.

mod hooks;
mod signals;

pub(crate) use hooks::{CommandHookController, CommandHookError};
pub(crate) use signals::ApplicationEffect;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs,
    path::{Path, PathBuf},
};

use pi_whim_core::{
    Action, AgentPermissionLevel, AppState, Attachment, ConversationItem, ConversationRole,
    HookAuditRecord, HookAuditSummary, Language, ModelOption, Project, ProjectHookStatus,
    ProjectId, ProviderId, ProviderProfile, ProviderProtocol, QueueMode, SESSION_TITLE_TASK_KIND,
    SearchEngineId, SearchEngineProfile, SessionMetrics, SessionStatus, SessionSummary, SubmitMode,
    ThinkingLevel, ToolAuditRecord, normalize_bash_patterns, normalize_provider_display_name,
    provider_name_key, stable_session_id, strings,
};
use pi_whim_one_shot_ai::{
    OneShotAiService, OneShotCompletion, OneShotErrorKind, OneShotRequestId,
    ResolvedOneShotAiConfig, SessionHistoryTitleTask, SessionTitleTask, fallback_session_title,
};
use pi_whim_persistence::{
    AppPreferences, AttachmentStore, HookAuditEntry, HookRepository, MacosKeychainStore,
    PreferencesRepository, ProjectRepository, ProviderRepository, SearchEngineRepository,
    SecretStore, SessionRepository, SqliteStore, ToolAuditEntry, persist_session_title_to_jsonl,
    session_summary_from_jsonl,
};
use pi_whim_runtime::{AgentRuntime, PiRpcRuntime, RuntimeEvent, RuntimeStart};
use serde_json::{Value, json};
use uuid::Uuid;

use pi_whim_catalog::ModelCapabilityResolver;
use pi_whim_engine::changes::{ChangeSet, CommitContext, CommitError, CommitScope, CommitSource};
use pi_whim_engine::commands::{
    AppCommand, CommandDiagnostic, CommandEnvelope, CommandLifecycle, ShellPaste,
};
use pi_whim_engine::mailbox::Delivery;
use pi_whim_engine::mailbox::SessionToken;
use pi_whim_engine::pool::{PendingPrompt, SessionPool, SessionRuntime, is_draft};
use pi_whim_engine::protocol::queue_mode_name;
use pi_whim_engine::providers::{
    configured_search_engine_api_keys, discover_models, normalize_base_url,
    provider_keychain_account, search_engine_keychain_account, valid_search_engine_url,
};
use pi_whim_engine::session::{
    attachment_from_path, bash_policy_name, canonical_path, ensure_agent_team_extension, now_ms,
    prompt_with_attachment_paths,
};
use pi_whim_engine::slash_commands::SlashCommand;
use pi_whim_engine::state::EngineState;
use pi_whim_engine::{controls, dialogs, events, launch, notice};
use pi_whim_signal::Signal;

/// A file picker the host should open, and what to do with the answer.
///
/// The orchestration decides *that* a picker is wanted — it owns the checks and
/// the notices — but cannot open one, so it names the intent and the host carries
/// it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Picker {
    /// Things to attach to the draft: files, folders, or a mix of both.
    ///
    /// One picker rather than one per kind. There used to be a menu asking which,
    /// but the platform dialog already lets the reader walk into a folder and pick
    /// either, so the menu only added a click before the same window opened.
    Attachments,
    /// A folder to register as a project.
    Project,
}

pub struct PiWhimApplication<R: AgentRuntime = PiRpcRuntime> {
    engine: EngineState,
    signals: signals::ApplicationSignals,
    store: Option<SqliteStore>,
    secrets: MacosKeychainStore,
    runtime_factory: Box<dyn Fn() -> R + Send>,
    sessions: SessionPool<R>,
    capability_resolver: ModelCapabilityResolver,
    sessions_root_override: Option<PathBuf>,
    agent_directory_override: Option<PathBuf>,
    hook_config_root_override: Option<PathBuf>,
    hook_host: hooks::ApplicationHookHost,
    hook_revisions: HashMap<ProjectId, String>,
    pending_hook_reloads: HashSet<ProjectId>,
    pending_hook_inputs: HashMap<ProjectId, VecDeque<PendingPrompt>>,
    attachment_store: AttachmentStore,
    /// Control-state refreshes in flight, tagged with the session they were
    /// asked about.
    #[allow(clippy::type_complexity)]
    control_updates: (
        crossbeam_channel::Sender<(Option<String>, Vec<Action>)>,
        crossbeam_channel::Receiver<(Option<String>, Vec<Action>)>,
    ),
    one_shot_ai: Option<OneShotAiService>,
    one_shot_generation: u64,
    one_shot_installs: (
        crossbeam_channel::Sender<OneShotInstall>,
        crossbeam_channel::Receiver<OneShotInstall>,
    ),
    one_shot_completions: (
        crossbeam_channel::Sender<OneShotCompletion>,
        crossbeam_channel::Receiver<OneShotCompletion>,
    ),
    pending_session_titles: HashMap<OneShotRequestId, PendingSessionTitle>,
    /// First-prompt title jobs held only while the keychain-backed service is
    /// initializing. Keeping the raw prompt here avoids losing auto naming to
    /// a startup race without persisting conversation text.
    deferred_session_titles: HashMap<SessionToken, DeferredSessionTitle>,
    /// Names sent by this process that may still produce a session-info echo.
    /// Keep all outstanding names: a fallback title can be acknowledged after
    /// the AI replacement has already been sent.
    expected_session_title_names: HashMap<SessionToken, VecDeque<String>>,
    title_eligible: HashSet<SessionToken>,
    title_attempted: HashSet<SessionToken>,
}

#[derive(Clone)]
struct PendingSessionTitle {
    target: SessionTitleTarget,
    generation: u64,
    fallback: String,
    baseline: String,
    source: SessionTitleSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessionTitleSource {
    Automatic,
    ExplicitSmartRename,
}

struct DeferredSessionTitle {
    content: String,
    fallback: String,
    baseline: String,
}

#[derive(Clone)]
enum SessionTitleTarget {
    Live(SessionToken),
    Stored { project_id: ProjectId, path: String },
}

type OneShotInstall = (u64, Option<ResolvedOneShotAiConfig>);
const MAX_EXPECTED_SESSION_TITLE_NAMES: usize = 8;

impl Default for PiWhimApplication<PiRpcRuntime> {
    fn default() -> Self {
        let mut engine = EngineState::new();
        let signals = signals::ApplicationSignals::default();
        let mut initial_actions = Vec::new();
        let capability_resolver = ModelCapabilityResolver::default();
        let store = SqliteStore::open_default()
            .map_err(|error| error.to_string())
            .ok();
        if let Some(store) = store.as_ref()
            && let Ok(projects) = store.list_projects()
        {
            let project_ids = projects
                .iter()
                .map(|project| project.id)
                .collect::<Vec<_>>();
            initial_actions.push(Action::ProjectsLoaded(projects));
            for project_id in project_ids {
                if let Ok(sessions) = store.list_sessions(project_id) {
                    initial_actions.push(Action::SessionsLoaded {
                        project_id,
                        sessions,
                    });
                }
            }
        }
        if let Some(store) = store.as_ref()
            && let Ok(preferences) = store.load_preferences()
        {
            initial_actions.push(Action::SetLanguage(preferences.language));
            initial_actions.push(Action::SetBashPolicy(preferences.bash_policy));
            initial_actions.push(Action::SetBashBlockedPatterns(
                preferences.bash_blocked_patterns,
            ));
            initial_actions.push(Action::SetAgentTeamConfig(preferences.agent_team_config));
            initial_actions.push(Action::SetOneShotAiConfig(preferences.one_shot_ai_config));
        }
        let mut search_engine_ids = Vec::new();
        if let Some(store) = store.as_ref()
            && let Ok(profiles) = store.list_search_engine_profiles()
        {
            search_engine_ids = profiles
                .iter()
                .filter(|profile| profile.kind.requires_api_key())
                .map(|profile| profile.id)
                .collect();
            initial_actions.push(Action::SearchEngineProfilesLoaded(profiles));
        }
        let mut provider_ids = Vec::new();
        if let Some(store) = store.as_ref()
            && let Ok(mut profiles) = store.list_provider_profiles()
        {
            for profile in &mut profiles {
                capability_resolver.enrich_profile(profile);
                let _ = store.save_provider_profile(profile);
            }
            provider_ids = profiles.iter().map(|profile| profile.id).collect();
            initial_actions.push(Action::ProviderProfilesLoaded(profiles));
        }
        let initial_commit_error = signals
            .publish_commit(engine.apply_batch(
                initial_actions,
                CommitContext::global(CommitSource::PersistenceLoad),
            ))
            .err();
        let mut application = Self {
            engine,
            signals,
            store,
            secrets: MacosKeychainStore::default(),
            runtime_factory: Box::new(PiRpcRuntime::default),
            sessions: SessionPool::new(),
            capability_resolver,
            sessions_root_override: None,
            agent_directory_override: None,
            hook_config_root_override: None,
            hook_host: hooks::ApplicationHookHost::default(),
            hook_revisions: HashMap::new(),
            pending_hook_reloads: HashSet::new(),
            pending_hook_inputs: HashMap::new(),
            attachment_store: AttachmentStore::open_default(),
            control_updates: crossbeam_channel::unbounded(),
            one_shot_ai: None,
            one_shot_generation: 0,
            one_shot_installs: crossbeam_channel::unbounded(),
            one_shot_completions: crossbeam_channel::unbounded(),
            pending_session_titles: HashMap::new(),
            deferred_session_titles: HashMap::new(),
            expected_session_title_names: HashMap::new(),
            title_eligible: HashSet::new(),
            title_attempted: HashSet::new(),
        };
        if let Some(error) = initial_commit_error {
            application.report_commit_error(error);
        }
        // Probing the keychain can block for a long time and this runs before
        // the window opens, so profiles start with their stored status and a
        // worker corrects them.
        application.refresh_provider_key_status(provider_ids);
        application.refresh_search_engine_key_status(search_engine_ids);
        application.rebuild_one_shot_ai();
        application
    }
}

impl<R: AgentRuntime> PiWhimApplication<R> {
    /// Read the domain state.
    ///
    /// The reducer is here rather than on the view, so orchestration does not
    /// care which UI is mounted; the shell is handed a snapshot of this.
    pub(crate) fn state(&self) -> &AppState {
        self.engine.get()
    }

    /// One of the app's own strings, in the language the user picked.
    ///
    /// These are written where the failure happens rather than in a view, so the
    /// language comes from stored state instead of a view's copy of it.
    fn say(&self, key: &str) -> &'static str {
        strings::text(key, self.state().language)
    }

    /// Report one of the app's own strings as a failure.
    fn report(&mut self, key: &str) {
        let message = self.say(key);
        self.emit_error(message);
    }

    /// Subscribe to committed domain changes without access to the emitter.
    #[allow(dead_code, reason = "reserved for the Host/GPUI signal bridge")]
    pub(crate) fn change_sets(&self) -> Signal<ChangeSet> {
        self.signals.change_sets()
    }

    /// Subscribe to metadata-only command lifecycle stages.
    #[allow(dead_code, reason = "reserved for the Host/GPUI signal bridge")]
    pub(crate) fn command_lifecycle(&self) -> Signal<CommandLifecycle> {
        self.signals.command_lifecycle()
    }

    /// Subscribe to reliable, ordered framework-owned effects.
    pub(crate) fn application_effects(&self) -> Signal<signals::ApplicationEffect> {
        self.signals.application_effects()
    }

    /// Flush bounded startup effects after the Host has subscribed.
    pub(crate) fn activate_effect_delivery(&self) {
        self.signals.activate_effect_delivery();
    }

    fn emit_effect(&self, effect: signals::ApplicationEffect) {
        self.signals.emit_effect(effect);
    }

    fn emit_error(&self, message: impl Into<String>) {
        self.emit_effect(signals::ApplicationEffect::Notice(notice::Notice::error(
            message,
        )));
    }

    fn emit_info(&self, message: impl Into<String>) {
        self.emit_effect(signals::ApplicationEffect::Notice(notice::Notice::info(
            message,
        )));
    }

    /// Wrap a UI command in authenticated metadata without exporting a session path.
    pub(crate) fn ui_command_envelope(&self, command: AppCommand) -> CommandEnvelope<AppCommand> {
        CommandEnvelope::ui(command).with_context(self.state().selected_project, None)
    }

    /// Select the shared project Hook controller for one authenticated envelope.
    pub(crate) fn command_hook_controller(
        &self,
        envelope: &CommandEnvelope<AppCommand>,
    ) -> Option<CommandHookController> {
        envelope
            .project_id()
            .and_then(|project_id| self.command_hook_controller_for_project(project_id))
    }

    /// Execute the typed payload after the Host has completed command control.
    pub(crate) fn execute_command_envelope(&mut self, envelope: CommandEnvelope<AppCommand>) {
        self.handle(envelope.into_payload());
    }

    /// Surface a bounded, metadata-only command diagnostic.
    pub(crate) fn report_command_diagnostic(&mut self, diagnostic: &CommandDiagnostic) {
        self.emit_error(diagnostic.as_str().to_owned());
    }

    /// Emit the local typed lifecycle stage and return its external observer.
    ///
    /// The Host schedules the observer off the GPUI thread after this returns,
    /// preserving local-before-external ordering without delaying the handler.
    pub(crate) fn emit_command_lifecycle(
        &self,
        lifecycle: CommandLifecycle,
    ) -> Option<CommandHookController> {
        self.signals.emit_command_lifecycle(lifecycle.clone());
        lifecycle
            .project_id()
            .and_then(|project_id| self.command_hook_controller_for_project(project_id))
    }

    fn command_hook_controller_for_project(
        &self,
        project_id: ProjectId,
    ) -> Option<CommandHookController> {
        let project = self.find_project(project_id)?;
        self.hook_host.controller(Path::new(&project.path))
    }

    fn observe_change_set(&self, change_set: &ChangeSet) {
        let project_id = match change_set.scope {
            CommitScope::Session(identity) => identity.project_id,
            // Global commits have no project identity of their own. Route them
            // through the currently selected project's shared scope when one
            // exists; with no selection there is no project Hook scope to use.
            CommitScope::Global => self.state().selected_project,
        };
        let Some(project_id) = project_id else {
            return;
        };
        if let Some(controller) = self.command_hook_controller_for_project(project_id) {
            controller.observe_change_set(change_set);
        }
    }

    /// Change the domain state through a one-action internal-effect commit.
    fn apply(&mut self, action: Action) {
        let _ = self.apply_batch(
            std::iter::once(action),
            CommitContext::global(CommitSource::InternalEffect),
        );
    }

    /// Commit an action batch and synchronously publish its typed change set.
    fn apply_batch<I>(&mut self, actions: I, context: CommitContext) -> Option<ChangeSet>
    where
        I: IntoIterator<Item = Action>,
    {
        let result = self.engine.apply_batch(actions, context);
        match self.signals.publish_commit(result) {
            Ok(change_set) => {
                self.observe_change_set(&change_set);
                Some(change_set)
            }
            Err(error) => {
                self.report_commit_error(error);
                None
            }
        }
    }

    fn report_commit_error(&mut self, error: CommitError) {
        let diagnostic = CommandDiagnostic::new(format!("state commit failed: {error}"));
        self.emit_error(diagnostic.as_str().to_owned());
    }

    /// The merged stream of every pooled session's events.
    pub(crate) fn session_events(&self) -> crossbeam_channel::Receiver<Delivery> {
        self.sessions.events()
    }

    /// The answers to the control-state RPCs issued on worker threads.
    #[allow(clippy::type_complexity)]
    pub(crate) fn control_answers(
        &self,
    ) -> crossbeam_channel::Receiver<(Option<String>, Vec<Action>)> {
        self.control_updates.1.clone()
    }

    pub(crate) fn one_shot_installs(
        &self,
    ) -> crossbeam_channel::Receiver<(u64, Option<ResolvedOneShotAiConfig>)> {
        self.one_shot_installs.1.clone()
    }

    pub(crate) fn one_shot_completions(&self) -> crossbeam_channel::Receiver<OneShotCompletion> {
        self.one_shot_completions.1.clone()
    }

    /// Apply the paths returned by a framework-owned picker.
    pub(crate) fn picked(&mut self, picker: Picker, paths: Vec<PathBuf>) {
        match picker {
            Picker::Attachments => self.attach_paths(paths),
            // One folder, so the picker was opened without `multiple`.
            Picker::Project => {
                if let Some(path) = paths.first() {
                    self.add_project_at(path);
                }
            }
        }
    }

    /// A signal that the online capability catalog has arrived.
    pub(crate) fn catalog_refreshed(&self) -> crossbeam_channel::Receiver<()> {
        self.capability_resolver.refreshed()
    }

    /// Re-enrich every stored provider now that the online catalog is known.
    ///
    /// Only worth doing once, when the fetch lands: the models a profile lists do
    /// not change on their own, and what the catalog says about them was already
    /// applied from the bundled table at startup.
    pub(crate) fn absorb_capability_catalog(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let Ok(mut profiles) = store.list_provider_profiles() else {
            return;
        };
        for profile in &mut profiles {
            let previous = profile.clone();
            self.capability_resolver.enrich_profile(profile);
            if *profile != previous {
                let _ = store.save_provider_profile(profile);
            }
        }
        self.reload_provider_profiles();
    }

    /// Carry out one request from the shell.
    ///
    /// The shell owns transient view state such as the cleared draft. Persistent
    /// transcript changes, the store, the pool, and Pi remain owned here.
    pub(crate) fn handle(&mut self, command: AppCommand) {
        match command {
            AppCommand::RemoveProject(project_id) => {
                self.stop_project_runtimes(project_id);
                if let Some(store) = self.store.as_ref()
                    && let Err(error) = store.delete_project(project_id)
                {
                    self.emit_error(error.to_string());
                }
                self.reload_projects();
            }
            AppCommand::OpenProject(project_id) => self.start_project(project_id),
            AppCommand::NewSession(project_id) => self.start_new_session(project_id),
            AppCommand::ActivateSession { project_id, path } => {
                self.switch_session(project_id, path)
            }
            AppCommand::RenameSession { path, title } => self.rename_session(path, title),
            AppCommand::CloneSession => self.clone_session(),
            AppCommand::DeleteSession(path) => self.delete_session(path),
            AppCommand::SubmitPrompt {
                content,
                attachments,
                mode,
            } => self.submit_prompt(content, attachments, mode),
            AppCommand::AnswerPrompt(answer) => self.send_answer(answer),
            AppCommand::DiscardAttachment(path) => {
                // Only the generated ones reach here — the composer decides that
                // — so this deletes the file it wrote without asking again.
                if let Err(error) = self.attachment_store.remove_generated(&path) {
                    self.emit_error(error);
                }
            }
            AppCommand::SetAutoCompaction(enabled) => self.set_auto_compaction(enabled),
            AppCommand::Stop => {
                if let Err(error) = self.active_command(json!({"type":"abort"})) {
                    self.emit_error(error);
                }
            }
            AppCommand::ClearQueue => {
                // Pi answers with the queue it dropped and a `queue_update`
                // event, which is what refreshes the snapshot.
                if let Err(error) = self.active_command(json!({"type":"clear_queue"})) {
                    self.emit_error(error);
                }
            }
            AppCommand::SetLanguage(language) => {
                self.apply(Action::SetLanguage(language));
                self.save_preferences();
            }
            AppCommand::SetBashPolicy(policy) => {
                if self.state().bash_policy != policy {
                    self.apply(Action::SetBashPolicy(policy));
                    self.save_preferences();
                    self.restart_selected_project();
                }
            }
            AppCommand::SetBlockedPatterns(patterns) => {
                let patterns = normalize_bash_patterns(patterns);
                if self.state().bash_blocked_patterns != patterns {
                    self.apply(Action::SetBashBlockedPatterns(patterns));
                    self.save_preferences();
                    self.restart_selected_project();
                }
            }
            AppCommand::SetPermissionLevel(level) => self.set_permission_level(level),
            AppCommand::LoadAgentsMdFiles
            | AppCommand::SaveGlobalAgentsMd(_)
            | AppCommand::SaveProjectAgentsMd(_) => {}
            AppCommand::ApproveProjectHooks {
                fingerprint,
                grants_hash,
            } => self.approve_project_hooks(&fingerprint, &grants_hash),
            AppCommand::RevokeProjectHooks => self.revoke_project_hooks(),
            AppCommand::SetAgentTeamConfig(config) => {
                let config = config.normalized();
                if self.state().agent_team_config != config {
                    self.apply(Action::SetAgentTeamConfig(config));
                    self.save_preferences();
                    self.restart_selected_project();
                }
            }
            AppCommand::SetOneShotAiConfig(config) => {
                let config = config.normalized();
                if self.state().one_shot_ai_config != config {
                    self.apply(Action::SetOneShotAiConfig(config));
                    self.save_preferences();
                    self.rebuild_one_shot_ai();
                }
            }
            AppCommand::SetModel(model) => self.queue_model_switch(model),
            AppCommand::SetThinkingLevel(level) => self.set_thinking_level(level),
            AppCommand::SetQueueModes {
                steering,
                follow_up,
            } => self.set_queue_modes(steering, follow_up),
            AppCommand::RunSlashCommand(command) => self.run_command(command),
            AppCommand::DeleteProvider(profile_id) => self.delete_provider(profile_id),
            AppCommand::SaveSearchEngines(profiles) => {
                self.save_search_engines(profiles);
            }
        }
    }

    fn approve_project_hooks(&mut self, expected_fingerprint: &str, expected_grants_hash: &str) {
        let Some(project) = self
            .state()
            .selected_project
            .and_then(|id| self.find_project(id))
        else {
            return;
        };
        let path = Path::new(&project.path).join(".pi-whim/hooks.json");
        let prepared = match fs::read(&path).and_then(|source| {
            hooks::manifest::prepare_manifest(&source, true, None)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        }) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.emit_error(error.to_string());
                return;
            }
        };
        if prepared.fingerprint != expected_fingerprint
            || prepared.grants_hash != expected_grants_hash
        {
            self.emit_error("project hook manifest or grants changed before approval");
            let _ = self.load_hooks(&project.path);
            return;
        }
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let record = pi_whim_persistence::HookProjectTrustRecord {
            fingerprint: prepared.fingerprint,
            grants_hash: prepared.grants_hash,
            grants: prepared.grants,
            approved_at_ms: now_ms(),
        };
        if let Err(error) = store.approve_project_hooks(&project.path, &record) {
            self.emit_error(error.to_string());
            return;
        }
        let _ = self.load_hooks(&project.path);
        self.request_hook_reload(project.id);
    }

    fn revoke_project_hooks(&mut self) {
        let Some(project) = self
            .state()
            .selected_project
            .and_then(|id| self.find_project(id))
        else {
            return;
        };
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.revoke_project_hooks(&project.path)
        {
            self.emit_error(error.to_string());
            return;
        }
        self.hook_host.revoke_project(Path::new(&project.path));
        let _ = self.load_hooks(&project.path);
        self.request_hook_reload(project.id);
    }

    /// Carry out one slash command.
    ///
    /// Only the ones that need something outside the shell: the palette fills the
    /// composer in for the rest itself.
    fn run_command(&mut self, command: SlashCommand) {
        match command {
            SlashCommand::NewSession => {
                if let Some(project_id) = self.state().selected_project {
                    self.start_new_session(project_id);
                }
            }
            SlashCommand::AddAttachment => {
                if self.state().selected_project.is_some() {
                    // Staged for the host: the picker needs the window, and the
                    // project check belongs here with the notice it produces.
                    self.emit_effect(signals::ApplicationEffect::OpenPicker(Picker::Attachments));
                } else {
                    self.report("notice-select-project-attachments");
                }
            }
            SlashCommand::SetModel(model) => self.queue_model_switch(model),
            SlashCommand::SetThinkingLevel(level) => self.set_thinking_level(level),
            SlashCommand::Compact => self.compact_session(),
            SlashCommand::Stop => {
                if let Err(error) = self.active_command(json!({"type":"abort"})) {
                    self.emit_error(error);
                }
            }
            SlashCommand::Clone => self.clone_session(),
            SlashCommand::Fork(entry_id) => self.fork_session(entry_id),
            SlashCommand::Share => self.share_session(),
            SlashCommand::Export(path) => self.export_session(path),
            SlashCommand::NameSession(Some(name)) => self.set_current_session_name(name),
            SlashCommand::CopyLastMessage => {
                // The streaming entry is skipped because a reply still arriving
                // would be copied half-written.
                if let Some(message) = self.state().conversation.iter().rev().find(|message| {
                    message.role == ConversationRole::Assistant
                        && !message.streaming
                        && !message.full_text.trim().is_empty()
                }) {
                    self.emit_effect(signals::ApplicationEffect::ClipboardWrite(
                        message.full_text.clone(),
                    ));
                }
            }
            SlashCommand::ShowSessionInfo => {
                let metrics = self.state().session_metrics.clone().unwrap_or_default();
                self.push_command_output(session_info(&metrics, self.state().language));
            }
            SlashCommand::ShowHotkeys => {
                self.push_command_output(hotkeys(self.state().language));
            }
            SlashCommand::ShowChangelog => {
                let heading = self.say("changelog");
                self.push_command_output(format!("{heading}: {CHANGELOG_URL}"));
            }
            // These only prefill the composer, which the palette does without
            // asking anyone, so they never travel as a request.
            SlashCommand::ChooseModel
            | SlashCommand::ChooseThinkingLevel
            | SlashCommand::ChooseFork
            | SlashCommand::NameSession(None)
            | SlashCommand::SubmitDynamic(_) => {
                unreachable!("the palette prefills these itself")
            }
        }
    }

    /// Answer a command in the conversation itself.
    ///
    /// The reducer's upsert rather than a notice: the text is long enough to want
    /// scrolling, and it belongs to the session it was asked in.
    fn push_command_output(&mut self, text: String) {
        self.apply(Action::UpsertConversation(ConversationItem {
            id: format!("slash-command-{}", now_ms()),
            role: ConversationRole::System,
            full_text: text,
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }));
    }

    /// Register `path` as a project.
    pub(crate) fn add_project_at(&mut self, path: &Path) {
        let path = canonical_path(path);
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("project")
            .to_owned();
        let project = Project {
            id: Uuid::new_v4(),
            name,
            path: path.to_string_lossy().into_owned(),
            pinned: false,
            last_opened_ms: now_ms(),
        };
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_project(&project)
        {
            self.emit_error(error.to_string());
        }
        self.reload_projects();
    }

    fn reload_projects(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let projects = match store.list_projects() {
            Ok(projects) => projects,
            Err(error) => {
                self.emit_error(error.to_string());
                return;
            }
        };
        // Every read happens before the first apply: reading and applying both
        // want `self`, and the store is the one part of it that cannot be
        // borrowed alongside the reducer.
        let sessions = projects
            .iter()
            .filter_map(|project| {
                store
                    .list_sessions(project.id)
                    .ok()
                    .map(|sessions| (project.id, sessions))
            })
            .collect::<Vec<_>>();

        self.apply(Action::ProjectsLoaded(projects));
        for (project_id, sessions) in sessions {
            self.apply(Action::SessionsLoaded {
                project_id,
                sessions,
            });
        }
    }

    fn save_preferences(&mut self) {
        let preferences = AppPreferences {
            language: self.state().language,
            bash_policy: self.state().bash_policy,
            bash_blocked_patterns: self.state().bash_blocked_patterns.clone(),
            agent_team_config: self.state().agent_team_config.clone(),
            one_shot_ai_config: self.state().one_shot_ai_config.clone(),
        };
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_preferences(preferences)
        {
            self.emit_error(error.to_string());
        }
    }

    /// Probe the keychain for each provider's key on a worker thread.
    ///
    /// A keychain read can block for a long time — long enough to be worth
    /// keeping off any path that has to draw — so profiles render with whatever
    /// status they were stored with and this corrects them.
    fn refresh_provider_key_status(&self, ids: Vec<ProviderId>) {
        if ids.is_empty() {
            return;
        }
        let sender = self.control_updates.0.clone();
        let key = self.sessions.active_key().map(str::to_owned);
        let secrets = self.secrets.clone();
        std::thread::spawn(move || {
            let statuses = ids
                .into_iter()
                .map(|id| {
                    let saved = secrets
                        .get(&provider_keychain_account(id))
                        .ok()
                        .flatten()
                        .is_some();
                    (id, saved)
                })
                .collect();
            let _ = sender.send((key, vec![Action::ProviderKeyStatusLoaded(statuses)]));
        });
    }

    fn refresh_search_engine_key_status(&self, ids: Vec<SearchEngineId>) {
        if ids.is_empty() {
            return;
        }
        let sender = self.control_updates.0.clone();
        let key = self.sessions.active_key().map(str::to_owned);
        let secrets = self.secrets.clone();
        std::thread::spawn(move || {
            let statuses = ids
                .into_iter()
                .map(|id| {
                    let saved = secrets
                        .get(&search_engine_keychain_account(id))
                        .ok()
                        .flatten()
                        .is_some();
                    (id, saved)
                })
                .collect();
            let _ = sender.send((key, vec![Action::SearchEngineKeyStatusLoaded(statuses)]));
        });
    }

    fn reload_provider_profiles(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.list_provider_profiles() {
            Ok(mut profiles) => {
                for profile in &mut profiles {
                    self.capability_resolver.enrich_profile(profile);
                }
                let ids: Vec<ProviderId> = profiles.iter().map(|profile| profile.id).collect();
                self.apply(Action::ProviderProfilesLoaded(profiles));
                self.refresh_provider_key_status(ids);
                self.rebuild_one_shot_ai();
            }
            Err(error) => self.emit_error(error.to_string()),
        }
    }

    /// Resolve the selected provider credential away from the UI thread. Every
    /// call retires the prior generation immediately; only the latest result may
    /// install a service when the keychain read returns.
    fn rebuild_one_shot_ai(&mut self) {
        let retired = self
            .pending_session_titles
            .drain()
            .map(|(_, pending)| {
                (
                    pending.target,
                    pending.fallback,
                    pending.baseline,
                    pending.source,
                )
            })
            .collect::<Vec<_>>();
        for (target, fallback, baseline, source) in retired {
            if source == SessionTitleSource::Automatic {
                self.apply_pending_session_title(target, fallback, &baseline, source, false);
            } else {
                self.report("notice-smart-rename-unavailable");
            }
        }
        self.one_shot_generation = self.one_shot_generation.wrapping_add(1);
        let generation = self.one_shot_generation;
        if let Some(service) = self.one_shot_ai.take() {
            service.shutdown();
        }

        let config = self.state().one_shot_ai_config.clone().normalized();
        let task = config.task(SESSION_TITLE_TASK_KIND);
        let profile = task
            .enabled
            .then_some(task.provider_id)
            .flatten()
            .and_then(|id| {
                self.state()
                    .provider_profiles
                    .iter()
                    .find(|profile| {
                        profile.id == id
                            && task.model_id.as_ref().is_some_and(|model_id| {
                                profile.models.iter().any(|model| &model.id == model_id)
                            })
                    })
                    .cloned()
            });
        let sender = self.one_shot_installs.0.clone();
        let secrets = self.secrets.clone();
        std::thread::spawn(move || {
            let resolved = profile.and_then(|profile| {
                let api_key = secrets
                    .get(&provider_keychain_account(profile.id))
                    .ok()
                    .flatten()?;
                ResolvedOneShotAiConfig::new(
                    generation,
                    &config,
                    SESSION_TITLE_TASK_KIND,
                    &profile,
                    api_key,
                )
                .ok()
            });
            let _ = sender.send((generation, resolved));
        });
    }

    pub(crate) fn settle_one_shot_install(
        &mut self,
        generation: u64,
        resolved: Option<ResolvedOneShotAiConfig>,
    ) {
        if generation != self.one_shot_generation {
            return;
        }
        let Some(resolved) = resolved else {
            return;
        };
        let service = OneShotAiService::new(resolved);
        let source = service.completion_receiver();
        let sink = self.one_shot_completions.0.clone();
        std::thread::spawn(move || {
            while let Ok(completion) = source.recv() {
                if sink.send(completion).is_err() {
                    return;
                }
            }
        });
        self.one_shot_ai = Some(service);
        self.submit_deferred_session_titles();
    }

    fn reload_search_engine_profiles(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.list_search_engine_profiles() {
            Ok(profiles) => {
                let ids = profiles
                    .iter()
                    .filter(|profile| profile.kind.requires_api_key())
                    .map(|profile| profile.id)
                    .collect();
                self.apply(Action::SearchEngineProfilesLoaded(profiles));
                self.refresh_search_engine_key_status(ids);
            }
            Err(error) => self.emit_error(error.to_string()),
        }
    }

    /// Store a provider, and its key if one was typed.
    ///
    /// Answers with the id the profile ended up under and whether a key really
    /// is in the keychain, because the badge beside the form is the one place
    /// that must not claim a key it has not seen. Nothing to report — the form
    /// did not validate — is `None`.
    pub(crate) fn save_provider(
        &mut self,
        mut profile: ProviderProfile,
        api_key: Option<String>,
    ) -> Option<(ProviderId, bool)> {
        profile.name = normalize_provider_display_name(&profile.name);
        if profile.name.trim().is_empty()
            || profile.base_url.trim().is_empty()
            || profile.models.is_empty()
        {
            self.report("provider-incomplete");
            return None;
        }
        if self.state().provider_profiles.iter().any(|existing| {
            existing.id != profile.id
                && provider_name_key(&existing.name) == provider_name_key(&profile.name)
        }) {
            let message = self.say("duplicate-provider-name");
            self.emit_error(message);
            return None;
        }
        profile.base_url = normalize_base_url(&profile.base_url);
        self.capability_resolver.enrich_profile(&mut profile);
        profile.updated_at_ms = now_ms();
        if let Some(api_key) = api_key {
            if let Err(error) = self
                .secrets
                .set(&provider_keychain_account(profile.id), &api_key)
            {
                self.emit_error(error.to_string());
                return None;
            }
            profile.has_api_key = self
                .secrets
                .get(&provider_keychain_account(profile.id))
                .map_err(|error| error.to_string())
                .ok()
                .flatten()
                .is_some();
            if !profile.has_api_key {
                self.report("notice-key-unreadable");
                // Reported as stored-nothing rather than as no answer: the write
                // was attempted, and the badge must not be left claiming a key.
                return Some((profile.id, false));
            }
        } else if self
            .secrets
            .get(&provider_keychain_account(profile.id))
            .ok()
            .flatten()
            .is_none()
        {
            self.report("notice-key-missing");
            return None;
        }
        let id = profile.id;
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_provider_profile(&profile)
        {
            self.emit_error(error.to_string());
            return None;
        }
        self.reload_provider_profiles();
        // Pi reads models.json at startup. Restart the active project to apply new providers.
        self.restart_selected_project();
        Some((id, true))
    }

    fn delete_provider(&mut self, profile_id: ProviderId) {
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.delete_provider_profile(profile_id)
        {
            self.emit_error(error.to_string());
            return;
        }
        if let Err(error) = self.secrets.delete(&provider_keychain_account(profile_id)) {
            self.emit_error(error.to_string());
        }
        self.reload_provider_profiles();
        self.restart_selected_project();
    }

    fn save_search_engines(&mut self, profiles: Vec<SearchEngineProfile>) -> bool {
        if let Some(invalid) = profiles.iter().find(|profile| {
            profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url)
        }) {
            let message = format!("{}: {}", invalid.name, self.say("search-engine-incomplete"));
            self.emit_error(message);
            return false;
        }
        let profiles = profiles
            .into_iter()
            .enumerate()
            .map(|(position, profile)| {
                let mut profile = profile.normalized();
                profile.position = position as u32;
                profile
            })
            .collect::<Vec<_>>();
        let stale_key_ids = self
            .state()
            .search_engine_profiles
            .iter()
            .filter(|existing| {
                existing.kind.requires_api_key()
                    && !profiles
                        .iter()
                        .any(|profile| profile.id == existing.id && profile.kind.requires_api_key())
            })
            .map(|profile| profile.id)
            .collect::<Vec<_>>();
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_search_engine_profiles(&profiles)
        {
            self.emit_error(error.to_string());
            return false;
        }
        for id in stale_key_ids {
            if let Err(error) = self.secrets.delete(&search_engine_keychain_account(id)) {
                self.emit_error(error.to_string());
            }
        }
        self.reload_search_engine_profiles();
        self.restart_selected_project();
        true
    }

    /// Store one search engine while keeping credentials out of persisted metadata.
    pub(crate) fn save_search_engine(
        &mut self,
        mut profile: SearchEngineProfile,
        api_key: Option<String>,
    ) -> bool {
        profile = profile.normalized();
        if profile.name.is_empty() || !valid_search_engine_url(&profile.base_url) {
            self.report("search-engine-incomplete");
            return false;
        }

        let account = search_engine_keychain_account(profile.id);
        let requires_api_key = profile.kind.requires_api_key();
        if requires_api_key {
            if let Some(api_key) = api_key
                && let Err(error) = self.secrets.set(&account, &api_key)
            {
                self.emit_error(error.to_string());
                return false;
            }
            match self.secrets.get(&account) {
                Ok(Some(value)) if !value.trim().is_empty() => profile.has_api_key = true,
                Ok(_) => {
                    self.report("notice-key-missing");
                    return false;
                }
                Err(error) => {
                    self.emit_error(error.to_string());
                    return false;
                }
            }
        } else {
            profile.has_api_key = false;
        }

        let mut profiles = self.state().search_engine_profiles.clone();
        if let Some(existing) = profiles
            .iter_mut()
            .find(|existing| existing.id == profile.id)
        {
            *existing = profile;
        } else {
            profiles.push(profile);
        }
        profiles.sort_by_key(|profile| profile.position);

        if !self.save_search_engines(profiles) {
            return false;
        }
        true
    }

    /// Resolve the credential for a connection test without performing network
    /// I/O. The host runs the actual request on its background executor so a
    /// slow endpoint cannot freeze the GPUI event loop.
    pub(crate) fn search_engine_test_api_key(
        &mut self,
        profile: &SearchEngineProfile,
        supplied_key: Option<String>,
    ) -> Result<Option<String>, String> {
        if profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url) {
            return Err(self.say("notice-search-engine-untestable").to_owned());
        }
        if supplied_key.is_some() || !profile.kind.requires_api_key() {
            Ok(supplied_key)
        } else {
            self.secrets
                .get(&search_engine_keychain_account(profile.id))
                .map_err(|error| error.to_string())
        }
    }

    /// Preserve the existing global notification while the settings page also
    /// shows its row-local result.
    pub(crate) fn report_search_engine_test(
        &mut self,
        profile_name: &str,
        result: &Result<(), String>,
    ) {
        match result {
            Ok(()) => {
                let message = format!("{} {}", profile_name, self.say("notice-search-engine-ok"));
                self.emit_info(message);
            }
            Err(error) => {
                let message = format!(
                    "{} {}: {error}",
                    profile_name,
                    self.say("notice-test-failed")
                );
                self.emit_error(message);
            }
        }
    }

    /// Ask a provider which models it has.
    ///
    /// Answers with the list rather than filling the form in, because the form is
    /// the shell's. Nothing to offer — no models, or the request failed — is
    /// reported as a notice and answered `None`.
    pub(crate) fn discover_provider_models(
        &mut self,
        profile_id: Option<ProviderId>,
        provider_name: String,
        base_url: String,
        protocol: ProviderProtocol,
        supplied_key: Option<String>,
    ) -> Option<Vec<pi_whim_core::ProviderModel>> {
        let key = supplied_key.or_else(|| {
            profile_id.and_then(|id| {
                self.secrets
                    .get(&provider_keychain_account(id))
                    .ok()
                    .flatten()
            })
        });
        match discover_models(&base_url, protocol, key.as_deref()) {
            Ok(mut models) if !models.is_empty() => {
                self.capability_resolver
                    .enrich_models(&provider_name, &base_url, &mut models);
                Some(models)
            }
            Ok(_) => {
                self.report("notice-no-models-discovered");
                None
            }
            Err(error) => {
                self.emit_error(error);
                None
            }
        }
    }

    pub(crate) fn project_path(&self, id: ProjectId) -> Option<String> {
        self.find_project(id).map(|project| project.path)
    }

    fn find_project(&self, id: ProjectId) -> Option<Project> {
        self.state()
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
    }

    fn active(&self) -> Option<&SessionRuntime<R>> {
        self.sessions.active()
    }

    fn active_mut(&mut self) -> Option<&mut SessionRuntime<R>> {
        let key = self.sessions.active_key().map(str::to_owned)?;
        self.sessions.get_mut(&key)
    }

    fn active_command(&self, command: Value) -> Result<Value, String> {
        self.active()
            .ok_or_else(|| "No active session.".to_owned())?
            .runtime
            .command(command)
            .map_err(|error| error.to_string())
    }

    fn active_send(&self, command: Value) -> Result<(), String> {
        self.active()
            .ok_or_else(|| "No active session.".to_owned())?
            .runtime
            .send(command)
            .map_err(|error| error.to_string())
    }

    /// Open a project: reactivate its most recently used live session when one
    /// exists, otherwise resume the newest stored session, otherwise start a
    /// fresh one. Previously running sessions keep their processes untouched.
    fn start_project(&mut self, project_id: ProjectId) {
        let mru_live = self.sessions.most_recent_in(project_id).map(str::to_owned);
        if let Some(key) = mru_live {
            self.activate_session(&key);
            return;
        }
        let stored = self
            .state()
            .sessions
            .get(&project_id)
            .and_then(|sessions| sessions.first())
            .map(|session| session.pi_path.clone())
            .filter(|path| Path::new(path).is_file());
        match stored {
            Some(path) => self.switch_session(project_id, path),
            None => self.start_new_session(project_id),
        }
    }

    /// Launch one Pi process bound to a session. `session_path = None` starts
    /// a fresh session; the pool key falls back to the file Pi reports, or a
    /// draft key until then.
    fn launch_session(
        &mut self,
        project_id: ProjectId,
        session_path: Option<&str>,
    ) -> Option<String> {
        let project = self.find_project(project_id)?;
        let sessions_path = match self
            .sessions_root_override
            .clone()
            .map(Ok)
            .unwrap_or_else(SqliteStore::sessions_root)
        {
            Ok(root) => root.join(project.id.to_string()),
            Err(error) => {
                self.emit_error(error.to_string());
                return None;
            }
        };
        if let Err(error) = fs::create_dir_all(&sessions_path) {
            self.emit_error(error.to_string());
            return None;
        }
        // Capabilities come from the catalog, which the app owns, so profiles
        // are enriched before they reach engine.
        let mut profiles = self
            .store
            .as_ref()
            .map(ProviderRepository::list_provider_profiles)
            .transpose()
            .unwrap_or_default()
            .unwrap_or_default();
        for profile in &mut profiles {
            self.capability_resolver.enrich_profile(profile);
        }
        let mut environment = match launch::prepare_pi_configuration(
            self.agent_directory_override.as_deref(),
            profiles,
            &self.secrets,
        ) {
            Ok(environment) => environment,
            Err(error) => {
                self.emit_error(error);
                return None;
            }
        };
        if self.sessions.active_key().is_none() {
            self.apply(Action::SetSessionStatus(SessionStatus::Starting));
        }
        let mut extension_paths = Vec::new();
        match ensure_agent_team_extension(&sessions_path) {
            Ok(path) => extension_paths.push(path.to_string_lossy().into_owned()),
            Err(error) => {
                if self.sessions.active_key().is_none() {
                    self.apply(Action::SetSessionStatus(SessionStatus::Failed(
                        error.to_string(),
                    )));
                }
                self.emit_error(error.to_string());
                return None;
            }
        }
        environment.insert(
            "PI_WHIM_BASH_POLICY".into(),
            bash_policy_name(&self.state().bash_policy).into(),
        );
        environment.insert(
            "PI_WHIM_BASH_BLOCKED_PATTERNS".into(),
            serde_json::to_string(&self.state().bash_blocked_patterns)
                .unwrap_or_else(|_| "[]".into()),
        );
        let search_engine_api_keys =
            match configured_search_engine_api_keys(&self.state().search_engine_profiles, |id| {
                self.secrets
                    .get(&search_engine_keychain_account(id))
                    .map_err(|error| error.to_string())
            }) {
                Ok(api_keys) => api_keys,
                Err(error) => {
                    self.emit_error(error);
                    return None;
                }
            };
        let mut runtime = (self.runtime_factory)();
        let project_path = project.path.clone();
        let hooks = self.load_hooks(&project_path);
        self.hook_revisions
            .insert(project_id, hooks.revision().to_owned());
        let hook_scope = match self.hook_host.resolve_scope(
            Path::new(&project_path),
            &hooks.global,
            hooks.project.as_ref(),
        ) {
            Ok(scope) => scope,
            Err(error) if !hooks.requires_shared_scope() => {
                self.emit_error(format!(
                    "failed to create shared hook scope; using legacy v1 hook runtime: {error}"
                ));
                None
            }
            Err(error) => {
                let message = format!(
                    "failed to create required v2 shared hook scope; session was not started: {error}"
                );
                if self.sessions.active_key().is_none() {
                    self.apply(Action::SetSessionStatus(SessionStatus::Failed(
                        message.clone(),
                    )));
                }
                self.emit_error(message);
                return None;
            }
        };
        if let Err(error) = runtime.start(RuntimeStart {
            project_path,
            sessions_path: sessions_path.to_string_lossy().into_owned(),
            session_path: session_path.map(str::to_owned),
            extension_paths,
            environment,
            agent_team_config: self.state().agent_team_config.clone(),
            search_engines: self.state().search_engine_profiles.clone(),
            search_engine_api_keys,
            hooks: hooks.legacy,
            hook_scope,
        }) {
            if self.sessions.active_key().is_none() {
                self.apply(Action::SetSessionStatus(SessionStatus::Failed(
                    error.to_string(),
                )));
            }
            self.emit_error(error.to_string());
            return None;
        }
        let events = runtime.events();
        let key = session_path.map(str::to_owned).unwrap_or_else(|| {
            runtime
                .command(json!({"type":"get_state"}))
                .ok()
                .and_then(|state| {
                    state
                        .get("sessionFile")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .unwrap_or_else(|| format!("draft://{}", Uuid::new_v4()))
        });
        self.sessions.insert(
            key.clone(),
            SessionRuntime::new(runtime, events, project_id, now_ms()),
        );
        self.discover_sessions(project_id, &sessions_path);
        Some(key)
    }

    fn load_hooks(&mut self, project_path: &str) -> hooks::LoadedHooks {
        self.refresh_hook_audit(project_path);
        let global = self.load_global_hooks();
        let path = Path::new(project_path).join(".pi-whim/hooks.json");
        let source = match fs::read(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.revoke_project_hook_scope(project_path);
                self.apply(Action::SetProjectHookStatus(ProjectHookStatus::NotPresent));
                return hooks::LoadedHooks::global_only(global);
            }
            Err(error) => {
                let message = format!("failed to read project hook manifest: {error}");
                self.revoke_project_hook_scope(project_path);
                self.apply(Action::SetProjectHookStatus(ProjectHookStatus::Invalid(
                    message.clone(),
                )));
                self.emit_error(message);
                return hooks::LoadedHooks::global_only(global);
            }
        };
        let project = match hooks::manifest::prepare_manifest(&source, true, None) {
            Ok(project) => project,
            Err(error) => {
                self.revoke_project_hook_scope(project_path);
                self.apply(Action::SetProjectHookStatus(ProjectHookStatus::Invalid(
                    error.clone(),
                )));
                self.emit_error(format!(
                    "invalid project hook manifest {}: {error}",
                    path.display()
                ));
                return hooks::LoadedHooks::global_only(global);
            }
        };
        let trusted = self
            .store
            .as_ref()
            .and_then(|store| store.project_hook_trust(project_path).ok().flatten())
            .is_some_and(|trusted| {
                trusted.fingerprint == project.fingerprint
                    && trusted.grants_hash == project.grants_hash
                    && trusted.grants == project.grants
                    && !trusted.grants_hash.is_empty()
            });
        if !trusted {
            self.revoke_project_hook_scope(project_path);
            self.apply(Action::SetProjectHookStatus(
                ProjectHookStatus::ApprovalRequired {
                    fingerprint: project.fingerprint.clone(),
                    grants_hash: project.grants_hash.clone(),
                    grants: project.grants.clone(),
                },
            ));
            return hooks::LoadedHooks::global_only(global);
        }
        let loaded = match hooks::LoadedHooks::approved(global.clone(), project.clone()) {
            Ok(loaded) => loaded,
            Err(error) => {
                self.revoke_project_hook_scope(project_path);
                self.apply(Action::SetProjectHookStatus(ProjectHookStatus::Invalid(
                    error.clone(),
                )));
                self.emit_error(error);
                return hooks::LoadedHooks::global_only(global);
            }
        };
        self.apply(Action::SetProjectHookStatus(ProjectHookStatus::Approved {
            fingerprint: project.fingerprint,
            grants_hash: project.grants_hash,
            grants: project.grants,
        }));
        loaded
    }

    fn revoke_project_hook_scope(&mut self, project_path: &str) {
        self.hook_host.revoke_project(Path::new(project_path));
    }

    fn refresh_hook_audit(&mut self, project_path: &str) {
        let entries = self
            .store
            .as_ref()
            .and_then(|store| store.recent_hook_audit(project_path, 20).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|entry| HookAuditSummary {
                hook_id: entry.hook_id,
                event: entry.event,
                outcome: entry.outcome,
                duration_ms: entry.duration_ms,
                output_truncated: entry.output_truncated,
                revision: entry.revision,
                created_at_ms: entry.created_at_ms,
            })
            .collect();
        self.apply(Action::SetHookAudit(entries));
    }

    fn load_global_hooks(&mut self) -> hooks::manifest::PreparedHookManifest {
        let Some(root) = self
            .hook_config_root_override
            .clone()
            .or_else(|| dirs::config_dir().map(|path| path.join("pi-whim")))
        else {
            return global_hook_fallback();
        };
        let path = root.join("hooks.json");
        match fs::read(&path) {
            Ok(source) => match hooks::manifest::prepare_manifest(&source, false, None) {
                Ok(manifest) => manifest,
                Err(error) => {
                    self.emit_error(format!("invalid hook manifest {}: {error}", path.display()));
                    global_hook_fallback()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => global_hook_fallback(),
            Err(error) => {
                self.emit_error(format!(
                    "failed to read hook manifest {}: {error}",
                    path.display()
                ));
                global_hook_fallback()
            }
        }
    }

    /// Bring a pooled session to the foreground: the conversation view binds to
    /// its process while every other session keeps running in the background.
    fn activate_session(&mut self, key: &str) {
        let Some(session) = self.sessions.activate(key, now_ms()) else {
            return;
        };
        let project_id = session.project_id;
        let running = session.is_running();
        // The visible state carries one session's queue; restore the incoming
        // session's rather than leaving the previous one's behind.
        let queued_steering = session.turn.queued_steering.clone();
        let queued_follow_up = session.turn.queued_follow_up.clone();
        self.apply(Action::SelectProject(project_id));
        if let Some(project) = self.find_project(project_id) {
            let _ = self.load_hooks(&project.path);
        }
        if !is_draft(key) {
            self.apply(Action::SelectSession(stable_session_id(key)));
        }
        self.apply(Action::ClearConversation);
        self.apply(Action::SetSessionStatus(if running {
            SessionStatus::Streaming
        } else {
            SessionStatus::Ready
        }));
        self.apply(Action::QueueUpdated {
            steering: queued_steering,
            follow_up: queued_follow_up,
        });
        let _ = self.load_current_entries();
        self.refresh_runtime_controls();
    }

    /// Re-key a pooled session once Pi reveals its real session file (fresh
    /// sessions start under a `draft://` key; fork/clone move to a new file).
    fn rekey_session(&mut self, from: &str, to: &str) {
        if let Some(outcome) = self.sessions.rekey(from, to, now_ms())
            && outcome.was_active
        {
            self.apply(Action::SelectSession(stable_session_id(to)));
        }
    }

    fn stop_project_runtimes(&mut self, project_id: ProjectId) {
        // Note the visible session before removing anything: the pool drops its
        // own selection as soon as that session leaves.
        let was_visible = self
            .sessions
            .active_key()
            .and_then(|key| self.sessions.get(key))
            .is_some_and(|session| session.project_id == project_id);

        for key in self.sessions.remove_project(project_id) {
            // Nothing is left to answer, and a dialog still up would ask the
            // reader to unblock a process that has gone.
            self.emit_effect(signals::ApplicationEffect::SessionClosed(key.clone()));
            self.apply(Action::SessionRunning {
                path: key,
                running: false,
            });
        }

        if was_visible {
            self.apply(Action::ClearConversation);
            self.apply(Action::SetSessionStatus(SessionStatus::Offline));
            self.apply(Action::QueueUpdated {
                steering: Vec::new(),
                follow_up: Vec::new(),
            });
        }
    }

    /// Ask the agent for its control state on a worker thread.
    ///
    /// Five RPCs at up to 20 seconds each; issuing them inline froze the window
    /// whenever Pi was slow to answer. Results arrive as actions and are applied
    /// by [`Self::settle_controls`].
    fn refresh_runtime_controls(&mut self) {
        let Some(commander) = self
            .active()
            .and_then(|session| session.runtime.commander())
        else {
            return;
        };
        let providers = controls::provider_names(&self.state().provider_profiles);
        let sender = self.control_updates.0.clone();
        let key = self.sessions.active_key().map(str::to_owned);
        std::thread::spawn(move || {
            let actions = controls::fetch(&commander, &providers);
            let _ = sender.send((key, actions));
        });
    }

    /// Block until an in-flight control refresh lands, for tests.
    ///
    /// Production code applies these as the pump delivers them; a test has no
    /// pump, so it waits for the worker instead of racing it.
    #[cfg(test)]
    fn settle_control_updates(&mut self) {
        while let Ok((key, actions)) = self
            .control_updates
            .1
            .recv_timeout(std::time::Duration::from_secs(5))
        {
            if key.as_deref() == self.sessions.active_key() {
                for action in actions {
                    self.apply(action);
                }
            }
            if self.control_updates.1.is_empty() {
                break;
            }
        }
    }

    /// Apply whatever one control refresh reported.
    ///
    /// An update for a session that is no longer visible is dropped: the user
    /// has moved on, and applying it would overwrite the current session's
    /// controls with another's.
    pub(crate) fn settle_controls(&mut self, key: Option<String>, actions: Vec<Action>) {
        if key.as_deref() != self.sessions.active_key() {
            return;
        }
        for action in actions {
            self.apply(action);
        }
    }

    fn set_model_on(&mut self, key: &str, model: ModelOption) {
        let result = self.sessions.get(key).map(|session| {
            session.runtime.command(json!({
                "type":"set_model",
                "provider": model.provider,
                "modelId": model.id,
            }))
        });
        match result {
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                self.emit_error(error.to_string());
                return;
            }
            None => {
                self.report("notice-session-gone");
                return;
            }
        }
        if self.sessions.active_key() == Some(key) {
            self.refresh_runtime_controls();
        }
    }

    /// Defer a model switch until the next prompt so the prior model can compact
    /// the conversation first (cache-friendly). The UI shows the pending model
    /// immediately while Pi keeps using the old one until the prompt lands.
    fn queue_model_switch(&mut self, model: ModelOption) {
        self.apply(Action::SetPendingModel(Some(model)));
    }

    /// Apply a deferred model switch: send set_model and refresh controls.
    fn apply_pending_model(&mut self, key: &str) {
        if let Some(model) = self.state().pending_model.clone() {
            self.set_model_on(key, model);
            self.apply(Action::SetPendingModel(None));
        }
    }

    fn set_thinking_level(&mut self, level: ThinkingLevel) {
        if let Err(error) =
            self.active_command(json!({"type":"set_thinking_level", "level": level.as_str()}))
        {
            self.emit_error(error);
            return;
        }
        self.refresh_runtime_controls();
    }

    fn set_auto_compaction(&mut self, enabled: bool) {
        if let Err(error) =
            self.active_command(json!({"type":"set_auto_compaction", "enabled": enabled}))
        {
            self.emit_error(error);
            return;
        }
        self.refresh_runtime_controls();
    }

    /// Apply the prompt's permission selector without restarting any session.
    ///
    /// The policy is global, so every live supervisor receives it. Existing
    /// children keep the ceiling they started with; newly spawned children use
    /// the new default while all root sessions and in-flight turns stay intact.
    fn set_permission_level(&mut self, level: AgentPermissionLevel) {
        if self.state().agent_team_config.default_policy.level == level {
            return;
        }
        for (_, session) in self.sessions.iter() {
            if let Err(error) = session.runtime.set_default_permission_level(level) {
                self.emit_error(error.to_string());
            }
        }
        let mut config = self.state().agent_team_config.clone();
        config.default_policy.level = level;
        self.apply(Action::SetAgentTeamConfig(config));
        self.save_preferences();
    }

    fn compact_session(&mut self) {
        if !matches!(self.state().session_status, SessionStatus::Ready) {
            return;
        }
        if let Err(error) = self.active_send(json!({"type":"compact"})) {
            self.emit_error(error);
            return;
        }
        if let Some(session) = self.active_mut() {
            session.turn.running = true;
        }
        self.apply(Action::SetSessionStatus(SessionStatus::Compacting));
    }

    fn set_queue_modes(&mut self, steering: QueueMode, follow_up: QueueMode) {
        let steering = queue_mode_name(steering);
        let follow_up = queue_mode_name(follow_up);
        let result = self
            .active_command(json!({"type":"set_steering_mode", "mode": steering}))
            .and_then(|_| {
                self.active_command(json!({"type":"set_follow_up_mode", "mode": follow_up}))
            });
        if let Err(error) = result {
            self.emit_error(error);
            return;
        }
        self.refresh_runtime_controls();
    }

    /// Re-read the active session's state from its own Pi process: index the
    /// session file (re-keying the pool when fork/clone moved to a new file)
    /// and reload the visible conversation.
    fn refresh_session_state(&mut self, project_id: ProjectId) {
        let Some(key) = self.sessions.active_key().map(str::to_owned) else {
            return;
        };
        match self.active_command(json!({"type":"get_state"})) {
            Ok(state) => {
                if let Some(path) = state.get("sessionFile").and_then(Value::as_str) {
                    if path != key {
                        self.rekey_session(&key, path);
                    }
                    self.index_session(
                        project_id,
                        path,
                        state.get("sessionName").and_then(Value::as_str),
                    );
                }
                let _ = self.load_current_entries();
            }
            Err(error) => self.emit_error(error),
        }
    }

    fn index_session(&mut self, project_id: ProjectId, pi_path: &str, name: Option<&str>) {
        let Some(mut session) = session_summary_from_jsonl(project_id, Path::new(pi_path)) else {
            if let Some(store) = self.store.as_ref() {
                let _ = store.delete_session(stable_session_id(pi_path));
                if let Ok(sessions) = store.list_sessions(project_id) {
                    self.apply(Action::SessionsLoaded {
                        project_id,
                        sessions,
                    });
                }
            }
            return;
        };
        if let Some(name) = name.filter(|name| !name.trim().is_empty()) {
            session.title = name.to_owned();
        }
        if let Some(store) = self.store.as_ref() {
            if let Err(error) = store.save_session(&session) {
                self.emit_error(error.to_string());
                return;
            }
            if let Ok(sessions) = store.list_sessions(project_id) {
                self.apply(Action::SessionsLoaded {
                    project_id,
                    sessions,
                });
            }
        }
    }

    fn persist_session_title_source(
        &mut self,
        session_id: pi_whim_core::SessionId,
        title: &str,
        source: SessionTitleSource,
    ) -> bool {
        let Some(store) = self.store.as_ref() else {
            return true;
        };
        let result = if source == SessionTitleSource::Automatic {
            store.set_session_ai_title(session_id, title)
        } else {
            store.rename_session(session_id, title)
        };
        if let Err(error) = result {
            self.emit_error(error.to_string());
            return false;
        }
        true
    }

    fn discover_sessions(&mut self, project_id: ProjectId, sessions_path: &Path) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let entries = match fs::read_dir(sessions_path) {
            Ok(entries) => entries,
            Err(_) => return,
        };
        let mut valid_session_ids = HashSet::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(summary) = session_summary_from_jsonl(project_id, &path) {
                valid_session_ids.insert(summary.id);
                if let Err(error) = store.save_session(&summary) {
                    self.emit_error(error.to_string());
                }
            }
        }
        if let Ok(indexed_sessions) = store.list_sessions(project_id) {
            for session in indexed_sessions {
                if Path::new(&session.pi_path).starts_with(sessions_path)
                    && !valid_session_ids.contains(&session.id)
                    && let Err(error) = store.delete_session(session.id)
                {
                    self.emit_error(error.to_string());
                }
            }
        }
        if let Ok(sessions) = store.list_sessions(project_id) {
            self.apply(Action::SessionsLoaded {
                project_id,
                sessions,
            });
        }
    }

    /// Start a fresh session in its own Pi process. The currently visible
    /// session keeps running untouched in the background.
    fn start_new_session(&mut self, project_id: ProjectId) {
        self.apply(Action::SelectProject(project_id));
        // An empty visible session of the same project is reused instead of
        // spawning another blank one.
        if self.active().is_some_and(|session| {
            session.project_id == project_id
                && !self
                    .state()
                    .conversation
                    .iter()
                    .any(|message| message.role == ConversationRole::User)
        }) {
            return;
        }
        let Some(key) = self.launch_session(project_id, None) else {
            return;
        };
        self.activate_session(&key);
        // pi-mono defers writing the session JSONL until the first assistant
        // response (its newSession contract), so the sidebar's disk scan would
        // not see the new session. Persist a placeholder summary so it shows up
        // immediately; a later refresh replaces it once the file is written.
        if key.starts_with("draft://") {
            return;
        }
        if let Some(store) = self.store.as_ref() {
            let summary = SessionSummary {
                id: stable_session_id(&key),
                project_id,
                pi_path: key.clone(),
                title: String::new(),
                preview: String::new(),
                updated_at_ms: now_ms(),
            };
            let _ = store.save_session(&summary);
            if let Ok(sessions) = store.list_sessions(project_id) {
                self.apply(Action::SessionsLoaded {
                    project_id,
                    sessions,
                });
            }
            self.apply(Action::SelectSession(summary.id));
        }
    }

    /// Switch the visible session. The target gets its own Pi process (reused
    /// when already pooled); the session being left keeps running, so parallel
    /// L0 agents never abort each other.
    fn switch_session(&mut self, project_id: ProjectId, path: String) {
        if !self.sessions.contains(&path) && self.launch_session(project_id, Some(&path)).is_none()
        {
            return;
        }
        self.activate_session(&path);
    }

    fn rename_session(&mut self, path: String, title: String) {
        self.cancel_pending_titles_for_path(&path);
        // A live session renames through its own process so the name lands in
        // the session file; renaming a background session never disturbs it.
        // Without a live process the title is appended to Pi's JSONL directly.
        let live_token = self.sessions.token_for(&path);
        if let Some(session) = self.sessions.get(&path) {
            if let Err(error) = session
                .runtime
                .command(json!({"type":"set_session_name", "name": title}))
            {
                self.emit_error(error.to_string());
                return;
            }
            if let Some(token) = live_token {
                self.expect_session_title_name(token, &title);
            }
        } else if let Err(error) = persist_session_title_to_jsonl(Path::new(&path), &title) {
            self.emit_error(error.to_string());
            return;
        }
        if let Some(project_id) = self.state().selected_project {
            self.index_session(project_id, &path, Some(&title));
            self.persist_session_title_source(
                stable_session_id(&path),
                &title,
                SessionTitleSource::ExplicitSmartRename,
            );
        }
        if self.sessions.active_key() == Some(path.as_str()) {
            self.refresh_runtime_controls();
        }
    }

    fn set_current_session_name(&mut self, name: String) {
        let Some(key) = self.sessions.active_key().map(str::to_owned) else {
            self.report("notice-no-session-to-name");
            return;
        };
        if key.starts_with("draft://") {
            self.report("notice-no-session-to-name");
            return;
        }
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.report("notice-name-usage");
            return;
        }
        self.rename_session(key, name);
    }

    fn export_session(&mut self, requested_path: Option<String>) {
        let output_path = requested_path.filter(|path| !path.trim().is_empty());
        let mut command = json!({"type": "export_html"});
        if let Some(output_path) = output_path {
            command["outputPath"] = Value::String(output_path);
        }
        let response = self.active_command(command);
        match response {
            Ok(value) => {
                if let Some(path) = value.get("path").and_then(Value::as_str) {
                    let message = format!("{} {path}", self.say("notice-session-exported"));
                    self.emit_error(message);
                }
            }
            Err(error) => self.emit_error(error),
        }
    }

    fn share_session(&mut self) {
        let Some(path) = self
            .active_command(json!({"type":"export_html"}))
            .ok()
            .and_then(|value| value.get("path").and_then(Value::as_str).map(str::to_owned))
        else {
            self.report("notice-export-failed");
            return;
        };
        let output = std::process::Command::new("gh")
            .args(["gist", "create", "--public=false", &path])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                let message = format!("{} {url}", self.say("notice-share-url"));
                self.emit_error(message);
            }
            Ok(output) => {
                self.emit_error(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(error) => {
                let message = format!("{} {error}", self.say("notice-gh-unavailable"));
                self.emit_error(message);
            }
        }
    }

    fn clone_session(&mut self) {
        if let Err(error) = self.active_command(json!({"type":"clone"})) {
            self.emit_error(error);
            return;
        }
        if let Some(project_id) = self.state().selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn fork_session(&mut self, entry_id: String) {
        if let Err(error) = self.active_command(json!({"type":"fork", "entryId": entry_id})) {
            self.emit_error(error);
            return;
        }
        if let Some(project_id) = self.state().selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn delete_session(&mut self, path: String) {
        // Stop the session's own process first so it cannot rewrite the file
        // after the delete; the conversation moves to another live session.
        let was_visible = self.sessions.active_key() == Some(path.as_str());
        self.cancel_pending_titles_for_path(&path);
        if let Some(mut session) = self.sessions.remove(&path) {
            let _ = session.runtime.stop();
            self.apply(Action::SessionRunning {
                path: path.clone(),
                running: false,
            });
            if was_visible {
                self.apply(Action::ClearConversation);
                self.apply(Action::SetSessionStatus(SessionStatus::Offline));
                self.apply(Action::QueueUpdated {
                    steering: Vec::new(),
                    follow_up: Vec::new(),
                });
            }
        }
        let target = PathBuf::from(&path);
        let status = std::process::Command::new("osascript")
            .args([
                "-e",
                &format!(
                    "tell application \"Finder\" to delete POSIX file \"{}\"",
                    applescript_escape(&path)
                ),
            ])
            .status();
        if !matches!(status, Ok(status) if status.success()) {
            self.report("notice-trash-failed");
            return;
        }
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.delete_session(stable_session_id(&path))
        {
            self.emit_error(error.to_string());
        }
        if let Some(project_id) = self.state().selected_project {
            self.discover_sessions(project_id, target.parent().unwrap_or(Path::new("")));
        }
    }

    fn load_current_entries(&mut self) -> Result<(), ()> {
        let entries = self
            .active_command(json!({"type":"get_entries"}))
            .map_err(|error| {
                self.emit_error(error);
            })?;
        self.apply(Action::ClearConversation);
        for action in entries
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(events::session_entry_action)
        {
            self.apply(action);
        }
        Ok(())
    }

    /// Stage the paths a picker returned.
    ///
    /// The picker itself is opened by the host, which has the window: the platform
    /// one is asynchronous, and awaiting it here would mean either blocking the
    /// main thread or holding a borrow across the await. Turning a path into an
    /// attachment is still this side's job, because the notices are.
    pub(crate) fn attach_paths(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            match attachment_from_path(&path, false) {
                Ok(attachment) => {
                    self.emit_effect(signals::ApplicationEffect::AttachmentReady(attachment));
                }
                Err(error) => self.emit_error(error),
            }
        }
    }

    /// Turn a paste into attachments the composer can hold.
    ///
    /// Answered rather than staged, because a paste is the reader waiting on one
    /// action and the composer is where it lands. Files come as a list, so this
    /// answers with however many the clipboard held.
    pub(crate) fn attachments_for(&mut self, paste: ShellPaste) -> Vec<Attachment> {
        let created = match paste {
            ShellPaste::Files(paths) => {
                let mut attachments = Vec::new();
                for path in paths {
                    match attachment_from_path(Path::new(&path), false) {
                        Ok(attachment) => attachments.push(attachment),
                        // One unreadable path does not spoil the rest of a
                        // multi-file paste.
                        Err(error) => self.emit_error(error),
                    }
                }
                return attachments;
            }
            ShellPaste::Image { extension, bytes } => self
                .attachment_store
                .create_pasted_encoded_image(&extension, &bytes),
            ShellPaste::LongText(text) => self.attachment_store.create_pasted_text(&text),
        };
        match created {
            Ok(attachment) => vec![attachment],
            Err(error) => {
                self.emit_error(error);
                Vec::new()
            }
        }
    }

    pub(crate) fn can_submit_prompt(&self) -> bool {
        let Some(project_id) = self.state().selected_project else {
            return false;
        };
        matches!(
            self.state().session_status,
            SessionStatus::Ready | SessionStatus::Streaming | SessionStatus::Compacting
        ) && self
            .active()
            .is_some_and(|session| session.project_id == project_id)
    }

    /// Report why a prompt could not be accepted without creating a transcript
    /// entry. The host uses this for the small window between a UI submission and
    /// draining its request queue.
    pub(crate) fn report_submission_unavailable(&mut self) {
        if self.state().selected_project.is_none() {
            self.report("notice-select-project-send");
        } else if !matches!(
            self.state().session_status,
            SessionStatus::Ready | SessionStatus::Streaming | SessionStatus::Compacting
        ) {
            self.report("notice-not-ready");
        } else {
            self.report("notice-no-session");
        }
    }

    fn submit_prompt(&mut self, content: String, attachments: Vec<Attachment>, mode: SubmitMode) {
        if !self.can_submit_prompt() {
            self.report_submission_unavailable();
            return;
        }
        // Capture this before adding the optimistic user entry: failure to find
        // an active session must leave both transcript and draft untouched.
        let key = self
            .sessions
            .active_key()
            .expect("can_submit_prompt requires an active session")
            .to_owned();
        let project_id = self
            .sessions
            .get(&key)
            .expect("active session is present")
            .project_id;
        if self.hooks_changed() {
            self.pending_hook_inputs
                .entry(project_id)
                .or_default()
                .push_back((content, attachments, mode));
            self.request_hook_reload(project_id);
            return;
        }
        let first_plain_prompt = matches!(mode, SubmitMode::Prompt)
            && !content.trim().is_empty()
            && !self.state().conversation.iter().any(|message| {
                message.role == ConversationRole::User && !message.full_text.trim().is_empty()
            });
        if first_plain_prompt && let Some(token) = self.sessions.token_for(&key) {
            self.title_eligible.insert(token);
        }
        // Only a fresh prompt is placed optimistically: it starts answering
        // right away, so its place in the conversation is now. A steered or
        // queued message would sit mid-transcript while the turn streams on
        // below it — it waits in the queue block instead, and Pi's
        // `message_start` places it at the end when it is consumed.
        if matches!(mode, SubmitMode::Prompt) {
            let item = ConversationItem {
                id: Uuid::new_v4().to_string(),
                role: ConversationRole::User,
                full_text: content.clone(),
                streaming: false,
                tool_name: None,
                tool_report: None,
                tool_details: None,
                is_error: false,
                model: None,
                attachments: attachments.clone(),
            };
            self.apply(Action::UpsertConversation(item));
        }
        // A deferred model switch waits until the prompt that continues the
        // conversation, so the prior model compacts the existing history first
        // (cache-friendly). Skip when there's nothing to compact or it just did.
        let defer_for_compaction = matches!(mode, SubmitMode::Prompt)
            && self.state().pending_model.is_some()
            && !self
                .active()
                .map(|session| session.turn.conversation_compacted)
                .unwrap_or(true)
            && self
                .state()
                .conversation
                .iter()
                .any(|message| message.role != ConversationRole::User);
        if defer_for_compaction {
            let result = self.active_send(json!({"type":"compact"}));
            match result {
                Ok(()) => {
                    if let Some(session) = self.sessions.get_mut(&key) {
                        session.turn.pending_prompt = Some((content, attachments, mode));
                        session.turn.running = true;
                    }
                    self.apply(Action::SetSessionStatus(SessionStatus::Compacting));
                }
                Err(error) => self.emit_error(error),
            }
            return;
        }
        if self.state().pending_model.is_some() {
            self.apply_pending_model(&key);
        }
        self.send_prompt(&key, content, attachments, mode);
    }

    fn send_prompt(
        &mut self,
        key: &str,
        content: String,
        attachments: Vec<Attachment>,
        mode: SubmitMode,
    ) {
        let result = {
            let Some(session) = self.sessions.get(key) else {
                self.report("notice-session-gone");
                return;
            };
            match mode {
                SubmitMode::Prompt => session.runtime.send(json!({
                    "type": "prompt",
                    "message": prompt_with_attachment_paths(&content, &attachments),
                    // Pi rejects a prompt that arrives mid-run unless told how
                    // to queue it. The session status normally keeps a plain
                    // prompt from going out then, but a run Pi started on its
                    // own is invisible until its first event lands — queue the
                    // message rather than lose it. Idle Pi ignores this field.
                    "streamingBehavior": "followUp",
                })),
                SubmitMode::Steer => session
                    .runtime
                    .send(json!({"type":"steer", "message": content})),
                SubmitMode::FollowUp => session
                    .runtime
                    .send(json!({"type":"follow_up", "message": content})),
            }
        };
        if let Err(error) = result {
            if let Some(token) = self.sessions.token_for(key) {
                self.title_eligible.remove(&token);
            }
            self.emit_error(error.to_string());
            return;
        }
        let is_active = self.sessions.active_key() == Some(key);
        if let Some(session) = self.sessions.get_mut(key) {
            session.turn.running = true;
            if matches!(mode, SubmitMode::Prompt) {
                // What `message_start` will report, so the translation can tell
                // this echo from a queued message it has never shown.
                session
                    .turn
                    .optimistic_prompts
                    .push_back(prompt_with_attachment_paths(&content, &attachments));
            }
        }
        self.apply(Action::SessionRunning {
            path: key.to_owned(),
            running: true,
        });
        if is_active {
            self.apply(Action::SetSessionStatus(SessionStatus::Streaming));
        }
        let should_title = self
            .sessions
            .token_for(key)
            .is_some_and(|token| self.title_eligible.remove(&token));
        if should_title {
            self.start_session_title(key, content);
        }
    }

    fn start_session_title(&mut self, key: &str, content: String) {
        let Some(token) = self.sessions.token_for(key) else {
            return;
        };
        if !self.title_attempted.insert(token) {
            return;
        }
        let fallback = fallback_session_title(&content);
        let Some((project_id, session_file)) = self.sessions.get(key).and_then(|session| {
            let state = session
                .runtime
                .command(json!({"type":"get_state"}))
                .ok()
                .unwrap_or(Value::Null);
            if session
                .runtime
                .command(json!({"type":"set_session_name", "name": fallback}))
                .is_err()
            {
                return None;
            }
            Some((
                session.project_id,
                state
                    .get("sessionFile")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ))
        }) else {
            return;
        };
        let baseline = fallback.clone();

        // A matching session-info event is the acknowledgement of this name,
        // not a manual rename that should cancel the background replacement.
        self.expect_session_title_name(token, &baseline);
        if let Some(path) = session_file {
            self.index_session(project_id, &path, Some(&baseline));
        }
        if self.one_shot_ai.is_none() {
            self.deferred_session_titles.insert(
                token,
                DeferredSessionTitle {
                    content,
                    fallback,
                    baseline,
                },
            );
        } else {
            let _ = self.schedule_session_title(
                SessionTitleTarget::Live(token),
                SessionTitleTask::new(content),
                fallback,
                baseline,
                SessionTitleSource::Automatic,
            );
        }
    }

    fn submit_deferred_session_titles(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_session_titles);
        for (token, title) in deferred {
            let still_current =
                self.expected_session_title_names
                    .get(&token)
                    .is_some_and(|expected| {
                        expected
                            .iter()
                            .any(|name| name.trim() == title.baseline.trim())
                    });
            if !still_current || self.sessions.key_for(token).is_none() {
                continue;
            }
            let _ = self.schedule_session_title(
                SessionTitleTarget::Live(token),
                SessionTitleTask::new(title.content),
                title.fallback,
                title.baseline,
                SessionTitleSource::Automatic,
            );
        }
    }

    fn schedule_session_title<T: pi_whim_one_shot_ai::OneShotTask>(
        &mut self,
        target: SessionTitleTarget,
        task: T,
        fallback: String,
        baseline: String,
        source: SessionTitleSource,
    ) -> bool {
        let Some(service) = self.one_shot_ai.as_ref() else {
            return false;
        };
        let generation = service.generation();
        match service.try_submit(task) {
            Ok(request_id) => {
                self.pending_session_titles.insert(
                    request_id,
                    PendingSessionTitle {
                        target,
                        generation,
                        fallback,
                        baseline,
                        source,
                    },
                );
                true
            }
            Err(_) => {
                if source == SessionTitleSource::Automatic {
                    self.apply_pending_session_title(target, fallback, &baseline, source, false);
                }
                false
            }
        }
    }

    pub(crate) fn start_smart_session_rename(
        &mut self,
        project_id: ProjectId,
        path: String,
        baseline: String,
        context: Option<String>,
    ) {
        let still_current = self
            .state()
            .sessions
            .get(&project_id)
            .and_then(|sessions| sessions.iter().find(|session| session.pi_path == path))
            .is_some_and(|session| session.title.trim() == baseline.trim());
        if !still_current {
            return;
        }
        let Some(context) = context else {
            self.report("notice-smart-rename-empty");
            return;
        };
        if self.one_shot_ai.is_none() {
            self.report("notice-smart-rename-unavailable");
            return;
        }

        self.cancel_pending_titles_for_path(&path);
        let target = self
            .sessions
            .token_for(&path)
            .map(SessionTitleTarget::Live)
            .unwrap_or(SessionTitleTarget::Stored { project_id, path });
        if !self.schedule_session_title(
            target,
            SessionHistoryTitleTask::new(context),
            baseline.clone(),
            baseline,
            SessionTitleSource::ExplicitSmartRename,
        ) {
            self.report("notice-smart-rename-unavailable");
        }
    }

    pub(crate) fn settle_one_shot_completions(&mut self, completions: Vec<OneShotCompletion>) {
        for completion in completions {
            let Some(pending) = self.pending_session_titles.remove(&completion.request_id) else {
                continue;
            };
            if completion.generation != pending.generation
                || completion.generation != self.one_shot_generation
                || completion.task_kind != "session_title"
            {
                continue;
            }
            match completion.result {
                Ok(title)
                    if pending.source == SessionTitleSource::ExplicitSmartRename
                        && title.trim() == pending.baseline.trim() =>
                {
                    self.report("notice-smart-rename-unchanged");
                }
                Ok(title) => self.apply_pending_session_title(
                    pending.target,
                    title,
                    &pending.baseline,
                    pending.source,
                    true,
                ),
                Err(error) if pending.source == SessionTitleSource::ExplicitSmartRename => {
                    self.report_smart_rename_error(error);
                }
                Err(_) => self.apply_pending_session_title(
                    pending.target,
                    pending.fallback,
                    &pending.baseline,
                    pending.source,
                    false,
                ),
            }
        }
    }

    fn report_smart_rename_error(&mut self, error: OneShotErrorKind) {
        let key = match error {
            OneShotErrorKind::TimedOut => "notice-smart-rename-timeout",
            OneShotErrorKind::Unauthorized => "notice-smart-rename-unauthorized",
            OneShotErrorKind::RateLimited => "notice-smart-rename-rate-limited",
            OneShotErrorKind::Network => "notice-smart-rename-network",
            OneShotErrorKind::ProviderRejected => "notice-smart-rename-rejected",
            OneShotErrorKind::InvalidResponse
            | OneShotErrorKind::ResponseTooLarge
            | OneShotErrorKind::InvalidOutput => "notice-smart-rename-invalid-response",
            OneShotErrorKind::InputTooLarge | OneShotErrorKind::InvalidInput => {
                "notice-smart-rename-invalid-input"
            }
            OneShotErrorKind::Disabled
            | OneShotErrorKind::InvalidConfiguration
            | OneShotErrorKind::Cancelled
            | OneShotErrorKind::ShuttingDown => "notice-smart-rename-unavailable",
            _ => "notice-smart-rename-unavailable",
        };
        self.report(key);
    }

    fn apply_pending_session_title(
        &mut self,
        target: SessionTitleTarget,
        title: String,
        baseline: &str,
        source: SessionTitleSource,
        persist_source: bool,
    ) {
        if title.trim() == baseline.trim() {
            return;
        }
        match target {
            SessionTitleTarget::Live(token) => {
                self.apply_live_session_title(token, title, baseline, source, persist_source)
            }
            SessionTitleTarget::Stored { project_id, path } => {
                let still_current = self
                    .state()
                    .sessions
                    .get(&project_id)
                    .and_then(|sessions| sessions.iter().find(|session| session.pi_path == path))
                    .is_some_and(|session| session.title.trim() == baseline.trim());
                if !still_current {
                    return;
                }
                if let Some(token) = self.sessions.token_for(&path) {
                    self.apply_live_session_title(token, title, baseline, source, persist_source);
                    return;
                }
                if let Err(error) = persist_session_title_to_jsonl(Path::new(&path), &title) {
                    self.emit_error(error.to_string());
                    return;
                }
                if persist_source
                    && !self.persist_session_title_source(stable_session_id(&path), &title, source)
                {
                    return;
                }
                self.index_session(project_id, &path, Some(&title));
            }
        }
    }

    fn apply_live_session_title(
        &mut self,
        token: SessionToken,
        title: String,
        baseline: &str,
        source: SessionTitleSource,
        persist_source: bool,
    ) {
        let Some(key) = self.sessions.key_for(token).map(str::to_owned) else {
            return;
        };
        let Some(session) = self.sessions.get(&key) else {
            return;
        };
        let Ok(state) = session.runtime.command(json!({"type":"get_state"})) else {
            if source == SessionTitleSource::ExplicitSmartRename {
                self.report("notice-smart-rename-unavailable");
            }
            return;
        };
        let app_title_matches = self
            .state()
            .sessions
            .get(&session.project_id)
            .and_then(|sessions| sessions.iter().find(|summary| summary.pi_path == key))
            .is_some_and(|summary| summary.title.trim() == baseline.trim());
        if source == SessionTitleSource::ExplicitSmartRename {
            // Explicit requests are guarded by the sidebar baseline and are
            // cancelled immediately by manual or session_info renames. Pi's
            // sessionName can lag the indexed title for the selected session.
            if !app_title_matches {
                return;
            }
        } else {
            if let Some(runtime_title) = state.get("sessionName").and_then(Value::as_str) {
                if runtime_title.trim() != baseline.trim() {
                    return;
                }
            } else if !app_title_matches {
                return;
            }
        }
        let project_id = session.project_id;
        if session
            .runtime
            .command(json!({"type":"set_session_name", "name": title}))
            .is_err()
        {
            if source == SessionTitleSource::ExplicitSmartRename {
                self.report("notice-smart-rename-unavailable");
            }
            return;
        }
        self.expect_session_title_name(token, &title);
        if let Some(path) = state.get("sessionFile").and_then(Value::as_str) {
            if persist_source
                && !self.persist_session_title_source(stable_session_id(path), &title, source)
            {
                return;
            }
            self.index_session(project_id, path, Some(&title));
        }
    }

    fn cancel_pending_title(&mut self, token: SessionToken) {
        self.expected_session_title_names.remove(&token);
        self.deferred_session_titles.remove(&token);
        let request_ids = self
            .pending_session_titles
            .iter()
            .filter_map(|(request_id, pending)| {
                matches!(pending.target, SessionTitleTarget::Live(pending_token) if pending_token == token)
                    .then_some(*request_id)
            })
            .collect::<Vec<_>>();
        for request_id in request_ids {
            self.pending_session_titles.remove(&request_id);
            if let Some(service) = self.one_shot_ai.as_ref() {
                service.cancel(request_id);
            }
        }
    }

    fn expect_session_title_name(&mut self, token: SessionToken, name: &str) {
        let names = self.expected_session_title_names.entry(token).or_default();
        if names.len() >= MAX_EXPECTED_SESSION_TITLE_NAMES {
            names.pop_front();
        }
        names.push_back(name.trim().to_owned());
    }

    fn consume_expected_session_title_name(&mut self, token: SessionToken, name: &str) -> bool {
        let (expected, empty) = self
            .expected_session_title_names
            .get_mut(&token)
            .map(|names| {
                let expected = names
                    .iter()
                    .position(|expected| expected == name.trim())
                    .and_then(|index| names.remove(index))
                    .is_some();
                (expected, names.is_empty())
            })
            .unwrap_or((false, false));
        if empty {
            self.expected_session_title_names.remove(&token);
        }
        expected
    }

    fn cancel_pending_titles_for_path(&mut self, path: &str) {
        if let Some(token) = self.sessions.token_for(path) {
            self.cancel_pending_title(token);
        }
        let request_ids = self
            .pending_session_titles
            .iter()
            .filter_map(|(request_id, pending)| {
                let matches = match &pending.target {
                    SessionTitleTarget::Live(token) => self.sessions.key_for(*token) == Some(path),
                    SessionTitleTarget::Stored {
                        path: pending_path, ..
                    } => pending_path == path,
                };
                matches.then_some(*request_id)
            })
            .collect::<Vec<_>>();
        for request_id in request_ids {
            self.pending_session_titles.remove(&request_id);
            if let Some(service) = self.one_shot_ai.as_ref() {
                service.cancel(request_id);
            }
        }
    }

    /// Take one batch off the pump. Events from the visible session update the
    /// conversation view; background sessions only update bookkeeping (busy
    /// dots, sidebar titles) so their progress survives until they are shown.
    pub(crate) fn handle_deliveries(&mut self, batch: Vec<Delivery>) {
        self.drain_hook_host_audits();
        for (token, event) in batch {
            // The key is resolved now rather than when the event was sent: a
            // session is re-keyed as soon as Pi reports its transcript path, and
            // events sent before that would otherwise name a key that has gone.
            let Some(key) = self.sessions.key_for(token).map(str::to_owned) else {
                // Its session has been removed, so there is nothing to update.
                continue;
            };
            self.handle_runtime_event(&key, event);
        }
    }

    fn handle_runtime_event(&mut self, key: &str, event: RuntimeEvent) {
        let is_active = self.sessions.active_key() == Some(key);
        match event {
            RuntimeEvent::Agent(value) => self.apply_agent_event(key, value),
            RuntimeEvent::ExtensionUi(value) => {
                if let Some(prompt) = dialogs::Prompt::from_extension(key, &value) {
                    self.emit_effect(signals::ApplicationEffect::Prompt(prompt));
                }
            }
            RuntimeEvent::Interaction(value) => {
                if let Some(prompt) = dialogs::Prompt::from_interaction(key, &value) {
                    self.emit_effect(signals::ApplicationEffect::Prompt(prompt));
                }
            }
            RuntimeEvent::HookAudit(record) => self.persist_hook_audit(key, record),
            RuntimeEvent::ToolAudit(record) => self.persist_tool_audit(key, record),
            RuntimeEvent::Stderr(message) => {
                if is_active && !message.trim().is_empty() {
                    self.emit_error(message);
                }
            }
            RuntimeEvent::Exited { generation, code } => {
                let current = self
                    .sessions
                    .get(key)
                    .is_some_and(|session| session.runtime.generation() == generation);
                if !current {
                    return;
                }
                if let Some(token) = self.sessions.token_for(key) {
                    self.cancel_pending_title(token);
                }
                self.sessions.remove(key);
                // Nothing is left to answer, and a dialog still up would ask the
                // reader to unblock a process that has gone.
                self.emit_effect(signals::ApplicationEffect::SessionClosed(key.to_owned()));
                self.apply(Action::SessionRunning {
                    path: key.to_owned(),
                    running: false,
                });
                if is_active {
                    self.apply(Action::SetSessionStatus(SessionStatus::Failed(format!(
                        "Pi exited: {code:?}"
                    ))));
                }
            }
            RuntimeEvent::Error(error) => {
                if is_active {
                    self.emit_error(error);
                }
            }
            RuntimeEvent::RpcResponse(_) => {}
        }
    }

    fn drain_hook_host_audits(&mut self) {
        if self.store.is_none() {
            return;
        }
        let audits = self.hook_host.drain_audits();
        if audits.is_empty() {
            return;
        }
        let selected_project_path = self
            .state()
            .selected_project
            .and_then(|id| self.find_project(id))
            .map(|project| project.path.clone());
        let refresh_selected = selected_project_path
            .as_ref()
            .is_some_and(|selected| audits.iter().any(|audit| &audit.project_path == selected));
        let Some(store) = self.store.as_ref() else {
            self.hook_host.requeue_audits(audits);
            return;
        };
        for (index, audit) in audits.iter().enumerate() {
            let entry = HookAuditEntry {
                project_path: audit.project_path.clone(),
                hook_id: audit.hook_id.clone(),
                event: audit.event.clone(),
                outcome: audit.outcome.clone(),
                duration_ms: audit.duration_ms,
                output_truncated: false,
                revision: audit.revision.clone(),
                created_at_ms: now_ms(),
            };
            if let Err(error) = store.append_hook_audit(&entry) {
                self.hook_host.requeue_audits(audits[index..].to_vec());
                self.emit_error(error.to_string());
                return;
            }
        }
        if refresh_selected && let Some(project_path) = selected_project_path {
            self.refresh_hook_audit(&project_path);
        }
    }

    fn persist_hook_audit(&mut self, session_key: &str, record: HookAuditRecord) {
        let Some(project) = self
            .sessions
            .get(session_key)
            .and_then(|session| self.find_project(session.project_id))
        else {
            return;
        };
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let event = serde_json::to_value(record.event)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "unknown".into());
        let outcome = serde_json::to_value(record.outcome)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "failed".into());
        if let Err(error) = store.append_hook_audit(&HookAuditEntry {
            project_path: project.path.clone(),
            hook_id: record.hook_id,
            event,
            outcome,
            duration_ms: record.duration_ms,
            output_truncated: record.output_truncated,
            revision: record.revision,
            created_at_ms: now_ms(),
        }) {
            self.emit_error(error.to_string());
        } else if self.state().selected_project == Some(project.id) {
            self.refresh_hook_audit(&project.path);
        }
    }

    fn persist_tool_audit(&mut self, session_key: &str, record: ToolAuditRecord) {
        let Some(project) = self
            .sessions
            .get(session_key)
            .and_then(|session| self.find_project(session.project_id))
        else {
            return;
        };
        let Some(store) = self.store.as_ref() else {
            return;
        };
        let outcome = serde_json::to_value(record.outcome)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "failed".into());
        if let Err(error) = store.append_tool_audit(&ToolAuditEntry {
            project_path: project.path.clone(),
            tool: record.tool,
            agent_id: record.agent_id,
            agent_level: record.agent_level,
            outcome,
            reason: record.reason,
            request_id: record.request_id,
            arguments_hash: record.arguments_hash,
            duration_ms: record.duration_ms,
            created_at_ms: now_ms(),
        }) {
            self.emit_error(error.to_string());
        }
    }

    /// Translate one agent event and carry out what the translation asked for.
    ///
    /// The reading of the event lives in `engine::events`; what stays here is
    /// only what needs this process's store, window, and Pi connection.
    fn apply_agent_event(&mut self, key: &str, event: Value) {
        let settled = event.get("type").and_then(Value::as_str) == Some("agent_settled");
        let ready = event.get("type").and_then(Value::as_str) == Some("agent_ready");
        let is_active = self.sessions.active_key() == Some(key);
        let now = now_ms();
        // The conversation is cloned because `translate` reads it while holding
        // the turn mutably. Only tool events look at it, and only to find one
        // entry, so a borrow split would cost more in plumbing than this does.
        // Read before the pool is borrowed: state and pool are both `self`.
        let conversation = self.state().conversation.clone();
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        let outcome = events::translate(
            &event,
            events::Context {
                key,
                is_active,
                conversation: &conversation,
                now_ms: now,
            },
            &mut session.turn,
        );
        for action in outcome.actions {
            self.apply(action);
        }
        for effect in outcome.effects {
            self.perform_effect(key, effect);
        }
        if settled || ready {
            self.restart_hooks_if_project_settled(key);
        }
        if ready {
            self.flush_pending_hook_inputs(key);
        }
    }

    /// Carry out one effect the translation asked for.
    ///
    /// Each of these needs something `engine::events` deliberately has no handle
    /// on: the Pi process, the session index, or the composer.
    fn perform_effect(&mut self, key: &str, effect: events::Effect) {
        match effect {
            // A fresh session starts before Pi has written a transcript, and
            // fork and clone move an existing one, so the pooled key is not
            // always where the file ends up.
            events::Effect::SyncSessionFile => {
                if let Some((project_id, path, name)) = self.reported_session_file(key) {
                    self.index_session(project_id, &path, name.as_deref());
                    if path != key {
                        self.rekey_session(key, &path);
                    }
                }
            }
            events::Effect::RenameSessionFile(name) => {
                let normalized_name = name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty());
                let expected = self
                    .sessions
                    .token_for(key)
                    .zip(normalized_name)
                    .is_some_and(|(token, name)| {
                        self.consume_expected_session_title_name(token, name)
                    });
                if let Some((project_id, path, _)) = self.reported_session_file(key) {
                    let unchanged = normalized_name.is_some_and(|name| {
                        self.state()
                            .sessions
                            .get(&project_id)
                            .and_then(|sessions| {
                                sessions.iter().find(|session| session.pi_path == path)
                            })
                            .is_some_and(|session| session.title.trim() == name)
                    });
                    let manual_change = !expected && !unchanged && normalized_name.is_some();
                    if manual_change && let Some(token) = self.sessions.token_for(key) {
                        // A changed external name wins over a late background
                        // result and every generated title.
                        self.cancel_pending_title(token);
                    }
                    if manual_change
                        && let Some(name) = normalized_name
                        && !self.persist_session_title_source(
                            stable_session_id(&path),
                            name,
                            SessionTitleSource::ExplicitSmartRename,
                        )
                    {
                        return;
                    }
                    self.index_session(project_id, &path, name.as_deref());
                }
            }
            events::Effect::ReloadEntries => {
                let _ = self.load_current_entries();
            }
            events::Effect::RefreshControls => self.refresh_runtime_controls(),
            events::Effect::SendPendingPrompt((content, attachments, mode)) => {
                // A switch-triggered compaction held this prompt back so the
                // prior model compacted the history first; now the new model
                // takes over and the turn continues.
                if self.state().pending_model.is_some() {
                    self.apply_pending_model(key);
                }
                self.send_prompt(key, content, attachments, mode);
            }
        }
    }

    /// Ask a session's own process where it is writing, and under what name.
    fn reported_session_file(&self, key: &str) -> Option<(ProjectId, String, Option<String>)> {
        let session = self.sessions.get(key)?;
        let state = session.runtime.command(json!({"type":"get_state"})).ok()?;
        let path = state.get("sessionFile").and_then(Value::as_str)?;
        Some((
            session.project_id,
            path.to_owned(),
            state
                .get("sessionName")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ))
    }

    /// Settings like the bash policy are process launch flags, so changing
    /// them restarts every session of the selected project (sessions resume
    /// from disk; in-flight runs are aborted by the restart, as before).
    fn restart_selected_project(&mut self) {
        if let Some(project) = self.state().selected_project {
            self.stop_project_runtimes(project);
            self.start_project(project);
        }
    }

    fn hooks_changed(&mut self) -> bool {
        let Some(project) = self
            .state()
            .selected_project
            .and_then(|id| self.find_project(id))
        else {
            return false;
        };
        let config = self.load_hooks(&project.path);
        self.hook_revisions.get(&project.id).map(String::as_str) != Some(config.revision())
    }

    fn request_hook_reload(&mut self, project_id: ProjectId) {
        self.pending_hook_reloads.insert(project_id);
        self.restart_hooks_for_settled_project(project_id);
    }

    fn restart_hooks_if_project_settled(&mut self, key: &str) {
        let Some(project_id) = self.sessions.get(key).map(|session| session.project_id) else {
            return;
        };
        self.restart_hooks_for_settled_project(project_id);
    }

    fn restart_hooks_for_settled_project(&mut self, project_id: ProjectId) {
        if !self.pending_hook_reloads.contains(&project_id) {
            return;
        }
        let visible_turn_is_busy = self.state().selected_project == Some(project_id)
            && matches!(
                self.state().session_status,
                SessionStatus::Streaming | SessionStatus::Compacting
            );
        let project_has_busy_turn = self
            .sessions
            .iter()
            .any(|(_, session)| session.project_id == project_id && session.is_running());
        if visible_turn_is_busy || project_has_busy_turn {
            return;
        }
        self.pending_hook_reloads.remove(&project_id);
        let remains_selected = self.state().selected_project == Some(project_id);
        self.stop_project_runtimes(project_id);
        if remains_selected {
            self.start_project(project_id);
        }
    }

    fn flush_pending_hook_inputs(&mut self, key: &str) {
        let Some(project_id) = self.sessions.get(key).map(|session| session.project_id) else {
            return;
        };
        let Some(mut pending) = self.pending_hook_inputs.remove(&project_id) else {
            return;
        };
        if !self.can_submit_prompt() {
            self.pending_hook_inputs.insert(project_id, pending);
            return;
        }
        while let Some((content, attachments, mode)) = pending.pop_front() {
            self.submit_prompt(content, attachments, mode);
        }
    }

    /// Carry a decision back over whichever channel asked.
    fn send_answer(&mut self, answer: dialogs::Answer) {
        let Some(session) = self.sessions.get(answer.session_key()) else {
            self.report("notice-asking-session-gone");
            return;
        };
        let result = match &answer {
            dialogs::Answer::Extension {
                request_id,
                response,
                ..
            } => {
                // Pi reads the field its method waits on: `confirmed` for a
                // confirm, `value` for a select, input, or editor, and
                // `cancelled` when the dialog was closed unanswered.
                let payload = match response {
                    dialogs::ExtensionResponse::Confirmed(confirmed) => {
                        json!({"type":"extension_ui_response", "id": request_id, "confirmed": confirmed})
                    }
                    dialogs::ExtensionResponse::Value(value) => {
                        json!({"type":"extension_ui_response", "id": request_id, "value": value})
                    }
                    dialogs::ExtensionResponse::Cancelled => {
                        json!({"type":"extension_ui_response", "id": request_id, "cancelled": true})
                    }
                };
                session.runtime.respond_extension_ui(payload)
            }
            dialogs::Answer::Interaction {
                request_id,
                decision,
                ..
            } => session
                .runtime
                .resolve_user_interaction(request_id.clone(), decision.clone())
                // The supervisor echoes the decision back; nothing here needs it.
                .map(|_| ()),
        };
        if let Err(error) = result {
            self.emit_error(error.to_string());
        }
    }
}

/// Where Pi's own changelog lives.
const CHANGELOG_URL: &str =
    "https://github.com/badlogic/pi-mono/blob/main/packages/coding-agent/CHANGELOG.md";

/// The body `/info` answers with.
///
/// Assembled from labels rather than one stored sentence per language: the
/// figures are the content, and a stored sentence with six holes in it drifts out
/// of step with them.
fn session_info(metrics: &SessionMetrics, language: Language) -> String {
    let say = |key| strings::text(key, language);
    format!(
        "{}\n\n{}: {} ({} {}, {} {})\n{}: {}\n{}: {}\n{}: ${:.4}",
        say("session-info"),
        say("info-messages"),
        metrics.total_messages,
        metrics.user_messages,
        say("info-user"),
        metrics.assistant_messages,
        say("info-assistant"),
        say("info-tool-calls"),
        metrics.tool_calls,
        say("info-tokens"),
        metrics.total_tokens,
        say("info-cost"),
        metrics.cost_microusd as f64 / 1_000_000.0
    )
}

/// The body `/hotkeys` answers with.
///
/// The keystrokes are literals in both languages — they are what is printed on
/// the keyboard — so only what each one does is translated.
fn hotkeys(language: Language) -> String {
    let say = |key| strings::text(key, language);
    let lines = [
        ("Enter", say("hint-send")),
        ("Shift+Enter", say("hint-newline")),
        ("/", say("hint-slash")),
        ("Up/Down", say("hint-arrows")),
        ("Tab / Enter", say("hint-confirm")),
        ("Esc", say("hint-escape")),
    ]
    .map(|(key, description)| format!("{key}: {description}"))
    .join("\n");
    format!("{}\n\n{lines}", say("hotkeys"))
}

fn global_hook_fallback() -> hooks::manifest::PreparedHookManifest {
    hooks::manifest::empty_manifest("v1:global-empty")
}

/// Pi accepts an environment reference here, keeping API keys out of models.json.
fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(target_os = "macos")]
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt};

    use pi_whim_core::ConversationItem;
    use pi_whim_engine::{changes::StateTopic, session::is_large_paste};
    #[cfg(target_os = "macos")]
    use pi_whim_hook_host::{
        ApprovedHookManifest, EventRegistry, HookAuditOutcome, HookHostManager, HookManifest,
        HookScopeKey,
    };
    use pi_whim_persistence::hook_manifest_fingerprint;
    use pi_whim_runtime::FakeRuntime;
    #[cfg(target_os = "macos")]
    use pi_whim_runtime::RuntimeHookScope;
    use serde_json::json;
    use tempfile::TempDir;

    #[cfg(target_os = "macos")]
    #[test]
    fn global_commit_observes_the_selected_project_scope() -> Result<(), String> {
        let _guard = hooks::hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let project = project("selected", directory.path());
        let mut app = test_application(&directory, FakeRuntime::default());
        app.apply(Action::ProjectsLoaded(vec![project.clone()]));
        app.apply(Action::SelectProject(project.id));

        let script = directory.path().join("state-observe.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
IFS= read -r hello || exit 2
hello_id=$(printf '%s\n' "$hello" | sed -n 's/.*"hello_id":"\([^"]*\)".*/\1/p')
printf '{"type":"ready","hook_id":"state-observe","event":"pi.state.committed","kind":"observe","hello_id":"%s"}\n' "$hello_id"
while IFS= read -r request; do
  request_id=$(printf '%s\n' "$request" | sed -n 's/.*"request_id":"\([^"]*\)".*/\1/p')
  printf '{"type":"response","request_id":"%s","hook_id":"state-observe","event":"pi.state.committed","response":{"kind":"observe","accepted":true}}\n' "$request_id"
done
"#,
        )
        .map_err(|error| error.to_string())?;
        let mut permissions = fs::metadata(&script)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&script, permissions).map_err(|error| error.to_string())?;

        let manifest = HookManifest::parse_json(
            &json!({
                "version": 2,
                "hooks": [{
                    "id": "state-observe",
                    "event": "pi.state.committed",
                    "kind": "observe",
                    "command": [script],
                    "fields": [
                        "revision", "topics", "action_count", "coalesced",
                        "scope", "commit_source", "project_id"
                    ]
                }]
            })
            .to_string(),
        )
        .map_err(|error| error.to_string())?
        .with_revision("state-r1");
        let fingerprints = BTreeMap::from([(
            "state-observe".to_owned(),
            hook_manifest_fingerprint(&fs::read(&script).map_err(|error| error.to_string())?),
        )]);
        let approved = ApprovedHookManifest::new(manifest, "state-r1", fingerprints)
            .map_err(|error| error.to_string())?;
        let manager = HookHostManager::new_with_registry(EventRegistry::default(), approved)
            .map_err(|error| error.to_string())?;
        let key = HookScopeKey::project(directory.path(), "state-r1")
            .map_err(|error| error.to_string())?;
        let scope = manager
            .open_scope(key, None)
            .map_err(|error| error.to_string())?;
        let expected_scope_id = scope.scope_id();
        app.hook_host.retain_scope_for_test(
            directory.path(),
            RuntimeHookScope::new(manager.clone(), scope),
        );
        let (audit_sender, audit_receiver) = crossbeam_channel::unbounded();
        let _subscription = manager.audit_signal().subscribe_fn(move |event| {
            let _ = audit_sender.send(event);
        });

        app.apply(Action::SetLanguage(Language::SimplifiedChinese));

        let audit = audit_receiver
            .recv_timeout(Duration::from_secs(2))
            .map_err(|error| error.to_string())?;
        assert_eq!(audit.event, "pi.state.committed");
        assert_eq!(audit.outcome, HookAuditOutcome::Observed);
        assert_eq!(audit.scope_id, expected_scope_id);
        assert!(audit_receiver.try_recv().is_err());
        Ok(())
    }

    #[test]
    fn the_session_info_body_translates_its_labels_and_keeps_its_figures() {
        let metrics = SessionMetrics {
            total_messages: 12,
            user_messages: 5,
            assistant_messages: 7,
            tool_calls: 3,
            total_tokens: 4096,
            cost_microusd: 12_345,
        };

        let english = session_info(&metrics, Language::English);
        assert!(english.starts_with("Session info"));
        assert!(english.contains("Messages: 12 (5 user, 7 assistant)"));
        assert!(english.contains("Cost: $0.0123"));

        // Same figures, different words: the numbers are the content and must
        // not depend on which language assembled the sentence.
        let chinese = session_info(&metrics, Language::SimplifiedChinese);
        assert!(chinese.starts_with("会话信息"));
        assert!(chinese.contains("12 (5"));
        assert!(chinese.contains("$0.0123"));
        assert!(!chinese.contains('?'));
    }

    #[test]
    fn the_hotkey_body_keeps_the_keystrokes_and_translates_the_rest() {
        // The keystrokes are printed on the keyboard, so they read the same in
        // both languages; only what each one does changes.
        for language in [Language::English, Language::SimplifiedChinese] {
            let body = hotkeys(language);
            for key in ["Enter", "Shift+Enter", "Up/Down", "Tab / Enter", "Esc"] {
                assert!(body.contains(key), "{key} missing in {language:?}");
            }
            assert!(!body.contains('?'), "a key is missing in {language:?}");
        }

        assert!(hotkeys(Language::English).contains("Enter: send"));
        assert!(hotkeys(Language::SimplifiedChinese).contains("Enter: 发送"));
    }

    struct EffectProbe {
        receiver: crossbeam_channel::Receiver<ApplicationEffect>,
        _subscription: pi_whim_signal::Subscription,
    }

    impl EffectProbe {
        fn new(app: &PiWhimApplication<FakeRuntime>) -> Self {
            let (sender, receiver) = crossbeam_channel::unbounded();
            let subscription = app.application_effects().subscribe_fn(move |effect| {
                let _ = sender.send(effect);
            });
            app.activate_effect_delivery();
            receiver.try_iter().for_each(drop);
            Self {
                receiver,
                _subscription: subscription,
            }
        }

        fn notices(&self) -> Vec<pi_whim_engine::notice::Notice> {
            self.receiver
                .try_iter()
                .filter_map(|effect| match effect {
                    ApplicationEffect::Notice(notice) => Some(notice),
                    _ => None,
                })
                .collect()
        }
    }

    fn test_application(
        directory: &TempDir,
        runtime: FakeRuntime,
    ) -> PiWhimApplication<FakeRuntime> {
        // Pooled runtimes are clones of the prototype so the observer sees
        // every start and command across all session processes.
        let factory_runtime = runtime;
        PiWhimApplication {
            engine: EngineState::new(),
            signals: signals::ApplicationSignals::default(),
            store: Some(SqliteStore::open(directory.path().join("test.sqlite")).unwrap()),
            secrets: MacosKeychainStore::default(),
            runtime_factory: Box::new(move || factory_runtime.clone()),
            sessions: SessionPool::new(),
            capability_resolver: ModelCapabilityResolver::new(
                &pi_whim_catalog::SharedCatalog::default(),
                false,
            ),
            sessions_root_override: Some(directory.path().join("sessions")),
            agent_directory_override: Some(directory.path().join("agent")),
            hook_config_root_override: Some(directory.path().join("config")),
            hook_host: hooks::ApplicationHookHost::default(),
            hook_revisions: HashMap::new(),
            pending_hook_reloads: HashSet::new(),
            pending_hook_inputs: HashMap::new(),
            attachment_store: AttachmentStore::open(directory.path().join("attachments")).unwrap(),
            control_updates: crossbeam_channel::unbounded(),
            one_shot_ai: None,
            one_shot_generation: 0,
            one_shot_installs: crossbeam_channel::unbounded(),
            one_shot_completions: crossbeam_channel::unbounded(),
            pending_session_titles: HashMap::new(),
            deferred_session_titles: HashMap::new(),
            expected_session_title_names: HashMap::new(),
            title_eligible: HashSet::new(),
            title_attempted: HashSet::new(),
        }
    }

    #[test]
    fn ui_command_envelope_uses_selected_project_without_session_path_context() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        let project = project("selected", directory.path());
        app.apply(Action::ProjectsLoaded(vec![project.clone()]));
        app.apply(Action::SelectProject(project.id));

        let envelope = app.ui_command_envelope(AppCommand::SetThinkingLevel(ThinkingLevel::Medium));

        assert_eq!(
            envelope.source(),
            pi_whim_engine::commands::CommandSource::Ui
        );
        assert_eq!(envelope.project_id(), Some(project.id));
        assert_eq!(envelope.session_key(), None);
        assert!(!format!("{envelope:?}").contains(directory.path().to_string_lossy().as_ref()));
    }

    #[test]
    fn single_action_commit_emits_internal_effect_change_set() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = app.change_sets().subscribe_fn(move |change_set| {
            let _ = sender.send(change_set);
        });

        app.apply(Action::SetLanguage(Language::SimplifiedChinese));

        let change_set = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the committed action publishes a change set");
        assert_eq!(change_set.revision.get(), 1);
        assert_eq!(change_set.source, CommitSource::InternalEffect);
        assert_eq!(change_set.changed_topics, vec![StateTopic::Preferences]);
        assert_eq!(change_set.action_count, 1);
    }

    #[test]
    fn action_batch_commits_once_and_emits_one_change_set() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = app.change_sets().subscribe_fn(move |change_set| {
            let _ = sender.send(change_set);
        });

        let returned = app
            .apply_batch(
                [
                    Action::SetLanguage(Language::SimplifiedChinese),
                    Action::ClearConversation,
                ],
                CommitContext::global(CommitSource::Test),
            )
            .expect("the reducer batch commits");

        let observed = receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("the committed batch publishes a change set");
        assert_eq!(observed, returned);
        assert_eq!(observed.revision.get(), 1);
        assert_eq!(observed.source, CommitSource::Test);
        assert_eq!(
            observed.changed_topics,
            vec![StateTopic::Preferences, StateTopic::Conversation]
        );
        assert_eq!(observed.action_count, 2);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn disabled_project_hooks_immediately_revoke_the_retained_scope() {
        let directory = tempfile::tempdir().unwrap();
        let project_path = directory.path().join("project");
        fs::create_dir_all(project_path.join(".pi-whim")).unwrap();
        let manifest_path = project_path.join(".pi-whim/hooks.json");
        let entrypoint = project_path.join("policy-hook");
        fs::write(&entrypoint, "#!/bin/sh\nexit 0\n").unwrap();
        let manifest = json!({
            "version": 1,
            "hooks": [{
                "id": "policy",
                "event": "tool_dispatching",
                "command": [entrypoint]
            }]
        });
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        let project = project("hooks", &project_path);
        app.apply(Action::ProjectsLoaded(vec![project.clone()]));
        app.apply(Action::SelectProject(project.id));

        assert!(app.load_hooks(&project.path).legacy.hooks.is_empty());
        let record = match &app.state().project_hook_status {
            ProjectHookStatus::ApprovalRequired {
                fingerprint,
                grants_hash,
                grants,
            } => pi_whim_persistence::HookProjectTrustRecord {
                fingerprint: fingerprint.clone(),
                grants_hash: grants_hash.clone(),
                grants: grants.clone(),
                approved_at_ms: 1,
            },
            status => panic!("unexpected status: {status:?}"),
        };
        app.store
            .as_ref()
            .unwrap()
            .approve_project_hooks(&project.path, &record)
            .unwrap();
        let loaded = app.load_hooks(&project.path);
        assert_eq!(loaded.legacy.hooks.len(), 1);
        let scope = app
            .hook_host
            .resolve_scope(
                Path::new(&project.path),
                &loaded.global,
                loaded.project.as_ref(),
            )
            .unwrap()
            .unwrap();
        let retained = scope.scope();
        assert!(app.hook_host.controller(Path::new(&project.path)).is_some());

        fs::write(&entrypoint, "#!/bin/sh\nexit 1\n").unwrap();
        app.approve_project_hooks(&record.fingerprint, &record.grants_hash);
        assert!(matches!(
            app.state().project_hook_status,
            ProjectHookStatus::ApprovalRequired { .. }
        ));
        assert!(retained.is_revoked());
        assert!(app.hook_host.controller(Path::new(&project.path)).is_none());

        let changed_record = match &app.state().project_hook_status {
            ProjectHookStatus::ApprovalRequired {
                fingerprint,
                grants_hash,
                grants,
            } => pi_whim_persistence::HookProjectTrustRecord {
                fingerprint: fingerprint.clone(),
                grants_hash: grants_hash.clone(),
                grants: grants.clone(),
                approved_at_ms: 2,
            },
            status => panic!("unexpected status: {status:?}"),
        };
        app.store
            .as_ref()
            .unwrap()
            .approve_project_hooks(&project.path, &changed_record)
            .unwrap();
        let loaded = app.load_hooks(&project.path);
        let scope = app
            .hook_host
            .resolve_scope(
                Path::new(&project.path),
                &loaded.global,
                loaded.project.as_ref(),
            )
            .unwrap()
            .unwrap();
        let retained = scope.scope();

        fs::remove_file(&manifest_path).unwrap();
        assert!(app.load_hooks(&project.path).legacy.hooks.is_empty());
        assert!(matches!(
            app.state().project_hook_status,
            ProjectHookStatus::NotPresent
        ));
        assert!(retained.is_revoked());
        assert!(app.hook_host.controller(Path::new(&project.path)).is_none());
    }

    #[test]
    fn changed_hooks_wait_for_a_busy_turn_and_preserve_the_input() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let project_id = app.state().selected_project.unwrap();
        app.apply(Action::SetSessionStatus(SessionStatus::Streaming));

        let config_root = directory.path().join("config");
        fs::create_dir_all(&config_root).unwrap();
        let entrypoint = directory.path().join("global-hook");
        fs::write(&entrypoint, "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(
            config_root.join("hooks.json"),
            serde_json::to_vec(&json!({
                "version": 1,
                "hooks": [{
                    "id": "gate",
                    "event": "tool_dispatching",
                    "command": [entrypoint]
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        app.submit_prompt("held input".into(), Vec::new(), SubmitMode::Prompt);
        assert_eq!(
            app.pending_hook_inputs.get(&project_id).map(VecDeque::len),
            Some(1)
        );
        assert_eq!(observer.starts().len(), 1);

        let old_key = app.sessions.active_key().unwrap().to_owned();
        app.apply_agent_event(&old_key, json!({"type":"agent_settled"}));
        assert_eq!(observer.starts().len(), 2);
        assert_eq!(
            app.pending_hook_inputs.get(&project_id).map(VecDeque::len),
            Some(1)
        );

        let new_key = app.sessions.active_key().unwrap().to_owned();
        app.apply_agent_event(&new_key, json!({"type":"agent_ready"}));
        assert!(!app.pending_hook_inputs.contains_key(&project_id));
        assert!(
            app.state()
                .conversation
                .iter()
                .any(|item| item.full_text == "held input")
        );
    }

    #[test]
    fn hook_trust_changes_wait_for_the_active_turn_to_settle() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join(".pi-whim")).unwrap();
        let entrypoint = directory.path().join("policy-hook");
        fs::write(&entrypoint, "#!/bin/sh\nexit 0\n").unwrap();
        fs::write(
            directory.path().join(".pi-whim/hooks.json"),
            serde_json::to_vec(&json!({
                "version": 1,
                "hooks": [{
                    "id": "policy",
                    "event": "tool_dispatching",
                    "command": [entrypoint]
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let project_id = app.state().selected_project.unwrap();
        let (fingerprint, grants_hash) = match &app.state().project_hook_status {
            ProjectHookStatus::ApprovalRequired {
                fingerprint,
                grants_hash,
                ..
            } => (fingerprint.clone(), grants_hash.clone()),
            status => panic!("unexpected status: {status:?}"),
        };
        app.apply(Action::SetSessionStatus(SessionStatus::Streaming));

        app.approve_project_hooks(&fingerprint, "stale-grants-hash");
        assert!(matches!(
            app.state().project_hook_status,
            ProjectHookStatus::ApprovalRequired { .. }
        ));
        assert_eq!(observer.starts().len(), 1);

        app.approve_project_hooks(&fingerprint, &grants_hash);
        assert_eq!(observer.starts().len(), 1);
        assert!(app.pending_hook_reloads.contains(&project_id));
        assert!(matches!(
            app.state().project_hook_status,
            ProjectHookStatus::Approved { .. }
        ));

        let old_key = app.sessions.active_key().unwrap().to_owned();
        app.apply_agent_event(&old_key, json!({"type":"agent_settled"}));
        assert_eq!(observer.starts().len(), 2);
        assert!(!app.pending_hook_reloads.contains(&project_id));

        app.apply(Action::SetSessionStatus(SessionStatus::Streaming));
        app.revoke_project_hooks();
        assert_eq!(observer.starts().len(), 2);
        assert!(app.pending_hook_reloads.contains(&project_id));
        assert!(matches!(
            app.state().project_hook_status,
            ProjectHookStatus::ApprovalRequired { .. }
        ));

        let approved_key = app.sessions.active_key().unwrap().to_owned();
        app.apply_agent_event(&approved_key, json!({"type":"agent_settled"}));
        assert_eq!(observer.starts().len(), 3);
        assert!(!app.pending_hook_reloads.contains(&project_id));
    }

    #[test]
    fn hook_reload_waits_for_background_sessions_in_the_project() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let project_id = app.state().selected_project.unwrap();
        let background_path = directory.path().join("background.jsonl");
        let background = app
            .launch_session(project_id, Some(&background_path.to_string_lossy()))
            .unwrap();
        app.sessions.get_mut(&background).unwrap().turn.running = true;
        assert_eq!(observer.starts().len(), 2);

        app.request_hook_reload(project_id);
        assert_eq!(observer.starts().len(), 2);
        assert!(app.pending_hook_reloads.contains(&project_id));

        app.apply_agent_event(&background, json!({"type":"agent_settled"}));
        assert_eq!(observer.starts().len(), 3);
        assert!(!app.pending_hook_reloads.contains(&project_id));
    }

    #[test]
    fn hook_audit_events_persist_only_metadata_and_refresh_settings_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        app.handle_runtime_event(
            &key,
            RuntimeEvent::HookAudit(HookAuditRecord {
                hook_id: "policy".into(),
                event: pi_whim_core::HookEvent::ToolDispatching,
                outcome: pi_whim_core::HookAuditOutcome::Denied,
                duration_ms: 7,
                output_truncated: false,
                revision: "sha256:test".into(),
            }),
        );
        assert_eq!(app.state().hook_audit.len(), 1);
        assert_eq!(app.state().hook_audit[0].hook_id, "policy");
        assert_eq!(app.state().hook_audit[0].outcome, "denied");
    }

    fn project(name: &str, path: &Path) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: path.to_string_lossy().into_owned(),
            pinned: false,
            last_opened_ms: 1,
        }
    }

    /// Give the app one active session so commands that target the visible
    /// session have a process to talk to.
    fn start_test_session(app: &mut PiWhimApplication<FakeRuntime>, directory: &TempDir) {
        let project = project("test", directory.path());
        app.store.as_ref().unwrap().save_project(&project).unwrap();
        app.apply(Action::ProjectsLoaded(vec![project.clone()]));
        app.start_new_session(project.id);
        assert!(app.sessions.active_key().is_some());
    }

    #[test]
    fn each_new_session_gets_its_own_process_and_reuses_only_empty_sessions() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        let first = project("first", &directory.path().join("first"));
        let second = project("second", &directory.path().join("second"));
        fs::create_dir_all(&first.path).unwrap();
        fs::create_dir_all(&second.path).unwrap();
        app.store.as_ref().unwrap().save_project(&first).unwrap();
        app.store.as_ref().unwrap().save_project(&second).unwrap();
        app.apply(Action::ProjectsLoaded(vec![first.clone(), second.clone()]));

        app.start_new_session(first.id);
        assert_eq!(observer.starts().len(), 1);

        // A still-empty visible session is reused instead of spawning blanks.
        app.start_new_session(first.id);
        assert_eq!(observer.starts().len(), 1);

        app.apply(Action::UpsertConversation(ConversationItem {
            id: "user-1".into(),
            role: ConversationRole::User,
            full_text: "hello".into(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }));
        app.start_new_session(first.id);
        assert_eq!(observer.starts().len(), 2);

        app.start_new_session(second.id);
        assert_eq!(observer.starts().len(), 3);

        // Sessions never share a process, so nothing ever sends Pi's
        // aborting new_session/switch_session RPCs, and every earlier
        // session stays alive in the pool.
        assert!(!observer.commands().iter().any(|command| {
            matches!(
                command.get("type").and_then(Value::as_str),
                Some("new_session") | Some("switch_session")
            )
        }));
        assert_eq!(app.sessions.iter().count(), 3);
    }

    #[test]
    fn switching_sessions_keeps_running_sessions_alive() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let project_id = app.state().selected_project.unwrap();
        let running_key = app.sessions.active_key().unwrap().to_owned();

        // Session A starts streaming.
        app.submit_prompt("long task".into(), Vec::new(), SubmitMode::Prompt);
        assert!(
            app.sessions
                .get(&running_key)
                .is_some_and(SessionRuntime::is_running)
        );

        // Switching away must not abort A: no abort/new_session/switch_session
        // RPCs, and A's process stays pooled with its running flag set.
        app.switch_session(project_id, "/sessions/b.jsonl".into());
        assert_eq!(app.sessions.active_key(), Some("/sessions/b.jsonl"));
        assert_eq!(app.sessions.iter().count(), 2);
        assert!(
            app.sessions
                .get(&running_key)
                .is_some_and(SessionRuntime::is_running)
        );
        assert!(!observer.commands().iter().any(|command| {
            matches!(
                command.get("type").and_then(Value::as_str),
                Some("abort") | Some("switch_session") | Some("new_session")
            )
        }));

        // Switching back reuses A's own process instead of starting a third.
        app.switch_session(project_id, running_key.clone());
        assert_eq!(app.sessions.active_key(), Some(running_key.as_str()));
        assert_eq!(app.sessions.iter().count(), 2);
        assert_eq!(observer.starts().len(), 2);
    }

    #[test]
    fn the_queue_follows_the_session_it_belongs_to() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        start_test_session(&mut app, &directory);
        let project_id = app.state().selected_project.unwrap();
        let first_key = app.sessions.active_key().unwrap().to_owned();

        // Queue something in session A while it is visible.
        app.apply_agent_event(
            &first_key,
            json!({"type": "queue_update", "steering": ["keep going"], "followUp": ["then this"]}),
        );
        assert_eq!(app.state().pending_steering, vec!["keep going".to_owned()]);
        assert_eq!(app.state().pending_follow_up, vec!["then this".to_owned()]);

        // Switching away shows B's queue — empty — not A's leftovers.
        app.switch_session(project_id, "/sessions/b.jsonl".into());
        assert!(app.state().pending_steering.is_empty());
        assert!(app.state().pending_follow_up.is_empty());

        // A queue change while A is in the background is cached for it without
        // touching the visible session's state.
        app.apply_agent_event(
            &first_key,
            json!({"type": "queue_update", "steering": ["keep going", "and faster"], "followUp": []}),
        );
        assert!(app.state().pending_steering.is_empty());

        // Switching back restores A's queue as Pi last reported it.
        app.switch_session(project_id, first_key.clone());
        assert_eq!(
            app.state().pending_steering,
            vec!["keep going".to_owned(), "and faster".to_owned()]
        );
        assert!(app.state().pending_follow_up.is_empty());
    }

    #[test]
    fn a_missing_runtime_rejects_without_adding_a_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        let project = project("test", directory.path());
        let project_id = project.id;
        app.apply(Action::ProjectsLoaded(vec![project]));
        app.apply(Action::SelectProject(project_id));
        app.apply(Action::SetSessionStatus(SessionStatus::Ready));

        assert!(!app.can_submit_prompt());
        let effects = EffectProbe::new(&app);
        app.submit_prompt("keep this draft".into(), Vec::new(), SubmitMode::Prompt);

        assert!(app.state().conversation.is_empty());
        assert!(
            effects
                .notices()
                .iter()
                .any(|notice| notice.message.contains("No active session"))
        );
    }

    #[test]
    fn an_active_session_from_another_project_cannot_receive_the_prompt() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);

        let other = project("other", &directory.path().join("other"));
        let other_id = other.id;
        let mut projects = app.state().projects.clone();
        projects.push(other);
        app.apply(Action::ProjectsLoaded(projects));
        app.apply(Action::SelectProject(other_id));
        app.apply(Action::SetSessionStatus(SessionStatus::Ready));

        assert!(!app.can_submit_prompt());
        app.submit_prompt(
            "do not misroute this".into(),
            Vec::new(),
            SubmitMode::Prompt,
        );

        assert!(app.state().conversation.is_empty());
        assert!(!observer.commands().iter().any(|command| {
            command.get("message").and_then(Value::as_str) == Some("do not misroute this")
        }));
    }

    #[test]
    fn applying_unchanged_launch_settings_does_not_restart_pi() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let starts = observer.starts().len();

        app.handle(AppCommand::SetBashPolicy(app.state().bash_policy));
        app.handle(AppCommand::SetBlockedPatterns(
            app.state().bash_blocked_patterns.clone(),
        ));
        app.handle(AppCommand::SetAgentTeamConfig(
            app.state().agent_team_config.clone(),
        ));

        assert_eq!(observer.starts().len(), starts);
    }

    #[test]
    fn permission_switch_updates_every_live_supervisor_without_restarting_or_compacting() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let project_id = app.state().selected_project.unwrap();

        // A user turn makes the visible draft non-empty, so a second session is
        // launched and the first remains pooled in the background.
        app.apply(Action::UpsertConversation(ConversationItem {
            id: "user-1".into(),
            role: ConversationRole::User,
            full_text: "keep the first session".into(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }));
        app.start_new_session(project_id);
        assert_eq!(app.sessions.iter().count(), 2);
        let starts = observer.starts().len();

        app.handle(AppCommand::SetPermissionLevel(
            AgentPermissionLevel::ReadOnly,
        ));

        assert_eq!(observer.starts().len(), starts);
        assert_eq!(app.sessions.iter().count(), 2);
        assert_eq!(
            observer.permission_levels(),
            vec![
                AgentPermissionLevel::ReadOnly,
                AgentPermissionLevel::ReadOnly
            ]
        );
        assert_eq!(
            app.state().agent_team_config.default_policy.level,
            AgentPermissionLevel::ReadOnly
        );
        assert!(observer.commands().iter().all(|command| {
            !matches!(
                command.get("type").and_then(Value::as_str),
                Some("abort" | "compact" | "set_model")
            )
        }));
        assert_eq!(
            app.store
                .as_ref()
                .unwrap()
                .load_preferences()
                .unwrap()
                .agent_team_config
                .default_policy
                .level,
            AgentPermissionLevel::ReadOnly
        );
    }

    #[test]
    fn model_switch_refreshes_and_clamps_thinking_levels_from_rpc() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        runtime.set_response(
            "get_state",
            json!({
                "model": {"provider":"provider-key", "id":"model-a", "name":"Model A"},
                "thinkingLevel":"xhigh"
            }),
        );
        runtime.set_response(
            "get_available_models",
            json!({"models":[{"provider":"provider-key", "id":"model-a", "name":"Model A"}]}),
        );
        runtime.set_response(
            "get_available_thinking_levels",
            json!({"levels":["off", "low", "high"]}),
        );
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        // Starting a session kicks off a control refresh of its own; let it
        // finish so the baseline covers only what the model switch issues.
        app.settle_control_updates();
        let baseline = observer.commands().len();

        let key = app.sessions.active_key().unwrap().to_owned();
        app.set_model_on(
            &key,
            ModelOption {
                provider: "provider-key".into(),
                provider_name: "Configured provider".into(),
                id: "model-a".into(),
                name: "Model A".into(),
            },
        );
        app.settle_control_updates();

        assert_eq!(app.state().thinking_level, ThinkingLevel::Off);
        assert_eq!(
            app.state().available_thinking_levels,
            vec![ThinkingLevel::Off, ThinkingLevel::Low, ThinkingLevel::High]
        );
        let recorded_commands = observer.commands();
        let command_types = recorded_commands[baseline..]
            .iter()
            .filter_map(|command| command.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            command_types,
            vec![
                "set_model",
                "get_state",
                "get_available_models",
                "get_available_thinking_levels",
                "get_session_stats",
                "get_commands",
            ]
        );
    }

    #[test]
    fn model_switch_defers_set_model_until_prompt_compacts_first() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);

        app.queue_model_switch(ModelOption {
            provider: "provider-key".into(),
            provider_name: "Configured provider".into(),
            id: "model-b".into(),
            name: "Model B".into(),
        });

        // Switch is deferred: the pending model is recorded but no set_model
        // RPC is sent until the next prompt triggers compaction.
        assert_eq!(app.state().pending_model.as_ref().unwrap().id, "model-b");
        assert!(
            observer
                .commands()
                .iter()
                .all(|command| command.get("type").and_then(Value::as_str) != Some("set_model"))
        );
    }

    #[test]
    fn prompt_appends_attachment_paths_without_images_rpc_field() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        app.apply(Action::SetSessionStatus(SessionStatus::Ready));
        let attachment_path = directory.path().join("notes.txt");
        fs::write(&attachment_path, "notes").unwrap();
        let attachment = attachment_from_path(&attachment_path, false).unwrap();
        let expected_path = attachment.path.clone();

        app.submit_prompt(
            "Please inspect this.".into(),
            vec![attachment],
            SubmitMode::Prompt,
        );

        let prompt = observer
            .commands()
            .into_iter()
            .find(|command| command["type"] == "prompt")
            .unwrap();
        assert_eq!(
            prompt["message"],
            format!("Please inspect this.\n{expected_path}")
        );
        assert!(prompt.get("images").is_none());
        // A prompt reaching Pi mid-run queues instead of being rejected and
        // lost; idle Pi ignores the field.
        assert_eq!(prompt["streamingBehavior"], "followUp");
    }

    #[test]
    fn large_paste_threshold_matches_codex_attachment_behavior() {
        assert!(!is_large_paste("short\ntext"));
        assert!(is_large_paste(&"x".repeat(1_001)));
        assert!(is_large_paste(&"line\n".repeat(11)));
    }

    #[test]
    fn auto_compaction_setting_round_trips_through_pi_rpc() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        runtime.set_response("get_state", json!({"autoCompactionEnabled": false}));
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);

        app.set_auto_compaction(false);
        app.settle_control_updates();

        assert!(!app.state().auto_compaction_enabled);
        let command = observer
            .commands()
            .into_iter()
            .find(|command| {
                command.get("type").and_then(Value::as_str) == Some("set_auto_compaction")
            })
            .expect("set_auto_compaction command");
        assert_eq!(command.get("enabled").and_then(Value::as_bool), Some(false));
    }

    #[test]
    fn switching_sessions_refreshes_the_model_restored_by_pi() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        runtime.set_response(
            "get_state",
            json!({
                "model": {"provider":"provider-b", "id":"model-b", "name":"Model B"},
                "thinkingLevel":"off"
            }),
        );
        runtime.set_response(
            "get_available_models",
            json!({"models":[{"provider":"provider-b", "id":"model-b", "name":"Model B"}]}),
        );
        runtime.set_response("get_available_thinking_levels", json!({"levels":["off"]}));
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        let project = project("test", directory.path());
        let project_id = project.id;
        app.store.as_ref().unwrap().save_project(&project).unwrap();
        app.apply(Action::ProjectsLoaded(vec![project]));
        let session_path = "/sessions/agent-model-b.jsonl";

        app.switch_session(project_id, session_path.into());
        app.settle_control_updates();

        // The session opens in its own process; Pi restores its recorded
        // model there and the picker reflects it immediately.
        assert_eq!(
            observer.starts()[0].session_path.as_deref(),
            Some(session_path)
        );
        assert_eq!(
            app.state()
                .current_model
                .as_ref()
                .map(|model| (model.provider.clone(), model.id.clone())),
            Some(("provider-b".into(), "model-b".into()))
        );
        let commands = observer.commands();
        let command_types = commands
            .iter()
            .filter_map(|command| command.get("type").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            command_types,
            vec![
                "get_entries",
                "get_state",
                "get_available_models",
                "get_available_thinking_levels",
                "get_session_stats",
                "get_commands",
            ]
        );
        assert!(
            !observer
                .commands()
                .iter()
                .any(|command| command.get("type").and_then(Value::as_str) == Some("set_model"))
        );
    }

    #[test]
    fn a_fresh_prompt_is_shown_now_and_remembered_for_its_echo() {
        // The card goes up immediately — the reply starts now — and the wire
        // text is remembered so Pi's `message_start` does not place it twice.
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        app.apply(Action::SetSessionStatus(SessionStatus::Ready));

        app.submit_prompt("what changed?".into(), Vec::new(), SubmitMode::Prompt);

        assert_eq!(app.state().conversation.len(), 1);
        assert_eq!(app.state().conversation[0].full_text, "what changed?");
        let key = app
            .sessions
            .active_key()
            .expect("an active session")
            .to_owned();
        let session = app.sessions.get(&key).expect("the pooled session");
        assert_eq!(session.turn.optimistic_prompts.len(), 1);
        assert_eq!(session.turn.optimistic_prompts[0], "what changed?");
        assert!(observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("prompt")
                && command.get("message").and_then(Value::as_str) == Some("what changed?")
        }));
    }

    #[test]
    fn a_queued_message_waits_in_the_queue_block_not_the_transcript() {
        // A steered or follow-up message lands mid-transcript if placed now —
        // the turn streams on below it — so it stays out of the conversation
        // until Pi announces it consumed.
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        app.apply(Action::SetSessionStatus(SessionStatus::Streaming));

        for mode in [SubmitMode::Steer, SubmitMode::FollowUp] {
            app.submit_prompt("hold on".into(), Vec::new(), mode);
        }

        assert!(
            app.state().conversation.is_empty(),
            "a queued message is placed when it is consumed, not when it is sent"
        );
        let key = app
            .sessions
            .active_key()
            .expect("an active session")
            .to_owned();
        let session = app.sessions.get(&key).expect("the pooled session");
        assert!(session.turn.optimistic_prompts.is_empty());
        for expected in ["steer", "follow_up"] {
            assert!(
                observer.commands().iter().any(|command| {
                    command.get("type").and_then(Value::as_str) == Some(expected)
                })
            );
        }
    }

    #[test]
    fn clearing_the_queue_asks_pi_to_drop_it() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);

        app.handle(AppCommand::ClearQueue);

        assert!(
            observer.commands().iter().any(|command| {
                command.get("type").and_then(Value::as_str) == Some("clear_queue")
            })
        );
    }

    #[test]
    fn session_title_uses_the_first_prompt_immediately_and_only_once() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        app.apply(Action::SetSessionStatus(SessionStatus::Ready));
        let attachment = Attachment {
            path: "/private/secret-image.png".into(),
            name: "secret-image.png".into(),
            kind: pi_whim_core::AttachmentKind::File,
            generated_by_app: false,
        };

        app.submit_prompt(
            "  给这个会话命名  ".into(),
            vec![attachment],
            SubmitMode::Prompt,
        );
        app.submit_prompt("second prompt".into(), Vec::new(), SubmitMode::Prompt);

        let names = observer
            .commands()
            .into_iter()
            .filter(|command| {
                command.get("type").and_then(Value::as_str) == Some("set_session_name")
            })
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0]["name"], "给这个会话命名");
        assert!(!names[0].to_string().contains("secret-image"));
        let deferred = app.deferred_session_titles.values().next().unwrap();
        assert_eq!(app.deferred_session_titles.len(), 1);
        assert_eq!(deferred.content, "  给这个会话命名  ");
        assert!(!deferred.content.contains("secret-image"));
    }

    #[test]
    fn the_initial_title_event_does_not_cancel_its_ai_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        observer.set_response("get_state", json!({"sessionName": "First prompt"}));
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 4;
        app.expected_session_title_names
            .insert(token, VecDeque::from(["First prompt".into()]));
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 4,
                fallback: "First prompt".into(),
                baseline: "First prompt".into(),
                source: SessionTitleSource::Automatic,
            },
        );

        app.perform_effect(
            &key,
            events::Effect::RenameSessionFile(Some("First prompt".into())),
        );
        assert!(app.pending_session_titles.contains_key(&request_id));
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 4,
            task_kind: "session_title".into(),
            result: Ok("AI title".into()),
        }]);

        assert!(observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("set_session_name")
                && command.get("name").and_then(Value::as_str) == Some("AI title")
        }));
    }

    #[test]
    fn a_late_fallback_ack_cannot_replace_the_persisted_ai_title() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.jsonl");
        fs::write(
            &path,
            r#"{"type":"message","message":{"role":"user","content":"First prompt"}}"#,
        )
        .unwrap();
        let runtime = FakeRuntime::default();
        runtime.set_response(
            "get_state",
            json!({
                "sessionName": "AI title",
                "sessionFile": path.to_string_lossy(),
            }),
        );
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let project_id = app.sessions.get(&key).unwrap().project_id;
        let summary = session_summary_from_jsonl(project_id, &path).unwrap();
        let session_id = summary.id;
        app.store.as_ref().unwrap().save_session(&summary).unwrap();
        app.store
            .as_ref()
            .unwrap()
            .set_session_ai_title(session_id, "AI title")
            .unwrap();
        app.apply(Action::SessionsLoaded {
            project_id,
            sessions: app
                .store
                .as_ref()
                .unwrap()
                .list_sessions(project_id)
                .unwrap(),
        });
        app.expected_session_title_names.insert(
            token,
            VecDeque::from(["First prompt".into(), "AI title".into()]),
        );

        app.perform_effect(
            &key,
            events::Effect::RenameSessionFile(Some("First prompt".into())),
        );

        assert_eq!(
            app.store
                .as_ref()
                .unwrap()
                .list_sessions(project_id)
                .unwrap()[0]
                .title,
            "AI title"
        );
        assert_eq!(
            app.expected_session_title_names.get(&token),
            Some(&VecDeque::from(["AI title".into()]))
        );

        app.perform_effect(
            &key,
            events::Effect::RenameSessionFile(Some("AI title".into())),
        );
        app.perform_effect(
            &key,
            events::Effect::RenameSessionFile(Some("AI title".into())),
        );
        app.store
            .as_ref()
            .unwrap()
            .set_session_ai_title(session_id, "Replacement AI title")
            .unwrap();
        assert_eq!(
            app.store
                .as_ref()
                .unwrap()
                .list_sessions(project_id)
                .unwrap()[0]
                .title,
            "Replacement AI title",
            "replaying the current title must not promote it to a manual title"
        );
    }

    #[test]
    fn expected_title_acknowledgements_are_bounded_and_count_duplicates() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let token = app
            .sessions
            .active_key()
            .and_then(|key| app.sessions.token_for(key))
            .unwrap();

        for index in 0..10 {
            app.expect_session_title_name(token, &format!("title-{index}"));
        }
        app.expect_session_title_name(token, "same");
        app.expect_session_title_name(token, "same");

        let names = app.expected_session_title_names.get(&token).unwrap();
        assert_eq!(names.len(), MAX_EXPECTED_SESSION_TITLE_NAMES);
        assert_eq!(
            names.iter().filter(|name| name.as_str() == "same").count(),
            2
        );
        assert!(app.consume_expected_session_title_name(token, "same"));
        assert!(app.consume_expected_session_title_name(token, "same"));
        assert!(!app.consume_expected_session_title_name(token, "same"));
    }

    #[test]
    fn live_smart_rename_works_when_pi_state_has_no_session_name() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let project_id = app.sessions.get(&key).unwrap().project_id;
        app.apply(Action::SessionsLoaded {
            project_id,
            sessions: vec![SessionSummary {
                id: stable_session_id(&key),
                project_id,
                pi_path: key.clone(),
                title: "Initial".into(),
                preview: "Prompt".into(),
                updated_at_ms: now_ms(),
            }],
        });
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 6;
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 6,
                fallback: "Initial".into(),
                baseline: "Initial".into(),
                source: SessionTitleSource::ExplicitSmartRename,
            },
        );

        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 6,
            task_kind: "session_title".into(),
            result: Ok("跨会话发送消息".into()),
        }]);

        assert!(observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("set_session_name")
                && command.get("name").and_then(Value::as_str) == Some("跨会话发送消息")
        }));
    }

    #[test]
    fn live_smart_rename_uses_the_sidebar_title_when_pi_name_is_stale() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        observer.set_response("get_state", json!({"sessionName": "Stale Pi title"}));
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let project_id = app.sessions.get(&key).unwrap().project_id;
        app.apply(Action::SessionsLoaded {
            project_id,
            sessions: vec![SessionSummary {
                id: stable_session_id(&key),
                project_id,
                pi_path: key.clone(),
                title: "Sidebar title".into(),
                preview: "Prompt".into(),
                updated_at_ms: now_ms(),
            }],
        });
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 7;
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 7,
                fallback: "Sidebar title".into(),
                baseline: "Sidebar title".into(),
                source: SessionTitleSource::ExplicitSmartRename,
            },
        );

        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 7,
            task_kind: "session_title".into(),
            result: Ok("Corrected task title".into()),
        }]);

        assert!(observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("set_session_name")
                && command.get("name").and_then(Value::as_str) == Some("Corrected task title")
        }));
    }

    #[test]
    fn explicit_smart_rename_failure_reports_error_without_rewriting_title() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 7;
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 7,
                fallback: "Current title".into(),
                baseline: "Current title".into(),
                source: SessionTitleSource::ExplicitSmartRename,
            },
        );

        let effects = EffectProbe::new(&app);
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 7,
            task_kind: "session_title".into(),
            result: Err(OneShotErrorKind::TimedOut),
        }]);

        assert!(!observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("set_session_name")
        }));
        assert!(
            effects
                .notices()
                .iter()
                .any(|notice| notice.message.contains("timed out"))
        );
    }

    #[test]
    fn explicit_smart_rename_does_not_rewrite_an_unchanged_title() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 8;
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 8,
                fallback: "Current title".into(),
                baseline: "Current title".into(),
                source: SessionTitleSource::ExplicitSmartRename,
            },
        );

        let effects = EffectProbe::new(&app);
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 8,
            task_kind: "session_title".into(),
            result: Ok(" Current title ".into()),
        }]);

        assert!(!observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("set_session_name")
        }));
        assert!(
            effects
                .notices()
                .iter()
                .any(|notice| notice.message.contains("current title"))
        );
    }

    #[test]
    fn automatic_title_failure_keeps_its_silent_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        observer.set_response("get_state", json!({"sessionName": "Initial"}));
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 9;
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 9,
                fallback: "Fallback title".into(),
                baseline: "Initial".into(),
                source: SessionTitleSource::Automatic,
            },
        );

        let effects = EffectProbe::new(&app);
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 9,
            task_kind: "session_title".into(),
            result: Err(OneShotErrorKind::Network),
        }]);

        assert!(observer.commands().iter().any(|command| {
            command.get("type").and_then(Value::as_str) == Some("set_session_name")
                && command.get("name").and_then(Value::as_str) == Some("Fallback title")
        }));
        assert!(effects.notices().is_empty());
    }

    #[test]
    fn manual_naming_cancels_a_pending_background_title() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&key).unwrap();
        let request_id = Uuid::new_v4();
        app.one_shot_generation = 9;
        app.pending_session_titles.insert(
            request_id,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 9,
                fallback: "fallback".into(),
                baseline: "Initial".into(),
                source: SessionTitleSource::Automatic,
            },
        );
        app.deferred_session_titles.insert(
            token,
            DeferredSessionTitle {
                content: "private prompt".into(),
                fallback: "fallback".into(),
                baseline: "Initial".into(),
            },
        );

        app.rename_session(key, "Manual".into());
        assert!(!app.deferred_session_titles.contains_key(&token));
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id,
            generation: 9,
            task_kind: "session_title".into(),
            result: Ok("Late AI".into()),
        }]);

        let names = observer
            .commands()
            .into_iter()
            .filter_map(|command| {
                (command.get("type").and_then(Value::as_str) == Some("set_session_name"))
                    .then(|| command["name"].as_str().unwrap().to_owned())
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["Manual"]);
    }

    #[test]
    fn stored_session_smart_rename_updates_without_pi_and_manual_rename_wins() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        let mut app = test_application(&directory, runtime);
        let project = project("test", directory.path());
        let path = directory
            .path()
            .join("stored.jsonl")
            .to_string_lossy()
            .into_owned();
        fs::write(
            &path,
            r#"{"type":"message","message":{"role":"user","content":"Prompt"}}"#,
        )
        .unwrap();
        let summary = SessionSummary {
            id: stable_session_id(&path),
            project_id: project.id,
            pi_path: path.clone(),
            title: "Original".into(),
            preview: "Prompt".into(),
            updated_at_ms: 1,
        };
        let store = app.store.as_ref().unwrap();
        store.save_project(&project).unwrap();
        store.save_session(&summary).unwrap();
        app.apply(Action::ProjectsLoaded(vec![project.clone()]));
        app.apply(Action::SelectProject(project.id));
        app.apply(Action::SessionsLoaded {
            project_id: project.id,
            sessions: vec![summary],
        });
        app.one_shot_generation = 5;

        let successful = Uuid::new_v4();
        app.pending_session_titles.insert(
            successful,
            PendingSessionTitle {
                target: SessionTitleTarget::Stored {
                    project_id: project.id,
                    path: path.clone(),
                },
                generation: 5,
                fallback: "Original".into(),
                baseline: "Original".into(),
                source: SessionTitleSource::ExplicitSmartRename,
            },
        );
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id: successful,
            generation: 5,
            task_kind: "session_title".into(),
            result: Ok("AI title".into()),
        }]);
        assert_eq!(
            app.store
                .as_ref()
                .unwrap()
                .list_sessions(project.id)
                .unwrap()[0]
                .title,
            "AI title"
        );
        assert_eq!(
            session_summary_from_jsonl(project.id, Path::new(&path))
                .unwrap()
                .title,
            "AI title"
        );

        let late = Uuid::new_v4();
        app.pending_session_titles.insert(
            late,
            PendingSessionTitle {
                target: SessionTitleTarget::Stored {
                    project_id: project.id,
                    path: path.clone(),
                },
                generation: 5,
                fallback: "AI title".into(),
                baseline: "AI title".into(),
                source: SessionTitleSource::ExplicitSmartRename,
            },
        );
        app.rename_session(path.clone(), "Manual".into());
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id: late,
            generation: 5,
            task_kind: "session_title".into(),
            result: Ok("Late AI".into()),
        }]);

        assert_eq!(
            app.store
                .as_ref()
                .unwrap()
                .list_sessions(project.id)
                .unwrap()[0]
                .title,
            "Manual"
        );
        assert_eq!(
            session_summary_from_jsonl(project.id, Path::new(&path))
                .unwrap()
                .title,
            "Manual"
        );
        assert!(observer.commands().is_empty());
    }

    #[test]
    fn a_background_title_follows_rekey_but_not_session_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = FakeRuntime::default();
        let observer = runtime.clone();
        observer.set_response("get_state", json!({"sessionName": "Initial"}));
        let mut app = test_application(&directory, runtime);
        start_test_session(&mut app, &directory);
        let old_key = app.sessions.active_key().unwrap().to_owned();
        let token = app.sessions.token_for(&old_key).unwrap();
        app.one_shot_generation = 3;
        let first = Uuid::new_v4();
        app.pending_session_titles.insert(
            first,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 3,
                fallback: "fallback".into(),
                baseline: "Initial".into(),
                source: SessionTitleSource::Automatic,
            },
        );
        app.sessions.rekey(&old_key, "rekeyed.jsonl", now_ms());

        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id: first,
            generation: 3,
            task_kind: "session_title".into(),
            result: Ok("After rekey".into()),
        }]);
        let late = Uuid::new_v4();
        app.pending_session_titles.insert(
            late,
            PendingSessionTitle {
                target: SessionTitleTarget::Live(token),
                generation: 3,
                fallback: "fallback".into(),
                baseline: "Initial".into(),
                source: SessionTitleSource::Automatic,
            },
        );
        app.sessions.remove("rekeyed.jsonl");
        app.settle_one_shot_completions(vec![OneShotCompletion {
            request_id: late,
            generation: 3,
            task_kind: "session_title".into(),
            result: Ok("After delete".into()),
        }]);

        let names = observer
            .commands()
            .into_iter()
            .filter_map(|command| {
                (command.get("type").and_then(Value::as_str) == Some("set_session_name"))
                    .then(|| command["name"].as_str().unwrap().to_owned())
            })
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["After rekey"]);
    }
}
