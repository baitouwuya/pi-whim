//! Messages the engine needs to put in front of the user.
//!
//! Orchestration fails in ways only a person can resolve — a project directory
//! that has moved, a provider with no key, `gh` not installed — and it also has
//! things worth reporting that are not failures, like a share URL.
//!
//! The egui app kept these in two `Option<String>` fields on the application
//! struct, which meant only the newest survived: a project failing to open while
//! an earlier error was still on screen dropped one of them silently. An engine
//! cannot hold a view field anyway, so they queue here and the view drains them.

use std::collections::VecDeque;

/// How much attention a message deserves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// Something failed and the user likely has to act.
    Error,
    /// Something worth knowing that is not a failure.
    Info,
}

/// One message bound for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub level: Level,
    pub message: String,
}

impl Notice {
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            message: message.into(),
        }
    }

    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: Level::Info,
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.level == Level::Error
    }
}

/// Pending messages, oldest first.
///
/// Bounded, because a wedged process reporting the same failure every frame
/// should not grow this without limit. When full, the oldest goes — the newest
/// message describes the current state, so that is the one worth keeping.
#[derive(Debug, Default)]
pub struct Outbox {
    pending: VecDeque<Notice>,
}

/// Past this, older messages are dropped.
const CAPACITY: usize = 32;

impl Outbox {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, notice: Notice) {
        // Repeating the same message adds nothing; a retry loop would otherwise
        // fill the queue with one identical error.
        if self.pending.back() == Some(&notice) {
            return;
        }
        if self.pending.len() == CAPACITY {
            self.pending.pop_front();
        }
        self.pending.push_back(notice);
    }

    pub fn error(&mut self, message: impl Into<String>) {
        self.push(Notice::error(message));
    }

    pub fn info(&mut self, message: impl Into<String>) {
        self.push(Notice::info(message));
    }

    /// Take the next message, if any.
    pub fn take(&mut self) -> Option<Notice> {
        self.pending.pop_front()
    }

    /// Take everything pending, for a view that shows more than one at a time.
    pub fn drain(&mut self) -> Vec<Notice> {
        self.pending.drain(..).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_come_back_in_the_order_they_were_reported() {
        let mut outbox = Outbox::new();
        outbox.error("first");
        outbox.info("second");

        assert_eq!(outbox.take(), Some(Notice::error("first")));
        assert_eq!(outbox.take(), Some(Notice::info("second")));
        assert_eq!(outbox.take(), None);
    }

    #[test]
    fn several_failures_all_survive() {
        // The egui build held one Option<String>, so a second failure overwrote
        // the first before anyone had read it.
        let mut outbox = Outbox::new();
        outbox.error("project is missing");
        outbox.error("provider has no key");

        assert_eq!(outbox.len(), 2);
    }

    #[test]
    fn an_immediate_repeat_is_not_queued_twice() {
        // A retry loop reporting the same failure every frame would otherwise fill
        // the queue with one message.
        let mut outbox = Outbox::new();
        outbox.error("Pi is unavailable");
        outbox.error("Pi is unavailable");

        assert_eq!(outbox.len(), 1);
    }

    #[test]
    fn a_repeat_after_something_else_is_kept() {
        // Alternating failures are a real sequence, not a stuck retry.
        let mut outbox = Outbox::new();
        outbox.error("a");
        outbox.error("b");
        outbox.error("a");

        assert_eq!(outbox.len(), 3);
    }

    #[test]
    fn the_same_text_at_different_levels_is_two_messages() {
        let mut outbox = Outbox::new();
        outbox.error("done");
        outbox.info("done");

        assert_eq!(outbox.len(), 2);
    }

    #[test]
    fn the_queue_is_bounded_and_keeps_the_newest() {
        // The newest message describes the current state.
        let mut outbox = Outbox::new();
        for index in 0..CAPACITY + 10 {
            outbox.error(format!("failure {index}"));
        }

        assert_eq!(outbox.len(), CAPACITY);
        let first = outbox.take().expect("a message");
        assert_eq!(first, Notice::error("failure 10"));
    }

    #[test]
    fn draining_empties_the_queue() {
        let mut outbox = Outbox::new();
        outbox.error("a");
        outbox.info("b");

        let drained = outbox.drain();

        assert_eq!(drained.len(), 2);
        assert!(outbox.is_empty());
        assert_eq!(outbox.take(), None);
    }

    #[test]
    fn levels_are_distinguishable_without_reading_the_text() {
        assert!(Notice::error("x").is_error());
        assert!(!Notice::info("x").is_error());
    }
}
