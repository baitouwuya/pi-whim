//! Web search settings: which SearXNG instances answer, and in what order.
//!
//! Order is the point of the list — it decides which instance is asked first —
//! so each row carries move-up and move-down beside its delete. The list
//! operations themselves are `engine::settings`, which renumbers positions so a
//! reload preserves what was arranged here.

use gpui::{
    AnyElement, Entity, IntoElement, ParentElement, SharedString, Styled, div,
    prelude::FluentBuilder, px,
};
use gpui_component::{
    Disableable, Sizable,
    button::{Button, ButtonVariants},
    checkbox::Checkbox,
    input::{Input, InputState},
};
use pi_whim_core::{AppState, SearchEngineProfile, strings::tr};
use pi_whim_engine::settings::SearchEngineDraft;
use pi_whim_theme::{Tokens, font, text};

use crate::{
    settings::{
        Emit, SettingsEvent,
        form::{self, CONTROL_WIDTH},
    },
    theme::IntoHsla,
};

/// The typed fields on this page.
pub struct Fields {
    pub name: Entity<InputState>,
    pub base_url: Entity<InputState>,
}

/// Build the Web search page.
pub fn render(
    state: &AppState,
    draft: &SearchEngineDraft,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .child(form::page_header(
            tr(state, "web-search"),
            Some(tr(state, "web-search-help")),
            tokens,
        ))
        .child(form::section_header(
            tr(state, "search-engines"),
            None,
            tokens,
        ))
        .child(engine_list(state, draft, tokens, emit.clone()))
        .child(form::row(
            tr(state, "provider-name"),
            None,
            tokens,
            div().w(px(CONTROL_WIDTH)).child(Input::new(&fields.name)),
        ))
        .child(form::row(
            tr(state, "base-url"),
            Some(tr(state, "searxng-url-help")),
            tokens,
            div()
                .w(px(CONTROL_WIDTH))
                .child(Input::new(&fields.base_url)),
        ))
        .child(enabled_row(state, draft, tokens, emit.clone()))
        .child(save_row(state, draft, tokens, emit))
        .into_any_element()
}

/// The stored instances, in the order they are asked.
fn engine_list(
    state: &AppState,
    draft: &SearchEngineDraft,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let profiles = &state.search_engine_profiles;
    if profiles.is_empty() {
        return form::control_row(form::help_text(tr(state, "no-search-engines"), tokens));
    }
    let last = profiles.len() - 1;
    form::control_row(
        div().flex().flex_col().gap(px(2.0)).children(
            profiles.iter().enumerate().map(|(index, profile)| {
                engine_row(index, last, profile, draft, tokens, emit.clone())
            }),
        ),
    )
}

fn engine_row(
    index: usize,
    last: usize,
    profile: &SearchEngineProfile,
    draft: &SearchEngineDraft,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let selected = draft.id == Some(profile.id);
    let profile_for_select = profile.clone();
    let select = emit.clone();
    let up = emit.clone();
    let down = emit.clone();

    div()
        .flex()
        .items_center()
        .gap(px(form::INLINE_GAP))
        .py(px(2.0))
        .child(
            Button::new(("engine", index as u64))
                .label(SharedString::from(profile.name.clone()))
                .when(selected, |button| button.primary())
                .when(!selected, |button| button.ghost())
                .small()
                .on_click(move |_, window, cx| {
                    select(
                        SettingsEvent::SelectSearchEngine(profile_for_select.clone()),
                        window,
                        cx,
                    )
                }),
        )
        .child(
            div()
                .flex_1()
                .font_family(font::MONO)
                .text_size(px(text::LABEL_SIZE))
                .text_color(if profile.enabled {
                    tokens.muted.hsla()
                } else {
                    // A disabled instance is still listed, because its position
                    // matters again the moment it is switched back on.
                    tokens.line_strong.hsla()
                })
                .child(SharedString::from(profile.base_url.clone())),
        )
        // Disabled at the ends rather than wrapping: a list that jumps from top
        // to bottom under one click is hard to aim.
        .child(
            Button::new(("engine-up", index as u64))
                .label("↑")
                .ghost()
                .xsmall()
                .disabled(index == 0)
                .on_click(move |_, window, cx| {
                    up(
                        SettingsEvent::MoveSearchEngine { index, delta: -1 },
                        window,
                        cx,
                    )
                }),
        )
        .child(
            Button::new(("engine-down", index as u64))
                .label("↓")
                .ghost()
                .xsmall()
                .disabled(index == last)
                .on_click(move |_, window, cx| {
                    down(
                        SettingsEvent::MoveSearchEngine { index, delta: 1 },
                        window,
                        cx,
                    )
                }),
        )
        .child(
            Button::new(("engine-remove", index as u64))
                .label("×")
                .ghost()
                .xsmall()
                .on_click(move |_, window, cx| {
                    emit(SettingsEvent::RemoveSearchEngine(index), window, cx)
                }),
        )
        .into_any_element()
}

fn enabled_row(
    state: &AppState,
    draft: &SearchEngineDraft,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let enabled = draft.enabled;
    form::row(
        tr(state, "enabled"),
        None,
        tokens,
        Checkbox::new("search-engine-enabled")
            .label(tr(state, "enabled"))
            .checked(enabled)
            .on_click(move |_, window, cx| {
                emit(SettingsEvent::SetSearchEngineEnabled(!enabled), window, cx)
            }),
    )
}

/// Save, and testing the instance before trusting it.
fn save_row(state: &AppState, draft: &SearchEngineDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    let can_save = draft.can_save();
    let test = emit.clone();
    form::control_row(
        div()
            .flex()
            .items_center()
            .gap(px(form::INLINE_GAP))
            .pt(px(6.0))
            .child(
                Button::new("save-search-engine")
                    .label(tr(state, "save-search-engine"))
                    .primary()
                    .small()
                    .disabled(!can_save)
                    .on_click(move |_, window, cx| {
                        emit(SettingsEvent::SaveSearchEngine, window, cx)
                    }),
            )
            .child(
                // Worth its own button: a URL that is reachable but not a SearXNG
                // instance fails at search time, far from where it was entered.
                Button::new("test-search-engine")
                    .label(tr(state, "test-search-engine"))
                    .outline()
                    .small()
                    .disabled(!can_save)
                    .on_click(move |_, window, cx| {
                        test(SettingsEvent::TestSearchEngine, window, cx)
                    }),
            )
            .when(!can_save, |this| {
                this.child(form::help_text(
                    tr(state, "search-engine-incomplete"),
                    tokens,
                ))
            }),
    )
}
