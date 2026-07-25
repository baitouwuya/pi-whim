//! The settings page.
//!
//! One view owning the drafts and every `InputState`, and three render functions
//! over it. The sections are functions rather than entities because none of them
//! has state of its own — the drafts are shared, and a provider's name field has
//! to survive a switch to the web-search section and back.
//!
//! Validation is not here. `engine::settings` decides whether a draft can be
//! saved and what a preset fills in; this module asks and arranges.

pub mod form;
pub mod general;
pub mod providers;
pub mod segmented;
pub mod web_search;

use std::rc::Rc;

use gpui::{
    App, AppContext, Context, EventEmitter, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    Sizable,
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
};
use pi_whim_core::{
    AgentTeamConfig, AppState, BashPolicy, Language, MAX_AGENT_DEPTH, MAX_AGENTS_PER_LEVEL,
    ProviderId, ProviderModel, ProviderProtocol, QueueMode, SearchEngineProfile, strings::tr,
};
use pi_whim_engine::settings::{
    Preset, ProviderDraft, SearchEngineDraft, Section, move_search_engine, remove_search_engine,
    upsert_search_engine,
};
use pi_whim_theme::{Tokens, font, layout, text};

use crate::theme::IntoHsla;

/// Width of the section list.
///
/// Matches the sidebar so the window's left edge does not shift when the page
/// changes.
pub const NAV_WIDTH: f32 = layout::SIDEBAR_WIDTH;

/// How a row reports a change.
///
/// An `Rc` rather than a generic closure so the render functions can be plain
/// functions returning `AnyElement` — a section builds dozens of handlers, and
/// each one needs its own clone.
pub type Emit = Rc<dyn Fn(SettingsEvent, &mut Window, &mut App)>;

/// What the settings page asks the shell to do.
///
/// Split between changes that only touch domain state and ones that need the
/// store, the keychain, or the network. The shell sorts them out; the page does
/// not know which is which.
#[derive(Clone, Debug, PartialEq)]
pub enum SettingsEvent {
    /// Leave settings.
    Close,
    /// Show a different section.
    Show(Section),

    SetLanguage(Language),
    SetAutoCompaction(bool),
    SetBashPolicy(BashPolicy),
    SetBlockedPatterns(Vec<String>),
    SetAgentTeamConfig(AgentTeamConfig),
    SetQueueModes {
        steering: QueueMode,
        follow_up: QueueMode,
    },

    /// Edit a stored provider.
    SelectProvider(ProviderId),
    /// Start a provider that has not been saved.
    NewProvider,
    SelectPreset(Preset),
    SetProtocol(ProviderProtocol),
    /// Add the model id typed into the field.
    AddManualModel,
    RemoveModel(String),
    /// Ask the provider what models it has.
    DiscoverModels,
    /// Store the draft, and its key if one was typed.
    SaveProvider,
    DeleteProvider(ProviderId),

    SelectSearchEngine(SearchEngineProfile),
    SetSearchEngineEnabled(bool),
    /// Store the whole list, which is how a reorder or a delete is saved too.
    SaveSearchEngines(Vec<SearchEngineProfile>),
    SaveSearchEngine,
    TestSearchEngine,
    RemoveSearchEngine(usize),
    MoveSearchEngine {
        index: usize,
        delta: isize,
    },
}

/// The settings page.
pub struct Settings {
    section: Section,
    tokens: Tokens,
    /// A copy of state, refreshed by `sync`.
    ///
    /// Held rather than borrowed because the render functions need it alongside
    /// the drafts, and the shell owns the real one.
    state: AppState,
    provider: ProviderDraft,
    search_engine: SearchEngineDraft,
    general_fields: general::Fields,
    provider_fields: providers::Fields,
    search_fields: web_search::Fields,
}

impl EventEmitter<SettingsEvent> for Settings {}

impl Settings {
    pub fn new(
        tokens: Tokens,
        state: AppState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let general_fields = general::Fields {
            blocked_patterns: cx.new(|cx| {
                InputState::new(window, cx)
                    // Multi-line, because the patterns are a list and a
                    // comma-separated single line hides which one is malformed.
                    .multi_line(true)
                    .placeholder("rm -rf /")
            }),
            max_depth: cx.new(|cx| {
                InputState::new(window, cx)
                    // Clamped at the control: a value the domain would reject
                    // should not be typeable.
                    .min(1.0)
                    .max(MAX_AGENT_DEPTH as f64)
            }),
            max_agents_per_level: cx.new(|cx| {
                InputState::new(window, cx)
                    .min(1.0)
                    .max(MAX_AGENTS_PER_LEVEL as f64)
            }),
        };
        let provider_fields = providers::Fields {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("OpenAI")),
            base_url: cx
                .new(|cx| InputState::new(window, cx).placeholder("https://api.openai.com/v1")),
            api_key: cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-…")),
            model_id: cx.new(|cx| InputState::new(window, cx).placeholder("gpt-5")),
        };
        let search_fields = web_search::Fields {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("SearXNG")),
            base_url: cx
                .new(|cx| InputState::new(window, cx).placeholder("https://search.example")),
        };

        // The blocked-pattern field has no Save button of its own: it belongs to a
        // section of toggles that each apply immediately, and one field that
        // needed confirming would be the odd one out.
        cx.subscribe_in(
            &general_fields.blocked_patterns,
            window,
            |_, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let patterns = general::parse_blocked_patterns(&input.read(cx).value());
                    cx.emit(SettingsEvent::SetBlockedPatterns(patterns));
                }
            },
        )
        .detach();

        // The two numeric fields fold into one domain value, so each reports the
        // whole config with its own component replaced. `normalized` clamps, which
        // matters for the moment mid-edit when the field is empty or out of range.
        cx.subscribe_in(
            &general_fields.max_depth,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change)
                    && let Ok(depth) = input.read(cx).value().parse::<u8>()
                {
                    let config = AgentTeamConfig {
                        max_depth: depth,
                        ..settings.state.agent_team_config.clone()
                    };
                    cx.emit(SettingsEvent::SetAgentTeamConfig(config.normalized()));
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &general_fields.max_agents_per_level,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change)
                    && let Ok(agents) = input.read(cx).value().parse::<u16>()
                {
                    let config = AgentTeamConfig {
                        max_agents_per_level: agents,
                        ..settings.state.agent_team_config.clone()
                    };
                    cx.emit(SettingsEvent::SetAgentTeamConfig(config.normalized()));
                }
            },
        )
        .detach();

        // The provider and search-engine drafts are edited into, then saved, so
        // their fields write into the draft rather than emitting.
        Self::watch_draft_fields(&provider_fields, &search_fields, window, cx);

        let mut settings = Self {
            section: Section::default(),
            tokens,
            state,
            provider: ProviderDraft::default(),
            search_engine: SearchEngineDraft::default(),
            general_fields,
            provider_fields,
            search_fields,
        };
        settings.seed_fields(window, cx);
        settings
    }

    /// Keep the draft in step with what is typed.
    fn watch_draft_fields(
        provider: &providers::Fields,
        search: &web_search::Fields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(
            &provider.name,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.provider.name = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &provider.base_url,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.provider.base_url = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &provider.api_key,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.provider.api_key = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &provider.model_id,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.provider.manual_model_id = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &search.name,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.search_engine.name = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &search.base_url,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.search_engine.base_url = input.read(cx).value().to_string();
                    cx.notify();
                }
            },
        )
        .detach();
    }

    pub fn section(&self) -> Section {
        self.section
    }

    pub fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// Refresh from state, and re-seed the fields whose value it owns.
    pub fn sync(&mut self, state: &AppState, window: &mut Window, cx: &mut Context<Self>) {
        let language_changed = self.state.language != state.language;
        let patterns_changed = self.state.bash_blocked_patterns != state.bash_blocked_patterns;
        let team_changed = self.state.agent_team_config != state.agent_team_config;
        self.state = state.clone();

        if patterns_changed || team_changed || language_changed {
            self.seed_general_fields(window, cx);
        }
        cx.notify();
    }

    /// Point the provider draft at a stored profile.
    pub fn edit_provider(&mut self, id: ProviderId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self
            .state
            .provider_profiles
            .iter()
            .find(|profile| profile.id == id)
        else {
            return;
        };
        self.provider = ProviderDraft::from_profile(profile);
        self.seed_provider_fields(window, cx);
    }

    /// Start a provider that has not been saved.
    pub fn new_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider = ProviderDraft::default();
        self.seed_provider_fields(window, cx);
    }

    /// Apply a preset to the draft.
    pub fn apply_preset(&mut self, preset: Preset, window: &mut Window, cx: &mut Context<Self>) {
        self.provider.preset = preset;
        preset.apply(&mut self.provider);
        self.seed_provider_fields(window, cx);
    }

    /// Change the draft's protocol, moving a default base URL with it.
    pub fn set_protocol(
        &mut self,
        protocol: ProviderProtocol,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.provider.set_protocol(protocol);
        self.seed_provider_fields(window, cx);
    }

    /// Add the typed model id to the draft.
    pub fn add_manual_model(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider.add_manual_model();
        self.seed_provider_fields(window, cx);
    }

    pub fn remove_model(&mut self, id: &str, cx: &mut Context<Self>) {
        self.provider.models.retain(|model| model.id != id);
        cx.notify();
    }

    /// Replace the draft's models with what discovery found.
    pub fn set_discovered_models(&mut self, models: Vec<ProviderModel>, cx: &mut Context<Self>) {
        self.provider.models = models;
        cx.notify();
    }

    /// Reflect whether a key is actually in the keychain.
    ///
    /// Driven by a verified result rather than by the field having text in it, so
    /// a failed keychain write does not read as a stored key.
    pub fn set_provider_key_status(
        &mut self,
        id: ProviderId,
        saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.provider.id != Some(id) {
            return;
        }
        self.provider.has_api_key = saved;
        if saved {
            self.provider.api_key.clear();
            self.provider_fields
                .api_key
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        cx.notify();
    }

    /// The provider draft, for the shell to save.
    pub fn provider_draft(&self) -> &ProviderDraft {
        &self.provider
    }

    /// Note the id a freshly saved provider was given.
    pub fn provider_saved(&mut self, id: ProviderId, cx: &mut Context<Self>) {
        self.provider.id = Some(id);
        self.provider.api_key.clear();
        cx.notify();
    }

    /// Point the search-engine draft at a stored profile.
    pub fn edit_search_engine(
        &mut self,
        profile: &SearchEngineProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_engine = SearchEngineDraft::from_profile(profile);
        self.seed_search_fields(window, cx);
    }

    pub fn set_search_engine_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.search_engine.enabled = enabled;
        cx.notify();
    }

    /// The search-engine draft, for the shell to save.
    pub fn search_engine_draft(&self) -> &SearchEngineDraft {
        &self.search_engine
    }

    /// The list to store after saving the draft.
    pub fn search_engines_with_draft(&self) -> Vec<SearchEngineProfile> {
        upsert_search_engine(&self.state.search_engine_profiles, &self.search_engine)
    }

    /// The list to store after dropping the engine at `index`.
    pub fn search_engines_without(&self, index: usize) -> Vec<SearchEngineProfile> {
        remove_search_engine(&self.state.search_engine_profiles, index)
    }

    /// The list to store after moving the engine at `index`.
    pub fn search_engines_moved(&self, index: usize, delta: isize) -> Vec<SearchEngineProfile> {
        move_search_engine(&self.state.search_engine_profiles, index, delta)
    }

    /// Clear the search-engine draft, after the one it edited was deleted.
    pub fn clear_search_engine_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_engine = SearchEngineDraft::default();
        self.seed_search_fields(window, cx);
    }

    /// Fill every field from the drafts and state.
    fn seed_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.seed_general_fields(window, cx);
        self.seed_provider_fields(window, cx);
        self.seed_search_fields(window, cx);
    }

    fn seed_general_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let patterns = general::format_blocked_patterns(&self.state.bash_blocked_patterns);
        let config = self.state.agent_team_config.clone();
        self.general_fields
            .blocked_patterns
            .update(cx, |input, cx| input.set_value(patterns, window, cx));
        self.general_fields.max_depth.update(cx, |input, cx| {
            input.set_value(config.max_depth.to_string(), window, cx)
        });
        self.general_fields
            .max_agents_per_level
            .update(cx, |input, cx| {
                input.set_value(config.max_agents_per_level.to_string(), window, cx)
            });
        cx.notify();
    }

    fn seed_provider_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.provider.clone();
        self.provider_fields
            .name
            .update(cx, |input, cx| input.set_value(draft.name, window, cx));
        self.provider_fields
            .base_url
            .update(cx, |input, cx| input.set_value(draft.base_url, window, cx));
        // The key field is deliberately not seeded from the draft: it is only ever
        // what was typed this session, and the stored key never comes back out.
        self.provider_fields.model_id.update(cx, |input, cx| {
            input.set_value(draft.manual_model_id, window, cx)
        });
        cx.notify();
    }

    fn seed_search_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.search_engine.clone();
        self.search_fields
            .name
            .update(cx, |input, cx| input.set_value(draft.name, window, cx));
        self.search_fields
            .base_url
            .update(cx, |input, cx| input.set_value(draft.base_url, window, cx));
        cx.notify();
    }

    /// How the two numeric fields fold into the team config.
    fn emit(&self, cx: &mut Context<Self>) -> Emit {
        let this = cx.entity();
        Rc::new(move |event, _window, cx| {
            this.update(cx, |_, cx| cx.emit(event));
        })
    }

    /// The section list.
    fn render_nav(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let tokens = self.tokens;
        let state = &self.state;
        div()
            .w(px(NAV_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .p(px(12.0))
            .bg(tokens.panel_soft.hsla())
            .border_r_1()
            .border_color(tokens.line.hsla())
            .child(
                Button::new("settings-back")
                    .label(tr(state, "back"))
                    .ghost()
                    .small()
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SettingsEvent::Close))),
            )
            .child(
                div()
                    .pt(px(14.0))
                    .pb(px(8.0))
                    .pl(px(6.0))
                    .font_family(font::MONO)
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .child(SharedString::from(tr(state, "settings"))),
            )
            .children(
                Section::ALL
                    .into_iter()
                    .enumerate()
                    .map(|(index, section)| {
                        let active = self.section == section;
                        Button::new(("settings-section", index as u64))
                            .label(tr(state, section.key()))
                            .when(active, |button| button.primary())
                            .when(!active, |button| button.ghost())
                            .small()
                            .on_click(
                                cx.listener(move |_, _, _, cx| {
                                    cx.emit(SettingsEvent::Show(section))
                                }),
                            )
                    }),
            )
            .into_any_element()
    }
}

impl Render for Settings {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        let emit = self.emit(cx);
        let body = match self.section {
            Section::General => general::render(&self.state, &self.general_fields, tokens, emit),
            Section::Providers => providers::render(
                &self.state,
                &self.provider,
                &self.provider_fields,
                tokens,
                emit,
            ),
            Section::WebSearch => web_search::render(
                &self.state,
                &self.search_engine,
                &self.search_fields,
                tokens,
                emit,
            ),
        };

        div()
            .size_full()
            .flex()
            .bg(tokens.bg_canvas.hsla())
            .child(self.render_nav(cx))
            .child(
                div().flex_1().h_full().overflow_hidden().child(
                    div()
                        .id("settings-body")
                        .size_full()
                        .overflow_y_scroll()
                        .child(
                            // Centred and width-limited: a form field spanning
                            // a wide window loses the eye between the label
                            // and its control.
                            div()
                                .max_w(px(form::CONTENT_WIDTH))
                                .mx_auto()
                                .px(px(28.0))
                                .py(px(24.0))
                                .child(body),
                        ),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nav_matches_the_sidebar_so_the_left_edge_does_not_shift() {
        // Settings replaces the chat pane; a different nav width would jump the
        // whole window's content when the page opens.
        assert_eq!(NAV_WIDTH, layout::SIDEBAR_WIDTH);
    }

    #[test]
    fn closing_and_switching_sections_are_different_events() {
        // Both come from the same column of buttons, so conflating them would make
        // Back land on a section.
        assert_ne!(SettingsEvent::Close, SettingsEvent::Show(Section::General));
    }
}
