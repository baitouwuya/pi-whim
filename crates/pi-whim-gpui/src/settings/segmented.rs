//! A row of mutually exclusive choices.
//!
//! Used wherever there are two or three options and all of them are worth
//! showing: language, the bash policy, how queued prompts are released. A select
//! would hide two of three behind a click for no gain.
//!
//! Square, because the app has no rounded corners outside circular indicators.

use gpui::{AnyElement, ElementId, IntoElement, ParentElement, SharedString, Styled, div};
use gpui_component::button::{Button, ButtonVariant, ButtonVariants};
use pi_whim_theme::Tokens;

use crate::theme::IntoHsla;

/// One choice.
pub struct Segment<T> {
    pub value: T,
    pub label: SharedString,
}

impl<T> Segment<T> {
    pub fn new(value: T, label: impl Into<SharedString>) -> Self {
        Self {
            value,
            label: label.into(),
        }
    }
}

/// Build a segmented row.
///
/// `on_pick` fires for any segment, including the one already active — a
/// re-pick is harmless everywhere this is used, and filtering it here would hide
/// the case where the caller does want to know.
pub fn segmented<T: Copy + PartialEq + 'static>(
    id: &'static str,
    current: T,
    segments: Vec<Segment<T>>,
    tokens: Tokens,
    on_pick: impl Fn(T, &mut gpui::Window, &mut gpui::App) + Clone + 'static,
) -> AnyElement {
    div()
        .flex()
        .items_center()
        .border_1()
        .border_color(tokens.line.hsla())
        .children(segments.into_iter().enumerate().map(|(index, segment)| {
            let active = segment.value == current;
            let value = segment.value;
            let on_pick = on_pick.clone();
            Button::new(ElementId::NamedInteger(id.into(), index as u64))
                .label(segment.label)
                .with_variant(if active {
                    ButtonVariant::Primary
                } else {
                    ButtonVariant::Ghost
                })
                // The border belongs to the group, so the buttons inside
                // it carry none of their own.
                .rounded_none()
                .on_click(move |_, window, cx| on_pick(value, window, cx))
        }))
        .into_any_element()
}

/// Whether a segment reads as the active one.
///
/// Split out so the distinction can be asserted: a segmented control where the
/// selection is not obvious is worse than a select, since it claims to show the
/// current value.
pub fn variant_for(active: bool) -> ButtonVariant {
    if active {
        ButtonVariant::Primary
    } else {
        ButtonVariant::Ghost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::QueueMode;

    #[test]
    fn the_active_segment_is_visually_distinct() {
        // The control claims to show the current value; if the selection does not
        // stand out it fails at the one thing it is for.
        assert_ne!(variant_for(true), variant_for(false));
    }

    #[test]
    fn a_segment_carries_its_value_and_its_label_separately() {
        // The label is translated; the value is what gets stored.
        let segment = Segment::new(QueueMode::All, "全部");

        assert_eq!(segment.value, QueueMode::All);
        assert_eq!(segment.label, SharedString::from("全部"));
    }
}
