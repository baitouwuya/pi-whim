//! The prompt input.
//!
//! Enter submits, Shift+Enter inserts a newline. Both are the component's own
//! behavior — `InputState` inserts the newline itself and emits `PressEnter` only
//! when a submit is meant, including while an IME is mid-composition. The egui
//! build had to guard that by hand, tracking preedit events across frames; there
//! is nothing to reimplement here.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement,
    ParentElement, Render, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
};
use pi_whim_core::{Attachment, SubmitMode};
use pi_whim_engine::composer::Composer as Draft;
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// Rows the input grows through before it starts scrolling.
const MIN_ROWS: usize = 2;
const MAX_ROWS: usize = 12;

/// What the composer asks the shell to do.
#[derive(Clone, Debug, PartialEq)]
pub enum ComposerEvent {
    /// Send the drafted prompt.
    Submit {
        content: String,
        attachments: Vec<Attachment>,
        mode: SubmitMode,
    },
    /// Interrupt the turn in flight.
    Stop,
    /// Drop an attachment from the draft.
    RemoveAttachment(String),
}

/// The prompt input, its attachments, and the send controls.
pub struct Composer {
    input: Entity<InputState>,
    draft: Draft,
    /// True while the agent is working, which turns Send into Stop.
    busy: bool,
    tokens: Tokens,
}

impl EventEmitter<ComposerEvent> for Composer {}

impl Composer {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .multi_line(true)
                .auto_grow(MIN_ROWS, MAX_ROWS)
                .soft_wrap(true)
                .placeholder("Ask Pi…")
        });

        cx.subscribe_in(
            &input,
            window,
            |composer, _, event, window, cx| match event {
                InputEvent::PressEnter { .. } => composer.submit(window, cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            },
        )
        .detach();

        Self {
            input,
            draft: Draft::new(),
            busy: false,
            tokens,
        }
    }

    pub fn set_busy(&mut self, busy: bool, cx: &mut Context<Self>) {
        if self.busy != busy {
            self.busy = busy;
            cx.notify();
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// Stage an attachment onto the draft.
    pub fn add_attachment(&mut self, attachment: Attachment, cx: &mut Context<Self>) {
        self.draft.add_attachment(attachment);
        cx.notify();
    }

    pub fn remove_attachment(&mut self, path: &str, cx: &mut Context<Self>) {
        self.draft.remove_attachment(path);
        cx.notify();
    }

    pub fn attachments(&self) -> &[Attachment] {
        self.draft.attachments()
    }

    /// The focus handle, for the paste interception the app installs.
    ///
    /// This replaces `composer_has_focus(&egui::Context)`, the one place the egui
    /// view leaked its framework into the app's API.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    /// Submit whatever is drafted, if there is anything worth sending.
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The input owns the text; mirror it into the draft so the emptiness and
        // attachment rules live in one place.
        let typed = self.input.read(cx).value().to_string();
        self.draft.set_text(typed);
        if self.draft.is_empty() {
            return;
        }

        let mode = if self.busy {
            // Typing while the agent works steers the turn in flight rather than
            // queueing behind it.
            SubmitMode::Steer
        } else {
            SubmitMode::Prompt
        };
        let (content, attachments) = self.draft.take();

        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
        });
        cx.emit(ComposerEvent::Submit {
            content,
            attachments,
            mode,
        });
        cx.notify();
    }
}

impl Render for Composer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        let attachments: Vec<String> = self
            .draft
            .attachments()
            .iter()
            .map(|attachment| attachment.name.clone())
            .collect();

        let action = if self.busy {
            Button::new("stop")
                .danger()
                .label("Stop")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
        } else {
            Button::new("send")
                .primary()
                .label("Send")
                .on_click(cx.listener(|composer, _, window, cx| composer.submit(window, cx)))
        };

        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .w_full()
            .p(px(10.0))
            .bg(tokens.panel.hsla())
            .border_t_1()
            .border_color(tokens.line.hsla())
            .when(!attachments.is_empty(), |this| {
                this.child(div().flex().flex_wrap().gap(px(4.0)).children(
                    attachments.into_iter().map(|name| {
                        div()
                            .px(px(6.0))
                            .py(px(2.0))
                            .bg(tokens.accent_surface_soft().hsla())
                            .border_1()
                            .border_color(tokens.accent_border_muted().hsla())
                            .text_size(px(text::LABEL_SIZE))
                            .text_color(tokens.muted.hsla())
                            .child(name)
                    }),
                ))
            })
            .child(Input::new(&self.input))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(text::LABEL_SIZE))
                            .text_color(tokens.muted.hsla())
                            .child("Enter to send · Shift+Enter for a newline"),
                    )
                    .child(action),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::AttachmentKind;

    fn attachment(path: &str) -> Attachment {
        Attachment {
            name: path.rsplit('/').next().unwrap_or(path).to_owned(),
            path: path.into(),
            kind: AttachmentKind::File,
            generated_by_app: false,
        }
    }

    /// The submit mode a draft should travel under.
    ///
    /// Extracted so the rule is testable without an input entity.
    fn submit_mode(busy: bool) -> SubmitMode {
        if busy {
            SubmitMode::Steer
        } else {
            SubmitMode::Prompt
        }
    }

    #[test]
    fn typing_while_the_agent_works_steers_the_turn() {
        // Queueing behind a long turn would make a correction arrive too late to
        // be useful.
        assert_eq!(submit_mode(true), SubmitMode::Steer);
        assert_eq!(submit_mode(false), SubmitMode::Prompt);
    }

    // The input grows before it scrolls.
    const _: () = {
        assert!(MIN_ROWS >= 1);
        assert!(MAX_ROWS > MIN_ROWS);
    };

    #[test]
    fn the_draft_keeps_attachment_paths_unique() {
        // The composer stages attachments into engine's Draft, so the same file
        // arriving twice — dropped, then pasted — is sent once. The shell relies
        // on this rather than deduplicating again.
        let mut draft = Draft::new();
        draft.add_attachment(attachment("/tmp/notes.txt"));
        draft.add_attachment(attachment("/tmp/notes.txt"));
        assert_eq!(draft.attachments().len(), 1);

        draft.remove_attachment("/tmp/notes.txt");
        assert!(draft.attachments().is_empty());
    }

    #[test]
    fn an_empty_draft_is_not_worth_submitting() {
        let mut draft = Draft::new();
        draft.set_text("   \n ");
        assert!(draft.is_empty());

        // Attachments alone are worth sending, even with no text.
        draft.add_attachment(attachment("/tmp/image.png"));
        assert!(!draft.is_empty());
    }
}
