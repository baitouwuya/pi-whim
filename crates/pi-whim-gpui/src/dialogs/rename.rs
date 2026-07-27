//! Renaming a session.
//!
//! A one-field form, but the field is the whole point: a session's title is what
//! makes the sidebar readable once there are twenty of them, so the dialog opens
//! with the current title selected and Enter commits.

use gpui::{
    AppContext, Context, Entity, EventEmitter, Focusable, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, px,
};
use gpui_component::{
    dialog::Dialog,
    input::{Input, InputEvent, InputState},
};
use pi_whim_core::{Language, strings::text as translate};
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// What the rename dialog asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenameEvent {
    /// Store this title for the session at `path`.
    Renamed { path: String, title: String },
}

/// A session waiting to be renamed.
pub struct Rename {
    /// The session being renamed. `None` means the dialog is closed.
    path: Option<String>,
    input: Entity<InputState>,
    /// The language the heading, the label, and the buttons are read in.
    language: Language,
    tokens: Tokens,
}

impl EventEmitter<RenameEvent> for Rename {}

impl Rename {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = Language::default();
        let input = cx.new(|cx| {
            InputState::new(window, cx).placeholder(translate("session-title", language))
        });
        // Enter commits, which is what a one-field form should do; reaching for
        // the mouse to confirm a rename is friction with no purpose.
        cx.subscribe_in(&input, window, |rename, _, event, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                rename.commit(cx);
            }
        })
        .detach();

        Self {
            path: None,
            input,
            language,
            tokens,
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// Switch the language of the dialog's own text.
    ///
    /// The placeholder has to be pushed into `InputState` rather than read at
    /// render: the component owns it.
    pub fn set_language(
        &mut self,
        language: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.language == language {
            return;
        }
        self.language = language;
        self.input.update(cx, |input, cx| {
            input.set_placeholder(translate("session-title", language), window, cx);
        });
        cx.notify();
    }

    pub fn is_open(&self) -> bool {
        self.path.is_some()
    }

    /// Open on `path`, seeded with the title it has now.
    ///
    /// Seeded rather than blank: most renames are edits of an auto-generated
    /// title, so starting from it saves retyping.
    pub fn open(
        &mut self,
        path: impl Into<String>,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.path = Some(path.into());
        // Focused on open: the reader came here to type, and a form that needs a
        // click before it accepts a keystroke is friction with no purpose.
        let focus = self.input.update(cx, |input, cx| {
            input.set_value(title, window, cx);
            input.focus_handle(cx)
        });
        focus.focus(window, cx);
        cx.notify();
    }

    /// Close without renaming.
    pub fn close(&mut self, cx: &mut Context<Self>) {
        self.path = None;
        cx.notify();
    }

    /// Commit the typed title, if there is one worth storing.
    pub fn commit(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.path.clone() else {
            return;
        };
        let Some(title) = usable_title(&self.input.read(cx).value()) else {
            return;
        };
        self.path = None;
        cx.emit(RenameEvent::Renamed { path, title });
        cx.notify();
    }
}

/// The title to store for what was typed, if it is worth storing.
///
/// Trimmed, because trailing space in a sidebar label is invisible and confusing.
/// Blank is rejected rather than stored: it would leave the row unlabelled with
/// no way back except renaming it again.
fn usable_title(typed: &str) -> Option<String> {
    let trimmed = typed.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

impl Render for Rename {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.is_open() {
            return div();
        }
        let tokens = self.tokens;
        // Both callbacks answer a bool — whether the dialog may close — so they
        // take plain closures rather than `cx.listener`, which discards it.
        let this = cx.entity();

        div().child(
            Dialog::new(cx)
                .title(SharedString::from(translate(
                    "rename-session",
                    self.language,
                )))
                .button_props(
                    gpui_component::dialog::DialogButtonProps::default()
                        .ok_text(translate("save", self.language))
                        .show_cancel(true),
                )
                .on_ok({
                    let this = this.clone();
                    move |_, _, cx| {
                        this.update(cx, |rename, cx| rename.commit(cx));
                        // Only close once something was stored: a blank title is
                        // rejected, and closing anyway would silently discard it.
                        !this.read(cx).is_open()
                    }
                })
                .on_cancel(move |_, _, cx| {
                    this.update(cx, |rename, cx| rename.close(cx));
                    true
                })
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .font_family(pi_whim_theme::font::MONO)
                                .text_size(px(text::LABEL_SIZE))
                                .text_color(tokens.muted.hsla())
                                .child(translate("title-field", self.language)),
                        )
                        .child(Input::new(&self.input)),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_title_is_stored_trimmed() {
        // Trailing space in a sidebar label is invisible and confusing.
        assert_eq!(
            usable_title("  Migrate the UI  ").as_deref(),
            Some("Migrate the UI")
        );
    }

    #[test]
    fn a_blank_title_is_rejected() {
        // Storing it would leave the row unlabelled with no way back except
        // renaming again, so Save does nothing and the dialog stays open.
        assert_eq!(usable_title(""), None);
        assert_eq!(usable_title("   "), None);
        assert_eq!(usable_title("\t\n"), None);
    }

    #[test]
    fn inner_spacing_is_left_alone() {
        // Only the ends are noise; the middle is what the reader typed.
        assert_eq!(
            usable_title("Migrate  the   UI").as_deref(),
            Some("Migrate  the   UI")
        );
    }
}
