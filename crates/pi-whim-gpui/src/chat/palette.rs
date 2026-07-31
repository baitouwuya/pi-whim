//! The slash-command palette.
//!
//! What it offers comes from `engine::slash_commands`, which is a pure query over
//! state. This module is only the presentation and the keyboard: a list anchored
//! above the composer, arrows to move, Enter or Tab to run, Escape to dismiss.
//!
//! The composer keeps focus while the palette is open, so typing continues to
//! filter. The catch is that the input binds arrows, Enter, and Tab to its own
//! actions, and a bound action never reaches a key listener: those three arrive
//! here as captured *actions* ([`Palette::handle_palette_key`]), while Escape —
//! which the input lets propagate — still comes through `capture_key_down`.

use gpui::{
    Context, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent, Keystroke, ParentElement,
    Render, ScrollHandle, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::Icon;
use pi_whim_core::{AppState, Language, strings::text as translate};
use pi_whim_engine::slash_commands::{
    SlashCommand, SlashCommandOption, options as command_options,
};
use pi_whim_theme::{Tokens, text};

use crate::{elements::isolated_vertical_scroll_area, icons, theme::IntoHsla};

/// Width of the panel, and how tall the list grows before it scrolls.
const PALETTE_WIDTH: f32 = 700.0;
const PALETTE_MAX_HEIGHT: f32 = 272.0;

/// What the palette asks the shell to do.
#[derive(Clone, Debug)]
pub enum PaletteEvent {
    /// Run the picked command.
    Run(SlashCommand),
    /// Replace the composer's text, for the commands that take an argument.
    SetComposerText(String),
}

/// A navigation key, named rather than read from a keystroke.
///
/// Arrows, Enter, and Tab reach the palette as captured input actions, not as
/// key events — see the module note. Escape is absent because it is the one
/// navigation key the input lets propagate, so it still arrives as a keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteKey {
    Up,
    Down,
    Run,
}

/// What a keystroke means to an open palette.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyAction {
    /// Move the selection by this many rows, wrapping.
    Move(isize),
    /// Run the selected option.
    Run,
    /// Close the panel, leaving the text alone.
    Dismiss,
    /// Not ours; let it reach the input.
    Ignore,
}

/// Read a keystroke, without touching state.
///
/// Modified keys are never the palette's: Shift+Enter is a newline the input
/// owns, and Cmd+A selects the text. Only the bare keys navigate.
fn key_action(keystroke: &Keystroke) -> KeyAction {
    let modifiers = &keystroke.modifiers;
    if modifiers.shift || modifiers.control || modifiers.alt || modifiers.platform {
        return KeyAction::Ignore;
    }
    match keystroke.key.as_str() {
        "up" => KeyAction::Move(-1),
        "down" => KeyAction::Move(1),
        // Tab as well as Enter: the palette reads as a completion, and Tab is
        // what completes elsewhere.
        "enter" | "tab" => KeyAction::Run,
        "escape" => KeyAction::Dismiss,
        _ => KeyAction::Ignore,
    }
}

/// Where the selection goes on an arrow key.
///
/// Wrapping is deliberate: the list is short and the reader is at the keyboard,
/// so running off the end should return to the start rather than stick.
fn step(selection: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let count = count as isize;
    (((selection as isize + delta) % count + count) % count) as usize
}

/// The palette, and where the selection sits within it.
pub struct Palette {
    options: Vec<SlashCommandOption>,
    selection: usize,
    /// The composer text the current options were derived from. A change resets
    /// the selection, since row 3 of the old list means nothing in the new one.
    query: Option<String>,
    /// A query the reader dismissed with Escape. Kept so the panel does not
    /// reopen on the next keystroke that leaves the text unchanged.
    dismissed: Option<String>,
    scroll: ScrollHandle,
    /// The language the panel's own headings are read in.
    ///
    /// Kept from the snapshot `sync` already receives rather than set separately —
    /// the options' own titles come from the same state, so a second path could
    /// leave the heading in one language and the rows in the other.
    language: Language,
    tokens: Tokens,
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Palette {
    pub fn new(tokens: Tokens) -> Self {
        Self {
            options: Vec::new(),
            selection: 0,
            query: None,
            dismissed: None,
            scroll: ScrollHandle::new(),
            language: Language::default(),
            tokens,
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// Whether the palette is showing anything, and so whether it owns the keys.
    pub fn is_open(&self) -> bool {
        !self.options.is_empty()
    }

    /// Re-derive the options from what is typed.
    ///
    /// Called on every composer keystroke: the palette opens, filters, and closes
    /// purely as a function of the text, with no separate open/close state to
    /// leave stale.
    pub fn sync(&mut self, state: &AppState, composer_text: &str, cx: &mut Context<Self>) {
        self.language = state.language;
        let Some(options) = command_options(state, composer_text) else {
            // Not a slash query at all. Clearing the dismissal too, so a later
            // `/` reopens rather than staying suppressed.
            self.options.clear();
            self.query = None;
            self.dismissed = None;
            cx.notify();
            return;
        };

        let query = composer_text.to_owned();
        if self.query.as_deref() != Some(query.as_str()) {
            // Different text, different list: the old index is meaningless.
            self.selection = 0;
            self.query = Some(query.clone());
        }
        self.options = if self.dismissed.as_deref() == Some(query.as_str()) {
            Vec::new()
        } else {
            options
        };
        // Guard against a shrinking list leaving the selection past the end.
        self.selection = self.selection.min(self.options.len().saturating_sub(1));
        cx.notify();
    }

    /// Handle a key while the palette is open.
    ///
    /// Returns whether the palette consumed it, so the caller knows to stop the
    /// keystroke reaching the input.
    pub fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if !self.is_open() {
            return false;
        }
        self.act(key_action(&event.keystroke), cx)
    }

    /// Handle a navigation key captured as an input action.
    ///
    /// Same contract as [`Palette::handle_key`]: whether the palette took it.
    pub fn handle_palette_key(&mut self, key: PaletteKey, cx: &mut Context<Self>) -> bool {
        if !self.is_open() {
            return false;
        }
        let action = match key {
            PaletteKey::Up => KeyAction::Move(-1),
            PaletteKey::Down => KeyAction::Move(1),
            PaletteKey::Run => KeyAction::Run,
        };
        self.act(action, cx)
    }

    fn act(&mut self, action: KeyAction, cx: &mut Context<Self>) -> bool {
        match action {
            KeyAction::Ignore => return false,
            KeyAction::Move(delta) => {
                self.selection = step(self.selection, self.options.len(), delta);
            }
            KeyAction::Run => {
                self.run(self.selection, cx);
                return true;
            }
            KeyAction::Dismiss => {
                // Suppress this exact query rather than clearing the text: the
                // reader dismissed the menu, not what they typed.
                self.dismissed = self.query.clone();
                self.options.clear();
            }
        }
        cx.notify();
        true
    }

    /// Run the option at `index`.
    pub fn run(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(option) = self.options.get(index) else {
            return;
        };
        let command = option.command.clone();
        self.options.clear();
        self.query = None;
        self.dismissed = None;

        match prefill(&command) {
            Some(text) => cx.emit(PaletteEvent::SetComposerText(text)),
            None => cx.emit(PaletteEvent::Run(command)),
        }
        cx.notify();
    }
}

/// What running `command` should leave in the field, if it needs an argument.
///
/// These do not reach the host at all: the reader still has to say which model,
/// level, or entry, and sending an incomplete command to the backend so it could
/// answer by changing what is typed would be a round trip to move a cursor.
fn prefill(command: &SlashCommand) -> Option<String> {
    match command {
        SlashCommand::ChooseModel => Some("/model ".into()),
        SlashCommand::ChooseThinkingLevel => Some("/thinking ".into()),
        SlashCommand::ChooseFork => Some("/fork ".into()),
        // A name is a command once it has one; without, it is a request for one.
        SlashCommand::NameSession(None) => Some("/name ".into()),
        // Pi's own commands, listed from what it reported. The trigger goes in the
        // field so arguments can be added before it is sent.
        SlashCommand::SubmitDynamic(name) => Some(format!("/{name} ")),
        _ => None,
    }
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        if !self.is_open() {
            return div();
        }
        let selection = self.selection;

        // Position is the shell's business, not this view's: it anchors the panel
        // above the prompt. Positioning here against whatever box happened to be
        // the containing block is what put the list on top of the field it filters.
        div()
            .w(px(PALETTE_WIDTH))
            .max_w_full()
            .bg(tokens.panel.hsla())
            .border_1()
            .border_color(tokens.line.hsla())
            .shadow_lg()
            .child(
                div()
                    .px(px(8.0))
                    .pt(px(6.0))
                    .font_family(pi_whim_theme::font::MONO)
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(translate("slash-commands", self.language))
                    // How to drive it, beside what it is: the keys are not
                    // discoverable from a list of rows, and this header row is
                    // already paid for.
                    .child(
                        div()
                            .flex_1()
                            .text_align(gpui::TextAlign::Right)
                            .text_color(tokens.line_strong.hsla())
                            .child(translate("slash-help", self.language)),
                    ),
            )
            .child(
                isolated_vertical_scroll_area("palette-rows", &self.scroll)
                    .max_h(px(PALETTE_MAX_HEIGHT))
                    .children(self.options.iter().enumerate().map(|(index, option)| {
                        let is_selected = index == selection;
                        div()
                            .id(("palette-row", index))
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(8.0))
                            .py(px(4.0))
                            .when(is_selected, |row| row.bg(tokens.selection().hsla()))
                            .hover(|row| row.bg(tokens.control_background_hover().hsla()))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(move |palette, _, _, cx| palette.run(index, cx)),
                            )
                            .child(
                                Icon::new(icons::command(option.icon))
                                    .size(px(20.0))
                                    .text_color(tokens.muted.hsla()),
                            )
                            // Name and description on one line, not stacked. Two
                            // lines per option made ten commands a wall of text and
                            // halved how many fit before scrolling; the name is what
                            // is being scanned, and the description only has to be
                            // there when the eye stops.
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(px(text::DETAIL_SIZE))
                                    .text_color(tokens.text.hsla())
                                    .child(SharedString::from(option.title.clone())),
                            )
                            .child(
                                // What to type, beside what it does: the palette
                                // teaches the commands it offers, and the trigger
                                // is the part worth remembering.
                                div()
                                    .flex_none()
                                    .font_family(pi_whim_theme::font::MONO)
                                    .text_size(px(text::LABEL_SIZE))
                                    .text_color(tokens.accent.hsla())
                                    .child(SharedString::from(option.trigger.clone())),
                            )
                            .child(
                                // Right-aligned, and the only part that gives up
                                // space: a long description truncates rather than
                                // pushing the name it belongs to out of line.
                                div()
                                    .flex_1()
                                    .min_w(px(0.0))
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    .text_right()
                                    .text_size(px(text::LABEL_SIZE))
                                    .text_color(tokens.muted.hsla())
                                    .child(SharedString::from(option.detail.clone())),
                            )
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::AppContext;

    #[test]
    fn stepping_wraps_at_both_ends() {
        // The list is short and the reader is on the keyboard: running off the
        // bottom should return to the top rather than stick there.
        assert_eq!(step(0, 3, 1), 1);
        assert_eq!(step(2, 3, 1), 0);
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(1, 3, -1), 0);
    }

    #[test]
    fn stepping_an_empty_list_stays_put() {
        // Guards the modulo: an empty list would divide by zero.
        assert_eq!(step(0, 0, 1), 0);
        assert_eq!(step(0, 0, -1), 0);
    }

    #[test]
    fn stepping_a_single_option_is_a_no_op() {
        assert_eq!(step(0, 1, 1), 0);
        assert_eq!(step(0, 1, -1), 0);
    }

    #[test]
    fn a_command_that_needs_an_argument_only_prefills_the_field() {
        // Running these would ask the backend to change what is typed, which it
        // would answer by asking the shell to change what is typed.
        assert_eq!(
            prefill(&SlashCommand::ChooseModel).as_deref(),
            Some("/model ")
        );
        assert_eq!(
            prefill(&SlashCommand::NameSession(None)).as_deref(),
            Some("/name ")
        );
        assert_eq!(
            prefill(&SlashCommand::SubmitDynamic("review".into())).as_deref(),
            Some("/review ")
        );
    }

    #[test]
    fn a_complete_command_runs() {
        // Including a name that already has its argument: the prefill is for the
        // bare trigger, not for the command.
        assert_eq!(prefill(&SlashCommand::Compact), None);
        assert_eq!(
            prefill(&SlashCommand::NameSession(Some("audit".into()))),
            None
        );
    }

    fn keystroke(key: &str) -> Keystroke {
        Keystroke::parse(key).expect("a parseable keystroke")
    }

    #[test]
    fn arrows_move_the_selection_and_enter_runs() {
        assert_eq!(key_action(&keystroke("up")), KeyAction::Move(-1));
        assert_eq!(key_action(&keystroke("down")), KeyAction::Move(1));
        assert_eq!(key_action(&keystroke("enter")), KeyAction::Run);
        // Tab too: the palette reads as a completion, and Tab is what completes.
        assert_eq!(key_action(&keystroke("tab")), KeyAction::Run);
        assert_eq!(key_action(&keystroke("escape")), KeyAction::Dismiss);
    }

    #[test]
    fn ordinary_typing_reaches_the_input() {
        // Filtering happens by typing, so anything that is not navigation has to
        // pass through untouched.
        assert_eq!(key_action(&keystroke("a")), KeyAction::Ignore);
        assert_eq!(key_action(&keystroke("/")), KeyAction::Ignore);
        assert_eq!(key_action(&keystroke("backspace")), KeyAction::Ignore);
    }

    #[test]
    fn a_modified_key_is_never_the_palettes() {
        // Shift+Enter is the newline the input owns; taking it here would make
        // multi-line prompts impossible while the panel is open.
        assert_eq!(key_action(&keystroke("shift-enter")), KeyAction::Ignore);
        assert_eq!(key_action(&keystroke("cmd-a")), KeyAction::Ignore);
        assert_eq!(key_action(&keystroke("ctrl-n")), KeyAction::Ignore);
        assert_eq!(key_action(&keystroke("alt-up")), KeyAction::Ignore);
    }

    // The panel is wide enough for a title and a detail line side by side, and
    // bounded so a long dynamic-command list scrolls instead of filling the pane.
    const _: () = {
        assert!(PALETTE_WIDTH > 0.0);
        assert!(PALETTE_MAX_HEIGHT > 0.0);
    };

    struct Probe;

    #[gpui::test]
    async fn captured_actions_move_the_selection_and_run_it(cx: &mut gpui::TestAppContext) {
        use std::{cell::RefCell, rc::Rc};

        // Arrows, Enter, and Tab reach the palette as captured input actions —
        // the input bound them first — so this exercises the path those keys
        // actually take, which is the one that once let Enter submit the raw
        // query as a prompt.
        let palette = cx.update(|cx| cx.new(|_| Palette::new(Tokens::light())));
        let events: Rc<RefCell<Vec<PaletteEvent>>> = Rc::new(RefCell::new(Vec::new()));
        let observed = events.clone();
        let _probe = cx.update(|cx| {
            cx.new(|cx| {
                cx.subscribe(&palette, move |_, _, event: &PaletteEvent, _| {
                    observed.borrow_mut().push(event.clone());
                })
                .detach();
                Probe
            })
        });

        cx.update(|cx| {
            palette.update(cx, |palette, cx| {
                palette.sync(&AppState::default(), "/", cx);
                assert!(palette.is_open());

                // An arrow moves; running off the top wraps to the bottom.
                assert!(palette.handle_palette_key(PaletteKey::Down, cx));
                assert_eq!(palette.selection, 1);
                assert!(palette.handle_palette_key(PaletteKey::Up, cx));
                assert_eq!(palette.selection, 0);
                assert!(palette.handle_palette_key(PaletteKey::Up, cx));
                assert_eq!(palette.selection, palette.options.len() - 1);

                // A command that needs an argument only prefills the field.
                let model = palette
                    .options
                    .iter()
                    .position(|option| option.command == SlashCommand::ChooseModel)
                    .expect("the model command is listed");
                palette.selection = model;
                assert!(palette.handle_palette_key(PaletteKey::Run, cx));
                assert!(!palette.is_open(), "running closes the panel");

                // Closed, the keys belong to the input again.
                assert!(!palette.handle_palette_key(PaletteKey::Down, cx));
            });
        });

        let events = events.borrow();
        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            PaletteEvent::SetComposerText(text) if text == "/model "
        ));
    }
}
