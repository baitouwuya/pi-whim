//! Typed application-level signals emitted after reducer commits and command stages.

use std::{collections::VecDeque, fmt, sync::Mutex};

use pi_whim_core::Attachment;
use pi_whim_engine::{
    changes::{ChangeSet, CommitError},
    commands::CommandLifecycle,
    dialogs::Prompt,
    notice::Notice,
};
use pi_whim_signal::{Signal, SignalEmitter};

use super::Picker;

/// A typed request for framework-owned UI work.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum ApplicationEffect {
    Notice(Notice),
    Prompt(Prompt),
    SessionClosed(String),
    AttachmentReady(Attachment),
    ClipboardWrite(String),
    OpenPicker(Picker),
}

impl fmt::Debug for ApplicationEffect {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Notice(notice) => formatter
                .debug_struct("Notice")
                .field("level", &notice.level)
                .finish(),
            Self::Prompt(_) => formatter.write_str("Prompt"),
            Self::SessionClosed(_) => formatter.write_str("SessionClosed"),
            Self::AttachmentReady(_) => formatter.write_str("AttachmentReady"),
            Self::ClipboardWrite(text) => formatter
                .debug_struct("ClipboardWrite")
                .field("bytes", &text.len())
                .finish(),
            Self::OpenPicker(picker) => formatter.debug_tuple("OpenPicker").field(picker).finish(),
        }
    }
}

const STARTUP_NOTICE_CAPACITY: usize = 64;

#[derive(Default)]
struct EffectDelivery {
    active: bool,
    draining: bool,
    pending: VecDeque<ApplicationEffect>,
    consecutive_notice: Option<Notice>,
}

impl EffectDelivery {
    fn accepts(&mut self, effect: &ApplicationEffect) -> bool {
        match effect {
            ApplicationEffect::Notice(notice)
                if self.consecutive_notice.as_ref() == Some(notice) =>
            {
                false
            }
            ApplicationEffect::Notice(notice) => {
                self.consecutive_notice = Some(notice.clone());
                true
            }
            _ => {
                self.consecutive_notice = None;
                true
            }
        }
    }

    /// Buffer startup effects while bounding only disposable notices.
    ///
    /// Reliable effects never count toward the notice cap and are never removed.
    /// Removing the oldest notice in place preserves the cross-variant order of
    /// every surviving effect in the single pending queue.
    fn buffer_startup(&mut self, effect: ApplicationEffect) {
        if matches!(effect, ApplicationEffect::Notice(_)) {
            let mut oldest_notice = None;
            let mut notice_count = 0;
            for (index, pending) in self.pending.iter().enumerate() {
                if matches!(pending, ApplicationEffect::Notice(_)) {
                    oldest_notice.get_or_insert(index);
                    notice_count += 1;
                }
            }
            if notice_count >= STARTUP_NOTICE_CAPACITY
                && let Some(index) = oldest_notice
            {
                self.pending.remove(index);
            }
        }
        self.pending.push_back(effect);
    }
}

/// Owns the application signal channels while exposing readers separately.
///
/// Emitters stay private to the orchestration. Consumers receive cloneable
/// [`Signal`] handles and therefore cannot forge reducer commits or command
/// lifecycle stages.
pub(super) struct ApplicationSignals {
    change_sets: Signal<ChangeSet>,
    change_set_emitter: SignalEmitter<ChangeSet>,
    command_lifecycle: Signal<CommandLifecycle>,
    command_lifecycle_emitter: SignalEmitter<CommandLifecycle>,
    application_effects: Signal<ApplicationEffect>,
    application_effect_emitter: SignalEmitter<ApplicationEffect>,
    effect_delivery: Mutex<EffectDelivery>,
}

impl Default for ApplicationSignals {
    fn default() -> Self {
        let (change_sets, change_set_emitter) = Signal::channel();
        let (command_lifecycle, command_lifecycle_emitter) = Signal::channel();
        let (application_effects, application_effect_emitter) = Signal::channel();
        Self {
            change_sets,
            change_set_emitter,
            command_lifecycle,
            command_lifecycle_emitter,
            application_effects,
            application_effect_emitter,
            effect_delivery: Mutex::new(EffectDelivery::default()),
        }
    }
}

impl fmt::Debug for ApplicationSignals {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ApplicationSignals")
            .field("change_set_listeners", &self.change_sets.listener_count())
            .field(
                "command_lifecycle_listeners",
                &self.command_lifecycle.listener_count(),
            )
            .field(
                "application_effect_listeners",
                &self.application_effects.listener_count(),
            )
            .finish_non_exhaustive()
    }
}

impl ApplicationSignals {
    pub(super) fn change_sets(&self) -> Signal<ChangeSet> {
        self.change_sets.clone()
    }

    pub(super) fn command_lifecycle(&self) -> Signal<CommandLifecycle> {
        self.command_lifecycle.clone()
    }

    pub(super) fn application_effects(&self) -> Signal<ApplicationEffect> {
        self.application_effects.clone()
    }

    pub(super) fn emit_effect(&self, effect: ApplicationEffect) {
        {
            let mut delivery = self
                .effect_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !delivery.accepts(&effect) {
                return;
            }
            if !delivery.active {
                delivery.buffer_startup(effect);
                return;
            }
            delivery.pending.push_back(effect);
            if delivery.draining {
                return;
            }
            delivery.draining = true;
        }
        self.drain_effects();
    }

    /// Switch from bounded startup buffering to reliable direct delivery.
    pub(super) fn activate_effect_delivery(&self) -> usize {
        let count = {
            let mut delivery = self
                .effect_delivery
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if delivery.active {
                return 0;
            }
            delivery.active = true;
            let count = delivery.pending.len();
            if count != 0 {
                delivery.draining = true;
            }
            count
        };
        if count != 0 {
            self.drain_effects();
        }
        count
    }

    fn drain_effects(&self) {
        loop {
            let effect = {
                let mut delivery = self
                    .effect_delivery
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let effect = delivery.pending.pop_front();
                if effect.is_none() {
                    delivery.draining = false;
                }
                effect
            };
            let Some(effect) = effect else {
                break;
            };
            let _ = self.application_effect_emitter.emit(effect);
        }
    }

    /// Publish exactly one change set for a successful reducer commit.
    ///
    /// Failed commits are returned unchanged and never reach subscribers.
    pub(super) fn publish_commit(
        &self,
        result: Result<ChangeSet, CommitError>,
    ) -> Result<ChangeSet, CommitError> {
        let change_set = result?;
        let _ = self.change_set_emitter.emit(change_set.clone());
        Ok(change_set)
    }

    pub(super) fn emit_command_lifecycle(&self, lifecycle: CommandLifecycle) {
        let _ = self.command_lifecycle_emitter.emit(lifecycle);
    }
}

#[cfg(test)]
mod tests {
    use pi_whim_core::SubmitMode;
    use pi_whim_engine::{
        changes::{CommitError, TransactionRevision},
        commands::{AppCommand, CommandEnvelope, CommandLifecycle},
    };

    use super::*;

    #[test]
    fn failed_commit_does_not_emit_a_change_set() {
        let signals = ApplicationSignals::default();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = signals.change_sets().subscribe_fn(move |change_set| {
            let _ = sender.send(change_set);
        });

        let result = signals.publish_commit(Err(CommitError::RevisionOverflow {
            current: TransactionRevision::MAX,
        }));

        assert!(matches!(
            result,
            Err(CommitError::RevisionOverflow {
                current: TransactionRevision::MAX
            })
        ));
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn lifecycle_signal_has_no_replay_and_debug_is_metadata_only() {
        let signals = ApplicationSignals::default();
        let envelope = CommandEnvelope::ui(AppCommand::SubmitPrompt {
            content: "prompt-secret-469c".into(),
            attachments: Vec::new(),
            mode: SubmitMode::Prompt,
        });
        let lifecycle = CommandLifecycle::submitted(&envelope);
        signals.emit_command_lifecycle(lifecycle.clone());

        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = signals.command_lifecycle().subscribe_fn(move |event| {
            let _ = sender.send(event);
        });
        assert!(receiver.try_recv().is_err());

        signals.emit_command_lifecycle(lifecycle);
        let received = receiver
            .try_recv()
            .expect("the active lifecycle subscriber receives the new event");
        assert_eq!(received.command_name(), "prompt.submit");
        assert!(!format!("{received:?}").contains("prompt-secret-469c"));
        assert!(!format!("{signals:?}").contains("prompt-secret-469c"));
    }

    #[test]
    fn startup_effects_flush_once_in_cross_variant_order() {
        let signals = ApplicationSignals::default();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = signals.application_effects().subscribe_fn(move |effect| {
            let _ = sender.send(effect);
        });

        signals.emit_effect(ApplicationEffect::Notice(Notice::info("ready")));
        signals.emit_effect(ApplicationEffect::SessionClosed("private-session".into()));
        signals.emit_effect(ApplicationEffect::OpenPicker(Picker::Project));
        assert!(receiver.try_recv().is_err());

        assert_eq!(signals.activate_effect_delivery(), 3);
        assert_eq!(signals.activate_effect_delivery(), 0);
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            vec![
                ApplicationEffect::Notice(Notice::info("ready")),
                ApplicationEffect::SessionClosed("private-session".into()),
                ApplicationEffect::OpenPicker(Picker::Project),
            ]
        );
    }

    #[test]
    fn startup_buffer_never_drops_reliable_non_notice_effects() {
        let signals = ApplicationSignals::default();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = signals.application_effects().subscribe_fn(move |effect| {
            let _ = sender.send(effect);
        });
        let effect_count = STARTUP_NOTICE_CAPACITY + 17;

        for index in 0..effect_count {
            signals.emit_effect(ApplicationEffect::SessionClosed(format!("session-{index}")));
        }

        assert_eq!(signals.activate_effect_delivery(), effect_count);
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            (0..effect_count)
                .map(|index| ApplicationEffect::SessionClosed(format!("session-{index}")))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn startup_notice_overflow_removes_only_the_oldest_notice() {
        let signals = ApplicationSignals::default();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = signals.application_effects().subscribe_fn(move |effect| {
            let _ = sender.send(effect);
        });

        signals.emit_effect(ApplicationEffect::Notice(Notice::info("notice-0")));
        signals.emit_effect(ApplicationEffect::SessionClosed("reliable-session".into()));
        for index in 1..=STARTUP_NOTICE_CAPACITY {
            signals.emit_effect(ApplicationEffect::Notice(Notice::info(format!(
                "notice-{index}"
            ))));
        }

        assert_eq!(
            signals.activate_effect_delivery(),
            STARTUP_NOTICE_CAPACITY + 1
        );
        let mut expected = vec![ApplicationEffect::SessionClosed("reliable-session".into())];
        expected.extend(
            (1..=STARTUP_NOTICE_CAPACITY)
                .map(|index| ApplicationEffect::Notice(Notice::info(format!("notice-{index}")))),
        );
        assert_eq!(receiver.try_iter().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn active_effects_deliver_immediately_and_dedupe_only_consecutive_notices() {
        let signals = ApplicationSignals::default();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let _subscription = signals.application_effects().subscribe_fn(move |effect| {
            let _ = sender.send(effect);
        });
        signals.activate_effect_delivery();

        signals.emit_effect(ApplicationEffect::Notice(Notice::error("same")));
        signals.emit_effect(ApplicationEffect::Notice(Notice::error("same")));
        signals.emit_effect(ApplicationEffect::OpenPicker(Picker::Attachments));
        signals.emit_effect(ApplicationEffect::Notice(Notice::error("same")));

        assert_eq!(receiver.try_iter().count(), 3);
    }

    #[test]
    fn sensitive_effect_debug_is_metadata_only() {
        let prompt = Prompt::from_interaction(
            "session-secret-734",
            &serde_json::json!({
                "request_id": "request-secret-734",
                "kind": "question",
                "title": "title-secret-734",
                "message": "message-secret-734",
            }),
        )
        .expect("the test prompt is valid");
        let effects = [
            ApplicationEffect::Prompt(prompt),
            ApplicationEffect::AttachmentReady(Attachment {
                name: "attachment-secret-734".into(),
                path: "/private/attachment-secret-734".into(),
                kind: pi_whim_core::AttachmentKind::File,
                generated_by_app: false,
            }),
            ApplicationEffect::ClipboardWrite("clipboard-secret-734".into()),
            ApplicationEffect::SessionClosed("session-secret-734".into()),
        ];

        let debug = format!("{effects:?}");
        for secret in [
            "title-secret-734",
            "message-secret-734",
            "attachment-secret-734",
            "clipboard-secret-734",
            "session-secret-734",
        ] {
            assert!(!debug.contains(secret));
        }
    }
}
