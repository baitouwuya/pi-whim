//! The pool of live Pi processes, one per session.
//!
//! Each session owns its own process, so switching which session is visible
//! never interrupts one that is working. Sessions are keyed by their Pi
//! transcript path, or a `draft://` key until Pi reports where it wrote the
//! file — which is why [`SessionPool::rekey`] exists.
//!
//! This type does the bookkeeping only: membership, which session is visible,
//! most-recently-used ordering, and the per-session state a turn accumulates.
//! Deciding what a key change means for the conversation view is the caller's
//! business, so the mutating methods report what happened rather than
//! dispatching actions themselves.

use std::collections::HashMap;

use pi_whim_core::{Attachment, ProjectId, SubmitMode};
use pi_whim_runtime::{AgentRuntime, RuntimeEvent};

/// Prefix for sessions Pi has not yet written a transcript for.
pub const DRAFT_PREFIX: &str = "draft://";

/// Whether `key` names a session that exists only in memory so far.
pub fn is_draft(key: &str) -> bool {
    key.starts_with(DRAFT_PREFIX)
}

/// One Pi process and the state its current turn accumulates.
pub struct SessionRuntime<R: AgentRuntime> {
    pub runtime: R,
    pub events: crossbeam_channel::Receiver<RuntimeEvent>,
    pub project_id: ProjectId,
    /// True while the agent is streaming or compacting.
    pub running: bool,
    pub assistant_message_id: Option<String>,
    pub conversation_compacted: bool,
    /// Conversation entry for the in-progress compaction card, so the result
    /// updates that card instead of adding a second one.
    pub compaction_item_id: Option<String>,
    pub pending_prompt: Option<(String, Vec<Attachment>, SubmitMode)>,
    /// When the session was last made visible; drives most-recently-used picks.
    pub last_used_ms: i64,
}

impl<R: AgentRuntime> SessionRuntime<R> {
    pub fn new(
        runtime: R,
        events: crossbeam_channel::Receiver<RuntimeEvent>,
        project_id: ProjectId,
        now_ms: i64,
    ) -> Self {
        Self {
            runtime,
            events,
            project_id,
            running: false,
            assistant_message_id: None,
            conversation_compacted: false,
            compaction_item_id: None,
            pending_prompt: None,
            last_used_ms: now_ms,
        }
    }
}

/// What changed when a session was re-keyed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rekeyed {
    /// True when the re-keyed session was the visible one, so the caller knows
    /// to point the conversation view at the new key.
    pub was_active: bool,
}

/// Live Pi processes, keyed by session.
pub struct SessionPool<R: AgentRuntime> {
    sessions: HashMap<String, SessionRuntime<R>>,
    active: Option<String>,
}

impl<R: AgentRuntime> Default for SessionPool<R> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            active: None,
        }
    }
}

impl<R: AgentRuntime> SessionPool<R> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, key: impl Into<String>, session: SessionRuntime<R>) {
        self.sessions.insert(key.into(), session);
    }

    pub fn get(&self, key: &str) -> Option<&SessionRuntime<R>> {
        self.sessions.get(key)
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut SessionRuntime<R>> {
        self.sessions.get_mut(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.sessions.contains_key(key)
    }

    pub fn remove(&mut self, key: &str) -> Option<SessionRuntime<R>> {
        if self.active.as_deref() == Some(key) {
            self.active = None;
        }
        self.sessions.remove(key)
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.sessions.keys().map(String::as_str)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &SessionRuntime<R>)> {
        self.sessions
            .iter()
            .map(|(key, session)| (key.as_str(), session))
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// The key of the visible session.
    pub fn active_key(&self) -> Option<&str> {
        self.active.as_deref()
    }

    pub fn active(&self) -> Option<&SessionRuntime<R>> {
        self.active
            .as_ref()
            .and_then(|key| self.sessions.get(key.as_str()))
    }

    pub fn active_mut(&mut self) -> Option<&mut SessionRuntime<R>> {
        let key = self.active.clone()?;
        self.sessions.get_mut(&key)
    }

    /// Make `key` the visible session, returning it.
    ///
    /// Touches the most-recently-used stamp. Returns `None` if no such session
    /// is pooled, leaving the current selection alone.
    pub fn activate(&mut self, key: &str, now_ms: i64) -> Option<&SessionRuntime<R>> {
        let session = self.sessions.get_mut(key)?;
        session.last_used_ms = now_ms;
        self.active = Some(key.to_owned());
        self.sessions.get(key)
    }

    /// Move a session to a new key, for when Pi reports its transcript path.
    ///
    /// Returns `None` when there was nothing to move, or when the key is
    /// unchanged.
    pub fn rekey(&mut self, from: &str, to: &str, now_ms: i64) -> Option<Rekeyed> {
        if from == to {
            return None;
        }
        let mut session = self.sessions.remove(from)?;
        session.last_used_ms = now_ms;
        let was_active = self.active.as_deref() == Some(from);
        self.sessions.insert(to.to_owned(), session);
        if was_active {
            self.active = Some(to.to_owned());
        }
        Some(Rekeyed { was_active })
    }

    /// Remove every session belonging to `project_id`, returning their keys.
    pub fn remove_project(&mut self, project_id: ProjectId) -> Vec<String> {
        let keys: Vec<String> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.project_id == project_id)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &keys {
            self.remove(key);
        }
        keys
    }

    /// The most recently used session for `project_id`.
    pub fn most_recent_in(&self, project_id: ProjectId) -> Option<&str> {
        self.sessions
            .iter()
            .filter(|(_, session)| session.project_id == project_id)
            .max_by_key(|(_, session)| session.last_used_ms)
            .map(|(key, _)| key.as_str())
    }

    /// Whether any pooled session is mid-turn.
    pub fn any_running(&self) -> bool {
        self.sessions.values().any(|session| session.running)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_runtime::FakeRuntime;
    use uuid::Uuid;

    fn session(project_id: ProjectId, now_ms: i64) -> SessionRuntime<FakeRuntime> {
        let runtime = FakeRuntime::default();
        let (_sender, events) = crossbeam_channel::unbounded();
        SessionRuntime::new(runtime, events, project_id, now_ms)
    }

    #[test]
    fn draft_keys_are_recognized() {
        assert!(is_draft("draft://abc"));
        assert!(!is_draft("/tmp/session.jsonl"));
    }

    #[test]
    fn activating_a_missing_session_leaves_the_selection_alone() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("a", session(project, 1));
        pool.activate("a", 2);

        assert!(pool.activate("missing", 3).is_none());
        assert_eq!(pool.active_key(), Some("a"));
    }

    #[test]
    fn activating_touches_the_recently_used_stamp() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("a", session(project, 1));
        pool.insert("b", session(project, 2));

        pool.activate("a", 99);

        assert_eq!(pool.most_recent_in(project), Some("a"));
    }

    #[test]
    fn rekeying_carries_the_session_and_reports_whether_it_was_visible() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("draft://x", session(project, 1));
        pool.activate("draft://x", 2);

        let outcome = pool.rekey("draft://x", "/tmp/s.jsonl", 3).unwrap();

        assert!(outcome.was_active);
        assert_eq!(pool.active_key(), Some("/tmp/s.jsonl"));
        assert!(!pool.contains("draft://x"));
        assert!(pool.contains("/tmp/s.jsonl"));
    }

    #[test]
    fn rekeying_a_background_session_does_not_change_the_selection() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("visible", session(project, 1));
        pool.insert("draft://x", session(project, 1));
        pool.activate("visible", 2);

        let outcome = pool.rekey("draft://x", "/tmp/s.jsonl", 3).unwrap();

        assert!(!outcome.was_active);
        assert_eq!(pool.active_key(), Some("visible"));
    }

    #[test]
    fn rekeying_an_unchanged_or_absent_key_reports_nothing() {
        let mut pool = SessionPool::new();
        pool.insert("a", session(Uuid::new_v4(), 1));

        assert!(pool.rekey("a", "a", 2).is_none());
        assert!(pool.rekey("missing", "b", 2).is_none());
        assert!(pool.contains("a"));
    }

    #[test]
    fn removing_the_visible_session_clears_the_selection() {
        let mut pool = SessionPool::new();
        pool.insert("a", session(Uuid::new_v4(), 1));
        pool.activate("a", 2);

        pool.remove("a");

        assert_eq!(pool.active_key(), None);
        assert!(pool.is_empty());
    }

    #[test]
    fn removing_a_background_session_keeps_the_selection() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("visible", session(project, 1));
        pool.insert("other", session(project, 1));
        pool.activate("visible", 2);

        pool.remove("other");

        assert_eq!(pool.active_key(), Some("visible"));
    }

    #[test]
    fn removing_a_project_takes_only_its_sessions() {
        let mut pool = SessionPool::new();
        let kept = Uuid::new_v4();
        let removed = Uuid::new_v4();
        pool.insert("keep", session(kept, 1));
        pool.insert("drop-1", session(removed, 1));
        pool.insert("drop-2", session(removed, 1));

        let mut keys = pool.remove_project(removed);
        keys.sort();

        assert_eq!(keys, vec!["drop-1".to_owned(), "drop-2".to_owned()]);
        assert!(pool.contains("keep"));
        assert_eq!(pool.most_recent_in(removed), None);
    }

    #[test]
    fn removing_a_project_clears_the_selection_when_it_owned_it() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("a", session(project, 1));
        pool.activate("a", 2);

        pool.remove_project(project);

        assert_eq!(pool.active_key(), None);
    }

    #[test]
    fn running_state_is_visible_across_the_pool() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("a", session(project, 1));
        pool.insert("b", session(project, 1));

        assert!(!pool.any_running());

        // A background session working still counts.
        pool.get_mut("b").unwrap().running = true;
        pool.activate("a", 2);

        assert!(pool.any_running());
    }

    #[test]
    fn most_recent_ignores_other_projects() {
        let mut pool = SessionPool::new();
        let mine = Uuid::new_v4();
        let theirs = Uuid::new_v4();
        pool.insert("mine", session(mine, 1));
        pool.insert("theirs", session(theirs, 100));

        assert_eq!(pool.most_recent_in(mine), Some("mine"));
    }
}
