//! Typed application-level signals emitted after reducer commits and command stages.

use std::fmt;

use pi_whim_engine::{
    changes::{ChangeSet, CommitError},
    commands::CommandLifecycle,
};
use pi_whim_signal::{Signal, SignalEmitter};

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
}

impl Default for ApplicationSignals {
    fn default() -> Self {
        let (change_sets, change_set_emitter) = Signal::channel();
        let (command_lifecycle, command_lifecycle_emitter) = Signal::channel();
        Self {
            change_sets,
            change_set_emitter,
            command_lifecycle,
            command_lifecycle_emitter,
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
            .finish()
    }
}

impl ApplicationSignals {
    pub(super) fn change_sets(&self) -> Signal<ChangeSet> {
        self.change_sets.clone()
    }

    pub(super) fn command_lifecycle(&self) -> Signal<CommandLifecycle> {
        self.command_lifecycle.clone()
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
}
