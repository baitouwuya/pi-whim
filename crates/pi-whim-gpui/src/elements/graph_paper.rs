//! pi.dev's graph-paper background.
//!
//! The stylesheet builds this out of seven stacked backgrounds on `body::before`:
//! a minor rule every `--grid-gap` (4px) in both axes, a major rule every fifth
//! step, and a cross layer masked by a radial gradient so it survives only inside
//! a `gap`-wide circle at each major intersection. What reaches the eye is a fine
//! paper tooth, a stronger 20px lattice, and a tick mark where the lattice
//! crosses.
//!
//! Three details are load-bearing:
//!
//! * The major layer is drawn *over* the minor one rather than instead of it, so
//!   a major rule carries both alphas. Substituting one for the other makes the
//!   lattice read too faint.
//! * The radial mask is offset by `gap * -2.5`, which lands the tick centres
//!   exactly on the major intersections rather than between them.
//! * All three colours sit between 0.9% and 7.5% alpha. This is texture, not
//!   decoration; if you can see it clearly, it is wrong.
//!
//! The egui build drew a single 28px layer in one hardcoded colour, recomputed
//! every frame (`pi-whim-ui/src/lib.rs`, `paint_graph_paper`). This is a `canvas`
//! element instead: it inserts no hitbox, so it cannot intercept the scrolling or
//! clicks of whatever sits above it.

use gpui::{App, Bounds, Hsla, IntoElement, ParentElement, Pixels, RenderOnce, Styled, Window};
use gpui::{canvas, div, fill, point, px, size};
use pi_whim_theme::{Rgba, Tokens, layout};

use crate::theme::IntoHsla;

/// Rule thickness in device pixels.
///
/// Every one of pi.dev's gradients stops at `1px`, which in CSS on a 2× display
/// is half a logical pixel. Drawing a full logical pixel instead doubles the ink:
/// with rules four logical pixels apart, a quarter of the surface turns to grid
/// and prose sitting on it stops being legible. So the thickness is resolved
/// against the window's scale factor rather than being taken literally.
const HAIRLINE_DEVICE_PX: f32 = 1.0;

/// How much of each layer's stated alpha to actually lay down.
///
/// pi.dev's figures are tuned for a browser at a 20px reading measure over a page
/// that is mostly whitespace. A chat window is denser — text nearly everywhere,
/// over the same 4px tooth — and at full strength the paper competes with the
/// prose. One factor across all three layers keeps their relative weights, which
/// is what makes the lattice read as a lattice.
const FADE: f32 = 0.45;

/// Rule thickness in logical pixels, for a display at `scale_factor`.
fn hairline(scale_factor: f32) -> f32 {
    if scale_factor > 0.0 {
        HAIRLINE_DEVICE_PX / scale_factor
    } else {
        HAIRLINE_DEVICE_PX
    }
}

/// A grid layer at the strength this window actually draws it.
fn faded(layer: Rgba) -> Hsla {
    layer.alpha(layer.a * FADE).hsla()
}

/// The graph-paper texture, sized to fill its parent.
#[derive(IntoElement)]
pub struct GraphPaper {
    tokens: Tokens,
}

impl GraphPaper {
    pub fn new(tokens: Tokens) -> Self {
        Self { tokens }
    }
}

impl RenderOnce for GraphPaper {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let tokens = self.tokens;
        // Absolute and unsized-by-content: the paper is behind the conversation,
        // and must not participate in laying it out.
        div().absolute().top_0().left_0().size_full().child(
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| paint(bounds, tokens, window),
            )
            .size_full(),
        )
    }
}

/// Offsets of the rules across `extent`, starting at the leading edge.
fn ticks(extent: f32, step: f32) -> impl Iterator<Item = f32> {
    let count = if step > 0.0 && extent > 0.0 {
        (extent / step).ceil() as usize
    } else {
        0
    };
    (0..count).map(move |index| index as f32 * step)
}

/// Paint the three layers into `bounds`.
///
/// The rules are laid out from the element's own origin rather than from the
/// scroll position, so the texture stays put while the conversation moves over
/// it — as a sheet of paper would.
fn paint(bounds: Bounds<Pixels>, tokens: Tokens, window: &mut Window) {
    let step = layout::GRID_STEP;
    let major_step = step * layout::GRID_MAJOR_EVERY as f32;
    let width = bounds.size.width.as_f32();
    let height = bounds.size.height.as_f32();
    let origin = bounds.origin;
    let thickness = hairline(window.scale_factor());

    let minor = faded(tokens.grid_minor);
    let major = faded(tokens.grid_major);
    let cross = faded(tokens.grid_cross);

    // Clip to our own bounds: the parent may be narrower than the rules imply.
    window.paint_layer(bounds, |window| {
        let mut rule_v = |x: f32, color: Hsla| {
            window.paint_quad(fill(
                Bounds {
                    origin: point(origin.x + px(x), origin.y),
                    size: size(px(thickness), px(height)),
                },
                color,
            ));
        };
        for x in ticks(width, step) {
            rule_v(x, minor);
        }
        for x in ticks(width, major_step) {
            rule_v(x, major);
        }

        let mut rule_h = |y: f32, color: Hsla| {
            window.paint_quad(fill(
                Bounds {
                    origin: point(origin.x, origin.y + px(y)),
                    size: size(px(width), px(thickness)),
                },
                color,
            ));
        };
        for y in ticks(height, step) {
            rule_h(y, minor);
        }
        for y in ticks(height, major_step) {
            rule_h(y, major);
        }

        // Ticks: two short arms centred on each major intersection, which is
        // what the radial mask leaves of pi.dev's cross layer.
        let arm = layout::GRID_CROSS_ARM;
        let half = arm / 2.0;
        for x in ticks(width, major_step) {
            for y in ticks(height, major_step) {
                window.paint_quad(fill(
                    Bounds {
                        origin: point(origin.x + px(x - half), origin.y + px(y)),
                        size: size(px(arm), px(thickness)),
                    },
                    cross,
                ));
                window.paint_quad(fill(
                    Bounds {
                        origin: point(origin.x + px(x), origin.y + px(y - half)),
                        size: size(px(thickness), px(arm)),
                    },
                    cross,
                ));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_start_at_the_leading_edge() {
        // A grid that started one step in would leave a visible margin.
        assert_eq!(ticks(100.0, 4.0).next(), Some(0.0));
    }

    #[test]
    fn rules_cover_the_whole_extent() {
        // Rounding up rather than down: a partial trailing cell should still be
        // ruled, or the texture stops short of the edge.
        let last = ticks(100.0, 30.0).last().expect("some rules");
        assert!(last + 30.0 >= 100.0, "last rule at {last} leaves a gap");
    }

    #[test]
    fn an_empty_area_is_not_ruled() {
        assert_eq!(ticks(0.0, 4.0).count(), 0);
        // A zero step would loop forever if it were not guarded.
        assert_eq!(ticks(100.0, 0.0).count(), 0);
    }

    #[test]
    fn every_fifth_rule_is_also_a_major_one() {
        // The two layers stack, so the major offsets have to be a subset of the
        // minor ones — otherwise the lattice sits half a step off the tooth.
        let step = layout::GRID_STEP;
        let major_step = step * layout::GRID_MAJOR_EVERY as f32;
        let minor: Vec<f32> = ticks(200.0, step).collect();

        for x in ticks(200.0, major_step) {
            assert!(
                minor.iter().any(|candidate| (candidate - x).abs() < 1e-3),
                "major rule at {x} has no minor rule under it"
            );
        }
    }

    #[test]
    fn a_rule_is_one_device_pixel_wide_whatever_the_display() {
        // On a 2x display a full logical pixel would be twice pi.dev's ink, and
        // with rules four logical pixels apart that is enough to make prose sitting
        // on the paper hard to read.
        assert_eq!(hairline(2.0), 0.5);
        assert_eq!(hairline(1.0), 1.0);
        assert!((hairline(3.0) - 1.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn an_unreported_scale_factor_falls_back_to_a_whole_pixel() {
        // Better a slightly heavy grid than a zero-width one that draws nothing.
        assert_eq!(hairline(0.0), 1.0);
        assert_eq!(hairline(-1.0), 1.0);
    }

    #[test]
    fn fading_keeps_the_layers_in_the_same_order() {
        // The lattice only reads as a lattice while minor < major < cross. A
        // per-layer tweak could invert that; one shared factor cannot.
        for tokens in [Tokens::light(), Tokens::dark()] {
            let minor = faded(tokens.grid_minor).a;
            let major = faded(tokens.grid_major).a;
            let cross = faded(tokens.grid_cross).a;
            assert!(minor < major, "minor {minor} is not under major {major}");
            assert!(major < cross, "major {major} is not under cross {cross}");
        }
    }

    #[test]
    fn fading_lightens_every_layer() {
        for tokens in [Tokens::light(), Tokens::dark()] {
            for layer in [tokens.grid_minor, tokens.grid_major, tokens.grid_cross] {
                let drawn = faded(layer).a;
                assert!(drawn < layer.a, "{drawn} is not lighter than {}", layer.a);
                assert!(
                    drawn > 0.0,
                    "a layer faded to nothing is not worth painting"
                );
            }
        }
    }

    #[test]
    fn ink_stays_a_small_share_of_the_surface() {
        // The ratio that actually governs legibility: how much of each cell the
        // rules cover. Past a fifth the texture starts competing with the text.
        let covered = hairline(2.0) / layout::GRID_STEP;
        assert!(covered < 0.2, "rules cover {covered} of each step");
    }

    #[test]
    fn ticks_are_centred_on_their_intersection() {
        // The arm reaches equally either side, which is what makes it read as a
        // cross rather than as a corner.
        let arm = layout::GRID_CROSS_ARM;
        let half = arm / 2.0;
        assert!((half * 2.0 - arm).abs() < 1e-6);
        assert!(half > 0.0);
    }

    #[test]
    fn the_texture_stays_faint() {
        // pi.dev's boldest layer is the light cross at #252f3d1d — 11.4%. Past
        // that this stops being paper and starts being a table.
        const CEILING: f32 = 0.12;
        for tokens in [Tokens::light(), Tokens::dark()] {
            for layer in [tokens.grid_minor, tokens.grid_major, tokens.grid_cross] {
                assert!(layer.a > 0.0, "an invisible layer is not worth painting");
                assert!(layer.a < CEILING, "alpha {} is too strong", layer.a);
            }
        }
    }
}
