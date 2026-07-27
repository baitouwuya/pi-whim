//! A compact disclosure control shared by thinking and tool sections.

use gpui::{AnyElement, Entity, Hsla, IntoElement, SharedString, Styled};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
};

use crate::{
    chat::{Conversation, ConversationEvent},
    icons,
};

/// Build a text-sized disclosure button without reserving normal button height.
pub(crate) fn disclosure_button(
    id: String,
    label: impl Into<SharedString>,
    expanded: bool,
    color: Hsla,
    event: ConversationEvent,
    owner: Option<Entity<Conversation>>,
) -> AnyElement {
    let label = label.into();
    let button = Button::new(id)
        .text()
        .xsmall()
        .icon(icons::disclosure(expanded))
        .label(label.clone())
        .tooltip(label)
        .text_color(color);
    let button = match owner {
        Some(owner) => button.on_click(move |_, _, cx| {
            owner.update(cx, |_, cx| cx.emit(event.clone()));
        }),
        None => button,
    };
    button.into_any_element()
}
