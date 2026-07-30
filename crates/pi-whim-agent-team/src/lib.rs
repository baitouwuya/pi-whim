//! Authenticated local supervisor for hierarchical Pi agent teams.

mod bash_dispatch;
mod capture;
mod catalog;
mod fetch;
mod file_compression;
mod file_dispatch;
mod image_compression;
mod model;
mod process;
mod routing;
mod session_read;
mod web_search;

pub use web_search::test_engine as test_search_engine;

use std::{
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use model::{
    AgentDescriptor, AgentId, AgentMessage, AgentNode, AgentOutcome, AgentSessionEntry,
    AgentStatus, ApprovalRequestArguments, AskUserArguments, BashArguments, ListAgentsArguments,
    ListSessionsArguments, ProcessCommand, ProcessIdArguments, ReadSessionArguments,
    ResetTeamArguments, SearchSessionsArguments, SendMessageArguments, SessionSnapshot,
    SpawnAgentArguments, TargetArguments, TeamState, WaitAgentArguments,
};
use pi_whim_core::{
    AgentModelSelection, AgentPermissionLevel, AgentPermissionPolicy, AgentTeamConfig,
    SearchEngineId, SearchEngineProfile, normalize_agent_policy, stable_session_id,
};
use pi_whim_tool_protocol::{
    ASK_USER_TOOL, BASH_TOOL, EDIT_FILE_TOOL, FETCH_TOOL, INTERRUPT_AGENT_TOOL, LIST_AGENTS_TOOL,
    LIST_AVAILABLE_MODELS_TOOL, LIST_PENDING_REQUESTS_TOOL, LIST_PROCESSES_TOOL,
    LIST_SESSIONS_TOOL, PROTOCOL_VERSION, READ_FILE_TOOL, READ_MESSAGES_TOOL, READ_PROCESS_TOOL,
    READ_SESSION_TOOL, RESET_TEAM_TOOL, RESOLVE_INTERACTION_TOOL, SEARCH_SESSIONS_TOOL,
    SEND_MESSAGE_TOOL, SPAWN_AGENT_TOOL, STOP_PROCESS_TOOL, ToolRequest, ToolResponse,
    WAIT_AGENT_TOOL, WEB_SEARCH_TOOL, WRITE_FILE_TOOL,
};
use routing::{
    RoutingError, is_direct_child, message_kind, message_kind_for_descriptors,
    resolve_visible_target, validate_child, visible_agent_ids,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;
use uuid::Uuid;

pub const HOST_ENV: &str = "PI_WHIM_AGENT_HOST";
pub const CAPABILITY_ENV: &str = "PI_WHIM_AGENT_CAPABILITY";
pub const AGENT_ID_ENV: &str = "PI_WHIM_AGENT_ID";
pub const AGENT_LEVEL_ENV: &str = "PI_WHIM_AGENT_LEVEL";
const MAX_INBOX_MESSAGES: usize = 256;
const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_SESSION_ENTRIES: usize = 64;
pub(crate) const MAX_SESSION_CONTENT_BYTES: usize = 16 * 1024;
const APPROVAL_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum InteractionKind {
    Approval,
    Question,
}

#[derive(Clone, Debug)]
struct PendingInteraction {
    id: Uuid,
    kind: InteractionKind,
    requester_id: AgentId,
    owner_id: AgentId,
    title: String,
    message: String,
    options: Vec<String>,
    default_option: Option<String>,
    operation_hash: Option<String>,
    created_at: Instant,
    response: Option<String>,
}

struct NewInteraction {
    kind: InteractionKind,
    title: String,
    message: String,
    options: Vec<String>,
    default_option: Option<String>,
    operation_hash: Option<String>,
    high_risk: bool,
}

pub(crate) type SharedState = Arc<(Mutex<TeamState>, Condvar)>;

/// Runtime-only web-search credentials.
///
/// Values originate in Keychain and are deliberately kept out of profiles,
/// environment variables, serialization, and debug output.
#[derive(Clone, Default)]
pub struct SearchEngineApiKeys(HashMap<SearchEngineId, String>);

impl SearchEngineApiKeys {
    pub fn insert(&mut self, id: SearchEngineId, api_key: String) {
        self.0.insert(id, api_key);
    }

    fn get(&self, id: SearchEngineId) -> Option<&str> {
        self.0.get(&id).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Debug for SearchEngineApiKeys {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SearchEngineApiKeys")
            .field("entries", &self.0.len())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub struct AgentLaunchConfig {
    pub executable: PathBuf,
    pub project_path: PathBuf,
    pub sessions_path: PathBuf,
    pub extension_paths: Vec<PathBuf>,
    pub environment: HashMap<String, String>,
    pub team_config: AgentTeamConfig,
    pub search_engines: Vec<SearchEngineProfile>,
    pub search_engine_api_keys: SearchEngineApiKeys,
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("failed to bind the local agent tool host: {0}")]
    Bind(#[source] std::io::Error),
    #[error("failed to inspect the local agent tool host address: {0}")]
    Address(#[source] std::io::Error),
    #[error("agent team state lock was poisoned")]
    Poisoned,
}

#[derive(Clone)]
pub(crate) struct HostContext {
    shared: SharedState,
    launch: Arc<AgentLaunchConfig>,
    /// Policy used for agents created after the supervisor started.
    ///
    /// Existing agents retain the policy they were launched with. The prompt
    /// permission control updates this value for future children without
    /// restarting their root Pi process.
    team_config: Arc<RwLock<AgentTeamConfig>>,
    endpoint: String,
    files: Arc<file_dispatch::FileCoordinator>,
    interactions: Arc<Mutex<HashMap<Uuid, PendingInteraction>>>,
    interaction_sender: std::sync::mpsc::Sender<Value>,
}

pub struct AgentSupervisor {
    host: HostContext,
    root_id: AgentId,
    root_capability: String,
    interaction_receiver: Option<std::sync::mpsc::Receiver<Value>>,
    stopping: Arc<AtomicBool>,
    server_thread: Option<JoinHandle<()>>,
}

impl AgentSupervisor {
    pub fn start(mut launch: AgentLaunchConfig) -> Result<Self, SupervisorError> {
        launch.team_config = launch.team_config.normalized();
        let listener = TcpListener::bind("127.0.0.1:0").map_err(SupervisorError::Bind)?;
        listener
            .set_nonblocking(true)
            .map_err(SupervisorError::Bind)?;
        let endpoint = listener
            .local_addr()
            .map_err(SupervisorError::Address)?
            .to_string();
        let root_id = AgentId::new_v4();
        let root_capability = Uuid::new_v4().to_string();
        let team_id = Uuid::new_v5(
            &Uuid::NAMESPACE_URL,
            format!("pi-whim-team:{}", launch.project_path.to_string_lossy()).as_bytes(),
        );
        let root_session_id = Uuid::new_v4();
        let root = AgentNode {
            descriptor: AgentDescriptor {
                id: root_id,
                session_id: root_session_id,
                team_id,
                parent_id: None,
                parent_session_id: None,
                level: 0,
                name: "root".into(),
                role: "team owner".into(),
                status: AgentStatus::Running,
                permission_level: AgentPermissionLevel::Full,
            },
            capability: root_capability.clone(),
            task: String::new(),
            session_path: None,
            transcript: VecDeque::new(),
            outcome: AgentOutcome::default(),
            policy: AgentPermissionPolicy {
                level: AgentPermissionLevel::Full,
                ..AgentPermissionPolicy::default()
            },
            delegated_models: configured_models(&launch),
        };
        let shared = Arc::new((
            Mutex::new(TeamState {
                root_id,
                actors: HashMap::from([(root_id, root)]),
                capabilities: HashMap::from([(root_capability.clone(), root_id)]),
                inboxes: HashMap::from([(root_id, VecDeque::new())]),
                controls: HashMap::new(),
                background_processes: HashMap::new(),
            }),
            Condvar::new(),
        ));
        if let Ok(state) = shared.0.lock()
            && let Some(root) = state.actors.get(&root_id)
        {
            catalog::publish(root);
            catalog::register_active(root_session_id, root_id, &shared);
        }
        let files = file_dispatch::FileCoordinator::for_project(launch.project_path.clone());
        let (interaction_sender, interaction_receiver) = std::sync::mpsc::channel();
        let host = HostContext {
            shared,
            team_config: Arc::new(RwLock::new(launch.team_config.clone())),
            launch: Arc::new(launch),
            endpoint,
            files,
            interactions: Arc::new(Mutex::new(HashMap::new())),
            interaction_sender,
        };
        let stopping = Arc::new(AtomicBool::new(false));
        let server_thread = Some(start_server(listener, host.clone(), stopping.clone()));
        Ok(Self {
            host,
            root_id,
            root_capability,
            interaction_receiver: Some(interaction_receiver),
            stopping,
            server_thread,
        })
    }

    pub fn root_environment(&self) -> [(String, String); 4] {
        [
            (HOST_ENV.into(), self.host.endpoint.clone()),
            (CAPABILITY_ENV.into(), self.root_capability.clone()),
            (AGENT_ID_ENV.into(), self.root_id.to_string()),
            (AGENT_LEVEL_ENV.into(), "0".into()),
        ]
    }

    /// Update the default policy used by subsequently spawned agents.
    pub fn set_default_permission_level(
        &self,
        level: AgentPermissionLevel,
    ) -> Result<(), SupervisorError> {
        let mut config = self
            .host
            .team_config
            .write()
            .map_err(|_| SupervisorError::Poisoned)?;
        config.default_policy.level = level;
        *config = config.clone().normalized();
        Ok(())
    }

    /// Root-owned interactions are delivered to the native UI, never to the
    /// root model. There is only one UI consumer for a supervisor.
    pub fn take_interaction_events(&mut self) -> Option<std::sync::mpsc::Receiver<Value>> {
        self.interaction_receiver.take()
    }

    /// Resolve an interaction from the native L0 UI. This intentionally does
    /// not use the root agent capability, so the root model cannot self-approve.
    pub fn resolve_user_interaction(
        &self,
        request_id: &str,
        decision: &str,
    ) -> Result<Value, String> {
        let request_id = request_id
            .parse::<Uuid>()
            .map_err(|_| "interaction request ID is invalid".to_owned())?;
        resolve_user_interaction(&self.host, self.root_id, request_id, decision)
            .map_err(|error| error.message)
    }

    pub fn stop(&mut self) -> Result<(), SupervisorError> {
        if self.stopping.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        {
            bash_dispatch::terminate_all(&self.host.shared);
            clear_interactions(&self.host);
            let (lock, condition) = &*self.host.shared;
            let mut state = lock.lock().map_err(|_| SupervisorError::Poisoned)?;
            for control in state.controls.values() {
                let _ = control.send(ProcessCommand::Interrupt);
            }
            for node in state.actors.values() {
                catalog::publish(node);
                catalog::unregister_active(node.descriptor.session_id);
            }
            condition.notify_all();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !state.controls.is_empty() {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let waited = condition
                    .wait_timeout(state, remaining)
                    .map_err(|_| SupervisorError::Poisoned)?;
                state = waited.0;
                if waited.1.timed_out() {
                    break;
                }
            }
        }
        let _ = TcpStream::connect(&self.host.endpoint);
        if let Some(server_thread) = self.server_thread.take() {
            let _ = server_thread.join();
        }
        Ok(())
    }
}

impl Drop for AgentSupervisor {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn start_server(
    listener: TcpListener,
    host: HostContext,
    stopping: Arc<AtomicBool>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stopping.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let host = host.clone();
                    thread::spawn(move || handle_connection(stream, &host));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    })
}

fn handle_connection(mut stream: TcpStream, host: &HostContext) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    let cloned = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut request_line = String::new();
    let read_result = BufReader::new(cloned)
        .take(1024 * 1024)
        .read_line(&mut request_line);
    let response = match read_result {
        Ok(0) | Err(_) => {
            ToolResponse::error("unknown", "invalid_request", "request was unreadable")
        }
        Ok(_) => match serde_json::from_str::<ToolRequest>(&request_line) {
            Ok(request) => {
                let cancelled = Arc::new(AtomicBool::new(false));
                let disconnect_watch = watch_disconnect(&stream, cancelled.clone());
                let request_id = request.request_id.clone();
                let response = guarded_dispatch(request_id, || {
                    dispatch_request_cancellable(host, request, Some(&cancelled))
                });
                disconnect_watch.stop(&stream);
                response
            }
            Err(error) => ToolResponse::error("unknown", "invalid_request", error.to_string()),
        },
    };
    match serde_json::to_vec(&response) {
        Ok(mut encoded) => {
            encoded.push(b'\n');
            if let Err(error) = stream.write_all(&encoded) {
                eprintln!("agent-team response write failed: {error}");
            }
        }
        Err(error) => eprintln!("agent-team response serialization failed: {error}"),
    }
    let _ = stream.shutdown(Shutdown::Both);
}

fn guarded_dispatch<F>(request_id: String, dispatch: F) -> ToolResponse
where
    F: FnOnce() -> ToolResponse,
{
    match catch_unwind(AssertUnwindSafe(dispatch)) {
        Ok(response) => response,
        Err(_) => ToolResponse::error(request_id, "internal", "agent tool failed unexpectedly"),
    }
}

#[cfg(test)]
fn dispatch_request(host: &HostContext, request: ToolRequest) -> ToolResponse {
    dispatch_request_cancellable(host, request, None)
}

fn dispatch_request_cancellable(
    host: &HostContext,
    request: ToolRequest,
    cancelled: Option<&AtomicBool>,
) -> ToolResponse {
    if request.version != PROTOCOL_VERSION {
        return ToolResponse::error(
            request.request_id,
            "protocol_version",
            format!("unsupported protocol version {}", request.version),
        );
    }
    let actor_id = {
        let (lock, _) = &*host.shared;
        let Ok(state) = lock.lock() else {
            return ToolResponse::error(
                request.request_id,
                "internal",
                "agent state is unavailable",
            );
        };
        let Some(actor_id) = state.capabilities.get(&request.capability).copied() else {
            return ToolResponse::error(
                request.request_id,
                "unauthorized",
                "invalid agent capability",
            );
        };
        actor_id
    };
    if !matches!(
        request.tool_name.as_str(),
        RESET_TEAM_TOOL | "_prompt_context"
    ) && let Err(error) = ensure_actor_active(host, actor_id)
    {
        return ToolResponse::error_with_details(
            request.request_id,
            error.code,
            error.message,
            error.details,
        );
    }
    if is_policy_tool(request.tool_name.as_str())
        && let Err(error) = ensure_tool_enabled(host, actor_id, &request.tool_name)
    {
        return ToolResponse::error_with_details(
            request.request_id,
            error.code,
            error.message,
            error.details,
        );
    }
    let result = match request.tool_name.as_str() {
        SPAWN_AGENT_TOOL => spawn_agent(host, actor_id, &request.arguments),
        SEND_MESSAGE_TOOL => send_message(host, actor_id, &request.arguments),
        LIST_AGENTS_TOOL => list_agents(host, actor_id, &request.arguments),
        READ_MESSAGES_TOOL => read_messages(host, actor_id),
        READ_SESSION_TOOL => read_session(host, actor_id, &request.arguments),
        LIST_SESSIONS_TOOL => list_sessions(host, actor_id, &request.arguments),
        SEARCH_SESSIONS_TOOL => search_sessions(host, actor_id, &request.arguments),
        WAIT_AGENT_TOOL => wait_agent(host, actor_id, &request.arguments),
        INTERRUPT_AGENT_TOOL => interrupt_agent(host, actor_id, &request.arguments),
        RESET_TEAM_TOOL => reset_team(host, actor_id, &request.arguments),
        READ_FILE_TOOL => read_file(host, actor_id, &request.request_id, &request.arguments),
        WRITE_FILE_TOOL => write_file(host, actor_id, &request.request_id, &request.arguments),
        EDIT_FILE_TOOL => edit_file(host, actor_id, &request.request_id, &request.arguments),
        BASH_TOOL => parse_arguments::<BashArguments>(&request.arguments)
            .and_then(|arguments| bash_dispatch::execute(host, actor_id, arguments, cancelled)),
        FETCH_TOOL => ensure_tool_enabled(host, actor_id, FETCH_TOOL)
            .and_then(|_| parse_arguments::<fetch::FetchArguments>(&request.arguments))
            .and_then(|arguments| fetch::execute(arguments, cancelled)),
        WEB_SEARCH_TOOL => ensure_tool_enabled(host, actor_id, WEB_SEARCH_TOOL)
            .and_then(|_| parse_arguments::<web_search::WebSearchArguments>(&request.arguments))
            .and_then(|arguments| {
                web_search::execute(
                    &host.launch.search_engines,
                    &host.launch.search_engine_api_keys,
                    arguments,
                    cancelled,
                )
            }),
        LIST_PROCESSES_TOOL => bash_dispatch::list(host, actor_id),
        READ_PROCESS_TOOL => parse_arguments::<ProcessIdArguments>(&request.arguments)
            .and_then(|arguments| bash_dispatch::read(host, actor_id, arguments)),
        STOP_PROCESS_TOOL => parse_arguments::<ProcessIdArguments>(&request.arguments)
            .and_then(|arguments| bash_dispatch::stop(host, actor_id, arguments)),
        LIST_AVAILABLE_MODELS_TOOL => list_available_models(host, actor_id),
        LIST_PENDING_REQUESTS_TOOL => list_pending_interactions(host, actor_id),
        RESOLVE_INTERACTION_TOOL => parse_arguments::<ApprovalRequestArguments>(&request.arguments)
            .and_then(|arguments| resolve_interaction(host, actor_id, arguments)),
        ASK_USER_TOOL => parse_arguments::<AskUserArguments>(&request.arguments)
            .and_then(|arguments| ask_user(host, actor_id, arguments)),
        "_prompt_context" => prompt_context(host, actor_id, &request.arguments),
        _ => Err(HostError::new("unknown_tool", "unknown agent tool")),
    };
    match result {
        Ok(content) => ToolResponse::success(request.request_id, content),
        Err(error) => ToolResponse::error_with_details(
            request.request_id,
            error.code,
            error.message,
            error.details,
        ),
    }
}

fn is_policy_tool(tool: &str) -> bool {
    matches!(
        tool,
        SPAWN_AGENT_TOOL
            | SEND_MESSAGE_TOOL
            | LIST_AGENTS_TOOL
            | READ_MESSAGES_TOOL
            | WAIT_AGENT_TOOL
            | INTERRUPT_AGENT_TOOL
            | READ_SESSION_TOOL
            | LIST_SESSIONS_TOOL
            | SEARCH_SESSIONS_TOOL
            | READ_FILE_TOOL
            | WRITE_FILE_TOOL
            | EDIT_FILE_TOOL
            | BASH_TOOL
            | FETCH_TOOL
            | LIST_PROCESSES_TOOL
            | READ_PROCESS_TOOL
            | STOP_PROCESS_TOOL
            | LIST_AVAILABLE_MODELS_TOOL
            | LIST_PENDING_REQUESTS_TOOL
            | RESOLVE_INTERACTION_TOOL
            | ASK_USER_TOOL
    )
}

struct DisconnectWatch {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl DisconnectWatch {
    fn stop(self, stream: &TcpStream) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake a monitor currently blocked in `read` without closing the
        // write half that still has to deliver the response.
        let _ = stream.shutdown(Shutdown::Read);
        let _ = self.handle.join();
    }
}

fn watch_disconnect(stream: &TcpStream, cancelled: Arc<AtomicBool>) -> DisconnectWatch {
    let Ok(mut monitor) = stream.try_clone() else {
        return DisconnectWatch {
            stop: Arc::new(AtomicBool::new(true)),
            handle: thread::spawn(|| {}),
        };
    };
    let _ = monitor.set_read_timeout(Some(Duration::from_millis(250)));
    let stop = Arc::new(AtomicBool::new(false));
    let monitor_stop = stop.clone();
    let handle = thread::spawn(move || {
        let mut byte = [0u8; 1];
        while !monitor_stop.load(Ordering::Relaxed) {
            match monitor.read(&mut byte) {
                Ok(0) => {
                    cancelled.store(true, Ordering::Relaxed);
                    return;
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {
                    cancelled.store(true, Ordering::Relaxed);
                    return;
                }
            }
        }
    });
    DisconnectWatch { stop, handle }
}

fn prompt_context(host: &HostContext, actor_id: AgentId, value: &Value) -> HostResult {
    #[derive(serde::Deserialize)]
    struct Arguments {
        text: String,
    }
    let arguments: Arguments = parse_arguments(value)?;
    Ok(json!({ "text": bash_dispatch::append_prompt_context(host, actor_id, &arguments.text)? }))
}

fn file_actor(host: &HostContext, actor_id: AgentId) -> Result<AgentDescriptor, HostError> {
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    state
        .actors
        .get(&actor_id)
        .map(|node| node.descriptor.clone())
        .ok_or_else(|| HostError::new("unauthorized", "agent is unavailable"))
}

fn ensure_tool_enabled(host: &HostContext, actor_id: AgentId, tool: &str) -> Result<(), HostError> {
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let node = state
        .actors
        .get(&actor_id)
        .ok_or_else(|| HostError::new("unauthorized", "agent is unavailable"))?;
    if !node.descriptor.status.is_active() {
        return Err(HostError::new(
            "agent_inactive",
            "agent capability is no longer active",
        ));
    }
    if node.descriptor.level == 0
        || node.policy.enabled_tools.is_empty()
        || node.policy.enabled_tools.iter().any(|value| value == tool)
    {
        return Ok(());
    }
    Err(HostError::with_details(
        "tool_forbidden",
        "tool is disabled for this agent",
        json!({ "tool": tool, "permission_level": node.policy.level }),
    ))
}

fn ensure_actor_active(host: &HostContext, actor_id: AgentId) -> Result<(), HostError> {
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let active = state
        .actors
        .get(&actor_id)
        .is_some_and(|node| node.descriptor.status.is_active());
    if active {
        Ok(())
    } else {
        Err(HostError::new(
            "agent_inactive",
            "agent capability is no longer active",
        ))
    }
}

fn read_file(host: &HostContext, actor_id: AgentId, request_id: &str, value: &Value) -> HostResult {
    ensure_tool_enabled(host, actor_id, READ_FILE_TOOL)?;
    let arguments: file_dispatch::ReadArguments = parse_arguments(value)?;
    let actor = file_actor(host, actor_id)?;
    let scope = approved_file_scope(
        host,
        &actor,
        READ_FILE_TOOL,
        &arguments.path,
        value,
        approval_ticket(value),
    )?;
    host.files
        .read_in_scope(&actor, request_id, arguments, scope)
        .map_err(file_error)
}

fn write_file(
    host: &HostContext,
    actor_id: AgentId,
    request_id: &str,
    value: &Value,
) -> HostResult {
    ensure_tool_enabled(host, actor_id, WRITE_FILE_TOOL)?;
    let actor = file_actor(host, actor_id)?;
    if actor.permission_level == AgentPermissionLevel::ReadOnly {
        return Err(HostError::new(
            "file_forbidden",
            "read-only agents cannot write files",
        ));
    }
    let arguments: file_dispatch::WriteArguments = parse_arguments(value)?;
    let scope = approved_file_scope(
        host,
        &actor,
        WRITE_FILE_TOOL,
        &arguments.path,
        value,
        approval_ticket(value),
    )?;
    host.files
        .write_in_scope(&actor, request_id, arguments, scope)
        .map_err(file_error)
}

fn edit_file(host: &HostContext, actor_id: AgentId, request_id: &str, value: &Value) -> HostResult {
    ensure_tool_enabled(host, actor_id, EDIT_FILE_TOOL)?;
    let actor = file_actor(host, actor_id)?;
    if actor.permission_level == AgentPermissionLevel::ReadOnly {
        return Err(HostError::new(
            "file_forbidden",
            "read-only agents cannot edit files",
        ));
    }
    let arguments: file_dispatch::EditArguments = parse_arguments(value)?;
    let scope = approved_file_scope(
        host,
        &actor,
        EDIT_FILE_TOOL,
        &arguments.path,
        value,
        approval_ticket(value),
    )?;
    host.files
        .edit_in_scope(&actor, request_id, arguments, scope)
        .map_err(file_error)
}

fn approved_file_scope(
    host: &HostContext,
    actor: &AgentDescriptor,
    tool: &str,
    path: &str,
    arguments: &Value,
    approval_ticket: Option<&str>,
) -> Result<file_dispatch::FileScope, HostError> {
    let requested_scope = host.files.scope_for_path(path).map_err(file_error)?;
    if requested_scope == file_dispatch::FileScope::Project
        || actor.permission_level == AgentPermissionLevel::Full
    {
        return Ok(requested_scope);
    }
    if actor.permission_level == AgentPermissionLevel::ReadOnly {
        return Err(HostError::new(
            "file_forbidden",
            "read-only agents cannot access paths outside the project",
        ));
    }

    let canonical_path = host.files.canonical_host_path(path).map_err(file_error)?;
    let operation_hash = normalized_file_operation_hash(tool, &canonical_path, arguments)?;
    if let Some(ticket) = approval_ticket {
        consume_approval(host, actor.id, &operation_hash, ticket, tool)?;
        return Ok(file_dispatch::FileScope::Host);
    }
    let request_id = request_file_approval(host, actor.id, tool, &canonical_path, operation_hash)?;
    Err(HostError::with_details(
        "approval_required",
        "controlled host file access requires parent approval",
        json!({ "request_id": request_id, "high_risk": true }),
    ))
}

fn normalized_file_operation_hash(
    tool: &str,
    canonical_path: &std::path::Path,
    arguments: &Value,
) -> Result<String, HostError> {
    let mut normalized = arguments.clone();
    let object = normalized.as_object_mut().ok_or_else(|| {
        HostError::new("invalid_arguments", "file tool arguments must be an object")
    })?;
    object.remove("approval_ticket");
    object.insert("path".into(), json!(canonical_path));
    let encoded = serde_json::to_string(&normalized)
        .map_err(|_| HostError::new("invalid_arguments", "file arguments are invalid"))?;
    Ok(operation_hash(tool, &encoded))
}

/// The approval ticket is protocol metadata. Keep it outside the deserialized
/// file payload so a ticket cannot accidentally become part of a file edit.
fn approval_ticket(arguments: &Value) -> Option<&str> {
    arguments.get("approval_ticket").and_then(Value::as_str)
}

fn file_error(error: file_dispatch::FileError) -> HostError {
    HostError::with_details(error.code, error.message, error.details)
}

fn spawn_agent(host: &HostContext, parent_id: AgentId, value: &Value) -> HostResult {
    let mut arguments: SpawnAgentArguments = parse_arguments(value)?;
    arguments.name = arguments.name.trim().to_owned();
    arguments.role = arguments.role.trim().to_owned();
    validate_spawn_arguments(&arguments)?;
    if arguments.provider.is_some() != arguments.model.is_some() {
        return Err(HostError::new(
            "invalid_arguments",
            "provider and model must be supplied together",
        ));
    }
    let (agent_id, capability, level, policy, delegated_models) =
        reserve_child(host, parent_id, &arguments)?;
    let name = arguments.name.clone();
    if let Err(error) = process::launch_child(
        host,
        agent_id,
        capability,
        level,
        policy.clone(),
        delegated_models.clone(),
        arguments,
    ) {
        let (lock, condition) = &*host.shared;
        if let Ok(mut state) = lock.lock()
            && let Some(node) = state.actors.get_mut(&agent_id)
        {
            node.descriptor.status = AgentStatus::Failed;
            node.outcome.error = error.clone();
            record_session_entry(node, "error", None, &error);
            catalog::unregister_active(node.descriptor.session_id);
            catalog::publish(node);
        }
        condition.notify_all();
        return Err(HostError::new("spawn_failed", error));
    }
    Ok(json!({
        "agent_id": agent_id,
        "session_id": agent_id,
        "name": name,
        "level": level,
        "status": "running",
        "capabilities": capability_summary(&policy, &delegated_models)
    }))
}

fn reserve_child(
    host: &HostContext,
    parent_id: AgentId,
    arguments: &SpawnAgentArguments,
) -> Result<
    (
        AgentId,
        String,
        u8,
        AgentPermissionPolicy,
        Vec<AgentModelSelection>,
    ),
    HostError,
> {
    let config = host
        .team_config
        .read()
        .map_err(|_| HostError::new("internal", "agent configuration is unavailable"))?
        .clone();
    let agent_id = AgentId::new_v4();
    let capability = Uuid::new_v4().to_string();
    let (lock, _) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let level =
        validate_child(&state, &config, parent_id, &arguments.name).map_err(routing_error)?;
    let parent = state
        .actors
        .get(&parent_id)
        .ok_or_else(|| HostError::new("target_unavailable", "parent agent is unavailable"))?;
    let team_id = parent.descriptor.team_id;
    let parent_session_id = parent.descriptor.session_id;
    let policy = effective_child_policy(&config, parent, arguments)?;
    let delegated_models = delegated_models(parent, &policy, arguments)?;
    let mut node = AgentNode {
        descriptor: AgentDescriptor {
            id: agent_id,
            session_id: agent_id,
            team_id,
            parent_id: Some(parent_id),
            parent_session_id: Some(parent_session_id),
            level,
            name: arguments.name.clone(),
            role: arguments.role.clone(),
            status: AgentStatus::Starting,
            permission_level: policy.level,
        },
        capability: capability.clone(),
        task: arguments.task.clone(),
        session_path: None,
        transcript: VecDeque::new(),
        outcome: AgentOutcome::default(),
        policy: policy.clone(),
        delegated_models: delegated_models.clone(),
    };
    record_session_entry(&mut node, "user", None, &arguments.task);
    state.capabilities.insert(capability.clone(), agent_id);
    state.inboxes.insert(agent_id, VecDeque::new());
    state.actors.insert(agent_id, node);
    if let Some(node) = state.actors.get(&agent_id) {
        catalog::publish(node);
        catalog::register_active(node.descriptor.session_id, agent_id, &host.shared);
    }
    Ok((agent_id, capability, level, policy, delegated_models))
}

fn effective_child_policy(
    config: &AgentTeamConfig,
    parent: &AgentNode,
    arguments: &SpawnAgentArguments,
) -> Result<AgentPermissionPolicy, HostError> {
    let mut policy = arguments
        .preset
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| {
            config
                .presets
                .iter()
                .find(|preset| preset.name == name)
                .map(|preset| preset.policy.clone())
                .ok_or_else(|| {
                    HostError::new("invalid_arguments", "permission preset was not found")
                })
        })
        .transpose()?
        .unwrap_or_else(|| config.default_policy.clone());
    let configured_level = policy.level;
    if let Some(level) = arguments.permission_level {
        policy.level = level;
    }
    if let Some(enabled_tools) = arguments.enabled_tools.clone() {
        policy.enabled_tools = enabled_tools;
    }
    if let Some(trusted_extensions) = arguments.trusted_extensions.clone() {
        policy.trusted_extensions = trusted_extensions;
    }
    policy = normalize_agent_policy(policy);
    if policy.level == AgentPermissionLevel::Full && configured_level != AgentPermissionLevel::Full
    {
        return Err(HostError::new(
            "full_permission_requires_policy",
            "full permission requires an explicitly configured full default or preset",
        ));
    }
    if policy.level.rank() > parent.policy.level.rank() {
        return Err(HostError::new(
            "permission_escalation",
            "a child cannot receive a higher permission level than its parent",
        ));
    }
    let allowed = level_tools(policy.level);
    if policy.enabled_tools.is_empty() {
        policy.enabled_tools = allowed.iter().map(|tool| (*tool).to_owned()).collect();
    } else {
        policy
            .enabled_tools
            .retain(|tool| allowed.contains(&tool.as_str()));
        if parent.descriptor.level > 0 {
            policy.enabled_tools.retain(|tool| {
                parent
                    .policy
                    .enabled_tools
                    .iter()
                    .any(|candidate| candidate == tool)
            });
        }
    }
    if parent.descriptor.level > 0 {
        policy.trusted_extensions.retain(|extension| {
            parent
                .policy
                .trusted_extensions
                .iter()
                .any(|candidate| candidate == extension)
        });
        policy.command_allowlist.retain(|pattern| {
            parent
                .policy
                .command_allowlist
                .iter()
                .any(|candidate| candidate == pattern)
        });
    }
    Ok(policy)
}

fn level_tools(level: AgentPermissionLevel) -> &'static [&'static str] {
    const READ_ONLY: &[&str] = &[
        "read",
        "grep",
        "find",
        "web_search",
        "list_agents",
        "read_messages",
        "wait_agent",
        "read_session",
        "list_sessions",
        "search_sessions",
        "send_message",
        "list_available_models",
        "list_pending_requests",
        "resolve_interaction",
        "ask_user",
        "spawn_agent",
    ];
    const CONTROLLED: &[&str] = &[
        "read",
        "grep",
        "find",
        "web_search",
        "fetch",
        "write",
        "edit",
        "bash",
        "list_processes",
        "read_process",
        "stop_process",
        "list_agents",
        "read_messages",
        "wait_agent",
        "read_session",
        "list_sessions",
        "search_sessions",
        "send_message",
        "list_available_models",
        "list_pending_requests",
        "resolve_interaction",
        "ask_user",
        "spawn_agent",
        "interrupt_agent",
    ];
    match level {
        AgentPermissionLevel::ReadOnly => READ_ONLY,
        AgentPermissionLevel::Controlled | AgentPermissionLevel::Full => CONTROLLED,
    }
}

fn delegated_models(
    parent: &AgentNode,
    policy: &AgentPermissionPolicy,
    arguments: &SpawnAgentArguments,
) -> Result<Vec<AgentModelSelection>, HostError> {
    let mut choices = parent.delegated_models.clone();
    if !policy.allowed_models.is_empty() {
        choices.retain(|model| policy.allowed_models.contains(model));
    }
    if let (Some(provider), Some(model)) = (&arguments.provider, &arguments.model) {
        let selected = AgentModelSelection {
            provider: provider.clone(),
            model: model.clone(),
        };
        if !choices.contains(&selected) {
            return Err(HostError::new(
                "model_forbidden",
                "the requested model is not delegated to this child",
            ));
        }
        return Ok(vec![selected]);
    }
    // Unit-test supervisors and older installations may not have a models.json
    // yet. The real launch path has already selected Pi's current model.
    if choices.len() <= 1 {
        return Ok(choices);
    }
    Err(HostError::new(
        "model_required",
        "choose a delegated provider and model; use list_available_models first",
    ))
}

fn capability_summary(policy: &AgentPermissionPolicy, models: &[AgentModelSelection]) -> Value {
    json!({
        "permission_level": policy.level,
        "enabled_tools": policy.enabled_tools,
        "command_allowlist": policy.command_allowlist,
        "models": models,
        "trusted_extensions": policy.trusted_extensions,
    })
}

fn configured_models(launch: &AgentLaunchConfig) -> Vec<AgentModelSelection> {
    let Some(directory) = launch.environment.get("PI_CODING_AGENT_DIR") else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(PathBuf::from(directory).join("models.json")) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return Vec::new();
    };
    value["providers"]
        .as_object()
        .into_iter()
        .flat_map(|providers| providers.iter())
        .flat_map(|(provider, config)| {
            config["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(move |model| {
                    model["id"].as_str().map(|model| AgentModelSelection {
                        provider: provider.clone(),
                        model: model.to_owned(),
                    })
                })
        })
        .collect()
}

fn list_available_models(host: &HostContext, actor_id: AgentId) -> HostResult {
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let node = state
        .actors
        .get(&actor_id)
        .ok_or_else(|| HostError::new("unauthorized", "agent is unavailable"))?;
    Ok(json!({ "models": node.delegated_models }))
}

fn list_pending_interactions(host: &HostContext, actor_id: AgentId) -> HostResult {
    let interactions = host
        .interactions
        .lock()
        .map_err(|_| HostError::new("internal", "interaction state is unavailable"))?;
    let pending = interactions
        .values()
        .filter(|request| request.owner_id == actor_id && request.response.is_none())
        .map(interaction_value)
        .collect::<Vec<_>>();
    Ok(json!({ "requests": pending }))
}

fn ask_user(host: &HostContext, actor_id: AgentId, arguments: AskUserArguments) -> HostResult {
    let title = arguments.title.trim();
    let message = arguments.message.trim();
    if title.is_empty() || message.is_empty() {
        return Err(HostError::new(
            "invalid_arguments",
            "title and message are required",
        ));
    }
    if arguments.options.is_empty()
        || arguments.options.len() > 8
        || arguments
            .options
            .iter()
            .any(|option| option.trim().is_empty())
    {
        return Err(HostError::new(
            "invalid_arguments",
            "questions need one to eight non-empty options",
        ));
    }
    if arguments.default_option.as_ref().is_some_and(|option| {
        !arguments
            .options
            .iter()
            .any(|candidate| candidate == option)
    }) {
        return Err(HostError::new(
            "invalid_arguments",
            "default_option must be one of the supplied options",
        ));
    }
    let request = create_interaction(
        host,
        actor_id,
        NewInteraction {
            kind: InteractionKind::Question,
            title: title.to_owned(),
            message: message.to_owned(),
            options: arguments.options,
            default_option: arguments.default_option,
            operation_hash: None,
            high_risk: false,
        },
    )?;
    Ok(json!({ "request": interaction_value(&request) }))
}

fn resolve_interaction(
    host: &HostContext,
    actor_id: AgentId,
    arguments: ApprovalRequestArguments,
) -> HostResult {
    let root_id = host
        .shared
        .0
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?
        .root_id;
    if actor_id == root_id {
        return Err(HostError::new(
            "interaction_forbidden",
            "root-owned interactions must be resolved by the L0 user",
        ));
    }
    resolve_interaction_for_owner(
        host,
        actor_id,
        arguments.request_id,
        &arguments.decision,
        false,
    )
}

fn resolve_user_interaction(
    host: &HostContext,
    root_id: AgentId,
    request_id: Uuid,
    decision: &str,
) -> HostResult {
    resolve_interaction_for_owner(host, root_id, request_id, decision, true)
}

fn resolve_interaction_for_owner(
    host: &HostContext,
    owner_id: AgentId,
    request_id: Uuid,
    decision: &str,
    from_l0_user: bool,
) -> HostResult {
    let mut interactions = host
        .interactions
        .lock()
        .map_err(|_| HostError::new("internal", "interaction state is unavailable"))?;
    let request = interactions.get_mut(&request_id).ok_or_else(|| {
        HostError::new("interaction_not_found", "interaction request was not found")
    })?;
    if request.owner_id != owner_id {
        return Err(HostError::new(
            "interaction_forbidden",
            "only the assigned owner may resolve this request",
        ));
    }
    if request.response.is_some() {
        return Err(HostError::new(
            "interaction_resolved",
            "interaction was already resolved",
        ));
    }
    let decision = decision.trim();
    if decision == "escalate" {
        if from_l0_user {
            return Err(HostError::new(
                "invalid_arguments",
                "the L0 user cannot escalate an interaction",
            ));
        }
        let root_id = host
            .shared
            .0
            .lock()
            .map_err(|_| HostError::new("internal", "agent state is unavailable"))?
            .root_id;
        if request.owner_id == root_id {
            return Err(HostError::new(
                "invalid_arguments",
                "root interactions cannot be escalated",
            ));
        }
        request.owner_id = parent_id(host, request.owner_id)?;
        let recipient = request.owner_id;
        let id = request.id;
        let requester = request.requester_id;
        drop(interactions);
        notify_interaction_owner(host, recipient, id)?;
        record_interaction_audit(
            host,
            requester,
            "interaction_escalated",
            &format!("Interaction {id} escalated"),
        );
        return Ok(json!({ "request_id": id, "status": "escalated" }));
    }
    match request.kind {
        InteractionKind::Approval if !matches!(decision, "approve" | "deny") => {
            return Err(HostError::new(
                "invalid_arguments",
                "approval decision must be approve, deny, or escalate",
            ));
        }
        InteractionKind::Question if decision.is_empty() => {
            return Err(HostError::new(
                "invalid_arguments",
                "question response cannot be empty",
            ));
        }
        InteractionKind::Question
            if !request.options.is_empty()
                && decision != "cancel"
                && !request.options.iter().any(|option| option == decision) =>
        {
            return Err(HostError::new(
                "invalid_arguments",
                "question response must be one of the supplied options",
            ));
        }
        _ => {}
    }
    request.response = Some(decision.to_owned());
    let requester = request.requester_id;
    let id = request.id;
    let kind = request.kind.clone();
    drop(interactions);
    let sender = if from_l0_user {
        l0_user_descriptor(host, owner_id)?
    } else {
        file_actor(host, owner_id)?
    };
    let ticket = matches!(kind, InteractionKind::Approval).then(|| id.to_string());
    notify_interaction_requester(host, &sender, requester, id, decision, ticket.as_deref())?;
    record_interaction_audit(
        host,
        requester,
        if from_l0_user {
            "interaction_user_resolved"
        } else {
            "interaction_resolved"
        },
        &format!("Interaction {id} resolved: {decision}"),
    );
    Ok(json!({
        "request_id": id,
        "status": "resolved",
        "decision": decision,
        "approval_ticket": ticket,
    }))
}

fn l0_user_descriptor(host: &HostContext, root_id: AgentId) -> Result<AgentDescriptor, HostError> {
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let root = state
        .actors
        .get(&root_id)
        .ok_or_else(|| HostError::new("unauthorized", "root agent is unavailable"))?;
    Ok(AgentDescriptor {
        id: root_id,
        session_id: root.descriptor.session_id,
        team_id: root.descriptor.team_id,
        parent_id: None,
        parent_session_id: None,
        level: 0,
        name: "L0 user".into(),
        role: "user approval".into(),
        status: AgentStatus::Running,
        permission_level: AgentPermissionLevel::Full,
    })
}

fn notify_interaction_requester(
    host: &HostContext,
    sender: &AgentDescriptor,
    requester: AgentId,
    id: Uuid,
    decision: &str,
    ticket: Option<&str>,
) -> Result<(), HostError> {
    let (lock, condition) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let ticket_note = ticket
        .map(|ticket| format!(" Approval ticket: {ticket}."))
        .unwrap_or_default();
    enqueue_message(
        &mut state,
        sender,
        requester,
        format!("Interaction {id} resolved: {decision}.{ticket_note}"),
    )?;
    condition.notify_all();
    Ok(())
}

pub(crate) fn request_bash_approval(
    host: &HostContext,
    actor_id: AgentId,
    command: &str,
    high_risk: bool,
) -> Result<Uuid, HostError> {
    let request = create_interaction(
        host,
        actor_id,
        NewInteraction {
            kind: InteractionKind::Approval,
            title: "Approve controlled command".into(),
            message: command.to_owned(),
            options: vec!["approve".into(), "deny".into()],
            default_option: Some("deny".into()),
            operation_hash: Some(operation_hash("bash", command)),
            high_risk,
        },
    )?;
    Ok(request.id)
}

fn request_file_approval(
    host: &HostContext,
    actor_id: AgentId,
    tool: &str,
    canonical_path: &std::path::Path,
    operation_hash: String,
) -> Result<Uuid, HostError> {
    let request = create_interaction(
        host,
        actor_id,
        NewInteraction {
            kind: InteractionKind::Approval,
            title: "Approve controlled host file access".into(),
            message: format!("{tool} {}", canonical_path.display()),
            options: vec!["approve".into(), "deny".into()],
            default_option: Some("deny".into()),
            operation_hash: Some(operation_hash),
            high_risk: true,
        },
    )?;
    Ok(request.id)
}

pub(crate) fn consume_bash_approval(
    host: &HostContext,
    actor_id: AgentId,
    command: &str,
    ticket: Option<&str>,
) -> Result<(), HostError> {
    consume_approval(
        host,
        actor_id,
        &operation_hash("bash", command),
        ticket.ok_or_else(|| {
            HostError::new("approval_required", "a parent approval ticket is required")
        })?,
        "bash",
    )
}

fn consume_approval(
    host: &HostContext,
    actor_id: AgentId,
    expected_operation_hash: &str,
    ticket: &str,
    tool: &str,
) -> Result<(), HostError> {
    let id = ticket
        .parse::<Uuid>()
        .map_err(|_| HostError::new("approval_invalid", "approval ticket is invalid"))?;
    let mut interactions = host
        .interactions
        .lock()
        .map_err(|_| HostError::new("internal", "interaction state is unavailable"))?;
    let request = interactions.remove(&id).ok_or_else(|| {
        HostError::new(
            "approval_invalid",
            "approval ticket is unknown or already used",
        )
    })?;
    if request.requester_id != actor_id
        || !matches!(request.kind, InteractionKind::Approval)
        || request.operation_hash.as_deref() != Some(expected_operation_hash)
        || request.created_at.elapsed() > APPROVAL_TTL
        || request.response.as_deref() != Some("approve")
    {
        return Err(HostError::new(
            "approval_invalid",
            "approval ticket does not authorize this operation",
        ));
    }
    record_interaction_audit(
        host,
        actor_id,
        "approved_operation_executed",
        &format!("Approved {tool} operation executed"),
    );
    Ok(())
}

fn create_interaction(
    host: &HostContext,
    requester_id: AgentId,
    interaction: NewInteraction,
) -> Result<PendingInteraction, HostError> {
    let _requester = file_actor(host, requester_id)?;
    let root_id = host
        .shared
        .0
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?
        .root_id;
    let owner_id = if requester_id == root_id || interaction.high_risk {
        root_id
    } else {
        parent_id(host, requester_id)?
    };
    let request = PendingInteraction {
        id: Uuid::new_v4(),
        kind: interaction.kind,
        requester_id,
        owner_id,
        title: interaction.title,
        message: interaction.message,
        options: interaction.options,
        default_option: interaction.default_option,
        operation_hash: interaction.operation_hash,
        created_at: Instant::now(),
        response: None,
    };
    host.interactions
        .lock()
        .map_err(|_| HostError::new("internal", "interaction state is unavailable"))?
        .insert(request.id, request.clone());
    notify_interaction_owner(host, owner_id, request.id)?;
    record_interaction_audit(
        host,
        requester_id,
        "interaction_created",
        &format!("Interaction {} created", request.id),
    );
    Ok(request)
}

fn parent_id(host: &HostContext, agent_id: AgentId) -> Result<AgentId, HostError> {
    host.shared
        .0
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?
        .actors
        .get(&agent_id)
        .and_then(|node| node.descriptor.parent_id)
        .ok_or_else(|| HostError::new("interaction_forbidden", "root has no parent agent"))
}

fn notify_interaction_owner(
    host: &HostContext,
    owner_id: AgentId,
    request_id: Uuid,
) -> Result<(), HostError> {
    let request = host
        .interactions
        .lock()
        .map_err(|_| HostError::new("internal", "interaction state is unavailable"))?
        .get(&request_id)
        .cloned()
        .ok_or_else(|| {
            HostError::new("interaction_not_found", "interaction request was not found")
        })?;
    let root_id = host
        .shared
        .0
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?
        .root_id;
    if owner_id == root_id {
        host.interaction_sender
            .send(interaction_value(&request))
            .map_err(|_| {
                HostError::new(
                    "interaction_unavailable",
                    "the user interaction channel is unavailable",
                )
            })?;
        return Ok(());
    }
    let (lock, condition) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let sender = state
        .actors
        .get(&request.requester_id)
        .map(|node| node.descriptor.clone())
        .ok_or_else(|| {
            HostError::new("interaction_not_found", "interaction request was not found")
        })?;
    enqueue_message(
        &mut state,
        &sender,
        owner_id,
        format!(
            "Interaction {request_id} requires your response. Use list_pending_requests and resolve_interaction."
        ),
    )?;
    condition.notify_all();
    Ok(())
}

fn record_interaction_audit(host: &HostContext, agent_id: AgentId, role: &str, content: &str) {
    let (lock, _) = &*host.shared;
    if let Ok(mut state) = lock.lock()
        && let Some(node) = state.actors.get_mut(&agent_id)
    {
        record_session_entry(node, role, None, content);
        catalog::publish(node);
    }
}

pub(crate) fn revoke_agent_interactions(host: &HostContext, agent_id: AgentId) {
    revoke_interactions_for_agent(&host.interactions, agent_id);
}

pub(crate) fn revoke_interactions_for_agent(
    interactions: &Arc<Mutex<HashMap<Uuid, PendingInteraction>>>,
    agent_id: AgentId,
) {
    let Ok(mut interactions) = interactions.lock() else {
        return;
    };
    interactions
        .retain(|_, request| request.requester_id != agent_id && request.owner_id != agent_id);
}

fn clear_interactions(host: &HostContext) {
    if let Ok(mut interactions) = host.interactions.lock() {
        interactions.clear();
    }
}

fn enqueue_message(
    state: &mut TeamState,
    sender: &AgentDescriptor,
    recipient_id: AgentId,
    content: String,
) -> Result<(), HostError> {
    let recipient = state
        .actors
        .get(&recipient_id)
        .ok_or_else(|| {
            HostError::new("target_unavailable", "interaction recipient is unavailable")
        })?
        .descriptor
        .clone();
    if !recipient.status.is_active() {
        return Err(HostError::new(
            "target_unavailable",
            "interaction recipient is not active",
        ));
    }
    let kind = message_kind_for_descriptors(sender, &recipient)
        .unwrap_or(model::MessageKind::DirectNotification);
    let message = AgentMessage {
        id: Uuid::new_v4(),
        sender_id: sender.id,
        sender_name: sender.name.clone(),
        recipient_id,
        sender_session_id: sender.session_id,
        recipient_session_id: recipient.session_id,
        kind,
        content: content.clone(),
    };
    let inbox = state.inboxes.entry(recipient_id).or_default();
    if inbox.len() >= MAX_INBOX_MESSAGES {
        inbox.pop_front();
    }
    inbox.push_back(message);
    if let Some(recipient_node) = state.actors.get_mut(&recipient_id) {
        record_session_entry(recipient_node, "incoming", Some(sender), &content);
        catalog::publish(recipient_node);
    }
    Ok(())
}

fn interaction_value(request: &PendingInteraction) -> Value {
    json!({
        "request_id": request.id,
        "kind": request.kind,
        "title": request.title,
        "message": request.message,
        "options": request.options,
        "default_option": request.default_option,
        "requester_id": request.requester_id,
        "owner_id": request.owner_id,
    })
}

fn operation_hash(kind: &str, text: &str) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in kind.bytes().chain([0]).chain(text.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a:{hash:016x}")
}

fn send_message(host: &HostContext, sender_id: AgentId, value: &Value) -> HostResult {
    let arguments: SendMessageArguments = parse_arguments(value)?;
    if arguments.message.trim().is_empty() {
        return Err(HostError::new(
            "invalid_arguments",
            "message cannot be empty",
        ));
    }
    if arguments.message.len() > MAX_MESSAGE_BYTES {
        return Err(HostError::new(
            "invalid_arguments",
            "message exceeds the 64 KiB limit",
        ));
    }
    let (lock, condition) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let sender = state
        .actors
        .get(&sender_id)
        .ok_or_else(|| HostError::new("unauthorized", "sender is unavailable"))?
        .descriptor
        .clone();
    let target_ids = if matches!(arguments.target.as_str(), "all_children" | "broadcast") {
        state
            .actors
            .values()
            .filter(|node| {
                node.descriptor.parent_id == Some(sender_id) && node.descriptor.status.is_active()
            })
            .map(|node| node.descriptor.id)
            .collect::<Vec<_>>()
    } else {
        match resolve_visible_target(&state, sender_id, &arguments.target) {
            Ok(target_id) => vec![target_id],
            Err(error) => {
                let Some(session_id) = arguments.target.parse().ok() else {
                    return Err(routing_error(error));
                };
                let sender = sender.clone();
                drop(state);
                if let Some(location) = catalog::active(session_id) {
                    return send_remote_message(
                        host,
                        sender_id,
                        sender,
                        session_id,
                        &arguments.message,
                        location,
                    );
                }
                return queue_historical_root_message(
                    host,
                    sender_id,
                    sender,
                    session_id,
                    &arguments.message,
                    error,
                );
            }
        }
    };
    if target_ids.is_empty() {
        return Err(HostError::new(
            "target_unavailable",
            "no active direct children are available",
        ));
    }
    let mut delivered = Vec::with_capacity(target_ids.len());
    let mut kinds = Vec::with_capacity(target_ids.len());
    for recipient_id in target_ids {
        let recipient = state
            .actors
            .get(&recipient_id)
            .ok_or_else(|| HostError::new("target_unavailable", "target is unavailable"))?
            .descriptor
            .clone();
        let kind = if arguments.target == "all_children" || arguments.target == "broadcast" {
            routing::message_kind_for_descriptors(&sender, &recipient).map_err(routing_error)?
        } else {
            message_kind(&state, sender_id, recipient_id).map_err(routing_error)?
        };
        if !recipient.status.is_active() {
            return Err(HostError::new(
                "target_unavailable",
                "target agent is not active",
            ));
        }
        let message = AgentMessage {
            id: Uuid::new_v4(),
            sender_id,
            sender_name: sender.name.clone(),
            recipient_id,
            sender_session_id: sender.session_id,
            recipient_session_id: recipient.session_id,
            kind,
            content: arguments.message.clone(),
        };
        let inbox = state.inboxes.entry(recipient_id).or_default();
        if inbox.len() >= MAX_INBOX_MESSAGES {
            inbox.pop_front();
        }
        inbox.push_back(message);
        if let Some(sender_node) = state.actors.get_mut(&sender_id) {
            record_session_entry(
                sender_node,
                "outgoing",
                Some(&recipient),
                &arguments.message,
            );
            catalog::publish(sender_node);
        }
        if let Some(recipient_node) = state.actors.get_mut(&recipient_id) {
            record_session_entry(
                recipient_node,
                "incoming",
                Some(&sender),
                &arguments.message,
            );
            catalog::publish(recipient_node);
        }
        delivered.push(recipient.session_id);
        kinds.push(kind);
    }
    condition.notify_all();
    Ok(json!({
        "delivered": true,
        "targets": delivered,
        "kind": kinds.first().copied(),
        "count": kinds.len()
    }))
}

fn queue_historical_root_message(
    host: &HostContext,
    sender_id: AgentId,
    sender: AgentDescriptor,
    recipient_session_id: pi_whim_core::SessionId,
    content: &str,
    resolution_error: RoutingError,
) -> HostResult {
    if sender.level != 0
        || catalog::find_peer_root_session(&host.launch.sessions_path, recipient_session_id)
            .is_none()
    {
        return Err(routing_error(resolution_error));
    }
    catalog::enqueue_root_message(
        &host.launch.sessions_path,
        recipient_session_id,
        &sender,
        content,
    )
    .map_err(|error| HostError::new("mailbox_unavailable", error.to_string()))?;
    let (lock, _) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    if let Some(node) = state.actors.get_mut(&sender_id) {
        record_session_entry(node, "outgoing", None, content);
        catalog::publish(node);
    }
    Ok(json!({
        "delivered": true,
        "queued": true,
        "targets": [recipient_session_id],
        "kind": "peer_message",
        "count": 1
    }))
}

fn send_remote_message(
    host: &HostContext,
    sender_id: AgentId,
    sender: AgentDescriptor,
    recipient_session_id: pi_whim_core::SessionId,
    content: &str,
    location: catalog::ActiveLocation,
) -> HostResult {
    let shared = location
        .shared
        .upgrade()
        .ok_or_else(|| HostError::new("target_unavailable", "target agent is unavailable"))?;
    let kind = {
        let (lock, condition) = &*shared;
        let mut state = lock
            .lock()
            .map_err(|_| HostError::new("internal", "target agent state is unavailable"))?;
        let target = state
            .actors
            .get(&location.agent_id)
            .ok_or_else(|| HostError::new("target_unavailable", "target agent is unavailable"))?
            .descriptor
            .clone();
        if target.session_id != recipient_session_id || !target.status.is_active() {
            return Err(HostError::new(
                "target_unavailable",
                "target agent is not active",
            ));
        }
        let kind = message_kind_for_descriptors(&sender, &target).map_err(routing_error)?;
        let message = AgentMessage {
            id: Uuid::new_v4(),
            sender_id,
            sender_name: sender.name.clone(),
            recipient_id: location.agent_id,
            sender_session_id: sender.session_id,
            recipient_session_id: target.session_id,
            kind,
            content: content.to_owned(),
        };
        let inbox = state.inboxes.entry(location.agent_id).or_default();
        if inbox.len() >= MAX_INBOX_MESSAGES {
            inbox.pop_front();
        }
        inbox.push_back(message);
        if let Some(node) = state.actors.get_mut(&location.agent_id) {
            record_session_entry(node, "incoming", Some(&sender), content);
            catalog::publish(node);
        }
        condition.notify_all();
        kind
    };
    let (lock, _) = &*host.shared;
    let mut sender_state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    if let Some(node) = sender_state.actors.get_mut(&sender_id) {
        record_session_entry(node, "outgoing", None, content);
        catalog::publish(node);
    }
    Ok(json!({
        "delivered": true,
        "targets": [recipient_session_id],
        "kind": kind,
        "count": 1
    }))
}

fn list_agents(host: &HostContext, actor_id: AgentId, value: &Value) -> HostResult {
    let arguments: ListAgentsArguments = parse_optional_arguments(value)?;
    if !matches!(
        arguments.status.as_str(),
        "all" | "active" | "starting" | "running" | "completed" | "failed" | "interrupted"
    ) {
        return Err(HostError::new(
            "invalid_arguments",
            "unknown agent status filter",
        ));
    }
    let (actor, mut agents) = {
        let (lock, _) = &*host.shared;
        let state = lock
            .lock()
            .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
        let actor = state
            .actors
            .get(&actor_id)
            .map(|node| node.descriptor.clone())
            .ok_or_else(|| HostError::new("unauthorized", "agent is unavailable"))?;
        let agents = visible_agent_ids(&state, actor_id)
            .into_iter()
            .filter_map(|id| state.actors.get(&id).map(|node| node.descriptor.clone()))
            .filter(|descriptor| status_matches(descriptor.status, &arguments.status))
            .collect::<Vec<_>>();
        (actor, agents)
    };
    let mut known: std::collections::HashSet<_> =
        agents.iter().map(|agent| agent.session_id).collect();
    for location in catalog::active_locations() {
        let Some(shared) = location.shared.upgrade() else {
            continue;
        };
        let Ok(state) = shared.0.lock() else {
            continue;
        };
        let Some(candidate) = state
            .actors
            .get(&location.agent_id)
            .map(|node| node.descriptor.clone())
        else {
            continue;
        };
        if known.contains(&candidate.session_id)
            || !status_matches(candidate.status, &arguments.status)
            || message_kind_for_descriptors(&actor, &candidate).is_err()
        {
            continue;
        }
        known.insert(candidate.session_id);
        agents.push(candidate);
    }
    agents.sort_by(|left, right| {
        left.level
            .cmp(&right.level)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(json!({ "agents": agents }))
}

fn read_messages(host: &HostContext, actor_id: AgentId) -> HostResult {
    let (lock, _) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let messages: Vec<_> = state
        .inboxes
        .entry(actor_id)
        .or_default()
        .drain(..)
        .collect();
    Ok(json!({ "messages": messages }))
}

fn read_session(host: &HostContext, actor_id: AgentId, value: &Value) -> HostResult {
    let arguments: ReadSessionArguments = parse_arguments(value)?;
    let actor = {
        let (lock, _) = &*host.shared;
        let state = lock
            .lock()
            .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
        state
            .actors
            .get(&actor_id)
            .map(|node| node.descriptor.clone())
            .ok_or_else(|| HostError::new("unauthorized", "agent is unavailable"))?
    };
    let snapshot = catalog::snapshot(arguments.session_id).or_else(|| {
        if actor.level != 0 {
            return None;
        }
        let path =
            catalog::find_peer_root_session(&host.launch.sessions_path, arguments.session_id)?;
        Some(SessionSnapshot {
            descriptor: AgentDescriptor {
                id: arguments.session_id,
                session_id: arguments.session_id,
                team_id: actor.team_id,
                parent_id: None,
                parent_session_id: None,
                level: 0,
                name: "root".into(),
                role: "historical level-0 session".into(),
                status: AgentStatus::Completed,
                permission_level: AgentPermissionLevel::Full,
            },
            task: String::new(),
            transcript: Vec::new(),
            outcome: AgentOutcome::default(),
            session_path: Some(path),
        })
    });
    let snapshot = snapshot.ok_or_else(|| {
        if catalog::is_expired(arguments.session_id) {
            HostError::new(
                "session_expired",
                "the session was known locally but its bounded transcript has expired",
            )
        } else {
            HostError::new(
                "session_not_found",
                "no session or persistent level-0 history matches this session ID",
            )
        }
    })?;
    if !session_readable_by(&actor, &snapshot.descriptor) {
        return Err(HostError::new(
            "session_forbidden",
            "session history is outside this agent's authorized team neighborhood",
        ));
    }
    let conversation = if let Some(path) = snapshot.session_path.as_deref() {
        match catalog::read_root_session(path) {
            Ok(conversation) => conversation,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(HostError::new(
                    "session_forbidden",
                    "the session exists but its history cannot be read by this process",
                ));
            }
            Err(_) => {
                return Err(HostError::new(
                    "session_expired",
                    "the session metadata remains but its history file is no longer available",
                ));
            }
        }
    } else {
        snapshot.transcript.clone()
    };
    let selection = session_read::select(&conversation, &snapshot.outcome, &arguments)
        .map_err(|message| HostError::new("invalid_arguments", message))?;
    let returned_entries = selection.entries.len();
    let root_peer = actor.level == 0
        && snapshot.descriptor.level == 0
        && actor.session_id != snapshot.descriptor.session_id;
    let can_message = root_peer
        || catalog::active(snapshot.descriptor.session_id)
            .and_then(|location| {
                location
                    .shared
                    .upgrade()
                    .map(|shared| (shared, location.agent_id))
            })
            .and_then(|(shared, target_id)| {
                let target = shared
                    .0
                    .lock()
                    .ok()?
                    .actors
                    .get(&target_id)?
                    .descriptor
                    .clone();
                Some(message_kind_for_descriptors(&actor, &target).is_ok())
            })
            .unwrap_or(false);
    Ok(json!({
        "session_id": snapshot.descriptor.session_id,
        "agent": snapshot.descriptor,
        "task": snapshot.task,
        "conversation": selection.entries,
        "selection": {
            "detail": arguments.detail,
            "range": arguments.range,
            "include": selection.include,
            "total_turns": selection.total_turns,
            "start_turn": selection.start_turn,
            "end_turn": selection.end_turn,
            "start_entry_id": arguments.start_entry_id,
            "end_entry_id": arguments.end_entry_id,
            "returned_entries": returned_entries,
            "truncated": selection.truncated,
            "next_entry_id": selection.next_entry_id
        },
        "outcome": snapshot.outcome,
        "access": { "read": true, "send_message": can_message }
    }))
}

fn list_sessions(host: &HostContext, actor_id: AgentId, value: &Value) -> HostResult {
    let arguments: ListSessionsArguments = parse_optional_arguments(value)?;
    let status = arguments.status.to_lowercase();
    if !matches!(
        status.as_str(),
        "all" | "active" | "starting" | "running" | "completed" | "failed" | "interrupted"
    ) {
        return Err(HostError::new(
            "invalid_arguments",
            "status must be all, active, starting, running, completed, failed, or interrupted",
        ));
    }
    let limit = arguments.limit.clamp(1, 100);
    let inventory = session_inventory(host, actor_id);
    let mut sessions: Vec<_> = inventory
        .iter()
        .filter(|snapshot| status_matches(snapshot.descriptor.status, &status))
        .map(session_summary_value)
        .collect();
    sessions.sort_by(|left, right| {
        right["updated_at_ms"]
            .as_i64()
            .cmp(&left["updated_at_ms"].as_i64())
            .then_with(|| {
                left["session_id"]
                    .as_str()
                    .cmp(&right["session_id"].as_str())
            })
    });
    let total = sessions.len();
    let offset = usize::from(arguments.offset).min(total);
    let end = (offset + usize::from(limit)).min(total);
    let page = sessions[offset..end].to_vec();
    Ok(json!({
        "sessions": page,
        "pagination": {
            "offset": offset,
            "limit": limit,
            "total": total,
            "next_offset": (end < total).then_some(end),
        }
    }))
}

fn search_sessions(host: &HostContext, actor_id: AgentId, value: &Value) -> HostResult {
    let arguments: SearchSessionsArguments = parse_arguments(value)?;
    let query = arguments.query.trim();
    if query.is_empty() {
        return Err(HostError::new("invalid_arguments", "query cannot be empty"));
    }
    if query.chars().count() > 256 {
        return Err(HostError::new(
            "invalid_arguments",
            "query must be 256 characters or shorter",
        ));
    }
    let limit = arguments.limit.clamp(1, 100);
    let query_lower = query.to_lowercase();
    let inventory = session_inventory(host, actor_id);
    let mut matches = Vec::new();
    let mut scanned_sessions = 0_usize;
    for snapshot in &inventory {
        let conversation = match snapshot.session_path.as_deref() {
            Some(path) => catalog::read_root_session(path).unwrap_or_default(),
            None => snapshot.transcript.clone(),
        };
        scanned_sessions += 1;
        if !snapshot.task.trim().is_empty() && snapshot.task.to_lowercase().contains(&query_lower) {
            matches.push(search_match_value(
                snapshot,
                None,
                "user",
                &snapshot.task,
                query,
            ));
        }
        let mut turn = 0_u16;
        for entry in &conversation {
            if entry.role == "user" {
                turn = turn.saturating_add(1);
            }
            let details_text = entry
                .details
                .as_ref()
                .map(Value::to_string)
                .unwrap_or_default();
            let searchable = format!("{}\n{}", entry.content, details_text);
            if searchable.to_lowercase().contains(&query_lower) {
                let mut indexed_entry = entry.clone();
                indexed_entry.turn = (turn > 0).then_some(turn);
                matches.push(search_match_value(
                    snapshot,
                    Some(&indexed_entry),
                    &entry.role,
                    &searchable,
                    query,
                ));
            }
        }
    }
    let total = matches.len();
    let offset = usize::from(arguments.offset).min(total);
    let end = (offset + usize::from(limit)).min(total);
    Ok(json!({
        "matches": matches[offset..end].to_vec(),
        "pagination": {
            "offset": offset,
            "limit": limit,
            "total": total,
            "next_offset": (end < total).then_some(end),
        },
        "scanned_sessions": scanned_sessions,
    }))
}

fn session_inventory(host: &HostContext, actor_id: AgentId) -> Vec<SessionSnapshot> {
    let actor = host.shared.0.lock().ok().and_then(|state| {
        state
            .actors
            .get(&actor_id)
            .map(|node| node.descriptor.clone())
    });
    let team_id = actor
        .as_ref()
        .map(|actor| actor.team_id)
        .unwrap_or_default();
    let Some(actor) = actor else {
        return Vec::new();
    };
    let mut snapshots = catalog::snapshots()
        .into_iter()
        .filter(|snapshot| session_readable_by(&actor, &snapshot.descriptor))
        .collect::<Vec<_>>();
    let known: std::collections::HashSet<_> = snapshots
        .iter()
        .map(|snapshot| snapshot.descriptor.session_id)
        .collect();
    for path in catalog::root_session_paths(&host.launch.sessions_path) {
        let session_id = stable_session_id(&path);
        if known.contains(&session_id) {
            continue;
        }
        if actor.level != 0 {
            continue;
        }
        snapshots.push(SessionSnapshot {
            descriptor: AgentDescriptor {
                id: session_id,
                session_id,
                team_id,
                parent_id: None,
                parent_session_id: None,
                level: 0,
                name: "root".into(),
                role: "historical level-0 session".into(),
                status: AgentStatus::Completed,
                permission_level: AgentPermissionLevel::Full,
            },
            task: String::new(),
            transcript: Vec::new(),
            outcome: AgentOutcome::default(),
            session_path: Some(path),
        });
    }
    snapshots
}

fn session_readable_by(actor: &AgentDescriptor, target: &AgentDescriptor) -> bool {
    if actor.session_id == target.session_id {
        return true;
    }
    if actor.level == 0 {
        return actor.team_id == target.team_id
            && (target.level == 0 || target.parent_id == Some(actor.id));
    }
    actor.team_id == target.team_id
        && (target.parent_id == Some(actor.id)
            || actor.parent_id == Some(target.id)
            || (actor.parent_id == target.parent_id && actor.level == target.level))
}

fn session_summary_value(snapshot: &SessionSnapshot) -> Value {
    json!({
        "session_id": snapshot.descriptor.session_id,
        "level": snapshot.descriptor.level,
        "name": snapshot.descriptor.name,
        "role": snapshot.descriptor.role,
        "status": snapshot.descriptor.status,
        "parent_session_id": snapshot.descriptor.parent_session_id,
        "persistent": snapshot.session_path.is_some(),
        "updated_at_ms": snapshot
            .session_path
            .as_deref()
            .and_then(|path| std::fs::metadata(path).ok())
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_millis() as i64)
            .unwrap_or_default(),
    })
}

fn search_match_value(
    snapshot: &SessionSnapshot,
    entry: Option<&AgentSessionEntry>,
    role: &str,
    content: &str,
    query: &str,
) -> Value {
    let snippet = search_snippet(content, query);
    json!({
        "session_id": snapshot.descriptor.session_id,
        "level": snapshot.descriptor.level,
        "name": snapshot.descriptor.name,
        "status": snapshot.descriptor.status,
        "entry_id": entry.map(|entry| entry.id.clone()),
        "turn": entry.and_then(|entry| entry.turn),
        "role": role,
        "snippet": snippet,
    })
}

fn search_snippet(content: &str, query: &str) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    let lower = compact.to_lowercase();
    let start = lower
        .find(&query.to_lowercase())
        .map(|index| lower[..index].chars().count().saturating_sub(80))
        .unwrap_or(0);
    compact.chars().skip(start).take(320).collect()
}

fn wait_agent(host: &HostContext, owner_id: AgentId, value: &Value) -> HostResult {
    let arguments: WaitAgentArguments = parse_arguments(value)?;
    let timeout = Duration::from_millis(arguments.timeout_ms.clamp(1, 300_000));
    let started = Instant::now();
    let (lock, condition) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let target_id =
        resolve_visible_target(&state, owner_id, &arguments.target).map_err(routing_error)?;
    if !is_direct_child(&state, owner_id, target_id) {
        return Err(HostError::new(
            "forbidden",
            "only a direct owner can wait for an agent",
        ));
    }
    let wait_status = loop {
        let target_is_active = subtree_is_active(&state, target_id);
        if !target_is_active {
            break "completed";
        }
        if has_message_from(&state, owner_id, target_id) {
            break "message";
        }
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            break "timeout";
        }
        let waited = condition
            .wait_timeout(state, remaining)
            .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
        state = waited.0;
        if waited.1.timed_out() {
            break "timeout";
        }
    };
    let node = state
        .actors
        .get(&target_id)
        .ok_or_else(|| HostError::new("target_unavailable", "agent is unavailable"))?;
    let descriptor = node.descriptor.clone();
    let outcome = node.outcome.clone();
    let descendants = subtree_outcomes(&state, target_id);
    let messages = take_messages_from(&mut state, owner_id, target_id);
    Ok(json!({
        "agent": descriptor,
        "outcome": outcome,
        "descendants": descendants,
        "messages": messages,
        "wait_status": wait_status,
    }))
}

fn interrupt_agent(host: &HostContext, owner_id: AgentId, value: &Value) -> HostResult {
    let arguments: TargetArguments = parse_arguments(value)?;
    let (lock, _) = &*host.shared;
    let state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    let target_id =
        resolve_visible_target(&state, owner_id, &arguments.target).map_err(routing_error)?;
    if !is_direct_child(&state, owner_id, target_id) {
        return Err(HostError::new(
            "forbidden",
            "only a direct owner can interrupt an agent",
        ));
    }
    let interrupted_ids: Vec<_> = state
        .actors
        .keys()
        .copied()
        .filter(|candidate| {
            *candidate == target_id || is_descendant_of(&state, *candidate, target_id)
        })
        .collect();
    let controls: Vec<_> = interrupted_ids
        .iter()
        .copied()
        .filter_map(|id| state.controls.get(&id).cloned())
        .collect();
    if controls.is_empty() {
        return Err(HostError::new(
            "not_running",
            "agent subtree is not running",
        ));
    }
    let interrupted = controls
        .into_iter()
        .filter(|control| control.send(ProcessCommand::Interrupt).is_ok())
        .count();
    drop(state);
    for agent_id in interrupted_ids {
        revoke_agent_interactions(host, agent_id);
    }
    Ok(json!({ "interrupted": true, "target": target_id, "processes": interrupted }))
}

fn reset_team(host: &HostContext, actor_id: AgentId, value: &Value) -> HostResult {
    let arguments: ResetTeamArguments = parse_optional_arguments(value)?;
    let root_id = {
        let (lock, _) = &*host.shared;
        let state = lock
            .lock()
            .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
        if actor_id != state.root_id {
            return Err(HostError::new(
                "forbidden",
                "only the level-0 owner can reset a team",
            ));
        }
        state.root_id
    };
    let new_session_id = arguments
        .session_path
        .as_deref()
        .map(stable_session_id)
        .unwrap_or_else(Uuid::new_v4);
    let pending_messages =
        catalog::take_root_messages(&host.launch.sessions_path, new_session_id, root_id);
    bash_dispatch::clear_all(&host.shared);
    clear_interactions(host);
    let (lock, condition) = &*host.shared;
    let mut state = lock
        .lock()
        .map_err(|_| HostError::new("internal", "agent state is unavailable"))?;
    if actor_id != state.root_id {
        return Err(HostError::new(
            "forbidden",
            "only the level-0 owner can reset a team",
        ));
    }
    for control in state.controls.values() {
        let _ = control.send(ProcessCommand::Interrupt);
    }
    for node in state.actors.values_mut() {
        catalog::unregister_active(node.descriptor.session_id);
        if node.descriptor.id != actor_id && node.descriptor.status.is_active() {
            node.descriptor.status = AgentStatus::Interrupted;
        }
        catalog::publish(node);
    }
    let root_capability = state
        .actors
        .get(&root_id)
        .map(|node| node.capability.clone())
        .ok_or_else(|| HostError::new("internal", "root agent is unavailable"))?;
    state.actors.retain(|id, _| *id == root_id);
    state.capabilities.clear();
    state.capabilities.insert(root_capability, root_id);
    state.inboxes.clear();
    state
        .inboxes
        .insert(root_id, pending_messages.into_iter().collect());
    state.controls.clear();
    if let Some(root) = state.actors.get_mut(&root_id) {
        root.descriptor.session_id = new_session_id;
        root.descriptor.parent_id = None;
        root.descriptor.parent_session_id = None;
        root.descriptor.status = AgentStatus::Running;
        root.task.clear();
        root.session_path = arguments.session_path;
        root.transcript.clear();
        root.outcome = AgentOutcome::default();
        catalog::publish(root);
        catalog::register_active(root.descriptor.session_id, root_id, &host.shared);
    }
    condition.notify_all();
    let root = state
        .actors
        .get(&root_id)
        .ok_or_else(|| HostError::new("internal", "root agent is unavailable"))?;
    Ok(json!({
        "team_id": root.descriptor.team_id,
        "session_id": root.descriptor.session_id
    }))
}

fn has_message_from(state: &TeamState, recipient_id: AgentId, sender_id: AgentId) -> bool {
    state
        .inboxes
        .get(&recipient_id)
        .is_some_and(|inbox| inbox.iter().any(|message| message.sender_id == sender_id))
}

fn subtree_is_active(state: &TeamState, root_id: AgentId) -> bool {
    state.actors.values().any(|node| {
        node.descriptor.status.is_active()
            && (node.descriptor.id == root_id
                || is_descendant_of(state, node.descriptor.id, root_id))
    })
}

fn subtree_outcomes(state: &TeamState, root_id: AgentId) -> Vec<Value> {
    let mut descendants: Vec<_> = state
        .actors
        .values()
        .filter(|node| is_descendant_of(state, node.descriptor.id, root_id))
        .map(|node| {
            json!({
                "agent": node.descriptor,
                "outcome": node.outcome,
            })
        })
        .collect();
    descendants.sort_by(|left, right| {
        left["agent"]["level"]
            .as_u64()
            .cmp(&right["agent"]["level"].as_u64())
            .then_with(|| {
                left["agent"]["name"]
                    .as_str()
                    .cmp(&right["agent"]["name"].as_str())
            })
    });
    descendants
}

fn is_descendant_of(state: &TeamState, candidate: AgentId, ancestor: AgentId) -> bool {
    let mut cursor = state
        .actors
        .get(&candidate)
        .and_then(|node| node.descriptor.parent_id);
    while let Some(parent) = cursor {
        if parent == ancestor {
            return true;
        }
        cursor = state
            .actors
            .get(&parent)
            .and_then(|node| node.descriptor.parent_id);
    }
    false
}

fn take_messages_from(
    state: &mut TeamState,
    recipient_id: AgentId,
    sender_id: AgentId,
) -> Vec<AgentMessage> {
    let inbox = state.inboxes.entry(recipient_id).or_default();
    let mut selected = Vec::new();
    let mut retained = VecDeque::with_capacity(inbox.len());
    while let Some(message) = inbox.pop_front() {
        if message.sender_id == sender_id {
            selected.push(message);
        } else {
            retained.push_back(message);
        }
    }
    *inbox = retained;
    selected
}

fn record_session_entry(
    node: &mut AgentNode,
    role: &str,
    peer: Option<&AgentDescriptor>,
    content: &str,
) {
    let entry = AgentSessionEntry {
        id: Uuid::new_v4().to_string(),
        role: role.into(),
        turn: None,
        stop_reason: None,
        peer_session_id: peer.map(|descriptor| descriptor.session_id),
        peer_name: peer.map(|descriptor| descriptor.name.clone()),
        content: capture::truncate_utf8(content.to_owned(), MAX_SESSION_CONTENT_BYTES),
        details: None,
        details_truncated: false,
    };
    session_read::push_bounded_entry(&mut node.transcript, entry, MAX_SESSION_ENTRIES);
}

fn status_matches(status: AgentStatus, filter: &str) -> bool {
    match filter {
        "all" => true,
        "active" => status.is_active(),
        "starting" => status == AgentStatus::Starting,
        "running" => status == AgentStatus::Running,
        "completed" => status == AgentStatus::Completed,
        "failed" => status == AgentStatus::Failed,
        "interrupted" => status == AgentStatus::Interrupted,
        _ => false,
    }
}

fn parse_arguments<T: DeserializeOwned>(value: &Value) -> Result<T, HostError> {
    serde_json::from_value(value.clone())
        .map_err(|error| HostError::new("invalid_arguments", error.to_string()))
}

fn parse_optional_arguments<T: DeserializeOwned + Default>(value: &Value) -> Result<T, HostError> {
    if value.is_null() {
        return Ok(T::default());
    }
    parse_arguments(value)
}

fn validate_spawn_arguments(arguments: &SpawnAgentArguments) -> Result<(), HostError> {
    if arguments.name.trim().is_empty() || arguments.name.len() > 64 {
        return Err(HostError::new(
            "invalid_arguments",
            "name must contain 1 to 64 characters",
        ));
    }
    if arguments.name == "parent" {
        return Err(HostError::new(
            "invalid_arguments",
            "the name 'parent' is reserved",
        ));
    }
    if arguments.task.trim().is_empty() {
        return Err(HostError::new("invalid_arguments", "task cannot be empty"));
    }
    if arguments.role.len() > 4_096 || arguments.task.len() > 64 * 1024 {
        return Err(HostError::new(
            "invalid_arguments",
            "role or task is too large",
        ));
    }
    Ok(())
}

fn routing_error(error: RoutingError) -> HostError {
    match error {
        RoutingError::TargetUnavailable => {
            HostError::new("target_unavailable", "target is unavailable")
        }
        RoutingError::Forbidden => {
            HostError::new("forbidden", "team routing policy denied the message")
        }
        RoutingError::DepthLimit => HostError::new("depth_limit", "maximum agent depth reached"),
        RoutingError::LevelCapacity => HostError::new(
            "level_capacity",
            "maximum active agents for this level reached",
        ),
        RoutingError::DuplicateName => {
            HostError::new("duplicate_name", "an active sibling already uses this name")
        }
    }
}

type HostResult = Result<Value, HostError>;

#[derive(Debug)]
struct HostError {
    code: &'static str,
    message: String,
    details: Value,
}

impl HostError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Null,
        }
    }

    fn with_details(code: &'static str, message: impl Into<String>, details: Value) -> Self {
        Self {
            code,
            message: message.into(),
            details,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Barrier, mpsc};

    fn test_host(config: AgentTeamConfig) -> (AgentSupervisor, String) {
        test_host_at(config, "/tmp")
    }

    fn test_host_at(config: AgentTeamConfig, project_path: &str) -> (AgentSupervisor, String) {
        test_host_with_paths(config, project_path, project_path)
    }

    fn test_host_with_paths(
        config: AgentTeamConfig,
        project_path: &str,
        sessions_path: &str,
    ) -> (AgentSupervisor, String) {
        test_host_with_environment(config, project_path, sessions_path, HashMap::new())
    }

    fn test_host_with_environment(
        config: AgentTeamConfig,
        project_path: &str,
        sessions_path: &str,
        environment: HashMap<String, String>,
    ) -> (AgentSupervisor, String) {
        let supervisor = AgentSupervisor::start(AgentLaunchConfig {
            executable: PathBuf::from("/missing/pi"),
            project_path: PathBuf::from(project_path),
            sessions_path: PathBuf::from(sessions_path),
            extension_paths: Vec::new(),
            environment,
            team_config: config,
            search_engines: Vec::new(),
            search_engine_api_keys: SearchEngineApiKeys::default(),
        })
        .unwrap();
        let capability = supervisor.root_capability.clone();
        (supervisor, capability)
    }

    #[test]
    fn panicking_tool_dispatch_returns_a_bounded_error() {
        let response = guarded_dispatch("panic-request".into(), || {
            panic!("simulated tool panic");
        });
        assert!(response.is_error);
        assert_eq!(response.request_id, "panic-request");
        assert_eq!(response.error_code.as_deref(), Some("internal"));
        assert_eq!(
            response.content["message"],
            "agent tool failed unexpectedly"
        );
    }

    #[test]
    fn disconnect_monitor_is_joined_without_waiting_for_the_client() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let watch = watch_disconnect(&server, cancelled);

        let started = Instant::now();
        watch.stop(&server);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(client);
    }

    #[test]
    fn invalid_capabilities_are_rejected() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let response = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "request".into(),
                capability: "wrong".into(),
                tool_name: LIST_AGENTS_TOOL.into(),
                arguments: Value::Null,
            },
        );
        assert!(response.is_error);
        assert_eq!(response.error_code.as_deref(), Some("unauthorized"));
        supervisor.stop().unwrap();
    }

    #[test]
    fn reset_binds_the_stable_root_session_id() {
        let (mut supervisor, capability) = test_host(AgentTeamConfig::default());
        let session_path = "/tmp/pi-whim-meeting.jsonl";
        let response = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "request".into(),
                capability,
                tool_name: RESET_TEAM_TOOL.into(),
                arguments: json!({ "session_path": session_path }),
            },
        );
        assert!(!response.is_error);
        assert_eq!(
            response.content["session_id"],
            json!(stable_session_id(session_path))
        );
        supervisor.stop().unwrap();
    }

    #[test]
    fn bash_foreground_output_and_explicit_timeout_are_coordinated() {
        let (mut supervisor, capability) = test_host(AgentTeamConfig::default());
        let output = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "bash-output".into(),
                capability: capability.clone(),
                tool_name: BASH_TOOL.into(),
                arguments: json!({ "command": "printf foreground-ok" }),
            },
        );
        assert!(!output.is_error);
        assert_eq!(output.content["output"], "foreground-ok");
        assert_eq!(output.content["background"], false);
        assert_eq!(output.content["status"], "completed");

        let timeout = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "bash-timeout".into(),
                capability,
                tool_name: BASH_TOOL.into(),
                arguments: json!({ "command": "sleep 2", "timeout": 1 }),
            },
        );
        assert!(!timeout.is_error);
        assert_eq!(timeout.content["timed_out"], true);
        supervisor.stop().unwrap();
    }

    #[test]
    fn background_output_drain_reports_truncation_after_process_exit() {
        let (mut supervisor, capability) = test_host(AgentTeamConfig::default());
        let started = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "large-background".into(),
                capability: capability.clone(),
                tool_name: BASH_TOOL.into(),
                arguments: json!({
                    "command": "head -c 300000 /dev/zero | tr '\\0' x",
                    "background": true
                }),
            },
        );
        assert!(!started.is_error);
        let process_id = started.content["process"]["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();

        let mut read = Value::Null;
        for _ in 0..150 {
            read = dispatch_request(
                &supervisor.host,
                ToolRequest {
                    version: PROTOCOL_VERSION,
                    request_id: "large-read".into(),
                    capability: capability.clone(),
                    tool_name: READ_PROCESS_TOOL.into(),
                    arguments: json!({ "process_id": process_id, "tail_bytes": 4096 }),
                },
            )
            .content;
            if read["process"]["status"] != "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(read["process"]["status"], "completed");
        assert_eq!(read["process"]["exit_code"], 0);
        assert_eq!(read["output_truncated"], true);
        assert_eq!(read["output_stream"], "stdout_stderr_combined");
        assert!(read["output"].as_str().unwrap_or_default().len() <= 4096);

        let listed = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "large-list".into(),
                capability,
                tool_name: LIST_PROCESSES_TOOL.into(),
                arguments: Value::Null,
            },
        );
        let summary = listed.content["processes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|process| process["id"] == json!(process_id))
            .unwrap();
        assert_eq!(summary["output_truncated"], true);
        assert!(summary["output_bytes"].as_u64().unwrap_or_default() >= 300_000);
        supervisor.stop().unwrap();
    }

    #[test]
    fn bash_background_processes_support_default_timeout_list_read_and_stop() {
        let (mut supervisor, capability) = test_host(AgentTeamConfig::default());
        let started = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "bash-background".into(),
                capability: capability.clone(),
                tool_name: BASH_TOOL.into(),
                arguments: json!({
                    "command": "printf background-ok; sleep 30",
                    "background": true
                }),
            },
        );
        assert!(!started.is_error);
        assert_eq!(started.content["process"]["timeout_seconds"], 300);
        let process_id = started.content["process"]["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();

        let listed = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "process-list".into(),
                capability: capability.clone(),
                tool_name: LIST_PROCESSES_TOOL.into(),
                arguments: Value::Null,
            },
        );
        assert!(!listed.is_error);
        assert!(
            listed.content["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|process| process["id"] == json!(process_id))
        );

        let mut read = Value::Null;
        for _ in 0..50 {
            read = dispatch_request(
                &supervisor.host,
                ToolRequest {
                    version: PROTOCOL_VERSION,
                    request_id: "process-read".into(),
                    capability: capability.clone(),
                    tool_name: READ_PROCESS_TOOL.into(),
                    arguments: json!({ "process_id": process_id }),
                },
            )
            .content;
            if read["output"]
                .as_str()
                .unwrap_or_default()
                .contains("background-ok")
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            read["output"]
                .as_str()
                .unwrap_or_default()
                .contains("background-ok")
        );

        let context =
            bash_dispatch::append_prompt_context(&supervisor.host, supervisor.root_id, "").unwrap();
        assert!(context.contains(&process_id.to_string()));

        let stopped = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "process-stop".into(),
                capability,
                tool_name: STOP_PROCESS_TOOL.into(),
                arguments: json!({ "process_id": process_id }),
            },
        );
        assert!(!stopped.is_error);
        assert_eq!(stopped.content["stopped"], true);
        assert_eq!(stopped.content["process"]["status"], "stopped");
        supervisor.stop().unwrap();
    }

    #[test]
    fn bash_command_filters_are_applied_by_the_rust_host() {
        let mut environment = HashMap::new();
        environment.insert(
            "PI_WHIM_BASH_BLOCKED_PATTERNS".into(),
            serde_json::to_string(&["rm -rf", "shutdown"]).unwrap(),
        );
        let (mut supervisor, capability) =
            test_host_with_environment(AgentTeamConfig::default(), "/tmp", "/tmp", environment);
        let response = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "bash-filter".into(),
                capability,
                tool_name: BASH_TOOL.into(),
                arguments: json!({ "command": "echo rm -rf /tmp" }),
            },
        );
        assert!(response.is_error);
        assert_eq!(response.error_code.as_deref(), Some("bash_blocked"));
        assert_eq!(response.error_details["pattern"], "rm -rf");
        supervisor.stop().unwrap();
    }

    #[test]
    fn background_processes_transfer_to_the_direct_parent() {
        let mut config = AgentTeamConfig::default();
        config.default_policy.level = AgentPermissionLevel::Full;
        let (mut supervisor, _) = test_host(config);
        let child = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "worker".into(),
                role: "test worker".into(),
                task: "run a process".into(),
                provider: None,
                model: None,
                permission_level: Some(AgentPermissionLevel::Full),
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let started = bash_dispatch::execute(
            &supervisor.host,
            child,
            BashArguments {
                command: "sleep 30".into(),
                timeout: None,
                background: true,
                approval_ticket: None,
            },
            None,
        )
        .unwrap();
        let process_id = started["process"]["id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap();
        bash_dispatch::transfer_owned_to_parent(&supervisor.host.shared, child);
        let listed = bash_dispatch::list(&supervisor.host, supervisor.root_id).unwrap();
        assert!(
            listed["processes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|process| process["id"] == json!(process_id))
        );
        let messages = read_messages(&supervisor.host, supervisor.root_id).unwrap();
        assert!(
            messages["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| message["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Inherited"))
        );
        bash_dispatch::stop(
            &supervisor.host,
            supervisor.root_id,
            ProcessIdArguments {
                process_id,
                tail_bytes: None,
            },
        )
        .unwrap();
        supervisor.stop().unwrap();
    }

    #[test]
    fn read_session_defaults_to_reports_and_supports_last_or_full_turns() {
        let root = std::env::temp_dir().join(format!("pi-whim-read-session-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let session_path = root.join("history.jsonl");
        std::fs::write(
            &session_path,
            concat!(
                "{\"type\":\"message\",\"id\":\"u1\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"first input\"}]}}\n",
                "{\"type\":\"message\",\"id\":\"a1\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"toolUse\",\"provider\":\"test-provider\",\"model\":\"test-model\",\"usage\":{\"input\":12,\"output\":4},\"content\":[{\"type\":\"thinking\",\"thinking\":\"private thought\"},{\"type\":\"text\",\"text\":\"working\"},{\"type\":\"toolCall\",\"id\":\"call-1\",\"name\":\"read\",\"arguments\":{\"path\":\"notes.md\"}}]}}\n",
                "{\"type\":\"message\",\"id\":\"t1\",\"message\":{\"role\":\"toolResult\",\"toolCallId\":\"call-1\",\"toolName\":\"read\",\"content\":[{\"type\":\"text\",\"text\":\"tool output\"}],\"isError\":false}}\n",
                "{\"type\":\"message\",\"id\":\"a2\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hidden final thought\"},{\"type\":\"text\",\"text\":\"first report\"}]}}\n",
                "{\"type\":\"message\",\"id\":\"u2\",\"message\":{\"role\":\"user\",\"content\":\"second input\"}}\n",
                "{\"type\":\"message\",\"id\":\"a3\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":\"second report\"}}\n"
            ),
        )
        .unwrap();
        let session_path_string = session_path.to_string_lossy().into_owned();
        let session_id = stable_session_id(&session_path_string);
        let (mut supervisor, _) = test_host_with_paths(
            AgentTeamConfig::default(),
            "/tmp/read-session-project",
            root.to_str().unwrap(),
        );

        let reports = read_session(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "session_id": session_id }),
        )
        .unwrap();
        assert_eq!(reports["selection"]["detail"], "report");
        assert_eq!(reports["selection"]["total_turns"], 2);
        assert_eq!(reports["conversation"].as_array().unwrap().len(), 4);
        assert_eq!(reports["conversation"][1]["content"], "first report");
        assert!(reports["conversation"][1].get("details").is_none());
        assert!(!reports.to_string().contains("private thought"));
        assert!(!reports.to_string().contains("tool output"));

        let last = read_session(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "session_id": session_id, "range": "last_turn" }),
        )
        .unwrap();
        assert_eq!(last["selection"]["start_turn"], 2);
        assert_eq!(last["conversation"].as_array().unwrap().len(), 2);
        assert_eq!(last["conversation"][0]["content"], "second input");

        let full = read_session(
            &supervisor.host,
            supervisor.root_id,
            &json!({
                "session_id": session_id,
                "detail": "full",
                "start_turn": 1,
                "end_turn": 1
            }),
        )
        .unwrap();
        assert_eq!(full["conversation"].as_array().unwrap().len(), 4);
        assert_eq!(full["conversation"][1]["turn"], 1);
        assert!(full["conversation"][1]["details"].get("usage").is_none());
        assert!(full["conversation"][1]["details"].get("model").is_none());
        assert!(full["conversation"][1]["details"].get("content").is_none());

        let selected_fields = read_session(
            &supervisor.host,
            supervisor.root_id,
            &json!({
                "session_id": session_id,
                "detail": "full",
                "start_turn": 1,
                "end_turn": 1,
                "include": ["usage", "metadata"]
            }),
        )
        .unwrap();
        assert_eq!(
            selected_fields["conversation"][1]["details"]["usage"]["input"],
            12
        );
        assert_eq!(
            selected_fields["conversation"][1]["details"]["model"],
            "test-model"
        );
        assert!(
            selected_fields["conversation"][1]["details"]["content"][2]
                .get("name")
                .is_none()
        );

        supervisor.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_discovery_search_and_unknown_errors_are_distinct() {
        let root =
            std::env::temp_dir().join(format!("pi-whim-session-discovery-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("historical.jsonl");
        std::fs::write(
            &path,
            concat!(
                "{\"type\":\"message\",\"id\":\"u1\",\"message\":{\"role\":\"user\",\"content\":\"find this unique phrase\"}}\n",
                "{\"type\":\"message\",\"id\":\"a1\",\"message\":{\"role\":\"assistant\",\"stopReason\":\"stop\",\"content\":\"historical report\"}}\n"
            ),
        )
        .unwrap();
        let path_string = path.to_string_lossy().into_owned();
        let session_id = stable_session_id(&path_string);
        let (mut supervisor, _) = test_host_with_paths(
            AgentTeamConfig::default(),
            "/tmp/session-discovery-project",
            root.to_str().unwrap(),
        );

        let sessions = list_sessions(&supervisor.host, supervisor.root_id, &Value::Null).unwrap();
        assert!(
            sessions["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|session| session["session_id"] == json!(session_id))
        );

        let matches = search_sessions(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "query": "UNIQUE PHRASE" }),
        )
        .unwrap();
        let first_match = matches["matches"].as_array().unwrap().iter().find(|item| {
            item["session_id"] == json!(session_id) && item["entry_id"] == json!("u1")
        });
        assert_eq!(first_match.unwrap()["turn"], json!(1));

        let unknown = read_session(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "session_id": Uuid::new_v4() }),
        )
        .unwrap_err();
        assert_eq!(unknown.code, "session_not_found");

        supervisor.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn session_inventory_does_not_scan_sibling_project_directories() {
        let root =
            std::env::temp_dir().join(format!("pi-whim-session-isolation-{}", Uuid::new_v4()));
        let first_sessions = root.join("first");
        let second_sessions = root.join("second");
        std::fs::create_dir_all(&first_sessions).unwrap();
        std::fs::create_dir_all(&second_sessions).unwrap();
        let foreign = second_sessions.join("foreign.jsonl");
        std::fs::write(
            &foreign,
            r#"{"type":"message","message":{"role":"user","content":"foreign"}}"#,
        )
        .unwrap();
        let foreign_id = stable_session_id(&foreign.to_string_lossy());
        let (mut supervisor, _) = test_host_with_paths(
            AgentTeamConfig::default(),
            "/tmp/session-isolation-project",
            first_sessions.to_str().unwrap(),
        );
        let sessions = list_sessions(&supervisor.host, supervisor.root_id, &Value::Null).unwrap();
        assert!(
            !sessions["sessions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|session| session["session_id"] == json!(foreign_id))
        );
        supervisor.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_reservations_never_exceed_level_capacity() {
        let (mut supervisor, _) = test_host(AgentTeamConfig {
            max_depth: 1,
            max_agents_per_level: 4,
            ..Default::default()
        });
        let barrier = Arc::new(Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|index| {
                let host = supervisor.host.clone();
                let barrier = barrier.clone();
                let root_id = supervisor.root_id;
                thread::spawn(move || {
                    barrier.wait();
                    reserve_child(
                        &host,
                        root_id,
                        &SpawnAgentArguments {
                            name: format!("agent-{index}"),
                            role: String::new(),
                            task: "task".into(),
                            provider: None,
                            model: None,
                            permission_level: None,
                            enabled_tools: None,
                            trusted_extensions: None,
                            preset: None,
                        },
                    )
                    .is_ok()
                })
            })
            .collect();
        let successes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .filter(|created| *created)
            .count();
        assert_eq!(successes, 4);
        supervisor.stop().unwrap();
    }

    #[test]
    fn read_only_children_receive_native_search_tools_by_default() {
        let (mut supervisor, _) = test_host(AgentTeamConfig {
            default_policy: AgentPermissionPolicy {
                level: AgentPermissionLevel::ReadOnly,
                ..AgentPermissionPolicy::default()
            },
            ..AgentTeamConfig::default()
        });
        let (_, _, _, policy, _) = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "searcher".into(),
                role: String::new(),
                task: "search".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap();

        assert!(policy.enabled_tools.iter().any(|tool| tool == "grep"));
        assert!(policy.enabled_tools.iter().any(|tool| tool == "find"));
        supervisor.stop().unwrap();
    }

    #[test]
    fn permission_updates_apply_to_future_children_without_restarting_the_supervisor() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        supervisor
            .set_default_permission_level(AgentPermissionLevel::ReadOnly)
            .unwrap();

        let (first_id, _, _, first_policy, _) = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "reader".into(),
                role: String::new(),
                task: "inspect".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap();
        assert_eq!(first_policy.level, AgentPermissionLevel::ReadOnly);

        supervisor
            .set_default_permission_level(AgentPermissionLevel::Full)
            .unwrap();
        let (_, _, _, second_policy, _) = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "operator".into(),
                role: String::new(),
                task: "change files".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap();
        assert_eq!(second_policy.level, AgentPermissionLevel::Full);

        let state = supervisor.host.shared.0.lock().unwrap();
        assert_eq!(
            state.actors[&first_id].policy.level,
            AgentPermissionLevel::ReadOnly,
            "an already-running child keeps the policy it started with"
        );
        drop(state);
        supervisor.stop().unwrap();
    }

    #[test]
    fn fetch_is_available_to_controlled_agents_but_not_read_only_agents() {
        assert!(!level_tools(AgentPermissionLevel::ReadOnly).contains(&FETCH_TOOL));
        assert!(level_tools(AgentPermissionLevel::Controlled).contains(&FETCH_TOOL));
        assert!(level_tools(AgentPermissionLevel::Full).contains(&FETCH_TOOL));
    }

    #[test]
    fn explicit_tool_allowlists_can_remove_native_search_tools() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let (_, _, _, policy, _) = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "reader".into(),
                role: String::new(),
                task: "read".into(),
                provider: None,
                model: None,
                permission_level: Some(AgentPermissionLevel::ReadOnly),
                enabled_tools: Some(vec!["read".into(), "grep".into(), "not-a-tool".into()]),
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap();

        assert_eq!(policy.enabled_tools, vec!["grep", "read"]);
        supervisor.stop().unwrap();
    }

    #[test]
    fn full_children_require_a_full_preset_or_default() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let error = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "full-worker".into(),
                role: String::new(),
                task: "task".into(),
                provider: None,
                model: None,
                permission_level: Some(AgentPermissionLevel::Full),
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "full_permission_requires_policy");
        supervisor.stop().unwrap();
    }

    #[test]
    fn root_owned_interactions_require_the_native_user_resolver() {
        let (mut supervisor, capability) = test_host(AgentTeamConfig::default());
        let request = ask_user(
            &supervisor.host,
            supervisor.root_id,
            AskUserArguments {
                title: "Choose".into(),
                message: "Pick one".into(),
                options: vec!["yes".into(), "no".into()],
                default_option: Some("no".into()),
            },
        )
        .unwrap();
        let request_id = request["request"]["request_id"]
            .as_str()
            .unwrap()
            .to_owned();
        let denied = dispatch_request(
            &supervisor.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "root-resolve".into(),
                capability,
                tool_name: RESOLVE_INTERACTION_TOOL.into(),
                arguments: json!({ "request_id": request_id, "decision": "yes" }),
            },
        );
        assert_eq!(denied.error_code.as_deref(), Some("interaction_forbidden"));
        assert!(
            supervisor
                .resolve_user_interaction(&request_id, "yes")
                .is_ok()
        );
        supervisor.stop().unwrap();
    }

    #[test]
    fn controlled_host_file_access_requires_a_matching_one_time_approval() {
        let project = tempfile::tempdir().unwrap();
        let host_directory = tempfile::tempdir().unwrap();
        let host_path = host_directory.path().join("approved.txt");
        std::fs::write(&host_path, "approved content").unwrap();
        let (mut supervisor, _) =
            test_host_at(AgentTeamConfig::default(), project.path().to_str().unwrap());
        let child = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "controlled".into(),
                role: String::new(),
                task: "task".into(),
                provider: None,
                model: None,
                permission_level: Some(AgentPermissionLevel::Controlled),
                enabled_tools: Some(vec![READ_FILE_TOOL.into()]),
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let request = json!({ "path": host_path, "mode": "raw" });
        let required = read_file(&supervisor.host, child, "first", &request).unwrap_err();
        assert_eq!(required.code, "approval_required");
        let request_id = required.details["request_id"].as_str().unwrap();
        let approved = supervisor
            .resolve_user_interaction(request_id, "approve")
            .unwrap();
        let ticket = approved["approval_ticket"].as_str().unwrap().to_owned();
        let mut approved_request = request.as_object().unwrap().clone();
        approved_request.insert("approval_ticket".into(), json!(ticket));
        let read = read_file(
            &supervisor.host,
            child,
            "approved",
            &Value::Object(approved_request.clone()),
        )
        .unwrap();
        assert_eq!(read["text"], "approved content");
        let reused = read_file(
            &supervisor.host,
            child,
            "reused",
            &Value::Object(approved_request),
        )
        .unwrap_err();
        assert_eq!(reused.code, "approval_invalid");
        supervisor.stop().unwrap();
    }

    #[test]
    fn questions_reject_unknown_defaults_and_accept_cancellation() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let error = ask_user(
            &supervisor.host,
            supervisor.root_id,
            AskUserArguments {
                title: "Choose".into(),
                message: "Pick".into(),
                options: vec!["yes".into()],
                default_option: Some("no".into()),
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_arguments");
        let request = ask_user(
            &supervisor.host,
            supervisor.root_id,
            AskUserArguments {
                title: "Choose".into(),
                message: "Pick".into(),
                options: vec!["yes".into()],
                default_option: None,
            },
        )
        .unwrap();
        let request_id = request["request"]["request_id"].as_str().unwrap();
        assert!(
            supervisor
                .resolve_user_interaction(request_id, "cancel")
                .is_ok()
        );
        supervisor.stop().unwrap();
    }

    #[test]
    fn waiting_owner_returns_immediately_for_a_direct_notification() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let (child_id, _, _, _, _) = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "child".into(),
                role: String::new(),
                task: "task".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap();
        send_message(
            &supervisor.host,
            child_id,
            &json!({ "target": "parent", "message": "question" }),
        )
        .unwrap();
        let result = wait_agent(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "target": child_id, "timeout_ms": 60_000 }),
        )
        .unwrap();
        assert_eq!(result["wait_status"], "message");
        assert_eq!(result["messages"][0]["content"], "question");
        supervisor.stop().unwrap();
    }

    #[test]
    fn waiting_for_one_child_preserves_other_child_messages() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let child = |name: &str| {
            reserve_child(
                &supervisor.host,
                supervisor.root_id,
                &SpawnAgentArguments {
                    name: name.into(),
                    role: String::new(),
                    task: "task".into(),
                    provider: None,
                    model: None,
                    permission_level: None,
                    enabled_tools: None,
                    trusted_extensions: None,
                    preset: None,
                },
            )
            .unwrap()
            .0
        };
        let first = child("first");
        let second = child("second");
        send_message(
            &supervisor.host,
            first,
            &json!({ "target": "parent", "message": "first update" }),
        )
        .unwrap();
        send_message(
            &supervisor.host,
            second,
            &json!({ "target": "parent", "message": "second update" }),
        )
        .unwrap();
        let result = wait_agent(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "target": first, "timeout_ms": 1 }),
        )
        .unwrap();
        assert_eq!(result["messages"][0]["content"], "first update");
        let remaining = read_messages(&supervisor.host, supervisor.root_id).unwrap();
        assert_eq!(remaining["messages"][0]["content"], "second update");
        supervisor.stop().unwrap();
    }

    #[test]
    fn completed_parent_keeps_descendants_running_and_waits_for_the_subtree() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let parent = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "parent".into(),
                role: String::new(),
                task: "parent task".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let child = reserve_child(
            &supervisor.host,
            parent,
            &SpawnAgentArguments {
                name: "child".into(),
                role: String::new(),
                task: "child task".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let (control, commands) = mpsc::channel();
        supervisor
            .host
            .shared
            .0
            .lock()
            .unwrap()
            .controls
            .insert(child, control);
        process::finish_agent(
            &supervisor.host.shared,
            &supervisor.host.interactions,
            parent,
            process::AgentFinish {
                interrupted: false,
                exit_code: Some(0),
                output: "parent finished".into(),
                error: String::new(),
                transcript_entries: Vec::new(),
            },
        );
        assert!(matches!(
            commands.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));

        let host = supervisor.host.clone();
        let root_id = supervisor.root_id;
        let waiting = thread::spawn(move || {
            wait_agent(
                &host,
                root_id,
                &json!({ "target": parent, "timeout_ms": 1_000 }),
            )
            .unwrap()
        });
        thread::sleep(Duration::from_millis(20));
        assert!(!waiting.is_finished());
        process::finish_agent(
            &supervisor.host.shared,
            &supervisor.host.interactions,
            child,
            process::AgentFinish {
                interrupted: false,
                exit_code: Some(0),
                output: "child result".into(),
                error: String::new(),
                transcript_entries: Vec::new(),
            },
        );
        let result = waiting.join().unwrap();
        assert_eq!(result["wait_status"], "completed");
        assert_eq!(
            result["descendants"][0]["outcome"]["output"],
            "child result"
        );
        supervisor.stop().unwrap();
    }

    #[test]
    fn sibling_session_id_allows_messaging_but_parent_session_id_does_not() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let first = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "first".into(),
                role: String::new(),
                task: "task".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let second = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "second".into(),
                role: String::new(),
                task: "task".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let second_session = supervisor.host.shared.0.lock().unwrap().actors[&second]
            .descriptor
            .session_id;
        send_message(
            &supervisor.host,
            first,
            &json!({ "target": second_session, "message": "peer update" }),
        )
        .unwrap();
        assert_eq!(
            read_messages(&supervisor.host, second).unwrap()["messages"][0]["content"],
            "peer update"
        );
        let root_session = supervisor.host.shared.0.lock().unwrap().actors[&supervisor.root_id]
            .descriptor
            .session_id;
        let result = send_message(
            &supervisor.host,
            first,
            &json!({ "target": root_session, "message": "direct notification" }),
        );
        assert!(result.is_ok());
        supervisor.stop().unwrap();
    }

    #[test]
    fn session_reads_are_cross_level_read_only() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let child = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "child".into(),
                role: String::new(),
                task: "inspect this".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let root_session = supervisor.host.shared.0.lock().unwrap().actors[&supervisor.root_id]
            .descriptor
            .session_id;
        let result = read_session(
            &supervisor.host,
            child,
            &json!({ "session_id": root_session }),
        )
        .unwrap();
        assert_eq!(result["access"]["read"], true);
        assert_eq!(result["access"]["send_message"], true);
        supervisor.stop().unwrap();
    }

    #[test]
    fn root_can_read_a_direct_child_session() {
        let (mut supervisor, _) = test_host(AgentTeamConfig::default());
        let child = reserve_child(
            &supervisor.host,
            supervisor.root_id,
            &SpawnAgentArguments {
                name: "child".into(),
                role: String::new(),
                task: "inspect this".into(),
                provider: None,
                model: None,
                permission_level: None,
                enabled_tools: None,
                trusted_extensions: None,
                preset: None,
            },
        )
        .unwrap()
        .0;
        let child_session = supervisor.host.shared.0.lock().unwrap().actors[&child]
            .descriptor
            .session_id;

        let result = read_session(
            &supervisor.host,
            supervisor.root_id,
            &json!({ "session_id": child_session }),
        )
        .unwrap();

        assert_eq!(result["access"]["read"], true);
        supervisor.stop().unwrap();
    }

    #[test]
    fn active_level_zero_sessions_in_one_project_can_message_by_session_id() {
        let (mut first, first_capability) = test_host(AgentTeamConfig::default());
        let (mut second, _) = test_host(AgentTeamConfig::default());
        let second_session = second.host.shared.0.lock().unwrap().actors[&second.root_id]
            .descriptor
            .session_id;
        let response = dispatch_request(
            &first.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "root-message".into(),
                capability: first_capability,
                tool_name: SEND_MESSAGE_TOOL.into(),
                arguments: json!({ "target": second_session, "message": "root peer update" }),
            },
        );
        assert!(!response.is_error);
        let received = read_messages(&second.host, second.root_id).unwrap();
        assert_eq!(received["messages"][0]["content"], "root peer update");
        first.stop().unwrap();
        second.stop().unwrap();
    }

    #[test]
    fn active_level_zero_sessions_across_teams_can_message() {
        let (mut first, first_capability) = test_host_at(AgentTeamConfig::default(), "/tmp/team-a");
        let (mut second, _) = test_host_at(AgentTeamConfig::default(), "/tmp/team-b");
        let second_session = second.host.shared.0.lock().unwrap().actors[&second.root_id]
            .descriptor
            .session_id;
        let response = dispatch_request(
            &first.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "cross-team-message".into(),
                capability: first_capability.clone(),
                tool_name: SEND_MESSAGE_TOOL.into(),
                arguments: json!({ "target": second_session, "message": "root update" }),
            },
        );
        assert!(!response.is_error);
        let read = dispatch_request(
            &first.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "cross-team-read".into(),
                capability: first_capability,
                tool_name: READ_SESSION_TOOL.into(),
                arguments: json!({ "session_id": second_session }),
            },
        );
        assert!(read.is_error);
        assert_eq!(read.error_code.as_deref(), Some("session_forbidden"));
        first.stop().unwrap();
        second.stop().unwrap();
    }

    #[test]
    fn cross_team_subagents_remain_read_only() {
        let (mut first, _) = test_host_at(AgentTeamConfig::default(), "/tmp/subteam-a");
        let (mut second, _) = test_host_at(AgentTeamConfig::default(), "/tmp/subteam-b");
        let child = |supervisor: &AgentSupervisor, name: &str| {
            reserve_child(
                &supervisor.host,
                supervisor.root_id,
                &SpawnAgentArguments {
                    name: name.into(),
                    role: String::new(),
                    task: "task".into(),
                    provider: None,
                    model: None,
                    permission_level: None,
                    enabled_tools: None,
                    trusted_extensions: None,
                    preset: None,
                },
            )
            .unwrap()
            .0
        };
        let first_child = child(&first, "first-child");
        let second_child = child(&second, "second-child");
        let second_session = second.host.shared.0.lock().unwrap().actors[&second_child]
            .descriptor
            .session_id;
        let response = send_message(
            &first.host,
            first_child,
            &json!({ "target": second_session, "message": "not allowed" }),
        );
        assert_eq!(response.unwrap_err().code, "forbidden");
        let read = read_session(
            &first.host,
            first_child,
            &json!({ "session_id": second_session }),
        );
        assert_eq!(read.unwrap_err().code, "session_forbidden");
        first.stop().unwrap();
        second.stop().unwrap();
    }

    #[test]
    fn historical_level_zero_mail_is_delivered_when_the_session_resumes() {
        let root = std::env::temp_dir().join(format!("pi-whim-mailbox-{}", Uuid::new_v4()));
        let first_sessions = root.join("first");
        let second_sessions = root.join("second");
        std::fs::create_dir_all(&first_sessions).unwrap();
        std::fs::create_dir_all(&second_sessions).unwrap();
        let historical_path = second_sessions.join("historical.jsonl");
        std::fs::write(
            &historical_path,
            r#"{"type":"message","message":{"role":"user","content":"existing"}}\n"#,
        )
        .unwrap();
        let historical_path = historical_path.to_string_lossy().into_owned();
        let historical_id = stable_session_id(&historical_path);
        let (mut sender, _) = test_host_with_paths(
            AgentTeamConfig::default(),
            "/tmp/mailbox-sender",
            first_sessions.to_str().unwrap(),
        );
        let queued = send_message(
            &sender.host,
            sender.root_id,
            &json!({ "target": historical_id, "message": "offline update" }),
        )
        .unwrap();
        assert_eq!(queued["queued"], true);
        let inspected = read_session(
            &sender.host,
            sender.root_id,
            &json!({ "session_id": historical_id }),
        )
        .unwrap();
        assert_eq!(inspected["access"]["send_message"], true);

        let (mut recipient, recipient_capability) = test_host_with_paths(
            AgentTeamConfig::default(),
            "/tmp/mailbox-recipient",
            second_sessions.to_str().unwrap(),
        );
        let reset = dispatch_request(
            &recipient.host,
            ToolRequest {
                version: PROTOCOL_VERSION,
                request_id: "resume".into(),
                capability: recipient_capability,
                tool_name: RESET_TEAM_TOOL.into(),
                arguments: json!({ "session_path": historical_path }),
            },
        );
        assert!(!reset.is_error);
        let messages = read_messages(&recipient.host, recipient.root_id).unwrap();
        assert_eq!(messages["messages"][0]["content"], "offline update");
        sender.stop().unwrap();
        recipient.stop().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }
}
