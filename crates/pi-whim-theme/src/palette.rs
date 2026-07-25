//! Named colors sampled from pi.dev's stylesheet.
//!
//! These are theme-agnostic base values. The semantic slots in [`crate::tokens`]
//! reference them; views should never use a named color directly, so that
//! swapping a theme cannot leave a stale hue behind.

use crate::Rgba;

// Warm paper family, used for light-theme surfaces.
pub const PARCHMENT: Rgba = Rgba::hex(0xdacbc2);
pub const MOONSTONE: Rgba = Rgba::hex(0xebe7e4);
pub const WARM_WHITE: Rgba = Rgba::hex(0xf3f2f0);
pub const COOL_WHITE: Rgba = Rgba::hex(0xf0f2f3);
pub const WARM_30: Rgba = Rgba::hex(0x8b847d);
pub const WARM_40: Rgba = Rgba::hex(0x5c5650);
pub const DRIFTWOOD: Rgba = Rgba::hex(0x5c5752);

/// Primary ink for light surfaces, and the base for dark-theme text tints.
pub const EVENING_BLUE: Rgba = Rgba::hex(0x252f3d);

// Accent family. `TIDAL_BLUE` is the light-theme accent, `ACCENT_BLUE` the dark
// one; both fill the same semantic slot (see `tokens::Tokens::accent`).
pub const TIDAL_BLUE: Rgba = Rgba::hex(0x4b607c);
pub const ACCENT_BLUE: Rgba = Rgba::hex(0x6a9fcc);

// Earth accents, used for errors and destructive emphasis.
pub const TERRACOTTA: Rgba = Rgba::hex(0x844f3b);
pub const TERRACOTTA_LIGHT: Rgba = Rgba::hex(0xb86b52);
pub const RUST_DARK: Rgba = Rgba::hex(0x8f3222);

// State colors. pi.dev only defines the dark-theme stops; the light theme
// reuses the earth accents instead.
pub const SUNKISSED: Rgba = Rgba::hex(0xe1b06e);
pub const SAGE: Rgba = Rgba::hex(0xa3a473);
pub const SUCCESS_DARK_STOP: Rgba = Rgba::hex(0x5db87a);
pub const WARNING_DARK_STOP: Rgba = Rgba::hex(0xe8993a);
pub const ERROR_DARK_STOP: Rgba = Rgba::hex(0xe8704f);

/// Tint source for dark-theme neutral overlays.
pub const WHITE: Rgba = Rgba::hex(0xffffff);
