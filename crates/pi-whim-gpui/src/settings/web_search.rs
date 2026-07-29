//! Web search settings: which SearXNG instances answer, and in what order.
//!
//! The page is deliberately list-first. Details only appear in a dialog after
//! adding an engine or opening a stored row, so an empty form never competes
//! with the collection it edits.

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
    AppState, Language, SearchEngineKind, SearchEngineProfile,
    strings::{text as translate, tr},
};
use pi_whim_engine::settings::SearchEngineDraft;
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
const ENGINE_ROW_HEIGHT: f32 = 54.0;
const STATUS_WIDTH: f32 = 76.0;

/// The typed fields in the add/edit dialog.
pub struct Fields {
    pub name: Entity<InputState>,
    pub kind: Entity<ChoiceState<SearchEngineKind>>,
    pub base_url: Entity<InputState>,
    /// Masked and never seeded from Keychain.
    pub api_key: Entity<InputState>,
}

/// Build the list-first Web search page.
pub fn render(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let add = emit.clone();

    div()
        .w_full()
        .flex()
        .flex_col()
        .child(form::page_header(
            tr(state, "web-search"),
            Some(tr(state, "web-search-help")),
            tokens,
        ))
        .child(
            div()
                .w_full()
                .flex()
                .flex_col()
                .pt(px(form::PAGE_GROUP_GAP))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .items_start()
                        .justify_between()
                        .child(div().flex_1().child(form::section_header(
                            tr(state, "search-engines"),
                            None,
                            tokens,
                        )))
                        .child(
                            Button::new("add-search-engine")
                                .icon(icons::add())
                                .ghost()
                                .xsmall()
                                .tooltip(tr(state, "add-search-engine"))
                                .on_click(move |_, window, cx| {
                                    add(SettingsEvent::NewSearchEngine, window, cx)
                                }),
                        ),
                )
                .child(engine_list(state, tokens, emit)),
        )
        .into_any_element()
}

/// The stored instances, in the order they are asked.
fn engine_list(state: &AppState, tokens: Tokens, emit: Emit) -> AnyElement {
    let profiles = &state.search_engine_profiles;
    if profiles.is_empty() {
        return div()
            .w_full()
            .min_h(px(76.0))
            .flex()
            .items_center()
            .justify_center()
            .border_t_1()
            .border_b_1()
            .border_color(tokens.line.hsla())
            .child(form::help_text(tr(state, "no-search-engines"), tokens))
            .into_any_element();
    }

    let last = profiles.len() - 1;
    div()
        .w_full()
        .flex()
        .flex_col()
        .border_t_1()
        .border_color(tokens.line.hsla())
        .children(profiles.iter().enumerate().map(|(index, profile)| {
            engine_row(index, last, profile, state.language, tokens, emit.clone())
        }))
        .into_any_element()
}

fn engine_row(
    index: usize,
    last: usize,
    profile: &SearchEngineProfile,
    language: Language,
    tokens: Tokens,
    emit: Emit,
) -> AnyElement {
    let profile_for_select = profile.clone();
    let select = emit.clone();
    let up = emit.clone();
    let down = emit.clone();
    let enabled = profile.enabled;
    let status = translate(if enabled { "enabled" } else { "disabled" }, language);
    let details = format!(
        "{}  |  {}",
        translate(kind_string(profile.kind), language),
        profile.base_url
    );

    div()
        .id(("search-engine-row", index))
        .w_full()
        .h(px(ENGINE_ROW_HEIGHT))
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
                .child(div().size(px(6.0)).rounded(px(radius::DOT)).bg(if enabled {
                    tokens.success.hsla()
                } else {
                    tokens.line_strong.hsla()
                }))
                .child(
                    div()
                        .font_family(font::MONO)
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(status),
                ),
        )
        // The action strip stops bubbling so a reorder or delete never opens
        // the editor underneath the pointer. This also covers disabled arrows.
        .child(
            div()
                .id(("search-engine-actions", index))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(2.0))
                .on_click(|_, _, cx| cx.stop_propagation())
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
        .on_click(move |_, window, cx| {
            select(
                SettingsEvent::SelectSearchEngine(profile_for_select.clone()),
                window,
                cx,
            )
        })
        .into_any_element()
}

/// The shared add/edit dialog. The same draft and backend requests power both
/// paths; only its title changes for a stored profile.
pub fn render_editor(
    state: &AppState,
    draft: &SearchEngineDraft,
    fields: &Fields,
    tokens: Tokens,
    emit: Emit,
    cx: &mut App,
) -> AnyElement {
    let can_save = draft.can_save();
    let title = tr(
        state,
        if draft.id.is_some() {
            "edit-search-engine"
        } else {
            "add-search-engine"
        },
    );
    let test = emit.clone();
    let cancel_button = emit.clone();
    let save_button = emit.clone();
    let confirm = emit.clone();
    let cancel = emit.clone();
    let endpoint_label = tr(
        state,
        if draft.kind == SearchEngineKind::Searxng {
            "base-url"
        } else {
            "endpoint-url"
        },
    );
    let endpoint_help = tr(
        state,
        if draft.kind == SearchEngineKind::Searxng {
            "searxng-url-help"
        } else {
            "doubao-url-help"
        },
    );

    let footer = div()
        .w_full()
        .flex()
        .items_center()
        .justify_between()
        .child(
            Button::new("test-search-engine")
                .label(tr(state, "test-search-engine"))
                .outline()
                .small()
                .disabled(!can_save)
                .on_click(move |_, window, cx| test(SettingsEvent::TestSearchEngine, window, cx)),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(form::INLINE_GAP))
                .child(
                    Button::new("cancel-search-engine")
                        .label(tr(state, "cancel"))
                        .outline()
                        .small()
                        .on_click(move |_, window, cx| {
                            cancel_button(SettingsEvent::CloseSearchEngineEditor, window, cx)
                        }),
                )
                .child(
                    Button::new("save-search-engine")
                        .label(tr(state, "save-search-engine"))
                        .primary()
                        .small()
                        .disabled(!can_save)
                        .on_click(move |_, window, cx| {
                            save_button(SettingsEvent::SaveSearchEngine, window, cx)
                        }),
                ),
        );

    div()
        .child(
            Dialog::new(cx)
                .w(px(EDITOR_WIDTH))
                .title(SharedString::from(title))
                .footer(footer)
                .on_ok(move |_, window, cx| {
                    if can_save {
                        confirm(SettingsEvent::SaveSearchEngine, window, cx);
                    }
                    // The host closes the editor after SQLite and Keychain both
                    // succeed. Keep it open so a failed credential write does
                    // not discard the values the reader just entered.
                    false
                })
                .on_cancel(move |_, window, cx| {
                    cancel(SettingsEvent::CloseSearchEngineEditor, window, cx);
                    true
                })
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_col()
                        .child(form::row(
                            tr(state, "search-engine-type"),
                            None,
                            tokens,
                            dropdown::dropdown(&fields.kind),
                        ))
                        .child(form::row(
                            tr(state, "provider-name"),
                            None,
                            tokens,
                            Input::new(&fields.name).w_full(),
                        ))
                        .child(form::row(
                            endpoint_label,
                            Some(endpoint_help),
                            tokens,
                            Input::new(&fields.base_url).w_full(),
                        ))
                        .when(draft.kind.requires_api_key(), |this| {
                            this.child(api_key_row(state, draft, fields, tokens))
                        })
                        .child(enabled_row(state, draft, tokens, emit))
                        .when(!can_save, |this| {
                            this.child(form::control_row(form::field_error(
                                tr(state, "search-engine-incomplete"),
                                tokens,
                            )))
                        }),
                ),
        )
        .into_any_element()
}

fn kind_string(kind: SearchEngineKind) -> &'static str {
    match kind {
        SearchEngineKind::Searxng => "search-engine-searxng",
        SearchEngineKind::DoubaoGlobal => "search-engine-doubao-global",
    }
}

pub fn kind_choices(state: &AppState) -> Vec<Choice<SearchEngineKind>> {
    [SearchEngineKind::Searxng, SearchEngineKind::DoubaoGlobal]
        .into_iter()
        .map(|kind| Choice::new(kind, tr(state, kind_string(kind))))
        .collect()
}

fn api_key_row(
    state: &AppState,
    draft: &SearchEngineDraft,
    fields: &Fields,
    tokens: Tokens,
) -> AnyElement {
    let status = if draft.has_api_key {
        tr(state, "key-stored")
    } else {
        tr(state, "key-required")
    };
    form::row(
        tr(state, "api-key"),
        Some(status),
        tokens,
        Input::new(&fields.api_key).w_full(),
    )
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
