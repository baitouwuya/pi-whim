//! Registers the bundled fallback fonts.
//!
//! macOS resolves pi.dev's own next choices — Georgia for serif, SF Mono for
//! monospace — from the system, so only the faces the system lacks are
//! embedded: CJK coverage and emoji. These are the same files the egui build
//! shipped.

use std::borrow::Cow;

use gpui::App;

/// Simplified Chinese coverage. macOS has PingFang, but bundling keeps text
/// identical across machines and matches what the egui build did.
const CJK_FONT: &[u8] = include_bytes!("../../pi-whim-ui/assets/NotoSansCJKsc-Regular.otf");
const EMOJI_FONT: &[u8] = include_bytes!("../../pi-whim-ui/assets/NotoEmoji.ttf");

/// Load the bundled fonts into the text system.
///
/// Returns an error only if the font data itself fails to parse, which would
/// mean a corrupt build rather than a missing system face.
pub fn install(cx: &App) -> anyhow::Result<()> {
    cx.text_system()
        .add_fonts(vec![Cow::Borrowed(CJK_FONT), Cow::Borrowed(EMOJI_FONT)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_are_embedded() {
        // A missing asset would still compile to an empty slice, so check that
        // both fonts carry real data and the expected signatures.
        assert!(CJK_FONT.len() > 1024);
        assert!(EMOJI_FONT.len() > 1024);
        // OpenType with CFF outlines starts with 'OTTO'; TrueType with 0x00010000.
        assert_eq!(&CJK_FONT[..4], b"OTTO");
        assert_eq!(&EMOJI_FONT[..4], &[0x00, 0x01, 0x00, 0x00]);
    }
}
