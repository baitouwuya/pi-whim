//! Orchestration: the session pool, the store, the keychain, and Pi.
//!
//! Split from `main.rs` so the entry point is only the entry point. Nothing here
//! knows which UI is mounted — the view is reached through `state()` / `apply()`
//! and a queue of requests — which is what lets the host be swapped without
//! touching any of it.

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

use pi_whim_core::{
    Action, AgentPermissionLevel, AppState, Attachment, ConversationItem, ConversationRole,
    Language, ModelOption, Project, ProjectId, ProviderId, ProviderProfile, ProviderProtocol,
    QueueMode, SearchEngineId, SearchEngineProfile, SessionMetrics, SessionStatus, SessionSummary,
    SubmitMode, ThinkingLevel, normalize_bash_patterns, normalize_provider_display_name,
    provider_name_key, stable_session_id, strings,
};
use pi_whim_persistence::{
    AppPreferences, AttachmentStore, MacosKeychainStore, PreferencesRepository, ProjectRepository,
    ProviderRepository, SearchEngineRepository, SecretStore, SessionRepository, SqliteStore,
    session_summary_from_jsonl,
};
use pi_whim_runtime::{AgentRuntime, PiRpcRuntime, RuntimeEvent, RuntimeStart};
use serde_json::{Value, json};
use uuid::Uuid;

use pi_whim_catalog::ModelCapabilityResolver;
use pi_whim_engine::mailbox::Delivery;
use pi_whim_engine::pool::{SessionPool, SessionRuntime, is_draft};
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
use pi_whim_gpui::Request;

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
    store: Option<SqliteStore>,
    secrets: MacosKeychainStore,
    runtime_factory: Box<dyn Fn() -> R + Send>,
    sessions: SessionPool<R>,
    /// Extension confirmations and supervisor interactions, in the order they
    /// arrived. Each carries the session that asked, so a background agent can
    /// prompt the user and still get its answer back.
    ///
    /// Only a staging area: the shell holds the queue that decides which one is
    /// on screen, so these are handed over and none are kept.
    prompts: Vec<dialogs::Prompt>,
    /// Sessions that have stopped since the shell was last told.
    ///
    /// The questions belong to the shell's queue now, so forgetting them is
    /// something it has to be asked to do rather than something done here.
    closed: Vec<String>,
    /// Attachments chosen in a file dialog, waiting for the composer.
    ///
    /// Staged rather than answered, because the dialog is opened from a slash
    /// command whose request carries nothing back.
    attached: Vec<Attachment>,
    /// Text waiting to go on the clipboard.
    ///
    /// Written by the host, which is the only part of this that has a window.
    clipboard: Option<String>,
    /// A file picker to open, and what its answer is for.
    ///
    /// Staged for the same reason as the clipboard: the platform picker needs the
    /// window. `Option` rather than a queue — two pickers at once is two modal
    /// dialogs, and the second would have nothing to add.
    picker: Option<Picker>,
    capability_resolver: ModelCapabilityResolver,
    sessions_root_override: Option<PathBuf>,
    agent_directory_override: Option<PathBuf>,
    attachment_store: AttachmentStore,
    /// Messages bound for the user, oldest first.
    ///
    /// A queue rather than two `Option<String>` fields: orchestration fails in
    /// bursts — a project that has moved, then a provider with no key — and the
    /// second was overwriting the first before anyone had read it.
    notices: notice::Outbox,
    /// Control-state refreshes in flight, tagged with the session they were
    /// asked about.
    #[allow(clippy::type_complexity)]
    control_updates: (
        crossbeam_channel::Sender<(Option<String>, Vec<Action>)>,
        crossbeam_channel::Receiver<(Option<String>, Vec<Action>)>,
    ),
}

impl Default for PiWhimApplication<PiRpcRuntime> {
    fn default() -> Self {
        let mut engine = EngineState::new();
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
            let _ = engine.apply(Action::ProjectsLoaded(projects));
            for project_id in project_ids {
                if let Ok(sessions) = store.list_sessions(project_id) {
                    let _ = engine.apply(Action::SessionsLoaded {
                        project_id,
                        sessions,
                    });
                }
            }
        }
        if let Some(store) = store.as_ref()
            && let Ok(preferences) = store.load_preferences()
        {
            let _ = engine.apply(Action::SetLanguage(preferences.language));
            let _ = engine.apply(Action::SetBashPolicy(preferences.bash_policy));
            let _ = engine.apply(Action::SetBashBlockedPatterns(
                preferences.bash_blocked_patterns,
            ));
            let _ = engine.apply(Action::SetAgentTeamConfig(preferences.agent_team_config));
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
            let _ = engine.apply(Action::SearchEngineProfilesLoaded(profiles));
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
            let _ = engine.apply(Action::ProviderProfilesLoaded(profiles));
        }
        let application = Self {
            engine,
            store,
            secrets: MacosKeychainStore::default(),
            runtime_factory: Box::new(PiRpcRuntime::default),
            sessions: SessionPool::new(),
            prompts: Vec::new(),
            closed: Vec::new(),
            attached: Vec::new(),
            clipboard: None,
            picker: None,
            capability_resolver,
            sessions_root_override: None,
            agent_directory_override: None,
            attachment_store: AttachmentStore::open_default(),
            notices: notice::Outbox::new(),
            control_updates: crossbeam_channel::unbounded(),
        };
        // Probing the keychain can block for a long time and this runs before
        // the window opens, so profiles start with their stored status and a
        // worker corrects them.
        application.refresh_provider_key_status(provider_ids);
        application.refresh_search_engine_key_status(search_engine_ids);
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
        self.notices.error(message);
    }

    /// Change the domain state through the reducer.
    fn apply(&mut self, action: Action) {
        // The view effect is dropped: it exists for a caller that renders, and
        // the shell derives what it needs from the snapshot it is handed next.
        let _ = self.engine.apply(action);
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

    /// Messages bound for the user, taken off the queue.
    pub(crate) fn take_notices(&mut self) -> Vec<notice::Notice> {
        self.notices.drain()
    }

    /// Questions an agent is blocked on, handed to whoever will ask them.
    pub(crate) fn take_prompts(&mut self) -> Vec<dialogs::Prompt> {
        std::mem::take(&mut self.prompts)
    }

    /// Sessions that have stopped, so their questions can be dropped.
    pub(crate) fn take_closed_sessions(&mut self) -> Vec<String> {
        std::mem::take(&mut self.closed)
    }

    /// Text a command produced for the clipboard.
    pub(crate) fn take_clipboard(&mut self) -> Option<String> {
        self.clipboard.take()
    }

    /// A file picker the host should open on this side's behalf.
    pub(crate) fn take_picker(&mut self) -> Option<Picker> {
        self.picker.take()
    }

    /// Act on what a picker returned.
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
    pub(crate) fn handle(&mut self, request: Request) {
        match request {
            Request::AddProject => self.add_project(),
            Request::RemoveProject(project_id) => {
                self.stop_project_runtimes(project_id);
                if let Some(store) = self.store.as_ref()
                    && let Err(error) = store.delete_project(project_id)
                {
                    self.notices.error(error.to_string());
                }
                self.reload_projects();
            }
            Request::RevealProject(project_id) => {
                if let Some(project) = self.find_project(project_id) {
                    let _ = std::process::Command::new("open").arg(project.path).spawn();
                }
            }
            Request::OpenProject(project_id) => self.start_project(project_id),
            Request::NewSession(project_id) => self.start_new_session(project_id),
            Request::ActivateSession { project_id, path } => self.switch_session(project_id, path),
            Request::RenameSession { path, title } => self.rename_session(path, title),
            Request::CloneSession => self.clone_session(),
            Request::DeleteSession(path) => self.delete_session(path),
            Request::SubmitPrompt {
                content,
                attachments,
                mode,
            } => self.submit_prompt(content, attachments, mode),
            Request::AnswerPrompt(answer) => self.send_answer(answer),
            Request::DiscardAttachment(path) => {
                // Only the generated ones reach here — the composer decides that
                // — so this deletes the file it wrote without asking again.
                if let Err(error) = self.attachment_store.remove_generated(&path) {
                    self.notices.error(error);
                }
            }
            // Opened by the host, which has the window the platform picker needs.
            Request::PickAttachments => {
                debug_assert!(false, "the host opens the picker");
            }
            Request::SetAutoCompaction(enabled) => self.set_auto_compaction(enabled),
            Request::Stop => {
                if let Err(error) = self.active_command(json!({"type":"abort"})) {
                    self.notices.error(error);
                }
            }
            Request::SetLanguage(language) => {
                self.apply(Action::SetLanguage(language));
                self.save_preferences();
            }
            Request::SetBashPolicy(policy) => {
                if self.state().bash_policy != policy {
                    self.apply(Action::SetBashPolicy(policy));
                    self.save_preferences();
                    self.restart_selected_project();
                }
            }
            Request::SetBlockedPatterns(patterns) => {
                let patterns = normalize_bash_patterns(patterns);
                if self.state().bash_blocked_patterns != patterns {
                    self.apply(Action::SetBashBlockedPatterns(patterns));
                    self.save_preferences();
                    self.restart_selected_project();
                }
            }
            Request::SetPermissionLevel(level) => self.set_permission_level(level),
            Request::SetAgentTeamConfig(config) => {
                let config = config.normalized();
                if self.state().agent_team_config != config {
                    self.apply(Action::SetAgentTeamConfig(config));
                    self.save_preferences();
                    self.restart_selected_project();
                }
            }
            Request::SetModel(model) => self.queue_model_switch(model),
            Request::SetThinkingLevel(level) => self.set_thinking_level(level),
            Request::SetQueueModes {
                steering,
                follow_up,
            } => self.set_queue_modes(steering, follow_up),
            Request::RunCommand(command) => self.run_command(command),
            Request::DeleteProvider(profile_id) => self.delete_provider(profile_id),
            Request::SaveSearchEngines(profiles) => {
                self.save_search_engines(profiles);
            }
            Request::SaveSearchEngine { profile, api_key } => {
                self.save_search_engine(profile, api_key);
            }
            // Each of these either needs the window and the clipboard, or answers
            // with something a view has to be told, so the host keeps them.
            Request::CopyToClipboard(_)
            | Request::AttachPaste(_)
            | Request::SaveProvider { .. }
            | Request::TestSearchEngine { .. }
            | Request::DiscoverProviderModels { .. } => {
                unreachable!("handled by the host")
            }
        }
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
                    self.picker = Some(Picker::Attachments);
                } else {
                    self.report("notice-select-project-attachments");
                }
            }
            SlashCommand::SetModel(model) => self.queue_model_switch(model),
            SlashCommand::SetThinkingLevel(level) => self.set_thinking_level(level),
            SlashCommand::Compact => self.compact_session(),
            SlashCommand::Stop => {
                if let Err(error) = self.active_command(json!({"type":"abort"})) {
                    self.notices.error(error);
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
                    self.clipboard = Some(message.full_text.clone());
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

    /// Ask for a folder to register. The host opens the picker and answers with
    /// [`Self::add_project_at`].
    fn add_project(&mut self) {
        self.picker = Some(Picker::Project);
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
            self.notices.error(error.to_string());
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
                self.notices.error(error.to_string());
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
        };
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_preferences(preferences)
        {
            self.notices.error(error.to_string());
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
            }
            Err(error) => self.notices.error(error.to_string()),
        }
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
            Err(error) => self.notices.error(error.to_string()),
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
            self.notices.error(message);
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
                self.notices.error(error.to_string());
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
            self.notices.error(error.to_string());
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
            self.notices.error(error.to_string());
            return;
        }
        if let Err(error) = self.secrets.delete(&provider_keychain_account(profile_id)) {
            self.notices.error(error.to_string());
        }
        self.reload_provider_profiles();
        self.restart_selected_project();
    }

    fn save_search_engines(&mut self, profiles: Vec<SearchEngineProfile>) -> bool {
        if let Some(invalid) = profiles.iter().find(|profile| {
            profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url)
        }) {
            let message = format!("{}: {}", invalid.name, self.say("search-engine-incomplete"));
            self.notices.error(message);
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
            self.notices.error(error.to_string());
            return false;
        }
        for id in stale_key_ids {
            if let Err(error) = self.secrets.delete(&search_engine_keychain_account(id)) {
                self.notices.error(error.to_string());
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
                self.notices.error(error.to_string());
                return false;
            }
            match self.secrets.get(&account) {
                Ok(Some(value)) if !value.trim().is_empty() => profile.has_api_key = true,
                Ok(_) => {
                    self.report("notice-key-missing");
                    return false;
                }
                Err(error) => {
                    self.notices.error(error.to_string());
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
                self.notices.info(message);
            }
            Err(error) => {
                let message = format!(
                    "{} {}: {error}",
                    profile_name,
                    self.say("notice-test-failed")
                );
                self.notices.error(message);
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
                self.notices.error(error);
                None
            }
        }
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
                self.notices.error(error.to_string());
                return None;
            }
        };
        if let Err(error) = fs::create_dir_all(&sessions_path) {
            self.notices.error(error.to_string());
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
                self.notices.error(error);
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
                self.notices.error(error.to_string());
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
                    self.notices.error(error);
                    return None;
                }
            };
        let mut runtime = (self.runtime_factory)();
        if let Err(error) = runtime.start(RuntimeStart {
            project_path: project.path,
            sessions_path: sessions_path.to_string_lossy().into_owned(),
            session_path: session_path.map(str::to_owned),
            extension_paths,
            environment,
            agent_team_config: self.state().agent_team_config.clone(),
            search_engines: self.state().search_engine_profiles.clone(),
            search_engine_api_keys,
        }) {
            if self.sessions.active_key().is_none() {
                self.apply(Action::SetSessionStatus(SessionStatus::Failed(
                    error.to_string(),
                )));
            }
            self.notices.error(error.to_string());
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

    /// Bring a pooled session to the foreground: the conversation view binds to
    /// its process while every other session keeps running in the background.
    fn activate_session(&mut self, key: &str) {
        let Some(session) = self.sessions.activate(key, now_ms()) else {
            return;
        };
        let project_id = session.project_id;
        let running = session.is_running();
        self.apply(Action::SelectProject(project_id));
        if !is_draft(key) {
            self.apply(Action::SelectSession(stable_session_id(key)));
        }
        self.apply(Action::ClearConversation);
        self.apply(Action::SetSessionStatus(if running {
            SessionStatus::Streaming
        } else {
            SessionStatus::Ready
        }));
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
            self.closed.push(key.clone());
            self.apply(Action::SessionRunning {
                path: key,
                running: false,
            });
        }

        if was_visible {
            self.apply(Action::ClearConversation);
            self.apply(Action::SetSessionStatus(SessionStatus::Offline));
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
                self.notices.error(error.to_string());
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
            self.notices.error(error);
            return;
        }
        self.refresh_runtime_controls();
    }

    fn set_auto_compaction(&mut self, enabled: bool) {
        if let Err(error) =
            self.active_command(json!({"type":"set_auto_compaction", "enabled": enabled}))
        {
            self.notices.error(error);
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
                self.notices.error(error.to_string());
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
            self.notices.error(error);
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
            self.notices.error(error);
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
            Err(error) => self.notices.error(error),
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
                self.notices.error(error.to_string());
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
                    self.notices.error(error.to_string());
                }
            }
        }
        if let Ok(indexed_sessions) = store.list_sessions(project_id) {
            for session in indexed_sessions {
                if Path::new(&session.pi_path).starts_with(sessions_path)
                    && !valid_session_ids.contains(&session.id)
                    && let Err(error) = store.delete_session(session.id)
                {
                    self.notices.error(error.to_string());
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
        // A live session renames through its own process so the name lands in
        // the session file; renaming a background session never disturbs it.
        // Without a live process the rename is store-only and may be
        // overwritten by a later disk rescan.
        if let Some(session) = self.sessions.get(&path)
            && let Err(error) = session
                .runtime
                .command(json!({"type":"set_session_name", "name": title}))
        {
            self.notices.error(error.to_string());
            return;
        }
        if let Some(project_id) = self.state().selected_project {
            self.index_session(project_id, &path, Some(&title));
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
                    self.notices.error(message);
                }
            }
            Err(error) => self.notices.error(error),
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
                self.notices.error(message);
            }
            Ok(output) => {
                self.notices
                    .error(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(error) => {
                let message = format!("{} {error}", self.say("notice-gh-unavailable"));
                self.notices.error(message);
            }
        }
    }

    fn clone_session(&mut self) {
        if let Err(error) = self.active_command(json!({"type":"clone"})) {
            self.notices.error(error);
            return;
        }
        if let Some(project_id) = self.state().selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn fork_session(&mut self, entry_id: String) {
        if let Err(error) = self.active_command(json!({"type":"fork", "entryId": entry_id})) {
            self.notices.error(error);
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
        if let Some(mut session) = self.sessions.remove(&path) {
            let _ = session.runtime.stop();
            self.apply(Action::SessionRunning {
                path: path.clone(),
                running: false,
            });
            if was_visible {
                self.apply(Action::ClearConversation);
                self.apply(Action::SetSessionStatus(SessionStatus::Offline));
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
            self.notices.error(error.to_string());
        }
        if let Some(project_id) = self.state().selected_project {
            self.discover_sessions(project_id, target.parent().unwrap_or(Path::new("")));
        }
    }

    fn load_current_entries(&mut self) -> Result<(), ()> {
        let entries = self
            .active_command(json!({"type":"get_entries"}))
            .map_err(|error| {
                self.notices.error(error);
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
                Ok(attachment) => self.attached.push(attachment),
                Err(error) => self.notices.error(error),
            }
        }
    }

    /// Turn a paste into attachments the composer can hold.
    ///
    /// Answered rather than staged, because a paste is the reader waiting on one
    /// action and the composer is where it lands. Files come as a list, so this
    /// answers with however many the clipboard held.
    pub(crate) fn attachments_for(&mut self, paste: pi_whim_gpui::chat::Paste) -> Vec<Attachment> {
        let created = match paste {
            pi_whim_gpui::chat::Paste::Files(paths) => {
                let mut attachments = Vec::new();
                for path in paths {
                    match attachment_from_path(Path::new(&path), false) {
                        Ok(attachment) => attachments.push(attachment),
                        // One unreadable path does not spoil the rest of a
                        // multi-file paste.
                        Err(error) => self.notices.error(error),
                    }
                }
                return attachments;
            }
            pi_whim_gpui::chat::Paste::Image { extension, bytes } => self
                .attachment_store
                .create_pasted_encoded_image(&extension, &bytes),
            pi_whim_gpui::chat::Paste::LongText(text) => {
                self.attachment_store.create_pasted_text(&text)
            }
            // The composer resolves this one itself by inserting the text, so it
            // is never reported as a request.
            pi_whim_gpui::chat::Paste::Insert => {
                unreachable!("the composer inserts rather than attaching")
            }
        };
        match created {
            Ok(attachment) => vec![attachment],
            Err(error) => {
                self.notices.error(error);
                Vec::new()
            }
        }
    }

    /// Attachments a file dialog produced, bound for the composer.
    pub(crate) fn take_attachments(&mut self) -> Vec<Attachment> {
        std::mem::take(&mut self.attached)
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
                Err(error) => self.notices.error(error),
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
            self.notices.error(error.to_string());
            return;
        }
        let is_active = self.sessions.active_key() == Some(key);
        if let Some(session) = self.sessions.get_mut(key) {
            session.turn.running = true;
        }
        self.apply(Action::SessionRunning {
            path: key.to_owned(),
            running: true,
        });
        if is_active {
            self.apply(Action::SetSessionStatus(SessionStatus::Streaming));
            self.ensure_session_title();
        }
    }

    fn ensure_session_title(&mut self) {
        let Some(project_id) = self.state().selected_project else {
            return;
        };
        let Ok(state) = self.active_command(json!({"type":"get_state"})) else {
            return;
        };
        if state.get("sessionName").and_then(Value::as_str).is_some() {
            return;
        }
        let Some(user_message) = self
            .state()
            .conversation
            .iter()
            .find(|message| message.role == ConversationRole::User)
            .map(|message| message.full_text.clone())
        else {
            return;
        };
        let title: String = user_message.chars().take(52).collect();
        if !title.trim().is_empty()
            && self
                .active_command(json!({"type":"set_session_name", "name": title}))
                .is_ok()
            && let Some(path) = state.get("sessionFile").and_then(Value::as_str)
        {
            self.index_session(project_id, path, Some(&title));
        }
    }

    /// Take one batch off the pump. Events from the visible session update the
    /// conversation view; background sessions only update bookkeeping (busy
    /// dots, sidebar titles) so their progress survives until they are shown.
    pub(crate) fn handle_deliveries(&mut self, batch: Vec<Delivery>) {
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
                self.prompts
                    .extend(dialogs::Prompt::from_extension(key, &value));
            }
            RuntimeEvent::Interaction(value) => {
                self.prompts
                    .extend(dialogs::Prompt::from_interaction(key, &value));
            }
            RuntimeEvent::Stderr(message) => {
                if is_active && !message.trim().is_empty() {
                    self.notices.error(message);
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
                self.sessions.remove(key);
                // Nothing is left to answer, and a dialog still up would ask the
                // reader to unblock a process that has gone.
                self.closed.push(key.to_owned());
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
                    self.notices.error(error);
                }
            }
            RuntimeEvent::RpcResponse(_) => {}
        }
    }

    /// Translate one agent event and carry out what the translation asked for.
    ///
    /// The reading of the event lives in `engine::events`; what stays here is
    /// only what needs this process's store, window, and Pi connection.
    fn apply_agent_event(&mut self, key: &str, event: Value) {
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
                if let Some((project_id, path, _)) = self.reported_session_file(key) {
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

    /// Carry a decision back over whichever channel asked.
    fn send_answer(&mut self, answer: dialogs::Answer) {
        let Some(session) = self.sessions.get(answer.session_key()) else {
            self.report("notice-asking-session-gone");
            return;
        };
        let result = match &answer {
            dialogs::Answer::Extension {
                request_id,
                confirmed,
                ..
            } => session.runtime.respond_extension_ui(
                json!({"type":"extension_ui_response", "id": request_id, "confirmed": confirmed}),
            ),
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
            self.notices.error(error.to_string());
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

/// Pi accepts an environment reference here, keeping API keys out of models.json.
fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::ConversationItem;
    use pi_whim_engine::session::is_large_paste;
    use pi_whim_runtime::FakeRuntime;
    use serde_json::json;
    use tempfile::TempDir;

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

    fn test_application(
        directory: &TempDir,
        runtime: FakeRuntime,
    ) -> PiWhimApplication<FakeRuntime> {
        // Pooled runtimes are clones of the prototype so the observer sees
        // every start and command across all session processes.
        let factory_runtime = runtime;
        PiWhimApplication {
            engine: EngineState::new(),
            store: Some(SqliteStore::open(directory.path().join("test.sqlite")).unwrap()),
            secrets: MacosKeychainStore::default(),
            runtime_factory: Box::new(move || factory_runtime.clone()),
            sessions: SessionPool::new(),
            prompts: Vec::new(),
            closed: Vec::new(),
            attached: Vec::new(),
            clipboard: None,
            picker: None,
            capability_resolver: ModelCapabilityResolver::new(
                &pi_whim_catalog::SharedCatalog::default(),
                false,
            ),
            sessions_root_override: Some(directory.path().join("sessions")),
            agent_directory_override: Some(directory.path().join("agent")),
            attachment_store: AttachmentStore::open(directory.path().join("attachments")).unwrap(),
            notices: notice::Outbox::new(),
            control_updates: crossbeam_channel::unbounded(),
        }
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
    fn a_missing_runtime_rejects_without_adding_a_user_message() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = test_application(&directory, FakeRuntime::default());
        let project = project("test", directory.path());
        let project_id = project.id;
        app.apply(Action::ProjectsLoaded(vec![project]));
        app.apply(Action::SelectProject(project_id));
        app.apply(Action::SetSessionStatus(SessionStatus::Ready));

        assert!(!app.can_submit_prompt());
        app.submit_prompt("keep this draft".into(), Vec::new(), SubmitMode::Prompt);

        assert!(app.state().conversation.is_empty());
        assert!(
            app.take_notices()
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

        app.handle(Request::SetBashPolicy(app.state().bash_policy));
        app.handle(Request::SetBlockedPatterns(
            app.state().bash_blocked_patterns.clone(),
        ));
        app.handle(Request::SetAgentTeamConfig(
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

        app.handle(Request::SetPermissionLevel(AgentPermissionLevel::ReadOnly));

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
}
