//! The chooser for questions the agent asks.
//!
//! One view for both asking protocols: `engine::dialogs` already flattens a Pi
//! extension confirmation and a supervisor interaction into the same [`Prompt`].
//! The chooser paints inline, taking the composer's place in the prompt area
//! while a question waits: the agent is blocked on the answer, so the question
//! is the one thing the reader can type a reply to anyway.
//!
//! The queue is drained one at a time. Each asking agent is blocked on its
//! answer, so stacking questions would only make the reader decide which
//! blocked agent to unblock first.

use gpui::{
    AppContext, Context, Entity, EventEmitter, Focusable, IntoElement, ParentElement, Render,
    ScrollHandle, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable,
    button::{Button, ButtonVariant, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use pi_whim_core::{Language, strings::text as translate};
use pi_whim_engine::dialogs::{Answer, Choice, Label, Prompt, Queue, Tone};
use pi_whim_theme::{Tokens, text};

use crate::{elements::isolated_vertical_scroll_area, theme::IntoHsla};

/// The tallest the agent's message renders before it scrolls: long briefings
/// must not push the choices out of view.
const MESSAGE_MAX_HEIGHT: f32 = 140.0;

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

/// What a label reads as on screen.
///
/// The engine keeps its own wording as a key and the agent's as text, so this is
/// where the two come back together.
fn read(label: &Label, language: Language) -> SharedString {
    match label {
        Label::Key(key) => SharedString::from(translate(key, language)),
        Label::Verbatim(text) => SharedString::from(text.clone()),
    }
}

/// The questions waiting, and the one on screen.
pub struct Prompts {
    queue: Queue,
    /// The free-form answer field, offered when the question accepts one.
    input: Entity<InputState>,
    /// The question the input was last reset for: a typed draft survives
    /// re-renders but never leaks into the next question.
    draft_for: Option<String>,
    message_scroll: ScrollHandle,
    /// The language the app's own wording in a prompt is read in.
    language: Language,
    tokens: Tokens,
}

impl EventEmitter<PromptEvent> for Prompts {}

impl Prompts {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(translate("prompt-custom-placeholder", Language::default()))
        });
        cx.subscribe_in(
            &input,
            window,
            |prompts, _, event, _window, cx| match event {
                InputEvent::PressEnter { .. } => prompts.submit_custom(cx),
                // The send button's enabled state is the text's emptiness.
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )
        .detach();
        Self {
            queue: Queue::new(),
            input,
            draft_for: None,
            message_scroll: ScrollHandle::new(),
            language: Language::default(),
            tokens,
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    pub fn set_language(&mut self, language: Language, cx: &mut Context<Self>) {
        if self.language != language {
            self.language = language;
            cx.notify();
        }
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

    /// Send the typed answer, if there is one to send.
    fn submit_custom(&mut self, cx: &mut Context<Self>) {
        let text = self.input.read(cx).value().trim().to_owned();
        if text.is_empty() {
            return;
        }
        self.answer(&text, cx);
    }

    /// One choice as a button.
    fn button(&self, index: usize, choice: &Choice, cx: &mut Context<Self>) -> Button {
        let value = choice.value.clone();
        Button::new(("prompt-choice", index))
            .label(read(&choice.label, self.language))
            .with_variant(variant(choice.tone))
            .on_click(cx.listener(move |prompts, _, _, cx| {
                prompts.answer(&value, cx);
            }))
    }
}

impl Render for Prompts {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(prompt) = self.queue.current().cloned() else {
            return div();
        };
        let tokens = self.tokens;
        // A fresh question resets the field — seeded if the request named a
        // prefill — and a question that reads typed answers takes the focus so
        // typing goes straight there.
        if self.draft_for.as_deref() != Some(prompt.request_id.as_str()) {
            self.draft_for = Some(prompt.request_id.clone());
            let placeholder = prompt
                .placeholder
                .clone()
                .unwrap_or_else(|| translate("prompt-custom-placeholder", self.language).into());
            let prefill = prompt.prefill.clone().unwrap_or_default();
            self.input.update(cx, |input, cx| {
                input.set_placeholder(placeholder, window, cx);
                input.set_value(prefill, window, cx);
            });
            if prompt.allows_custom_answer {
                self.input.focus_handle(cx).focus(window, cx);
            }
        }
        let buttons: Vec<Button> = prompt
            .choices
            .iter()
            .enumerate()
            .map(|(index, choice)| self.button(index, choice, cx))
            .collect();
        let custom_empty = self.input.read(cx).value().trim().is_empty();

        div()
            .flex()
            .flex_col()
            .w_full()
            .gap(px(8.0))
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(read(&prompt.title, self.language)),
            )
            .when(!prompt.message.is_empty(), |this| {
                this.child(
                    isolated_vertical_scroll_area("prompt-message", &self.message_scroll)
                        .max_h(px(MESSAGE_MAX_HEIGHT))
                        .text_size(px(text::DETAIL_SIZE))
                        .text_color(tokens.copy.hsla())
                        .child(read(&prompt.message, self.language)),
                )
            })
            .when(!buttons.is_empty(), |this| {
                this.child(
                    div()
                        .flex()
                        .flex_wrap()
                        .justify_end()
                        .gap(px(8.0))
                        .children(buttons),
                )
            })
            .when(prompt.allows_custom_answer, |this| {
                this.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().flex_1().min_w(px(0.0)).child(Input::new(&self.input)))
                        .child(
                            Button::new("prompt-custom-send")
                                .label(translate("prompt-custom-send", self.language))
                                .with_variant(ButtonVariant::Primary)
                                .disabled(custom_empty)
                                .on_click(cx.listener(|prompts, _, _, cx| {
                                    prompts.submit_custom(cx);
                                })),
                        ),
                )
            })
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
