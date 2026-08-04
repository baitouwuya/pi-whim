//! Provider settings: which endpoints answer, and with which models.
//!
//! The draft, its validation, and the presets all live in
//! `engine::settings::ProviderDraft`. What is here is the arrangement and the
//! parts that need an `InputState`.

use gpui::{
    AnyElement, App, Entity, InteractiveElement, IntoElement, ParentElement, ScrollHandle,
    SharedString, StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, Icon, Sizable,
    button::{Button, ButtonVariants},
    dialog::Dialog,
    input::{Input, InputState},
};
use pi_whim_core::{AppState, ProviderModel, ProviderProtocol, strings::tr};
use pi_whim_engine::settings::{ModelConfigDraft, Preset, ProviderDraft};
use pi_whim_theme::{Tokens, font, radius, text};

use crate::{
    elements::isolated_vertical_scroll_area_with_scrollbar,
    icons,
    settings::{
        Emit, SettingsEvent,
        dropdown::{self, Choice, ChoiceState},
        form,
    },
    theme::IntoHsla,
};

/// Cards wrap at these widths, giving the normal settings measure three
/// provider columns and two wider model columns without a breakpoint system.
const PROVIDER_CARD_MIN_WIDTH: f32 = 210.0;
const MODEL_CELL_MIN_WIDTH: f32 = 300.0;
const CARD_HEIGHT: f32 = 64.0;
const MODEL_GRID_MAX_HEIGHT: f32 = 316.0;

/// The typed fields on this page.
pub struct Fields {
    pub name: Entity<InputState>,
    pub base_url: Entity<InputState>,
    /// The key field. Masked, and never filled from the keychain.
    pub api_key: Entity<InputState>,
    pub model_id: Entity<InputState>,
    pub preset: Entity<ChoiceState<Preset>>,
    pub protocol: Entity<ChoiceState<ProviderProtocol>>,
    /// Model config dialog fields.
    pub model_config_protocol: Entity<ChoiceState<ProviderProtocol>>,
    pub model_config_context_window: Entity<InputState>,
}

/// Build the Providers page.
pub fn render(
    state: &AppState,
    draft: &ProviderDraft,
    fields: &Fields,
    model_scroll: &ScrollHandle,
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
                vec![provider_grid(state, draft, tokens, emit.clone())],
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
                    model_grid(state, draft, model_scroll, tokens, emit.clone()),
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

/// Stored providers are peers, so they use the whole content measure rather
/// than being squeezed into the form's right-hand control column.
fn provider_grid(
    state: &AppState,
    draft: &ProviderDraft,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let cards: Vec<AnyElement> = state
        .provider_profiles
        .iter()
        .map(|profile| {
            let selected = draft.id == Some(profile.id);
            let id = profile.id;
            let emit = emit.clone();
            let subtitle = format!(
                "{}  ·  {} {}",
                profile.protocol.label(),
                profile.models.len(),
                tr(state, "models")
            );
            Button::new(profile.id)
                .w_full()
                .min_w(px(PROVIDER_CARD_MIN_WIDTH))
                .flex_1()
                .flex_basis(px(PROVIDER_CARD_MIN_WIDTH))
                .h(px(CARD_HEIGHT))
                .px(px(10.0))
                .border_1()
                .border_color(if selected {
                    tokens.accent_border_strong().hsla()
                } else {
                    tokens.line.hsla()
                })
                .bg(if selected {
                    tokens.accent_surface_subtle().hsla()
                } else {
                    tokens.control_background().hsla()
                })
                .child(
                    div()
                        .w_full()
                        .min_w_0()
                        .flex()
                        .items_center()
                        .gap(px(9.0))
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
                                        .text_color(tokens.text.hsla())
                                        .child(SharedString::from(profile.name.clone())),
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
                                        .child(SharedString::from(subtitle)),
                                ),
                        ),
                )
                .on_click(move |_, window, cx| emit(SettingsEvent::SelectProvider(id), window, cx))
                .into_any_element()
        })
        .collect();

    let empty = cards.is_empty();
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .gap(px(8.0))
        .children(cards)
        .when(empty, |this| {
            this.child(
                div()
                    .flex_1()
                    .min_w(px(PROVIDER_CARD_MIN_WIDTH))
                    .flex_basis(px(PROVIDER_CARD_MIN_WIDTH))
                    .h(px(CARD_HEIGHT))
                    .flex()
                    .items_center()
                    .px(px(12.0))
                    .border_1()
                    .border_color(tokens.line.hsla())
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .child(tr(state, "no-providers")),
            )
        })
        .child(
            Button::new("add-provider")
                .w_full()
                .min_w(px(PROVIDER_CARD_MIN_WIDTH))
                .flex_1()
                .flex_basis(px(PROVIDER_CARD_MIN_WIDTH))
                .h(px(CARD_HEIGHT))
                .icon(icons::add())
                .label(tr(state, "add-provider"))
                .outline()
                .on_click(move |_, window, cx| emit(SettingsEvent::NewProvider, window, cx)),
        )
        .into_any_element()
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
    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(7.0))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .justify_between()
                .child(form::help_text(
                    format!("{} · {}", tr(state, "models"), draft.models.len()),
                    tokens,
                ))
                .child(
                    Button::new("discover-models")
                        .icon(icons::search())
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
        .child(form::help_text(tr(state, "model-id"), tokens))
        .into_any_element()
}

/// Models use a wrapping two-column table: each cell keeps the same internal
/// columns, while the outer grid contracts to one column on a narrow window.
fn model_grid(
    state: &AppState,
    draft: &ProviderDraft,
    scroll: &ScrollHandle,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    if draft.models.is_empty() {
        return div()
            .w_full()
            .min_h(px(58.0))
            .flex()
            .items_center()
            .justify_center()
            .border_1()
            .border_color(tokens.line.hsla())
            .bg(tokens.control_background().hsla())
            .child(form::help_text(tr(state, "no-models"), tokens))
            .into_any_element();
    }
    let content = div()
        .w_full()
        .pr(px(12.0))
        .flex()
        .flex_wrap()
        .gap(px(1.0))
        .p(px(1.0))
        .bg(tokens.line.hsla())
        .children(
            draft
                .models
                .iter()
                .map(|model| model_cell(model, state, tokens, emit.clone())),
        );
    isolated_vertical_scroll_area_with_scrollbar(
        "provider-model-grid",
        "provider-model-grid-scrollbar",
        scroll,
        px(MODEL_GRID_MAX_HEIGHT),
        content,
    )
    .into_any_element()
}

fn model_cell(model: &ProviderModel, state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let id = model.id.clone();
    let id2 = model.id.clone();
    let configure = emit.clone();
    // The thinking levels are listed because they are the reason a model is worth
    // picking over another, and they are not inferable from the id.
    let levels = model
        .available_thinking_levels()
        .iter()
        .map(|level| level.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let capabilities = match (model.reasoning, model.supports_images) {
        (true, true) => format!(
            "{} · {} · {levels}",
            tr(state, "reasoning"),
            tr(state, "vision")
        ),
        (true, false) => format!("{} · {levels}", tr(state, "reasoning")),
        (false, true) => tr(state, "vision").to_owned(),
        (false, false) => tr(state, "basic-model").to_owned(),
    };
    let details = if model.name == model.id {
        capabilities
    } else {
        format!("{} · {capabilities}", model.id)
    };

    div()
        .id(SharedString::from(model.id.clone()))
        .min_w(px(MODEL_CELL_MIN_WIDTH))
        .flex_1()
        .flex_basis(px(MODEL_CELL_MIN_WIDTH))
        .h(px(62.0))
        .flex()
        .items_center()
        .gap(px(form::INLINE_GAP))
        .px(px(10.0))
        .bg(tokens.panel.hsla())
        .hover(move |this| this.bg(tokens.control_background_hover().hsla()))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            configure(SettingsEvent::ConfigureModel(id.clone()), window, cx)
        })
        .child(
            div()
                .size(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(radius::DOT))
                .bg(tokens.accent_surface_faint().hsla())
                .child(
                    Icon::new(icons::model())
                        .size(px(13.0))
                        .text_color(tokens.accent.hsla()),
                ),
        )
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
                .id(SharedString::from(format!("remove:{}", model.id)))
                .flex_none()
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    Button::new(SharedString::from(format!("remove-model:{}", model.id)))
                        .icon(icons::close())
                        .ghost()
                        .xsmall()
                        .tooltip(tr(state, "remove"))
                        .on_click(move |_, window, cx| {
                            emit(SettingsEvent::RemoveModel(id2.clone()), window, cx)
                        }),
                ),
        )
        .into_any_element()
}

/// Save, and deleting the provider being edited.
fn save_row(state: &AppState, draft: &ProviderDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    let can_save = draft.can_save(&state.provider_profiles);
    let saved = draft.id;
    let save = emit.clone();
    div()
        .w_full()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .gap(px(form::INLINE_GAP))
        .pt(px(8.0))
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
        })
        .into_any_element()
}

/// Show the model config dialog.
pub fn render_model_config(
    state: &AppState,
    config: &ModelConfigDraft,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
    cx: &mut App,
) -> AnyElement {
    let save = emit.clone();
    let cancel_btn = emit.clone();
    let cancel_ok = emit.clone();
    let cancel_close = emit.clone();

    div()
        .child(
            Dialog::new(cx)
                .w(px(480.0))
                .title(SharedString::from(format!(
                    "{}: {}",
                    tr(state, "model"),
                    config.model_id
                )))
                .footer(
                    div()
                        .flex()
                        .items_center()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            Button::new("cancel-model-config")
                                .label(tr(state, "cancel"))
                                .outline()
                                .small()
                                .on_click(move |_, window, cx| {
                                    cancel_btn(SettingsEvent::CloseModelConfig, window, cx)
                                }),
                        )
                        .child(
                            Button::new("save-model-config")
                                .label(tr(state, "save"))
                                .primary()
                                .small()
                                .on_click(move |_, window, cx| {
                                    save(SettingsEvent::SaveModelConfig, window, cx)
                                }),
                        ),
                )
                .on_ok(move |_, window, cx| {
                    cancel_ok(SettingsEvent::CloseModelConfig, window, cx);
                    false
                })
                .on_cancel(move |_, window, cx| {
                    cancel_close(SettingsEvent::CloseModelConfig, window, cx);
                    true
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(form::row(
                            tr(state, "protocol"),
                            None,
                            tokens,
                            dropdown::dropdown(&fields.model_config_protocol),
                        ))
                        .child(form::row(
                            tr(state, "context-window"),
                            None,
                            tokens,
                            div()
                                .w(px(180.0))
                                .child(Input::new(&fields.model_config_context_window).w_full()),
                        )),
                ),
        )
        .into_any_element()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_settings_width_fits_dense_provider_and_model_grids() {
        // The settings body applies 28px on each side inside its width cap.
        let usable = form::CONTENT_WIDTH - 56.0;
        assert!(usable >= PROVIDER_CARD_MIN_WIDTH * 3.0 + 16.0);
        assert!(usable >= MODEL_CELL_MIN_WIDTH * 2.0 + 1.0);
    }
}
