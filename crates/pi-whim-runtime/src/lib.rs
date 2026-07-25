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
use pi_whim_core::{AgentTeamConfig, SearchEngineProfile};
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
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Agent(Value),
    RpcResponse(Value),
    ExtensionUi(Value),
    Interaction(Value),
    Stderr(String),
    Exited { generation: u64, code: Option<i32> },
    Error(String),
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
    fn events(&self) -> Receiver<RuntimeEvent>;
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
        let mut supervisor = AgentSupervisor::start(AgentLaunchConfig {
            executable: executable.clone(),
            project_path: PathBuf::from(&config.project_path),
            sessions_path: PathBuf::from(&config.sessions_path),
            extension_paths: extension_paths.clone(),
            environment: config.environment.clone(),
            team_config: config.agent_team_config,
            search_engines: config.search_engines,
        })
        .map_err(|error| RuntimeError::AgentSupervisor(error.to_string()))?;
        if let Some(interactions) = supervisor.take_interaction_events() {
            self.forward_interactions(interactions);
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
            .request(command, Duration::from_secs(20))?;
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

    fn events(&self) -> Receiver<RuntimeEvent> {
        self.event_receiver.clone()
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
        }
    }
}

impl FakeRuntime {
    pub fn commands(&self) -> Vec<Value> {
        self.commands.lock().expect("fake runtime commands").clone()
    }

    pub fn starts(&self) -> Vec<RuntimeStart> {
        self.starts.lock().expect("fake runtime starts").clone()
    }

    pub fn set_response(&self, command_type: &str, response: Value) {
        self.responses
            .lock()
            .expect("fake runtime responses")
            .insert(command_type.to_owned(), response);
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
            .unwrap_or_else(|| match command_type.as_str() {
                "get_state" => json!({}),
                "get_available_models" => json!({"models": []}),
                "get_available_thinking_levels" => json!({"levels": ["off"]}),
                "get_session_stats" => json!({}),
                _ => Value::Null,
            }))
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
    fn events(&self) -> Receiver<RuntimeEvent> {
        self.receiver.clone()
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
