//! List-first background AI task settings and shared resource limits.

use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, ParentElement, SharedString,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    dialog::Dialog,
    input::{Input, InputState},
};
use pi_whim_core::{
    AppState, OneShotAiConfig, OneShotAiTaskConfig, ProviderId, SESSION_TITLE_TASK_KIND,
    ThinkingLevel,
    strings::{text as translate, tr},
};
use pi_whim_theme::{Tokens, font, radius, text};

use crate::{
    icons,
    settings::{
        Emit, SettingsEvent,
        dropdown::{self, Choice, ChoiceState},
        form, toggle,
    },
    theme::IntoHsla,
};

const EDITOR_WIDTH: f32 = 620.0;
const TASK_ROW_HEIGHT: f32 = 58.0;
const STATUS_WIDTH: f32 = 76.0;

#[derive(Clone, Copy)]
struct TaskDefinition {
    kind: &'static str,
    title_key: &'static str,
    help_key: &'static str,
}

const TASKS: [TaskDefinition; 1] = [TaskDefinition {
    kind: SESSION_TITLE_TASK_KIND,
    title_key: "background-ai-session-title",
    help_key: "background-ai-session-title-help",
}];

pub struct Fields {
    pub model: Entity<ChoiceState<Option<(ProviderId, String)>>>,
    pub thinking: Entity<ChoiceState<ThinkingLevel>>,
    pub concurrency: Entity<InputState>,
    pub queue_capacity: Entity<InputState>,
    pub timeout: Entity<InputState>,
}

pub fn render(state: &AppState, fields: &Fields, tokens: Tokens, emit: Emit) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .child(form::page_header(
            tr(state, "background-ai"),
            Some(tr(state, "background-ai-help")),
            tokens,
        ))
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .pt(px(form::PAGE_GROUP_GAP))
                .child(form::section_header(
                    tr(state, "background-ai-tasks"),
                    Some(tr(state, "background-ai-tasks-help")),
                    tokens,
                ))
                .child(task_list(state, tokens, emit.clone())),
        )
        .child(div().w_full().pt(px(form::GROUP_GAP)).child(form::group(
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
        )))
        .into_any_element()
}

fn task_list(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .border_t_1()
        .border_color(tokens.line.hsla())
        .children(
            TASKS
                .iter()
                .enumerate()
                .map(|(index, task)| task_row(index, *task, state, tokens, emit.clone())),
        )
        .into_any_element()
}

fn task_row(
    index: usize,
    definition: TaskDefinition,
    state: &AppState,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let task = state.one_shot_ai_config.task(definition.kind);
    let enabled = task.enabled;
    let status = tr(state, if enabled { "enabled" } else { "disabled" });
    let status_color = if enabled {
        tokens.success
    } else {
        tokens.muted
    };
    let dot_color = if enabled {
        tokens.success
    } else {
        tokens.line_strong
    };
    let details = task_details(state, &task);
    let edit = emit.clone();
    let kind_for_edit = definition.kind.to_owned();
    let kind_for_row = definition.kind.to_owned();

    div()
        .id(("background-ai-task-row", index))
        .w_full()
        .h(px(TASK_ROW_HEIGHT))
        .flex()
        .items_center()
        .gap(px(12.0))
        .px(px(10.0))
        .border_b_1()
        .border_color(tokens.line.hsla())
        .cursor_pointer()
        .hover(move |row| row.bg(tokens.control_background_hover().hsla()))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(px(text::DETAIL_SIZE))
                        .text_color(if enabled {
                            tokens.text.hsla()
                        } else {
                            tokens.muted.hsla()
                        })
                        .child(SharedString::from(tr(state, definition.title_key))),
                )
                .child(
                    div()
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(font::MONO)
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(SharedString::from(details)),
                ),
        )
        .child(
            div()
                .w(px(STATUS_WIDTH))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.0))
                .child(
                    div()
                        .size(px(6.0))
                        .rounded(px(radius::DOT))
                        .bg(dot_color.hsla()),
                )
                .child(
                    div()
                        .font_family(font::MONO)
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(status_color.hsla())
                        .child(status),
                ),
        )
        .child(
            div()
                .id(("background-ai-task-actions", index))
                .flex_none()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    Button::new(("edit-background-ai-task", index as u64))
                        .icon(icons::settings())
                        .ghost()
                        .xsmall()
                        .tooltip(tr(state, "edit-background-ai-task"))
                        .on_click(move |_, window, cx| {
                            edit(
                                SettingsEvent::EditBackgroundAiTask(kind_for_edit.clone()),
                                window,
                                cx,
                            )
                        }),
                ),
        )
        .on_click(move |_, window, cx| {
            emit(
                SettingsEvent::EditBackgroundAiTask(kind_for_row.clone()),
                window,
                cx,
            )
        })
        .into_any_element()
}

fn task_details(state: &AppState, task: &OneShotAiTaskConfig) -> String {
    let model = task
        .provider_id
        .zip(task.model_id.as_deref())
        .and_then(|(provider_id, model_id)| {
            state
                .provider_profiles
                .iter()
                .find(|provider| provider.id == provider_id)
                .and_then(|provider| {
                    provider
                        .models
                        .iter()
                        .find(|model| model.id == model_id)
                        .map(|model| format!("{} / {}", provider.name, model.name))
                })
        })
        .unwrap_or_else(|| tr(state, "background-ai-select-model").to_owned());
    let thinking = if task.thinking_level == ThinkingLevel::Off {
        tr(state, "thinking-off").to_owned()
    } else {
        task.thinking_level.as_str().to_owned()
    };
    format!(
        "{model}  |  {}: {thinking}",
        tr(state, "background-ai-thinking")
    )
}

pub fn render_editor(
    state: &AppState,
    kind: &str,
    draft: &OneShotAiTaskConfig,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
    cx: &mut App,
) -> AnyElement {
    let definition = task_definition(kind).unwrap_or(TASKS[0]);
    let can_save = !draft.enabled || (draft.provider_id.is_some() && draft.model_id.is_some());
    let cancel_button = emit.clone();
    let save_button = emit.clone();
    let confirm = emit.clone();
    let cancel = emit.clone();
    let footer = div()
        .w_full()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(form::INLINE_GAP))
        .child(
            Button::new("cancel-background-ai-task")
                .label(tr(state, "cancel"))
                .outline()
                .small()
                .on_click(move |_, window, cx| {
                    cancel_button(SettingsEvent::CloseBackgroundAiTaskEditor, window, cx)
                }),
        )
        .child(
            Button::new("save-background-ai-task")
                .label(tr(state, "save"))
                .primary()
                .small()
                .disabled(!can_save)
                .on_click(move |_, window, cx| {
                    save_button(SettingsEvent::SaveBackgroundAiTask, window, cx)
                }),
        );

    div()
        .child(
            Dialog::new(cx)
                .w(px(EDITOR_WIDTH))
                .title(SharedString::from(tr(state, definition.title_key)))
                .footer(footer)
                .on_ok(move |_, window, cx| {
                    if can_save {
                        confirm(SettingsEvent::SaveBackgroundAiTask, window, cx);
                    }
                    false
                })
                .on_cancel(move |_, window, cx| {
                    cancel(SettingsEvent::CloseBackgroundAiTaskEditor, window, cx);
                    true
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .pb(px(form::GROUP_HEADER_GAP))
                                .child(form::help_text(tr(state, definition.help_key), tokens)),
                        )
                        .child(enabled_row(state, draft, tokens, emit))
                        .child(form::row(
                            tr(state, "background-ai-model"),
                            Some(tr(state, "background-ai-model-help")),
                            tokens,
                            dropdown::dropdown(&fields.model),
                        ))
                        .child(form::row(
                            tr(state, "background-ai-thinking"),
                            None,
                            tokens,
                            dropdown::dropdown(&fields.thinking),
                        ))
                        .when(!can_save, |this| {
                            this.child(form::control_row(form::field_error(
                                tr(state, "background-ai-task-incomplete"),
                                tokens,
                            )))
                        }),
                ),
        )
        .into_any_element()
}

fn task_definition(kind: &str) -> Option<TaskDefinition> {
    TASKS.iter().copied().find(|task| task.kind == kind)
}

fn enabled_row(
    state: &AppState,
    draft: &OneShotAiTaskConfig,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    form::row(
        tr(state, "enabled"),
        Some(tr(state, "background-ai-enabled-help")),
        tokens,
        toggle::toggle(
            "background-ai-task-enabled",
            tr(state, "enabled"),
            draft.enabled,
            tokens,
            move |enabled, window, cx| {
                emit(
                    SettingsEvent::SetBackgroundAiTaskEnabled(enabled),
                    window,
                    cx,
                )
            },
        ),
    )
}

fn numeric(state: &Entity<InputState>) -> AnyElement {
    Input::new(state).small().into_any_element()
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

pub fn selected_model(task: &OneShotAiTaskConfig) -> Option<(ProviderId, String)> {
    task.provider_id.zip(task.model_id.clone())
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

pub fn thinking_choices(
    state: &AppState,
    task: &OneShotAiTaskConfig,
) -> Vec<Choice<ThinkingLevel>> {
    let levels = state
        .provider_profiles
        .iter()
        .find(|provider| Some(provider.id) == task.provider_id)
        .and_then(|provider| {
            provider
                .models
                .iter()
                .find(|model| Some(model.id.as_str()) == task.model_id.as_deref())
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
    fn thinking_follows_the_drafts_selected_model() {
        let provider_id = ProviderId::new_v4();
        let mut state = AppState::default();
        state.provider_profiles.push(ProviderProfile {
            id: provider_id,
            name: "Example".into(),
            base_url: "https://example.test".into(),
            protocol: ProviderProtocol::OpenAiCompletions,
            models: vec![ProviderModel::new("plain")],
            updated_at_ms: 0,
            has_api_key: true,
        });
        let task = OneShotAiTaskConfig {
            provider_id: Some(provider_id),
            model_id: Some("plain".into()),
            ..Default::default()
        };

        let choices = thinking_choices(&state, &task);
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].value, ThinkingLevel::Off);
    }

    #[test]
    fn task_details_are_list_friendly_and_task_specific() {
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
        let details = task_details(
            &state,
            &OneShotAiTaskConfig {
                provider_id: Some(provider_id),
                model_id: Some("small".into()),
                thinking_level: ThinkingLevel::High,
                ..Default::default()
            },
        );
        assert!(details.contains("Example / small"));
        assert!(details.contains("high"));
    }
}
