//! Design tokens for Pi-Whim, transcribed from pi.dev's stylesheet.
//!
//! This crate is deliberately framework-free: colors are plain floats and
//! metrics are plain constants, so the token set can be unit-tested against
//! pi.dev's published values without standing up a window. The gpui layer
//! converts [`Rgba`] into its own color type at the boundary.
//!
//! Two ideas carry most of the weight:
//!
//! - **One accent slot, two values.** pi.dev derives every accent surface,
//!   border, and ring from a single `--accent` by varying alpha. The egui build
//!   instead kept `BLUE` and `ACCENT_STRONG` side by side, which obscured that
//!   they are the same slot in different themes. Here [`Tokens::accent`] is one
//!   field and the derived steps are methods.
//! - **Light is the default.** pi.dev ships dark as `:root`; Pi-Whim prefers
//!   light, so [`ThemeMode::default`] is [`ThemeMode::Light`] while both themes
//!   stay faithful to the source values.

mod color;
mod metrics;
mod palette;
mod tokens;

pub use color::Rgba;
pub use metrics::{ROOT_FONT_SIZE, control, font, layout, radius, text};
pub use tokens::Tokens;

/// Which of the two pi.dev themes is active.
///
/// Defaults to [`Light`](ThemeMode::Light), matching gpui-component's own
/// default so the two agree without extra wiring at startup.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
}

impl ThemeMode {
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }

    /// Lowercase name, for persisting the preference.
    pub fn name(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// Parse a persisted [`name`](Self::name), falling back to the default for
    /// anything unrecognized.
    pub fn from_name(name: &str) -> Self {
        match name {
            "dark" => Self::Dark,
            "light" => Self::Light,
            _ => Self::default(),
        }
    }

    /// The other mode, for a toggle control.
    pub fn toggled(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::Light,
        }
    }
}

/// How the app decides which theme to show.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemePreference {
    /// Follow the OS appearance.
    #[default]
    System,
    /// Pin to one mode regardless of the OS.
    Fixed(ThemeMode),
}

impl ThemePreference {
    /// Resolve to a concrete mode, given what the OS currently reports.
    ///
    /// `system` is `None` when the platform appearance is unknown, in which
    /// case the default mode applies.
    pub fn resolve(self, system: Option<ThemeMode>) -> ThemeMode {
        match self {
            Self::System => system.unwrap_or_default(),
            Self::Fixed(mode) => mode,
        }
    }
}

/// Tokens plus the preference that produced them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub preference: ThemePreference,
    pub tokens: Tokens,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(ThemePreference::default(), None)
    }
}

impl Theme {
    /// Resolve `preference` against the OS appearance.
    pub fn new(preference: ThemePreference, system: Option<ThemeMode>) -> Self {
        Self {
            preference,
            tokens: Tokens::new(preference.resolve(system)),
        }
    }

    pub fn mode(&self) -> ThemeMode {
        self.tokens.mode
    }

    /// Re-resolve after the OS appearance changed. Returns whether the visible
    /// mode moved, so callers can skip redundant work.
    pub fn system_appearance_changed(&mut self, system: Option<ThemeMode>) -> bool {
        let next = Tokens::new(self.preference.resolve(system));
        let changed = next.mode != self.tokens.mode;
        self.tokens = next;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_is_the_default_mode() {
        assert_eq!(ThemeMode::default(), ThemeMode::Light);
        assert!(!ThemeMode::default().is_dark());
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            assert_eq!(ThemeMode::from_name(mode.name()), mode);
        }
    }

    #[test]
    fn unknown_mode_names_fall_back_to_the_default() {
        assert_eq!(ThemeMode::from_name("sepia"), ThemeMode::default());
        assert_eq!(ThemeMode::from_name(""), ThemeMode::default());
    }

    #[test]
    fn toggling_twice_returns_to_the_start() {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            assert_ne!(mode.toggled(), mode);
            assert_eq!(mode.toggled().toggled(), mode);
        }
    }

    #[test]
    fn system_preference_follows_the_os() {
        let preference = ThemePreference::System;
        assert_eq!(preference.resolve(Some(ThemeMode::Dark)), ThemeMode::Dark);
        assert_eq!(preference.resolve(Some(ThemeMode::Light)), ThemeMode::Light);
    }

    #[test]
    fn unknown_os_appearance_uses_the_default() {
        assert_eq!(ThemePreference::System.resolve(None), ThemeMode::Light);
    }

    #[test]
    fn fixed_preference_ignores_the_os() {
        let pinned = ThemePreference::Fixed(ThemeMode::Light);
        assert_eq!(pinned.resolve(Some(ThemeMode::Dark)), ThemeMode::Light);
    }

    #[test]
    fn default_theme_resolves_to_light_tokens() {
        let theme = Theme::default();
        assert_eq!(theme.mode(), ThemeMode::Light);
        assert_eq!(theme.tokens, Tokens::light());
    }

    #[test]
    fn appearance_changes_are_reported_only_when_visible() {
        let mut theme = Theme::new(ThemePreference::System, Some(ThemeMode::Light));
        assert!(theme.system_appearance_changed(Some(ThemeMode::Dark)));
        assert_eq!(theme.mode(), ThemeMode::Dark);
        // Same appearance again is not a change.
        assert!(!theme.system_appearance_changed(Some(ThemeMode::Dark)));

        // A pinned preference never moves, so nothing is reported.
        let mut pinned = Theme::new(ThemePreference::Fixed(ThemeMode::Light), None);
        assert!(!pinned.system_appearance_changed(Some(ThemeMode::Dark)));
        assert_eq!(pinned.mode(), ThemeMode::Light);
    }
}
