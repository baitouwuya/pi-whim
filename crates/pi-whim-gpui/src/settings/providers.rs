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
    settings::{
        Emit, SettingsEvent,
        form::{self, CONTROL_WIDTH},
        segmented::{Segment, segmented},
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
        .child(form::section_header(
            tr(state, "configured-providers"),
            None,
            tokens,
        ))
        .child(provider_list(state, draft, tokens, emit.clone()))
        // Where the key ends up is worth saying next to the field that takes it:
        // a reader typing a secret into a form wants to know it is not going into
        // the database beside it.
        .child(form::section_header(
            tr(state, "connection"),
            Some(tr(state, "provider-help")),
            tokens,
        ))
        .child(preset_row(state, draft, tokens, emit.clone()))
        .child(name_row(state, fields, tokens, collides))
        .child(form::row(
            tr(state, "base-url"),
            None,
            tokens,
            field(&fields.base_url),
        ))
        .child(protocol_row(state, draft, tokens, emit.clone()))
        .child(api_key_row(state, draft, fields, tokens))
        .child(form::section_header(
            tr(state, "models"),
            Some(tr(state, "models-help")),
            tokens,
        ))
        .child(model_tools(state, draft, fields, tokens, emit.clone()))
        .child(model_list(state, draft, tokens, emit.clone()))
        .child(save_row(state, draft, tokens, emit))
        .into_any_element()
}

/// A text field filling the control column.
fn field(state: &Entity<InputState>) -> AnyElement {
    Input::new(state).into_any_element()
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
                .flex()
                .items_center()
                .gap(px(form::INLINE_GAP))
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
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(rows)
            .when(empty, |this| {
                this.child(form::help_text(tr(state, "no-providers"), tokens))
            })
            .child(
                div().pt(px(6.0)).child(
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

fn preset_row(state: &AppState, draft: &ProviderDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    form::row(
        tr(state, "preset"),
        Some(tr(state, "preset-help")),
        tokens,
        div()
            .flex()
            .flex_wrap()
            .gap(px(4.0))
            .children(Preset::ALL.into_iter().enumerate().map(|(index, preset)| {
                let active = draft.preset == preset;
                let emit = emit.clone();
                Button::new(("preset", index as u64))
                    .label(preset.label())
                    .when(active, |button| button.primary())
                    .when(!active, |button| button.outline())
                    .small()
                    .on_click(move |_, window, cx| {
                        emit(SettingsEvent::SelectPreset(preset), window, cx)
                    })
            })),
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

fn protocol_row(state: &AppState, draft: &ProviderDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    form::row(
        tr(state, "protocol"),
        None,
        tokens,
        segmented(
            "provider-protocol",
            draft.protocol,
            ProviderProtocol::ALL
                .into_iter()
                .map(|protocol| Segment::new(protocol, protocol.label()))
                .collect(),
            tokens,
            move |protocol, window, cx| emit(SettingsEvent::SetProtocol(protocol), window, cx),
        ),
    )
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
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                Button::new("discover-models")
                    .label(tr(state, "discover-models"))
                    .outline()
                    .small()
                    .disabled(!can_discover)
                    .on_click(move |_, window, cx| {
                        discover(SettingsEvent::DiscoverModels, window, cx)
                    }),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(form::INLINE_GAP))
                    .child(
                        div()
                            .w(px(CONTROL_WIDTH - 100.0))
                            .child(Input::new(&fields.model_id)),
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
    form::control_row(
        div().flex().flex_col().gap(px(2.0)).children(
            draft
                .models
                .iter()
                .enumerate()
                .map(|(index, model)| model_row(index, model, tokens, emit.clone())),
        ),
    )
}

fn model_row(index: usize, model: &ProviderModel, tokens: Tokens, emit: Emit) -> AnyElement {
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
        .flex()
        .items_center()
        .gap(px(form::INLINE_GAP))
        .py(px(3.0))
        .child(
            div()
                .flex_1()
                .flex()
                .flex_col()
                .child(
                    div()
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
                .label("×")
                .ghost()
                .xsmall()
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
