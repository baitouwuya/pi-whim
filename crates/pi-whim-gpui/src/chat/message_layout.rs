//! Shared geometry for transcript rows.

use gpui::{Div, IntoElement, ParentElement, Styled, div, px};
use pi_whim_theme::layout;

/// Place transcript content in the centred reading lane used by every row.
pub(crate) fn reading_lane(content: impl IntoElement) -> Div {
    div().w_full().flex().justify_center().px(px(16.0)).child(
        div()
            .w_full()
            .max_w(px(layout::CHAT_CONTENT_WIDTH))
            .child(content),
    )
}
