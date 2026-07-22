use std::{
    collections::{HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::Engine;
use eframe::egui;
use pi_whim_core::{
    Action, AgentStatus, BashPolicy, ConversationItem, ConversationRole, ImageAttachment,
    ModelOption, Project, ProjectId, ProviderId, ProviderModel, ProviderProfile, ProviderProtocol,
    QueueMode, SessionMetrics, SessionSummary,
};
use pi_whim_persistence::{
    AppPreferences, MacosKeychainStore, PreferencesRepository, ProjectRepository,
    ProviderRepository, SecretStore, SessionRepository, SqliteStore,
};
use pi_whim_runtime::{AgentRuntime, PiRpcRuntime, RuntimeEvent, RuntimeStart};
use pi_whim_ui::{SubmitMode, UiIntent, Workbench, install_fonts};
use serde_json::{Value, json};
use uuid::Uuid;

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([900.0, 620.0])
            .with_title("Pi-Whim"),
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

struct PiWhimApplication {
    workbench: Workbench,
    store: Option<SqliteStore>,
    secrets: MacosKeychainStore,
    runtime: PiRpcRuntime,
    runtime_events: crossbeam_channel::Receiver<RuntimeEvent>,
    assistant_message_id: Option<String>,
    pending_extension_request: Option<Value>,
    error: Option<String>,
}

impl Default for PiWhimApplication {
    fn default() -> Self {
        let mut workbench = Workbench::default();
        let store = SqliteStore::open_default()
            .map_err(|error| error.to_string())
            .ok();
        if let Some(store) = store.as_ref()
            && let Ok(projects) = store.list_projects()
        {
            workbench.apply(Action::ProjectsLoaded(projects));
        }
        if let Some(store) = store.as_ref()
            && let Ok(preferences) = store.load_preferences()
        {
            workbench.apply(Action::SetLanguage(preferences.language));
            workbench.apply(Action::SetBashPolicy(preferences.bash_policy));
        }
        if let Some(store) = store.as_ref()
            && let Ok(mut profiles) = store.list_provider_profiles()
        {
            for profile in &mut profiles {
                profile.has_api_key = MacosKeychainStore::default()
                    .get(&provider_keychain_account(profile.id))
                    .ok()
                    .flatten()
                    .is_some();
            }
            workbench.apply(Action::ProviderProfilesLoaded(profiles));
        }
        let runtime = PiRpcRuntime::default();
        let runtime_events = runtime.events();
        Self {
            workbench,
            store,
            secrets: MacosKeychainStore::default(),
            runtime,
            runtime_events,
            assistant_message_id: None,
            pending_extension_request: None,
            error: None,
        }
    }
}

impl eframe::App for PiWhimApplication {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.consume_runtime_events();
        self.workbench.show(context);
        for intent in self.workbench.take_intents() {
            self.handle_intent(intent);
        }
        self.extension_dialog(context);
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
        context.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

impl PiWhimApplication {
    fn handle_intent(&mut self, intent: UiIntent) {
        match intent {
            UiIntent::AddProject => self.add_project(),
            UiIntent::RemoveProject(project_id) => {
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
            UiIntent::StartNewSession => {
                if self.workbench.state.selected_project.is_none() {
                    self.error = Some("Select a project before creating a session.".into());
                    return;
                }
                if !self
                    .workbench
                    .state
                    .conversation
                    .iter()
                    .any(|message| message.role == ConversationRole::User)
                {
                    return;
                }
                if let Err(error) = self.runtime.command(json!({"type":"new_session"})) {
                    self.error = Some(error.to_string());
                }
                self.workbench.apply(Action::ClearConversation);
            }
            UiIntent::SwitchSession(path) => self.switch_session(path),
            UiIntent::RenameSession { path, title } => self.rename_session(path, title),
            UiIntent::CloneSession => self.clone_session(),
            UiIntent::ForkSession(entry_id) => self.fork_session(entry_id),
            UiIntent::DeleteSession(path) => self.delete_session(path),
            UiIntent::AddImageAttachment => {
                if self.workbench.state.selected_project.is_some() {
                    self.add_image_attachment();
                } else {
                    self.error = Some("Select a project before adding an image.".into());
                }
            }
            UiIntent::SubmitPrompt {
                content,
                attachments,
                mode,
            } => self.submit_prompt(content, attachments, mode),
            UiIntent::Stop => {
                if let Err(error) = self.runtime.command(json!({"type":"abort"})) {
                    self.error = Some(error.to_string());
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
            UiIntent::SetModel(model) => self.set_model(model),
            UiIntent::SetThinkingLevel(level) => self.set_thinking_level(level),
            UiIntent::SetQueueModes {
                steering,
                follow_up,
            } => self.set_queue_modes(steering, follow_up),
            UiIntent::SaveProvider { profile, api_key } => self.save_provider(profile, api_key),
            UiIntent::DeleteProvider(profile_id) => self.delete_provider(profile_id),
            UiIntent::DiscoverProviderModels {
                profile_id,
                base_url,
                protocol,
                api_key,
            } => self.discover_provider_models(profile_id, base_url, protocol, api_key),
            UiIntent::RespondExtensionUi {
                request_id,
                confirmed,
            } => {
                let command = json!({"type":"extension_ui_response", "id": request_id, "confirmed": confirmed});
                if let Err(error) = self.runtime.respond_extension_ui(command) {
                    self.error = Some(error.to_string());
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
                Ok(projects) => self.workbench.apply(Action::ProjectsLoaded(projects)),
                Err(error) => self.error = Some(error.to_string()),
            }
        }
    }

    fn save_preferences(&mut self) {
        let preferences = AppPreferences {
            language: self.workbench.state.language,
            bash_policy: self.workbench.state.bash_policy,
        };
        if let Some(store) = self.store.as_ref()
            && let Err(error) = store.save_preferences(preferences)
        {
            self.error = Some(error.to_string());
        }
    }

    fn reload_provider_profiles(&mut self) {
        let Some(store) = self.store.as_ref() else {
            return;
        };
        match store.list_provider_profiles() {
            Ok(mut profiles) => {
                for profile in &mut profiles {
                    profile.has_api_key = self
                        .secrets
                        .get(&provider_keychain_account(profile.id))
                        .ok()
                        .flatten()
                        .is_some();
                }
                self.workbench
                    .apply(Action::ProviderProfilesLoaded(profiles));
            }
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    fn save_provider(&mut self, mut profile: ProviderProfile, api_key: Option<String>) {
        if profile.name.trim().is_empty()
            || profile.base_url.trim().is_empty()
            || profile.models.is_empty()
        {
            self.error = Some("A provider needs a name, base URL, and at least one model.".into());
            return;
        }
        profile.base_url = normalize_base_url(&profile.base_url);
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

    fn prepare_pi_configuration(&self) -> Result<HashMap<String, String>, String> {
        let agent_directory = pi_agent_directory()?;
        fs::create_dir_all(&agent_directory).map_err(|error| error.to_string())?;
        let profiles = self
            .store
            .as_ref()
            .map(ProviderRepository::list_provider_profiles)
            .transpose()
            .map_err(|error| error.to_string())?
            .unwrap_or_default();
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

        environment.insert(
            "PI_CODING_AGENT_DIR".into(),
            agent_directory.to_string_lossy().into_owned(),
        );
        Ok(environment)
    }

    fn discover_provider_models(
        &mut self,
        profile_id: Option<ProviderId>,
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
            Ok(models) if !models.is_empty() => self.workbench.set_discovered_models(models),
            Ok(_) => {
                self.error =
                    Some("The provider returned no models; add a model ID manually.".into())
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn find_project(&self, id: ProjectId) -> Option<Project> {
        self.workbench
            .state
            .projects
            .iter()
            .find(|project| project.id == id)
            .cloned()
    }

    fn start_project(&mut self, project_id: ProjectId) {
        let Some(project) = self.find_project(project_id) else {
            return;
        };
        let sessions_path = match SqliteStore::sessions_root() {
            Ok(root) => root.join(project.id.to_string()),
            Err(error) => {
                self.error = Some(error.to_string());
                return;
            }
        };
        if let Err(error) = fs::create_dir_all(&sessions_path) {
            self.error = Some(error.to_string());
            return;
        }
        let mut environment = match self.prepare_pi_configuration() {
            Ok(environment) => environment,
            Err(error) => {
                self.error = Some(error);
                return;
            }
        };
        self.workbench
            .apply(Action::SetAgentStatus(AgentStatus::Starting));
        let extension_path = match ensure_bash_extension(&sessions_path) {
            Ok(path) => Some(path.to_string_lossy().into_owned()),
            Err(error) => {
                self.error = Some(error.to_string());
                None
            }
        };
        environment.insert(
            "PI_WHIM_BASH_POLICY".into(),
            bash_policy_name(&self.workbench.state.bash_policy).into(),
        );
        match self.runtime.start(RuntimeStart {
            project_path: project.path,
            sessions_path: sessions_path.to_string_lossy().into_owned(),
            extension_path,
            environment,
        }) {
            Ok(()) => {
                self.workbench
                    .apply(Action::SetAgentStatus(AgentStatus::Ready));
                self.discover_sessions(project.id, &sessions_path);
                self.refresh_session_state(project.id);
                self.refresh_runtime_controls();
            }
            Err(error) => {
                self.workbench
                    .apply(Action::SetAgentStatus(AgentStatus::Failed(
                        error.to_string(),
                    )));
                self.error = Some(error.to_string());
            }
        }
    }

    fn refresh_runtime_controls(&mut self) {
        let state = match self.runtime.command(json!({"type":"get_state"})) {
            Ok(state) => state,
            Err(error) => {
                self.workbench
                    .apply(Action::SetAgentStatus(AgentStatus::Failed(
                        error.to_string(),
                    )));
                return;
            }
        };
        let models = match self.runtime.command(json!({"type":"get_available_models"})) {
            Ok(response) => response
                .get("models")
                .cloned()
                .and_then(|models| serde_json::from_value::<Vec<Value>>(models).ok())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|model| model_option(&model))
                .collect(),
            Err(error) => {
                self.workbench
                    .apply(Action::SetAgentStatus(AgentStatus::Failed(
                        error.to_string(),
                    )));
                Vec::new()
            }
        };
        let thinking_levels = self
            .runtime
            .command(json!({"type":"get_available_thinking_levels"}))
            .ok()
            .and_then(|response| response.get("levels").cloned())
            .and_then(|levels| serde_json::from_value::<Vec<String>>(levels).ok())
            .unwrap_or_default();
        let steering_mode = state
            .get("steeringMode")
            .and_then(Value::as_str)
            .map(queue_mode)
            .unwrap_or_default();
        let follow_up_mode = state
            .get("followUpMode")
            .and_then(Value::as_str)
            .map(queue_mode)
            .unwrap_or_default();
        self.workbench.apply(Action::RuntimeControlsUpdated {
            current_model: state.get("model").and_then(model_option),
            available_models: models,
            thinking_level: state
                .get("thinkingLevel")
                .and_then(Value::as_str)
                .unwrap_or("off")
                .into(),
            available_thinking_levels: thinking_levels,
            steering_mode,
            follow_up_mode,
        });
        if let Ok(metrics) = self.runtime.command(json!({"type":"get_session_stats"})) {
            self.workbench
                .apply(Action::SessionMetricsUpdated(session_metrics(&metrics)));
        }
    }

    fn set_model(&mut self, model: ModelOption) {
        if let Err(error) = self.runtime.command(json!({
            "type":"set_model",
            "provider": model.provider,
            "modelId": model.id,
        })) {
            self.error = Some(error.to_string());
            return;
        }
        self.refresh_runtime_controls();
    }

    fn set_thinking_level(&mut self, level: String) {
        if let Err(error) = self
            .runtime
            .command(json!({"type":"set_thinking_level", "level": level}))
        {
            self.error = Some(error.to_string());
            return;
        }
        self.refresh_runtime_controls();
    }

    fn set_queue_modes(&mut self, steering: QueueMode, follow_up: QueueMode) {
        let steering = queue_mode_name(steering);
        let follow_up = queue_mode_name(follow_up);
        let result = self
            .runtime
            .command(json!({"type":"set_steering_mode", "mode": steering}))
            .and_then(|_| {
                self.runtime
                    .command(json!({"type":"set_follow_up_mode", "mode": follow_up}))
            });
        if let Err(error) = result {
            self.error = Some(error.to_string());
            return;
        }
        self.refresh_runtime_controls();
    }

    fn refresh_session_state(&mut self, project_id: ProjectId) {
        match self.runtime.command(json!({"type":"get_state"})) {
            Ok(state) => {
                if let Some(path) = state.get("sessionFile").and_then(Value::as_str) {
                    self.index_session(
                        project_id,
                        path,
                        state.get("sessionName").and_then(Value::as_str),
                    );
                }
                let _ = self.load_current_entries();
            }
            Err(error) => self.error = Some(error.to_string()),
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

    fn switch_session(&mut self, path: String) {
        if let Err(error) = self
            .runtime
            .command(json!({"type":"switch_session", "sessionPath": path}))
        {
            self.error = Some(error.to_string());
            return;
        }
        self.workbench.apply(Action::ClearConversation);
        let _ = self.load_current_entries();
    }

    fn rename_session(&mut self, path: String, title: String) {
        if let Err(error) = self
            .runtime
            .command(json!({"type":"switch_session", "sessionPath": path}))
        {
            self.error = Some(error.to_string());
            return;
        }
        if let Err(error) = self
            .runtime
            .command(json!({"type":"set_session_name", "name": title}))
        {
            self.error = Some(error.to_string());
            return;
        }
        if let Some(project_id) = self.workbench.state.selected_project {
            self.index_session(project_id, &path, Some(&title));
        }
    }

    fn clone_session(&mut self) {
        if let Err(error) = self.runtime.command(json!({"type":"clone"})) {
            self.error = Some(error.to_string());
            return;
        }
        if let Some(project_id) = self.workbench.state.selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn fork_session(&mut self, entry_id: String) {
        if let Err(error) = self
            .runtime
            .command(json!({"type":"fork", "entryId": entry_id}))
        {
            self.error = Some(error.to_string());
            return;
        }
        if let Some(project_id) = self.workbench.state.selected_project {
            self.refresh_session_state(project_id);
        }
    }

    fn delete_session(&mut self, path: String) {
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
        if let Some(project_id) = self.workbench.state.selected_project {
            self.discover_sessions(project_id, target.parent().unwrap_or(Path::new("")));
        }
    }

    fn load_current_entries(&mut self) -> Result<(), ()> {
        let entries = self
            .runtime
            .command(json!({"type":"get_entries"}))
            .map_err(|error| {
                self.error = Some(error.to_string());
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

    fn add_image_attachment(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Images", &["png", "jpg", "jpeg", "gif", "webp"])
            .pick_file()
        else {
            return;
        };
        match image_attachment(&path) {
            Ok(attachment) => self
                .workbench
                .apply(Action::AddComposerAttachment(attachment)),
            Err(error) => self.error = Some(error),
        }
    }

    fn submit_prompt(
        &mut self,
        content: String,
        attachments: Vec<ImageAttachment>,
        mode: SubmitMode,
    ) {
        if self.workbench.state.selected_project.is_none() {
            self.error = Some("Select a project before sending a message.".into());
            return;
        }
        if !matches!(
            self.workbench.state.agent_status,
            AgentStatus::Ready | AgentStatus::Streaming | AgentStatus::Compacting
        ) {
            self.error = Some("Pi is not ready for the selected project yet.".into());
            return;
        }
        let item = ConversationItem {
            id: Uuid::new_v4().to_string(),
            role: ConversationRole::User,
            full_text: content.clone(),
            revealed_graphemes: 0,
            reveal_credit: 0.0,
            streaming: false,
            tool_name: None,
            tool_details: None,
            is_error: false,
            attachments: attachments.clone(),
        };
        self.workbench.apply(Action::UpsertConversation(item));
        let result = match mode {
            SubmitMode::Prompt => {
                let mut command = json!({"type":"prompt", "message": content});
                if !attachments.is_empty() {
                    command["images"] = Value::Array(attachments.iter().map(|attachment| json!({"type":"image", "data":attachment.base64_data, "mimeType":attachment.mime_type})).collect());
                }
                self.runtime.send(command)
            }
            SubmitMode::Steer => self
                .runtime
                .send(json!({"type":"steer", "message": content})),
            SubmitMode::FollowUp => self
                .runtime
                .send(json!({"type":"follow_up", "message": content})),
        };
        if let Err(error) = result {
            self.error = Some(error.to_string());
        } else {
            self.workbench
                .apply(Action::SetAgentStatus(AgentStatus::Streaming));
            self.ensure_session_title();
        }
    }

    fn ensure_session_title(&mut self) {
        let Some(project_id) = self.workbench.state.selected_project else {
            return;
        };
        let Ok(state) = self.runtime.command(json!({"type":"get_state"})) else {
            return;
        };
        if state.get("sessionName").and_then(Value::as_str).is_some() {
            return;
        }
        let Some(user_message) = self
            .workbench
            .state
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
                .runtime
                .command(json!({"type":"set_session_name", "name": title}))
                .is_ok()
            && let Some(path) = state.get("sessionFile").and_then(Value::as_str)
        {
            self.index_session(project_id, path, Some(&title));
        }
    }

    fn consume_runtime_events(&mut self) {
        let events: Vec<_> = self.runtime_events.try_iter().collect();
        for event in events {
            match event {
                RuntimeEvent::Agent(value) => self.apply_agent_event(value),
                RuntimeEvent::ExtensionUi(value) => self.pending_extension_request = Some(value),
                RuntimeEvent::Stderr(message) => {
                    if !message.trim().is_empty() {
                        self.error = Some(message);
                    }
                }
                RuntimeEvent::Exited { generation, code } => {
                    if generation == self.runtime.generation() {
                        self.workbench
                            .apply(Action::SetAgentStatus(AgentStatus::Failed(format!(
                                "Pi exited: {code:?}"
                            ))));
                    }
                }
                RuntimeEvent::Error(error) => self.error = Some(error),
                RuntimeEvent::RpcResponse(_) => {}
            }
        }
    }

    fn apply_agent_event(&mut self, event: Value) {
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") | Some("message_update") => {
                let message = event.get("message").cloned().unwrap_or(Value::Null);
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    let id = message
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            self.assistant_message_id
                                .clone()
                                .unwrap_or_else(|| Uuid::new_v4().to_string())
                        });
                    if let Some(previous_id) = self.assistant_message_id.replace(id.clone())
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
                            revealed_graphemes: 0,
                            reveal_credit: 0.0,
                            streaming: true,
                            tool_name: None,
                            tool_details: None,
                            is_error: false,
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
                if let Some(id) = self.assistant_message_id.take() {
                    self.workbench.apply(Action::FinishMessage(id));
                }
            }
            Some("tool_execution_start") | Some("tool_execution_end") => {
                self.apply_tool_event(&event)
            }
            Some("queue_update") => {
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
                self.workbench
                    .apply(Action::SetAgentStatus(AgentStatus::Ready));
                if let Some(project_id) = self.workbench.state.selected_project {
                    self.refresh_session_state(project_id);
                }
                self.refresh_runtime_controls();
            }
            Some("session_info_changed") => {
                if let Some(project_id) = self.workbench.state.selected_project
                    && let Ok(state) = self.runtime.command(json!({"type":"get_state"}))
                    && let Some(path) = state.get("sessionFile").and_then(Value::as_str)
                {
                    self.index_session(project_id, path, event.get("name").and_then(Value::as_str));
                }
            }
            Some("thinking_level_changed") => self.refresh_runtime_controls(),
            Some("compaction_start") => self
                .workbench
                .apply(Action::SetAgentStatus(AgentStatus::Compacting)),
            Some("compaction_end") => self
                .workbench
                .apply(Action::SetAgentStatus(AgentStatus::Ready)),
            Some("entry_appended") => {
                if let Some(entry) = event.get("entry") {
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
        let content = event
            .get("result")
            .map(|value| value.to_string())
            .unwrap_or_else(|| format!("{name} is running…"));
        self.workbench
            .apply(Action::UpsertConversation(ConversationItem {
                id,
                role: ConversationRole::Tool,
                full_text: content,
                revealed_graphemes: 0,
                reveal_credit: 0.0,
                streaming: false,
                tool_name: Some(name.into()),
                tool_details: Some(event.to_string()),
                is_error: false,
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
        let text = match role {
            ConversationRole::Assistant => assistant_text(message),
            _ => content_text(message.get("content")).unwrap_or_else(|| message.to_string()),
        };
        self.workbench
            .apply(Action::UpsertConversation(ConversationItem {
                id,
                role,
                full_text: text,
                revealed_graphemes: 0,
                reveal_credit: 0.0,
                streaming: false,
                tool_name: message
                    .get("toolName")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                tool_details: None,
                is_error: message
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                attachments: Vec::new(),
            }));
    }

    fn restart_selected_project(&mut self) {
        if let Some(project) = self.workbench.state.selected_project {
            self.start_project(project);
        }
    }

    fn extension_dialog(&mut self, context: &egui::Context) {
        let Some(request) = self.pending_extension_request.clone() else {
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
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}
fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn pi_agent_directory() -> Result<PathBuf, String> {
    let root = dirs::data_dir()
        .ok_or_else(|| "Application Support directory is unavailable.".to_owned())?
        .join("pi-whim")
        .join("agent");
    Ok(root)
}

fn provider_keychain_account(id: ProviderId) -> String {
    format!("provider-api-key-{id}")
}

fn provider_environment_name(id: ProviderId) -> String {
    format!("PI_WHIM_PROVIDER_{}", id.simple())
}

fn configured_provider_environment(
    profiles: Vec<ProviderProfile>,
    mut get_key: impl FnMut(ProviderId) -> Result<Option<String>, String>,
) -> Result<(Vec<ProviderProfile>, HashMap<String, String>), String> {
    let had_profiles = !profiles.is_empty();
    let mut configured_profiles = Vec::new();
    let mut environment = HashMap::new();
    for profile in profiles {
        if let Some(key) = get_key(profile.id)? {
            environment.insert(provider_environment_name(profile.id), key);
            configured_profiles.push(profile);
        }
    }
    if had_profiles && configured_profiles.is_empty() {
        return Err(
            "No configured provider has an API key in Keychain. Open Settings > Providers, select a provider, and save its API key."
                .into(),
        );
    }
    Ok((configured_profiles, environment))
}

fn normalize_base_url(value: &str) -> String {
    value.trim().trim_end_matches('/').to_owned()
}

/// Pi accepts an environment reference here, keeping API keys out of models.json.
fn pi_models_json(profiles: &[ProviderProfile]) -> Value {
    let providers = profiles
        .iter()
        .map(|profile| {
            let models = profile
                .models
                .iter()
                .map(|model| {
                    json!({
                        "id": model.id,
                        "name": model.name,
                        "reasoning": model.reasoning,
                        "input": if model.supports_images { json!(["text", "image"]) } else { json!(["text"]) },
                        "contextWindow": 128000,
                        "maxTokens": 16384,
                        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
                    })
                })
                .collect::<Vec<_>>();
            (
                provider_config_key(profile.id),
                json!({
                    "name": profile.name,
                    "baseUrl": normalize_base_url(&profile.base_url),
                    "api": profile.protocol.pi_api(),
                    "apiKey": format!("${}", provider_environment_name(profile.id)),
                    "models": models,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({ "providers": providers })
}

fn provider_config_key(id: ProviderId) -> String {
    format!("pi-whim-{}", id.simple())
}

fn discover_models(
    base_url: &str,
    protocol: ProviderProtocol,
    api_key: Option<&str>,
) -> Result<Vec<ProviderModel>, String> {
    let base_url = normalize_base_url(base_url);
    if base_url.is_empty() {
        return Err("Enter a base URL before discovering models.".into());
    }
    let endpoint = match protocol {
        ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => {
            join_api_path(&base_url, "models")
        }
        ProviderProtocol::AnthropicMessages => join_api_path(&base_url, "v1/models"),
        ProviderProtocol::GoogleGenerativeAi => join_api_path(&base_url, "models"),
    };
    let mut request = ureq::get(&endpoint).header("Accept", "application/json");
    if let Some(api_key) = api_key.filter(|key| !key.trim().is_empty()) {
        request = match protocol {
            ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => {
                request.header("Authorization", &format!("Bearer {api_key}"))
            }
            ProviderProtocol::AnthropicMessages => request
                .header("x-api-key", api_key)
                .header("anthropic-version", "2023-06-01"),
            ProviderProtocol::GoogleGenerativeAi => request.header("x-goog-api-key", api_key),
        };
    }
    let mut response = request
        .call()
        .map_err(|error| format!("Model discovery failed: {error}"))?;
    let body: Value = response
        .body_mut()
        .read_json()
        .map_err(|error| format!("Model discovery returned invalid JSON: {error}"))?;
    let candidates = match protocol {
        ProviderProtocol::OpenAiCompletions | ProviderProtocol::OpenAiResponses => body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.get("id").and_then(Value::as_str).map(|id| {
                    let mut model = ProviderModel::new(id);
                    model.name = entry
                        .get("display_name")
                        .or_else(|| entry.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned();
                    model
                })
            })
            .collect(),
        ProviderProtocol::AnthropicMessages => body
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.get("id").and_then(Value::as_str).map(|id| {
                    let mut model = ProviderModel::new(id);
                    model.name = entry
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned();
                    model
                })
            })
            .collect(),
        ProviderProtocol::GoogleGenerativeAi => body
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                entry.get("name").and_then(Value::as_str).map(|id| {
                    let id = id.strip_prefix("models/").unwrap_or(id);
                    let mut model = ProviderModel::new(id);
                    model.name = entry
                        .get("displayName")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned();
                    model.supports_images = entry
                        .get("supportedGenerationMethods")
                        .and_then(Value::as_array)
                        .is_some_and(|methods| {
                            methods.iter().any(|method| method == "generateContent")
                        });
                    model
                })
            })
            .collect(),
    };
    let mut models: Vec<ProviderModel> = candidates;
    models.sort_by_key(|model| model.name.to_lowercase());
    models.dedup_by(|left, right| left.id == right.id);
    Ok(models)
}

fn join_api_path(base_url: &str, suffix: &str) -> String {
    let base_url = base_url.trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/');
    if base_url.ends_with("/v1") && suffix.starts_with("v1/") {
        format!("{base_url}/{}", suffix.trim_start_matches("v1/"))
    } else {
        format!("{base_url}/{suffix}")
    }
}
fn stable_session_id(path: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_URL, path.as_bytes())
}

fn model_option(value: &Value) -> Option<ModelOption> {
    Some(ModelOption {
        provider: value.get("provider")?.as_str()?.into(),
        id: value.get("id")?.as_str()?.into(),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_else(|| value.get("id").and_then(Value::as_str).unwrap_or("model"))
            .into(),
    })
}

fn queue_mode(value: &str) -> QueueMode {
    match value {
        "all" => QueueMode::All,
        _ => QueueMode::OneAtATime,
    }
}

fn queue_mode_name(mode: QueueMode) -> &'static str {
    match mode {
        QueueMode::All => "all",
        QueueMode::OneAtATime => "one-at-a-time",
    }
}

fn session_metrics(value: &Value) -> SessionMetrics {
    let number = |key| value.get(key).and_then(Value::as_u64).unwrap_or_default();
    let cost_microusd = value
        .get("cost")
        .and_then(Value::as_f64)
        .filter(|cost| *cost >= 0.0)
        .map(|cost| (cost * 1_000_000.0).round() as u64)
        .unwrap_or_default();
    SessionMetrics {
        total_messages: number("totalMessages"),
        user_messages: number("userMessages"),
        assistant_messages: number("assistantMessages"),
        tool_calls: number("toolCalls"),
        total_tokens: value
            .get("tokens")
            .and_then(|tokens| tokens.get("total"))
            .and_then(Value::as_u64)
            .unwrap_or_default(),
        cost_microusd,
    }
}
fn content_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(value) => Some(value.clone()),
        Value::Array(items) => Some(
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<String>(),
        ),
        _ => None,
    }
}
fn assistant_text(message: &Value) -> String {
    content_text(message.get("content")).unwrap_or_default()
}

fn image_attachment(path: &Path) -> Result<ImageAttachment, String> {
    let mime_type = match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => return Err("Unsupported image format".into()),
    };
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() > 10 * 1024 * 1024 {
        return Err("Images must be 10 MB or smaller".into());
    }
    Ok(ImageAttachment {
        name: path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("image")
            .into(),
        mime_type: mime_type.into(),
        base64_data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

fn session_summary_from_jsonl(project_id: ProjectId, path: &Path) -> Option<SessionSummary> {
    let contents = fs::read_to_string(path).ok()?;
    let (title, preview, has_user_message) = session_title_and_preview(&contents);
    if !has_user_message {
        return None;
    }
    let pi_path = path.to_string_lossy().into_owned();
    let title = title.unwrap_or_else(|| {
        if preview.trim().is_empty() {
            "Image conversation".into()
        } else {
            preview.chars().take(52).collect()
        }
    });
    let updated_at_ms = path
        .metadata()
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some(SessionSummary {
        id: stable_session_id(&pi_path),
        project_id,
        pi_path,
        title,
        preview,
        updated_at_ms,
    })
}

fn session_title_and_preview(contents: &str) -> (Option<String>, String, bool) {
    let mut title = None;
    let mut preview = String::new();
    let mut has_user_message = false;
    for line in contents.lines() {
        // A Pi process can be interrupted while appending JSONL. Preserve the
        // usable history instead of hiding the entire session because of one tail line.
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) == Some("session_info") {
            title = entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
        }
        let is_user_message = entry
            .get("message")
            .and_then(|message| message.get("role"))
            .and_then(Value::as_str)
            == Some("user");
        has_user_message |= is_user_message;
        if preview.is_empty() && is_user_message {
            preview = content_text(
                entry
                    .get("message")
                    .and_then(|message| message.get("content")),
            )
            .unwrap_or_default();
        }
    }
    (title, preview, has_user_message)
}

fn applescript_escape(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

fn bash_policy_name(policy: &BashPolicy) -> &'static str {
    match policy {
        BashPolicy::Allow => "allow",
        BashPolicy::Ask => "ask",
        BashPolicy::Deny => "deny",
    }
}

fn ensure_bash_extension(sessions_path: &Path) -> std::io::Result<PathBuf> {
    let extension_path = sessions_path.join("pi-whim-bash-policy.ts");
    if !extension_path.exists() {
        fs::write(&extension_path, BASH_EXTENSION_SOURCE)?;
    }
    Ok(extension_path)
}

const BASH_EXTENSION_SOURCE: &str = r#"import type { ExtensionAPI } from '@earendil-works/pi-coding-agent';

export default function(pi: ExtensionAPI) {
  pi.on('tool_call', async (event, ctx) => {
    if (event.toolName !== 'bash') return;
    const policy = process.env.PI_WHIM_BASH_POLICY ?? 'allow';
    if (policy === 'deny') return { block: true, reason: 'Blocked by Pi-Whim Bash policy' };
    if (policy === 'ask') {
      const command = String(event.input.command ?? '');
      const allowed = await ctx.ui.confirm('Pi-Whim Bash confirmation', command);
      if (!allowed) return { block: true, reason: 'Blocked by user' };
    }
  });
}
"#;

#[cfg(test)]
mod tests {
    use super::{
        configured_provider_environment, pi_models_json, provider_environment_name,
        session_title_and_preview,
    };
    use pi_whim_core::{ProviderModel, ProviderProfile, ProviderProtocol};
    use uuid::Uuid;

    #[test]
    fn history_uses_the_latest_pi_session_info_title() {
        let history = r#"
{"type":"message","message":{"role":"user","content":"first prompt"}}
{"type":"session_info","name":"Initial title"}
{"type":"message","message":{"role":"assistant","content":"reply"}}
{"type":"session_info","name":"  中文会话标题  "}
"#;

        let (title, preview, has_user_message) = session_title_and_preview(history);

        assert_eq!(title.as_deref(), Some("中文会话标题"));
        assert_eq!(preview, "first prompt");
        assert!(has_user_message);
    }

    #[test]
    fn history_skips_an_incomplete_jsonl_tail() {
        let history = r#"
{"type":"message","message":{"role":"user","content":"hello"}}
{"type":"session_info","name":"Named session"}
{"type":"message"
"#;

        let (title, preview, has_user_message) = session_title_and_preview(history);

        assert_eq!(title.as_deref(), Some("Named session"));
        assert_eq!(preview, "hello");
        assert!(has_user_message);
    }

    #[test]
    fn metadata_only_history_is_not_a_persisted_conversation() {
        let history = r#"
{"type":"session","id":"session-id","timestamp":"2026-07-22T12:00:00Z"}
{"type":"session_info","name":"New session"}
{"type":"model_change","provider":"example","modelId":"gpt-example"}
"#;

        let (title, preview, has_user_message) = session_title_and_preview(history);

        assert_eq!(title.as_deref(), Some("New session"));
        assert!(preview.is_empty());
        assert!(!has_user_message);
    }

    #[test]
    fn generated_pi_models_config_only_references_a_key_environment_variable() {
        let profile = ProviderProfile {
            id: Uuid::new_v4(),
            name: "Private gateway".into(),
            base_url: "https://gateway.example/v1/".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("gpt-example")],
            updated_at_ms: 1,
            has_api_key: true,
        };
        let config = pi_models_json(std::slice::from_ref(&profile));
        let provider = config["providers"]
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap();
        assert_eq!(provider["baseUrl"], "https://gateway.example/v1");
        assert_eq!(
            provider["apiKey"],
            format!("${}", provider_environment_name(profile.id))
        );
        assert!(!config.to_string().contains("sk-"));
    }

    #[test]
    fn provider_without_a_key_does_not_block_a_configured_provider() {
        let missing_id = Uuid::new_v4();
        let configured_id = Uuid::new_v4();
        let profile = |id| ProviderProfile {
            id,
            name: "Private gateway".into(),
            base_url: "https://gateway.example/v1".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("gpt-example")],
            updated_at_ms: 1,
            has_api_key: id == configured_id,
        };

        let (profiles, environment) = configured_provider_environment(
            vec![profile(missing_id), profile(configured_id)],
            |id| Ok((id == configured_id).then(|| "secret-key".to_owned())),
        )
        .unwrap();

        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, configured_id);
        assert_eq!(
            environment.get(&provider_environment_name(configured_id)),
            Some(&"secret-key".to_owned())
        );
        assert!(!environment.contains_key(&provider_environment_name(missing_id)));
    }
}
