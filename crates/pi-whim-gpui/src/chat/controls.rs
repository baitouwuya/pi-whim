//! The runtime controls: what the agent may reach, which model answers, and how
//! hard it thinks.
//!
//! These render into the prompt's own box rather than as a bar under the top
//! chrome. They belong next to the prompt because they describe the turn about to
//! be sent, and as a full-width row they wrapped onto three lines while leaving
//! most of each one empty.
//!
//! All three controls share one flat trigger and one dense popup surface. Models
//! add search and scrolling; the two short closed lists stay simpler.

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::tooltip::Tooltip;
use pi_whim_core::{
    AgentPermissionLevel, AppState, Language, ModelOption, SessionStatus, ThinkingLevel,
    strings::text as translate,
};
use pi_whim_theme::{Tokens, radius, text};

use super::{
    dropdown::{Choice, ChoicePicker, ChoicePickerEvent},
    model_picker::{ModelPicker, ModelPickerEvent},
};
use crate::theme::IntoHsla;

const PERMISSION_MENU_WIDTH: f32 = 148.0;
const THINKING_MENU_WIDTH: f32 = 132.0;

/// What the controls ask the shell to change.
#[derive(Clone, Debug, PartialEq)]
pub enum ControlsEvent {
    /// Switch the model. Deferred until the next prompt, so the prior model
    /// compacts the history first.
    SetModel(ModelOption),
    SetThinkingLevel(ThinkingLevel),
    /// Raise or lower what a spawned agent may reach without asking.
    SetPermissionLevel(AgentPermissionLevel),
}

/// Diameter of the permission dot.
const DOT_SIZE: f32 = 7.0;

/// The three permission levels, in the order they escalate.
///
/// Offered as a list, like the other two controls on this row. Click-to-advance
/// fit in less space, but raising what an agent may reach is not something to do
/// by accident, and it is the one control here where landing on the wrong value
/// costs more than a second click.
const PERMISSION_LEVELS: [AgentPermissionLevel; 3] = [
    AgentPermissionLevel::ReadOnly,
    AgentPermissionLevel::Controlled,
    AgentPermissionLevel::Full,
];

/// The string key naming `level`.
fn permission_key(level: AgentPermissionLevel) -> &'static str {
    match level {
        AgentPermissionLevel::ReadOnly => "permission-read-only",
        AgentPermissionLevel::Controlled => "permission-controlled",
        AgentPermissionLevel::Full => "permission-full",
    }
}

/// What colour the dot beside the level takes.
///
/// Full access is the one worth flagging: the agent can reach the host without
/// asking, and that should be visible from across the room rather than only on
/// the settings page. The other two are ordinary states, so they stay muted.
fn permission_color(level: AgentPermissionLevel, tokens: Tokens) -> gpui::Hsla {
    match level {
        AgentPermissionLevel::Full => tokens.warning.hsla(),
        _ => tokens.muted.hsla(),
    }
}

/// Why the model picker is not showing any models.
///
/// A failed session explains itself; an empty list with no failure means the
/// agent has not answered yet, which is not an error to report as one.
fn models_unavailable_note(status: &SessionStatus, language: Language) -> String {
    let note = translate("no-models-available", language);
    match status {
        SessionStatus::Failed(error) => format!("{note}: {error}"),
        _ => note.to_owned(),
    }
}

/// The runtime controls bar.
pub struct Controls {
    model_picker: Entity<ModelPicker>,
    thinking: Entity<ChoicePicker<ThinkingLevel>>,
    permission_picker: Entity<ChoicePicker<AgentPermissionLevel>>,
    /// Whether a project is selected. With none, there is no agent to configure.
    visible: bool,
    /// Kept so the picker can explain an empty model list.
    status: SessionStatus,
    /// What a spawned agent may reach without asking.
    ///
    /// A copy of `agent_team_config.default_policy.level`. Read from the snapshot
    /// rather than held as its own truth: the settings page changes the same field.
    permission: AgentPermissionLevel,
    /// The language the permission label is read in.
    language: Language,
    /// The models as the agent reported them, for resolving a picked row back to
    /// the option the shell needs.
    models: Vec<ModelOption>,
    tokens: Tokens,
}

impl EventEmitter<ControlsEvent> for Controls {}

impl Controls {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let model_picker = cx.new(|cx| ModelPicker::new(tokens, window, cx));
        cx.subscribe_in(&model_picker, window, |_, _, event, _, cx| match event {
            ModelPickerEvent::Confirm(model) => {
                cx.emit(ControlsEvent::SetModel(model.clone()));
            }
        })
        .detach();

        let thinking: Entity<ChoicePicker<ThinkingLevel>> =
            cx.new(|_| ChoicePicker::new("thinking", THINKING_MENU_WIDTH, tokens));
        cx.subscribe_in(&thinking, window, |_, _, event, _, cx| match event {
            ChoicePickerEvent::Confirm(level) => {
                cx.emit(ControlsEvent::SetThinkingLevel(*level));
            }
        })
        .detach();

        let permission_picker: Entity<ChoicePicker<AgentPermissionLevel>> =
            cx.new(|_| ChoicePicker::new("permission", PERMISSION_MENU_WIDTH, tokens));
        cx.subscribe_in(
            &permission_picker,
            window,
            |_, _, event, _, cx| match event {
                ChoicePickerEvent::Confirm(level) => {
                    cx.emit(ControlsEvent::SetPermissionLevel(*level));
                }
            },
        )
        .detach();

        Self {
            model_picker,
            thinking,
            permission_picker,
            visible: false,
            status: SessionStatus::default(),
            models: Vec::new(),
            permission: AgentPermissionLevel::default(),
            language: Language::default(),
            tokens,
        }
    }

    /// Reseed every control from the state the agent reported.
    pub fn sync(&mut self, state: &AppState, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = state.selected_project.is_some();
        self.status = state.session_status.clone();

        // A pending switch is what the picker shows: the user chose it, and it
        // takes effect on the next prompt.
        let selected = state
            .pending_model
            .as_ref()
            .or(state.current_model.as_ref())
            .map(|model| (model.provider.clone(), model.id.clone()));
        if self.models != state.available_models {
            self.models = state.available_models.clone();
        }
        self.model_picker.update(cx, |picker, cx| {
            picker.sync(
                &state.available_models,
                selected,
                state.language,
                window,
                cx,
            );
        });

        let permission = state.agent_team_config.default_policy.level;
        let language = state.language;
        self.permission = permission;
        self.language = language;
        self.thinking.update(cx, |picker, cx| {
            picker.sync(
                state
                    .available_thinking_levels
                    .iter()
                    .map(|level| Choice::new(level.as_str(), *level))
                    .collect(),
                Some(state.thinking_level),
                translate("thinking-prefix", language),
                translate("thinking-off", language),
                cx,
            );
        });
        self.permission_picker.update(cx, |picker, cx| {
            picker.sync(
                PERMISSION_LEVELS
                    .iter()
                    .map(|level| Choice::new(translate(permission_key(*level), language), *level))
                    .collect(),
                Some(permission),
                "",
                translate(permission_key(permission), language),
                cx,
            );
        });

        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        self.model_picker
            .update(cx, |picker, cx| picker.set_tokens(tokens, cx));
        self.thinking
            .update(cx, |picker, cx| picker.set_tokens(tokens, cx));
        self.permission_picker
            .update(cx, |picker, cx| picker.set_tokens(tokens, cx));
        cx.notify();
    }

    /// The permission level: a dot, then a picker over the three levels.
    ///
    /// The dot stays separate from the shared trigger because it carries the
    /// warning: full access lets an agent reach the host without asking.
    fn permission_indicator(&self) -> impl IntoElement {
        let tokens = self.tokens;
        // The tooltip sits on the group rather than the picker: the dot and the
        // level name say what is granted, not what kind of setting this is, and
        // The whole group owns the tooltip, so both the dot and label explain it.
        let label = translate("permission-level", self.language);
        div()
            .id("permission-level")
            .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.0))
            .child(
                div()
                    .w(px(DOT_SIZE))
                    .h(px(DOT_SIZE))
                    .flex_none()
                    .rounded(px(radius::DOT))
                    .bg(permission_color(self.permission, tokens)),
            )
            .child(self.permission_picker.clone())
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

        // No background, border, or width of its own: this sits inside the prompt's
        // box, so the panel and the border around it are already the shell's.
        // Giving it a second surface drew a bar inside a bar.
        //
        // One line, never wrapped. Every picker uses the same neutral trigger:
        // quiet at rest, grey on hover or while open.
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(10.0))
            .child(self.permission_indicator())
            .child(
                div()
                    .when(!self.models.is_empty(), |this| {
                        this.child(self.model_picker.clone())
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
                                .child(models_unavailable_note(&self.status, self.language)),
                        )
                    }),
            )
            .child(self.thinking.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_model_list_says_why_when_the_session_failed() {
        // A failure the reader can act on; anything else is just "not yet".
        let english = Language::English;
        assert_eq!(
            models_unavailable_note(&SessionStatus::Failed("pi not found".into()), english),
            "No models available: pi not found"
        );
        assert_eq!(
            models_unavailable_note(&SessionStatus::Starting, english),
            "No models available"
        );
        assert_eq!(
            models_unavailable_note(&SessionStatus::Ready, english),
            "No models available"
        );

        // The error is the agent's own text, so only the note ahead of it is
        // translated — but that much has to be.
        assert!(
            models_unavailable_note(
                &SessionStatus::Failed("pi not found".into()),
                Language::SimplifiedChinese
            )
            .ends_with(": pi not found")
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
    fn the_permission_levels_are_offered_in_escalating_order() {
        // The picker lists them in this order, so a reader scanning down the popup
        // reads increasing reach rather than an arbitrary sequence.
        let ranks: Vec<u8> = PERMISSION_LEVELS.iter().map(|level| level.rank()).collect();
        let mut sorted = ranks.clone();
        sorted.sort_unstable();

        assert_eq!(ranks, sorted);
        // And all three are offered: a level missing from the list is one the
        // reader could never get back to after leaving it.
        assert_eq!(ranks.len(), 3);
    }

    #[test]
    fn only_full_access_is_flagged() {
        // The dot is a warning, not decoration: full access lets a spawned agent
        // reach the host without asking, and the other two do not.
        let tokens = Tokens::light();
        assert_eq!(
            permission_color(AgentPermissionLevel::Full, tokens),
            tokens.warning.hsla()
        );
        assert_eq!(
            permission_color(AgentPermissionLevel::Controlled, tokens),
            tokens.muted.hsla()
        );
        assert_eq!(
            permission_color(AgentPermissionLevel::ReadOnly, tokens),
            tokens.muted.hsla()
        );
    }

    #[test]
    fn every_permission_level_has_a_label_in_both_languages() {
        // A missing key renders as "?", which would leave the row claiming the
        // agent's reach is unknown.
        for level in PERMISSION_LEVELS {
            for language in [Language::English, Language::SimplifiedChinese] {
                assert_ne!(
                    translate(permission_key(level), language),
                    "?",
                    "{level:?} has no label in {language:?}"
                );
            }
        }
    }

    const _: () = {
        assert!(DOT_SIZE > 0.0);
    };
}
