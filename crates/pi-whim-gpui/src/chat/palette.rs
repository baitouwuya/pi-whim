//! The slash-command palette.
//!
//! What it offers comes from `engine::slash_commands`, which is a pure query over
//! state. This module is only the presentation and the keyboard: a list anchored
//! above the composer, arrows to move, Enter or Tab to run, Escape to dismiss.
//!
//! The keys are bound with `capture_key_down`, which runs before the focused
//! input sees them. That ordering is the whole trick — the composer keeps focus
//! while the palette is open, so typing continues to filter, but an arrow moves
//! the selection instead of the caret and Enter runs the option instead of
//! submitting the prompt.

use gpui::{
    Context, EventEmitter, InteractiveElement, IntoElement, KeyDownEvent, Keystroke, ParentElement,
    Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::Icon;
use pi_whim_core::AppState;
use pi_whim_engine::slash_commands::{
    SlashCommand, SlashCommandOption, options as command_options,
};
use pi_whim_theme::{Tokens, text};

use crate::{icons, theme::IntoHsla};

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
        match key_action(&event.keystroke) {
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

        // The three that take an argument prefill the composer instead of
        // running: the reader still has to say which model, level, or message.
        match &command {
            SlashCommand::ChooseModel => {
                cx.emit(PaletteEvent::SetComposerText("/model ".into()));
            }
            SlashCommand::ChooseThinkingLevel => {
                cx.emit(PaletteEvent::SetComposerText("/thinking ".into()));
            }
            SlashCommand::ChooseFork => {
                cx.emit(PaletteEvent::SetComposerText("/fork ".into()));
            }
            _ => cx.emit(PaletteEvent::Run(command)),
        }
        cx.notify();
    }
}

impl Render for Palette {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        if !self.is_open() {
            return div();
        }
        let selection = self.selection;

        div()
            .absolute()
            // Anchored above the composer so the panel grows upward and never
            // covers the text it is filtering on.
            .bottom(px(8.0))
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
                    .child("SLASH COMMANDS"),
            )
            .child(
                div()
                    .id("palette-rows")
                    .max_h(px(PALETTE_MAX_HEIGHT))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
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
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .child(
                                        div()
                                            .text_size(px(text::DETAIL_SIZE))
                                            .text_color(tokens.text.hsla())
                                            .child(SharedString::from(option.title.clone())),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(text::LABEL_SIZE))
                                            .text_color(tokens.muted.hsla())
                                            .child(SharedString::from(option.detail.clone())),
                                    ),
                            )
                    })),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
