//! A compact dropdown for finite setting choices.

use gpui::{AnyElement, Entity, IntoElement, SharedString, Styled, px};
use gpui_component::{
    Sizable,
    select::{Select, SelectItem, SelectState},
};

use super::form::{CONTROL_HEIGHT, CONTROL_WIDTH};

const MENU_MAX_HEIGHT: f32 = 280.0;

/// A translated label paired with the domain value it stores.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice<T: Clone + PartialEq + 'static> {
    pub label: SharedString,
    pub value: T,
}

impl<T: Clone + PartialEq + 'static> Choice<T> {
    pub fn new(value: T, label: impl Into<SharedString>) -> Self {
        Self {
            label: label.into(),
            value,
        }
    }
}

impl<T: Clone + PartialEq + 'static> SelectItem for Choice<T> {
    type Value = T;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

pub type ChoiceState<T> = SelectState<Vec<Choice<T>>>;

/// Render one full-width trigger with a menu that shares the control column.
pub fn dropdown<T>(state: &Entity<ChoiceState<T>>) -> AnyElement
where
    T: Clone + PartialEq + 'static,
{
    Select::new(state)
        .small()
        .w_full()
        .h(px(CONTROL_HEIGHT))
        .menu_width(px(CONTROL_WIDTH))
        .menu_max_h(px(MENU_MAX_HEIGHT))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::QueueMode;

    #[test]
    fn a_choice_keeps_its_label_separate_from_its_value() {
        let choice = Choice::new(QueueMode::All, "全部");

        assert_eq!(choice.title(), SharedString::from("全部"));
        assert_eq!(choice.value(), &QueueMode::All);
    }
}
