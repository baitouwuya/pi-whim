//! A compact model picker for the prompt controls.
//!
//! The component-library `Select` is a good fit for settings forms, but its
//! two-line virtual rows make the prompt's model menu much larger than the
//! dense command menu it should resemble. This picker keeps one stable-height
//! row per model, owns its search and scroll state, and only asks the controls
//! bar to forward a confirmed model to the shell.

use gpui::{
    Anchor, AnyElement, AppContext, Context, Entity, EventEmitter, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, Keystroke, MouseButton, ParentElement, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Icon, IconName, Sizable,
    input::{Input, InputEvent, InputState},
    popover::{Popover, PopoverState},
};
use pi_whim_core::{Language, ModelOption, strings::text as translate};
use pi_whim_theme::{Tokens, text};

use crate::{
    chat::dropdown::{DROPDOWN_ROW_HEIGHT, dropdown_panel, dropdown_trigger},
    elements::isolated_vertical_scroll_area,
    icons,
    theme::IntoHsla,
};

/// Fixed popup geometry: enough room for a model name without becoming a panel.
const MENU_WIDTH: f32 = 280.0;
const LIST_MAX_HEIGHT: f32 = 224.0;
const SEARCH_HEIGHT: f32 = 30.0;

type ModelKey = (String, String);

#[derive(Clone, Debug, PartialEq)]
pub(super) enum ModelPickerEvent {
    Confirm(ModelOption),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KeyAction {
    Move(isize),
    Confirm,
    Shortcut(usize),
    Dismiss,
    Ignore,
}

/// Read navigation keys before the focused search field sees them.
///
/// Bare digits select the first nine visible rows only before a search starts.
/// Once there is a query, digits remain ordinary search input so model ids such
/// as `gpt-5` can still be entered.
fn key_action(keystroke: &Keystroke, query_is_empty: bool) -> KeyAction {
    let modifiers = &keystroke.modifiers;
    if modifiers.shift || modifiers.control || modifiers.alt || modifiers.platform {
        return KeyAction::Ignore;
    }

    match keystroke.key.as_str() {
        "up" => KeyAction::Move(-1),
        "down" => KeyAction::Move(1),
        "enter" | "tab" => KeyAction::Confirm,
        "escape" => KeyAction::Dismiss,
        key if query_is_empty && key.len() == 1 => key
            .as_bytes()
            .first()
            .filter(|digit| (b'1'..=b'9').contains(digit))
            .map(|digit| KeyAction::Shortcut((digit - b'1') as usize))
            .unwrap_or(KeyAction::Ignore),
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

fn model_key(model: &ModelOption) -> ModelKey {
    (model.provider.clone(), model.id.clone())
}

fn model_matches(model: &ModelOption, normalized_query: &str) -> bool {
    normalized_query.is_empty()
        || model.name.to_lowercase().contains(normalized_query)
        || model.id.to_lowercase().contains(normalized_query)
        || model
            .provider_name
            .to_lowercase()
            .contains(normalized_query)
}

fn name_is_duplicated(models: &[ModelOption], name: &str) -> bool {
    models
        .iter()
        .filter(|model| model.name.eq_ignore_ascii_case(name))
        .take(2)
        .count()
        > 1
}

fn row_label(models: &[ModelOption], index: usize) -> SharedString {
    let Some(model) = models.get(index) else {
        return SharedString::default();
    };
    if name_is_duplicated(models, &model.name) {
        format!("{}  ·  {}", model.name, model.provider_name).into()
    } else {
        model.name.clone().into()
    }
}

/// Search state, visible rows, and popover interaction for the model control.
pub(super) struct ModelPicker {
    models: Vec<ModelOption>,
    visible: Vec<usize>,
    selected: Option<ModelKey>,
    /// Last selection received from the backend.
    ///
    /// Confirming a row updates `selected` immediately. Repeated transcript
    /// snapshots still contain the previous model for a moment, so they are
    /// ignored until this reported value actually changes.
    reported_selected: Option<ModelKey>,
    search: Entity<InputState>,
    query: String,
    active: usize,
    scroll: ScrollHandle,
    open: bool,
    synced: bool,
    language: Language,
    tokens: Tokens,
}

impl EventEmitter<ModelPickerEvent> for ModelPicker {}

impl ModelPicker {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let language = Language::default();
        let search = cx.new(|cx| {
            InputState::new(window, cx).placeholder(translate("search-models", language))
        });
        cx.subscribe_in(&search, window, |picker, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                picker.set_query(input.read(cx).value().to_string(), cx);
            }
        })
        .detach();

        Self {
            models: Vec::new(),
            visible: Vec::new(),
            selected: None,
            reported_selected: None,
            search,
            query: String::new(),
            active: 0,
            scroll: ScrollHandle::new(),
            open: false,
            synced: false,
            language,
            tokens,
        }
    }

    pub fn sync(
        &mut self,
        models: &[ModelOption],
        selected: Option<ModelKey>,
        language: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;

        if self.models != models {
            self.models = models.to_vec();
            self.rebuild_visible();
            if self
                .selected
                .as_ref()
                .is_some_and(|key| !self.models.iter().any(|model| model_key(model) == *key))
            {
                self.selected = selected.clone();
            }
            self.active = self.active.min(self.visible.len().saturating_sub(1));
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

        if self.language != language {
            self.language = language;
            self.search.update(cx, |search, cx| {
                search.set_placeholder(translate("search-models", language), window, cx);
            });
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

    fn set_query(&mut self, query: String, cx: &mut Context<Self>) {
        if self.query == query {
            return;
        }
        self.query = query;
        self.rebuild_visible();
        self.active = 0;
        self.scroll.scroll_to_top_of_item(0);
        cx.notify();
    }

    fn rebuild_visible(&mut self) {
        let query = self.query.trim().to_lowercase();
        self.visible = self
            .models
            .iter()
            .enumerate()
            .filter_map(|(index, model)| model_matches(model, &query).then_some(index))
            .collect();
    }

    fn activate_selected(&mut self) {
        self.active = self
            .selected
            .as_ref()
            .and_then(|selected| {
                self.visible.iter().position(|index| {
                    self.models
                        .get(*index)
                        .is_some_and(|model| model_key(model) == *selected)
                })
            })
            .unwrap_or(0);
    }

    fn popover_changed(&mut self, open: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.open = open;
        if open {
            if !self.query.is_empty() || !self.search.read(cx).value().is_empty() {
                self.query.clear();
                self.search
                    .update(cx, |search, cx| search.set_value("", window, cx));
                self.rebuild_visible();
            }
            self.activate_selected();
            self.scroll.scroll_to_item(self.active);
        }
        cx.notify();
    }

    fn confirm(&mut self, visible_index: usize, cx: &mut Context<Self>) {
        let Some(model_index) = self.visible.get(visible_index).copied() else {
            return;
        };
        let Some(model) = self.models.get(model_index).cloned() else {
            return;
        };
        self.selected = Some(model_key(&model));
        cx.emit(ModelPickerEvent::Confirm(model));
        cx.notify();
    }

    /// Returns whether the key was consumed and whether the popover should close.
    fn handle_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> (bool, bool) {
        match key_action(&event.keystroke, self.query.trim().is_empty()) {
            KeyAction::Ignore => (false, false),
            KeyAction::Dismiss => (true, true),
            KeyAction::Move(delta) => {
                self.active = step(self.active, self.visible.len(), delta);
                self.scroll.scroll_to_item(self.active);
                cx.notify();
                (true, false)
            }
            KeyAction::Confirm => {
                self.confirm(self.active, cx);
                (true, !self.visible.is_empty())
            }
            KeyAction::Shortcut(index) => {
                if index >= self.visible.len() {
                    return (false, false);
                }
                self.confirm(index, cx);
                (true, true)
            }
        }
    }

    fn render_row(
        &self,
        visible_index: usize,
        popover: Entity<PopoverState>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model_index = self.visible[visible_index];
        let model = &self.models[model_index];
        let selected = self
            .selected
            .as_ref()
            .is_some_and(|selected| model_key(model) == *selected);
        let active = visible_index == self.active;
        let tokens = self.tokens;
        let owner = cx.entity();

        div()
            .id(("model-row", visible_index))
            .h(px(DROPDOWN_ROW_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(9.0))
            .when(active, |row| row.bg(tokens.selection().hsla()))
            .hover(|row| row.bg(tokens.control_background_hover().hsla()))
            .on_hover(cx.listener(move |picker, hovered, _, cx| {
                if *hovered && picker.active != visible_index {
                    picker.active = visible_index;
                    cx.notify();
                }
            }))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                owner.update(cx, |picker, cx| picker.confirm(visible_index, cx));
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
                    .child(row_label(&self.models, model_index)),
            )
            .child(
                div()
                    .w(px(18.0))
                    .flex_none()
                    .flex()
                    .justify_center()
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .when(selected, |slot| {
                        slot.child(
                            Icon::new(IconName::Check)
                                .size(px(13.0))
                                .text_color(tokens.text.hsla()),
                        )
                    })
                    .when(!selected && visible_index < 9, |slot| {
                        slot.child(SharedString::from((visible_index + 1).to_string()))
                    }),
            )
            .into_any_element()
    }

    fn render_menu(&mut self, popover: Entity<PopoverState>, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.tokens;
        let owner = cx.entity();
        let key_owner = owner.clone();
        let key_popover = popover.clone();

        dropdown_panel(MENU_WIDTH, tokens)
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
            .child(
                div()
                    .h(px(SEARCH_HEIGHT))
                    .w_full()
                    .flex()
                    .items_center()
                    .border_b_1()
                    .border_color(tokens.line.hsla())
                    .child(
                        Input::new(&self.search)
                            .xsmall()
                            .w_full()
                            .appearance(false)
                            .bordered(false)
                            .focus_bordered(false)
                            .prefix(
                                Icon::new(icons::search())
                                    .size(px(13.0))
                                    .text_color(tokens.muted.hsla()),
                            ),
                    ),
            )
            .child(
                isolated_vertical_scroll_area("model-picker-rows", &self.scroll)
                    .max_h(px(LIST_MAX_HEIGHT))
                    .when(self.visible.is_empty(), |rows| {
                        rows.child(
                            div()
                                .h(px(DROPDOWN_ROW_HEIGHT + 4.0))
                                .flex()
                                .items_center()
                                .px(px(9.0))
                                .text_size(px(text::LABEL_SIZE))
                                .text_color(tokens.muted.hsla())
                                .child(translate("no-models-available", self.language)),
                        )
                    })
                    .children(
                        (0..self.visible.len())
                            .map(|index| self.render_row(index, popover.clone(), cx)),
                    ),
            )
            .into_any_element()
    }
}

impl Render for ModelPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self
            .selected
            .as_ref()
            .and_then(|selected| {
                self.models
                    .iter()
                    .find(|model| model_key(model) == *selected)
                    .map(|model| model.name.clone())
            })
            .unwrap_or_else(|| translate("model", self.language).to_owned());
        let focus = self.search.focus_handle(cx);
        let owner = cx.entity();
        let content_owner = owner.clone();

        Popover::new("model-picker")
            // BottomRight anchors the menu above the compact trigger and keeps
            // their right edges aligned, so the menu never hides the value that
            // opened it.
            .anchor(Anchor::BottomRight)
            .appearance(false)
            .track_focus(&focus)
            .on_open_change(move |open, window, cx| {
                owner.update(cx, |picker, cx| picker.popover_changed(*open, window, cx));
            })
            .trigger(dropdown_trigger("model-picker-trigger", label))
            .content(move |_, _window, cx| {
                let popover = cx.entity();
                content_owner.update(cx, |picker, cx| picker.render_menu(popover, cx))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(provider: &str, provider_name: &str, id: &str, name: &str) -> ModelOption {
        ModelOption {
            provider: provider.into(),
            provider_name: provider_name.into(),
            id: id.into(),
            name: name.into(),
        }
    }

    #[test]
    fn search_matches_name_id_and_provider() {
        let candidate = model("anthropic", "Anthropic", "claude-sonnet-5", "Sonnet 5");

        assert!(model_matches(&candidate, "sonnet"));
        assert!(model_matches(&candidate, "claude-sonnet-5"));
        assert!(model_matches(&candidate, "anthropic"));
        assert!(!model_matches(&candidate, "gpt"));
    }

    #[test]
    fn provider_is_only_added_to_duplicate_names() {
        let unique = model("openai", "OpenAI", "gpt-5", "GPT-5");
        let first = model("anthropic", "Anthropic", "opus", "Opus");
        let second = model("bedrock", "Bedrock", "opus", "Opus");
        let models = vec![unique, first, second];

        assert_eq!(row_label(&models, 0), SharedString::from("GPT-5"));
        assert_eq!(
            row_label(&models, 1),
            SharedString::from("Opus  ·  Anthropic")
        );
        assert_eq!(
            row_label(&models, 2),
            SharedString::from("Opus  ·  Bedrock")
        );
    }

    #[test]
    fn digits_are_shortcuts_only_before_searching() {
        let three = Keystroke::parse("3").expect("a digit keystroke");
        assert_eq!(key_action(&three, true), KeyAction::Shortcut(2));
        assert_eq!(key_action(&three, false), KeyAction::Ignore);
    }

    #[test]
    fn selection_steps_wrap() {
        assert_eq!(step(0, 4, -1), 3);
        assert_eq!(step(3, 4, 1), 0);
        assert_eq!(step(0, 0, 1), 0);
    }

    #[gpui::test]
    async fn unchanged_backend_snapshots_preserve_a_local_selection(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::init(pi_whim_theme::ThemePreference::default(), cx).unwrap());
        let picker = cx.add_window(|window, cx| ModelPicker::new(Tokens::light(), window, cx));

        picker
            .update(cx, |picker, window, cx| {
                let first = model("p", "Provider", "first", "First");
                let second = model("p", "Provider", "second", "Second");
                let reported = Some(model_key(&first));
                picker.sync(
                    &[first.clone(), second.clone()],
                    reported.clone(),
                    Language::English,
                    window,
                    cx,
                );

                picker.visible = vec![0, 1];
                picker.confirm(1, cx);
                assert_eq!(picker.selected, Some(model_key(&second)));

                // A transcript update can arrive before SetModel is reflected in
                // state. Repeating the old value must not pull the trigger back.
                picker.sync(
                    &[first, second.clone()],
                    reported,
                    Language::English,
                    window,
                    cx,
                );
                assert_eq!(picker.selected, Some(model_key(&second)));
            })
            .expect("the picker window is open");
    }

    const _: () = {
        assert!(MENU_WIDTH > 0.0);
        assert!(LIST_MAX_HEIGHT >= DROPDOWN_ROW_HEIGHT);
        assert!(SEARCH_HEIGHT >= DROPDOWN_ROW_HEIGHT);
    };
}
