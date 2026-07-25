mod macos_paste;

use std::{
    collections::{HashMap, HashSet},
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
    content_text, session_summary_from_jsonl,
};
use pi_whim_runtime::{AgentRuntime, PiRpcRuntime, RuntimeEvent, RuntimeStart};
use pi_whim_ui::{UiIntent, Workbench, install_fonts};
use serde_json::{Value, json};
use uuid::Uuid;

use macos_paste::{ClipboardAttachment, FinderPasteMonitor};
use pi_whim_catalog::ModelCapabilityResolver;
use pi_whim_engine::controls;
use pi_whim_engine::pool::{SessionPool, SessionRuntime, is_draft};
use pi_whim_engine::protocol::{
    assistant_text, queue_mode_name, tool_call_report, tool_event_details, tool_result_report,
    tool_result_summary,
};
use pi_whim_engine::providers::{
    configured_provider_environment, discover_models, normalize_base_url, pi_models_json,
    provider_keychain_account, test_searxng_engine, valid_search_engine_url,
};
use pi_whim_engine::session::{
    attachment_from_path, bash_policy_name, canonical_path, ensure_agent_team_extension,
    is_large_paste, now_ms, pi_agent_directory, prompt_with_attachment_paths,
};

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
    /// Extension confirmations and supervisor interactions are tagged with the
    /// owning session so background agents can still prompt the user.
    pending_extension_request: Option<(String, Value)>,
    pending_interactions: Vec<(String, Value)>,
    capability_resolver: ModelCapabilityResolver,
    sessions_root_override: Option<PathBuf>,
    agent_directory_override: Option<PathBuf>,
    attachment_store: AttachmentStore,
    finder_paste_monitor: Option<FinderPasteMonitor>,
    finder_paste_monitor_install_pending: bool,
    error: Option<String>,
    notice: Option<String>,
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
            pending_extension_request: None,
            pending_interactions: Vec::new(),
            capability_resolver,
            sessions_root_override: None,
            agent_directory_override: None,
            attachment_store: AttachmentStore::open_default(),
            finder_paste_monitor: None,
            finder_paste_monitor_install_pending: true,
            error: None,
            notice: None,
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
        self.extension_dialog(context);
        self.interaction_dialog(context);
        if let Some(error) = self.error.clone() {
            let mut open = true;
            egui::Window::new("Pi-Whim error")
                .open(&mut open)
                .collapsible(false)
                .show(context, |ui| {
                    ui.label(error);
                });
            if !open {
                self.error = None;
            }
        }
        if let Some(notice) = self.notice.clone() {
            let mut open = true;
            egui::Window::new("Pi-Whim")
                .open(&mut open)
                .collapsible(false)
                .show(context, |ui| {
                    ui.label(notice);
                });
            if !open {
                self.notice = None;
            }
        }
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
                    self.error = Some(error.to_string());
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
                    self.error = Some("Select a project before adding attachments.".into());
                }
            }
            UiIntent::AddFolderAttachment => {
                if self.workbench.state().selected_project.is_some() {
                    self.add_folder_attachment();
                } else {
                    self.error = Some("Select a project before adding attachments.".into());
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
                    self.error = Some(error);
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
            UiIntent::RespondExtensionUi {
                request_id,
                confirmed,
            } => {
                let command = json!({"type":"extension_ui_response", "id": request_id, "confirmed": confirmed});
                let owner = self
                    .pending_extension_request
                    .as_ref()
                    .map(|(key, _)| key.clone());
                let result = owner
                    .and_then(|key| self.sessions.get(&key))
                    .map(|session| session.runtime.respond_extension_ui(command));
                match result {
                    Some(Err(error)) => self.error = Some(error.to_string()),
                    Some(Ok(())) => {}
                    None => {
                        self.error = Some("The session that asked is no longer running.".into())
                    }
                }
            }
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
            self.error = Some(error.to_string());
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
                Err(error) => self.error = Some(error.to_string()),
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
            self.error = Some(error.to_string());
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
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn save_provider(&mut self, mut profile: ProviderProfile, api_key: Option<String>) {
        profile.name = normalize_provider_display_name(&profile.name);
        if profile.name.trim().is_empty()
            || profile.base_url.trim().is_empty()
            || profile.models.is_empty()
        {
            self.error = Some("A provider needs a name, base URL, and at least one model.".into());
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
            self.error = Some(format!(
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
                self.error = Some(error.to_string());
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
                self.error = Some(
                    "The API key could not be read back from Keychain. Pi was not restarted; try Save and apply again."
                        .into(),
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
            self.error = Some(
                "This provider has no API key in Keychain. Enter and save its API key before starting Pi."
                    .into(),
            );
            return;
        }
        self.workbench.set_provider_key_status(profile.id, true);
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_provider_profile(&profile)
        {
            self.error = Some(error.to_string());
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
            self.error = Some(error.to_string());
            return;
        }
        if let Err(error) = self.secrets.delete(&provider_keychain_account(profile_id)) {
            self.error = Some(error.to_string());
        }
        self.reload_provider_profiles();
        self.restart_selected_project();
    }

    fn save_search_engines(&mut self, profiles: Vec<SearchEngineProfile>) {
        if let Some(invalid) = profiles.iter().find(|profile| {
            profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url)
        }) {
            self.error = Some(format!(
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
            self.error = Some(error.to_string());
            return;
        }
        self.workbench
            .apply(Action::SearchEngineProfilesLoaded(profiles));
        self.restart_selected_project();
    }

    fn test_search_engine(&mut self, profile: SearchEngineProfile) {
        if profile.name.trim().is_empty() || !valid_search_engine_url(&profile.base_url) {
            self.error =
                Some("Enter a name and valid HTTP or HTTPS base URL before testing.".into());
            return;
        }
        match test_searxng_engine(&profile) {
            Ok(()) => {
                self.notice = Some(format!(
                    "{} is reachable and returned valid SearXNG JSON.",
                    profile.name
                ))
            }
            Err(error) => self.error = Some(format!("{} test failed: {error}", profile.name)),
        }
    }

    fn prepare_pi_configuration(&self) -> Result<HashMap<String, String>, String> {
        const PI_COMPACTION_KEEP_RECENT_TOKENS: u64 = 100;
        let agent_directory = self
            .agent_directory_override
            .clone()
            .map(Ok)
            .unwrap_or_else(pi_agent_directory)?;
        fs::create_dir_all(&agent_directory).map_err(|error| error.to_string())?;
        let mut profiles = self
            .store
            .as_ref()
            .map(ProviderRepository::list_provider_profiles)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
        for profile in &mut profiles {
            self.capability_resolver.enrich_profile(profile);
        }
        let (configured_profiles, mut environment) =
            configured_provider_environment(profiles, |profile_id| {
                self.secrets
                    .get(&provider_keychain_account(profile_id))
                    .map_err(|error| error.to_string())
            })?;
        fs::write(
            agent_directory.join("models.json"),
            serde_json::to_vec_pretty(&pi_models_json(&configured_profiles))
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        // Lower pi-mono's keepRecentTokens (default 20000) so small sessions can be compacted.
        let settings_path = agent_directory.join("settings.json");
        let mut settings: Value = fs::read(&settings_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = settings.as_object_mut() {
            let compaction = obj
                .entry("compaction".to_string())
                .or_insert_with(|| json!({}));
            if let Some(compaction) = compaction.as_object_mut() {
                compaction.insert(
                    "keepRecentTokens".to_string(),
                    Value::from(PI_COMPACTION_KEEP_RECENT_TOKENS),
                );
            }
        }
        fs::write(
            &settings_path,
            serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;

        environment.insert(
            "PI_CODING_AGENT_DIR".into(),
            agent_directory.to_string_lossy().into_owned(),
        );
        Ok(environment)
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
            Ok(_) => {
                self.error =
                    Some("The provider returned no models; add a model ID manually.".into())
            }
            Err(error) => self.error = Some(error),
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
                self.error = Some(error.to_string());
                return None;
            }
        };
        if let Err(error) = fs::create_dir_all(&sessions_path) {
            self.error = Some(error.to_string());
            return None;
        }
        let mut environment = match self.prepare_pi_configuration() {
            Ok(environment) => environment,
            Err(error) => {
                self.error = Some(error);
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
                self.error = Some(error.to_string());
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
            self.error = Some(error.to_string());
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
            SessionRuntime {
                runtime,
                events,
                project_id,
                running: false,
                assistant_message_id: None,
                conversation_compacted: false,
                compaction_item_id: None,
                pending_prompt: None,
                last_used_ms: now_ms(),
            },
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
        let running = session.running;
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
                self.error = Some(error.to_string());
                return;
            }
            None => {
                self.error = Some("The session is no longer running.".into());
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
            self.error = Some(error);
            return;
        }
        self.refresh_runtime_controls();
    }

    fn set_auto_compaction(&mut self, enabled: bool) {
        if let Err(error) =
            self.active_command(json!({"type":"set_auto_compaction", "enabled": enabled}))
        {
            self.error = Some(error);
            return;
        }
        self.refresh_runtime_controls();
    }

    fn compact_session(&mut self) {
        if !matches!(self.workbench.state().session_status, SessionStatus::Ready) {
            return;
        }
        if let Err(error) = self.active_send(json!({"type":"compact"})) {
            self.error = Some(error);
            return;
        }
        if let Some(session) = self.active_mut() {
            session.running = true;
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
            self.error = Some(error);
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
            Err(error) => self.error = Some(error),
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
                self.error = Some(error.to_string());
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
                    self.error = Some(error.to_string());
                }
            }
        }
        if let Ok(indexed_sessions) = store.list_sessions(project_id) {
            for session in indexed_sessions {
                if Path::new(&session.pi_path).starts_with(sessions_path)
                    && !valid_session_ids.contains(&session.id)
                    && let Err(error) = store.delete_session(session.id)
                {
                    self.error = Some(error.to_string());
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
            self.error = Some(error.to_string());
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
            self.error = Some("No active session to name.".into());
            return;
        };
        if key.starts_with("draft://") {
            self.error = Some("No active session to name.".into());
            return;
        }
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.error = Some("Usage: /name <name>".into());
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
                    self.error = Some(format!("Session exported to {path}"));
                }
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn share_session(&mut self) {
        let Some(path) = self
            .active_command(json!({"type":"export_html"}))
            .ok()
            .and_then(|value| value.get("path").and_then(Value::as_str).map(str::to_owned))
        else {
            self.error = Some("Could not export the session for sharing.".into());
            return;
        };
        let output = std::process::Command::new("gh")
            .args(["gist", "create", "--public=false", &path])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                let url = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                self.error = Some(format!("Share URL: {url}"));
            }
            Ok(output) => {
                self.error = Some(String::from_utf8_lossy(&output.stderr).trim().to_string());
            }
            Err(error) => self.error = Some(format!("GitHub CLI unavailable: {error}")),
        }
    }

    fn clone_session(&mut self) {
        if let Err(error) = self.active_command(json!({"type":"clone"})) {
            self.error = Some(error);
            return;
        }
        if let Some(project_id) = self.workbench.state().selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn fork_session(&mut self, entry_id: String) {
        if let Err(error) = self.active_command(json!({"type":"fork", "entryId": entry_id})) {
            self.error = Some(error);
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
            self.error = Some("Could not move the Pi session to Trash".into());
            return;
        }
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.delete_session(stable_session_id(&path))
        {
            self.error = Some(error.to_string());
        }
        if let Some(project_id) = self.workbench.state().selected_project {
            self.discover_sessions(project_id, target.parent().unwrap_or(Path::new("")));
        }
    }

    fn load_current_entries(&mut self) -> Result<(), ()> {
        let entries = self
            .active_command(json!({"type":"get_entries"}))
            .map_err(|error| {
                self.error = Some(error);
            })?;
        self.workbench.apply(Action::ClearConversation);
        for entry in entries
            .get("entries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.apply_session_entry(entry);
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
                Err(error) => self.error = Some(error),
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
                Err(error) => self.error = Some(error),
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
            self.error = Some(error);
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
                Err(error) => self.error = Some(error),
            }
        }
        false
    }

    fn submit_prompt(&mut self, content: String, attachments: Vec<Attachment>, mode: SubmitMode) {
        if self.workbench.state().selected_project.is_none() {
            self.error = Some("Select a project before sending a message.".into());
            return;
        }
        if !matches!(
            self.workbench.state().session_status,
            SessionStatus::Ready | SessionStatus::Streaming | SessionStatus::Compacting
        ) {
            self.error = Some("Pi is not ready for the selected project yet.".into());
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
                .map(|session| session.conversation_compacted)
                .unwrap_or(true)
            && self
                .workbench
                .state()
                .conversation
                .iter()
                .any(|message| message.role != ConversationRole::User);
        if defer_for_compaction {
            let Some(key) = self.sessions.active_key().map(str::to_owned) else {
                self.error = Some("No active session.".into());
                return;
            };
            let result = self.active_send(json!({"type":"compact"}));
            match result {
                Ok(()) => {
                    if let Some(session) = self.sessions.get_mut(&key) {
                        session.pending_prompt = Some((content, attachments, mode));
                        session.running = true;
                    }
                    self.workbench
                        .apply(Action::SetSessionStatus(SessionStatus::Compacting));
                }
                Err(error) => self.error = Some(error),
            }
            return;
        }
        let Some(key) = self.sessions.active_key().map(str::to_owned) else {
            self.error = Some("No active session.".into());
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
                self.error = Some("The session is no longer running.".into());
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
            self.error = Some(error.to_string());
            return;
        }
        let is_active = self.sessions.active_key() == Some(key);
        if let Some(session) = self.sessions.get_mut(key) {
            session.running = true;
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
        let mut drained: Vec<(String, RuntimeEvent)> = Vec::new();
        for (key, session) in self.sessions.iter() {
            drained.extend(
                session
                    .events
                    .try_iter()
                    .map(|event| (key.to_owned(), event)),
            );
        }
        for (key, event) in drained {
            self.handle_runtime_event(&key, event);
        }
    }

    fn handle_runtime_event(&mut self, key: &str, event: RuntimeEvent) {
        let is_active = self.sessions.active_key() == Some(key);
        match event {
            RuntimeEvent::Agent(value) => self.apply_agent_event(key, value),
            RuntimeEvent::ExtensionUi(value) => {
                self.pending_extension_request = Some((key.to_owned(), value));
            }
            RuntimeEvent::Interaction(value) => {
                self.pending_interactions.push((key.to_owned(), value));
            }
            RuntimeEvent::Stderr(message) => {
                if is_active && !message.trim().is_empty() {
                    self.error = Some(message);
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
                    self.error = Some(error);
                }
            }
            RuntimeEvent::RpcResponse(_) => {}
        }
    }

    fn apply_agent_event(&mut self, key: &str, event: Value) {
        let is_active = self.sessions.active_key() == Some(key);
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") | Some("message_update") => {
                let message = event.get("message").cloned().unwrap_or(Value::Null);
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    let Some(session) = self.sessions.get_mut(key) else {
                        return;
                    };
                    session.running = true;
                    self.workbench.apply(Action::SessionRunning {
                        path: key.to_owned(),
                        running: true,
                    });
                    if !is_active {
                        return;
                    }
                    // A new assistant reply means the conversation grew past the
                    // last compaction; a later model switch should compact again.
                    session.conversation_compacted = false;
                    let id = message
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            session
                                .assistant_message_id
                                .clone()
                                .unwrap_or_else(|| Uuid::new_v4().to_string())
                        });
                    if let Some(previous_id) = session.assistant_message_id.replace(id.clone())
                        && previous_id != id
                    {
                        self.workbench.apply(Action::RekeyConversation {
                            from: previous_id,
                            to: id.clone(),
                        });
                    }
                    let text = assistant_text(&message);
                    self.workbench
                        .apply(Action::UpsertConversation(ConversationItem {
                            id,
                            role: ConversationRole::Assistant,
                            full_text: text,
                            streaming: true,
                            tool_name: None,
                            tool_report: None,
                            tool_details: None,
                            is_error: false,
                            model: message
                                .get("model")
                                .and_then(Value::as_str)
                                .map(str::to_owned),
                            attachments: Vec::new(),
                        }));
                }
            }
            Some("message_end")
                if event
                    .get("message")
                    .and_then(|message| message.get("role"))
                    .and_then(Value::as_str)
                    == Some("assistant") =>
            {
                let Some(session) = self.sessions.get_mut(key) else {
                    return;
                };
                if is_active && let Some(id) = session.assistant_message_id.take() {
                    self.workbench.apply(Action::FinishMessage(id));
                }
            }
            Some("tool_execution_start") | Some("tool_execution_end") => {
                if is_active {
                    self.apply_tool_event(&event)
                }
            }
            Some("queue_update") => {
                if !is_active {
                    return;
                }
                let steering = event
                    .get("steering")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                let follow_up = event
                    .get("followUp")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                self.workbench.apply(Action::QueueUpdated {
                    steering,
                    follow_up,
                });
            }
            Some("agent_settled") => {
                let Some(session) = self.sessions.get_mut(key) else {
                    return;
                };
                session.running = false;
                let project_id = session.project_id;
                let state = session.runtime.command(json!({"type":"get_state"})).ok();
                self.workbench.apply(Action::SessionRunning {
                    path: key.to_owned(),
                    running: false,
                });
                if let Some(state) = state
                    && let Some(path) = state.get("sessionFile").and_then(Value::as_str)
                {
                    self.index_session(
                        project_id,
                        path,
                        state.get("sessionName").and_then(Value::as_str),
                    );
                    if path != key {
                        self.rekey_session(key, path);
                    }
                }
                if is_active {
                    self.workbench
                        .apply(Action::SetSessionStatus(SessionStatus::Ready));
                    let _ = self.load_current_entries();
                    self.refresh_runtime_controls();
                }
            }
            Some("session_info_changed") => {
                let Some(session) = self.sessions.get(key) else {
                    return;
                };
                let project_id = session.project_id;
                if let Ok(state) = session.runtime.command(json!({"type":"get_state"}))
                    && let Some(path) = state.get("sessionFile").and_then(Value::as_str)
                {
                    self.index_session(project_id, path, event.get("name").and_then(Value::as_str));
                }
            }
            Some("thinking_level_changed") => {
                if is_active {
                    self.refresh_runtime_controls();
                }
            }
            Some("compaction_start") => {
                let Some(session) = self.sessions.get_mut(key) else {
                    return;
                };
                session.running = true;
                self.workbench.apply(Action::SessionRunning {
                    path: key.to_owned(),
                    running: true,
                });
                if !is_active {
                    return;
                }
                let item_id = format!("compaction-{}", now_ms());
                session.compaction_item_id = Some(item_id.clone());
                self.workbench
                    .apply(Action::UpsertConversation(ConversationItem {
                        id: item_id,
                        role: ConversationRole::Tool,
                        full_text: "Compacting…".into(),
                        streaming: false,
                        tool_name: Some("compact".into()),
                        tool_report: None,
                        tool_details: None,
                        is_error: false,
                        model: None,
                        attachments: Vec::new(),
                    }));
                self.workbench
                    .apply(Action::SetSessionStatus(SessionStatus::Compacting));
            }
            Some("compaction_end") => {
                let error = event.get("errorMessage").and_then(Value::as_str);
                let Some(session) = self.sessions.get_mut(key) else {
                    return;
                };
                session.running = false;
                session.conversation_compacted = error.is_none();
                let pending_prompt = session.pending_prompt.take();
                let compaction_item_id = session.compaction_item_id.take();
                self.workbench.apply(Action::SessionRunning {
                    path: key.to_owned(),
                    running: false,
                });
                if !is_active {
                    // The deferred prompt continues even with the session in
                    // the background; only the visible status updates skip.
                    if let Some((content, attachments, mode)) = pending_prompt {
                        if self.workbench.state().pending_model.is_some() {
                            self.apply_pending_model(key);
                        }
                        self.send_prompt(key, content, attachments, mode);
                    }
                    return;
                }
                let benign = error.is_some_and(|e| e.contains("Nothing to compact"));
                let status = match error {
                    Some(_) if benign => SessionStatus::Ready,
                    Some(e) => SessionStatus::Failed(e.to_owned()),
                    None => SessionStatus::Ready,
                };
                self.workbench.apply(Action::SetSessionStatus(status));
                if let Some(item_id) = compaction_item_id {
                    let (text, is_error) = match error {
                        Some(_) if benign => {
                            ("Nothing to compact (session too small)".to_owned(), false)
                        }
                        Some(e) => (e.to_owned(), true),
                        None => {
                            let result = event.get("result");
                            let before = result
                                .and_then(|r| r.get("tokensBefore"))
                                .and_then(Value::as_i64);
                            let after = result
                                .and_then(|r| r.get("estimatedTokensAfter"))
                                .and_then(Value::as_i64);
                            match (before, after) {
                                (Some(b), Some(a)) => {
                                    (format!("Compacted context · {b} → {a} tokens"), false)
                                }
                                _ => ("Compacted context".to_owned(), false),
                            }
                        }
                    };
                    self.workbench
                        .apply(Action::UpsertConversation(ConversationItem {
                            id: item_id,
                            role: ConversationRole::Tool,
                            full_text: text,
                            streaming: false,
                            tool_name: Some("compact".into()),
                            tool_report: None,
                            tool_details: None,
                            is_error,
                            model: None,
                            attachments: Vec::new(),
                        }));
                }
                // After a switch-triggered compaction, apply the pending model
                // and send the prompt that was held back.
                if let Some((content, attachments, mode)) = pending_prompt {
                    if self.workbench.state().pending_model.is_some() {
                        self.apply_pending_model(key);
                    }
                    self.send_prompt(key, content, attachments, mode);
                }
            }
            Some("entry_appended") => {
                if is_active && let Some(entry) = event.get("entry") {
                    self.apply_session_entry(entry);
                }
            }
            _ => {}
        }
    }

    fn apply_tool_event(&mut self, event: &Value) {
        let name = event
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("tool");
        let id = event
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or(name)
            .to_owned();
        let is_error = event
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let previous = self
            .workbench
            .state()
            .conversation
            .iter()
            .find(|message| message.id == id.as_str())
            .map(|message| (message.tool_report.clone(), message.tool_details.clone()));
        let previous_report = previous.as_ref().and_then(|(report, _)| report.as_deref());
        let previous_details = previous
            .as_ref()
            .and_then(|(_, details)| details.as_deref());
        let (content, tool_report) = match event.get("type").and_then(Value::as_str) {
            Some("tool_execution_end") => {
                let result_content = event.get("result").and_then(|result| result.get("content"));
                (
                    tool_result_summary(Some(name), result_content, is_error),
                    tool_result_report(Some(name), result_content, previous_report, is_error),
                )
            }
            _ => ("Running…".into(), tool_call_report(name, event.get("args"))),
        };
        self.workbench
            .apply(Action::UpsertConversation(ConversationItem {
                id,
                role: ConversationRole::Tool,
                full_text: content,
                streaming: false,
                tool_name: Some(name.into()),
                tool_report: Some(tool_report),
                tool_details: Some(tool_event_details(event, previous_details)),
                is_error,
                model: None,
                attachments: Vec::new(),
            }));
    }

    fn apply_session_entry(&mut self, entry: &Value) {
        let Some(message) = entry.get("message") else {
            return;
        };
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => ConversationRole::User,
            Some("assistant") => ConversationRole::Assistant,
            Some("toolResult") | Some("bashExecution") => ConversationRole::Tool,
            _ => return,
        };
        let id = entry
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("entry")
            .to_owned();
        let is_tool = role == ConversationRole::Tool;
        let tool_name = message
            .get("toolName")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let is_error = message
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let text = match role {
            ConversationRole::Assistant => assistant_text(message),
            ConversationRole::Tool => {
                tool_result_summary(tool_name.as_deref(), message.get("content"), is_error)
            }
            _ => content_text(message.get("content")).unwrap_or_else(|| message.to_string()),
        };
        self.workbench
            .apply(Action::UpsertConversation(ConversationItem {
                id,
                role,
                full_text: text,
                streaming: false,
                tool_name,
                tool_report: is_tool.then(|| {
                    tool_result_report(
                        message.get("toolName").and_then(Value::as_str),
                        message.get("content"),
                        None,
                        is_error,
                    )
                }),
                tool_details: is_tool.then(|| message.to_string()),
                is_error,
                model: message
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                attachments: Vec::new(),
            }));
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

    fn extension_dialog(&mut self, context: &egui::Context) {
        let Some((_, request)) = self.pending_extension_request.clone() else {
            return;
        };
        if request.get("method").and_then(Value::as_str) != Some("confirm") {
            return;
        }
        let mut open = true;
        egui::Window::new(
            request
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Pi confirmation"),
        )
        .open(&mut open)
        .collapsible(false)
        .show(context, |ui| {
            ui.label(
                request
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Allow this operation?"),
            );
            ui.horizontal(|ui| {
                if ui.button("Allow").clicked() {
                    self.handle_intent(UiIntent::RespondExtensionUi {
                        request_id: request["id"].as_str().unwrap_or_default().into(),
                        confirmed: true,
                    });
                    self.pending_extension_request = None;
                }
                if ui.button("Deny").clicked() {
                    self.handle_intent(UiIntent::RespondExtensionUi {
                        request_id: request["id"].as_str().unwrap_or_default().into(),
                        confirmed: false,
                    });
                    self.pending_extension_request = None;
                }
            });
        });
        if !open {
            self.pending_extension_request = None;
        }
    }

    /// This is the single native chooser used for root-owned approvals and
    /// questions. Pi's extension confirmations keep using their RPC response,
    /// while supervisor interactions return through the team tool host.
    fn interaction_dialog(&mut self, context: &egui::Context) {
        let Some((session_key, request)) = self.pending_interactions.first().cloned() else {
            return;
        };
        let request_id = request["request_id"].as_str().unwrap_or_default();
        let kind = request["kind"].as_str().unwrap_or("question");
        let title = request["title"].as_str().unwrap_or("Agent request");
        let message = request["message"].as_str().unwrap_or_default();
        let options = request["options"].as_array().cloned().unwrap_or_default();
        let cancel_decision = request["default_option"]
            .as_str()
            .filter(|option| {
                options
                    .iter()
                    .any(|candidate| candidate.as_str() == Some(*option))
            })
            .unwrap_or(if kind == "approval" { "deny" } else { "cancel" });
        let mut open = true;
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .show(context, |ui| {
                ui.label(message);
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    for option in &options {
                        let Some(option) = option.as_str() else {
                            continue;
                        };
                        let label = match (kind, option) {
                            ("approval", "approve") => "Allow once",
                            ("approval", "deny") => "Deny",
                            _ => option,
                        };
                        if ui.button(label).clicked() {
                            self.resolve_interaction(&session_key, request_id, option);
                            self.pending_interactions.remove(0);
                        }
                    }
                });
            });
        if !open {
            self.resolve_interaction(&session_key, request_id, cancel_decision);
            self.pending_interactions.remove(0);
        }
    }

    /// Route an approval/question answer to the supervisor of the session that
    /// asked, which may be running in the background.
    fn resolve_interaction(&mut self, session_key: &str, request_id: &str, decision: &str) {
        let Some(session) = self.sessions.get(session_key) else {
            return;
        };
        if let Err(error) = session
            .runtime
            .resolve_user_interaction(request_id.to_owned(), decision.to_owned())
        {
            self.error = Some(error.to_string());
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
            pending_extension_request: None,
            pending_interactions: Vec::new(),
            capability_resolver: ModelCapabilityResolver::new(
                &pi_whim_catalog::SharedCatalog::default(),
                false,
            ),
            sessions_root_override: Some(directory.path().join("sessions")),
            agent_directory_override: Some(directory.path().join("agent")),
            attachment_store: AttachmentStore::open(directory.path().join("attachments")).unwrap(),
            finder_paste_monitor: None,
            finder_paste_monitor_install_pending: false,
            error: None,
            notice: None,
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
                .is_some_and(|session| session.running)
        );

        // Switching away must not abort A: no abort/new_session/switch_session
        // RPCs, and A's process stays pooled with its running flag set.
        app.switch_session(project_id, "/sessions/b.jsonl".into());
        assert_eq!(app.sessions.active_key(), Some("/sessions/b.jsonl"));
        assert_eq!(app.sessions.iter().count(), 2);
        assert!(
            app.sessions
                .get(&running_key)
                .is_some_and(|session| session.running)
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
