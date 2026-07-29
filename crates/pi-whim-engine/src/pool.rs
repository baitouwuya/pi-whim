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

use crate::mailbox::{Delivery, Mailbox, SessionToken};

/// Prefix for sessions Pi has not yet written a transcript for.
pub const DRAFT_PREFIX: &str = "draft://";

/// Whether `key` names a session that exists only in memory so far.
pub fn is_draft(key: &str) -> bool {
    key.starts_with(DRAFT_PREFIX)
}

/// A prompt held back until something else finishes.
pub type PendingPrompt = (String, Vec<Attachment>, SubmitMode);

/// What a session accumulates over one turn.
///
/// Split out from [`SessionRuntime`] so event translation can borrow it
/// mutably without also borrowing the process: `translate` needs to update the
/// streaming message id while reading the conversation, and holding the whole
/// session would put the runtime in that borrow for no reason.
#[derive(Debug, Default, PartialEq)]
pub struct Turn {
    /// True while the agent is streaming or compacting.
    pub running: bool,
    /// The entry a stream is appending to. Pi renames it once the model has
    /// answered, so this is a placeholder until then.
    pub assistant_message_id: Option<String>,
    pub conversation_compacted: bool,
    /// Conversation entry for the in-progress compaction card, so the result
    /// updates that card instead of adding a second one.
    pub compaction_item_id: Option<String>,
    /// A provider may end a stream successfully without returning any content.
    /// Keep that protocol failure through `agent_settled`, which would otherwise
    /// overwrite the visible failure with `Ready` immediately.
    pub reply_error: Option<String>,
    pub pending_prompt: Option<PendingPrompt>,
}

/// One Pi process and the state its current turn accumulates.
pub struct SessionRuntime<R: AgentRuntime> {
    pub runtime: R,
    /// This process's events, until the pool takes them for its mailbox.
    ///
    /// Not public, and taken rather than cloned: a cloned crossbeam receiver
    /// competes with the original for each event, so two readers would split the
    /// stream between them rather than both seeing it.
    events: Option<crossbeam_channel::Receiver<RuntimeEvent>>,
    /// Identity that survives a rekey, for labelling this session's events.
    ///
    /// Minted here rather than passed in so no session can be pooled without
    /// one, and so two sessions cannot be given the same token by mistake.
    pub token: SessionToken,
    pub project_id: ProjectId,
    pub turn: Turn,
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
            events: Some(events),
            token: SessionToken::next(),
            project_id,
            turn: Turn::default(),
            last_used_ms: now_ms,
        }
    }

    /// Whether this session is mid-turn.
    pub fn is_running(&self) -> bool {
        self.turn.running
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
    /// Every pooled session's events on one channel.
    mailbox: Mailbox,
}

impl<R: AgentRuntime> Default for SessionPool<R> {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            active: None,
            mailbox: Mailbox::new(),
        }
    }
}

impl<R: AgentRuntime> SessionPool<R> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Pool a session and start merging its events.
    pub fn insert(&mut self, key: impl Into<String>, mut session: SessionRuntime<R>) {
        // Merged on the way in rather than by a separate call, so a session
        // cannot be pooled with its events going nowhere.
        if let Some(events) = session.events.take() {
            self.mailbox.forward(session.token, events);
        }
        self.sessions.insert(key.into(), session);
    }

    /// The merged event stream, for a worker to block on.
    ///
    /// Deliveries are labelled with a [`SessionToken`]; resolve it with
    /// [`Self::key_for`] when the event is handled, not before.
    pub fn events(&self) -> crossbeam_channel::Receiver<Delivery> {
        self.mailbox.events()
    }

    /// Every event that has arrived since the last call, without waiting.
    pub fn drain_events(&self) -> Vec<Delivery> {
        self.mailbox.try_drain()
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
        self.sessions.values().any(SessionRuntime::is_running)
    }

    /// The key `token` currently names.
    ///
    /// Events are labelled with a token because a key can change while they are
    /// in flight, so the key is resolved when one is delivered rather than when
    /// its session was pooled. Returns `None` once the session has been removed,
    /// which is the ordinary way a late event from an exited process arrives.
    pub fn key_for(&self, token: SessionToken) -> Option<&str> {
        self.sessions
            .iter()
            .find(|(_, session)| session.token == token)
            .map(|(key, _)| key.as_str())
    }

    /// The token of the session at `key`.
    pub fn token_for(&self, key: &str) -> Option<SessionToken> {
        self.sessions.get(key).map(|session| session.token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_runtime::FakeRuntime;
    use uuid::Uuid;

    fn session(project_id: ProjectId, now_ms: i64) -> SessionRuntime<FakeRuntime> {
        with_events(project_id, now_ms).1
    }

    /// A session and the sender that plays its process, for the merge tests.
    fn with_events(
        project_id: ProjectId,
        now_ms: i64,
    ) -> (
        crossbeam_channel::Sender<RuntimeEvent>,
        SessionRuntime<FakeRuntime>,
    ) {
        let (sender, events) = crossbeam_channel::unbounded();
        let session = SessionRuntime::new(FakeRuntime::default(), events, project_id, now_ms);
        (sender, session)
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
        pool.get_mut("b").unwrap().turn.running = true;
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

    #[test]
    fn a_token_follows_its_session_through_a_rekey() {
        // The whole reason events carry a token: a rekey happens while events
        // are in flight, and one labelled with the old key would be dropped.
        let mut pool = SessionPool::new();
        pool.insert("draft://x", session(Uuid::new_v4(), 1));
        let token = pool.token_for("draft://x").expect("the pooled session");

        pool.rekey("draft://x", "/tmp/s.jsonl", 2);

        assert_eq!(pool.key_for(token), Some("/tmp/s.jsonl"));
    }

    #[test]
    fn each_pooled_session_gets_its_own_token() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        pool.insert("a", session(project, 1));
        pool.insert("b", session(project, 1));

        assert_ne!(pool.token_for("a"), pool.token_for("b"));
    }

    #[test]
    fn every_pooled_sessions_events_arrive_on_one_stream() {
        let mut pool = SessionPool::new();
        let project = Uuid::new_v4();
        let (first_sender, first) = with_events(project, 1);
        let (second_sender, second) = with_events(project, 1);
        pool.insert("a", first);
        pool.insert("b", second);

        first_sender.send(RuntimeEvent::Stderr("a".into())).unwrap();
        second_sender
            .send(RuntimeEvent::Stderr("b".into()))
            .unwrap();

        // Both forwarders are threads, so wait for two rather than draining.
        let events = pool.events();
        let mut keys: Vec<String> = (0..2)
            .map(|_| {
                let (token, _) = events.recv().expect("a forwarded event");
                pool.key_for(token).expect("its session").to_owned()
            })
            .collect();
        keys.sort();
        assert_eq!(keys, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn an_event_sent_before_a_rekey_resolves_to_the_new_key() {
        // Pi reports its transcript path partway through the first turn, so the
        // events already in the channel were sent under the draft key. Resolving
        // at delivery is what keeps them attached to the session.
        let mut pool = SessionPool::new();
        let (sender, session) = with_events(Uuid::new_v4(), 1);
        pool.insert("draft://x", session);

        sender.send(RuntimeEvent::Stderr("early".into())).unwrap();
        let (token, _) = pool.events().recv().expect("the forwarded event");
        pool.rekey("draft://x", "/tmp/s.jsonl", 2);

        assert_eq!(pool.key_for(token), Some("/tmp/s.jsonl"));
    }

    #[test]
    fn a_removed_sessions_token_resolves_to_nothing() {
        // A process that exits can still have events queued behind it. They
        // resolve to no key, which is how the caller knows to discard them
        // rather than attributing them to whichever session took the key.
        let mut pool = SessionPool::new();
        pool.insert("a", session(Uuid::new_v4(), 1));
        let token = pool.token_for("a").expect("the pooled session");

        pool.remove("a");

        assert_eq!(pool.key_for(token), None);
    }
}
