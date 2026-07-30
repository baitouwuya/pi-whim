//! Background AI task routing and shared resource limits.
//!
//! Each task belongs in its own group so it can gain an independent model
//! without turning General settings into a growing collection of AI controls.

use gpui::{AnyElement, Entity, IntoElement, ParentElement, Styled, div};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use pi_whim_core::{
    AppState, OneShotAiConfig, ProviderId, ThinkingLevel,
    strings::{text as translate, tr},
};
use pi_whim_theme::Tokens;

use crate::settings::{
    Emit, SettingsEvent,
    dropdown::{self, Choice, ChoiceState},
    form, toggle,
};

pub struct Fields {
    pub model: Entity<ChoiceState<Option<(ProviderId, String)>>>,
    pub thinking: Entity<ChoiceState<ThinkingLevel>>,
    pub concurrency: Entity<InputState>,
    pub queue_capacity: Entity<InputState>,
    pub timeout: Entity<InputState>,
}

pub fn render(state: &AppState, fields: &Fields, tokens: Tokens, emit: Emit) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(form::page_header(
            tr(state, "background-ai"),
            Some(tr(state, "background-ai-help")),
            tokens,
        ))
        .child(form::group_stack(vec![
            form::group(
                tr(state, "background-ai-session-title"),
                Some(tr(state, "background-ai-session-title-help")),
                tokens,
                vec![
                    enabled_row(state, tokens, emit.clone()),
                    model_row(state, fields, tokens),
                    thinking_row(state, fields, tokens),
                ],
            ),
            form::group(
                tr(state, "background-ai-resources"),
                None,
                tokens,
                vec![
                    form::row(
                        tr(state, "background-ai-concurrency"),
                        Some(tr(state, "background-ai-concurrency-help")),
                        tokens,
                        numeric(&fields.concurrency),
                    ),
                    form::row(
                        tr(state, "background-ai-queue-capacity"),
                        Some(tr(state, "background-ai-queue-capacity-help")),
                        tokens,
                        numeric(&fields.queue_capacity),
                    ),
                    form::row(
                        tr(state, "background-ai-timeout"),
                        Some(tr(state, "background-ai-timeout-help")),
                        tokens,
                        numeric(&fields.timeout),
                    ),
                    limits_apply(fields, state, emit),
                ],
            ),
        ]))
        .into_any_element()
}

fn numeric(state: &Entity<InputState>) -> AnyElement {
    Input::new(state).small().into_any_element()
}

fn enabled_row(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let config = state.one_shot_ai_config.clone();
    form::row(
        tr(state, "background-ai-enabled"),
        Some(tr(state, "background-ai-enabled-help")),
        tokens,
        toggle::toggle(
            "background-ai-enabled",
            tr(state, "background-ai-enabled"),
            config.enabled,
            tokens,
            move |enabled, window, cx| {
                emit(
                    SettingsEvent::SetOneShotAiConfig(OneShotAiConfig {
                        enabled,
                        ..config.clone()
                    }),
                    window,
                    cx,
                );
            },
        ),
    )
}

fn model_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "background-ai-model"),
        Some(tr(state, "background-ai-model-help")),
        tokens,
        dropdown::dropdown(&fields.model),
    )
}

fn thinking_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "background-ai-thinking"),
        None,
        tokens,
        dropdown::dropdown(&fields.thinking),
    )
}

fn limits_apply(fields: &Fields, state: &AppState, emit: Emit) -> AnyElement {
    let concurrency = fields.concurrency.clone();
    let queue_capacity = fields.queue_capacity.clone();
    let timeout = fields.timeout.clone();
    let current = state.one_shot_ai_config.clone();
    let language = state.language;
    form::control_row(
        Button::new("apply-background-ai-limits")
            .primary()
            .small()
            .label(translate("apply", language))
            .on_click(move |_, window, cx| {
                let config = OneShotAiConfig {
                    max_concurrency: parse_number(&concurrency, cx, current.max_concurrency),
                    queue_capacity: parse_number(&queue_capacity, cx, current.queue_capacity),
                    timeout_secs: parse_number(&timeout, cx, current.timeout_secs),
                    ..current.clone()
                }
                .normalized();
                emit(SettingsEvent::SetOneShotAiConfig(config), window, cx);
            }),
    )
}

fn parse_number<T>(field: &Entity<InputState>, cx: &gpui::App, fallback: T) -> T
where
    T: std::str::FromStr,
{
    field.read(cx).value().parse().unwrap_or(fallback)
}

pub fn selected_model(state: &AppState) -> Option<(ProviderId, String)> {
    state
        .one_shot_ai_config
        .provider_id
        .zip(state.one_shot_ai_config.model_id.clone())
}

pub fn model_choices(state: &AppState) -> Vec<Choice<Option<(ProviderId, String)>>> {
    let mut choices = vec![Choice::new(None, tr(state, "background-ai-select-model"))];
    for provider in &state.provider_profiles {
        for model in &provider.models {
            choices.push(Choice::new(
                Some((provider.id, model.id.clone())),
                format!("{} / {}", provider.name, model.name),
            ));
        }
    }
    choices
}

pub fn thinking_choices(state: &AppState) -> Vec<Choice<ThinkingLevel>> {
    let config = &state.one_shot_ai_config;
    let levels = state
        .provider_profiles
        .iter()
        .find(|provider| Some(provider.id) == config.provider_id)
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|model| Some(model.id.as_str()) == config.model_id.as_deref())
        })
        .map(|model| model.available_thinking_levels())
        .unwrap_or_else(|| ThinkingLevel::ALL.to_vec());
    levels
        .into_iter()
        .map(|level| {
            let label = if level == ThinkingLevel::Off {
                translate("thinking-off", state.language)
            } else {
                level.as_str()
            };
            Choice::new(level, label)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{ProviderModel, ProviderProfile, ProviderProtocol};

    #[test]
    fn model_choices_keep_provider_and_model_together() {
        let provider_id = ProviderId::new_v4();
        let mut state = AppState::default();
        state.provider_profiles.push(ProviderProfile {
            id: provider_id,
            name: "Example".into(),
            base_url: "https://example.test".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("small")],
            updated_at_ms: 0,
            has_api_key: true,
        });

        let choices = model_choices(&state);
        assert_eq!(choices[0].value, None);
        assert_eq!(choices[1].value, Some((provider_id, "small".to_owned())));
    }

    #[test]
    fn thinking_follows_the_selected_models_capabilities() {
        let provider_id = ProviderId::new_v4();
        let mut state = AppState::default();
        state.one_shot_ai_config.provider_id = Some(provider_id);
        state.one_shot_ai_config.model_id = Some("plain".into());
        state.provider_profiles.push(ProviderProfile {
            id: provider_id,
            name: "Example".into(),
            base_url: "https://example.test".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("plain")],
            updated_at_ms: 0,
            has_api_key: true,
        });

        let choices = thinking_choices(&state);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].value, ThinkingLevel::Off);
    }
}
