mod macos_paste;

use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use eframe::egui;
use pi_whim_core::{
    Action, Attachment, ConversationItem, ConversationRole, ModelOption, Project, ProjectId,
    ProviderId, ProviderProfile, ProviderProtocol, QueueMode, SearchEngineProfile, SessionStatus,
    SessionSummary, SubmitMode, ThinkingLevel, normalize_provider_display_name, provider_name_key,
    stable_session_id,
};
use pi_whim_persistence::{
    AppPreferences, AttachmentStore, MacosKeychainStore, PreferencesRepository, ProjectRepository,
    ProviderRepository, SearchEngineRepository, SecretStore, SessionRepository, SqliteStore,
    session_summary_from_jsonl,
};
use pi_whim_runtime::{AgentRuntime, PiRpcRuntime, RuntimeEvent, RuntimeStart};
use pi_whim_ui::{UiIntent, Workbench, install_fonts};
use serde_json::{Value, json};
use uuid::Uuid;

use macos_paste::{ClipboardAttachment, FinderPasteMonitor};
use pi_whim_catalog::ModelCapabilityResolver;
use pi_whim_engine::pool::{SessionPool, SessionRuntime, is_draft};
use pi_whim_engine::protocol::queue_mode_name;
use pi_whim_engine::providers::{
    discover_models, normalize_base_url, provider_keychain_account, test_searxng_engine,
    valid_search_engine_url,
};
use pi_whim_engine::session::{
    attachment_from_path, bash_policy_name, canonical_path, ensure_agent_team_extension,
    is_large_paste, now_ms, prompt_with_attachment_paths,
};
use pi_whim_engine::{controls, dialogs, events, launch, notice};

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 620.0])
            .with_title("Pi-Whim"),
        hardware_acceleration: eframe::HardwareAcceleration::Required,
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };
    eframe::run_native(
        "Pi-Whim",
        native_options,
        Box::new(|creation_context| {
            install_fonts(&creation_context.egui_ctx);
            Ok(Box::<PiWhimApplication>::default())
        }),
    )
}

struct PiWhimApplication<R: AgentRuntime = PiRpcRuntime> {
    workbench: Workbench,
    store: Option<SqliteStore>,
    secrets: MacosKeychainStore,
    runtime_factory: Box<dyn Fn() -> R + Send>,
    sessions: SessionPool<R>,
    /// Extension confirmations and supervisor interactions, in the order they
    /// arrived. Each carries the session that asked, so a background agent can
    /// prompt the user and still get its answer back.
    prompts: dialogs::Queue,
    capability_resolver: ModelCapabilityResolver,
    sessions_root_override: Option<PathBuf>,
    agent_directory_override: Option<PathBuf>,
    attachment_store: AttachmentStore,
    finder_paste_monitor: Option<FinderPasteMonitor>,
    finder_paste_monitor_install_pending: bool,
    /// Messages bound for the user, oldest first.
    ///
    /// A queue rather than two `Option<String>` fields: orchestration fails in
    /// bursts — a project that has moved, then a provider with no key — and the
    /// second was overwriting the first before anyone had read it.
    notices: notice::Outbox,
    /// The message currently on screen, taken off the queue.
    showing: Option<notice::Notice>,
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
        let mut workbench = Workbench::default();
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
            workbench.apply(Action::ProjectsLoaded(projects));
            for project_id in project_ids {
                if let Ok(sessions) = store.list_sessions(project_id) {
                    workbench.apply(Action::SessionsLoaded {
                        project_id,
                        sessions,
                    });
                }
            }
        }
        if let Some(store) = store.as_ref()
            && let Ok(preferences) = store.load_preferences()
        {
            workbench.apply(Action::SetLanguage(preferences.language));
            workbench.apply(Action::SetBashPolicy(preferences.bash_policy));
            workbench.apply(Action::SetBashBlockedPatterns(
                preferences.bash_blocked_patterns,
            ));
            workbench.apply(Action::SetAgentTeamConfig(preferences.agent_team_config));
        }
        if let Some(store) = store.as_ref()
            && let Ok(profiles) = store.list_search_engine_profiles()
        {
            workbench.apply(Action::SearchEngineProfilesLoaded(profiles));
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
            workbench.apply(Action::ProviderProfilesLoaded(profiles));
        }
        let application = Self {
            workbench,
            store,
            secrets: MacosKeychainStore::default(),
            runtime_factory: Box::new(PiRpcRuntime::default),
            sessions: SessionPool::new(),
            prompts: dialogs::Queue::new(),
            capability_resolver,
            sessions_root_override: None,
            agent_directory_override: None,
            attachment_store: AttachmentStore::open_default(),
            finder_paste_monitor: None,
            finder_paste_monitor_install_pending: true,
            notices: notice::Outbox::new(),
            showing: None,
            control_updates: crossbeam_channel::unbounded(),
        };
        // Probing the keychain can block for a long time and this runs before
        // the first frame, so profiles render with their stored status and a
        // worker corrects them.
        application.refresh_provider_key_status(provider_ids);
        application
    }
}

impl<R: AgentRuntime> eframe::App for PiWhimApplication<R> {
    fn raw_input_hook(&mut self, context: &egui::Context, raw_input: &mut egui::RawInput) {
        let composer_focused = self.workbench.composer_has_focus(context);
        if !composer_focused {
            return;
        }
        raw_input.events.retain(|event| match event {
            egui::Event::Paste(text) => !self.capture_attachment_paste(text),
            _ => true,
        });
    }

    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        if self.finder_paste_monitor_install_pending {
            self.finder_paste_monitor = FinderPasteMonitor::install();
            self.finder_paste_monitor_install_pending = false;
        }
        self.consume_finder_paste(context);
        self.consume_runtime_events();
        self.consume_control_updates();
        self.consume_capability_catalog();
        self.workbench.show(context);
        for intent in self.workbench.take_intents() {
            self.handle_intent(intent);
        }
        self.prompt_dialog(context);
        self.notice_window(context);
        let session_running = self.sessions.any_running();
        let agent_busy = matches!(
            self.workbench.state().session_status,
            SessionStatus::Starting | SessionStatus::Streaming | SessionStatus::Compacting
        );
        if session_running || agent_busy {
            // Runtime events arrive on channels and need a periodic poll while
            // a session is active.  Fifty milliseconds keeps streaming smooth
            // without laying out the whole conversation at display refresh rate.
            context.request_repaint_after(Duration::from_millis(50));
        }
    }
}

impl<R: AgentRuntime> PiWhimApplication<R> {
    fn consume_finder_paste(&mut self, context: &egui::Context) {
        let attachments = self
            .finder_paste_monitor
            .as_ref()
            .map(FinderPasteMonitor::drain_attachments)
            .unwrap_or_default();
        for attachment in attachments {
            if self.workbench.composer_has_focus(context) {
                self.add_clipboard_attachment(attachment);
            }
        }
    }

    fn consume_capability_catalog(&mut self) {
        if !self.capability_resolver.online_refresh_completed() {
            return;
        }
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

    fn handle_intent(&mut self, intent: UiIntent) {
        match intent {
            UiIntent::AddProject => self.add_project(),
            UiIntent::RemoveProject(project_id) => {
                self.stop_project_runtimes(project_id);
                if let Some(store) = self.store.as_ref()
                    && let Err(error) = store.delete_project(project_id)
                {
                    self.notices.error(error.to_string());
                }
                self.reload_projects();
            }
            UiIntent::RevealProject(project_id) => {
                if let Some(project) = self.find_project(project_id) {
                    let _ = std::process::Command::new("open").arg(project.path).spawn();
                }
            }
            UiIntent::StartProject(project_id) => self.start_project(project_id),
            UiIntent::StartNewSession(project_id) => self.start_new_session(project_id),
            UiIntent::SwitchSession { project_id, path } => self.switch_session(project_id, path),
            UiIntent::RenameSession { path, title } => self.rename_session(path, title),
            UiIntent::CloneSession => self.clone_session(),
            UiIntent::ForkSession(entry_id) => self.fork_session(entry_id),
            UiIntent::DeleteSession(path) => self.delete_session(path),
            UiIntent::SetSessionName(name) => self.set_current_session_name(name),
            UiIntent::ExportSession(path) => self.export_session(path),
            UiIntent::ShareSession => self.share_session(),
            UiIntent::AddFileAttachments => {
                if self.workbench.state().selected_project.is_some() {
                    self.add_file_attachments();
                } else {
                    self.notices
                        .error("Select a project before adding attachments.");
                }
            }
            UiIntent::AddFolderAttachment => {
                if self.workbench.state().selected_project.is_some() {
                    self.add_folder_attachment();
                } else {
                    self.notices
                        .error("Select a project before adding attachments.");
                }
            }
            UiIntent::RemoveComposerAttachment(path) => self.remove_composer_attachment(&path),
            UiIntent::SubmitPrompt {
                content,
                attachments,
                mode,
            } => self.submit_prompt(content, attachments, mode),
            UiIntent::Compact => self.compact_session(),
            UiIntent::SetAutoCompaction(enabled) => self.set_auto_compaction(enabled),
            UiIntent::Stop => {
                if let Err(error) = self.active_command(json!({"type":"abort"})) {
                    self.notices.error(error);
                }
            }
            UiIntent::SetLanguage(language) => {
                self.workbench.apply(Action::SetLanguage(language));
                self.save_preferences();
            }
            UiIntent::SetBashPolicy(policy) => {
                self.workbench.apply(Action::SetBashPolicy(policy));
                self.save_preferences();
                self.restart_selected_project();
            }
            UiIntent::SetBashBlockedPatterns(patterns) => {
                self.workbench
                    .apply(Action::SetBashBlockedPatterns(patterns));
                self.save_preferences();
                self.restart_selected_project();
            }
            UiIntent::SetAgentTeamConfig(config) => {
                self.workbench.apply(Action::SetAgentTeamConfig(config));
                self.save_preferences();
                self.restart_selected_project();
            }
            UiIntent::SetModel(model) => self.queue_model_switch(model),
            UiIntent::SetThinkingLevel(level) => self.set_thinking_level(level),
            UiIntent::SetQueueModes {
                steering,
                follow_up,
            } => self.set_queue_modes(steering, follow_up),
            UiIntent::SaveProvider { profile, api_key } => self.save_provider(profile, api_key),
            UiIntent::DeleteProvider(profile_id) => self.delete_provider(profile_id),
            UiIntent::SaveSearchEngines(profiles) => self.save_search_engines(profiles),
            UiIntent::TestSearchEngine(profile) => self.test_search_engine(profile),
            UiIntent::DiscoverProviderModels {
                profile_id,
                provider_name,
                base_url,
                protocol,
                api_key,
            } => self.discover_provider_models(
                profile_id,
                provider_name,
                base_url,
                protocol,
                api_key,
            ),
        }
    }

    fn add_project(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Add Pi-Whim project")
            .pick_folder()
        else {
            return;
        };
        let path = canonical_path(&path);
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
        if let Some(store) = self.store.as_ref() {
            match store.list_projects() {
                Ok(projects) => {
                    let project_ids = projects
                        .iter()
                        .map(|project| project.id)
                        .collect::<Vec<_>>();
                    self.workbench.apply(Action::ProjectsLoaded(projects));
                    for project_id in project_ids {
                        if let Ok(sessions) = store.list_sessions(project_id) {
                            self.workbench.apply(Action::SessionsLoaded {
                                project_id,
                                sessions,
                            });
                        }
                    }
                }
                Err(error) => self.notices.error(error.to_string()),
            }
        }
    }

    fn save_preferences(&mut self) {
        let preferences = AppPreferences {
            language: self.workbench.state().language,
            bash_policy: self.workbench.state().bash_policy,
            bash_blocked_patterns: self.workbench.state().bash_blocked_patterns.clone(),
            agent_team_config: self.workbench.state().agent_team_config.clone(),
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
                self.workbench
                    .apply(Action::ProviderProfilesLoaded(profiles));
                self.refresh_provider_key_status(ids);
            }
            Err(error) => self.notices.error(error.to_string()),
        }
    }

    fn save_provider(&mut self, mut profile: ProviderProfile, api_key: Option<String>) {
        profile.name = normalize_provider_display_name(&profile.name);
        if profile.name.trim().is_empty()
            || profile.base_url.trim().is_empty()
            || profile.models.is_empty()
        {
            self.notices
                .error("A provider needs a name, base URL, and at least one model.");
            return;
        }
        if self
            .workbench
            .state()
            .provider_profiles
            .iter()
            .any(|existing| {
                existing.id != profile.id
                    && provider_name_key(&existing.name) == provider_name_key(&profile.name)
            })
        {
            self.notices.error(format!(
                "A provider named '{}' already exists.",
                profile.name
            ));
            return;
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
                return;
            }
            profile.has_api_key = self
                .secrets
                .get(&provider_keychain_account(profile.id))
                .map_err(|error| error.to_string())
                .ok()
                .flatten()
                .is_some();
            if !profile.has_api_key {
                self.workbench.set_provider_key_status(profile.id, false);
                self.notices.error(
                    "The API key could not be read back from Keychain. Pi was not restarted; try Save and apply again.",
                );
                return;
            }
        } else if self
            .secrets
            .get(&provider_keychain_account(profile.id))
            .ok()
            .flatten()
            .is_none()
        {
            self.notices.error(
                "This provider has no API key in Keychain. Enter and save its API key before starting Pi.",
            );
            return;
        }
        self.workbench.set_provider_key_status(profile.id, true);
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_provider_profile(&profile)
        {
            self.notices.error(error.to_string());
            return;
        }
        self.reload_provider_profiles();
        // Pi reads models.json at startup. Restart the active project to apply new providers.
        self.restart_selected_project();
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

    fn save_search_engines(&mut self, profiles: Vec<SearchEngineProfile>) {
        if let Some(invalid) = profiles.iter().find(|profile| {
            profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url)
        }) {
            self.notices.error(format!(
                "Search engine '{}' needs a name and a valid HTTP or HTTPS base URL.",
                invalid.name
            ));
            return;
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
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_search_engine_profiles(&profiles)
        {
            self.notices.error(error.to_string());
            return;
        }
        self.workbench
            .apply(Action::SearchEngineProfilesLoaded(profiles));
        self.restart_selected_project();
    }

    fn test_search_engine(&mut self, profile: SearchEngineProfile) {
        if profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url) {
            self.notices
                .error("Enter a name and valid HTTP or HTTPS base URL before testing.");
            return;
        }
        match test_searxng_engine(&profile) {
            Ok(()) => self.notices.info(format!(
                "{} is reachable and returned valid SearXNG JSON.",
                profile.name
            )),
            Err(error) => self
                .notices
                .error(format!("{} test failed: {error}", profile.name)),
        }
    }

    fn discover_provider_models(
        &mut self,
        profile_id: Option<ProviderId>,
        provider_name: String,
        base_url: String,
        protocol: ProviderProtocol,
        supplied_key: Option<String>,
    ) {
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
                self.workbench.set_discovered_models(models);
            }
            Ok(_) => self
                .notices
                .error("The provider returned no models; add a model ID manually."),
            Err(error) => self.notices.error(error),
        }
    }

    fn find_project(&self, id: ProjectId) -> Option<Project> {
        self.workbench
            .state()
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
            .workbench
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
            self.workbench
                .apply(Action::SetSessionStatus(SessionStatus::Starting));
        }
        let mut extension_paths = Vec::new();
        match ensure_agent_team_extension(&sessions_path) {
            Ok(path) => extension_paths.push(path.to_string_lossy().into_owned()),
            Err(error) => {
                if self.sessions.active_key().is_none() {
                    self.workbench
                        .apply(Action::SetSessionStatus(SessionStatus::Failed(
                            error.to_string(),
                        )));
                }
                self.notices.error(error.to_string());
                return None;
            }
        }
        environment.insert(
            "PI_WHIM_BASH_POLICY".into(),
            bash_policy_name(&self.workbench.state().bash_policy).into(),
        );
        environment.insert(
            "PI_WHIM_BASH_BLOCKED_PATTERNS".into(),
            serde_json::to_string(&self.workbench.state().bash_blocked_patterns)
                .unwrap_or_else(|_| "[]".into()),
        );
        let mut runtime = (self.runtime_factory)();
        if let Err(error) = runtime.start(RuntimeStart {
            project_path: project.path,
            sessions_path: sessions_path.to_string_lossy().into_owned(),
            session_path: session_path.map(str::to_owned),
            extension_paths,
            environment,
            agent_team_config: self.workbench.state().agent_team_config.clone(),
            search_engines: self.workbench.state().search_engine_profiles.clone(),
        }) {
            if self.sessions.active_key().is_none() {
                self.workbench
                    .apply(Action::SetSessionStatus(SessionStatus::Failed(
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
        self.workbench.apply(Action::SelectProject(project_id));
        if !is_draft(key) {
            self.workbench
                .apply(Action::SelectSession(stable_session_id(key)));
        }
        self.workbench.apply(Action::ClearConversation);
        self.workbench.apply(Action::SetSessionStatus(if running {
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
            self.workbench
                .apply(Action::SelectSession(stable_session_id(to)));
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
            self.prompts.forget_session(&key);
            self.workbench.apply(Action::SessionRunning {
                path: key,
                running: false,
            });
        }

        if was_visible {
            self.workbench.apply(Action::ClearConversation);
            self.workbench
                .apply(Action::SetSessionStatus(SessionStatus::Offline));
        }
    }

    /// Ask the agent for its control state on a worker thread.
    ///
    /// Five RPCs at up to 20 seconds each; issuing them inline froze the window
    /// whenever Pi was slow to answer. Results arrive as actions and are applied
    /// by `consume_control_updates`.
    fn refresh_runtime_controls(&mut self) {
        let Some(commander) = self
            .active()
            .and_then(|session| session.runtime.commander())
        else {
            return;
        };
        let providers = controls::provider_names(&self.workbench.state().provider_profiles);
        let sender = self.control_updates.0.clone();
        let key = self.sessions.active_key().map(str::to_owned);
        std::thread::spawn(move || {
            let actions = controls::fetch(&commander, &providers);
            let _ = sender.send((key, actions));
        });
    }

    /// Block until an in-flight control refresh lands, for tests.
    ///
    /// Production code applies these from the frame loop; a test has no frame
    /// loop, so it waits for the worker instead of racing it.
    #[cfg(test)]
    fn settle_control_updates(&mut self) {
        while let Ok((key, actions)) = self.control_updates.1.recv_timeout(Duration::from_secs(5)) {
            if key.as_deref() == self.sessions.active_key() {
                for action in actions {
                    self.workbench.apply(action);
                }
            }
            if self.control_updates.1.is_empty() {
                break;
            }
        }
    }

    /// Apply whatever the control refresh reported.
    ///
    /// Updates for a session that is no longer visible are dropped: the user
    /// has moved on, and applying them would overwrite the current session's
    /// controls with another's.
    fn consume_control_updates(&mut self) {
        let updates: Vec<_> = self.control_updates.1.try_iter().collect();
        for (key, actions) in updates {
            if key.as_deref() != self.sessions.active_key() {
                continue;
            }
            for action in actions {
                self.workbench.apply(action);
            }
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
                self.notices.error("The session is no longer running.");
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
        self.workbench.apply(Action::SetPendingModel(Some(model)));
    }

    /// Apply a deferred model switch: send set_model and refresh controls.
    fn apply_pending_model(&mut self, key: &str) {
        if let Some(model) = self.workbench.state().pending_model.clone() {
            self.set_model_on(key, model);
            self.workbench.apply(Action::SetPendingModel(None));
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

    fn compact_session(&mut self) {
        if !matches!(self.workbench.state().session_status, SessionStatus::Ready) {
            return;
        }
        if let Err(error) = self.active_send(json!({"type":"compact"})) {
            self.notices.error(error);
            return;
        }
        if let Some(session) = self.active_mut() {
            session.turn.running = true;
        }
        self.workbench
            .apply(Action::SetSessionStatus(SessionStatus::Compacting));
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
                    self.workbench.apply(Action::SessionsLoaded {
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
                self.workbench.apply(Action::SessionsLoaded {
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
            self.workbench.apply(Action::SessionsLoaded {
                project_id,
                sessions,
            });
        }
    }

    /// Start a fresh session in its own Pi process. The currently visible
    /// session keeps running untouched in the background.
    fn start_new_session(&mut self, project_id: ProjectId) {
        self.workbench.apply(Action::SelectProject(project_id));
        // An empty visible session of the same project is reused instead of
        // spawning another blank one.
        if self.active().is_some_and(|session| {
            session.project_id == project_id
                && !self
                    .workbench
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
                self.workbench.apply(Action::SessionsLoaded {
                    project_id,
                    sessions,
                });
            }
            self.workbench.apply(Action::SelectSession(summary.id));
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
        if let Some(project_id) = self.workbench.state().selected_project {
            self.index_session(project_id, &path, Some(&title));
        }
        if self.sessions.active_key() == Some(path.as_str()) {
            self.refresh_runtime_controls();
        }
    }

    fn set_current_session_name(&mut self, name: String) {
        let Some(key) = self.sessions.active_key().map(str::to_owned) else {
            self.notices.error("No active session to name.");
            return;
        };
        if key.starts_with("draft://") {
            self.notices.error("No active session to name.");
            return;
        }
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.notices.error("Usage: /name <name>");
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
                    self.notices.error(format!("Session exported to {path}"));
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
            self.notices
                .error("Could not export the session for sharing.");
            return;
        };
        let output = std::process::Command::new("gh")
            .args(["gist", "create", "--public=false", &path])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                self.notices.error(format!("Share URL: {url}"));
            }
            Ok(output) => {
                self.notices
                    .error(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(error) => self
                .notices
                .error(format!("GitHub CLI unavailable: {error}")),
        }
    }

    fn clone_session(&mut self) {
        if let Err(error) = self.active_command(json!({"type":"clone"})) {
            self.notices.error(error);
            return;
        }
        if let Some(project_id) = self.workbench.state().selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn fork_session(&mut self, entry_id: String) {
        if let Err(error) = self.active_command(json!({"type":"fork", "entryId": entry_id})) {
            self.notices.error(error);
            return;
        }
        if let Some(project_id) = self.workbench.state().selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn delete_session(&mut self, path: String) {
        // Stop the session's own process first so it cannot rewrite the file
        // after the delete; the conversation moves to another live session.
        let was_visible = self.sessions.active_key() == Some(path.as_str());
        if let Some(mut session) = self.sessions.remove(&path) {
            let _ = session.runtime.stop();
            self.workbench.apply(Action::SessionRunning {
                path: path.clone(),
                running: false,
            });
            if was_visible {
                self.workbench.apply(Action::ClearConversation);
                self.workbench
                    .apply(Action::SetSessionStatus(SessionStatus::Offline));
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
            self.notices.error("Could not move the Pi session to Trash");
            return;
        }
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.delete_session(stable_session_id(&path))
        {
            self.notices.error(error.to_string());
        }
        if let Some(project_id) = self.workbench.state().selected_project {
            self.discover_sessions(project_id, target.parent().unwrap_or(Path::new("")));
        }
    }

    fn load_current_entries(&mut self) -> Result<(), ()> {
        let entries = self
            .active_command(json!({"type":"get_entries"}))
            .map_err(|error| {
                self.notices.error(error);
            })?;
        self.workbench.apply(Action::ClearConversation);
        for action in entries
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(events::session_entry_action)
        {
            self.workbench.apply(action);
        }
        Ok(())
    }

    fn add_file_attachments(&mut self) {
        if let Some(paths) = rfd::FileDialog::new()
            .set_title("Choose attachments")
            .pick_files()
        {
            self.add_attachments(paths);
        }
    }

    fn add_folder_attachment(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Choose attachment folder")
            .pick_folder()
        {
            self.add_attachments(vec![path]);
        }
    }

    fn add_attachments(&mut self, paths: Vec<PathBuf>) {
        for path in paths {
            match attachment_from_path(&path, false) {
                Ok(attachment) => self
                    .workbench
                    .composer_draft_mut()
                    .add_attachment(attachment),
                Err(error) => self.notices.error(error),
            }
        }
    }

    fn add_clipboard_attachment(&mut self, attachment: ClipboardAttachment) {
        match attachment {
            ClipboardAttachment::Paths(paths) => self.add_attachments(paths),
            ClipboardAttachment::Image {
                width,
                height,
                rgba,
            } => match self
                .attachment_store
                .create_pasted_image(width, height, &rgba)
            {
                Ok(attachment) => self
                    .workbench
                    .composer_draft_mut()
                    .add_attachment(attachment),
                Err(error) => self.notices.error(error),
            },
        }
    }

    fn remove_composer_attachment(&mut self, path: &str) {
        let attachment = self
            .workbench
            .composer_draft()
            .attachments()
            .iter()
            .find(|attachment| attachment.path == path)
            .cloned();
        self.workbench.composer_draft_mut().remove_attachment(path);
        if attachment.is_some_and(|attachment| attachment.generated_by_app)
            && let Err(error) = self.attachment_store.remove_generated(path)
        {
            self.notices.error(error);
        }
    }

    fn capture_attachment_paste(&mut self, text: &str) -> bool {
        if let Ok(mut clipboard) = arboard::Clipboard::new()
            && let Ok(paths) = clipboard.get().file_list()
            && !paths.is_empty()
        {
            self.add_attachments(paths);
            return true;
        }
        if let Ok(mut clipboard) = arboard::Clipboard::new()
            && let Ok(image) = clipboard.get_image()
        {
            self.add_clipboard_attachment(ClipboardAttachment::Image {
                width: image.width,
                height: image.height,
                rgba: image.bytes.into_owned(),
            });
            return true;
        }
        if is_large_paste(text) {
            match self.attachment_store.create_pasted_text(text) {
                Ok(attachment) => {
                    self.workbench
                        .composer_draft_mut()
                        .add_attachment(attachment);
                    return true;
                }
                Err(error) => self.notices.error(error),
            }
        }
        false
    }

    fn submit_prompt(&mut self, content: String, attachments: Vec<Attachment>, mode: SubmitMode) {
        if self.workbench.state().selected_project.is_none() {
            self.notices
                .error("Select a project before sending a message.");
            return;
        }
        if !matches!(
            self.workbench.state().session_status,
            SessionStatus::Ready | SessionStatus::Streaming | SessionStatus::Compacting
        ) {
            self.notices
                .error("Pi is not ready for the selected project yet.");
            return;
        }
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
        self.workbench.apply(Action::UpsertConversation(item));
        // A deferred model switch waits until the prompt that continues the
        // conversation, so the prior model compacts the existing history first
        // (cache-friendly). Skip when there's nothing to compact or it just did.
        let defer_for_compaction = matches!(mode, SubmitMode::Prompt)
            && self.workbench.state().pending_model.is_some()
            && !self
                .active()
                .map(|session| session.turn.conversation_compacted)
                .unwrap_or(true)
            && self
                .workbench
                .state()
                .conversation
                .iter()
                .any(|message| message.role != ConversationRole::User);
        if defer_for_compaction {
            let Some(key) = self.sessions.active_key().map(str::to_owned) else {
                self.notices.error("No active session.");
                return;
            };
            let result = self.active_send(json!({"type":"compact"}));
            match result {
                Ok(()) => {
                    if let Some(session) = self.sessions.get_mut(&key) {
                        session.turn.pending_prompt = Some((content, attachments, mode));
                        session.turn.running = true;
                    }
                    self.workbench
                        .apply(Action::SetSessionStatus(SessionStatus::Compacting));
                }
                Err(error) => self.notices.error(error),
            }
            return;
        }
        let Some(key) = self.sessions.active_key().map(str::to_owned) else {
            self.notices.error("No active session.");
            return;
        };
        if self.workbench.state().pending_model.is_some() {
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
                self.notices.error("The session is no longer running.");
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
        self.workbench.apply(Action::SessionRunning {
            path: key.to_owned(),
            running: true,
        });
        if is_active {
            self.workbench
                .apply(Action::SetSessionStatus(SessionStatus::Streaming));
            self.ensure_session_title();
        }
    }

    fn ensure_session_title(&mut self) {
        let Some(project_id) = self.workbench.state().selected_project else {
            return;
        };
        let Ok(state) = self.active_command(json!({"type":"get_state"})) else {
            return;
        };
        if state.get("sessionName").and_then(Value::as_str).is_some() {
            return;
        }
        let Some(user_message) = self
            .workbench
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

    /// Drain every session process. Events from the visible session update the
    /// conversation view; background sessions only update bookkeeping (busy
    /// dots, sidebar titles) so their progress survives until they are shown.
    fn consume_runtime_events(&mut self) {
        for (token, event) in self.sessions.drain_events() {
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
                self.prompts.push_extension(key, &value);
            }
            RuntimeEvent::Interaction(value) => {
                self.prompts.push_interaction(key, &value);
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
                self.prompts.forget_session(key);
                self.workbench.apply(Action::SessionRunning {
                    path: key.to_owned(),
                    running: false,
                });
                if is_active {
                    self.workbench
                        .apply(Action::SetSessionStatus(SessionStatus::Failed(format!(
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
        let Some(session) = self.sessions.get_mut(key) else {
            return;
        };
        // The conversation is cloned because `translate` reads it while holding
        // the turn mutably. Only tool events look at it, and only to find one
        // entry, so a borrow split would cost more in plumbing than this does.
        let conversation = self.workbench.state().conversation.clone();
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
            self.workbench.apply(action);
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
                if self.workbench.state().pending_model.is_some() {
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
        if let Some(project) = self.workbench.state().selected_project {
            self.stop_project_runtimes(project);
            self.start_project(project);
        }
    }

    /// The one chooser both asking protocols share.
    ///
    /// What a request means is read in `engine::dialogs`; what stays here is the
    /// window and routing the answer back to the session that asked, which may
    /// be running in the background.
    fn prompt_dialog(&mut self, context: &egui::Context) {
        let Some(prompt) = self.prompts.current().cloned() else {
            return;
        };
        let mut answer = None;
        let mut open = true;
        egui::Window::new(prompt.title.as_str())
            .open(&mut open)
            .collapsible(false)
            .show(context, |ui| {
                if !prompt.message.is_empty() {
                    ui.label(prompt.message.as_str());
                    ui.add_space(8.0);
                }
                ui.horizontal_wrapped(|ui| {
                    for choice in &prompt.choices {
                        if ui.button(choice.label.as_str()).clicked() {
                            answer = Some(self.prompts.answer(&choice.value));
                        }
                    }
                });
            });
        // Closing the window is an answer too: the agent is blocked waiting, and
        // the prompt names the cautious one to send.
        if !open && answer.is_none() {
            answer = Some(self.prompts.dismiss());
        }
        if let Some(Some(answer)) = answer {
            self.send_answer(answer);
        }
    }

    /// Carry a decision back over whichever channel asked.
    fn send_answer(&mut self, answer: dialogs::Answer) {
        let Some(session) = self.sessions.get(answer.session_key()) else {
            self.notices
                .error("The session that asked is no longer running.");
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

    /// Show the oldest unread message, one at a time.
    fn notice_window(&mut self, context: &egui::Context) {
        if self.showing.is_none() {
            self.showing = self.notices.take();
        }
        let Some(showing) = self.showing.clone() else {
            return;
        };
        let title = if showing.is_error() {
            "Pi-Whim error"
        } else {
            "Pi-Whim"
        };
        let mut open = true;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .show(context, |ui| {
                ui.label(showing.message.as_str());
            });
        if !open {
            self.showing = None;
        }
    }
}

/// Pi accepts an environment reference here, keeping API keys out of models.json.
fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::ConversationItem;
    use pi_whim_runtime::FakeRuntime;
    use serde_json::json;
    use tempfile::TempDir;

    fn test_application(
        directory: &TempDir,
        runtime: FakeRuntime,
    ) -> PiWhimApplication<FakeRuntime> {
        // Pooled runtimes are clones of the prototype so the observer sees
        // every start and command across all session processes.
        let factory_runtime = runtime;
        PiWhimApplication {
            workbench: Workbench::default(),
            store: Some(SqliteStore::open(directory.path().join("test.sqlite")).unwrap()),
            secrets: MacosKeychainStore::default(),
            runtime_factory: Box::new(move || factory_runtime.clone()),
            sessions: SessionPool::new(),
            prompts: dialogs::Queue::new(),
            capability_resolver: ModelCapabilityResolver::new(
                &pi_whim_catalog::SharedCatalog::default(),
                false,
            ),
            sessions_root_override: Some(directory.path().join("sessions")),
            agent_directory_override: Some(directory.path().join("agent")),
            attachment_store: AttachmentStore::open(directory.path().join("attachments")).unwrap(),
            finder_paste_monitor: None,
            finder_paste_monitor_install_pending: false,
            notices: notice::Outbox::new(),
            showing: None,
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
        app.workbench
            .apply(Action::ProjectsLoaded(vec![project.clone()]));
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
        app.workbench
            .apply(Action::ProjectsLoaded(vec![first.clone(), second.clone()]));

        app.start_new_session(first.id);
        assert_eq!(observer.starts().len(), 1);

        // A still-empty visible session is reused instead of spawning blanks.
        app.start_new_session(first.id);
        assert_eq!(observer.starts().len(), 1);

        app.workbench
            .apply(Action::UpsertConversation(ConversationItem {
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
        let project_id = app.workbench.state().selected_project.unwrap();
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

        assert_eq!(app.workbench.state().thinking_level, ThinkingLevel::Off);
        assert_eq!(
            app.workbench.state().available_thinking_levels,
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
        assert_eq!(
            app.workbench.state().pending_model.as_ref().unwrap().id,
            "model-b"
        );
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
        app.workbench
            .apply(Action::SetSessionStatus(SessionStatus::Ready));
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

        assert!(!app.workbench.state().auto_compaction_enabled);
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
        app.workbench.apply(Action::ProjectsLoaded(vec![project]));
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
            app.workbench
                .state()
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
