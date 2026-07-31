use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    process::Child,
    sync::{Arc, Mutex},
    time::Instant,
};

use pi_whim_core::{AgentModelSelection, AgentPermissionLevel, AgentPermissionPolicy, SessionId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub type AgentId = Uuid;
pub type TeamId = Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Starting,
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl AgentStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Starting | Self::Running)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentDescriptor {
    pub id: AgentId,
    /// Stable session/meeting address. Unlike `id`, this is safe to expose in history.
    pub session_id: SessionId,
    pub team_id: TeamId,
    pub parent_id: Option<AgentId>,
    pub parent_session_id: Option<SessionId>,
    pub level: u8,
    pub name: String,
    pub role: String,
    pub status: AgentStatus,
    pub permission_level: AgentPermissionLevel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    PeerMessage,
    DirectNotification,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentMessage {
    pub id: Uuid,
    pub sender_id: AgentId,
    pub sender_name: String,
    pub recipient_id: AgentId,
    pub sender_session_id: SessionId,
    pub recipient_session_id: SessionId,
    pub kind: MessageKind,
    pub content: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct AgentSessionEntry {
    pub id: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    pub peer_session_id: Option<SessionId>,
    pub peer_name: Option<String>,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "is_false")]
    pub details_truncated: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct AgentOutcome {
    pub output: String,
    pub error: String,
    pub exit_code: Option<i32>,
}

#[derive(Debug)]
pub struct AgentNode {
    pub descriptor: AgentDescriptor,
    pub capability: String,
    pub task: String,
    pub session_path: Option<String>,
    pub transcript: VecDeque<AgentSessionEntry>,
    pub outcome: AgentOutcome,
    pub policy: AgentPermissionPolicy,
    pub delegated_models: Vec<AgentModelSelection>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionSnapshot {
    pub descriptor: AgentDescriptor,
    pub task: String,
    pub transcript: Vec<AgentSessionEntry>,
    pub outcome: AgentOutcome,
    pub session_path: Option<String>,
}

#[derive(Debug)]
pub struct TeamState {
    pub root_id: AgentId,
    pub actors: HashMap<AgentId, AgentNode>,
    pub capabilities: HashMap<String, AgentId>,
    pub inboxes: HashMap<AgentId, VecDeque<AgentMessage>>,
    pub controls: HashMap<AgentId, std::sync::mpsc::Sender<ProcessCommand>>,
    pub background_processes: HashMap<Uuid, BackgroundProcess>,
}

#[derive(Clone, Copy, Debug)]
pub enum ProcessCommand {
    Interrupt,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundProcessStatus {
    Running,
    Completed,
    Failed,
    TimedOut,
    Stopped,
}

impl BackgroundProcessStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, Self::Running)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct BackgroundProcessSummary {
    pub id: Uuid,
    pub owner_id: AgentId,
    pub owner_session_id: SessionId,
    pub command: String,
    pub cwd: PathBuf,
    pub status: BackgroundProcessStatus,
    pub started_at_ms: u128,
    pub timeout_seconds: u64,
    pub output_bytes: usize,
    pub output_truncated: bool,
    pub exit_code: Option<i32>,
}

#[derive(Debug)]
pub struct BackgroundProcess {
    pub summary: BackgroundProcessSummary,
    pub child: Arc<Mutex<Child>>,
    pub started_at: Instant,
    pub finished_at: Option<Instant>,
    pub output: VecDeque<u8>,
    pub output_truncated: bool,
    pub readers: Vec<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Deserialize)]
pub struct SpawnAgentArguments {
    pub name: String,
    #[serde(default)]
    pub role: String,
    pub task: String,
    pub provider: Option<String>,
    pub model: Option<String>,
    #[serde(default)]
    pub permission_level: Option<AgentPermissionLevel>,
    #[serde(default)]
    pub enabled_tools: Option<Vec<String>>,
    #[serde(default)]
    pub trusted_extensions: Option<Vec<String>>,
    #[serde(default)]
    pub preset: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TargetArguments {
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageArguments {
    pub target: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
pub struct ResolveSessionArguments {
    pub target: String,
}

#[derive(Debug, Deserialize)]
pub struct BashArguments {
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u64>,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub approval_ticket: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ApprovalRequestArguments {
    pub request_id: Uuid,
    pub decision: String,
}

#[derive(Debug, Deserialize)]
pub struct AskUserArguments {
    pub title: String,
    pub message: String,
    #[serde(default)]
    pub options: Vec<String>,
    pub default_option: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ProcessIdArguments {
    pub process_id: Uuid,
    #[serde(default)]
    pub tail_bytes: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct ReadSessionArguments {
    pub session_id: SessionId,
    #[serde(default = "default_read_detail")]
    pub detail: String,
    #[serde(default = "default_read_range")]
    pub range: String,
    pub start_turn: Option<u16>,
    pub end_turn: Option<u16>,
    pub start_entry_id: Option<String>,
    pub end_entry_id: Option<String>,
    #[serde(default)]
    pub include: Vec<String>,
}

fn default_read_detail() -> String {
    "report".into()
}

fn default_read_range() -> String {
    "all".into()
}

#[derive(Debug, Deserialize)]
pub struct ListSessionsArguments {
    #[serde(default = "default_sessions_status")]
    pub status: String,
    #[serde(default)]
    pub offset: u16,
    #[serde(default = "default_session_limit")]
    pub limit: u16,
}

impl Default for ListSessionsArguments {
    fn default() -> Self {
        Self {
            status: default_sessions_status(),
            offset: 0,
            limit: default_session_limit(),
        }
    }
}

fn default_sessions_status() -> String {
    "all".into()
}

fn default_session_limit() -> u16 {
    50
}

#[derive(Debug, Deserialize)]
pub struct SearchSessionsArguments {
    pub query: String,
    #[serde(default)]
    pub offset: u16,
    #[serde(default = "default_search_limit")]
    pub limit: u16,
}

fn default_search_limit() -> u16 {
    20
}

#[derive(Debug, Deserialize)]
pub struct ListAgentsArguments {
    #[serde(default = "default_status_filter")]
    pub status: String,
}

impl Default for ListAgentsArguments {
    fn default() -> Self {
        Self {
            status: default_status_filter(),
        }
    }
}

fn default_status_filter() -> String {
    "active".into()
}

#[derive(Debug, Default, Deserialize)]
pub struct ResetTeamArguments {
    pub session_path: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WaitAgentArguments {
    pub target: String,
    #[serde(default = "default_wait_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_wait_timeout_ms() -> u64 {
    30_000
}
