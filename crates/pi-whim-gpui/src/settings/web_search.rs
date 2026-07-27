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
    input::{Input, InputState},
};
use pi_whim_core::{
    AppState, Language, SearchEngineProfile,
    strings::{text as translate, tr},
};
use pi_whim_engine::settings::SearchEngineDraft;
use pi_whim_theme::{Tokens, font, text};

use crate::{
    icons,
    settings::{Emit, SettingsEvent, form, toggle},
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
        .child(form::group_stack(vec![
            form::group(
                tr(state, "search-engines"),
                None,
                tokens,
                vec![engine_list(state, draft, tokens, emit.clone())],
            ),
            // These fields edit whichever instance is selected above, or a new
            // one when none is; the group keeps that relationship explicit.
            form::group(
                tr(state, "search-engine-details"),
                None,
                tokens,
                vec![
                    form::row(
                        tr(state, "provider-name"),
                        None,
                        tokens,
                        Input::new(&fields.name).w_full(),
                    ),
                    form::row(
                        tr(state, "base-url"),
                        Some(tr(state, "searxng-url-help")),
                        tokens,
                        Input::new(&fields.base_url).w_full(),
                    ),
                    enabled_row(state, draft, tokens, emit.clone()),
                    save_row(state, draft, tokens, emit),
                ],
            ),
        ]))
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
    form::control_row(div().w_full().flex().flex_col().gap(px(2.0)).children(
        profiles.iter().enumerate().map(|(index, profile)| {
            engine_row(
                index,
                last,
                profile,
                draft,
                state.language,
                tokens,
                emit.clone(),
            )
        }),
    ))
}

fn engine_row(
    index: usize,
    last: usize,
    profile: &SearchEngineProfile,
    draft: &SearchEngineDraft,
    language: Language,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let selected = draft.id == Some(profile.id);
    let profile_for_select = profile.clone();
    let select = emit.clone();
    let up = emit.clone();
    let down = emit.clone();

    div()
        .w_full()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .py(px(2.0))
        .child(
            div()
                .w_full()
                .flex()
                .items_center()
                .gap(px(form::INLINE_GAP))
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
                .child(div().flex_1())
                .child(
                    Button::new(("engine-up", index as u64))
                        .icon(icons::move_up())
                        .ghost()
                        .xsmall()
                        .disabled(index == 0)
                        .tooltip(translate("move-up", language))
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
                        .icon(icons::move_down())
                        .ghost()
                        .xsmall()
                        .disabled(index == last)
                        .tooltip(translate("move-down", language))
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
                        .icon(icons::close())
                        .ghost()
                        .xsmall()
                        .tooltip(translate("remove", language))
                        .on_click(move |_, window, cx| {
                            emit(SettingsEvent::RemoveSearchEngine(index), window, cx)
                        }),
                ),
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
                .text_color(if profile.enabled {
                    tokens.muted.hsla()
                } else {
                    // A disabled instance is still listed, because its position
                    // matters again the moment it is switched back on.
                    tokens.line_strong.hsla()
                })
                .child(SharedString::from(profile.base_url.clone())),
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
        toggle::toggle(
            "search-engine-enabled",
            tr(state, "enabled"),
            enabled,
            tokens,
            move |checked, window, cx| {
                emit(SettingsEvent::SetSearchEngineEnabled(checked), window, cx)
            },
        ),
    )
}

/// Save, and testing the instance before trusting it.
fn save_row(state: &AppState, draft: &SearchEngineDraft, tokens: Tokens, emit: Emit) -> AnyElement {
    let can_save = draft.can_save();
    let test = emit.clone();
    form::control_row(
        div()
            .flex()
            .flex_wrap()
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
