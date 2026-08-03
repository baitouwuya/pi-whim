//! The settings page.
//!
//! One view owning the drafts and every `InputState`, and section render functions
//! over it. The sections are functions rather than entities because none of them
//! has state of its own — the drafts are shared, and a provider's name field has
//! to survive a switch to the web-search section and back.
//!
//! Validation is not here. `engine::settings` decides whether a draft can be
//! saved and what a preset fills in; this module asks and arranges.

pub mod background_ai;
pub mod dropdown;
pub mod form;
pub mod general;
pub mod providers;
pub mod toggle;
pub mod web_search;

use std::{collections::BTreeMap, rc::Rc};

use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Focusable, IntoElement, ParentElement, Render,
    ScrollHandle, SharedString, Styled, Window, div, point, prelude::FluentBuilder, px,
};
use gpui_component::{
    IndexPath, Sizable,
    button::{Button, ButtonVariants},
    input::{InputEvent, InputState},
    select::{SelectEvent, SelectState},
};
use pi_whim_core::{
    AgentTeamConfig, AppState, BashPolicy, Language, MAX_AGENT_DEPTH, MAX_AGENTS_PER_LEVEL,
    MAX_ONE_SHOT_AI_CONCURRENCY, MAX_ONE_SHOT_AI_MAX_OUTPUT_TOKENS, MAX_ONE_SHOT_AI_QUEUE_CAPACITY,
    MAX_ONE_SHOT_AI_TIMEOUT_SECS, MIN_ONE_SHOT_AI_MAX_OUTPUT_TOKENS, MIN_ONE_SHOT_AI_TIMEOUT_SECS,
    OneShotAiConfig, OneShotAiTaskConfig, ProviderId, ProviderModel, ProviderProtocol, QueueMode,
    SESSION_TITLE_TASK_KIND, SearchEngineId, SearchEngineKind, SearchEngineProfile, strings::tr,
};
use pi_whim_engine::settings::{
    Preset, ProviderDraft, SearchEngineDraft, Section, move_search_engine, remove_search_engine,
};
use pi_whim_theme::{Tokens, font, layout, text};

use crate::{elements::isolated_vertical_scroll_area, theme::IntoHsla};
use dropdown::{Choice, ChoiceState};

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

/// Visible state for one connection test. Results stay local to settings; the
/// configured engine profile remains pure persisted metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SearchEngineTestStatus {
    Testing,
    Succeeded,
    Failed(String),
}

impl SearchEngineTestStatus {
    pub fn is_testing(&self) -> bool {
        matches!(self, Self::Testing)
    }
}

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

    EditBackgroundAiTask(String),
    CloseBackgroundAiTaskEditor,
    SetBackgroundAiTaskEnabled(bool),
    SaveBackgroundAiTask,

    SetLanguage(Language),
    SetAutoCompaction(bool),
    SetBashPolicy(BashPolicy),
    SetBlockedPatterns(Vec<String>),
    SetAgentTeamConfig(AgentTeamConfig),
    ApproveProjectHooks {
        fingerprint: String,
    },
    RevokeProjectHooks,
    SetOneShotAiConfig(OneShotAiConfig),
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
    /// Start a search engine that has not been saved.
    NewSearchEngine,
    /// Dismiss the add/edit dialog without storing its draft.
    CloseSearchEngineEditor,
    SetSearchEngineKind(SearchEngineKind),
    SetSearchEngineEnabled(bool),
    /// Store the whole list, which is how a reorder or a delete is saved too.
    SaveSearchEngines(Vec<SearchEngineProfile>),
    SaveSearchEngine,
    TestSearchEngine,
    QuickTestSearchEngine(SearchEngineProfile),
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
    background_ai_task_kind: Option<String>,
    background_ai_task: OneShotAiTaskConfig,
    background_ai_editor_open: bool,
    search_engine: SearchEngineDraft,
    search_engine_editor_open: bool,
    editor_search_test: Option<SearchEngineTestStatus>,
    search_engine_tests: BTreeMap<SearchEngineId, SearchEngineTestStatus>,
    general_fields: general::Fields,
    background_ai_fields: background_ai::Fields,
    provider_fields: providers::Fields,
    search_fields: web_search::Fields,
    scroll: ScrollHandle,
    model_scroll: ScrollHandle,
}

impl EventEmitter<SettingsEvent> for Settings {}

fn choice_picker<T>(
    items: Vec<Choice<T>>,
    selected: T,
    window: &mut Window,
    cx: &mut Context<Settings>,
) -> Entity<ChoiceState<T>>
where
    T: Clone + PartialEq + 'static,
{
    let selected_index = items
        .iter()
        .position(|item| item.value == selected)
        .map(IndexPath::new);
    cx.new(|cx| SelectState::new(items, selected_index, window, cx))
}

fn sync_choice_picker<T>(
    picker: &Entity<ChoiceState<T>>,
    items: Vec<Choice<T>>,
    selected: T,
    window: &mut Window,
    cx: &mut Context<Settings>,
) where
    T: Clone + PartialEq + 'static,
{
    picker.update(cx, |picker, cx| {
        picker.set_items(items, window, cx);
        picker.set_selected_value(&selected, window, cx);
    });
}

fn sync_choice_selection<T>(
    picker: &Entity<ChoiceState<T>>,
    selected: T,
    window: &mut Window,
    cx: &mut Context<Settings>,
) where
    T: Clone + PartialEq + 'static,
{
    picker.update(cx, |picker, cx| {
        picker.set_selected_value(&selected, window, cx);
    });
}

impl Settings {
    pub fn new(
        tokens: Tokens,
        state: AppState,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let provider = ProviderDraft::default();
        let background_ai_task = state.one_shot_ai_config.task(SESSION_TITLE_TASK_KIND);
        let search_engine = SearchEngineDraft::default();
        let general_fields = general::Fields {
            blocked_patterns: cx.new(|cx| {
                InputState::new(window, cx)
                    // Multi-line, because the patterns are a list and a
                    // comma-separated single line hides which one is malformed.
                    .multi_line(true)
                    .rows(general::BLOCKED_PATTERN_ROWS)
                    // Each pattern is one logical row. Keeping wrapping off lets
                    // the wheel boundary use that row count as an exact overflow
                    // signal instead of guessing from text width.
                    .soft_wrap(false)
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
            language: choice_picker(general::language_choices(), state.language, window, cx),
            bash_policy: choice_picker(
                general::bash_policy_choices(&state),
                state.bash_policy,
                window,
                cx,
            ),
            steering_mode: choice_picker(
                general::queue_mode_choices(&state),
                state.steering_mode,
                window,
                cx,
            ),
            follow_up_mode: choice_picker(
                general::queue_mode_choices(&state),
                state.follow_up_mode,
                window,
                cx,
            ),
        };
        let background_ai_fields = background_ai::Fields {
            model: choice_picker(
                background_ai::model_choices(&state),
                background_ai::selected_model(&background_ai_task),
                window,
                cx,
            ),
            thinking: choice_picker(
                background_ai::thinking_choices(&state, &background_ai_task),
                background_ai_task.thinking_level,
                window,
                cx,
            ),
            max_output_tokens: cx.new(|cx| {
                InputState::new(window, cx)
                    .min(MIN_ONE_SHOT_AI_MAX_OUTPUT_TOKENS as f64)
                    .max(MAX_ONE_SHOT_AI_MAX_OUTPUT_TOKENS as f64)
            }),
            concurrency: cx.new(|cx| {
                InputState::new(window, cx)
                    .min(1.0)
                    .max(MAX_ONE_SHOT_AI_CONCURRENCY as f64)
            }),
            queue_capacity: cx.new(|cx| {
                InputState::new(window, cx)
                    .min(0.0)
                    .max(MAX_ONE_SHOT_AI_QUEUE_CAPACITY as f64)
            }),
            timeout: cx.new(|cx| {
                InputState::new(window, cx)
                    .min(MIN_ONE_SHOT_AI_TIMEOUT_SECS as f64)
                    .max(MAX_ONE_SHOT_AI_TIMEOUT_SECS as f64)
            }),
        };
        let provider_fields = providers::Fields {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("OpenAI")),
            base_url: cx
                .new(|cx| InputState::new(window, cx).placeholder("https://api.openai.com/v1")),
            api_key: cx.new(|cx| InputState::new(window, cx).masked(true).placeholder("sk-…")),
            model_id: cx.new(|cx| InputState::new(window, cx).placeholder("gpt-5")),
            preset: choice_picker(providers::preset_choices(), provider.preset, window, cx),
            protocol: choice_picker(providers::protocol_choices(), provider.protocol, window, cx),
        };
        let search_fields = web_search::Fields {
            name: cx.new(|cx| InputState::new(window, cx).placeholder("SearXNG")),
            kind: choice_picker(
                web_search::kind_choices(&state),
                search_engine.kind,
                window,
                cx,
            ),
            base_url: cx
                .new(|cx| InputState::new(window, cx).placeholder("https://search.example")),
            api_key: cx.new(|cx| {
                InputState::new(window, cx)
                    .masked(true)
                    .placeholder("API Key")
            }),
        };

        // The provider and search-engine drafts are edited into, then saved, so
        // their fields write into the draft rather than emitting.
        Self::watch_draft_fields(
            &background_ai_fields,
            &provider_fields,
            &search_fields,
            window,
            cx,
        );
        Self::watch_choice_fields(
            &general_fields,
            &background_ai_fields,
            &provider_fields,
            &search_fields,
            window,
            cx,
        );

        let mut settings = Self {
            section: Section::default(),
            tokens,
            state,
            provider,
            background_ai_task_kind: None,
            background_ai_task,
            background_ai_editor_open: false,
            search_engine,
            search_engine_editor_open: false,
            editor_search_test: None,
            search_engine_tests: BTreeMap::new(),
            general_fields,
            background_ai_fields,
            provider_fields,
            search_fields,
            scroll: ScrollHandle::new(),
            model_scroll: ScrollHandle::new(),
        };
        settings.seed_fields(window, cx);
        settings
    }

    /// Keep the draft in step with what is typed.
    fn watch_draft_fields(
        background_ai: &background_ai::Fields,
        provider: &providers::Fields,
        search: &web_search::Fields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(
            &background_ai.max_output_tokens,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change)
                    && let Ok(value) = input.read(cx).value().parse::<u32>()
                {
                    settings.background_ai_task.max_output_tokens = value;
                    cx.notify();
                }
            },
        )
        .detach();
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
                    settings.editor_search_test = None;
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
                    settings.editor_search_test = None;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &search.api_key,
            window,
            |settings, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    settings.search_engine.api_key = input.read(cx).value().to_string();
                    settings.editor_search_test = None;
                    cx.notify();
                }
            },
        )
        .detach();
    }

    /// Route finite-choice dropdowns through the same settings events as the
    /// controls they replaced.
    fn watch_choice_fields(
        general: &general::Fields,
        background_ai: &background_ai::Fields,
        provider: &providers::Fields,
        search: &web_search::Fields,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.subscribe_in(&general.language, window, |_, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(language)) = event {
                cx.emit(SettingsEvent::SetLanguage(*language));
            }
        })
        .detach();
        cx.subscribe_in(&general.bash_policy, window, |_, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(policy)) = event {
                cx.emit(SettingsEvent::SetBashPolicy(*policy));
            }
        })
        .detach();
        cx.subscribe_in(
            &general.steering_mode,
            window,
            |settings, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(steering)) = event {
                    cx.emit(SettingsEvent::SetQueueModes {
                        steering: *steering,
                        follow_up: settings.state.follow_up_mode,
                    });
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &general.follow_up_mode,
            window,
            |settings, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(follow_up)) = event {
                    cx.emit(SettingsEvent::SetQueueModes {
                        steering: settings.state.steering_mode,
                        follow_up: *follow_up,
                    });
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &background_ai.model,
            window,
            |settings, _, event, window, cx| {
                if let SelectEvent::Confirm(Some(selection)) = event {
                    let (provider_id, model_id) = selection
                        .clone()
                        .map(|(provider, model)| (Some(provider), Some(model)))
                        .unwrap_or((None, None));
                    settings.background_ai_task.provider_id = provider_id;
                    settings.background_ai_task.model_id = model_id;
                    // Off is valid for every model; do not carry an unsupported
                    // reasoning level across a model change.
                    settings.background_ai_task.thinking_level = pi_whim_core::ThinkingLevel::Off;
                    settings.seed_background_ai_choices(window, cx);
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(
            &background_ai.thinking,
            window,
            |settings, _, event, _, cx| {
                if let SelectEvent::Confirm(Some(thinking_level)) = event {
                    settings.background_ai_task.thinking_level = *thinking_level;
                    cx.notify();
                }
            },
        )
        .detach();
        cx.subscribe_in(&provider.preset, window, |_, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(preset)) = event {
                cx.emit(SettingsEvent::SelectPreset(*preset));
            }
        })
        .detach();
        cx.subscribe_in(&provider.protocol, window, |_, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(protocol)) = event {
                cx.emit(SettingsEvent::SetProtocol(*protocol));
            }
        })
        .detach();
        cx.subscribe_in(&search.kind, window, |_, _, event, _, cx| {
            if let SelectEvent::Confirm(Some(kind)) = event {
                cx.emit(SettingsEvent::SetSearchEngineKind(*kind));
            }
        })
        .detach();
    }

    pub fn section(&self) -> Section {
        self.section
    }

    pub fn show(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.reset_scroll(cx);
    }

    /// Return to the page heading after reopening settings or changing section.
    pub fn reset_scroll(&mut self, cx: &mut Context<Self>) {
        self.scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    /// Refresh from state, and re-seed the fields whose value it owns.
    pub fn sync(&mut self, state: &AppState, window: &mut Window, cx: &mut Context<Self>) {
        let language_changed = self.state.language != state.language;
        let bash_policy_changed = self.state.bash_policy != state.bash_policy;
        let steering_changed = self.state.steering_mode != state.steering_mode;
        let follow_up_changed = self.state.follow_up_mode != state.follow_up_mode;
        let patterns_changed = self.state.bash_blocked_patterns != state.bash_blocked_patterns;
        let team_changed = self.state.agent_team_config != state.agent_team_config;
        let one_shot_changed = self.state.one_shot_ai_config != state.one_shot_ai_config;
        let providers_changed = self.state.provider_profiles != state.provider_profiles;
        self.state = state.clone();
        if one_shot_changed && !self.background_ai_editor_open {
            let kind = self
                .background_ai_task_kind
                .as_deref()
                .unwrap_or(SESSION_TITLE_TASK_KIND);
            self.background_ai_task = self.state.one_shot_ai_config.task(kind);
            self.seed_background_ai_task_inputs(window, cx);
        }
        self.search_engine_tests.retain(|id, _| {
            self.state
                .search_engine_profiles
                .iter()
                .any(|profile| profile.id == *id)
        });
        if let Some(id) = self.search_engine.id
            && let Some(profile) = self
                .state
                .search_engine_profiles
                .iter()
                .find(|profile| profile.id == id)
        {
            self.search_engine.has_api_key = profile.has_api_key;
        }

        // A language change replaces every translated menu label. Other snapshots
        // only move the picker whose domain value changed, so an unrelated stream
        // update cannot reset keyboard focus in an open menu.
        if language_changed {
            self.seed_general_choices(window, cx);
            self.seed_background_ai_choices(window, cx);
            self.seed_search_kind(window, cx);
        } else {
            if bash_policy_changed {
                sync_choice_selection(
                    &self.general_fields.bash_policy,
                    self.state.bash_policy,
                    window,
                    cx,
                );
            }
            if steering_changed {
                sync_choice_selection(
                    &self.general_fields.steering_mode,
                    self.state.steering_mode,
                    window,
                    cx,
                );
            }
            if follow_up_changed {
                sync_choice_selection(
                    &self.general_fields.follow_up_mode,
                    self.state.follow_up_mode,
                    window,
                    cx,
                );
            }
            if one_shot_changed || providers_changed {
                self.seed_background_ai_choices(window, cx);
            }
        }

        if patterns_changed || team_changed {
            self.seed_general_inputs(window, cx);
        }
        if one_shot_changed {
            self.seed_background_ai_inputs(window, cx);
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
        self.model_scroll.set_offset(point(px(0.0), px(0.0)));
        self.seed_provider_fields(window, cx);
    }

    /// Start a provider that has not been saved.
    pub fn new_provider(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.provider = ProviderDraft::default();
        self.model_scroll.set_offset(point(px(0.0), px(0.0)));
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
        self.model_scroll.set_offset(point(px(0.0), px(0.0)));
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

    /// Open one fixed background-task profile in the shared editor.
    pub fn edit_background_ai_task(
        &mut self,
        kind: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.background_ai_task = self.state.one_shot_ai_config.task(&kind);
        self.background_ai_task_kind = Some(kind);
        self.background_ai_editor_open = true;
        self.seed_background_ai_task_inputs(window, cx);
        self.seed_background_ai_choices(window, cx);
        cx.notify();
    }

    /// Close the task editor and discard changes not sent to preferences.
    pub fn close_background_ai_task_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.background_ai_editor_open = false;
        let kind = self
            .background_ai_task_kind
            .as_deref()
            .unwrap_or(SESSION_TITLE_TASK_KIND);
        self.background_ai_task = self.state.one_shot_ai_config.task(kind);
        self.seed_background_ai_task_inputs(window, cx);
        self.seed_background_ai_choices(window, cx);
        cx.notify();
    }

    pub fn set_background_ai_task_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.background_ai_task.enabled = enabled;
        cx.notify();
    }

    /// Merge the open task draft into the latest shared limits.
    pub fn background_ai_config_with_draft(&self) -> OneShotAiConfig {
        let mut config = self.state.one_shot_ai_config.clone();
        if let Some(kind) = self.background_ai_task_kind.as_deref() {
            config.set_task(kind, self.background_ai_task.clone());
        }
        config.normalized()
    }

    /// Start a search engine that has not been saved and open its editor.
    pub fn new_search_engine(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_engine = SearchEngineDraft::default();
        self.open_search_engine_editor(window, cx);
    }

    /// Point the search-engine draft at a stored profile and open its editor.
    pub fn edit_search_engine(
        &mut self,
        profile: &SearchEngineProfile,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_engine = SearchEngineDraft::from_profile(profile);
        self.open_search_engine_editor(window, cx);
    }

    fn open_search_engine_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_engine_editor_open = true;
        self.editor_search_test = None;
        self.seed_search_fields(window, cx);
        let focus = self
            .search_fields
            .name
            .update(cx, |input, cx| input.focus_handle(cx));
        focus.focus(window, cx);
        cx.notify();
    }

    /// Close the editor and discard anything that was not saved.
    pub fn close_search_engine_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.search_engine_editor_open = false;
        self.editor_search_test = None;
        self.search_engine = SearchEngineDraft::default();
        self.seed_search_fields(window, cx);
    }

    pub fn set_search_engine_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.search_engine.enabled = enabled;
        self.editor_search_test = None;
        cx.notify();
    }

    /// Change adapter, carrying an untouched default name and endpoint with it.
    pub fn set_search_engine_kind(
        &mut self,
        kind: SearchEngineKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.search_engine.set_kind(kind);
        self.editor_search_test = None;
        self.seed_search_fields(window, cx);
    }

    pub fn start_search_engine_test(
        &mut self,
        id: SearchEngineId,
        editor: bool,
        cx: &mut Context<Self>,
    ) {
        if editor {
            self.editor_search_test = Some(SearchEngineTestStatus::Testing);
        } else {
            self.search_engine_tests
                .insert(id, SearchEngineTestStatus::Testing);
        }
        cx.notify();
    }

    pub fn finish_search_engine_test(
        &mut self,
        id: SearchEngineId,
        editor: bool,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        let status = match result {
            Ok(()) => SearchEngineTestStatus::Succeeded,
            Err(error) => SearchEngineTestStatus::Failed(error),
        };
        if editor {
            self.editor_search_test = Some(status);
        } else {
            self.search_engine_tests.insert(id, status);
        }
        cx.notify();
    }

    /// The search-engine draft, for the shell to save.
    pub fn search_engine_draft(&self) -> &SearchEngineDraft {
        &self.search_engine
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
        self.close_search_engine_editor(window, cx);
    }

    /// Fill every field from the drafts and state.
    fn seed_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.seed_general_inputs(window, cx);
        self.seed_general_choices(window, cx);
        self.seed_background_ai_inputs(window, cx);
        self.seed_background_ai_task_inputs(window, cx);
        self.seed_background_ai_choices(window, cx);
        self.seed_provider_fields(window, cx);
        self.seed_search_fields(window, cx);
    }

    fn seed_general_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    fn seed_background_ai_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let one_shot = self.state.one_shot_ai_config.clone();
        self.background_ai_fields
            .concurrency
            .update(cx, |input, cx| {
                input.set_value(one_shot.max_concurrency.to_string(), window, cx)
            });
        self.background_ai_fields
            .queue_capacity
            .update(cx, |input, cx| {
                input.set_value(one_shot.queue_capacity.to_string(), window, cx)
            });
        self.background_ai_fields.timeout.update(cx, |input, cx| {
            input.set_value(one_shot.timeout_secs.to_string(), window, cx)
        });
        cx.notify();
    }

    fn seed_background_ai_task_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let max_output_tokens = self.background_ai_task.max_output_tokens;
        self.background_ai_fields
            .max_output_tokens
            .update(cx, |input, cx| {
                input.set_value(max_output_tokens.to_string(), window, cx)
            });
        cx.notify();
    }

    fn seed_general_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        sync_choice_picker(
            &self.general_fields.language,
            general::language_choices(),
            self.state.language,
            window,
            cx,
        );
        sync_choice_picker(
            &self.general_fields.bash_policy,
            general::bash_policy_choices(&self.state),
            self.state.bash_policy,
            window,
            cx,
        );
        sync_choice_picker(
            &self.general_fields.steering_mode,
            general::queue_mode_choices(&self.state),
            self.state.steering_mode,
            window,
            cx,
        );
        sync_choice_picker(
            &self.general_fields.follow_up_mode,
            general::queue_mode_choices(&self.state),
            self.state.follow_up_mode,
            window,
            cx,
        );
    }

    fn seed_background_ai_choices(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        sync_choice_picker(
            &self.background_ai_fields.model,
            background_ai::model_choices(&self.state),
            background_ai::selected_model(&self.background_ai_task),
            window,
            cx,
        );
        sync_choice_picker(
            &self.background_ai_fields.thinking,
            background_ai::thinking_choices(&self.state, &self.background_ai_task),
            self.background_ai_task.thinking_level,
            window,
            cx,
        );
    }

    fn seed_provider_fields(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let draft = self.provider.clone();
        let preset = draft.preset;
        let protocol = draft.protocol;
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
        sync_choice_picker(
            &self.provider_fields.preset,
            providers::preset_choices(),
            preset,
            window,
            cx,
        );
        sync_choice_picker(
            &self.provider_fields.protocol,
            providers::protocol_choices(),
            protocol,
            window,
            cx,
        );
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
        if draft.api_key.is_empty() {
            self.search_fields
                .api_key
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.seed_search_kind(window, cx);
        cx.notify();
    }

    fn seed_search_kind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        sync_choice_picker(
            &self.search_fields.kind,
            web_search::kind_choices(&self.state),
            self.search_engine.kind,
            window,
            cx,
        );
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
        let back = SharedString::from(tr(state, "back"));
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
                    .ghost()
                    .small()
                    .w_full()
                    .h(px(34.0))
                    .px(px(10.0))
                    .child(
                        div()
                            .w_full()
                            .text_size(px(text::DETAIL_SIZE))
                            .text_color(tokens.text.hsla())
                            .child(back),
                    )
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
                        let label = SharedString::from(tr(state, section.key()));
                        Button::new(("settings-section", index as u64))
                            .ghost()
                            .small()
                            .w_full()
                            .h(px(34.0))
                            .px(px(10.0))
                            .when(active, |button| {
                                button
                                    .bg(tokens.accent_surface_soft().hsla())
                                    .border_l_2()
                                    .border_color(tokens.accent_border_strong().hsla())
                            })
                            .child(
                                div()
                                    .w_full()
                                    .text_size(px(text::DETAIL_SIZE))
                                    .text_color(if active {
                                        tokens.text.hsla()
                                    } else {
                                        tokens.muted_strong.hsla()
                                    })
                                    .child(label),
                            )
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
        let background_ai_editor = self.background_ai_editor_open.then(|| {
            background_ai::render_editor(
                &self.state,
                self.background_ai_task_kind
                    .as_deref()
                    .unwrap_or(SESSION_TITLE_TASK_KIND),
                &self.background_ai_task,
                &self.background_ai_fields,
                tokens,
                emit.clone(),
                cx,
            )
        });
        let search_engine_editor = self.search_engine_editor_open.then(|| {
            web_search::render_editor(
                &self.state,
                &self.search_engine,
                &self.search_fields,
                self.editor_search_test.as_ref(),
                tokens,
                emit.clone(),
                cx,
            )
        });
        let body = match self.section {
            Section::General => {
                general::render_general(&self.state, &self.general_fields, tokens, emit)
            }
            Section::Execution => {
                general::render_execution(&self.state, &self.general_fields, tokens, emit)
            }
            Section::Providers => providers::render(
                &self.state,
                &self.provider,
                &self.provider_fields,
                &self.model_scroll,
                tokens,
                emit,
            ),
            Section::BackgroundAi => {
                background_ai::render(&self.state, &self.background_ai_fields, tokens, emit)
            }
            Section::WebSearch => {
                web_search::render(&self.state, &self.search_engine_tests, tokens, emit)
            }
        };

        div()
            .size_full()
            .flex()
            .bg(tokens.bg_canvas.hsla())
            .child(self.render_nav(cx))
            .child(
                div().flex_1().h_full().overflow_hidden().child(
                    isolated_vertical_scroll_area("settings-body", &self.scroll)
                        .size_full()
                        .child(
                            // Centred and width-limited: a form field spanning
                            // a wide window loses the eye between the label
                            // and its control.
                            div()
                                .w_full()
                                .max_w(px(form::CONTENT_WIDTH))
                                .mx_auto()
                                .px(px(28.0))
                                .py(px(24.0))
                                .child(body),
                        ),
                ),
            )
            .when_some(background_ai_editor, |this, editor| this.child(editor))
            .when_some(search_engine_editor, |this, editor| this.child(editor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_theme::ThemePreference;

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

    #[gpui::test]
    async fn changing_language_keeps_unapplied_launch_drafts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::init(ThemePreference::default(), cx).unwrap());
        let settings = cx.add_window(|window, cx| {
            Settings::new(Tokens::light(), AppState::default(), window, cx)
        });

        settings
            .update(cx, |settings, window, cx| {
                settings
                    .general_fields
                    .blocked_patterns
                    .update(cx, |input, cx| {
                        input.set_value("custom command", window, cx)
                    });
                settings
                    .general_fields
                    .max_depth
                    .update(cx, |input, cx| input.set_value("3", window, cx));

                let mut state = settings.state.clone();
                state.language = Language::SimplifiedChinese;
                settings.sync(&state, window, cx);

                assert_eq!(
                    settings.general_fields.blocked_patterns.read(cx).value(),
                    "custom command"
                );
                assert_eq!(settings.general_fields.max_depth.read(cx).value(), "3");
            })
            .expect("the settings window is open");
    }

    #[gpui::test]
    async fn dropdowns_follow_domain_state_and_provider_drafts(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::init(ThemePreference::default(), cx).unwrap());
        let settings = cx.add_window(|window, cx| {
            Settings::new(Tokens::light(), AppState::default(), window, cx)
        });

        settings
            .update(cx, |settings, window, cx| {
                let mut state = settings.state.clone();
                state.language = Language::SimplifiedChinese;
                state.bash_policy = BashPolicy::Deny;
                state.steering_mode = QueueMode::All;
                state.follow_up_mode = QueueMode::OneAtATime;
                settings.sync(&state, window, cx);

                assert_eq!(
                    settings.general_fields.language.read(cx).selected_value(),
                    Some(&Language::SimplifiedChinese)
                );
                assert_eq!(
                    settings
                        .general_fields
                        .bash_policy
                        .read(cx)
                        .selected_value(),
                    Some(&BashPolicy::Deny)
                );
                assert_eq!(
                    settings
                        .general_fields
                        .steering_mode
                        .read(cx)
                        .selected_value(),
                    Some(&QueueMode::All)
                );

                settings.apply_preset(Preset::Anthropic, window, cx);
                assert_eq!(
                    settings.provider_fields.preset.read(cx).selected_value(),
                    Some(&Preset::Anthropic)
                );
                assert_eq!(
                    settings.provider_fields.protocol.read(cx).selected_value(),
                    Some(&ProviderProtocol::AnthropicMessages)
                );

                settings.set_protocol(ProviderProtocol::GoogleGenerativeAi, window, cx);
                assert_eq!(
                    settings.provider_fields.protocol.read(cx).selected_value(),
                    Some(&ProviderProtocol::GoogleGenerativeAi)
                );
            })
            .expect("the settings window is open");
    }

    #[gpui::test]
    async fn search_engine_details_only_live_in_the_open_editor(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| crate::init(ThemePreference::default(), cx).unwrap());
        let settings = cx.add_window(|window, cx| {
            Settings::new(Tokens::light(), AppState::default(), window, cx)
        });

        settings
            .update(cx, |settings, window, cx| {
                settings.new_search_engine(window, cx);
                assert!(settings.search_engine_editor_open);
                assert_eq!(settings.search_engine, SearchEngineDraft::default());

                let mut profile = SearchEngineProfile::new_searxng();
                profile.name = "Private search".into();
                profile.base_url = "https://search.example".into();
                profile.enabled = false;
                settings.edit_search_engine(&profile, window, cx);

                assert!(settings.search_engine_editor_open);
                assert_eq!(settings.search_engine.id, Some(profile.id));
                assert_eq!(settings.search_engine.name, profile.name);
                assert_eq!(settings.search_engine.base_url, profile.base_url);
                assert!(!settings.search_engine.enabled);

                settings.search_engine.name = "Unsaved name".into();
                settings.close_search_engine_editor(window, cx);
                assert!(!settings.search_engine_editor_open);
                assert_eq!(settings.search_engine, SearchEngineDraft::default());
            })
            .expect("the settings window is open");
    }

    #[gpui::test]
    async fn editor_and_row_connection_tests_keep_independent_status(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| crate::init(ThemePreference::default(), cx).unwrap());
        let profile = SearchEngineProfile::new_doubao_global();
        let id = profile.id;
        let settings = cx.add_window(move |window, cx| {
            Settings::new(
                Tokens::light(),
                AppState {
                    search_engine_profiles: vec![profile],
                    ..AppState::default()
                },
                window,
                cx,
            )
        });

        settings
            .update(cx, |settings, _, cx| {
                settings.start_search_engine_test(id, false, cx);
                settings.start_search_engine_test(id, true, cx);
                settings.finish_search_engine_test(id, false, Ok(()), cx);
                settings.finish_search_engine_test(id, true, Err("401".into()), cx);

                assert_eq!(
                    settings.search_engine_tests.get(&id),
                    Some(&SearchEngineTestStatus::Succeeded)
                );
                assert_eq!(
                    settings.editor_search_test,
                    Some(SearchEngineTestStatus::Failed("401".into()))
                );
            })
            .expect("the settings window is open");
    }
}
