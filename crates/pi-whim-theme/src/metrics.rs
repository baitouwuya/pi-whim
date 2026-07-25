//! Typography and control metrics from pi.dev.
//!
//! The egui build spelled sizes as inline literals at each call site, so there
//! was no scale to stay consistent with. These constants are that scale.
//!
//! pi.dev states several values as `calc(11rem / 18)` — a ratio against an
//! 18px root rather than a round pixel count. They are resolved here against
//! [`ROOT_FONT_SIZE`] so the proportions survive.

/// The root font size pi.dev's `calc(Nrem / 18)` expressions divide against.
pub const ROOT_FONT_SIZE: f32 = 18.0;

/// Resolve one of pi.dev's `calc(<numerator>rem / 18)` expressions to pixels.
const fn rem_ratio(numerator: f32) -> f32 {
    numerator
}

/// Body and detail copy.
pub mod text {
    use super::{ROOT_FONT_SIZE, rem_ratio};

    /// `--body-copy-font-size: 1rem`.
    pub const BODY_SIZE: f32 = ROOT_FONT_SIZE;
    /// `--body-copy-font-weight: 450` — between regular and medium, and a
    /// noticeable part of pi.dev's feel.
    pub const BODY_WEIGHT: u16 = 450;
    /// `--body-copy-line-height: 1.55`.
    pub const BODY_LINE_HEIGHT: f32 = 1.55;

    /// `--detail-copy-font-size: 0.95rem`.
    pub const DETAIL_SIZE: f32 = ROOT_FONT_SIZE * 0.95;
    /// `--detail-copy-line-height: 1.55`.
    pub const DETAIL_LINE_HEIGHT: f32 = 1.55;

    /// `--mono-detail-copy-font-size: 0.84rem`.
    pub const MONO_DETAIL_SIZE: f32 = ROOT_FONT_SIZE * 0.84;

    /// `--form-label-font-size: calc(11rem / 18)`.
    pub const LABEL_SIZE: f32 = rem_ratio(11.0);
    /// `--form-label-line-height: calc(16rem / 18)`.
    pub const LABEL_LINE_HEIGHT: f32 = rem_ratio(16.0);
    /// `--form-label-letter-spacing: 0.12em`. Wide tracking on a monospace
    /// label is what makes pi.dev's form headers read as small caps.
    pub const LABEL_LETTER_SPACING: f32 = 0.12;
}

/// Form controls.
pub mod control {
    use super::rem_ratio;

    /// `--form-control-block-size: 44px`, comfortably past the 44pt touch
    /// target minimum.
    pub const HEIGHT: f32 = 44.0;
    /// `--form-control-padding-block: calc(8rem / 18)`.
    pub const PADDING_Y: f32 = rem_ratio(8.0);
    /// `--form-control-padding-inline: calc(13rem / 18)`.
    pub const PADDING_X: f32 = rem_ratio(13.0);
    /// `--form-control-font-size: calc(15rem / 18)`.
    pub const FONT_SIZE: f32 = rem_ratio(15.0);
    /// `--form-control-line-height: 1`.
    pub const LINE_HEIGHT: f32 = 1.0;

    /// `--form-control-focus-ring-width: 2px`.
    pub const FOCUS_RING_WIDTH: f32 = 2.0;
    /// `--form-control-focus-ring-offset: -1px`, drawn just inside the border.
    pub const FOCUS_RING_OFFSET: f32 = -1.0;
}

/// Layout widths carried over from the egui build, where they were tuned
/// against pi.dev's reading measure.
pub mod layout {
    /// Maximum width of the conversation column.
    pub const CHAT_CONTENT_WIDTH: f32 = 820.0;
    /// Maximum width of a user message bubble, narrower so the two roles are
    /// distinguishable at a glance.
    pub const USER_MESSAGE_WIDTH: f32 = 620.0;
    /// Fixed sidebar width.
    pub const SIDEBAR_WIDTH: f32 = 260.0;
    /// Spacing of the graph-paper grid.
    pub const GRID_STEP: f32 = 28.0;
    /// Every fourth grid line is drawn at major weight.
    pub const GRID_MAJOR_EVERY: u32 = 4;
}

/// Font stacks. pi.dev's first choices — Plantin MT Pro, Commit Mono,
/// Departure Mono — are commercial and not bundled, so each stack starts at
/// the first freely available fallback pi.dev itself names.
pub mod font {
    /// `--serif`, minus the unavailable Plantin faces.
    pub const SERIF: &[&str] = &["Georgia", "serif"];

    /// `--mono`, minus Commit Mono. `ui-monospace` resolves to SF Mono on
    /// macOS, which is pi.dev's own next choice.
    pub const MONO: &[&str] = &[
        "SF Mono",
        "ui-monospace",
        "Menlo",
        "Monaco",
        "Consolas",
        "monospace",
    ];

    /// `--accent-mono`, used for form labels. Departure Mono is unavailable,
    /// so this collapses onto the same stack as [`MONO`].
    pub const ACCENT_MONO: &[&str] = MONO;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_copy_uses_pi_dev_weight() {
        // 450, not 400 — a regular-weight body would miss the mark.
        assert_eq!(text::BODY_WEIGHT, 450);
    }

    // These relationships hold between constants, so they are checked at
    // compile time; a regression is a build failure rather than a test failure.
    const _: () = {
        assert!(ROOT_FONT_SIZE == 18.0);
        assert!(text::BODY_SIZE == 18.0);
        assert!(text::LABEL_SIZE == 11.0);
        assert!(control::FONT_SIZE == 15.0);

        // The type scale descends from body copy down to form labels.
        assert!(text::BODY_SIZE > text::DETAIL_SIZE);
        assert!(text::DETAIL_SIZE > text::MONO_DETAIL_SIZE);
        assert!(text::MONO_DETAIL_SIZE > control::FONT_SIZE);
        assert!(control::FONT_SIZE > text::LABEL_SIZE);

        // Controls clear the 44pt touch target, and their text plus symmetric
        // padding fits inside that height.
        assert!(control::HEIGHT >= 44.0);
        assert!(control::FONT_SIZE + control::PADDING_Y * 2.0 <= control::HEIGHT);

        // The focus ring is drawn just inside the border.
        assert!(control::FOCUS_RING_OFFSET < 0.0);
        assert!(control::FOCUS_RING_WIDTH > 0.0);

        // User bubbles stay narrower than the column so the two roles differ.
        assert!(layout::USER_MESSAGE_WIDTH < layout::CHAT_CONTENT_WIDTH);
    };

    #[test]
    fn font_stacks_end_in_a_generic_family() {
        assert_eq!(font::SERIF.last(), Some(&"serif"));
        assert_eq!(font::MONO.last(), Some(&"monospace"));
        // No commercial faces we cannot ship.
        for stack in [font::SERIF, font::MONO, font::ACCENT_MONO] {
            for family in stack {
                assert!(
                    !matches!(*family, "Plantin MT Pro" | "Commit Mono" | "Departure Mono"),
                    "{family} is not bundled"
                );
            }
        }
    }
}
