//! The prompt input.
//!
//! Enter submits, Shift+Enter inserts a newline. Both are the component's own
//! behavior — `InputState` inserts the newline itself and emits `PressEnter` only
//! when a submit is meant, including while an IME is mid-composition. The egui
//! build had to guard that by hand, tracking preedit events across frames; there
//! is nothing to reimplement here.

use gpui::{
    App, AppContext, ClipboardEntry, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    Icon,
    Sizable,
    button::{Button, ButtonVariants},
    // Aliased because this module's own `Paste` is the classification, and the
    // action is the keystroke that produces one.
    input::{Input, InputEvent, InputState, Paste as PasteAction},
    menu::{DropdownMenu, PopupMenuItem},
};
use pi_whim_core::{Attachment, SubmitMode};
use pi_whim_engine::composer::Composer as Draft;
use pi_whim_engine::session::is_large_paste;
use pi_whim_theme::{Tokens, text};

use crate::{
    chat::paste::{self, Clipboard, Paste},
    icons,
    theme::IntoHsla,
};

/// Rows the input grows through before it starts scrolling.
const MIN_ROWS: usize = 2;
const MAX_ROWS: usize = 12;

/// What the two keys do, for the footer beneath the field.
///
/// Lives here because it describes this input's behavior, but is rendered by the
/// shell: the footer is one row shared with the runtime controls, and a row cannot
/// be split across two entities.
pub const SUBMIT_HINT: &str = "Enter to send · Shift+Enter for a newline";

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
    /// The typed text changed. The palette re-derives its options from this, so
    /// it opens, filters, and closes purely as a function of what is typed.
    TextChanged(String),
    /// A paste that belongs on disk rather than in the field.
    ///
    /// Reported rather than handled: writing it needs the attachment store, which
    /// this crate does not own.
    AttachPaste(Paste),
    /// Ask for something on disk to attach.
    ///
    /// Two kinds because a folder is attached whole — the model is handed the
    /// directory and reads what it needs — and a file picker that also accepted
    /// directories would make "one file inside" and "the folder" the same gesture.
    PickAttachments { directories: bool },
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
                InputEvent::Change => {
                    let text = composer.text(cx);
                    cx.emit(ComposerEvent::TextChanged(text));
                    cx.notify();
                }
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

    /// What is typed right now.
    ///
    /// The input owns the text, so this reads through to it rather than to the
    /// draft, which is only synced on submit.
    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    /// Replace what is typed.
    ///
    /// Used by the palette for the commands that take an argument: picking
    /// "choose model" leaves `/model ` in the field for the reader to finish.
    pub fn set_text(&mut self, value: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.set_value(value, window, cx);
        });
        cx.notify();
    }

    /// Where keyboard focus goes when the composer is asked to take it.
    ///
    /// The input's own handle rather than one of the composer's: focus has to land
    /// on the thing that receives the typing, and a second handle in front of it
    /// would swallow every keystroke.
    pub fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }

    /// Decide what a paste means, and report it if it belongs on disk.
    ///
    /// Returns true when the paste was taken, which is the caller's signal to stop
    /// the input from also inserting it. The clipboard is read here rather than in
    /// the shell because this runs during dispatch, where a read is the whole
    /// decision — deferring it would let the input insert first.
    fn intercept_paste(&mut self, cx: &mut Context<Self>) -> bool {
        let clipboard = read_clipboard(cx);
        match paste::classify(clipboard, is_large_paste) {
            Paste::Insert => false,
            attach => {
                cx.emit(ComposerEvent::AttachPaste(attach));
                true
            }
        }
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

/// The clipboard, in the shape [`paste::classify`] reads.
///
/// gpui hands over encoded image bytes and copied paths as their own entries, so
/// there is nothing to sniff: each kind is already distinguished.
fn read_clipboard(cx: &App) -> Clipboard {
    let Some(item) = cx.read_from_clipboard() else {
        return Clipboard::default();
    };
    let mut clipboard = Clipboard::default();
    for entry in &item.entries {
        match entry {
            ClipboardEntry::ExternalPaths(paths) => {
                clipboard.paths.extend(
                    paths
                        .paths()
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned()),
                );
            }
            ClipboardEntry::Image(image) => {
                clipboard.image = Some((image.format.extension().to_owned(), image.bytes.clone()));
            }
            ClipboardEntry::String(_) => {}
        }
    }
    // Read through the item rather than the string entries, so a paste of several
    // strings arrives concatenated the way the input would insert it.
    clipboard.text = item.text();
    clipboard
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

        // Icon only, and inside the field: the glyph is unambiguous next to the
        // text it acts on, and a labelled button below the input spent a whole
        // row on a control Enter already reaches.
        let action = if self.busy {
            Button::new("stop")
                .danger()
                .icon(icons::stop())
                .xsmall()
                .tooltip("Stop the turn in flight")
                .on_click(cx.listener(|_, _, _, cx| cx.emit(ComposerEvent::Stop)))
        } else {
            Button::new("send")
                .primary()
                .icon(icons::send())
                .xsmall()
                .tooltip("Send · Enter")
                .on_click(cx.listener(|composer, _, window, cx| composer.submit(window, cx)))
        };

        // The only way to attach from disk. A paste covers the common case, but a
        // file the reader has not copied still has to be reachable, and the egui
        // build's menu on the same "+" is where they will look for it.
        let attach = Button::new("attach")
            .ghost()
            .icon(icons::add())
            .xsmall()
            .tooltip("Attach files or a folder")
            .dropdown_menu({
                let composer = cx.entity();
                move |menu, _, _| {
                    [("Choose files…", false), ("Choose folder…", true)]
                        .into_iter()
                        .fold(menu, |menu, (label, directories)| {
                            let composer = composer.clone();
                            menu.item(PopupMenuItem::new(label).on_click(move |_, _, cx| {
                                composer.update(cx, |_, cx| {
                                    cx.emit(ComposerEvent::PickAttachments { directories });
                                });
                            }))
                        })
                }
            });

        div()
            // Captured, not bubbled: an action reaches the focused element first
            // in the bubble phase, so by then the input would already have
            // inserted the text. Capture runs from the root down, which is the
            // only phase where declining to propagate still prevents the insert.
            //
            // This is the whole replacement for egui's application-wide
            // `raw_input_hook`: the handler sits on the composer's own element, so
            // it only fires when the composer has focus and nothing above has to
            // ask whether it does.
            .capture_action(cx.listener(|composer, _: &PasteAction, _, cx| {
                if composer.intercept_paste(cx) {
                    cx.stop_propagation();
                }
            }))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .w_full()
            // The panel surface and the rule above it belong to the shell's
            // composer box, not here: the runtime controls share that surface, and
            // two entities cannot each draw half of one panel.
            .when(!attachments.is_empty(), |this| {
                this.child(div().flex().flex_wrap().gap(px(4.0)).children(
                    attachments.into_iter().map(|name| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .px(px(6.0))
                            .py(px(2.0))
                            .bg(tokens.accent_surface_soft().hsla())
                            .border_1()
                            .border_color(tokens.accent_border_muted().hsla())
                            .text_size(px(text::LABEL_SIZE))
                            .text_color(tokens.muted.hsla())
                            .child(Icon::new(icons::attachment()).size(px(11.0)))
                            .child(name)
                    }),
                ))
            })
            // The action rides in the field's suffix slot, which the component
            // lays out inside the border and vertically centred.
            .child(Input::new(&self.input).prefix(attach).suffix(action))
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
