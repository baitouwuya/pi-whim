//! Bridge between [`pi_whim_theme::Rgba`] and gpui's color types.
//!
//! Both sides store straight-alpha `f32` channels in `0.0..=1.0`, so the
//! conversion is a field copy. gpui-component's [`ThemeColor`] slots are all
//! `Hsla`, and gpui already provides `From<Rgba> for Hsla`, so nothing here
//! implements a color-space conversion by hand.

use gpui::{Hsla, Rgba as GpuiRgba};
use pi_whim_theme::Rgba;

/// Convert a token color into gpui's RGBA.
pub fn to_gpui(color: Rgba) -> GpuiRgba {
    GpuiRgba {
        r: color.r,
        g: color.g,
        b: color.b,
        a: color.a,
    }
}

/// Convert a token color into the `Hsla` that gpui-component's theme slots and
/// most gpui styling methods expect.
pub fn to_hsla(color: Rgba) -> Hsla {
    to_gpui(color).into()
}

/// Extension trait so call sites read as `tokens.accent.hsla()` rather than
/// wrapping every color in a function call.
pub trait IntoHsla {
    fn hsla(self) -> Hsla;
}

impl IntoHsla for Rgba {
    fn hsla(self) -> Hsla {
        to_hsla(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_theme::Tokens;

    #[test]
    fn channels_survive_the_round_trip() {
        let token = Rgba::hexa(0x4b607c33);
        let gpui_color = to_gpui(token);
        assert_eq!(gpui_color.r, token.r);
        assert_eq!(gpui_color.g, token.g);
        assert_eq!(gpui_color.b, token.b);
        assert_eq!(gpui_color.a, token.a);
    }

    #[test]
    fn hsla_preserves_alpha() {
        // Alpha carries pi.dev's whole accent ladder, so losing it here would
        // flatten every derived surface to opaque.
        for alpha in [0.0, 0.0196, 0.2, 0.7216, 1.0] {
            let hsla = Tokens::light().accent.alpha(alpha).hsla();
            assert!((hsla.a - alpha).abs() < 1e-6, "alpha {alpha} was dropped");
        }
    }

    #[test]
    fn hsla_round_trips_back_to_the_same_rgb() {
        // gpui converts Hsla -> Rgba too; going both ways should land on the
        // original channels, which confirms we are not shifting hues.
        let token = Tokens::dark().accent;
        let back: GpuiRgba = token.hsla().into();
        assert!((back.r - token.r).abs() < 1e-3);
        assert!((back.g - token.g).abs() < 1e-3);
        assert!((back.b - token.b).abs() < 1e-3);
    }

    #[test]
    fn every_light_and_dark_slot_converts() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            for color in [
                tokens.bg_deep,
                tokens.bg_canvas,
                tokens.panel,
                tokens.text,
                tokens.accent,
                tokens.line,
            ] {
                let hsla = color.hsla();
                assert!(hsla.h.is_finite() && hsla.s.is_finite() && hsla.l.is_finite());
            }
        }
    }
}
