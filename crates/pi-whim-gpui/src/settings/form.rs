//! Layout primitives shared by the settings sections.
//!
//! Every row is label, optional help text, control. Keeping that in one place is
//! what stops the three sections from each inventing their own spacing — the
//! egui build had a `settings` module for exactly this reason, and this is its
//! counterpart.

use gpui::{AnyElement, IntoElement, ParentElement, SharedString, Styled, div, px};
use pi_whim_theme::{Tokens, font, text};

use crate::theme::IntoHsla;

/// How wide the settings column grows before it stops.
///
/// A form field stretched across a 1400px window is unreadable; the eye loses
/// the row it was on between the label and the control. A maximum rather than a
/// fixed width, so the narrowest window the app allows still fits it beside the
/// section list.
pub const CONTENT_WIDTH: f32 = 760.0;
/// Width of the label column, so controls line up down the page.
pub const LABEL_WIDTH: f32 = 220.0;
/// Width of the control column.
///
/// [`row`] applies this to every control slot, so a page of mixed controls — text
/// fields, dropdowns, and switches — shares one right edge. Call sites used
/// to set it individually, and the ones that forgot stretched to fill the row
/// instead, which is what made the page look ragged.
pub const CONTROL_WIDTH: f32 = 320.0;
/// Height of a single-line control.
pub const CONTROL_HEIGHT: f32 = 34.0;
/// Gap between controls sitting side by side.
pub const INLINE_GAP: f32 = 8.0;
/// Space from the page heading to its first group.
pub const PAGE_GROUP_GAP: f32 = 18.0;
/// Space between independent groups on a page.
pub const GROUP_GAP: f32 = 24.0;
/// Space from a group heading to its first row.
pub const GROUP_HEADER_GAP: f32 = 8.0;

/// The page's title and what it is for.
pub fn page_header(
    title: impl Into<SharedString>,
    help: Option<&str>,
    tokens: Tokens,
) -> AnyElement {
    let help = help.map(|help| help.to_owned());
    div()
        .flex()
        .flex_col()
        .gap(px(4.0))
        .child(
            div()
                .text_size(px(text::BODY_SIZE * 1.35))
                .text_color(tokens.text.hsla())
                .child(title.into()),
        )
        .when_some_help(help, tokens)
        .into_any_element()
}

/// A group of rows under a heading.
///
/// The heading is monospace and letter-spaced like the rest of the app's
/// structural labels, which is what distinguishes it from a field label.
pub fn section_header(
    title: impl Into<SharedString>,
    help: Option<&str>,
    tokens: Tokens,
) -> AnyElement {
    let help = help.map(|help| help.to_owned());
    div()
        .flex()
        .flex_col()
        .gap(px(3.0))
        .pb(px(GROUP_HEADER_GAP))
        .child(
            div()
                .font_family(font::MONO)
                .text_size(px(text::LABEL_SIZE))
                .text_color(tokens.muted.hsla())
                .child(title.into()),
        )
        .when_some_help(help, tokens)
        .into_any_element()
}

/// One semantic settings group: heading, optional help, then its rows.
pub fn group(
    title: impl Into<SharedString>,
    help: Option<&str>,
    tokens: Tokens,
    rows: Vec<AnyElement>,
) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .child(section_header(title, help, tokens))
        .children(rows)
        .into_any_element()
}

/// Stack independent groups below a page heading with stable visual rhythm.
pub fn group_stack(groups: Vec<AnyElement>) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(GROUP_GAP))
        .pt(px(PAGE_GROUP_GAP))
        .children(groups)
        .into_any_element()
}

/// One labelled row.
pub fn row(
    label: impl Into<SharedString>,
    help: Option<&str>,
    tokens: Tokens,
    control: impl IntoElement,
) -> AnyElement {
    let help = help.map(|help| help.to_owned());
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(16.0))
        .justify_between()
        .min_h(px(52.0))
        .py(px(8.0))
        .border_b_1()
        .border_color(tokens.line.hsla())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .max_w(px(LABEL_WIDTH))
                .flex()
                .flex_col()
                .gap(px(2.0))
                // Nudged down so the label sits on the control's centre line
                // rather than its top edge.
                .pt(px(7.0))
                .child(
                    div()
                        .text_size(px(text::DETAIL_SIZE))
                        .text_color(tokens.text.hsla())
                        .child(label.into()),
                )
                .when_some_help(help, tokens),
        )
        // Fixed rather than `flex_1`: the column is what the rows align on, and a
        // slot that grows to fill the row gives every control a different width.
        .child(
            div()
                .w(px(CONTROL_WIDTH))
                .flex_none()
                .flex()
                .min_h(px(CONTROL_HEIGHT))
                .items_center()
                .justify_end()
                .child(control),
        )
        .into_any_element()
}

/// A row with a control but no label, for buttons that act on the section above.
pub fn control_row(control: impl IntoElement) -> AnyElement {
    div()
        .w_full()
        .flex()
        .items_center()
        .justify_end()
        .py(px(7.0))
        .child(
            div()
                .w(px(CONTROL_WIDTH))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .child(control),
        )
        .into_any_element()
}

/// A short explanation under a label or heading.
pub fn help_text(text: impl Into<SharedString>, tokens: Tokens) -> AnyElement {
    div()
        .text_size(px(text::LABEL_SIZE))
        .text_color(tokens.muted.hsla())
        .child(text.into())
        .into_any_element()
}

/// A validation failure, in the error colour.
pub fn field_error(message: impl Into<SharedString>, tokens: Tokens) -> AnyElement {
    div()
        .pt(px(4.0))
        .text_size(px(text::LABEL_SIZE))
        .text_color(tokens.error.hsla())
        .child(message.into())
        .into_any_element()
}

/// Attaching optional help text without repeating the `when_some` at each site.
trait WithHelp: Sized {
    fn when_some_help(self, help: Option<String>, tokens: Tokens) -> Self;
}

impl WithHelp for gpui::Div {
    fn when_some_help(self, help: Option<String>, tokens: Tokens) -> Self {
        match help {
            Some(help) if !help.is_empty() => self.child(help_text(help, tokens)),
            _ => self,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These hold between constants, so they are checked at compile time.
    const _: () = {
        // Label and control both have to fit the column, or controls would be
        // pushed off the right edge.
        assert!(LABEL_WIDTH + CONTROL_WIDTH <= CONTENT_WIDTH);
        // A form field stretched the full window width is unreadable.
        assert!(CONTENT_WIDTH < crate::DEFAULT_WINDOW_SIZE.0);
        // Single-line controls have to clear their own text.
        assert!(CONTROL_HEIGHT > text::DETAIL_SIZE);
        assert!(GROUP_GAP > PAGE_GROUP_GAP);
        assert!(PAGE_GROUP_GAP > GROUP_HEADER_GAP);
    };

    #[test]
    fn the_settings_column_has_room_to_shrink_at_the_smallest_window() {
        // `max_w` is a ceiling. At the minimum window the page contracts to the
        // available body width, while the label and control still fit side by side.
        let minimum_body = crate::MIN_WINDOW_SIZE.0 - super::super::NAV_WIDTH;
        assert!(LABEL_WIDTH + CONTROL_WIDTH + 16.0 <= minimum_body);
    }
}
