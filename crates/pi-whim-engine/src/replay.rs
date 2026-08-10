//! Typed, topic-scoped replay projections of [`AppState`].
//!
//! A [`StateSelector`] describes the reducer topics that can affect a pure
//! projection. [`ReplaySelection`] stores that projection in a
//! [`pi_whim_signal::StateSignal`], so consumers can receive the current value
//! immediately and only recompute it for relevant committed changes.

use std::fmt;
use std::sync::Arc;

use pi_whim_core::AppState;
use pi_whim_signal::StateSignal;

use crate::changes::{ChangeSet, StateTopic};

/// An error returned when a state selector has no topics to observe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateSelectorError {
    /// A selector must declare at least one state topic.
    EmptyTopics,
}

impl fmt::Display for StateSelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyTopics => {
                formatter.write_str("state selector must observe at least one topic")
            }
        }
    }
}

impl std::error::Error for StateSelectorError {}

/// A topic-scoped pure projection of [`AppState`].
///
/// Topics are deduplicated in first-seen order. The projection is type-erased
/// behind a thread-shareable function so a selector can be cloned and reused by
/// independent engine or UI consumers without exposing reducer actions.
pub struct StateSelector<T> {
    topics: Arc<[StateTopic]>,
    projection: Arc<dyn Fn(&AppState) -> T + Send + Sync + 'static>,
}

impl<T> Clone for StateSelector<T> {
    fn clone(&self) -> Self {
        Self {
            topics: self.topics.clone(),
            projection: self.projection.clone(),
        }
    }
}

impl<T> StateSelector<T> {
    /// Creates a selector with a non-empty, first-seen-ordered topic set.
    pub fn new<I, F>(topics: I, projection: F) -> Result<Self, StateSelectorError>
    where
        I: IntoIterator<Item = StateTopic>,
        F: Fn(&AppState) -> T + Send + Sync + 'static,
    {
        let mut unique_topics = Vec::new();
        for topic in topics {
            if !unique_topics.contains(&topic) {
                unique_topics.push(topic);
            }
        }

        if unique_topics.is_empty() {
            return Err(StateSelectorError::EmptyTopics);
        }

        Ok(Self {
            topics: Arc::from(unique_topics.into_boxed_slice()),
            projection: Arc::new(projection),
        })
    }

    /// Returns the selector's topics in declaration order.
    pub fn topics(&self) -> &[StateTopic] {
        &self.topics
    }

    /// Returns whether a non-noop change set touches one of the selector's topics.
    pub fn matches(&self, change_set: &ChangeSet) -> bool {
        !change_set.is_noop()
            && change_set
                .changed_topics
                .iter()
                .any(|topic| self.topics.contains(topic))
    }

    fn project(&self, state: &AppState) -> T {
        (self.projection)(state)
    }
}

/// A replayable, topic-scoped projection of [`AppState`].
///
/// Cloning the selection or the value returned by [`Self::signal`] preserves
/// the same underlying state signal. New subscribers therefore receive the
/// latest projection immediately through [`StateSignal::subscribe`].
#[derive(Clone)]
pub struct ReplaySelection<T> {
    selector: StateSelector<T>,
    signal: StateSignal<T>,
}

impl<T> ReplaySelection<T>
where
    T: Clone + PartialEq + Send + Sync + 'static,
{
    /// Creates a replay selection initialized from the complete application state.
    pub fn new(selector: StateSelector<T>, state: &AppState) -> Self {
        let initial = selector.project(state);
        Self {
            selector,
            signal: StateSignal::new(initial),
        }
    }

    /// Returns a cloneable replay-capable state signal.
    pub fn signal(&self) -> StateSignal<T> {
        self.signal.clone()
    }

    /// Returns the latest projected value.
    pub fn current(&self) -> T {
        self.signal.get()
    }

    /// Returns the topics observed by this selection in declaration order.
    pub fn topics(&self) -> &[StateTopic] {
        self.selector.topics()
    }

    /// Returns whether a non-noop change set touches this selection.
    pub fn matches(&self, change_set: &ChangeSet) -> bool {
        self.selector.matches(change_set)
    }

    /// Recomputes and publishes the projection only for a relevant changed state.
    ///
    /// Returns `true` when the projection changed and the state signal accepted
    /// the update. No-op, unrelated, and equal-value changes return `false`.
    pub fn publish(&self, change_set: &ChangeSet, state: &AppState) -> bool {
        if !self.matches(change_set) {
            return false;
        }

        let next = self.selector.project(state);
        if self.current() == next {
            return false;
        }

        self.signal.set(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::{CommitScope, CommitSource, TransactionRevision};
    use pi_whim_core::{ConversationItem, ConversationRole};
    use std::sync::mpsc::{self, Receiver, TryRecvError};

    fn change_set<I>(topics: I, action_count: usize) -> ChangeSet
    where
        I: IntoIterator<Item = StateTopic>,
    {
        ChangeSet {
            revision: TransactionRevision::new(1),
            scope: CommitScope::Global,
            source: CommitSource::Test,
            changed_topics: topics.into_iter().collect(),
            action_count,
            coalesced: false,
        }
    }

    fn conversation_item(id: &str) -> ConversationItem {
        ConversationItem {
            id: id.into(),
            role: ConversationRole::Assistant,
            full_text: String::new(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        }
    }

    fn subscribe<T>(selection: &ReplaySelection<T>) -> (Receiver<T>, pi_whim_signal::Subscription)
    where
        T: Clone + PartialEq + Send + Sync + 'static,
    {
        let (sender, receiver) = mpsc::channel();
        let subscription = selection.signal().subscribe_fn(move |value| {
            let _ = sender.send(value);
        });
        (receiver, subscription)
    }

    fn assert_no_value<T>(receiver: &Receiver<T>) {
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn initial_replay_is_available_to_the_first_subscriber() -> Result<(), StateSelectorError> {
        let mut state = AppState::default();
        state.conversation.push(conversation_item("initial"));
        let selector = StateSelector::new([StateTopic::Conversation], |state: &AppState| {
            state.conversation.len()
        })?;
        let selection = ReplaySelection::new(selector, &state);
        let (receiver, _subscription) = subscribe(&selection);

        assert_eq!(receiver.recv().ok(), Some(1));
        assert_eq!(selection.current(), 1);
        Ok(())
    }

    #[test]
    fn unrelated_and_noop_changes_do_not_recompute_or_emit() -> Result<(), StateSelectorError> {
        let projection_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_for_projection = projection_count.clone();
        let selector = StateSelector::new([StateTopic::Conversation], move |state: &AppState| {
            count_for_projection.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            state.conversation.len()
        })?;
        let state = AppState::default();
        let selection = ReplaySelection::new(selector, &state);
        let (receiver, _subscription) = subscribe(&selection);
        assert_eq!(receiver.recv().ok(), Some(0));
        assert_eq!(
            projection_count.load(std::sync::atomic::Ordering::Relaxed),
            1
        );

        assert!(!selection.publish(&change_set([StateTopic::Queue], 1), &state));
        assert!(!selection.publish(&change_set([StateTopic::Conversation], 0), &state));
        assert_eq!(
            projection_count.load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        assert_no_value(&receiver);
        Ok(())
    }

    #[test]
    fn related_equal_projection_does_not_emit() -> Result<(), StateSelectorError> {
        let state = AppState::default();
        let selector = StateSelector::new([StateTopic::Conversation], |state: &AppState| {
            state.conversation.len()
        })?;
        let selection = ReplaySelection::new(selector, &state);
        let (receiver, _subscription) = subscribe(&selection);
        assert_eq!(receiver.recv().ok(), Some(0));

        assert!(!selection.publish(&change_set([StateTopic::Conversation], 1), &state));
        assert_no_value(&receiver);
        Ok(())
    }

    #[test]
    fn related_changed_projection_emits() -> Result<(), StateSelectorError> {
        let state = AppState::default();
        let selector = StateSelector::new([StateTopic::Conversation], |state: &AppState| {
            state.conversation.len()
        })?;
        let selection = ReplaySelection::new(selector, &state);
        let (receiver, _subscription) = subscribe(&selection);
        assert_eq!(receiver.recv().ok(), Some(0));

        let mut changed_state = state.clone();
        changed_state
            .conversation
            .push(conversation_item("initial"));
        assert!(selection.publish(&change_set([StateTopic::Conversation], 1), &changed_state));
        assert_eq!(receiver.recv().ok(), Some(1));
        assert_eq!(selection.current(), 1);
        Ok(())
    }

    #[test]
    fn one_selector_can_match_and_project_multiple_topics() -> Result<(), StateSelectorError> {
        let selector = StateSelector::new(
            [StateTopic::Conversation, StateTopic::Queue],
            |state: &AppState| {
                (
                    state.conversation.len(),
                    state.pending_steering.len() + state.pending_follow_up.len(),
                )
            },
        )?;
        let state = AppState::default();
        let selection = ReplaySelection::new(selector, &state);
        assert_eq!(
            selection.topics(),
            &[StateTopic::Conversation, StateTopic::Queue]
        );
        assert!(selection.matches(&change_set([StateTopic::Queue], 1)));

        let mut changed_state = state.clone();
        changed_state.pending_steering.push(String::from("steer"));
        assert!(selection.publish(&change_set([StateTopic::Queue], 1), &changed_state));
        assert_eq!(selection.current(), (0, 1));
        Ok(())
    }

    #[test]
    fn topics_are_deduplicated_in_first_seen_order() -> Result<(), StateSelectorError> {
        let selector = StateSelector::new(
            [
                StateTopic::Queue,
                StateTopic::Conversation,
                StateTopic::Queue,
                StateTopic::Selection,
                StateTopic::Conversation,
            ],
            |_| (),
        )?;

        assert_eq!(
            selector.topics(),
            &[
                StateTopic::Queue,
                StateTopic::Conversation,
                StateTopic::Selection,
            ]
        );
        Ok(())
    }

    #[test]
    fn empty_topics_return_a_structured_displayable_error() {
        let result = StateSelector::<usize>::new([], |_| 0);
        let Err(error) = result else {
            return;
        };
        assert_eq!(error, StateSelectorError::EmptyTopics);
        assert_eq!(
            error.to_string(),
            "state selector must observe at least one topic"
        );
        fn assert_error<E: std::error::Error>() {}
        assert_error::<StateSelectorError>();
    }

    #[test]
    fn a_cloned_signal_replays_the_latest_value_to_later_subscribers()
    -> Result<(), StateSelectorError> {
        let state = AppState::default();
        let selector = StateSelector::new([StateTopic::Conversation], |state: &AppState| {
            state.conversation.len()
        })?;
        let selection = ReplaySelection::new(selector, &state);
        let cloned_signal = selection.signal();

        let mut changed_state = state.clone();
        changed_state
            .conversation
            .push(conversation_item("initial"));
        assert!(selection.publish(&change_set([StateTopic::Conversation], 1), &changed_state));

        let (sender, receiver) = mpsc::channel();
        let _subscription = cloned_signal.subscribe_fn(move |value| {
            let _ = sender.send(value);
        });
        assert_eq!(receiver.recv().ok(), Some(1));
        Ok(())
    }
}
