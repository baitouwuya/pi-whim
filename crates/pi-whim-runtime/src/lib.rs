//! UI-facing agent boundary. The egui crate never manages Pi processes directly.

use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use pi_whim_agent_team::{AgentLaunchConfig, AgentSupervisor};
pub use pi_whim_agent_team::{SearchEngineApiKeys, test_search_engine};
use pi_whim_core::{
    AgentPermissionLevel, AgentTeamConfig, HookAuditRecord, HookConfig, SearchEngineProfile,
    ToolAuditRecord,
};
use pi_whim_hook_host::{HookHostManager, HookScopeHandle, HookScopeKey};
use pi_whim_pi_rpc::{PiLaunch, PiRpcClient, PiRpcEvent, RpcError};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("RPC error: {0}")]
    Rpc(#[from] RpcError),
    #[error(
        "Pi executable was not found; run `cargo run -p xtask -- pi-build` or set PI_WHIM_PI_BIN"
    )]
    PiUnavailable,
    #[error("agent supervisor error: {0}")]
    AgentSupervisor(String),
}

/// Shared, caller-created project Hook scope reused by one or more runtimes.
///
/// Runtime only transports these handles. Scope creation, manifests, approval,
/// and registry policy remain owned by the caller and `pi-whim-hook-host`.
#[derive(Clone)]
pub struct RuntimeHookScope {
    manager: HookHostManager,
    scope: HookScopeHandle,
}

impl RuntimeHookScope {
    pub fn new(manager: HookHostManager, scope: HookScopeHandle) -> Self {
        Self { manager, scope }
    }

    pub fn scope_id(&self) -> String {
        self.scope.scope_id()
    }

    pub fn key(&self) -> HookScopeKey {
        self.scope.key()
    }

    fn into_parts(self) -> (HookHostManager, HookScopeHandle) {
        (self.manager, self.scope)
    }
}

impl std::fmt::Debug for RuntimeHookScope {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeHookScope")
            .field("scope_id", &self.scope.scope_id())
            .field("key", &self.scope.key())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeStart {
    pub project_path: String,
    pub sessions_path: String,
    /// Session file Pi opens on launch. `None` starts a fresh session. Each
    /// session gets its own Pi process so parallel sessions never abort each
    /// other through Pi's single-session `switch_session` RPC.
    pub session_path: Option<String>,
    pub extension_paths: Vec<String>,
    pub environment: HashMap<String, String>,
    pub agent_team_config: AgentTeamConfig,
    pub search_engines: Vec<SearchEngineProfile>,
    pub search_engine_api_keys: SearchEngineApiKeys,
    pub hooks: HookConfig,
    /// Optional shared project Hook scope. When present, the runtime reuses
    /// its manager and scope instead of constructing the v1 compatibility scope.
    pub hook_scope: Option<RuntimeHookScope>,
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Agent(Value),
    RpcResponse(Value),
    ExtensionUi(Value),
    Interaction(Value),
    HookAudit(HookAuditRecord),
    ToolAudit(ToolAuditRecord),
    Stderr(String),
    Exited { generation: u64, code: Option<i32> },
    Error(String),
}

/// Issues commands to a running agent from any thread.
///
/// `AgentRuntime` is `Send` but not `Sync`, so it cannot be shared with a
/// worker. The parts needed to make a request are shareable, though, and this
/// hands out just those — which is what lets the app fetch runtime state
/// without blocking the frame it is drawing.
/// The RPC client is `Send + Sync`, so it can serve as its own commander.
impl RuntimeCommander for PiRpcClient {
    fn command(&self, command: Value) -> Result<Value, RuntimeError> {
        let response = self.request(command, COMMAND_TIMEOUT)?;
        Ok(response.data.unwrap_or(Value::Null))
    }
}

/// How long a command waits for Pi to answer.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub trait RuntimeCommander: Send + Sync {
    fn command(&self, command: Value) -> Result<Value, RuntimeError>;
}

pub trait AgentRuntime: Send {
    fn start(&mut self, config: RuntimeStart) -> Result<(), RuntimeError>;
    fn send_prompt(
        &self,
        message: String,
        behavior: Option<StreamingBehavior>,
    ) -> Result<(), RuntimeError>;
    fn command(&self, command: Value) -> Result<Value, RuntimeError>;
    fn send(&self, command: Value) -> Result<(), RuntimeError>;
    fn respond_extension_ui(&self, response: Value) -> Result<(), RuntimeError>;
    fn resolve_user_interaction(
        &self,
        request_id: String,
        decision: String,
    ) -> Result<Value, RuntimeError>;
    /// Update the default policy for agents spawned after this call.
    fn set_default_permission_level(&self, level: AgentPermissionLevel)
    -> Result<(), RuntimeError>;
    fn events(&self) -> Receiver<RuntimeEvent>;
    /// A handle for issuing commands off the calling thread, if the agent is
    /// running.
    fn commander(&self) -> Option<Arc<dyn RuntimeCommander>>;
    fn stop(&mut self) -> Result<(), RuntimeError>;
    fn generation(&self) -> u64;
}

#[derive(Clone, Copy, Debug)]
pub enum StreamingBehavior {
    Steer,
    FollowUp,
}

impl StreamingBehavior {
    fn as_rpc_value(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::FollowUp => "followUp",
        }
    }
}

pub struct PiRpcRuntime {
    executable: Option<PathBuf>,
    client: Option<Arc<PiRpcClient>>,
    event_sender: Sender<RuntimeEvent>,
    event_receiver: Receiver<RuntimeEvent>,
    generation: u64,
    agent_supervisor: Option<AgentSupervisor>,
}

impl Default for PiRpcRuntime {
    fn default() -> Self {
        let (event_sender, event_receiver) = unbounded();
        Self {
            executable: None,
            client: None,
            event_sender,
            event_receiver,
            generation: 0,
            agent_supervisor: None,
        }
    }
}

impl PiRpcRuntime {
    pub fn with_executable(executable: PathBuf) -> Self {
        Self {
            executable: Some(executable),
            ..Self::default()
        }
    }

    pub fn locate_pi() -> Result<PathBuf, RuntimeError> {
        if let Ok(resource_path) = env::var("PI_WHIM_BUNDLED_PI") {
            let path = PathBuf::from(resource_path);
            if path.is_file() {
                return Ok(path);
            }
        }
        if let Some(path) = env::var_os("PI_WHIM_PI_BIN").map(PathBuf::from)
            && path.is_file()
        {
            return Ok(path);
        }
        if let Ok(current_executable) = env::current_exe()
            && let Some(macos_directory) = current_executable.parent()
        {
            let bundled = macos_directory.join("../Resources/pi/pi");
            if bundled.is_file() {
                return Ok(bundled);
            }
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let candidates = [
            root.join("vendor/pi-mono/packages/coding-agent/binaries/darwin-arm64/pi"),
            root.join("vendor/pi-mono/packages/coding-agent/binaries/pi"),
        ];
        candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
            .ok_or(RuntimeError::PiUnavailable)
    }

    /// Each Pi launch owns a generation so a stopped process cannot mark its
    /// replacement as failed after a restart.
    fn forward_events(&self, client: Arc<PiRpcClient>, generation: u64) {
        let sender = self.event_sender.clone();
        thread::spawn(move || {
            for event in client.events().iter() {
                let forwarded = match event {
                    PiRpcEvent::Event(value)
                        if value.get("type").and_then(Value::as_str)
                            == Some("extension_ui_request") =>
                    {
                        RuntimeEvent::ExtensionUi(value)
                    }
                    PiRpcEvent::Event(value) => RuntimeEvent::Agent(value),
                    PiRpcEvent::Response(response) => RuntimeEvent::RpcResponse(
                        serde_json::to_value(response).unwrap_or(Value::Null),
                    ),
                    PiRpcEvent::Stderr(message) => RuntimeEvent::Stderr(message),
                    PiRpcEvent::ProcessExited { code } => RuntimeEvent::Exited { code, generation },
                    PiRpcEvent::MalformedFrame(message) => RuntimeEvent::Error(message),
                };
                let _ = sender.send(forwarded);
            }
        });
    }

    fn forward_interactions(&self, receiver: std::sync::mpsc::Receiver<Value>) {
        let sender = self.event_sender.clone();
        thread::spawn(move || {
            for interaction in receiver {
                if sender.send(RuntimeEvent::Interaction(interaction)).is_err() {
                    break;
                }
            }
        });
    }

    fn forward_hook_audit(&self, receiver: std::sync::mpsc::Receiver<HookAuditRecord>) {
        let sender = self.event_sender.clone();
        thread::spawn(move || {
            for record in receiver {
                if sender.send(RuntimeEvent::HookAudit(record)).is_err() {
                    break;
                }
            }
        });
    }

    fn forward_tool_audit(&self, receiver: std::sync::mpsc::Receiver<ToolAuditRecord>) {
        let sender = self.event_sender.clone();
        thread::spawn(move || {
            for record in receiver {
                if sender.send(RuntimeEvent::ToolAudit(record)).is_err() {
                    break;
                }
            }
        });
    }

    fn advance_generation(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

impl AgentRuntime for PiRpcRuntime {
    fn start(&mut self, config: RuntimeStart) -> Result<(), RuntimeError> {
        self.stop()?;
        self.advance_generation();
        let executable = match self.executable.clone() {
            Some(executable) => executable,
            None => Self::locate_pi()?,
        };
        let extension_paths: Vec<_> = config.extension_paths.iter().map(PathBuf::from).collect();
        let supervisor_launch = AgentLaunchConfig {
            executable: executable.clone(),
            project_path: PathBuf::from(&config.project_path),
            sessions_path: PathBuf::from(&config.sessions_path),
            extension_paths: extension_paths.clone(),
            environment: config.environment.clone(),
            team_config: config.agent_team_config,
            search_engines: config.search_engines,
            search_engine_api_keys: config.search_engine_api_keys,
            hooks: config.hooks,
        };
        let mut supervisor = match config.hook_scope {
            Some(hook_scope) => {
                let (manager, scope) = hook_scope.into_parts();
                AgentSupervisor::start_with_hook_scope(supervisor_launch, manager, scope)
            }
            None => AgentSupervisor::start(supervisor_launch),
        }
        .map_err(|error| RuntimeError::AgentSupervisor(error.to_string()))?;
        if let Some(interactions) = supervisor.take_interaction_events() {
            self.forward_interactions(interactions);
        }
        if let Some(audit) = supervisor.take_hook_audit_events() {
            self.forward_hook_audit(audit);
        }
        if let Some(tool_audit) = supervisor.take_tool_audit_events() {
            self.forward_tool_audit(tool_audit);
        }
        let mut launch = PiLaunch::new(executable.to_string_lossy(), &config.project_path);
        launch
            .arguments
            .extend(["--session-dir".into(), config.sessions_path]);
        if let Some(session_path) = &config.session_path {
            launch
                .arguments
                .extend(["--session".into(), session_path.clone()]);
        }
        for extension_path in config.extension_paths {
            launch
                .arguments
                .extend(["--extension".into(), extension_path]);
        }
        launch.environment = config.environment;
        launch.environment.extend(supervisor.root_environment());
        let client = match PiRpcClient::launch(launch) {
            Ok(client) => Arc::new(client),
            Err(error) => {
                drop(supervisor);
                return Err(error.into());
            }
        };
        self.forward_events(client.clone(), self.generation);
        self.client = Some(client);
        self.agent_supervisor = Some(supervisor);
        Ok(())
    }

    fn send_prompt(
        &self,
        message: String,
        behavior: Option<StreamingBehavior>,
    ) -> Result<(), RuntimeError> {
        let mut command = json!({"type":"prompt", "message": message});
        if let Some(behavior) = behavior {
            command["streamingBehavior"] = Value::String(behavior.as_rpc_value().into());
        }
        self.client
            .as_ref()
            .ok_or(RpcError::Unavailable)?
            .send(command)?;
        Ok(())
    }

    fn command(&self, command: Value) -> Result<Value, RuntimeError> {
        let response = self
            .client
            .as_ref()
            .ok_or(RpcError::Unavailable)?
            .request(command, COMMAND_TIMEOUT)?;
        Ok(response.data.unwrap_or(Value::Null))
    }

    fn send(&self, command: Value) -> Result<(), RuntimeError> {
        self.client
            .as_ref()
            .ok_or(RpcError::Unavailable)?
            .send(command)?;
        Ok(())
    }

    fn respond_extension_ui(&self, response: Value) -> Result<(), RuntimeError> {
        self.client
            .as_ref()
            .ok_or(RpcError::Unavailable)?
            .send(response)?;
        Ok(())
    }

    fn resolve_user_interaction(
        &self,
        request_id: String,
        decision: String,
    ) -> Result<Value, RuntimeError> {
        self.agent_supervisor
            .as_ref()
            .ok_or_else(|| RuntimeError::AgentSupervisor("agent supervisor is unavailable".into()))?
            .resolve_user_interaction(&request_id, &decision)
            .map_err(RuntimeError::AgentSupervisor)
    }

    fn set_default_permission_level(
        &self,
        level: AgentPermissionLevel,
    ) -> Result<(), RuntimeError> {
        self.agent_supervisor
            .as_ref()
            .ok_or_else(|| RuntimeError::AgentSupervisor("agent supervisor is unavailable".into()))?
            .set_default_permission_level(level)
            .map_err(|error| RuntimeError::AgentSupervisor(error.to_string()))
    }

    fn events(&self) -> Receiver<RuntimeEvent> {
        self.event_receiver.clone()
    }

    fn commander(&self) -> Option<Arc<dyn RuntimeCommander>> {
        self.client
            .as_ref()
            .map(|client| client.clone() as Arc<dyn RuntimeCommander>)
    }

    fn stop(&mut self) -> Result<(), RuntimeError> {
        let client_result = self.client.take().map(|client| client.stop()).transpose();
        let supervisor_result = self
            .agent_supervisor
            .take()
            .map(|mut supervisor| {
                supervisor
                    .stop()
                    .map_err(|error| RuntimeError::AgentSupervisor(error.to_string()))
            })
            .transpose();
        client_result?;
        supervisor_result?;
        Ok(())
    }

    fn generation(&self) -> u64 {
        self.generation
    }
}

pub struct FakeRuntime {
    sender: Sender<RuntimeEvent>,
    receiver: Receiver<RuntimeEvent>,
    pub prompts: Vec<String>,
    commands: Arc<Mutex<Vec<Value>>>,
    starts: Arc<Mutex<Vec<RuntimeStart>>>,
    responses: Arc<Mutex<HashMap<String, Value>>>,
    permission_levels: Arc<Mutex<Vec<AgentPermissionLevel>>>,
}

impl Clone for FakeRuntime {
    /// Clones share the recorded commands/starts/responses so a test observer
    /// sees every pooled runtime, but each clone owns an independent event
    /// channel so per-session event routing stays isolated.
    fn clone(&self) -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            prompts: self.prompts.clone(),
            commands: self.commands.clone(),
            starts: self.starts.clone(),
            responses: self.responses.clone(),
            permission_levels: self.permission_levels.clone(),
        }
    }
}

impl Default for FakeRuntime {
    fn default() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            prompts: Vec::new(),
            commands: Arc::new(Mutex::new(Vec::new())),
            starts: Arc::new(Mutex::new(Vec::new())),
            responses: Arc::new(Mutex::new(HashMap::new())),
            permission_levels: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl FakeRuntime {
    fn fake_commander(&self) -> FakeCommander {
        FakeCommander {
            commands: self.commands.clone(),
            responses: self.responses.clone(),
        }
    }
    pub fn commands(&self) -> Vec<Value> {
        self.commands.lock().expect("fake runtime commands").clone()
    }

    pub fn starts(&self) -> Vec<RuntimeStart> {
        self.starts.lock().expect("fake runtime starts").clone()
    }

    pub fn permission_levels(&self) -> Vec<AgentPermissionLevel> {
        self.permission_levels
            .lock()
            .expect("fake runtime permission levels")
            .clone()
    }

    pub fn set_response(&self, command_type: &str, response: Value) {
        self.responses
            .lock()
            .expect("fake runtime responses")
            .insert(command_type.to_owned(), response);
    }
}

/// Shared recording state behind a [`FakeRuntime`], so tests can issue commands
/// from a worker exactly as the real client allows.
#[derive(Clone, Default)]
pub struct FakeCommander {
    commands: Arc<Mutex<Vec<Value>>>,
    responses: Arc<Mutex<HashMap<String, Value>>>,
}

impl RuntimeCommander for FakeCommander {
    fn command(&self, command: Value) -> Result<Value, RuntimeError> {
        let command_type = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.commands
            .lock()
            .expect("fake runtime commands")
            .push(command);
        Ok(self
            .responses
            .lock()
            .expect("fake runtime responses")
            .get(&command_type)
            .cloned()
            .unwrap_or_else(|| default_response(&command_type)))
    }
}

/// Stand-in responses for the commands the app issues on every refresh, so a
/// test that does not care about them need not stub each one.
fn default_response(command_type: &str) -> Value {
    match command_type {
        "get_state" => json!({}),
        "get_available_models" => json!({"models": []}),
        "get_available_thinking_levels" => json!({"levels": ["off"]}),
        "get_session_stats" => json!({}),
        _ => Value::Null,
    }
}

impl AgentRuntime for FakeRuntime {
    fn start(&mut self, config: RuntimeStart) -> Result<(), RuntimeError> {
        self.starts
            .lock()
            .expect("fake runtime starts")
            .push(config);
        let _ = self
            .sender
            .send(RuntimeEvent::Agent(json!({"type":"agent_ready"})));
        Ok(())
    }
    fn send_prompt(
        &self,
        _message: String,
        _behavior: Option<StreamingBehavior>,
    ) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn command(&self, command: Value) -> Result<Value, RuntimeError> {
        self.fake_commander().command(command)
    }
    fn send(&self, command: Value) -> Result<(), RuntimeError> {
        self.commands
            .lock()
            .expect("fake runtime commands")
            .push(command);
        Ok(())
    }
    fn respond_extension_ui(&self, _response: Value) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn resolve_user_interaction(
        &self,
        request_id: String,
        decision: String,
    ) -> Result<Value, RuntimeError> {
        let command = json!({
            "type": "resolve_user_interaction",
            "request_id": request_id,
            "decision": decision,
        });
        self.commands
            .lock()
            .expect("fake runtime commands")
            .push(command);
        Ok(Value::Null)
    }
    fn set_default_permission_level(
        &self,
        level: AgentPermissionLevel,
    ) -> Result<(), RuntimeError> {
        self.permission_levels
            .lock()
            .expect("fake runtime permission levels")
            .push(level);
        Ok(())
    }
    fn events(&self) -> Receiver<RuntimeEvent> {
        self.receiver.clone()
    }

    fn commander(&self) -> Option<Arc<dyn RuntimeCommander>> {
        Some(Arc::new(self.fake_commander()))
    }
    fn stop(&mut self) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn generation(&self) -> u64 {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared_scope(project_path: &Path) -> RuntimeHookScope {
        let manager = HookHostManager::empty().unwrap();
        let key = HookScopeKey::project(project_path, "runtime-test").unwrap();
        let scope = manager.open_scope(key, None).unwrap();
        RuntimeHookScope::new(manager, scope)
    }

    fn runtime_start(project_path: &Path, hook_scope: Option<RuntimeHookScope>) -> RuntimeStart {
        RuntimeStart {
            project_path: project_path.to_string_lossy().into_owned(),
            sessions_path: project_path.to_string_lossy().into_owned(),
            session_path: None,
            extension_paths: Vec::new(),
            environment: HashMap::new(),
            agent_team_config: AgentTeamConfig::default(),
            search_engines: Vec::new(),
            search_engine_api_keys: SearchEngineApiKeys::default(),
            hooks: HookConfig::default(),
            hook_scope,
        }
    }

    #[test]
    fn runtime_hook_scope_clone_and_debug_expose_only_scope_metadata() {
        let scope = shared_scope(&env::temp_dir());
        let cloned = scope.clone();
        assert_eq!(cloned.scope_id(), scope.scope_id());
        assert_eq!(cloned.key(), scope.key());
        let debug = format!("{scope:?}");
        assert!(debug.contains("RuntimeHookScope"));
        assert!(debug.contains(&scope.scope_id()));
        assert!(!debug.contains("HookHostManager"));
    }

    #[test]
    fn runtime_start_clone_and_fake_runtime_preserve_shared_scope() {
        let scope = shared_scope(&env::temp_dir());
        let expected_scope_id = scope.scope_id();
        let start = runtime_start(&env::temp_dir(), Some(scope));
        let cloned = start.clone();
        assert_eq!(
            cloned.hook_scope.as_ref().map(RuntimeHookScope::scope_id),
            Some(expected_scope_id.clone())
        );

        let mut runtime = FakeRuntime::default();
        runtime.start(cloned).unwrap();
        let starts = runtime.starts();
        assert_eq!(starts.len(), 1);
        assert_eq!(
            starts[0]
                .hook_scope
                .as_ref()
                .map(RuntimeHookScope::scope_id),
            Some(expected_scope_id)
        );
    }

    #[test]
    fn pi_runtime_uses_shared_scope_branch_when_present() {
        let scope = shared_scope(&env::temp_dir());
        scope.scope.revoke();
        let mut runtime = PiRpcRuntime::with_executable(PathBuf::from("/usr/bin/false"));
        let error = runtime
            .start(runtime_start(&env::temp_dir(), Some(scope)))
            .unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::AgentSupervisor(message) if message.contains("revoked")
        ));
    }

    #[test]
    fn pi_runtime_preserves_legacy_v1_path_without_scope() {
        let mut start = runtime_start(&env::temp_dir(), None);
        start.hooks.version = 2;
        let mut runtime = PiRpcRuntime::with_executable(PathBuf::from("/usr/bin/false"));
        let error = runtime.start(start).unwrap_err();
        assert!(matches!(
            error,
            RuntimeError::AgentSupervisor(message)
                if message.contains("unsupported hook manifest version 2")
        ));
    }

    #[test]
    fn launch_generation_advances_after_a_restart() {
        let mut runtime = PiRpcRuntime::default();
        assert_eq!(runtime.generation(), 0);
        runtime.advance_generation();
        assert_eq!(runtime.generation(), 1);
        runtime.advance_generation();
        assert_eq!(runtime.generation(), 2);
    }
}
