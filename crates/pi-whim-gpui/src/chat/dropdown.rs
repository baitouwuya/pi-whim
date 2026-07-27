//! Shared runtime-dropdown chrome and a compact picker for short choices.

use gpui::{
    Anchor, AnyElement, Context, ElementId, Entity, EventEmitter, InteractiveElement, IntoElement,
    KeyDownEvent, MouseButton, ParentElement, Render, SharedString, StatefulInteractiveElement,
    Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Icon, IconName, Sizable,
    button::{Button, ButtonRounded, ButtonVariants},
    popover::{Popover, PopoverState},
};
use pi_whim_theme::{Tokens, radius, text};

use crate::theme::IntoHsla;

pub(super) const DROPDOWN_ROW_HEIGHT: f32 = 28.0;

/// The one trigger used by permission, model, and thinking controls.
///
/// Ghost buttons are visually quiet at rest and use the component theme's
/// neutral secondary fill on hover and while the menu is open.
pub(super) fn dropdown_trigger(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id)
        .xsmall()
        .compact()
        .ghost()
        .rounded(ButtonRounded::None)
        .dropdown_caret(true)
        .label(label)
}

/// Shared square, opaque popup surface.
pub(super) fn dropdown_panel(width: f32, tokens: Tokens) -> gpui::Div {
    div()
        .w(px(width))
        .max_w_full()
        .overflow_hidden()
        .bg(tokens.panel_base.hsla())
        .border_1()
        .border_color(tokens.line.hsla())
        .rounded(px(radius::NONE))
        .shadow_lg()
}

#[derive(Clone, Debug, PartialEq)]
pub(super) struct Choice<T> {
    pub label: SharedString,
    pub value: T,
}

impl<T> Choice<T> {
    pub fn new(label: impl Into<SharedString>, value: T) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ChoicePickerEvent<T> {
    Confirm(T),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyAction {
    Move(isize),
    Confirm,
    Dismiss,
    Ignore,
}

fn key_action(event: &KeyDownEvent) -> KeyAction {
    let keystroke = &event.keystroke;
    let modifiers = &keystroke.modifiers;
    if modifiers.shift || modifiers.control || modifiers.alt || modifiers.platform {
        return KeyAction::Ignore;
    }
    match keystroke.key.as_str() {
        "up" => KeyAction::Move(-1),
        "down" => KeyAction::Move(1),
        "enter" | "tab" => KeyAction::Confirm,
        "escape" => KeyAction::Dismiss,
        _ => KeyAction::Ignore,
    }
}

fn step(selection: usize, count: usize, delta: isize) -> usize {
    if count == 0 {
        return 0;
    }
    let count = count as isize;
    (((selection as isize + delta) % count + count) % count) as usize
}

/// A no-search picker for short closed lists such as permission and thinking.
pub(super) struct ChoicePicker<T>
where
    T: Clone + PartialEq + 'static,
{
    id: SharedString,
    menu_width: f32,
    prefix: SharedString,
    placeholder: SharedString,
    items: Vec<Choice<T>>,
    selected: Option<T>,
    reported_selected: Option<T>,
    active: usize,
    open: bool,
    synced: bool,
    tokens: Tokens,
}

impl<T> EventEmitter<ChoicePickerEvent<T>> for ChoicePicker<T> where T: Clone + PartialEq + 'static {}

impl<T> ChoicePicker<T>
where
    T: Clone + PartialEq + 'static,
{
    pub fn new(id: impl Into<SharedString>, menu_width: f32, tokens: Tokens) -> Self {
        Self {
            id: id.into(),
            menu_width,
            prefix: SharedString::default(),
            placeholder: SharedString::default(),
            items: Vec::new(),
            selected: None,
            reported_selected: None,
            active: 0,
            open: false,
            synced: false,
            tokens,
        }
    }

    pub fn sync(
        &mut self,
        items: Vec<Choice<T>>,
        selected: Option<T>,
        prefix: impl Into<SharedString>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let prefix = prefix.into();
        let placeholder = placeholder.into();
        let mut changed = false;

        if self.items != items {
            self.items = items;
            self.active = self.active.min(self.items.len().saturating_sub(1));
            changed = true;
        }
        if !self.synced || self.reported_selected != selected {
            self.reported_selected = selected.clone();
            self.selected = selected;
            if !self.open {
                self.activate_selected();
            }
            changed = true;
        }
        if self.prefix != prefix {
            self.prefix = prefix;
            changed = true;
        }
        if self.placeholder != placeholder {
            self.placeholder = placeholder;
            changed = true;
        }

        self.synced = true;
        if changed {
            cx.notify();
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        if self.tokens != tokens {
            self.tokens = tokens;
            cx.notify();
        }
    }

    fn activate_selected(&mut self) {
        self.active = self
            .selected
            .as_ref()
            .and_then(|selected| {
                self.items
                    .iter()
                    .position(|choice| choice.value == *selected)
            })
            .unwrap_or(0);
    }

    fn popover_changed(&mut self, open: bool, cx: &mut Context<Self>) {
        self.open = open;
        if open {
            self.activate_selected();
        }
        cx.notify();
    }

    fn confirm(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(choice) = self.items.get(index) else {
            return;
        };
        self.selected = Some(choice.value.clone());
        cx.emit(ChoicePickerEvent::Confirm(choice.value.clone()));
        cx.notify();
    }

    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> (bool, bool) {
        match key_action(event) {
            KeyAction::Ignore => (false, false),
            KeyAction::Dismiss => (true, true),
            KeyAction::Move(delta) => {
                self.active = step(self.active, self.items.len(), delta);
                cx.notify();
                (true, false)
            }
            KeyAction::Confirm => {
                self.confirm(self.active, cx);
                (true, !self.items.is_empty())
            }
        }
    }

    fn render_row(
        &self,
        index: usize,
        popover: Entity<PopoverState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let choice = &self.items[index];
        let selected = self
            .selected
            .as_ref()
            .is_some_and(|selected| choice.value == *selected);
        let active = index == self.active;
        let tokens = self.tokens;
        let owner = cx.entity();

        div()
            .id(("choice-row", index))
            .h(px(DROPDOWN_ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(9.0))
            .when(active, |row| row.bg(tokens.selection().hsla()))
            .hover(|row| row.bg(tokens.control_background_hover().hsla()))
            .on_hover(cx.listener(move |picker, hovered, _, cx| {
                if *hovered && picker.active != index {
                    picker.active = index;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                owner.update(cx, |picker, cx| picker.confirm(index, cx));
                popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                cx.stop_propagation();
            })
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(text::DETAIL_SIZE))
                    .text_color(tokens.text.hsla())
                    .child(choice.label.clone()),
            )
            .child(div().w(px(18.0)).flex_none().when(selected, |slot| {
                slot.child(
                    Icon::new(IconName::Check)
                        .size(px(13.0))
                        .text_color(tokens.text.hsla()),
                )
            }))
            .into_any_element()
    }

    fn render_menu(&mut self, popover: Entity<PopoverState>, cx: &mut Context<Self>) -> AnyElement {
        let owner = cx.entity();
        let key_owner = owner.clone();
        let key_popover = popover.clone();

        dropdown_panel(self.menu_width, self.tokens)
            .capture_key_down(move |event, window, cx| {
                let (consumed, close) =
                    key_owner.update(cx, |picker, cx| picker.handle_key(event, cx));
                if close {
                    key_popover.update(cx, |popover, cx| popover.dismiss(window, cx));
                }
                if consumed {
                    cx.stop_propagation();
                }
            })
            .children(
                (0..self.items.len()).map(|index| self.render_row(index, popover.clone(), cx)),
            )
            .into_any_element()
    }
}

impl<T> Render for ChoicePicker<T>
where
    T: Clone + PartialEq + 'static,
{
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let value = self
            .selected
            .as_ref()
            .and_then(|selected| {
                self.items
                    .iter()
                    .find(|choice| choice.value == *selected)
                    .map(|choice| choice.label.clone())
            })
            .unwrap_or_else(|| self.placeholder.clone());
        let label = format!("{}{}", self.prefix, value);
        let popover_id = format!("{}-popover", self.id);
        let trigger_id = format!("{}-trigger", self.id);
        let owner = cx.entity();
        let content_owner = owner.clone();

        Popover::new(popover_id)
            .anchor(Anchor::BottomRight)
            .appearance(false)
            .on_open_change(move |open, _, cx| {
                owner.update(cx, |picker, cx| picker.popover_changed(*open, cx));
            })
            .trigger(dropdown_trigger(trigger_id, label))
            .content(move |_, _, cx| {
                let popover = cx.entity();
                content_owner.update(cx, |picker, cx| picker.render_menu(popover, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Keystroke;

    #[test]
    fn short_picker_navigation_wraps() {
        assert_eq!(step(0, 3, -1), 2);
        assert_eq!(step(2, 3, 1), 0);
    }

    #[test]
    fn modified_keys_are_left_to_the_focused_control() {
        let event = KeyDownEvent {
            keystroke: Keystroke::parse("shift-down").expect("a valid key"),
            is_held: false,
            prefer_character_input: false,
        };
        assert_eq!(key_action(&event), KeyAction::Ignore);
    }
}
