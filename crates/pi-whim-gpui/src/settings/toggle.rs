//! A Pi-styled binary setting control.
//!
//! The application keeps square corners globally, but a switch track and thumb
//! are genuinely round shapes. gpui-component derives its switch radius from the
//! global theme, so its stock switch collapses into two hard-edged blocks here.

use gpui::{
    AnyElement, App, ElementId, InteractiveElement, IntoElement, ParentElement, Role, SharedString,
    StatefulInteractiveElement, Styled, Toggled, Window, div, prelude::FluentBuilder, px,
};
use pi_whim_theme::{Tokens, radius};

use crate::theme::IntoHsla;

const TRACK_WIDTH: f32 = 32.0;
const TRACK_HEIGHT: f32 = 18.0;
const THUMB_SIZE: f32 = 14.0;
const TRACK_INSET: f32 = 2.0;

/// Render a compact toggle button with an accessible pressed state.
pub fn toggle(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    checked: bool,
    tokens: Tokens,
    on_toggle: impl Fn(bool, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let track = if checked {
        tokens.accent
    } else {
        tokens.line_strong
    };
    let hover = if checked {
        tokens.accent.alpha(0.86)
    } else {
        tokens.muted.alpha(0.42)
    };

    div()
        .id(id)
        .role(Role::Switch)
        .aria_label(label)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .w(px(TRACK_WIDTH))
        .h(px(TRACK_HEIGHT))
        .flex_none()
        .rounded(px(radius::DOT))
        .bg(track.hsla())
        .cursor_pointer()
        .hover(move |this| this.bg(hover.hsla()))
        .child(
            div()
                .size_full()
                .flex()
                .items_center()
                .px(px(TRACK_INSET))
                .when(checked, |this| this.justify_end())
                .when(!checked, |this| this.justify_start())
                .child(
                    div()
                        .size(px(THUMB_SIZE))
                        .rounded(px(radius::DOT))
                        .bg(tokens.panel_base.hsla())
                        .border_1()
                        .border_color(tokens.line.hsla()),
                ),
        )
        .on_click(move |_, window, cx| on_toggle(!checked, window, cx))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    const _: () = {
        assert!(TRACK_WIDTH > TRACK_HEIGHT);
        assert!(THUMB_SIZE + TRACK_INSET * 2.0 <= TRACK_HEIGHT);
    };
}
