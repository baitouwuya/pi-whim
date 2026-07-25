//! Semantic color slots for the two pi.dev themes.
//!
//! pi.dev ships dark as its `:root` default and layers light on top via
//! `[data-theme="light"]`. Pi-Whim inverts that preference — light is the
//! default here — but the values below stay faithful to the stylesheet so the
//! app reads as part of the same family.
//!
//! Views resolve colors through [`Tokens`], never through [`crate::palette`]
//! directly. Derived surfaces come from the alpha-step helpers rather than
//! stored constants, mirroring how pi.dev writes `rgb(from var(--accent) …)`.

use crate::{Rgba, ThemeMode, palette};

/// Resolved color slots for one theme mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tokens {
    pub mode: ThemeMode,

    // Surfaces, back to front.
    /// Deepest backdrop, behind the canvas.
    pub bg_deep: Rgba,
    /// The page canvas that conversation and settings sit on.
    pub bg_canvas: Rgba,
    /// Opaque panel base, for surfaces that must not show through.
    pub panel_base: Rgba,
    /// Opaque secondary panel base.
    pub panel_soft_base: Rgba,
    /// Panel fill as pi.dev renders it, with its own translucency.
    pub panel: Rgba,
    /// Secondary panel fill.
    pub panel_soft: Rgba,

    // Lines.
    pub line_base: Rgba,
    pub line_strong_base: Rgba,
    pub line: Rgba,
    pub line_strong: Rgba,

    // Text, most to least prominent.
    /// Opaque text base; also the tint source for muted overlays.
    pub text_base: Rgba,
    /// Primary text.
    pub text: Rgba,
    /// Body copy, a touch softer than `text`.
    pub copy: Rgba,
    pub muted_base: Rgba,
    /// Secondary text.
    pub muted: Rgba,
    /// Secondary text that still needs to carry weight.
    pub muted_strong: Rgba,

    /// The single accent hue. Light resolves to tidal blue, dark to the
    /// brighter accent blue; every accent surface and border derives from this.
    pub accent: Rgba,
    /// Warm counter-accent, used for destructive emphasis.
    pub accent_rust: Rgba,
    /// Thread blue, held separate from `accent` because scrollbars keep this
    /// hue in both themes.
    pub thread_blue: Rgba,

    // States.
    pub success: Rgba,
    pub warning: Rgba,
    pub error: Rgba,

    /// Graph-paper grid, finest to boldest.
    pub grid_minor: Rgba,
    pub grid_major: Rgba,
    pub grid_cross: Rgba,

    /// Drop shadow for raised media. `None` in dark, where pi.dev omits it.
    pub media_shadow: Option<Rgba>,
}

impl Tokens {
    /// Slots for `mode`.
    pub fn new(mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Light => Self::light(),
            ThemeMode::Dark => Self::dark(),
        }
    }

    /// pi.dev's `[data-theme="light"]` — warm paper under evening-blue ink.
    pub fn light() -> Self {
        Self {
            mode: ThemeMode::Light,

            bg_deep: palette::PARCHMENT,
            bg_canvas: palette::MOONSTONE,
            panel_base: palette::WARM_WHITE,
            panel_soft_base: palette::COOL_WHITE,
            panel: Rgba::hex(0xf4f2f0),
            panel_soft: Rgba::hex(0xeef1f3),

            line_base: palette::WARM_30,
            line_strong_base: palette::WARM_40,
            line: Rgba::hexa(0x8b847d59),
            line_strong: Rgba::hexa(0x5c575240),

            text_base: palette::EVENING_BLUE,
            text: Rgba::hexa(0x252f3df5),
            copy: Rgba::hexa(0x384251dc),
            muted_base: palette::DRIFTWOOD,
            muted: Rgba::hexa(0x5c5752c4),
            muted_strong: Rgba::hexa(0x394352d6),

            accent: palette::TIDAL_BLUE,
            accent_rust: palette::TERRACOTTA_LIGHT,
            thread_blue: palette::TIDAL_BLUE,

            // pi.dev leaves the light theme's states to the earth accents;
            // sage reads as success against paper, terracotta as error.
            success: palette::SAGE,
            warning: palette::SUNKISSED,
            error: palette::TERRACOTTA,

            grid_minor: Rgba::hexa(0x252f3d08),
            grid_major: Rgba::hexa(0x252f3d12),
            grid_cross: Rgba::hexa(0x252f3d1d),

            media_shadow: Some(Rgba::hexa(0x7862511a)),
        }
    }

    /// pi.dev's `:root` — deep slate under cool light text.
    pub fn dark() -> Self {
        Self {
            mode: ThemeMode::Dark,

            bg_deep: Rgba::hex(0x0d1116),
            bg_canvas: Rgba::hex(0x161d27),
            panel_base: Rgba::hex(0x212730),
            panel_soft_base: Rgba::hex(0x252f3d),
            panel: Rgba::hexa(0x212730e6),
            panel_soft: Rgba::hexa(0x252f3dd1),

            line_base: Rgba::hex(0x495059),
            line_strong_base: Rgba::hex(0x757d89),
            line: Rgba::hexa(0x49505980),
            line_strong: Rgba::hexa(0x757d8975),

            text_base: Rgba::hex(0xd5d8db),
            text: Rgba::hex(0xebe7e4),
            copy: Rgba::hexa(0xebe7e4bf),
            muted_base: Rgba::hex(0x9fa4ab),
            muted: Rgba::hexa(0x9fa4abad),
            muted_strong: Rgba::hexa(0xd5d8dbcc),

            accent: palette::ACCENT_BLUE,
            accent_rust: palette::RUST_DARK,
            thread_blue: palette::TIDAL_BLUE,

            success: palette::SUCCESS_DARK_STOP,
            warning: palette::WARNING_DARK_STOP,
            error: palette::ERROR_DARK_STOP,

            // pi.dev tints the dark grid with a cool blue at very low alpha.
            grid_minor: GRID_TINT_DARK.alpha(0.009_375),
            grid_major: GRID_TINT_DARK.alpha(0.026_25),
            grid_cross: GRID_TINT_DARK.alpha(0.075),

            media_shadow: None,
        }
    }

    // Accent surfaces. pi.dev derives all of these from `--accent` by alpha, so
    // they are functions here rather than stored fields.

    /// Barely-there accent wash.
    pub fn accent_surface_faint(&self) -> Rgba {
        self.accent.alpha(0.019_6)
    }

    /// Resting fill for accent-tinted cards.
    pub fn accent_surface_subtle(&self) -> Rgba {
        self.accent.alpha(0.054_9)
    }

    /// Hovered fill for accent-tinted cards; also inline code backgrounds.
    pub fn accent_surface_soft(&self) -> Rgba {
        self.accent.alpha(0.074_5)
    }

    /// Pressed or selected accent fill.
    pub fn accent_surface_strong(&self) -> Rgba {
        self.accent.alpha(0.078_4)
    }

    /// Solid-feeling accent fill, for pills and progress tracks.
    pub fn accent_fill_soft(&self) -> Rgba {
        self.accent.alpha(0.2)
    }

    pub fn accent_border_muted(&self) -> Rgba {
        self.accent.alpha(0.141_2)
    }

    pub fn accent_underline_muted(&self) -> Rgba {
        self.accent.alpha(0.219_6)
    }

    pub fn accent_outline_soft(&self) -> Rgba {
        self.accent.alpha(0.360_8)
    }

    pub fn accent_border_strong(&self) -> Rgba {
        self.accent.alpha(0.4)
    }

    pub fn accent_border_hover(&self) -> Rgba {
        self.accent.alpha(0.419_6)
    }

    pub fn accent_border_active(&self) -> Rgba {
        self.accent.alpha(0.502)
    }

    /// Focus ring, at pi.dev's `--form-control-focus-ring-color`.
    pub fn focus_ring(&self) -> Rgba {
        self.accent.alpha(0.721_6)
    }

    /// Text selection highlight.
    pub fn selection(&self) -> Rgba {
        self.accent.alpha(0.239_2)
    }

    // Neutral overlays. These tint with white in dark and with the text base in
    // light, and the alphas differ per mode — pi.dev tunes each separately.

    /// Resting fill for buttons and inputs.
    pub fn control_background(&self) -> Rgba {
        match self.mode {
            ThemeMode::Light => self.text_base.alpha(0.031_4),
            ThemeMode::Dark => palette::WHITE.alpha(0.015_7),
        }
    }

    /// Hovered fill for buttons and inputs.
    pub fn control_background_hover(&self) -> Rgba {
        match self.mode {
            ThemeMode::Light => self.text_base.alpha(0.062_7),
            ThemeMode::Dark => palette::WHITE.alpha(0.035_3),
        }
    }

    /// Generic surface lift, for nested cards.
    pub fn surface_tint(&self) -> Rgba {
        match self.mode {
            ThemeMode::Light => self.text_base.alpha(0.051),
            ThemeMode::Dark => palette::WHITE.alpha(0.019_6),
        }
    }

    /// Table header fill.
    pub fn table_heading_background(&self) -> Rgba {
        match self.mode {
            ThemeMode::Light => self.text_base.alpha(0.058_8),
            ThemeMode::Dark => palette::WHITE.alpha(0.043_1),
        }
    }

    /// Scrollbar thumb. Keeps the thread-blue hue in both themes.
    pub fn scrollbar_thumb(&self) -> Rgba {
        self.thread_blue.alpha(0.278_4)
    }

    pub fn scrollbar_thumb_hover(&self) -> Rgba {
        self.thread_blue.alpha(0.4)
    }

    /// Accent for invalid form state.
    pub fn form_invalid(&self) -> Rgba {
        match self.mode {
            ThemeMode::Light => palette::TERRACOTTA,
            ThemeMode::Dark => palette::WARNING_DARK_STOP,
        }
    }
}

/// Cool blue that pi.dev tints the dark-theme grid with, as
/// `hsl(218 60% 80%)` before alpha.
const GRID_TINT_DARK: Rgba = Rgba::hex(0xadc4e0);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_is_the_default_mode() {
        assert_eq!(ThemeMode::default(), ThemeMode::Light);
        assert_eq!(Tokens::new(ThemeMode::default()), Tokens::light());
    }

    #[test]
    fn accent_is_one_slot_with_a_value_per_mode() {
        // The egui build exposed these as two parallel constants, BLUE and
        // ACCENT_STRONG, which hid the fact that they are the same slot.
        assert_eq!(Tokens::light().accent, palette::TIDAL_BLUE);
        assert_eq!(Tokens::dark().accent, palette::ACCENT_BLUE);
    }

    #[test]
    fn light_slots_match_pi_dev() {
        let light = Tokens::light();
        assert_eq!(light.bg_canvas.to_hexa(), 0xebe7e4ff);
        assert_eq!(light.text.to_hexa(), 0x252f3df5);
        assert_eq!(light.line.to_hexa(), 0x8b847d59);
        assert_eq!(light.panel.to_hexa(), 0xf4f2f0ff);
    }

    #[test]
    fn dark_slots_match_pi_dev() {
        let dark = Tokens::dark();
        assert_eq!(dark.bg_deep.to_hexa(), 0x0d1116ff);
        assert_eq!(dark.bg_canvas.to_hexa(), 0x161d27ff);
        assert_eq!(dark.text.to_hexa(), 0xebe7e4ff);
        assert_eq!(dark.panel.to_hexa(), 0x212730e6);
    }

    #[test]
    fn accent_steps_derive_from_the_accent_slot() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            for derived in [
                tokens.accent_surface_faint(),
                tokens.accent_fill_soft(),
                tokens.focus_ring(),
                tokens.selection(),
            ] {
                assert_eq!(derived.r, tokens.accent.r);
                assert_eq!(derived.g, tokens.accent.g);
                assert_eq!(derived.b, tokens.accent.b);
            }
        }
    }

    #[test]
    fn accent_steps_ascend_in_opacity() {
        let tokens = Tokens::light();
        let steps = [
            tokens.accent_surface_faint().a,
            tokens.accent_surface_subtle().a,
            tokens.accent_surface_soft().a,
            tokens.accent_surface_strong().a,
            tokens.accent_border_muted().a,
            tokens.accent_fill_soft().a,
            tokens.accent_underline_muted().a,
            tokens.accent_outline_soft().a,
            tokens.accent_border_strong().a,
            tokens.accent_border_hover().a,
            tokens.accent_border_active().a,
            tokens.focus_ring().a,
        ];
        // border_muted (0.1412) sits below fill_soft (0.2), and every other
        // pair ascends, so the sequence must be sorted as written.
        for pair in steps.windows(2) {
            assert!(pair[0] < pair[1], "{:?} should ascend", pair);
        }
    }

    #[test]
    fn light_accent_fill_matches_pi_dev_hex() {
        assert_eq!(Tokens::light().accent_fill_soft().to_hexa(), 0x4b607c33);
        assert_eq!(Tokens::light().selection().to_hexa(), 0x4b607c3d);
    }

    #[test]
    fn neutral_overlays_tint_per_mode() {
        // Light tints with ink so overlays darken; dark tints with white so
        // they lift.
        let light = Tokens::light();
        assert_eq!(light.control_background().r, light.text_base.r);

        let dark = Tokens::dark();
        assert_eq!(dark.control_background().r, palette::WHITE.r);

        // Hover is always more opaque than rest.
        assert!(light.control_background_hover().a > light.control_background().a);
        assert!(dark.control_background_hover().a > dark.control_background().a);
    }

    #[test]
    fn grid_layers_ascend_in_weight() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            assert!(tokens.grid_minor.a < tokens.grid_major.a);
            assert!(tokens.grid_major.a < tokens.grid_cross.a);
        }
    }

    #[test]
    fn media_shadow_is_light_only() {
        assert!(Tokens::light().media_shadow.is_some());
        assert!(Tokens::dark().media_shadow.is_none());
    }

    #[test]
    fn scrollbar_keeps_thread_blue_in_both_modes() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            assert_eq!(tokens.scrollbar_thumb().r, palette::TIDAL_BLUE.r);
            assert!(tokens.scrollbar_thumb_hover().a > tokens.scrollbar_thumb().a);
        }
    }

    #[test]
    fn translucent_slots_composite_to_opaque_over_canvas() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            let composited = tokens.panel.over(tokens.bg_canvas);
            assert_eq!(composited.a, 1.0);
        }
    }
}
