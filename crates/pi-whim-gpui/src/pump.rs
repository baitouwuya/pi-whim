//! Waking the window when a Pi process says something.
//!
//! The egui build asked every session for its events once a frame, which cost a
//! redraw whether or not anything had happened. A retained-mode window has no
//! frame to hang that off, so the events have to do the waking themselves.
//!
//! `pi_whim_engine::mailbox` merges every session onto one crossbeam channel.
//! Blocking on it is the cheap way to wait — no polling, no timer — but a
//! blocking `recv` is not a future, so it cannot be awaited on the main thread
//! without freezing the window. The loop therefore alternates: block on a
//! background thread, hand the batch back to the main thread, repeat.
//!
//! The returned [`gpui::Task`] must be **stored**, not detached. Dropping a task
//! cancels it, which is exactly the lifetime wanted: when the view that owns the
//! pump goes away, the loop stops with it.

use gpui::{AsyncWindowContext, Context, Task, WeakEntity, Window};
use pi_whim_engine::mailbox::{Delivery, RuntimeEvent, SessionToken};

/// How many deliveries one main-thread visit will take.
///
/// A batch bound rather than one-at-a-time: a streaming turn produces events far
/// faster than a display refreshes, and a hop to the main thread per token would
/// spend more time scheduling than rendering. Bounded rather than unbounded so a
/// long backlog still yields between batches instead of holding the main thread
/// for as long as the events keep coming.
const BATCH: usize = 64;

/// What the pump does with each batch it collects.
///
/// Taking a callback rather than an entity keeps this module free of any view
/// type, so the pump can be tested and reused without one.
pub type Handler<T> = fn(&mut T, Vec<Delivery>, &mut Window, &mut Context<T>);

/// Start delivering `events` to `view` until the returned task is dropped.
///
/// Blocks on a background thread and returns to the main thread with each batch,
/// so a quiet app costs nothing and a busy one still yields between batches.
pub fn spawn<T: 'static>(
    events: crossbeam_channel::Receiver<Delivery>,
    window: &Window,
    cx: &mut Context<T>,
    handle: Handler<T>,
) -> Task<()> {
    cx.spawn_in(window, async move |view, cx| {
        pump(events, view, cx, handle).await;
    })
}

/// Block, deliver, repeat.
async fn pump<T: 'static>(
    events: crossbeam_channel::Receiver<Delivery>,
    view: WeakEntity<T>,
    cx: &mut AsyncWindowContext,
    handle: Handler<T>,
) {
    loop {
        // The blocking wait happens here, on a thread that is allowed to block.
        // Awaiting the task is what keeps the main thread free meanwhile.
        let receiver = events.clone();
        let batch = cx
            .background_executor()
            .spawn(async move { collect(&receiver, BATCH) })
            .await;
        if batch.is_empty() {
            // Only when every session's channel has disconnected, so there will
            // never be another event to wait for.
            return;
        }
        // An error means the window closed; a missing entity means the view was
        // dropped. Either way there is nobody left to deliver to.
        let delivered = cx.update(|window, cx| {
            view.update(cx, |view, cx| handle(view, batch, window, cx))
                .is_ok()
        });
        if !matches!(delivered, Ok(true)) {
            return;
        }
    }
}

/// Wait for one delivery, then take up to `limit` more that are already queued.
///
/// Returns empty only on disconnect, which is the pump's signal to stop: an
/// empty batch from a still-connected channel would spin.
fn collect(events: &crossbeam_channel::Receiver<Delivery>, limit: usize) -> Vec<Delivery> {
    let Ok(first) = events.recv() else {
        return Vec::new();
    };
    let mut batch = Vec::with_capacity(limit);
    batch.push(first);
    // `try_iter` and not `iter`: everything already queued goes in this batch,
    // and anything later waits for the next blocking `recv` rather than holding
    // this batch open.
    batch.extend(events.try_iter().take(limit - 1));
    batch
}

/// Whether `handle` should treat this delivery as belonging to the visible
/// session.
///
/// Split out because the answer decides whether an event updates the
/// conversation or only the sidebar's busy dot, and that rule is worth asserting
/// without a window.
pub fn is_active(active_key: Option<&str>, key: &str) -> bool {
    active_key == Some(key)
}

/// Discard deliveries whose session has gone, resolving the rest to their
/// current key.
///
/// A token resolves to nothing once its session has been removed, which is the
/// ordinary way a late event from an exited process arrives. Attributing it to
/// whichever session took the key next would put one process's output in
/// another's conversation.
pub fn resolve<'a>(
    batch: Vec<Delivery>,
    key_for: impl Fn(SessionToken) -> Option<&'a str>,
) -> Vec<(String, RuntimeEvent)> {
    batch
        .into_iter()
        .filter_map(|(token, event)| key_for(token).map(|key| (key.to_owned(), event)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stderr(message: &str) -> RuntimeEvent {
        RuntimeEvent::Stderr(message.to_owned())
    }

    #[test]
    fn a_batch_takes_everything_already_queued() {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let token = SessionToken::next();
        for message in ["one", "two", "three"] {
            sender.send((token, stderr(message))).unwrap();
        }

        let batch = collect(&receiver, BATCH);

        assert_eq!(batch.len(), 3);
    }

    #[test]
    fn a_batch_stops_at_the_limit_and_leaves_the_rest() {
        // The bound is what keeps a long backlog from holding the main thread:
        // the remainder has to still be there for the next batch.
        let (sender, receiver) = crossbeam_channel::unbounded();
        let token = SessionToken::next();
        for _ in 0..5 {
            sender.send((token, stderr("event"))).unwrap();
        }

        let batch = collect(&receiver, 2);

        assert_eq!(batch.len(), 2);
        assert_eq!(receiver.len(), 3);
    }

    #[test]
    fn a_disconnected_channel_ends_the_pump() {
        // Empty means disconnected and nothing else — an empty batch from a live
        // channel would send the loop spinning.
        let (sender, receiver) = crossbeam_channel::unbounded::<Delivery>();
        drop(sender);

        assert!(collect(&receiver, BATCH).is_empty());
    }

    #[test]
    fn events_are_labelled_with_the_key_their_session_has_now() {
        let token = SessionToken::next();
        let batch = vec![(token, stderr("output"))];

        // Resolved at delivery, so a session re-keyed since the event was sent
        // still gets its own output.
        let resolved = resolve(batch, |_| Some("/tmp/renamed.jsonl"));

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].0, "/tmp/renamed.jsonl");
    }

    #[test]
    fn events_from_a_session_that_has_gone_are_dropped() {
        let live = SessionToken::next();
        let gone = SessionToken::next();
        let batch = vec![(gone, stderr("late")), (live, stderr("current"))];

        let resolved = resolve(batch, |token| (token == live).then_some("/tmp/s.jsonl"));

        let messages: Vec<_> = resolved
            .into_iter()
            .map(|(_, event)| match event {
                RuntimeEvent::Stderr(message) => message,
                other => panic!("unexpected event: {other:?}"),
            })
            .collect();
        assert_eq!(messages, vec!["current".to_owned()]);
    }

    #[test]
    fn only_the_visible_sessions_events_reach_the_conversation() {
        assert!(is_active(Some("/tmp/a.jsonl"), "/tmp/a.jsonl"));
        assert!(!is_active(Some("/tmp/a.jsonl"), "/tmp/b.jsonl"));
        // Nothing is visible before the first session is shown, and a background
        // session's events must not be taken for it.
        assert!(!is_active(None, "/tmp/a.jsonl"));
    }
}
