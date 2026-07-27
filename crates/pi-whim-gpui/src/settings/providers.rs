//! Provider settings: which endpoints answer, and with which models.
//!
//! The draft, its validation, and the presets all live in
//! `engine::settings::ProviderDraft`. What is here is the arrangement and the
//! parts that need an `InputState`.

use gpui::{
    AnyElement, Entity, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
};
use pi_whim_core::{AppState, ProviderModel, ProviderProtocol, strings::tr};
use pi_whim_engine::settings::{Preset, ProviderDraft};
use pi_whim_theme::{Tokens, font, text};

use crate::{
    icons,
    settings::{
        Emit, SettingsEvent,
        dropdown::{self, Choice, ChoiceState},
        form,
    },
    theme::IntoHsla,
};

/// The typed fields on this page.
pub struct Fields {
    pub name: Entity<InputState>,
    pub base_url: Entity<InputState>,
    /// The key field. Masked, and never filled from the keychain.
    pub api_key: Entity<InputState>,
    pub model_id: Entity<InputState>,
    pub preset: Entity<ChoiceState<Preset>>,
    pub protocol: Entity<ChoiceState<ProviderProtocol>>,
}

/// Build the Providers page.
pub fn render(
    state: &AppState,
    draft: &ProviderDraft,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let existing = &state.provider_profiles;
    let collides = draft.name_collides(existing);

    div()
        .flex()
        .flex_col()
        .child(form::page_header(
            tr(state, "providers"),
            Some(tr(state, "providers-help")),
            tokens,
        ))
        .child(form::group_stack(vec![
            form::group(
                tr(state, "configured-providers"),
                None,
                tokens,
                vec![provider_list(state, draft, tokens, emit.clone())],
            ),
            // Where the key ends up is worth saying next to the field that takes
            // it: a reader typing a secret into a form wants to know it is not
            // going into the database beside it.
            form::group(
                tr(state, "connection"),
                Some(tr(state, "provider-help")),
                tokens,
                vec![
                    preset_row(state, fields, tokens),
                    name_row(state, fields, tokens, collides),
                    form::row(tr(state, "base-url"), None, tokens, field(&fields.base_url)),
                    protocol_row(state, fields, tokens),
                    api_key_row(state, draft, fields, tokens),
                ],
            ),
            form::group(
                tr(state, "models"),
                Some(tr(state, "models-help")),
                tokens,
                vec![
                    model_tools(state, draft, fields, tokens, emit.clone()),
                    model_list(state, draft, tokens, emit.clone()),
                    save_row(state, draft, tokens, emit),
                ],
            ),
        ]))
        .into_any_element()
}

/// A text field filling the control column.
fn field(state: &Entity<InputState>) -> AnyElement {
    Input::new(state).w_full().into_any_element()
}

/// The stored providers, plus a way to start a new one.
fn provider_list(
    state: &AppState,
    draft: &ProviderDraft,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let rows: Vec<AnyElement> = state
        .provider_profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let selected = draft.id == Some(profile.id);
            let id = profile.id;
            let emit = emit.clone();
            div()
                .w_full()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .py(px(3.0))
                .child(
                    Button::new(("provider", index as u64))
                        .label(SharedString::from(profile.name.clone()))
                        .when(selected, |button| button.primary())
                        .when(!selected, |button| button.ghost())
                        .small()
                        .on_click(move |_, window, cx| {
                            emit(SettingsEvent::SelectProvider(id), window, cx)
                        }),
                )
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(font::MONO)
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(SharedString::from(profile.base_url.clone())),
                )
                .into_any_element()
        })
        .collect();

    let empty = rows.is_empty();
    form::control_row(
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(rows)
            .when(empty, |this| {
                this.child(form::help_text(tr(state, "no-providers"), tokens))
            })
            .child(
                div().flex().justify_end().pt(px(6.0)).child(
                    Button::new("add-provider")
                        .label(tr(state, "add-provider"))
                        .outline()
                        .small()
                        .on_click(move |_, window, cx| {
                            emit(SettingsEvent::NewProvider, window, cx)
                        }),
                ),
            ),
    )
}

fn preset_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "preset"),
        Some(tr(state, "preset-help")),
        tokens,
        dropdown::dropdown(&fields.preset),
    )
}

/// The name row, which is where a collision is reported.
///
/// Reported under the field rather than only by disabling Save: a greyed button
/// with no explanation is the failure mode this replaces.
fn name_row(state: &AppState, fields: &Fields, tokens: Tokens, collides: bool) -> AnyElement {
    form::row(
        tr(state, "provider-name"),
        None,
        tokens,
        div()
            .w_full()
            .flex()
            .flex_col()
            .child(field(&fields.name))
            .when(collides, |this| {
                this.child(form::field_error(
                    tr(state, "duplicate-provider-name"),
                    tokens,
                ))
            }),
    )
}

fn protocol_row(state: &AppState, fields: &Fields, tokens: Tokens) -> AnyElement {
    form::row(
        tr(state, "protocol"),
        None,
        tokens,
        dropdown::dropdown(&fields.protocol),
    )
}

pub fn preset_choices() -> Vec<Choice<Preset>> {
    Preset::ALL
        .into_iter()
        .map(|preset| Choice::new(preset, preset.label()))
        .collect()
}

pub fn protocol_choices() -> Vec<Choice<ProviderProtocol>> {
    ProviderProtocol::ALL
        .into_iter()
        .map(|protocol| Choice::new(protocol, protocol.label()))
        .collect()
}

/// The key field, with whether one is already stored.
fn api_key_row(
    state: &AppState,
    draft: &ProviderDraft,
    fields: &Fields,
    tokens: Tokens,
) -> AnyElement {
    // Saying so is the only way a reader can tell an empty field means
    // "unchanged" rather than "none".
    let status = if draft.has_api_key {
        tr(state, "key-stored")
    } else {
        tr(state, "key-required")
    };
    form::row(
        tr(state, "api-key"),
        Some(status),
        tokens,
        field(&fields.api_key),
    )
}

/// Discovering models, and adding one by hand.
fn model_tools(
    state: &AppState,
    draft: &ProviderDraft,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let can_discover = draft.can_discover();
    let discover = emit.clone();
    form::control_row(
        div()
            .w_full()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div().flex().justify_end().child(
                    Button::new("discover-models")
                        .label(tr(state, "discover-models"))
                        .outline()
                        .small()
                        .disabled(!can_discover)
                        .on_click(move |_, window, cx| {
                            discover(SettingsEvent::DiscoverModels, window, cx)
                        }),
                ),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .gap(px(form::INLINE_GAP))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(Input::new(&fields.model_id).w_full()),
                    )
                    .child(
                        Button::new("add-model")
                            .label(tr(state, "add-model"))
                            .outline()
                            .small()
                            .on_click(move |_, window, cx| {
                                emit(SettingsEvent::AddManualModel, window, cx)
                            }),
                    ),
            )
            .child(form::help_text(tr(state, "model-id"), tokens)),
    )
}

/// The draft's models, each removable.
fn model_list(state: &AppState, draft: &ProviderDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    if draft.models.is_empty() {
        return form::control_row(form::help_text(tr(state, "no-models"), tokens));
    }
    form::control_row(div().w_full().flex().flex_col().gap(px(2.0)).children(
        draft.models.iter().enumerate().map(|(index, model)| {
            model_row(index, model, tr(state, "remove"), tokens, emit.clone())
        }),
    ))
}

fn model_row(
    index: usize,
    model: &ProviderModel,
    remove_label: &'static str,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let id = model.id.clone();
    // The thinking levels are listed because they are the reason a model is worth
    // picking over another, and they are not inferable from the id.
    let levels = model
        .available_thinking_levels()
        .iter()
        .map(|level| level.as_str())
        .collect::<Vec<_>>()
        .join(" · ");

    div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(form::INLINE_GAP))
        .py(px(3.0))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family(font::MONO)
                        .text_size(px(text::MONO_DETAIL_SIZE))
                        .text_color(tokens.text.hsla())
                        .child(SharedString::from(model.name.clone())),
                )
                .child(
                    div()
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(SharedString::from(levels)),
                ),
        )
        .child(
            Button::new(("remove-model", index as u64))
                .icon(icons::close())
                .ghost()
                .xsmall()
                .tooltip(remove_label)
                .on_click(move |_, window, cx| {
                    emit(SettingsEvent::RemoveModel(id.clone()), window, cx)
                }),
        )
        .into_any_element()
}

/// Save, and deleting the provider being edited.
fn save_row(state: &AppState, draft: &ProviderDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    let can_save = draft.can_save(&state.provider_profiles);
    let saved = draft.id;
    let save = emit.clone();
    form::control_row(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(form::INLINE_GAP))
            .pt(px(6.0))
            .child(
                Button::new("save-provider")
                    .label(tr(state, "save-provider"))
                    .primary()
                    .small()
                    .disabled(!can_save)
                    .on_click(move |_, window, cx| save(SettingsEvent::SaveProvider, window, cx)),
            )
            .when_some(saved, |this, id| {
                this.child(
                    Button::new("delete-provider")
                        .label(tr(state, "remove"))
                        .danger()
                        .small()
                        .on_click(move |_, window, cx| {
                            emit(SettingsEvent::DeleteProvider(id), window, cx)
                        }),
                )
            })
            .when(!can_save, |this| {
                this.child(form::help_text(tr(state, "provider-incomplete"), tokens))
            }),
    )
}
