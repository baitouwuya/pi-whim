//! Straight-alpha RGBA color, independent of any UI framework.
//!
//! pi.dev expresses most of its palette as alpha steps over a handful of base
//! hues (`rgb(from var(--accent) r g b / 0.2)` and friends). Keeping colors in
//! straight (non-premultiplied) alpha lets [`Rgba::alpha`] reproduce those
//! steps exactly, instead of storing each derived value as its own constant.

/// A color with straight (non-premultiplied) alpha, each channel in `0.0..=1.0`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    /// Opaque color from a `0xRRGGBB` literal.
    pub const fn hex(rgb: u32) -> Self {
        Self {
            r: ((rgb >> 16) & 0xff) as f32 / 255.0,
            g: ((rgb >> 8) & 0xff) as f32 / 255.0,
            b: (rgb & 0xff) as f32 / 255.0,
            a: 1.0,
        }
    }

    /// Color from a `0xRRGGBBAA` literal, matching pi.dev's 8-digit CSS hex.
    pub const fn hexa(rgba: u32) -> Self {
        Self {
            r: ((rgba >> 24) & 0xff) as f32 / 255.0,
            g: ((rgba >> 16) & 0xff) as f32 / 255.0,
            b: ((rgba >> 8) & 0xff) as f32 / 255.0,
            a: (rgba & 0xff) as f32 / 255.0,
        }
    }

    /// The same hue at a new opacity — pi.dev's `rgb(from X r g b / a)`.
    pub const fn alpha(self, a: f32) -> Self {
        Self { a, ..self }
    }

    /// Composite `self` over an opaque `base`, returning an opaque color.
    ///
    /// Needed wherever a translucent fill would otherwise blend against an
    /// unknown backdrop, such as the window's clear color.
    pub fn over(self, base: Rgba) -> Rgba {
        let t = self.a.clamp(0.0, 1.0);
        Rgba {
            r: base.r * (1.0 - t) + self.r * t,
            g: base.g * (1.0 - t) + self.g * t,
            b: base.b * (1.0 - t) + self.b * t,
            a: 1.0,
        }
    }

    /// Linear interpolation toward `other`, ignoring alpha.
    pub fn mix(self, other: Rgba, amount: f32) -> Rgba {
        let t = amount.clamp(0.0, 1.0);
        Rgba {
            r: self.r * (1.0 - t) + other.r * t,
            g: self.g * (1.0 - t) + other.g * t,
            b: self.b * (1.0 - t) + other.b * t,
            a: self.a * (1.0 - t) + other.a * t,
        }
    }

    /// Pack back into `0xRRGGBBAA`, rounding each channel.
    pub fn to_hexa(self) -> u32 {
        let channel = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u32;
        (channel(self.r) << 24) | (channel(self.g) << 16) | (channel(self.b) << 8) | channel(self.a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_through_hexa() {
        assert_eq!(Rgba::hex(0x252f3d).to_hexa(), 0x252f3dff);
    }

    #[test]
    fn hexa_parses_pi_dev_eight_digit_literals() {
        // --text in the light theme is #252f3df5.
        let text = Rgba::hexa(0x252f3df5);
        assert_eq!(text.to_hexa(), 0x252f3df5);
        assert_eq!(Rgba::hex(0x252f3d).alpha(0xf5 as f32 / 255.0), text);
    }

    #[test]
    fn alpha_reproduces_pi_dev_accent_steps() {
        // --accent-fill-soft is accent @ 0.2; in the light theme that is
        // #4b607c33 once rounded to eight-digit hex.
        let fill_soft = crate::palette::TIDAL_BLUE.alpha(0.2);
        assert_eq!(fill_soft.to_hexa(), 0x4b607c33);
    }

    #[test]
    fn over_composites_against_an_opaque_base() {
        let half_white = Rgba::hex(0xffffff).alpha(0.5);
        let blended = half_white.over(Rgba::hex(0x000000));
        assert_eq!(blended.a, 1.0);
        assert_eq!(blended.to_hexa(), 0x808080ff);
    }

    #[test]
    fn over_is_a_no_op_for_opaque_colors() {
        let opaque = Rgba::hex(0x123456);
        assert_eq!(opaque.over(Rgba::hex(0xffffff)), opaque);
    }

    #[test]
    fn mix_interpolates_endpoints() {
        let a = Rgba::hex(0x000000);
        let b = Rgba::hex(0xffffff);
        assert_eq!(a.mix(b, 0.0), a);
        assert_eq!(a.mix(b, 1.0), b);
        assert_eq!(a.mix(b, 0.5).to_hexa(), 0x808080ff);
    }

    #[test]
    fn out_of_range_amounts_clamp() {
        let a = Rgba::hex(0x000000);
        let b = Rgba::hex(0xffffff);
        assert_eq!(a.mix(b, -1.0), a);
        assert_eq!(a.mix(b, 2.0), b);
    }
}
