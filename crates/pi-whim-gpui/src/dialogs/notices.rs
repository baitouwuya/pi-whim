//! Reporting messages the engine wants the user to see.
//!
//! `engine::notice::Outbox` queues them; this pushes them onto the window's
//! notification stack, which slides them in at the corner and auto-hides the
//! ones that are not failures.
//!
//! The egui build put each of these in a modal window that had to be dismissed
//! before anything else could happen — including a "share URL" that was only
//! ever informational. Errors still stay until they are read, since they usually
//! need the reader to do something; anything else goes on its own.

use gpui::{App, Window};
use gpui_component::{
    WindowExt,
    notification::{Notification, NotificationType},
};
use pi_whim_engine::notice::{Level, Notice, Outbox};

/// How a level reads in the corner.
fn notification_type(level: Level) -> NotificationType {
    match level {
        Level::Error => NotificationType::Error,
        Level::Info => NotificationType::Info,
    }
}

/// Whether a message should disappear on its own.
///
/// A failure usually needs the reader to act, and one that vanished while they
/// were looking elsewhere is a failure they never saw.
fn autohide(level: Level) -> bool {
    level != Level::Error
}

/// Turn one message into a notification.
pub fn notification(notice: &Notice) -> Notification {
    Notification::new()
        .message(notice.message.clone())
        .with_type(notification_type(notice.level))
        .autohide(autohide(notice.level))
}

/// Show everything queued, oldest first.
///
/// Drains rather than taking one: the notification stack shows several at once,
/// so there is no reason to hold the rest back.
pub fn show(outbox: &mut Outbox, window: &mut Window, cx: &mut App) {
    for notice in outbox.drain() {
        window.push_notification(notification(&notice), cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failure_waits_to_be_read() {
        // The egui build made every message a modal; the reason to keep that
        // behavior for errors is that they need the reader to do something.
        assert!(!autohide(Level::Error));
        assert!(autohide(Level::Info));
    }

    #[test]
    fn levels_map_onto_distinct_notification_types() {
        // The colour and icon are what distinguish "your provider has no key"
        // from "here is your share URL" at a glance.
        assert!(matches!(
            notification_type(Level::Error),
            NotificationType::Error
        ));
        assert!(matches!(
            notification_type(Level::Info),
            NotificationType::Info
        ));
    }

    #[test]
    fn everything_queued_is_shown() {
        // The stack holds several, so nothing waits its turn.
        let mut outbox = Outbox::new();
        outbox.error("project is missing");
        outbox.info("share URL ready");

        // `show` needs a window; this covers the draining half of it.
        assert_eq!(outbox.drain().len(), 2);
        assert!(outbox.is_empty());
    }
}
