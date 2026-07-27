//! The runtime controls: which model answers, how hard it thinks, how queued
//! prompts are released.
//!
//! These render into the composer's footer rather than as a bar under the top
//! chrome. They belong next to the prompt because they describe the turn about to
//! be sent, and as a full-width row they wrapped onto three lines while leaving
//! most of each one empty.
//!
//! All four are `Select`s over `gpui-component`'s searchable list. The egui build
//! hand-rolled the model picker out of a `ComboBox` wrapping a `TextEdit` and a
//! `ScrollArea`, re-grouping every model by provider on each frame it was open;
//! grouping and filtering are both built into the delegate here.
//!
//! Two details are load-bearing:
//!
//! * The virtual list measures one probe row and assumes the rest match, so a
//!   model row reserves its second line even when the id equals the name. Showing
//!   it conditionally makes rows of two heights and the list mis-measures.
//! * `matches` is widened past the default. The default only searches the display
//!   title, and a reader hunting `sonnet-4-5` is typing the id, not the name.

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, IntoElement, ParentElement, Render,
    SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Sizable,
    select::{SearchableVec, Select, SelectEvent, SelectGroup, SelectItem, SelectState},
};
use pi_whim_core::{AppState, ModelOption, SessionStatus, ThinkingLevel};
use pi_whim_theme::{Tokens, text};

use crate::theme::IntoHsla;

/// Width of the model trigger, and of the popup it opens.
///
/// The popup is wider than the trigger because a model's id is longer than its
/// name — that is the whole reason the second line exists.
const MODEL_TRIGGER_WIDTH: f32 = 190.0;
const MODEL_MENU_WIDTH: f32 = 340.0;
const MODEL_MENU_MAX_HEIGHT: f32 = 320.0;

/// What the controls ask the shell to change.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlsEvent {
    /// Switch the model. Deferred until the next prompt, so the prior model
    /// compacts the history first.
    SetModel(ModelOption),
    SetThinkingLevel(ThinkingLevel),
}

/// One model in the picker.
///
/// Carries the provider name so the delegate can group by it, and so a search
/// for a provider surfaces everything under it.
#[derive(Clone, Debug, PartialEq)]
struct ModelItem {
    model: ModelOption,
    /// Provider and id, precomputed because `value` returns a reference.
    key: (String, String),
    tokens: Tokens,
}

impl ModelItem {
    fn new(model: ModelOption, tokens: Tokens) -> Self {
        let key = (model.provider.clone(), model.id.clone());
        Self { model, key, tokens }
    }

    /// The dimmed second line: the model's id, when it says anything the name
    /// does not.
    ///
    /// Empty rather than absent when it would repeat — see the note on row
    /// heights at the top of this module.
    fn secondary(&self) -> SharedString {
        if self.model.id == self.model.name {
            SharedString::default()
        } else {
            self.model.id.clone().into()
        }
    }
}

impl SelectItem for ModelItem {
    /// Provider and id together: two models can share a name across providers,
    /// and selecting one has to be distinguishable from the other.
    type Value = (String, String);

    fn title(&self) -> SharedString {
        self.model.name.clone().into()
    }

    fn value(&self) -> &Self::Value {
        // Stored rather than derived so the borrow outlives this call.
        &self.key
    }

    fn matches(&self, query: &str) -> bool {
        // The default searches only the title. A reader looking for a specific
        // build is typing the id.
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }
        let model = &self.model;
        model.name.to_lowercase().contains(&query)
            || model.id.to_lowercase().contains(&query)
            || model.provider_name.to_lowercase().contains(&query)
    }

    fn render(&self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        // The second line is always laid out, even when it is empty: the virtual
        // list measures one row and applies that height to all of them.
        let secondary = self.secondary();
        div()
            .flex()
            .flex_col()
            .child(div().text_size(px(text::DETAIL_SIZE)).child(self.title()))
            .child(
                div()
                    .font_family(pi_whim_theme::font::MONO)
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(self.tokens.muted.hsla())
                    .child(secondary),
            )
    }
}

/// A picker over a fixed set of enum values, with no search.
///
/// Thinking levels and queue modes are both short closed lists where a search
/// field would be more chrome than help.
#[derive(Clone, Debug, PartialEq)]
struct Choice<T: Clone + PartialEq + 'static> {
    label: SharedString,
    value: T,
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

/// Group the available models by provider, in a stable order.
///
/// Providers sort by name and models keep the order the agent reported them in,
/// which is its own ranking and worth preserving.
fn model_groups(models: &[ModelOption], tokens: Tokens) -> Vec<SelectGroup<ModelItem>> {
    let mut providers: Vec<&str> = Vec::new();
    for model in models {
        if !providers.contains(&model.provider_name.as_str()) {
            providers.push(&model.provider_name);
        }
    }
    providers.sort_unstable();

    providers
        .into_iter()
        .map(|provider| {
            SelectGroup::new(provider.to_owned()).items(
                models
                    .iter()
                    .filter(|model| model.provider_name == provider)
                    .map(|model| ModelItem::new(model.clone(), tokens)),
            )
        })
        .collect()
}

/// Why the model picker is not showing any models.
///
/// A failed session explains itself; an empty list with no failure means the
/// agent has not answered yet, which is not an error to report as one.
fn models_unavailable_note(status: &SessionStatus) -> String {
    match status {
        SessionStatus::Failed(error) => format!("No models available: {error}"),
        _ => "No models available".to_owned(),
    }
}

/// The runtime controls bar.
pub struct Controls {
    model: Entity<SelectState<SearchableVec<SelectGroup<ModelItem>>>>,
    thinking: Entity<SelectState<Vec<Choice<ThinkingLevel>>>>,
    /// Whether a project is selected. With none, there is no agent to configure.
    visible: bool,
    /// Kept so the picker can explain an empty model list.
    status: SessionStatus,
    /// The models as the agent reported them, for resolving a picked row back to
    /// the option the shell needs.
    models: Vec<ModelOption>,
    tokens: Tokens,
}

impl EventEmitter<ControlsEvent> for Controls {}

impl Controls {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model = cx.new(|cx| {
            SelectState::new(SearchableVec::new(Vec::new()), None, window, cx).searchable(true)
        });
        cx.subscribe_in(&model, window, |controls, _, event, _, cx| {
            let SelectEvent::Confirm(Some(key)) = event else {
                return;
            };
            if let Some(model) = controls.model_for(key) {
                cx.emit(ControlsEvent::SetModel(model));
            }
        })
        .detach();

        let thinking = cx.new(|cx| SelectState::new(Vec::new(), None, window, cx));
        cx.subscribe_in(&thinking, window, |_, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(level)) = event {
                cx.emit(ControlsEvent::SetThinkingLevel(*level));
            }
        })
        .detach();

        Self {
            model,
            thinking,
            visible: false,
            status: SessionStatus::default(),
            models: Vec::new(),
            tokens,
        }
    }

    /// Reseed every control from the state the agent reported.
    pub fn sync(&mut self, state: &AppState, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = state.selected_project.is_some();
        self.status = state.session_status.clone();
        self.models = state.available_models.clone();
        let tokens = self.tokens;

        let groups = model_groups(&self.models, tokens);
        // A pending switch is what the picker shows: the user chose it, and it
        // takes effect on the next prompt.
        let selected = state
            .pending_model
            .as_ref()
            .or(state.current_model.as_ref())
            .map(|model| (model.provider.clone(), model.id.clone()));
        self.model.update(cx, |picker, cx| {
            picker.set_items(SearchableVec::new(groups), window, cx);
            match &selected {
                Some(key) => picker.set_selected_value(key, window, cx),
                None => picker.set_selected_index(None, window, cx),
            }
        });

        let levels: Vec<Choice<ThinkingLevel>> = state
            .available_thinking_levels
            .iter()
            .map(|level| Choice {
                label: level.as_str().into(),
                value: *level,
            })
            .collect();
        let level = state.thinking_level;
        self.thinking.update(cx, |picker, cx| {
            picker.set_items(levels, window, cx);
            picker.set_selected_value(&level, window, cx);
        });

        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// The model behind a picked row.
    ///
    /// The event carries only the row's value, and the shell needs the whole
    /// option, so it is looked back up here. Keeping the table on this side means
    /// not reaching into the picker for its items.
    fn model_for(&self, key: &(String, String)) -> Option<ModelOption> {
        self.models
            .iter()
            .find(|model| model.provider == key.0 && model.id == key.1)
            .cloned()
    }
}

impl Render for Controls {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        if !self.visible {
            // No project means no agent to configure, and an empty bar would
            // still cost a row of chrome.
            return div();
        }

        // No background, border, or width of its own: this sits in the composer's
        // footer now, so the panel and the border around it are already the
        // composer's. Giving it a second surface drew a bar inside a bar.
        // One line, never wrapped: these sit in the composer's footer beside the
        // key hint, and the hint is what gives up space when the window narrows.
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .child(
                div()
                    .when(!self.models.is_empty(), |this| {
                        this.child(
                            Select::new(&self.model)
                                .small()
                                .w(px(MODEL_TRIGGER_WIDTH))
                                .menu_width(px(MODEL_MENU_WIDTH))
                                .menu_max_h(px(MODEL_MENU_MAX_HEIGHT))
                                .placeholder("Model")
                                .search_placeholder("Search models"),
                        )
                    })
                    .when(self.models.is_empty(), |this| {
                        // Why there is nothing to pick, rather than an empty
                        // control the reader would take for a broken one.
                        this.child(
                            div()
                                .font_family(pi_whim_theme::font::MONO)
                                .text_size(px(text::LABEL_SIZE))
                                .text_color(match self.status {
                                    SessionStatus::Failed(_) => tokens.error.hsla(),
                                    _ => tokens.muted.hsla(),
                                })
                                .child(models_unavailable_note(&self.status)),
                        )
                    }),
            )
            .child(
                Select::new(&self.thinking)
                    .small()
                    .title_prefix("Thinking: ")
                    .placeholder("off"),
            )
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

    fn item(model: ModelOption) -> ModelItem {
        ModelItem::new(model, Tokens::light())
    }

    #[test]
    fn models_are_grouped_under_their_provider() {
        let models = vec![
            model("p1", "Anthropic", "claude-opus-4-8", "Opus 4.8"),
            model("p2", "OpenAI", "gpt-5", "GPT-5"),
            model("p1", "Anthropic", "claude-sonnet-5", "Sonnet 5"),
        ];
        let groups = model_groups(&models, Tokens::light());

        assert_eq!(groups.len(), 2);
        // Providers sort by name, so the order does not shift between refreshes.
        assert_eq!(groups[0].title, "Anthropic");
        assert_eq!(groups[1].title, "OpenAI");
        assert_eq!(groups[0].items.len(), 2);
        // Within a provider the agent's own order is kept — it is a ranking.
        assert_eq!(groups[0].items[0].model.name, "Opus 4.8");
        assert_eq!(groups[0].items[1].model.name, "Sonnet 5");
    }

    #[test]
    fn no_models_means_no_groups() {
        assert!(model_groups(&[], Tokens::light()).is_empty());
    }

    #[test]
    fn a_model_is_identified_by_provider_and_id_together() {
        // Two providers can serve the same model id, and picking one must not
        // select the other's.
        let first = item(model("p1", "Anthropic", "claude-opus-4-8", "Opus"));
        let second = item(model("p2", "Bedrock", "claude-opus-4-8", "Opus"));

        assert_ne!(first.value(), second.value());
    }

    #[test]
    fn searching_matches_the_id_as_well_as_the_name() {
        // The default delegate only searches the title; someone hunting a
        // specific build types the id.
        let candidate = item(model("p1", "Anthropic", "claude-sonnet-5", "Sonnet 5"));

        assert!(candidate.matches("sonnet"));
        assert!(candidate.matches("claude-sonnet-5"));
        assert!(candidate.matches("Anthropic"));
        assert!(!candidate.matches("gpt"));
    }

    #[test]
    fn searching_ignores_case_and_surrounding_space() {
        let candidate = item(model("p1", "Anthropic", "claude-opus-4-8", "Opus 4.8"));

        assert!(candidate.matches("  OPUS "));
        assert!(candidate.matches("CLAUDE-OPUS"));
    }

    #[test]
    fn an_empty_query_matches_everything() {
        let candidate = item(model("p1", "Anthropic", "claude-opus-4-8", "Opus 4.8"));

        assert!(candidate.matches(""));
        assert!(candidate.matches("   "));
    }

    #[test]
    fn an_empty_model_list_says_why_when_the_session_failed() {
        // A failure the reader can act on; anything else is just "not yet".
        assert_eq!(
            models_unavailable_note(&SessionStatus::Failed("pi not found".into())),
            "No models available: pi not found"
        );
        assert_eq!(
            models_unavailable_note(&SessionStatus::Starting),
            "No models available"
        );
        assert_eq!(
            models_unavailable_note(&SessionStatus::Ready),
            "No models available"
        );
    }

    #[test]
    fn every_thinking_level_has_a_label() {
        // The picker labels come straight from the enum, so a level added to
        // core cannot render blank.
        for level in ThinkingLevel::ALL {
            assert!(!level.as_str().is_empty());
        }
    }

    #[test]
    fn a_models_secondary_line_is_reserved_even_when_it_would_repeat() {
        // The virtual list measures one row and applies that height to all of
        // them; a conditionally-present second line makes rows of two heights.
        let same = item(model("p1", "Local", "llama", "llama"));
        assert_eq!(same.secondary(), SharedString::default());

        let different = item(model("p1", "Anthropic", "claude-opus-4-8", "Opus 4.8"));
        assert_eq!(different.secondary(), SharedString::from("claude-opus-4-8"));
    }

    // The popup is wider than its trigger, since an id is longer than a name.
    const _: () = {
        assert!(MODEL_MENU_WIDTH > MODEL_TRIGGER_WIDTH);
        assert!(MODEL_MENU_MAX_HEIGHT > 0.0);
    };
}
