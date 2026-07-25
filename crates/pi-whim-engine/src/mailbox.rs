//! One event stream for every session.
//!
//! Each Pi process gets its own crossbeam receiver, which suited a UI that could
//! poll all of them once a frame. A retained-mode UI has no frame to poll on: it
//! needs a single handle a worker can block on, waking only when something
//! actually arrived.
//!
//! So sessions are merged here. A forwarder per session blocks on that session's
//! receiver and republishes onto one shared channel, which [`Mailbox::events`]
//! hands out.
//!
//! What a forwarder republishes is a [`SessionToken`], not the pool key. Keys
//! move: a session starts as `draft://…` and is re-keyed once Pi reports where it
//! wrote the transcript. A forwarder that captured the key at launch would keep
//! labelling events with the old one, and every event after the rekey would be
//! attributed to a session that no longer exists. A token is assigned once and
//! never changes, so the rekey is invisible to the forwarder and the consumer
//! resolves the current key at delivery time.

use std::sync::atomic::{AtomicU64, Ordering};

use pi_whim_runtime::RuntimeEvent;

/// A session's identity for the lifetime of its process.
///
/// Distinct from the pool key, which is the transcript path and changes when Pi
/// reports one. Assigned at launch and never reused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionToken(u64);

impl SessionToken {
    /// Mint a token that no session has held before.
    pub fn next() -> Self {
        // Relaxed is enough: the only requirement is that no two calls agree, and
        // a fetch_add gives that regardless of ordering with other memory.
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The raw value, for logging and test assertions.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// An event, and which session produced it.
pub type Delivery = (SessionToken, RuntimeEvent);

/// Every session's events on one channel.
pub struct Mailbox {
    sender: crossbeam_channel::Sender<Delivery>,
    receiver: crossbeam_channel::Receiver<Delivery>,
}

impl Default for Mailbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Mailbox {
    pub fn new() -> Self {
        // Unbounded because the alternative is worse: a full bounded channel
        // would block the forwarder, which would stall the Pi process behind it
        // rather than merely using memory.
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self { sender, receiver }
    }

    /// Start republishing `events` under `token`.
    ///
    /// The forwarder ends when the session's channel disconnects, which is what
    /// happens when its process exits, so nothing has to shut it down.
    pub fn forward(&self, token: SessionToken, events: crossbeam_channel::Receiver<RuntimeEvent>) {
        let sender = self.sender.clone();
        // A thread per session rather than one `Select` over every receiver:
        // `Select` cannot be extended while it is blocked, so adding a session
        // would need a control channel to interrupt it. A thread that blocks on
        // one receiver costs little beside the Pi process it is following.
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                // A send failure means the mailbox is gone, so there is nobody
                // left to tell.
                if sender.send((token, event)).is_err() {
                    return;
                }
            }
        });
    }

    /// The merged stream.
    ///
    /// Cloneable and blocking: `recv` on this is what a background worker waits
    /// on instead of polling once a frame.
    pub fn events(&self) -> crossbeam_channel::Receiver<Delivery> {
        self.receiver.clone()
    }

    /// Take what has already arrived, without waiting.
    ///
    /// For a host that drives from a frame loop rather than a worker.
    pub fn try_drain(&self) -> Vec<Delivery> {
        self.receiver.try_iter().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_unique() {
        // Two sessions sharing a token would cross their event streams.
        let first = SessionToken::next();
        let second = SessionToken::next();
        assert_ne!(first, second);
    }

    #[test]
    fn events_arrive_tagged_with_their_session() {
        let mailbox = Mailbox::new();
        let (first_sender, first_events) = crossbeam_channel::unbounded();
        let (second_sender, second_events) = crossbeam_channel::unbounded();
        let first = SessionToken::next();
        let second = SessionToken::next();
        mailbox.forward(first, first_events);
        mailbox.forward(second, second_events);

        first_sender
            .send(RuntimeEvent::Stderr("from first".into()))
            .unwrap();
        second_sender
            .send(RuntimeEvent::Stderr("from second".into()))
            .unwrap();

        // Blocking rather than draining: two forwarder threads race, so the order
        // is not fixed and only the pairing is asserted.
        let events = mailbox.events();
        let mut seen = vec![
            events.recv().expect("the first event"),
            events.recv().expect("the second event"),
        ];
        seen.sort_by_key(|(token, _)| token.get());

        let labelled: Vec<_> = seen
            .into_iter()
            .map(|(token, event)| match event {
                RuntimeEvent::Stderr(message) => (token, message),
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(
            labelled,
            vec![
                (first, "from first".to_owned()),
                (second, "from second".to_owned())
            ]
        );
    }

    #[test]
    fn a_session_that_exits_ends_its_forwarder_without_ending_the_others() {
        // One Pi process dying must not take the stream down: the other sessions
        // are still running and their events still have to arrive.
        let mailbox = Mailbox::new();
        let (dying_sender, dying_events) = crossbeam_channel::unbounded();
        let (live_sender, live_events) = crossbeam_channel::unbounded();
        mailbox.forward(SessionToken::next(), dying_events);
        let live = SessionToken::next();
        mailbox.forward(live, live_events);

        drop(dying_sender);
        live_sender
            .send(RuntimeEvent::Stderr("still here".into()))
            .unwrap();

        let (token, event) = mailbox.events().recv().expect("the live session's event");
        assert_eq!(token, live);
        assert!(matches!(event, RuntimeEvent::Stderr(message) if message == "still here"));
    }

    #[test]
    fn draining_takes_what_arrived_and_does_not_block() {
        let mailbox = Mailbox::new();
        let (sender, events) = crossbeam_channel::unbounded();
        let token = SessionToken::next();
        mailbox.forward(token, events);

        sender.send(RuntimeEvent::Stderr("one".into())).unwrap();
        // Wait for the forwarder rather than sleeping: draining immediately would
        // race the thread and make this test flaky.
        let arrived = mailbox.events().recv().expect("the forwarded event");
        assert_eq!(arrived.0, token);

        // Nothing further is waiting, and asking must return rather than block.
        assert!(mailbox.try_drain().is_empty());
    }
}
