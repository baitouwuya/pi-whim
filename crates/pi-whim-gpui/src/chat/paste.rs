//! What a paste into the composer means.
//!
//! Most pastes are text and belong in the field. Three kinds do not: copied
//! files, a screenshot, and a wall of text long enough to bury the prompt. Those
//! become attachments, so the model gets a path to read instead of the whole
//! thing inlined in the message.
//!
//! The egui build did this from `raw_input_hook`, an application-wide hook that
//! had to ask the view whether the composer had focus before deciding. That was
//! the only place the view leaked `egui::Context` into the app's API. Here the
//! composer intercepts the paste on its own element, so focus decides by itself
//! and nothing above needs to ask.
//!
//! This module only classifies. Writing an attachment to disk needs the store,
//! which the shell does not own, so the decision travels up as a request.
//!
//! The rules below are asserted here; the keystroke that reaches them is not.
//! Laying out the input headlessly panics — `gpui_component::Input` asks for a
//! `window_handle`, and gpui's test window declares that `unimplemented!` — so a
//! `cmd-v` test needs a real window. What that would add over these assertions is
//! only that the binding is wired, which the composer's `capture_action` shows by
//! inspection.

/// What should happen to a paste.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Paste {
    /// Let the input insert it.
    Insert,
    /// Attach the copied files.
    Files(Vec<String>),
    /// Attach the clipboard image.
    ///
    /// Already-encoded bytes with the extension that names the encoding, because
    /// that is what the platform clipboard hands over. Decoding to re-encode
    /// would lose quality on a JPEG and gain nothing on a PNG.
    Image { extension: String, bytes: Vec<u8> },
    /// Attach the text rather than inlining it.
    LongText(String),
}

/// The clipboard as this module needs to see it.
///
/// A plain struct and not gpui's `ClipboardItem` so the rules below can be
/// asserted without a window or a live clipboard.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Clipboard {
    /// Paths, when files were copied in a file manager.
    pub paths: Vec<String>,
    pub text: Option<String>,
    /// An encoded image and the extension naming its encoding.
    pub image: Option<(String, Vec<u8>)>,
}

/// Decide what a paste of `clipboard` means.
///
/// Files win over text because a file manager puts both on the clipboard — the
/// paths are also available as their own text — and pasting the path strings
/// would give the model a list of names with nothing behind them.
pub fn classify(clipboard: Clipboard, is_long: impl Fn(&str) -> bool) -> Paste {
    if !clipboard.paths.is_empty() {
        return Paste::Files(clipboard.paths);
    }
    // Before the text check for the same reason: a screenshot can arrive
    // alongside a text rendering of itself, and the image is what was meant.
    if let Some((extension, bytes)) = clipboard.image {
        return Paste::Image { extension, bytes };
    }
    match clipboard.text {
        Some(text) if is_long(&text) => Paste::LongText(text),
        _ => Paste::Insert,
    }
}

#[cfg(test)]
mod tests {
    use pi_whim_engine::session::is_large_paste;

    use super::*;

    fn text(value: &str) -> Clipboard {
        Clipboard {
            text: Some(value.to_owned()),
            ..Clipboard::default()
        }
    }

    #[test]
    fn ordinary_text_goes_into_the_field() {
        assert_eq!(
            classify(text("a short question"), is_large_paste),
            Paste::Insert
        );
    }

    #[test]
    fn a_wall_of_text_becomes_an_attachment() {
        // Inlined, a long log buries the actual question in the message.
        let long = "line\n".repeat(40);
        assert_eq!(
            classify(text(&long), is_large_paste),
            Paste::LongText(long.clone())
        );
    }

    #[test]
    fn copied_files_win_over_the_paths_as_text() {
        // A file manager puts both on the clipboard. Pasting the text would give
        // the model names with no contents behind them.
        let clipboard = Clipboard {
            paths: vec!["/tmp/a.rs".to_owned()],
            text: Some("/tmp/a.rs".to_owned()),
            image: None,
        };

        assert_eq!(
            classify(clipboard, is_large_paste),
            Paste::Files(vec!["/tmp/a.rs".to_owned()])
        );
    }

    #[test]
    fn an_image_wins_over_text_that_came_with_it() {
        let clipboard = Clipboard {
            paths: Vec::new(),
            text: Some("screenshot".to_owned()),
            image: Some(("png".to_owned(), vec![0x89, b'P', b'N', b'G'])),
        };

        assert_eq!(
            classify(clipboard, is_large_paste),
            Paste::Image {
                extension: "png".to_owned(),
                bytes: vec![0x89, b'P', b'N', b'G'],
            }
        );
    }

    #[test]
    fn an_empty_clipboard_is_left_to_the_input() {
        // Nothing to attach, and the input's own paste handles the no-op.
        assert_eq!(
            classify(Clipboard::default(), is_large_paste),
            Paste::Insert
        );
    }
}
