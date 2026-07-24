use std::{
    collections::{HashMap, HashSet, VecDeque},
    io::Write,
    sync::{Mutex, OnceLock, Weak},
    time::{SystemTime, UNIX_EPOCH},
};

use pi_whim_core::{SessionId, stable_session_id};
use serde::{Deserialize, Serialize};

use crate::{
    SharedState,
    model::{
        AgentDescriptor, AgentMessage, AgentNode, AgentSessionEntry, MessageKind, SessionSnapshot,
    },
};
use uuid::Uuid;

const MAX_CATALOG_SESSIONS: usize = 256;
const MAX_EXPIRED_SESSIONS: usize = 1_024;

#[derive(Clone)]
pub struct ActiveLocation {
    pub agent_id: Uuid,
    pub shared: Weak<(Mutex<crate::model::TeamState>, std::sync::Condvar)>,
}

struct CatalogState {
    snapshots: HashMap<SessionId, SessionSnapshot>,
    active: HashMap<SessionId, ActiveLocation>,
    order: VecDeque<SessionId>,
    expired: HashSet<SessionId>,
    expired_order: VecDeque<SessionId>,
}

static CATALOG: OnceLock<Mutex<CatalogState>> = OnceLock::new();
static MAILBOX_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[derive(Debug, Serialize, Deserialize)]
struct PersistedRootMessage {
    id: Uuid,
    sender_session_id: SessionId,
    sender_name: String,
    content: String,
    created_at_ms: u128,
}

fn catalog() -> &'static Mutex<CatalogState> {
    CATALOG.get_or_init(|| {
        Mutex::new(CatalogState {
            snapshots: HashMap::new(),
            active: HashMap::new(),
            order: VecDeque::new(),
            expired: HashSet::new(),
            expired_order: VecDeque::new(),
        })
    })
}

pub fn publish(node: &AgentNode) {
    let snapshot = SessionSnapshot {
        descriptor: node.descriptor.clone(),
        task: node.task.clone(),
        transcript: node.transcript.iter().cloned().collect(),
        outcome: node.outcome.clone(),
        session_path: node.session_path.clone(),
    };
    let Ok(mut state) = catalog().lock() else {
        return;
    };
    let id = snapshot.descriptor.session_id;
    state.expired.remove(&id);
    if !state.snapshots.contains_key(&id) {
        state.order.push_back(id);
    }
    state.snapshots.insert(id, snapshot);
    while state.order.len() > MAX_CATALOG_SESSIONS {
        if let Some(old) = state.order.pop_front() {
            state.snapshots.remove(&old);
            state.active.remove(&old);
            if state.expired.insert(old) {
                state.expired_order.push_back(old);
            }
        }
    }
    while state.expired_order.len() > MAX_EXPIRED_SESSIONS {
        if let Some(old) = state.expired_order.pop_front() {
            state.expired.remove(&old);
        }
    }
}

pub fn register_active(session_id: SessionId, agent_id: Uuid, shared: &SharedState) {
    let Ok(mut state) = catalog().lock() else {
        return;
    };
    state.active.insert(
        session_id,
        ActiveLocation {
            agent_id,
            shared: std::sync::Arc::downgrade(shared),
        },
    );
}

pub fn unregister_active(session_id: SessionId) {
    if let Ok(mut state) = catalog().lock() {
        state.active.remove(&session_id);
    }
}

pub fn active(session_id: SessionId) -> Option<ActiveLocation> {
    catalog().lock().ok()?.active.get(&session_id).cloned()
}

pub fn active_locations() -> Vec<ActiveLocation> {
    catalog()
        .lock()
        .map(|state| state.active.values().cloned().collect())
        .unwrap_or_default()
}

pub fn snapshot(session_id: SessionId) -> Option<SessionSnapshot> {
    catalog().lock().ok()?.snapshots.get(&session_id).cloned()
}

pub fn snapshots() -> Vec<SessionSnapshot> {
    catalog()
        .lock()
        .map(|state| state.snapshots.values().cloned().collect())
        .unwrap_or_default()
}

pub fn is_expired(session_id: SessionId) -> bool {
    catalog()
        .lock()
        .map(|state| state.expired.contains(&session_id))
        .unwrap_or(false)
}

pub fn enqueue_root_message(
    sessions_path: &std::path::Path,
    recipient_session_id: SessionId,
    sender: &AgentDescriptor,
    content: &str,
) -> std::io::Result<()> {
    let _guard = MAILBOX_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| std::io::Error::other("root mailbox lock was poisoned"))?;
    let mailbox = root_mailbox_directory(sessions_path, recipient_session_id)?;
    std::fs::create_dir_all(&mailbox)?;
    let mut entries = mailbox_entries(&mailbox)?;
    while entries.len() >= crate::MAX_INBOX_MESSAGES {
        if let Some(oldest) = entries.first() {
            let _ = std::fs::remove_file(oldest);
        }
        entries.remove(0);
    }
    let created_at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let message = PersistedRootMessage {
        id: Uuid::new_v4(),
        sender_session_id: sender.session_id,
        sender_name: sender.name.clone(),
        content: content.to_owned(),
        created_at_ms,
    };
    let path = mailbox.join(format!("{created_at_ms:020}-{}.json", message.id));
    let encoded = serde_json::to_vec(&message).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(&encoded)?;
    file.sync_all()
}

pub fn take_root_messages(
    sessions_path: &std::path::Path,
    recipient_session_id: SessionId,
    recipient_id: Uuid,
) -> Vec<AgentMessage> {
    let Ok(_guard) = MAILBOX_LOCK.get_or_init(|| Mutex::new(())).lock() else {
        return Vec::new();
    };
    let Ok(mailbox) = root_mailbox_directory(sessions_path, recipient_session_id) else {
        return Vec::new();
    };
    let Ok(entries) = mailbox_entries(&mailbox) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|path| claim_and_read_message(&path))
        .map(|message| AgentMessage {
            id: message.id,
            sender_id: message.sender_session_id,
            sender_name: message.sender_name,
            recipient_id,
            sender_session_id: message.sender_session_id,
            recipient_session_id,
            kind: MessageKind::PeerMessage,
            content: message.content,
        })
        .collect()
}

fn root_mailbox_directory(
    sessions_path: &std::path::Path,
    recipient_session_id: SessionId,
) -> std::io::Result<std::path::PathBuf> {
    let root = sessions_path
        .parent()
        .ok_or_else(|| std::io::Error::other("session root is unavailable"))?;
    Ok(root
        .join(".agent-mailboxes")
        .join(recipient_session_id.to_string()))
}

fn mailbox_entries(directory: &std::path::Path) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_file())
            .collect::<Vec<_>>(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error),
    };
    entries.sort();
    Ok(entries)
}

fn claim_and_read_message(path: &std::path::Path) -> Option<PersistedRootMessage> {
    let claimed = path.with_extension(format!("claim-{}.json", Uuid::new_v4()));
    std::fs::rename(path, &claimed).ok()?;
    let parsed = std::fs::read(&claimed)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let _ = std::fs::remove_file(claimed);
    parsed
}

/// Read a root Pi JSONL session lazily. It is deliberately bounded and does not index child
/// sessions into the sidebar or into the catalog.
pub fn read_root_session(path: &str) -> std::io::Result<Vec<AgentSessionEntry>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut entries = VecDeque::new();
    let mut line_number = 0_u64;
    while reader.read_line(&mut line)? > 0 {
        line_number += 1;
        let parsed = serde_json::from_str::<serde_json::Value>(&line).ok();
        line.clear();
        let Some(entry) = parsed else { continue };
        if entry.get("type").and_then(serde_json::Value::as_str) != Some("message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        let Some(role) = message.get("role").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let content = crate::session_read::message_content(Some(message), true);
        let has_structured_content = message
            .get("content")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|parts| !parts.is_empty());
        if content.trim().is_empty() && !has_structured_content {
            continue;
        }
        let (details, details_truncated) = crate::session_read::bounded_message_details(message);
        let item = AgentSessionEntry {
            id: entry
                .get("id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("line:{line_number}")),
            role: role.to_owned(),
            turn: None,
            stop_reason: message
                .get("stopReason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            peer_session_id: None,
            peer_name: None,
            content,
            details: Some(details),
            details_truncated,
        };
        crate::session_read::push_bounded_entry(&mut entries, item, crate::MAX_SESSION_ENTRIES);
    }
    Ok(entries.into_iter().collect())
}

pub fn find_root_session(sessions_path: &std::path::Path, session_id: SessionId) -> Option<String> {
    root_session_paths(sessions_path)
        .into_iter()
        .find(|path| stable_session_id(path) == session_id)
}

/// Root sessions may exchange durable messages across project directories that
/// share a sessions root. This is intentionally separate from session listing,
/// which remains limited to the caller's own directory.
pub fn find_peer_root_session(
    sessions_path: &std::path::Path,
    session_id: SessionId,
) -> Option<String> {
    find_root_session(sessions_path, session_id).or_else(|| {
        let parent = sessions_path.parent()?;
        std::fs::read_dir(parent)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path != sessions_path)
            .find_map(|directory| find_root_session(&directory, session_id))
    })
}

pub fn root_session_paths(sessions_path: &std::path::Path) -> Vec<String> {
    let directories = vec![sessions_path.to_path_buf()];
    let mut paths = Vec::new();
    let mut known = HashSet::new();
    for directory in directories {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }
            let path = path.to_string_lossy().into_owned();
            if known.insert(path.clone()) {
                paths.push(path);
            }
        }
    }
    paths.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    paths.reverse();
    paths
}
