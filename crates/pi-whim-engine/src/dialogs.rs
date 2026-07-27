//! Questions the agent asks the user, and the answers that go back.
//!
//! Two sources ask: Pi's extension protocol sends a `confirm` request when a
//! tool wants permission, and the agent-team supervisor sends an interaction
//! when a sub-agent needs an approval or a decision. Both arrive as untyped
//! wire JSON and both need the same thing on screen — a title, a message, and a
//! row of choices — so they are parsed into one shape here.
//!
//! The egui app held these in `Option<(String, Value)>` and `Vec<(String,
//! Value)>` on the application struct, reaching into the JSON again at render
//! time and, in the extension case, dropping an unanswered request when a second
//! one arrived. They queue here instead, and the view only sees [`Prompt`].

use std::collections::VecDeque;

use serde_json::Value;

/// How much a choice stands out.
///
/// The view maps these onto its own button variants; the engine only says which
/// choice is the expected one and which destroys something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// The affirmative answer.
    Primary,
    /// A refusal.
    Danger,
    /// Anything else the request offered.
    Neutral,
}

/// Text on its way to the screen.
///
/// The engine writes some of this itself — the word on an approval button, the
/// heading when a request sends none — and passes the rest through from the
/// agent. Only the first kind can be translated, and only the view knows which
/// language to translate into, so the two are distinguished here rather than
/// flattened into one `String` the view would have to guess about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Label {
    /// The app's own words, as a key into the string table.
    Key(&'static str),
    /// The agent's own text, shown as it was sent.
    Verbatim(String),
}

impl Label {
    fn verbatim(text: &str) -> Self {
        Self::Verbatim(text.to_owned())
    }

    /// The agent's text if it sent any, otherwise the app's own wording.
    fn or_key(text: Option<&str>, key: &'static str) -> Self {
        match text {
            Some(text) if !text.is_empty() => Self::verbatim(text),
            _ => Self::Key(key),
        }
    }

    /// Whether there is anything to show.
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Key(_) => false,
            Self::Verbatim(text) => text.is_empty(),
        }
    }
}

/// One answer the reader can give.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    /// What travels back to the agent.
    pub value: String,
    /// What the button says.
    pub label: Label,
    pub tone: Tone,
}

impl Choice {
    fn new(value: &str, label: Label, tone: Tone) -> Self {
        Self {
            value: value.to_owned(),
            label,
            tone,
        }
    }
}

/// Which protocol asked, and so how the answer travels back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// Pi's extension confirmation, answered over the extension RPC as a bool.
    Extension,
    /// A supervisor interaction, answered with the option that was picked.
    Interaction,
}

/// The value an extension confirmation treats as consent.
const ALLOW: &str = "allow";

/// A question waiting for the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prompt {
    /// Which session asked. Background agents can prompt too, so this is how the
    /// answer finds its way home.
    pub session_key: String,
    pub source: Source,
    pub request_id: String,
    pub title: Label,
    pub message: Label,
    pub choices: Vec<Choice>,
    /// The value used when the dialog is dismissed rather than answered.
    ///
    /// Closing the window is itself an answer: the agent is blocked waiting, so
    /// there has to be one, and it should be the cautious one.
    pub dismissal: String,
}

impl Prompt {
    /// Read an extension confirmation.
    ///
    /// Returns `None` for any other extension method, since those are not
    /// questions and nothing on screen would make sense.
    pub fn from_extension(session_key: &str, request: &Value) -> Option<Self> {
        if request.get("method").and_then(Value::as_str) != Some("confirm") {
            return None;
        }
        Some(Self {
            session_key: session_key.to_owned(),
            source: Source::Extension,
            request_id: request
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            title: Label::or_key(
                request.get("title").and_then(Value::as_str),
                "confirm-title",
            ),
            message: Label::or_key(
                request.get("message").and_then(Value::as_str),
                "confirm-message",
            ),
            choices: vec![
                Choice::new(ALLOW, Label::Key("allow"), Tone::Primary),
                Choice::new("deny", Label::Key("deny"), Tone::Danger),
            ],
            // Dismissing a permission request denies it. Anything else would
            // grant access the reader never agreed to.
            dismissal: "deny".to_owned(),
        })
    }

    /// Read a supervisor interaction.
    pub fn from_interaction(session_key: &str, request: &Value) -> Option<Self> {
        let kind = request
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("question");
        let options: Vec<String> = request
            .get("options")
            .and_then(Value::as_array)
            .map(|options| {
                options
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();

        // The request names its own fallback, but only a value it actually
        // offered is usable: a default that is not on any button would send back
        // a decision the agent never listed.
        let fallback = if kind == "approval" { "deny" } else { "cancel" };
        let dismissal = request
            .get("default_option")
            .and_then(Value::as_str)
            .filter(|option| options.iter().any(|candidate| candidate == option))
            .unwrap_or(fallback)
            .to_owned();

        Some(Self {
            session_key: session_key.to_owned(),
            source: Source::Interaction,
            request_id: request
                .get("request_id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            title: Label::or_key(
                request.get("title").and_then(Value::as_str),
                "agent-request",
            ),
            message: Label::Verbatim(
                request
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            choices: options
                .iter()
                .map(|option| choice_for(kind, option))
                .collect(),
            dismissal,
        })
    }

    /// What to send back for `value`.
    pub fn answer(&self, value: &str) -> Answer {
        match self.source {
            Source::Extension => Answer::Extension {
                session_key: self.session_key.clone(),
                request_id: self.request_id.clone(),
                confirmed: value == ALLOW,
            },
            Source::Interaction => Answer::Interaction {
                session_key: self.session_key.clone(),
                request_id: self.request_id.clone(),
                decision: value.to_owned(),
            },
        }
    }

    /// What to send back when the dialog is closed unanswered.
    pub fn dismissed(&self) -> Answer {
        self.answer(&self.dismissal)
    }
}

/// How an interaction option should read and look.
///
/// An approval's raw options are protocol words; the rest are already written
/// for a person and pass through untouched.
fn choice_for(kind: &str, option: &str) -> Choice {
    match (kind, option) {
        ("approval", "approve") => Choice::new(option, Label::Key("allow-once"), Tone::Primary),
        ("approval", "deny") => Choice::new(option, Label::Key("deny"), Tone::Danger),
        _ => Choice::new(option, Label::verbatim(option), Tone::Neutral),
    }
}

/// A decision on its way back to the agent.
///
/// The two variants carry the same identity but travel over different calls, so
/// the host matches rather than guessing from the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answer {
    Extension {
        session_key: String,
        request_id: String,
        confirmed: bool,
    },
    Interaction {
        session_key: String,
        request_id: String,
        decision: String,
    },
}

impl Answer {
    /// Which session the answer belongs to.
    pub fn session_key(&self) -> &str {
        match self {
            Self::Extension { session_key, .. } | Self::Interaction { session_key, .. } => {
                session_key
            }
        }
    }
}

/// Questions waiting to be asked, oldest first.
///
/// One at a time reaches the screen: each asking agent is blocked on its answer,
/// so stacking dialogs would only make the reader choose which blocked agent to
/// unblock first.
#[derive(Debug, Default)]
pub struct Queue {
    pending: VecDeque<Prompt>,
}

impl Queue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queue a prompt, ignoring one already asked.
    ///
    /// A retried request carries the request id it had before, and asking twice
    /// would leave a second dialog behind after the first is answered.
    pub fn push(&mut self, prompt: Prompt) {
        if self.pending.iter().any(|pending| {
            pending.session_key == prompt.session_key && pending.request_id == prompt.request_id
        }) {
            return;
        }
        self.pending.push_back(prompt);
    }

    /// Queue an extension confirmation, if that is what the request is.
    pub fn push_extension(&mut self, session_key: &str, request: &Value) {
        if let Some(prompt) = Prompt::from_extension(session_key, request) {
            self.push(prompt);
        }
    }

    /// Queue a supervisor interaction.
    pub fn push_interaction(&mut self, session_key: &str, request: &Value) {
        if let Some(prompt) = Prompt::from_interaction(session_key, request) {
            self.push(prompt);
        }
    }

    /// The prompt on screen, if any.
    pub fn current(&self) -> Option<&Prompt> {
        self.pending.front()
    }

    /// Answer the current prompt with `value` and move on.
    pub fn answer(&mut self, value: &str) -> Option<Answer> {
        let prompt = self.pending.pop_front()?;
        Some(prompt.answer(value))
    }

    /// Close the current prompt without picking, and move on.
    pub fn dismiss(&mut self) -> Option<Answer> {
        let prompt = self.pending.pop_front()?;
        Some(prompt.dismissed())
    }

    /// Drop everything a session asked.
    ///
    /// Called when the session goes away: there is nothing left to answer, and
    /// leaving the dialog up would ask the reader to unblock a dead process.
    pub fn forget_session(&mut self, session_key: &str) {
        self.pending
            .retain(|prompt| prompt.session_key != session_key);
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
    use serde_json::json;

    fn confirm() -> Value {
        json!({
            "id": "req-1",
            "method": "confirm",
            "title": "Run a command?",
            "message": "Pi wants to run `rm -rf build`.",
        })
    }

    fn approval() -> Value {
        json!({
            "request_id": "int-1",
            "kind": "approval",
            "title": "Sub-agent wants to write",
            "message": "Write to src/main.rs?",
            "options": ["approve", "deny"],
        })
    }

    #[test]
    fn a_confirmation_offers_allow_and_deny() {
        let prompt = Prompt::from_extension("s1", &confirm()).expect("a prompt");

        assert_eq!(prompt.source, Source::Extension);
        assert_eq!(prompt.request_id, "req-1");
        assert_eq!(prompt.title, Label::verbatim("Run a command?"));
        // The two buttons are the app's own words, so they travel as keys and
        // the view reads them in whichever language is set.
        assert_eq!(
            prompt.choices,
            vec![
                Choice::new("allow", Label::Key("allow"), Tone::Primary),
                Choice::new("deny", Label::Key("deny"), Tone::Danger),
            ]
        );
    }

    #[test]
    fn a_confirmation_without_text_still_reads_as_a_question() {
        // Pi does not have to send a title or message, and a blank dialog would
        // be unanswerable.
        let prompt =
            Prompt::from_extension("s1", &json!({"id": "req-1", "method": "confirm"})).unwrap();

        assert!(!prompt.title.is_empty());
        assert!(!prompt.message.is_empty());
        // And what stands in for them is the app's own wording, not English
        // baked into the engine.
        assert_eq!(prompt.title, Label::Key("confirm-title"));
        assert_eq!(prompt.message, Label::Key("confirm-message"));
    }

    #[test]
    fn other_extension_methods_are_not_questions() {
        // The extension channel carries more than confirmations; putting a
        // dialog up for those would block on an answer nothing wants.
        assert!(Prompt::from_extension("s1", &json!({"id": "x", "method": "log"})).is_none());
        assert!(Prompt::from_extension("s1", &json!({"id": "x"})).is_none());
    }

    #[test]
    fn dismissing_a_confirmation_denies_it() {
        // Closing the window must not grant access the reader never agreed to.
        let prompt = Prompt::from_extension("s1", &confirm()).unwrap();

        assert_eq!(
            prompt.dismissed(),
            Answer::Extension {
                session_key: "s1".into(),
                request_id: "req-1".into(),
                confirmed: false,
            }
        );
    }

    #[test]
    fn only_allow_counts_as_consent() {
        let prompt = Prompt::from_extension("s1", &confirm()).unwrap();

        let Answer::Extension { confirmed, .. } = prompt.answer("allow") else {
            panic!("an extension answer");
        };
        assert!(confirmed);

        let Answer::Extension { confirmed, .. } = prompt.answer("deny") else {
            panic!("an extension answer");
        };
        assert!(!confirmed);
    }

    #[test]
    fn approval_options_read_as_actions() {
        // "approve" is a protocol word; a button says what pressing it does.
        let prompt = Prompt::from_interaction("s1", &approval()).expect("a prompt");

        assert_eq!(
            prompt.choices,
            vec![
                Choice::new("approve", Label::Key("allow-once"), Tone::Primary),
                Choice::new("deny", Label::Key("deny"), Tone::Danger),
            ]
        );
    }

    #[test]
    fn question_options_are_already_written_for_a_person() {
        let prompt = Prompt::from_interaction(
            "s1",
            &json!({
                "request_id": "int-2",
                "kind": "question",
                "title": "Which branch?",
                "options": ["main", "develop"],
            }),
        )
        .unwrap();

        assert_eq!(
            prompt.choices,
            vec![
                Choice::new("main", Label::verbatim("main"), Tone::Neutral),
                Choice::new("develop", Label::verbatim("develop"), Tone::Neutral),
            ]
        );
    }

    #[test]
    fn a_dismissed_approval_denies_and_a_question_cancels() {
        let approval = Prompt::from_interaction("s1", &approval()).unwrap();
        assert_eq!(approval.dismissal, "deny");

        let question =
            Prompt::from_interaction("s1", &json!({"request_id": "q", "kind": "question"}))
                .unwrap();
        assert_eq!(question.dismissal, "cancel");
    }

    #[test]
    fn the_requests_own_default_wins_when_it_is_on_offer() {
        let prompt = Prompt::from_interaction(
            "s1",
            &json!({
                "request_id": "int-3",
                "kind": "question",
                "options": ["keep", "discard"],
                "default_option": "keep",
            }),
        )
        .unwrap();

        assert_eq!(prompt.dismissal, "keep");
    }

    #[test]
    fn a_default_that_is_not_on_offer_is_ignored() {
        // Sending back a decision the agent never listed is worse than the
        // cautious fallback.
        let prompt = Prompt::from_interaction(
            "s1",
            &json!({
                "request_id": "int-4",
                "kind": "approval",
                "options": ["approve", "deny"],
                "default_option": "maybe",
            }),
        )
        .unwrap();

        assert_eq!(prompt.dismissal, "deny");
    }

    #[test]
    fn an_interaction_answer_carries_the_option_verbatim() {
        let prompt = Prompt::from_interaction("s1", &approval()).unwrap();

        assert_eq!(
            prompt.answer("approve"),
            Answer::Interaction {
                session_key: "s1".into(),
                request_id: "int-1".into(),
                decision: "approve".into(),
            }
        );
    }

    #[test]
    fn questions_are_asked_in_the_order_they_arrived() {
        let mut queue = Queue::new();
        queue.push_extension("s1", &confirm());
        queue.push_interaction("s2", &approval());

        assert_eq!(
            queue.current().map(|prompt| prompt.source),
            Some(Source::Extension)
        );
        assert_eq!(
            queue.answer("allow"),
            Some(Answer::Extension {
                session_key: "s1".into(),
                request_id: "req-1".into(),
                confirmed: true,
            })
        );
        assert_eq!(
            queue.current().map(|prompt| prompt.source),
            Some(Source::Interaction)
        );
    }

    #[test]
    fn a_second_confirmation_does_not_replace_an_unanswered_one() {
        // The egui build held one Option here, so the first request was dropped
        // and its agent waited forever.
        let mut queue = Queue::new();
        queue.push_extension("s1", &confirm());
        queue.push_extension("s2", &json!({"id": "req-2", "method": "confirm"}));

        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_retried_request_is_asked_once() {
        // A resend carries the id it had before; two dialogs would leave one
        // behind after the first is answered.
        let mut queue = Queue::new();
        queue.push_extension("s1", &confirm());
        queue.push_extension("s1", &confirm());

        assert_eq!(queue.len(), 1);
    }

    #[test]
    fn the_same_id_from_a_different_session_is_a_different_question() {
        // Request ids are per-agent, so two sessions can pick the same one.
        let mut queue = Queue::new();
        queue.push_extension("s1", &confirm());
        queue.push_extension("s2", &confirm());

        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn a_non_question_is_never_queued() {
        let mut queue = Queue::new();
        queue.push_extension("s1", &json!({"id": "x", "method": "log"}));

        assert!(queue.is_empty());
    }

    #[test]
    fn a_dead_sessions_questions_are_dropped() {
        // Nothing is left to answer, and the dialog would ask the reader to
        // unblock a process that has gone.
        let mut queue = Queue::new();
        queue.push_extension("s1", &confirm());
        queue.push_interaction("s2", &approval());

        queue.forget_session("s1");

        assert_eq!(queue.len(), 1);
        assert_eq!(
            queue.current().map(|prompt| prompt.session_key.as_str()),
            Some("s2")
        );
    }

    #[test]
    fn answering_an_empty_queue_reports_nothing() {
        let mut queue = Queue::new();

        assert_eq!(queue.answer("allow"), None);
        assert_eq!(queue.dismiss(), None);
    }

    #[test]
    fn an_answer_knows_which_session_it_belongs_to() {
        let prompt = Prompt::from_interaction("s7", &approval()).unwrap();

        assert_eq!(prompt.answer("deny").session_key(), "s7");
    }
}
