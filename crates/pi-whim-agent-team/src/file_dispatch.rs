//! Project-scoped, authenticated file operations for all agents in a team.
//!
//! The coordinator deliberately keeps file-lane locks separate from the agent topology lock.
//! Reads on different files (and multiple reads on one file) can therefore proceed concurrently,
//! while pending writers prevent later readers from overtaking them.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Condvar, Mutex, OnceLock, Weak},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{file_compression, model::AgentDescriptor};
#[cfg(test)]
use pi_whim_core::AgentPermissionLevel;

const DEFAULT_MAX_TOKENS: usize = 6_000;
const DEFAULT_MAX_BYTES: usize = 48 * 1024;
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;
const MAX_IMAGE_RESPONSE_BYTES: usize = 700 * 1024;
const DEFAULT_DIRECTORY_PAGE_SIZE: usize = 200;
const MAX_DIRECTORY_PAGE_BYTES: usize = 48 * 1024;

#[derive(Debug)]
pub struct FileCoordinator {
    project_root: PathBuf,
    lanes: Mutex<HashMap<PathBuf, Arc<FileLane>>>,
    observations: Mutex<HashMap<ObservationKey, String>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ObservationKey {
    agent_id: Uuid,
    session_id: Uuid,
    path: PathBuf,
}

fn coordinators() -> &'static Mutex<HashMap<PathBuf, Weak<FileCoordinator>>> {
    static COORDINATORS: OnceLock<Mutex<HashMap<PathBuf, Weak<FileCoordinator>>>> = OnceLock::new();
    COORDINATORS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Debug)]
struct FileLane {
    state: Mutex<LaneState>,
    condition: Condvar,
}

#[derive(Debug, Default)]
struct LaneState {
    active_readers: usize,
    active_writer: bool,
    next_ticket: u64,
    writers: VecDeque<u64>,
    mutations: VecDeque<MutationRecord>,
}

#[derive(Clone, Debug)]
struct MutationRecord {
    request_id: String,
    agent_id: Uuid,
    session_id: Uuid,
    agent_name: String,
    operation: &'static str,
    before: String,
    after: String,
    changed_lines: String,
}

#[derive(Debug)]
pub struct FileError {
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

pub type FileResult = Result<Value, FileError>;

/// Restricted agents resolve every existing path and parent directory beneath
/// the canonical project root so a symlink cannot turn a relative path into a
/// host path. The supervisor may grant a controlled agent a one-time Host
/// scope after parent approval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileScope {
    Project,
    Host,
}

#[cfg(test)]
impl FileScope {
    fn for_actor(actor: &AgentDescriptor) -> Self {
        match actor.permission_level {
            AgentPermissionLevel::Full => Self::Host,
            AgentPermissionLevel::ReadOnly | AgentPermissionLevel::Controlled => Self::Project,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct ReadArguments {
    pub path: String,
    pub offset: Option<usize>,
    pub limit: Option<usize>,
    #[serde(default = "default_mode")]
    pub mode: String,
    pub max_tokens: Option<usize>,
    pub max_bytes: Option<usize>,
    pub snapshot_id: Option<String>,
    pub cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct DirectoryCursor {
    snapshot_id: String,
    offset: usize,
}

#[derive(Debug, Serialize)]
struct DirectoryEntry {
    name: String,
    file_type: &'static str,
    size: u64,
    created_at_ms: Option<u128>,
    modified_at_ms: Option<u128>,
}

impl DirectoryEntry {
    fn value(&self) -> Value {
        json!({
            "name": self.name,
            "type": self.file_type,
            "size": self.size,
            "created_at_ms": self.created_at_ms,
            "modified_at_ms": self.modified_at_ms,
        })
    }
}

struct DirectoryEntries(Vec<DirectoryEntry>);

impl DirectoryEntries {
    fn snapshot_material(&self) -> String {
        serde_json::to_string(&self.0).expect("directory entries serialize")
    }
}

#[derive(Debug, Deserialize)]
pub struct WriteArguments {
    pub path: String,
    pub content: String,
    pub base_revision: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EditArguments {
    pub path: String,
    pub edits: Vec<ReplaceEdit>,
    pub base_revision: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ReplaceEdit {
    #[serde(rename = "oldText")]
    pub old_text: String,
    #[serde(rename = "newText")]
    pub new_text: String,
}

fn default_mode() -> String {
    "auto".into()
}

impl FileCoordinator {
    pub fn for_project(project_root: PathBuf) -> Arc<Self> {
        let project_root = make_absolute(&project_root);
        let project_root = fs::canonicalize(&project_root).unwrap_or(project_root);
        let mut known = coordinators()
            .lock()
            .expect("file coordinator registry poisoned");
        if let Some(existing) = known.get(&project_root).and_then(Weak::upgrade) {
            return existing;
        }
        known.retain(|_, coordinator| coordinator.strong_count() > 0);
        let coordinator = Arc::new(Self {
            project_root: project_root.clone(),
            lanes: Mutex::new(HashMap::new()),
            observations: Mutex::new(HashMap::new()),
        });
        known.insert(
            coordinator.project_root.clone(),
            Arc::downgrade(&coordinator),
        );
        coordinator
    }

    #[cfg(test)]
    pub fn read(
        &self,
        actor: &AgentDescriptor,
        request_id: &str,
        arguments: ReadArguments,
    ) -> FileResult {
        self.read_in_scope(actor, request_id, arguments, FileScope::for_actor(actor))
    }

    pub(crate) fn read_in_scope(
        &self,
        actor: &AgentDescriptor,
        request_id: &str,
        arguments: ReadArguments,
        scope: FileScope,
    ) -> FileResult {
        let path = self.resolve_path(&arguments.path, scope)?;
        let lane = self.lane(&path);
        let started = Instant::now();
        let _permit = lane.acquire_read();
        if path.is_dir() {
            return self.read_directory(actor, &path, &arguments, started);
        }
        let bytes = fs::read(&path).map_err(|error| io_error("file_not_found", &path, error))?;
        if bytes.len() > MAX_REQUEST_BYTES && arguments.mode == "raw" {
            return Err(raw_read_too_large(&path, bytes.len(), "file"));
        }
        let revision = revision(&bytes);
        let expected_snapshot = arguments
            .snapshot_id
            .as_deref()
            .map(str::to_owned)
            .or(cursor_snapshot_id(arguments.cursor.as_deref())?);
        if let Some(expected) = expected_snapshot.as_deref()
            && expected != revision
        {
            return Err(FileError::simple(
                "stale_snapshot",
                "the file changed before this snapshot could be read",
            ));
        }
        let queue = json!({ "waited_ms": started.elapsed().as_millis() as u64 });
        if let Some(mime_type) = image_mime(&path, &bytes) {
            if bytes.len() > MAX_REQUEST_BYTES {
                return Err(raw_read_too_large(&path, bytes.len(), "image"));
            }
            if bytes.len() > MAX_IMAGE_RESPONSE_BYTES && arguments.mode != "raw" {
                let retry = json!({ "path": path, "mode": "raw" });
                return Err(FileError {
                    code: "file_too_large",
                    message: format!(
                        "image is {} bytes and exceeds the normal {} byte response limit; prefer a subagent for large-file inspection, or retry read with {} to return the complete image (up to {} bytes)",
                        bytes.len(),
                        MAX_IMAGE_RESPONSE_BYTES,
                        retry,
                        MAX_REQUEST_BYTES
                    ),
                    details: json!({
                        "path": path,
                        "bytes": bytes.len(),
                        "normal_limit_bytes": MAX_IMAGE_RESPONSE_BYTES,
                        "raw_limit_bytes": MAX_REQUEST_BYTES,
                        "recommended_action": "delegate large-file inspection to a subagent",
                        "retry": retry,
                    }),
                });
            }
            self.observe(actor, &path, &revision);
            let details = json!({
                "path": path,
                "revision": revision,
                "snapshot_id": revision,
                "format": "binary",
                "mime_type": mime_type,
                "bytes": bytes.len(),
                "complete": arguments.mode == "raw" || bytes.len() <= MAX_IMAGE_RESPONSE_BYTES,
                "queue": queue,
            });
            return Ok(json!({
                "text": format!("Read image file [{mime_type}]"),
                "image": { "data": BASE64.encode(bytes), "mime_type": mime_type },
                "details": details,
            }));
        }
        let text = match String::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) => {
                self.observe(actor, &path, &revision);
                return Ok(file_metadata_result(
                    &path,
                    error.into_bytes().len(),
                    &revision,
                    queue,
                ));
            }
        };
        let rendered = file_compression::render_text(
            &path,
            &text,
            &arguments.mode,
            arguments.offset,
            arguments.limit,
            arguments.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            arguments.max_bytes.unwrap_or(DEFAULT_MAX_BYTES),
            Some(&revision),
            arguments.cursor.as_deref(),
        )
        .map_err(compression_error)?;
        let mut details = rendered.details;
        details["path"] = json!(path);
        details["revision"] = json!(revision);
        details["snapshot_id"] = json!(revision);
        details["queue"] = queue;
        self.observe(actor, &path, &revision);
        let _ = request_id;
        Ok(json!({ "text": rendered.text, "details": details }))
    }

    fn read_directory(
        &self,
        actor: &AgentDescriptor,
        path: &Path,
        arguments: &ReadArguments,
        started: Instant,
    ) -> FileResult {
        let entries = directory_entries(path)?;
        let snapshot_id = revision(entries.snapshot_material().as_bytes());
        let expected_snapshot = arguments
            .snapshot_id
            .as_deref()
            .map(str::to_owned)
            .or(directory_cursor(arguments.cursor.as_deref())?.map(|cursor| cursor.snapshot_id));
        if let Some(expected) = expected_snapshot.as_deref()
            && expected != snapshot_id
        {
            return Err(FileError::simple(
                "stale_snapshot",
                "the directory changed before this snapshot could be read",
            ));
        }
        let offset = directory_cursor(arguments.cursor.as_deref())?
            .map(|cursor| cursor.offset)
            .unwrap_or(0);
        if offset > entries.0.len() {
            return Err(FileError::simple(
                "file_invalid_cursor",
                "the continuation cursor is invalid",
            ));
        }
        let requested = arguments
            .limit
            .unwrap_or(DEFAULT_DIRECTORY_PAGE_SIZE)
            .min(DEFAULT_DIRECTORY_PAGE_SIZE);
        let mut selected = Vec::new();
        let mut rendered_bytes = 0;
        for entry in entries.0.iter().skip(offset).take(requested) {
            let value = entry.value();
            let value_bytes = serde_json::to_vec(&value).map_or(0, |value| value.len());
            if !selected.is_empty() && rendered_bytes + value_bytes > MAX_DIRECTORY_PAGE_BYTES {
                break;
            }
            rendered_bytes += value_bytes;
            selected.push(value);
        }
        let next_offset = offset + selected.len();
        let next_cursor = (next_offset < entries.0.len()).then(|| {
            serde_json::to_string(&DirectoryCursor {
                snapshot_id: snapshot_id.clone(),
                offset: next_offset,
            })
            .expect("directory cursor serializes")
        });
        self.observe(actor, path, &snapshot_id);
        Ok(json!({
            "text": format!("Directory {}: {} entries", path.display(), selected.len()),
            "details": {
                "path": path,
                "name": path.file_name().and_then(|name| name.to_str()).unwrap_or("/"),
                "file_type": "directory",
                "entries": selected,
                "offset": offset,
                "total": entries.0.len(),
                "revision": snapshot_id,
                "snapshot_id": snapshot_id,
                "next_cursor": next_cursor,
                "queue": { "waited_ms": started.elapsed().as_millis() as u64 },
            }
        }))
    }

    #[cfg(test)]
    pub fn write(
        &self,
        actor: &AgentDescriptor,
        request_id: &str,
        arguments: WriteArguments,
    ) -> FileResult {
        self.write_in_scope(actor, request_id, arguments, FileScope::for_actor(actor))
    }

    pub(crate) fn write_in_scope(
        &self,
        actor: &AgentDescriptor,
        request_id: &str,
        arguments: WriteArguments,
        scope: FileScope,
    ) -> FileResult {
        if arguments.content.len() > MAX_REQUEST_BYTES {
            return Err(FileError::simple(
                "file_too_large",
                format!("write exceeds the {} byte limit", MAX_REQUEST_BYTES),
            ));
        }
        let path = self.resolve_path(&arguments.path, scope)?;
        let expected = arguments
            .base_revision
            .or_else(|| self.observed_revision(actor, &path));
        let expected = match expected {
            Some(revision) => Some(revision),
            None => snapshot_revision(&path)?,
        };
        let lane = self.lane(&path);
        let started = Instant::now();
        let _permit = lane.acquire_write();
        let current = fs::read(&path).unwrap_or_default();
        let current_revision = if path.exists() {
            Some(revision(&current))
        } else {
            None
        };
        if expected != current_revision {
            return Err(conflict(
                &lane,
                "the file changed before the queued write ran",
                expected.as_deref(),
                current_revision.as_deref(),
            ));
        }
        let before = current_revision.clone().unwrap_or_else(|| revision(b""));
        atomic_write(&path, arguments.content.as_bytes())?;
        let after = revision(arguments.content.as_bytes());
        let mutation = MutationRecord {
            request_id: request_id.into(),
            agent_id: actor.id,
            session_id: actor.session_id,
            agent_name: actor.name.clone(),
            operation: "write",
            before: before.clone(),
            after: after.clone(),
            changed_lines: changed_line_summary(
                &String::from_utf8_lossy(&current),
                &arguments.content,
            ),
        };
        lane.record_mutation(mutation);
        self.observe(actor, &path, &after);
        Ok(json!({
            "text": format!("Successfully wrote {} bytes to {}", arguments.content.len(), arguments.path),
            "details": {
                "path": path,
                "revision": after,
                "previous_revision": current_revision,
                "operation": "write",
                "queue": { "waited_ms": started.elapsed().as_millis() as u64 },
            }
        }))
    }

    #[cfg(test)]
    pub fn edit(
        &self,
        actor: &AgentDescriptor,
        request_id: &str,
        arguments: EditArguments,
    ) -> FileResult {
        self.edit_in_scope(actor, request_id, arguments, FileScope::for_actor(actor))
    }

    pub(crate) fn edit_in_scope(
        &self,
        actor: &AgentDescriptor,
        request_id: &str,
        arguments: EditArguments,
        scope: FileScope,
    ) -> FileResult {
        if arguments.edits.is_empty() {
            return Err(FileError::simple(
                "file_invalid_edit",
                "edits must contain at least one replacement",
            ));
        }
        let path = self.resolve_path(&arguments.path, scope)?;
        let expected = arguments
            .base_revision
            .or_else(|| self.observed_revision(actor, &path));
        let expected = match expected {
            Some(revision) => Some(revision),
            None => snapshot_revision(&path)?,
        };
        let lane = self.lane(&path);
        let started = Instant::now();
        let _permit = lane.acquire_write();
        let current_bytes =
            fs::read(&path).map_err(|error| io_error("file_not_found", &path, error))?;
        let current_revision = revision(&current_bytes);
        let raw_current = String::from_utf8(current_bytes).map_err(|_| {
            FileError::simple("file_binary_unsupported", "edit requires UTF-8 text")
        })?;
        let (bom, content_without_bom) = raw_current
            .strip_prefix('\u{feff}')
            .map_or(("", raw_current.as_str()), |content| ("\u{feff}", content));
        let use_crlf = content_without_bom.contains("\r\n");
        let mut current = content_without_bom.replace("\r\n", "\n");
        let rebased = expected.as_deref() != Some(current_revision.as_str());
        let mut matches = Vec::with_capacity(arguments.edits.len());
        for edit in &arguments.edits {
            let old_text = edit.old_text.replace("\r\n", "\n");
            let new_text = edit.new_text.replace("\r\n", "\n");
            let positions: Vec<_> = current
                .match_indices(&old_text)
                .map(|(index, _)| index)
                .collect();
            if positions.is_empty() {
                return Err(conflict(
                    &lane,
                    format!(
                        "could not find the requested edit anchor: {}",
                        edit.old_text
                    ),
                    expected.as_deref(),
                    Some(&current_revision),
                ));
            }
            if positions.len() != 1 {
                return Err(FileError::simple(
                    "file_invalid_edit",
                    format!(
                        "edit anchor occurs {} times; it must be unique",
                        positions.len()
                    ),
                ));
            }
            let start = positions[0];
            matches.push((start, start + old_text.len(), new_text));
        }
        matches.sort_by_key(|(start, _, _)| *start);
        if matches.windows(2).any(|window| window[0].1 > window[1].0) {
            return Err(FileError::simple(
                "file_invalid_edit",
                "edit anchors overlap",
            ));
        }
        for (start, end, replacement) in matches.iter().rev() {
            current.replace_range(*start..*end, replacement);
        }
        let restored = if use_crlf {
            current.replace('\n', "\r\n")
        } else {
            current.clone()
        };
        let final_content = format!("{bom}{restored}");
        atomic_write(&path, final_content.as_bytes())?;
        let after = revision(final_content.as_bytes());
        lane.record_mutation(MutationRecord {
            request_id: request_id.into(),
            agent_id: actor.id,
            session_id: actor.session_id,
            agent_name: actor.name.clone(),
            operation: "edit",
            before: current_revision.clone(),
            after: after.clone(),
            changed_lines: changed_line_summary(&raw_current, &final_content),
        });
        self.observe(actor, &path, &after);
        Ok(json!({
            "text": format!("Successfully replaced {} block(s) in {}{}.", arguments.edits.len(), arguments.path, if rebased { " (rebased)" } else { "" }),
            "details": {
                "path": path,
                "revision": after,
                "previous_revision": current_revision,
                "operation": "edit",
                "rebased": rebased,
                "changed_lines": changed_line_summary(&raw_current, &final_content),
                "queue": { "waited_ms": started.elapsed().as_millis() as u64 },
            }
        }))
    }

    fn lane(&self, path: &Path) -> Arc<FileLane> {
        let mut lanes = self.lanes.lock().expect("file lane map poisoned");
        lanes
            .entry(path.to_path_buf())
            .or_insert_with(|| Arc::new(FileLane::default()))
            .clone()
    }

    fn observe(&self, actor: &AgentDescriptor, path: &Path, revision: &str) {
        let key = ObservationKey {
            agent_id: actor.id,
            session_id: actor.session_id,
            path: path.to_path_buf(),
        };
        if let Ok(mut observations) = self.observations.lock() {
            observations.insert(key, revision.to_owned());
        }
    }

    fn observed_revision(&self, actor: &AgentDescriptor, path: &Path) -> Option<String> {
        let key = ObservationKey {
            agent_id: actor.id,
            session_id: actor.session_id,
            path: path.to_path_buf(),
        };
        self.observations
            .lock()
            .ok()
            .and_then(|observations| observations.get(&key).cloned())
    }

    /// Resolve through the same canonicalization used for an actual operation,
    /// then report whether the final target stays inside the project boundary.
    pub(crate) fn scope_for_path(&self, raw: &str) -> Result<FileScope, FileError> {
        let resolved = self.resolve_path(raw, FileScope::Host)?;
        Ok(if resolved.starts_with(&self.project_root) {
            FileScope::Project
        } else {
            FileScope::Host
        })
    }

    pub(crate) fn canonical_host_path(&self, raw: &str) -> Result<PathBuf, FileError> {
        self.resolve_path(raw, FileScope::Host)
    }

    fn resolve_path(&self, raw: &str, scope: FileScope) -> Result<PathBuf, FileError> {
        if raw.trim().is_empty() {
            return Err(FileError::simple("file_forbidden", "path cannot be empty"));
        }
        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            self.project_root.join(raw)
        };
        let normalized = lexical_normalize(&candidate)?;
        if scope == FileScope::Project && !normalized.starts_with(&self.project_root) {
            return Err(FileError::simple(
                "file_forbidden",
                "path escapes the project root",
            ));
        }
        if normalized.exists() {
            let canonical = fs::canonicalize(&normalized)
                .map_err(|error| io_error("file_forbidden", &normalized, error))?;
            if scope == FileScope::Project && !canonical.starts_with(&self.project_root) {
                return Err(FileError::simple(
                    "file_forbidden",
                    "symlink escapes the project root",
                ));
            }
            return Ok(canonical);
        }
        let mut parent = normalized
            .parent()
            .unwrap_or(&self.project_root)
            .to_path_buf();
        let mut suffix = Vec::new();
        while !parent.exists() {
            if let Some(name) = parent.file_name() {
                suffix.push(name.to_os_string());
            }
            let Some(next) = parent.parent() else { break };
            parent = next.to_path_buf();
        }
        let canonical_parent = fs::canonicalize(&parent)
            .map_err(|error| io_error("file_forbidden", &parent, error))?;
        if scope == FileScope::Project && !canonical_parent.starts_with(&self.project_root) {
            return Err(FileError::simple(
                "file_forbidden",
                "parent escapes the project root",
            ));
        }
        for component in suffix.iter().rev() {
            parent.push(component);
        }
        if let Some(name) = normalized.file_name() {
            parent.push(name);
        }
        Ok(parent)
    }
}

impl Default for FileLane {
    fn default() -> Self {
        Self {
            state: Mutex::new(LaneState::default()),
            condition: Condvar::new(),
        }
    }
}

struct ReadPermit {
    lane: Arc<FileLane>,
}

impl Drop for ReadPermit {
    fn drop(&mut self) {
        let mut state = self.lane.state.lock().expect("file lane poisoned");
        state.active_readers = state.active_readers.saturating_sub(1);
        self.lane.condition.notify_all();
    }
}

struct WritePermit {
    lane: Arc<FileLane>,
}

impl Drop for WritePermit {
    fn drop(&mut self) {
        let mut state = self.lane.state.lock().expect("file lane poisoned");
        state.active_writer = false;
        self.lane.condition.notify_all();
    }
}

impl FileLane {
    fn acquire_read(self: &Arc<Self>) -> ReadPermit {
        let mut state = self.state.lock().expect("file lane poisoned");
        while state.active_writer || !state.writers.is_empty() {
            state = self.condition.wait(state).expect("file lane poisoned");
        }
        state.active_readers += 1;
        ReadPermit { lane: self.clone() }
    }

    fn acquire_write(self: &Arc<Self>) -> WritePermit {
        let mut state = self.state.lock().expect("file lane poisoned");
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.writers.push_back(ticket);
        while state.active_writer
            || state.active_readers > 0
            || state.writers.front().copied() != Some(ticket)
        {
            state = self.condition.wait(state).expect("file lane poisoned");
        }
        state.writers.pop_front();
        state.active_writer = true;
        WritePermit { lane: self.clone() }
    }

    fn record_mutation(&self, mutation: MutationRecord) {
        let mut state = self.state.lock().expect("file lane poisoned");
        if state.mutations.len() >= 16 {
            state.mutations.pop_front();
        }
        state.mutations.push_back(mutation);
        self.condition.notify_all();
    }

    fn recent_mutations(&self) -> Vec<MutationRecord> {
        self.state
            .lock()
            .map(|state| state.mutations.iter().cloned().collect())
            .unwrap_or_default()
    }
}

fn conflict(
    lane: &FileLane,
    message: impl Into<String>,
    expected: Option<&str>,
    current: Option<&str>,
) -> FileError {
    let mutations = lane.recent_mutations();
    let mut details = json!({
        "expected_revision": expected,
        "current_revision": current,
    });
    if let Some(mutation) = mutations.last() {
        details["previous_operation"] = json!({
            "request_id": mutation.request_id,
            "agent_id": mutation.agent_id,
            "session_id": mutation.session_id,
            "agent_name": mutation.agent_name,
            "operation": mutation.operation,
            "before": mutation.before,
            "after": mutation.after,
            "changed_lines": mutation.changed_lines,
        });
    }
    details["previous_operations"] =
        json!(mutations.iter().map(mutation_value).collect::<Vec<_>>());
    FileError {
        code: "file_conflict",
        message: message.into(),
        details,
    }
}

fn mutation_value(mutation: &MutationRecord) -> Value {
    json!({
        "request_id": mutation.request_id,
        "agent_id": mutation.agent_id,
        "session_id": mutation.session_id,
        "agent_name": mutation.agent_name,
        "operation": mutation.operation,
        "before": mutation.before,
        "after": mutation.after,
        "changed_lines": mutation.changed_lines,
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), FileError> {
    if bytes.len() > MAX_REQUEST_BYTES {
        return Err(FileError::simple(
            "file_too_large",
            "file content exceeds the write limit",
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| FileError::simple("file_io_error", "file parent is unavailable"))?;
    fs::create_dir_all(parent).map_err(|error| io_error("file_io_error", parent, error))?;
    let existing_permissions = fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    let temporary = parent.join(format!(".pi-whim-{}.tmp", Uuid::new_v4()));
    fs::write(&temporary, bytes).map_err(|error| io_error("file_io_error", &temporary, error))?;
    if let Some(permissions) = existing_permissions
        && let Err(error) = fs::set_permissions(&temporary, permissions)
    {
        let _ = fs::remove_file(&temporary);
        return Err(io_error("file_io_error", &temporary, error));
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error("file_io_error", path, error));
    }
    Ok(())
}

fn io_error(code: &'static str, path: &Path, error: std::io::Error) -> FileError {
    FileError {
        code,
        message: format!("{}: {}", path.display(), error),
        details: json!({ "path": path, "kind": error.kind().to_string() }),
    }
}

fn compression_error(error: file_compression::CompressionError) -> FileError {
    FileError::simple(error.code, error.message)
}

fn raw_read_too_large(path: &Path, bytes: usize, kind: &str) -> FileError {
    FileError {
        code: "file_too_large",
        message: format!(
            "{kind} is {bytes} bytes and exceeds the {MAX_REQUEST_BYTES} byte raw-read hard limit; delegate large-file inspection to a subagent"
        ),
        details: json!({
            "path": path,
            "bytes": bytes,
            "raw_limit_bytes": MAX_REQUEST_BYTES,
            "recommended_action": "delegate large-file inspection to a subagent",
        }),
    }
}

impl FileError {
    fn simple(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Null,
        }
    }
}

fn revision(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a:{hash:016x}")
}

fn snapshot_revision(path: &Path) -> Result<Option<String>, FileError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(revision(&bytes))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("file_io_error", path, error)),
    }
}

fn cursor_snapshot_id(cursor: Option<&str>) -> Result<Option<String>, FileError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    serde_json::from_str::<file_compression::ReadCursor>(cursor)
        .map(|cursor| cursor.snapshot_id)
        .map_err(|_| FileError::simple("file_invalid_cursor", "the continuation cursor is invalid"))
}

fn directory_cursor(cursor: Option<&str>) -> Result<Option<DirectoryCursor>, FileError> {
    let Some(cursor) = cursor else {
        return Ok(None);
    };
    serde_json::from_str(cursor)
        .map(Some)
        .map_err(|_| FileError::simple("file_invalid_cursor", "the continuation cursor is invalid"))
}

fn directory_entries(path: &Path) -> Result<DirectoryEntries, FileError> {
    let entries = fs::read_dir(path).map_err(|error| io_error("file_not_found", path, error))?;
    let mut entries = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let metadata = entry.metadata().ok()?;
            let file_type = if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else if metadata.file_type().is_symlink() {
                "symlink"
            } else {
                "other"
            };
            Some(DirectoryEntry {
                name: entry.file_name().to_string_lossy().into_owned(),
                file_type,
                size: metadata.len(),
                created_at_ms: system_time_ms(metadata.created().ok()),
                modified_at_ms: system_time_ms(metadata.modified().ok()),
            })
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(DirectoryEntries(entries))
}

fn file_metadata_result(path: &Path, size: usize, revision: &str, queue: Value) -> Value {
    let metadata = fs::metadata(path).ok();
    let file_type = metadata.as_ref().map_or("unknown", |metadata| {
        if metadata.is_file() {
            "file"
        } else if metadata.is_dir() {
            "directory"
        } else {
            "other"
        }
    });
    let details = json!({
        "path": path,
        "name": path.file_name().and_then(|name| name.to_str()).unwrap_or("file"),
        "file_type": file_type,
        "size": size,
        "created_at_ms": metadata.as_ref().and_then(|metadata| system_time_ms(metadata.created().ok())),
        "modified_at_ms": metadata.as_ref().and_then(|metadata| system_time_ms(metadata.modified().ok())),
        "revision": revision,
        "snapshot_id": revision,
        "format": "metadata",
        "queue": queue,
    });
    json!({
        "text": format!("File metadata for {}", path.display()),
        "details": details,
    })
}

fn system_time_ms(time: Option<SystemTime>) -> Option<u128> {
    time.and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
}

fn changed_line_summary(before: &str, after: &str) -> String {
    let before_lines: Vec<_> = before.split_inclusive('\n').collect();
    let after_lines: Vec<_> = after.split_inclusive('\n').collect();
    let prefix = before_lines
        .iter()
        .zip(&after_lines)
        .take_while(|(left, right)| left == right)
        .count();
    let maximum_suffix = before_lines
        .len()
        .saturating_sub(prefix)
        .min(after_lines.len().saturating_sub(prefix));
    let suffix = (0..maximum_suffix)
        .take_while(|offset| {
            before_lines[before_lines.len() - 1 - offset]
                == after_lines[after_lines.len() - 1 - offset]
        })
        .count();
    let before_end = before_lines.len().saturating_sub(suffix);
    let after_end = after_lines.len().saturating_sub(suffix);
    format!(
        "L{}-L{} -> L{}-L{}; bytes {}->{}",
        prefix + 1,
        before_end.max(prefix + 1),
        prefix + 1,
        after_end.max(prefix + 1),
        before.len(),
        after.len()
    )
}

fn image_mime(path: &Path, bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        return Some("image/webp");
    }
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "bmp" => Some("image/bmp"),
        _ => None,
    }
}

fn make_absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn lexical_normalize(path: &Path) -> Result<PathBuf, FileError> {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !output.pop() {
                    return Err(FileError::simple(
                        "file_forbidden",
                        "path escapes the project root",
                    ));
                }
            }
            other => output.push(other.as_os_str()),
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, thread, time::Duration};
    use tempfile::tempdir;

    fn actor() -> AgentDescriptor {
        AgentDescriptor {
            id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            team_id: Uuid::new_v4(),
            parent_id: None,
            parent_session_id: None,
            level: 0,
            name: "test".into(),
            role: "test".into(),
            status: crate::model::AgentStatus::Running,
            permission_level: pi_whim_core::AgentPermissionLevel::Full,
        }
    }

    #[test]
    fn write_and_edit_report_revisions() {
        let directory = tempdir().unwrap();
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());
        let path = directory.path().join("sample.txt");
        coordinator
            .write(
                &actor(),
                "w",
                WriteArguments {
                    path: "sample.txt".into(),
                    content: "one\ntwo\n".into(),
                    base_revision: None,
                },
            )
            .unwrap();
        let read = coordinator
            .read(
                &actor(),
                "r",
                ReadArguments {
                    path: "sample.txt".into(),
                    offset: None,
                    limit: None,
                    mode: "raw".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap();
        let revision = read["details"]["revision"].as_str().unwrap().to_owned();
        coordinator
            .edit(
                &actor(),
                "e",
                EditArguments {
                    path: "sample.txt".into(),
                    edits: vec![ReplaceEdit {
                        old_text: "two".into(),
                        new_text: "THREE".into(),
                    }],
                    base_revision: Some(revision),
                },
            )
            .unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "one\nTHREE\n");
    }

    #[test]
    fn disjoint_stale_edits_rebase_and_overlap_reports_prior_agents() {
        let directory = tempdir().unwrap();
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());
        let first = actor();
        let second = actor();
        coordinator
            .write(
                &first,
                "seed",
                WriteArguments {
                    path: "sample.txt".into(),
                    content: "alpha\nbeta\ngamma\n".into(),
                    base_revision: None,
                },
            )
            .unwrap();
        let original_revision = revision(b"alpha\nbeta\ngamma\n");
        coordinator
            .edit(
                &first,
                "first-edit",
                EditArguments {
                    path: "sample.txt".into(),
                    edits: vec![ReplaceEdit {
                        old_text: "alpha".into(),
                        new_text: "ALPHA".into(),
                    }],
                    base_revision: Some(original_revision.clone()),
                },
            )
            .unwrap();
        let rebased = coordinator
            .edit(
                &second,
                "second-edit",
                EditArguments {
                    path: "sample.txt".into(),
                    edits: vec![ReplaceEdit {
                        old_text: "gamma".into(),
                        new_text: "GAMMA".into(),
                    }],
                    base_revision: Some(original_revision.clone()),
                },
            )
            .unwrap();
        assert_eq!(rebased["details"]["rebased"], true);

        let conflict = coordinator
            .edit(
                &second,
                "overlap",
                EditArguments {
                    path: "sample.txt".into(),
                    edits: vec![ReplaceEdit {
                        old_text: "alpha".into(),
                        new_text: "again".into(),
                    }],
                    base_revision: Some(original_revision),
                },
            )
            .unwrap_err();
        assert_eq!(conflict.code, "file_conflict");
        assert!(
            conflict.details["previous_operations"]
                .as_array()
                .unwrap()
                .len()
                >= 2
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("sample.txt")).unwrap(),
            "ALPHA\nbeta\nGAMMA\n"
        );
    }

    #[test]
    fn paths_cannot_escape_the_project_root() {
        let directory = tempdir().unwrap();
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());
        let mut restricted_actor = actor();
        restricted_actor.permission_level = pi_whim_core::AgentPermissionLevel::Controlled;
        let error = coordinator
            .write(
                &restricted_actor,
                "escape",
                WriteArguments {
                    path: "../outside.txt".into(),
                    content: "blocked".into(),
                    base_revision: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "file_forbidden");
    }

    #[test]
    fn full_agents_can_use_host_paths_but_controlled_agents_cannot() {
        let project = tempdir().unwrap();
        let host = tempdir().unwrap();
        let coordinator = FileCoordinator::for_project(project.path().to_path_buf());
        let host_path = host.path().join("host.txt");
        fs::write(&host_path, "host content").unwrap();

        let mut controlled = actor();
        controlled.permission_level = AgentPermissionLevel::Controlled;
        let denied = coordinator
            .read(
                &controlled,
                "controlled-host-read",
                ReadArguments {
                    path: host_path.to_string_lossy().into_owned(),
                    offset: None,
                    limit: None,
                    mode: "raw".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap_err();
        assert_eq!(denied.code, "file_forbidden");

        let allowed = coordinator
            .read(
                &actor(),
                "full-host-read",
                ReadArguments {
                    path: host_path.to_string_lossy().into_owned(),
                    offset: None,
                    limit: None,
                    mode: "raw".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap();
        assert_eq!(allowed["text"], "host content");
    }

    #[test]
    fn read_lists_directory_in_sorted_pages_and_rejects_stale_snapshots() {
        let directory = tempdir().unwrap();
        for name in ["zeta.txt", "Alpha.txt", "middle"] {
            fs::write(directory.path().join(name), name).unwrap();
        }
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());
        let first = coordinator
            .read(
                &actor(),
                "directory-first",
                ReadArguments {
                    path: ".".into(),
                    offset: None,
                    limit: Some(2),
                    mode: "auto".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap();
        assert_eq!(first["details"]["entries"][0]["name"], "Alpha.txt");
        let cursor = first["details"]["next_cursor"].as_str().unwrap().to_owned();
        fs::write(directory.path().join("new.txt"), "new").unwrap();
        let stale = coordinator
            .read(
                &actor(),
                "directory-stale",
                ReadArguments {
                    path: ".".into(),
                    offset: None,
                    limit: None,
                    mode: "auto".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: Some(cursor),
                },
            )
            .unwrap_err();
        assert_eq!(stale.code, "stale_snapshot");
    }

    #[test]
    fn read_returns_binary_file_metadata_instead_of_an_error() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("archive.bin"), [0, 159, 255]).unwrap();
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());
        let result = coordinator
            .read(
                &actor(),
                "binary",
                ReadArguments {
                    path: "archive.bin".into(),
                    offset: None,
                    limit: None,
                    mode: "auto".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap();
        assert_eq!(result["details"]["format"], "metadata");
        assert_eq!(result["details"]["size"], 3);
        assert_eq!(result["details"]["file_type"], "file");
    }

    #[test]
    fn large_images_recommend_raw_or_subagent_and_raw_returns_all_bytes() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("large.png");
        let mut image = vec![0; MAX_IMAGE_RESPONSE_BYTES + 1];
        image[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        fs::write(&path, &image).unwrap();
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());

        let error = coordinator
            .read(
                &actor(),
                "large-image-auto",
                ReadArguments {
                    path: path.to_string_lossy().into_owned(),
                    offset: None,
                    limit: None,
                    mode: "auto".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "file_too_large");
        assert!(error.message.contains("mode"));
        assert!(error.message.contains("raw"));
        assert!(error.message.contains("subagent"));
        assert_eq!(error.details["retry"]["mode"], "raw");

        let result = coordinator
            .read(
                &actor(),
                "large-image-raw",
                ReadArguments {
                    path: path.to_string_lossy().into_owned(),
                    offset: None,
                    limit: None,
                    mode: "raw".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap();
        let returned = BASE64
            .decode(result["image"]["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(returned, image);
        assert_eq!(result["details"]["bytes"], MAX_IMAGE_RESPONSE_BYTES + 1);
        assert_eq!(result["details"]["complete"], true);
    }

    #[test]
    fn coordinators_are_shared_per_project_and_reads_supply_write_bases() {
        let directory = tempdir().unwrap();
        let first = FileCoordinator::for_project(directory.path().to_path_buf());
        let second = FileCoordinator::for_project(directory.path().to_path_buf());
        assert!(Arc::ptr_eq(&first, &second));
        let owner = actor();
        let writer = actor();
        first
            .write(
                &owner,
                "seed",
                WriteArguments {
                    path: "sample.txt".into(),
                    content: "original".into(),
                    base_revision: None,
                },
            )
            .unwrap();
        second
            .read(
                &writer,
                "read",
                ReadArguments {
                    path: "sample.txt".into(),
                    offset: None,
                    limit: None,
                    mode: "raw".into(),
                    max_tokens: None,
                    max_bytes: None,
                    snapshot_id: None,
                    cursor: None,
                },
            )
            .unwrap();
        first
            .write(
                &owner,
                "mutate",
                WriteArguments {
                    path: "sample.txt".into(),
                    content: "changed".into(),
                    base_revision: None,
                },
            )
            .unwrap();
        let error = second
            .write(
                &writer,
                "stale-write",
                WriteArguments {
                    path: "sample.txt".into(),
                    content: "must not overwrite".into(),
                    base_revision: None,
                },
            )
            .unwrap_err();
        assert_eq!(error.code, "file_conflict");
        assert_eq!(
            fs::read_to_string(directory.path().join("sample.txt")).unwrap(),
            "changed"
        );
    }

    #[test]
    fn edit_preserves_bom_and_crlf() {
        let directory = tempdir().unwrap();
        let coordinator = FileCoordinator::for_project(directory.path().to_path_buf());
        fs::write(
            directory.path().join("windows.txt"),
            "\u{feff}one\r\ntwo\r\n",
        )
        .unwrap();
        coordinator
            .edit(
                &actor(),
                "crlf-edit",
                EditArguments {
                    path: "windows.txt".into(),
                    edits: vec![ReplaceEdit {
                        old_text: "two".into(),
                        new_text: "three".into(),
                    }],
                    base_revision: None,
                },
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(directory.path().join("windows.txt")).unwrap(),
            "\u{feff}one\r\nthree\r\n"
        );
    }

    #[test]
    fn queued_writer_runs_before_a_later_reader() {
        let lane = Arc::new(FileLane::default());
        let initial_read = lane.acquire_read();
        let (events_tx, events_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();

        let writer_lane = lane.clone();
        let writer_events = events_tx.clone();
        let writer = thread::spawn(move || {
            let _permit = writer_lane.acquire_write();
            writer_events.send("writer").unwrap();
            release_rx.recv().unwrap();
        });
        for _ in 0..100 {
            if !lane.state.lock().unwrap().writers.is_empty() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!lane.state.lock().unwrap().writers.is_empty());

        let reader_lane = lane.clone();
        let reader = thread::spawn(move || {
            let _permit = reader_lane.acquire_read();
            events_tx.send("reader").unwrap();
        });
        drop(initial_read);
        assert_eq!(
            events_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "writer"
        );
        release_tx.send(()).unwrap();
        assert_eq!(
            events_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            "reader"
        );
        writer.join().unwrap();
        reader.join().unwrap();
    }
}
