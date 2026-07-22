//! UI-facing agent boundary. The egui crate never manages Pi processes directly.

use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
    thread,
    time::Duration,
};

use crossbeam_channel::{Receiver, Sender, unbounded};
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
}

#[derive(Clone, Debug)]
pub struct RuntimeStart {
    pub project_path: String,
    pub sessions_path: String,
    pub extension_path: Option<String>,
    pub environment: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum RuntimeEvent {
    Agent(Value),
    RpcResponse(Value),
    ExtensionUi(Value),
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
        let mut launch = PiLaunch::new(executable.to_string_lossy(), &config.project_path);
        launch
            .arguments
            .extend(["--session-dir".into(), config.sessions_path]);
        if let Some(extension_path) = config.extension_path {
            launch
                .arguments
                .extend(["--extension".into(), extension_path]);
        }
        launch.environment = config.environment;
        let client = Arc::new(PiRpcClient::launch(launch)?);
        self.forward_events(client.clone(), self.generation);
        self.client = Some(client);
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

    fn events(&self) -> Receiver<RuntimeEvent> {
        self.event_receiver.clone()
    }

    fn stop(&mut self) -> Result<(), RuntimeError> {
        if let Some(client) = self.client.take() {
            client.stop()?;
        }
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
}

impl Default for FakeRuntime {
    fn default() -> Self {
        let (sender, receiver) = unbounded();
        Self {
            sender,
            receiver,
            prompts: Vec::new(),
        }
    }
}

impl AgentRuntime for FakeRuntime {
    fn start(&mut self, _config: RuntimeStart) -> Result<(), RuntimeError> {
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
    fn command(&self, _command: Value) -> Result<Value, RuntimeError> {
        Ok(Value::Null)
    }
    fn send(&self, _command: Value) -> Result<(), RuntimeError> {
        Ok(())
    }
    fn respond_extension_ui(&self, _response: Value) -> Result<(), RuntimeError> {
        Ok(())
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
