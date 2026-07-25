//! The chooser for questions the agent asks.
//!
//! One view for both asking protocols: `engine::dialogs` already flattens a Pi
//! extension confirmation and a supervisor interaction into the same [`Prompt`],
//! so what is left here is a modal with a row of buttons.
//!
//! The queue is drained one at a time. Each asking agent is blocked on its
//! answer, so stacking dialogs would only make the reader decide which blocked
//! agent to unblock first.

use gpui::{
    Context, EventEmitter, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    button::{Button, ButtonVariant, ButtonVariants},
    dialog::Dialog,
};
use pi_whim_engine::dialogs::{Answer, Choice, Prompt, Queue, Tone};
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// What the chooser asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptEvent {
    /// Send this decision back to the session that asked.
    Answered(Answer),
}

/// How a choice's tone reads as a button.
///
/// The engine says which answer is expected and which refuses; the mapping onto
/// the component library's variants belongs here.
fn variant(tone: Tone) -> ButtonVariant {
    match tone {
        Tone::Primary => ButtonVariant::Primary,
        Tone::Danger => ButtonVariant::Danger,
        Tone::Neutral => ButtonVariant::Default,
    }
}

/// The questions waiting, and the one on screen.
pub struct Prompts {
    queue: Queue,
    tokens: Tokens,
}

impl EventEmitter<PromptEvent> for Prompts {}

impl Prompts {
    pub fn new(tokens: Tokens) -> Self {
        Self {
            queue: Queue::new(),
            tokens,
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// Whether a question is on screen.
    pub fn is_asking(&self) -> bool {
        !self.queue.is_empty()
    }

    /// The question on screen, if any.
    pub fn current(&self) -> Option<&Prompt> {
        self.queue.current()
    }

    /// Queue a question.
    ///
    /// Takes a parsed [`Prompt`] rather than the wire JSON: reading the request
    /// is the engine's job, and this crate deliberately has no `serde_json`.
    pub fn push(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        self.queue.push(prompt);
        cx.notify();
    }

    /// Drop everything a session asked, because it has gone.
    pub fn forget_session(&mut self, session_key: &str, cx: &mut Context<Self>) {
        self.queue.forget_session(session_key);
        cx.notify();
    }

    /// Answer the question on screen and move to the next.
    pub fn answer(&mut self, value: &str, cx: &mut Context<Self>) {
        if let Some(answer) = self.queue.answer(value) {
            cx.emit(PromptEvent::Answered(answer));
        }
        cx.notify();
    }

    /// Close the question on screen without picking.
    ///
    /// Still an answer: the agent is blocked waiting, and the prompt names the
    /// cautious value to send — deny, for anything asking permission.
    pub fn dismiss(&mut self, cx: &mut Context<Self>) {
        if let Some(answer) = self.queue.dismiss() {
            cx.emit(PromptEvent::Answered(answer));
        }
        cx.notify();
    }

    /// One choice as a button.
    fn button(&self, index: usize, choice: &Choice, cx: &mut Context<Self>) -> Button {
        let value = choice.value.clone();
        Button::new(("prompt-choice", index))
            .label(SharedString::from(choice.label.clone()))
            .with_variant(variant(choice.tone))
            .on_click(cx.listener(move |prompts, _, _, cx| {
                prompts.answer(&value, cx);
            }))
    }
}

impl Render for Prompts {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(prompt) = self.queue.current().cloned() else {
            return div();
        };
        let tokens = self.tokens;
        let buttons: Vec<Button> = prompt
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| self.button(index, choice, cx))
            .collect();
        // `on_cancel` answers a bool — whether the dialog may close — so it takes
        // a plain closure rather than `cx.listener`, which discards the return.
        let this = cx.entity();

        div().child(
            Dialog::new(cx)
                .title(SharedString::from(prompt.title.clone()))
                // The choices *are* the actions, so the default OK/Cancel pair
                // would be a second, contradictory way to answer.
                .footer(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_end()
                        .gap(px(8.0))
                        .children(buttons),
                )
                // Escape and the overlay both close it, which sends the cautious
                // answer rather than leaving the agent waiting.
                .on_cancel(move |_, _, cx| {
                    this.update(cx, |prompts, cx| prompts.dismiss(cx));
                    true
                })
                .when(!prompt.message.is_empty(), |dialog| {
                    dialog.child(
                        div()
                            .text_size(px(text::DETAIL_SIZE))
                            .text_color(tokens.copy.hsla())
                            .child(SharedString::from(prompt.message.clone())),
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expected_answer_stands_out_and_a_refusal_warns() {
        assert_eq!(variant(Tone::Primary), ButtonVariant::Primary);
        assert_eq!(variant(Tone::Danger), ButtonVariant::Danger);
        assert_eq!(variant(Tone::Neutral), ButtonVariant::Default);
    }

    #[test]
    fn a_refusal_never_looks_like_the_expected_answer() {
        // These sit side by side, and mistaking one for the other grants access
        // the reader meant to deny.
        assert_ne!(variant(Tone::Primary), variant(Tone::Danger));
        assert_ne!(variant(Tone::Neutral), variant(Tone::Danger));
    }
}
