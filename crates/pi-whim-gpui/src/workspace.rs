//! Root view.
//!
//! Owns the domain state and the resolved theme, and arranges the chrome around
//! the space the conversation and sidebar will fill. Those two, along with the
//! settings page, land as their own modules.

use std::collections::BTreeSet;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Task, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_core::{
    Action, AgentTeamConfig, AppState, Attachment, BashPolicy, ConversationItem, ConversationRole,
    Language, ModelOption, ProjectId, ProviderId, ProviderProfile, ProviderProtocol, QueueMode,
    SearchEngineProfile, SessionStatus, SubmitMode, ThinkingLevel, stable_session_id,
};
use pi_whim_engine::dialogs::{Answer, Prompt};
use pi_whim_engine::mailbox::Delivery;
use pi_whim_engine::notice::Outbox;
use pi_whim_engine::session::now_ms;
use pi_whim_engine::slash_commands::SlashCommand;
use pi_whim_engine::state::{EngineState, ViewEffect};
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, text};

use crate::{
    chat::{
        self, Composer, ComposerEvent, Controls, ControlsEvent, Conversation, ConversationEvent,
        Palette, PaletteEvent, Paste, Sidebar, SidebarEvent,
    },
    chrome::{Banner, TopBar},
    dialogs::{PromptEvent, Prompts, Rename, RenameEvent},
    elements::GraphPaper,
    pump,
    settings::{Settings, SettingsEvent},
    theme::IntoHsla,
};

/// Something the shell cannot do itself, queued for whoever owns the backend.
///
/// Adding a project opens a folder picker and writes to the store; starting a
/// session launches a Pi process. Both live behind `AgentRuntime`, which this
/// crate deliberately does not depend on, so the views record the request and the
/// app drains it. This is the same pull model the egui build used for `UiIntent`,
/// kept only for the actions that genuinely cross the boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// Ask for a folder, and add it as a project.
    AddProject,
    /// Start a fresh session in a project.
    NewSession(ProjectId),
    /// Switch which model answers. The host defers this until the next prompt so
    /// the prior model compacts the history first.
    SetModel(ModelOption),
    SetThinkingLevel(ThinkingLevel),
    SetQueueModes {
        steering: QueueMode,
        follow_up: QueueMode,
    },
    /// Run a slash command. Every one of these needs the store, the clipboard, or
    /// an RPC, so the shell does not try to interpret them.
    RunCommand(SlashCommand),
    /// Show a project's folder in Finder.
    RevealProject(ProjectId),
    /// Forget a project. Its sessions stop; the folder on disk is left alone.
    RemoveProject(ProjectId),
    /// Store a new title for the session at `path`.
    RenameSession {
        path: String,
        title: String,
    },
    /// Copy the visible session's transcript into a new one.
    CloneSession,
    /// Put text on the clipboard.
    CopyToClipboard(String),
    /// Move a session's transcript to the trash.
    DeleteSession(String),
    /// Send a decision back to the agent that asked for it.
    AnswerPrompt(Answer),
    /// Turn a paste into an attachment. Copied files need canonicalizing and
    /// pasted bytes need writing, both of which need the attachment store.
    AttachPaste(Paste),
    /// Send the drafted prompt to the agent.
    ///
    /// The shell has already put it in the conversation, because a prompt should
    /// appear the moment it is sent rather than when Pi acknowledges it. What is
    /// left is the RPC.
    SubmitPrompt {
        content: String,
        attachments: Vec<Attachment>,
        mode: SubmitMode,
    },
    /// Bind the conversation to an already-pooled session.
    ///
    /// Selecting one is the shell's, but the process behind it is the host's: the
    /// pool decides which is visible and the transcript has to be re-read.
    ActivateSession {
        project_id: ProjectId,
        path: String,
    },
    /// Interrupt the turn in flight.
    Stop,

    // The settings page's requests. The reducer has already been run for the
    // ones that have an `Action`, so these are the persistence and network half:
    // writing preferences to the store, the key to the keychain, and asking a
    // provider what it offers.
    /// Write the preference the shell has already applied.
    PersistLanguage(Language),
    PersistBashPolicy(BashPolicy),
    PersistBlockedPatterns(Vec<String>),
    PersistAgentTeamConfig(AgentTeamConfig),
    SetAutoCompaction(bool),
    /// Store a provider, and its key if one was typed.
    SaveProvider {
        profile: ProviderProfile,
        api_key: Option<String>,
    },
    DeleteProvider(ProviderId),
    /// Store the whole search-engine list, which is how a reorder and a delete
    /// are saved too.
    SaveSearchEngines(Vec<SearchEngineProfile>),
    /// Check that a URL really answers as a SearXNG instance.
    TestSearchEngine(SearchEngineProfile),
    /// Ask a provider which models it has.
    DiscoverProviderModels {
        profile_id: Option<ProviderId>,
        provider_name: String,
        base_url: String,
        protocol: ProviderProtocol,
        api_key: Option<String>,
    },
}

/// The application shell.
pub struct Workspace {
    preference: ThemePreference,
    tokens: Tokens,
    engine: EngineState,
    sidebar: Entity<Sidebar>,
    conversation: Entity<Conversation>,
    composer: Entity<Composer>,
    controls: Entity<Controls>,
    palette: Entity<Palette>,
    prompts: Entity<Prompts>,
    rename: Entity<Rename>,
    settings: Entity<Settings>,
    /// Whether settings is showing instead of the chat panes.
    ///
    /// A page rather than a modal, because the provider form is long enough that
    /// a dialog would scroll inside a scroll.
    showing_settings: bool,
    /// Messages waiting to be shown.
    ///
    /// Held rather than pushed straight to the window because the shell is
    /// reachable without one — the app can report a failure before the first
    /// render — and because the notification stack lives on the window, not here.
    notices: Outbox,
    /// Projects whose sessions are listed. View-local: which projects a reader
    /// has open says nothing about the session.
    expanded_projects: BTreeSet<ProjectId>,
    /// Requests waiting for the backend owner to drain.
    requests: Vec<Request>,
    /// Session events waiting for the same owner.
    ///
    /// Held rather than handled, because turning one into state changes needs the
    /// session pool — which key it belongs to now, whether its process is still
    /// the current generation — and the pool is the owner's, not the shell's.
    deliveries: Vec<Delivery>,
    /// The loop delivering into `deliveries`.
    ///
    /// Stored and not detached on purpose: a [`Task`] cancels when dropped, so
    /// holding it here is what stops the pump when the shell goes away.
    pump: Option<Task<()>>,
}

impl Workspace {
    pub fn new(preference: ThemePreference, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mode = if ComponentTheme::global(cx).is_dark() {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        let tokens = Tokens::new(mode);
        // These subscribe with `subscribe_in` rather than `subscribe` so the
        // handlers receive a window: reseeding the runtime pickers needs one, and
        // every state change can change what they offer.
        let sidebar = cx.new(|_| Sidebar::new(tokens));
        cx.subscribe_in(&sidebar, window, |workspace, _, event, window, cx| {
            workspace.handle_sidebar_event(event.clone(), window, cx);
        })
        .detach();

        let conversation = cx.new(|_| Conversation::new(tokens));
        cx.subscribe(&conversation, |_, conversation, event, cx| {
            match event {
                ConversationEvent::ToggleToolDetails(id) => {
                    let id = id.clone();
                    conversation.update(cx, |conversation, cx| {
                        conversation.toggle_details(&id, cx);
                    });
                }
            }
            cx.notify();
        })
        .detach();

        let composer = cx.new(|cx| Composer::new(tokens, window, cx));
        cx.subscribe_in(&composer, window, |workspace, _, event, window, cx| {
            workspace.handle_composer_event(event.clone(), window, cx);
        })
        .detach();

        let controls = cx.new(|cx| Controls::new(tokens, window, cx));
        cx.subscribe_in(&controls, window, |workspace, _, event, window, cx| {
            workspace.handle_controls_event(event.clone(), window, cx);
        })
        .detach();

        let palette = cx.new(|_| Palette::new(tokens));
        cx.subscribe_in(&palette, window, |workspace, _, event, window, cx| {
            workspace.handle_palette_event(event.clone(), window, cx);
        })
        .detach();

        let prompts = cx.new(|_| Prompts::new(tokens));
        cx.subscribe(&prompts, |workspace, _, event, cx| {
            let PromptEvent::Answered(answer) = event;
            // Straight through: the shell has no session pool, and an unanswered
            // question leaves the agent that asked it blocked.
            workspace
                .requests
                .push(Request::AnswerPrompt(answer.clone()));
            cx.notify();
        })
        .detach();

        let rename = cx.new(|cx| Rename::new(tokens, window, cx));
        cx.subscribe(&rename, |workspace, _, event, cx| {
            let RenameEvent::Renamed { path, title } = event;
            workspace.requests.push(Request::RenameSession {
                path: path.clone(),
                title: title.clone(),
            });
            cx.notify();
        })
        .detach();

        let engine = EngineState::new();
        let settings = cx.new(|cx| Settings::new(tokens, engine.get().clone(), window, cx));
        cx.subscribe_in(&settings, window, |workspace, _, event, window, cx| {
            workspace.handle_settings_event(event.clone(), window, cx);
        })
        .detach();

        Self {
            preference,
            tokens,
            engine,
            sidebar,
            conversation,
            composer,
            controls,
            palette,
            prompts,
            rename,
            settings,
            showing_settings: false,
            notices: Outbox::new(),
            expanded_projects: BTreeSet::new(),
            requests: Vec::new(),
            deliveries: Vec::new(),
            // Started by the owner once it has a pool to stream from, since a
            // shell built before any session exists has nothing to listen to.
            pump: None,
        }
    }

    /// Start waking the window when a session says something.
    ///
    /// `events` is the engine's merged stream. Replaces any pump already running,
    /// so the old one is dropped — and therefore cancelled — rather than left
    /// delivering alongside the new one.
    pub fn listen(
        &mut self,
        events: crossbeam_channel::Receiver<Delivery>,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        self.pump = Some(pump::spawn(
            events,
            window,
            cx,
            |workspace, batch, _, cx| {
                workspace.deliveries.extend(batch);
                // The point of the whole pump: an event arriving is what marks the
                // shell dirty, instead of a timer redrawing on the chance one did.
                cx.notify();
            },
        ));
    }

    /// Take the session events that have arrived.
    ///
    /// Deliveries carry a `SessionToken` rather than a key; resolve it against
    /// the pool when handling one, because a session is re-keyed as soon as Pi
    /// reports its transcript path.
    pub fn take_deliveries(&mut self) -> Vec<Delivery> {
        std::mem::take(&mut self.deliveries)
    }

    /// Whether the pump is running.
    pub fn is_listening(&self) -> bool {
        self.pump.is_some()
    }

    /// Put the cursor in the prompt field.
    ///
    /// Where the window sends focus on open, and where it goes back to after a
    /// dialog closes — typing should reach the prompt without a click.
    pub fn focus_composer(&self, window: &mut Window, cx: &mut Context<Self>) {
        // The handle is read out before focusing so the composer's borrow ends
        // first: `focus` needs the app mutably, and holding the read across it
        // would borrow `cx` twice.
        let handle = self.composer.read(cx).focus_handle(cx);
        handle.focus(window, cx);
    }

    /// Queue a question from the agent.
    ///
    /// Takes a parsed [`Prompt`]: reading the wire request is `engine::dialogs`'
    /// job, and this crate has no `serde_json` on purpose.
    pub fn ask(&mut self, prompt: Prompt, cx: &mut Context<Self>) {
        self.prompts
            .update(cx, |prompts, cx| prompts.push(prompt, cx));
        cx.notify();
    }

    /// Drop the questions a session asked, because it has gone.
    pub fn forget_session(&mut self, session_key: &str, cx: &mut Context<Self>) {
        self.prompts.update(cx, |prompts, cx| {
            prompts.forget_session(session_key, cx);
        });
        cx.notify();
    }

    /// Report a failure to the user.
    pub fn report_error(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.notices.error(message);
        cx.notify();
    }

    /// Tell the user something that is not a failure.
    pub fn report_info(&mut self, message: impl Into<String>, cx: &mut Context<Self>) {
        self.notices.info(message);
        cx.notify();
    }

    /// Take the requests the views have raised since the last drain.
    ///
    /// The app calls this; the shell does not know how they are carried out.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// Act on what the sidebar reported, and refresh its rows.
    fn handle_sidebar_event(
        &mut self,
        event: SidebarEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // Both of these need the backend: one opens a folder picker and
            // stores a project, the other launches a Pi session. Neither is
            // reachable until the shell is connected, so they are recorded as
            // requests rather than silently doing nothing.
            SidebarEvent::AddProject => self.requests.push(Request::AddProject),
            SidebarEvent::NewSession(id) => {
                self.engine.apply(Action::SelectProject(id));
                self.requests.push(Request::NewSession(id));
            }
            SidebarEvent::ToggleProject(id) | SidebarEvent::OpenProject(id) => {
                // Selecting a project also toggles whether its sessions show,
                // so one click on a header does the obvious thing.
                toggle_expanded(&mut self.expanded_projects, id);
                self.engine.apply(Action::SelectProject(id));
            }
            SidebarEvent::OpenSession {
                project_id,
                pi_path,
            } => {
                self.engine.apply(Action::SelectProject(project_id));
                self.engine
                    .apply(Action::SelectSession(stable_session_id(&pi_path)));
                // Selecting is the shell's; binding to the process is not. The
                // pool decides which session is visible and the transcript has to
                // be re-read, so without this the sidebar would move while the
                // conversation kept showing the previous session.
                self.requests.push(Request::ActivateSession {
                    project_id,
                    path: pi_path,
                });
            }
            // The rest of the row menu is the backend's: Finder, the store, the
            // clipboard, and the trash all sit behind the boundary this crate
            // does not cross.
            SidebarEvent::RevealProject(id) => self.requests.push(Request::RevealProject(id)),
            SidebarEvent::RemoveProject(id) => self.requests.push(Request::RemoveProject(id)),
            SidebarEvent::CloneSession => self.requests.push(Request::CloneSession),
            SidebarEvent::CopySessionId(id) => {
                self.requests.push(Request::CopyToClipboard(id.to_string()))
            }
            SidebarEvent::DeleteSession(path) => self.requests.push(Request::DeleteSession(path)),
            SidebarEvent::RenameSession { pi_path, title } => {
                self.rename.update(cx, |rename, cx| {
                    rename.open(pi_path, &title, window, cx);
                });
            }
        }
        self.sync_views(window, cx);
        cx.notify();
    }

    /// Act on what the composer reported.
    ///
    /// A submitted prompt goes into the conversation here and to the agent as a
    /// request: it should appear the moment it is sent, not when Pi acknowledges
    /// it, and the RPC needs the runtime this crate has no handle on.
    fn handle_composer_event(
        &mut self,
        event: ComposerEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ComposerEvent::Submit {
                content,
                attachments,
                mode,
            } => {
                self.requests.push(Request::SubmitPrompt {
                    content: content.clone(),
                    attachments: attachments.clone(),
                    mode,
                });
                // Composer::add_attachment already keeps paths unique, so the
                // list arrives deduplicated.
                let item = ConversationItem {
                    id: format!("prompt-{}", self.engine.get().conversation.len()),
                    role: ConversationRole::User,
                    full_text: content,
                    streaming: false,
                    tool_name: None,
                    tool_report: None,
                    tool_details: None,
                    is_error: false,
                    model: None,
                    attachments,
                };
                self.apply(Action::UpsertConversation(item), window, cx);
            }
            ComposerEvent::Stop => {
                // Requested, not applied: the status has to follow what the agent
                // actually did. Setting it Ready here would show a stopped session
                // while the process kept streaming.
                self.requests.push(Request::Stop);
                cx.notify();
            }
            ComposerEvent::RemoveAttachment(path) => {
                self.composer.update(cx, |composer, cx| {
                    composer.remove_attachment(&path, cx);
                });
            }
            ComposerEvent::TextChanged(text) => {
                // The palette is a function of what is typed: no open/close state
                // to leave stale, so a `/` opens it and a backspace closes it.
                let state = self.engine.get().clone();
                self.palette.update(cx, |palette, cx| {
                    palette.sync(&state, &text, cx);
                });
            }
            ComposerEvent::AttachPaste(paste) => {
                // Straight through: the copied files have to be canonicalized and
                // the pasted bytes written somewhere Pi can read them, and both
                // need the store.
                self.requests.push(Request::AttachPaste(paste));
            }
        }
    }

    /// Act on what the palette reported.
    ///
    /// Most commands reach the backend, so they queue as requests. The exception
    /// is the pair the shell can answer itself.
    fn handle_palette_event(
        &mut self,
        event: PaletteEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            PaletteEvent::SetComposerText(text) => {
                self.composer.update(cx, |composer, cx| {
                    composer.set_text(&text, window, cx);
                });
                // Setting the value does not emit a change, so re-derive the
                // options here or the palette would keep showing the old list.
                let state = self.engine.get().clone();
                self.palette.update(cx, |palette, cx| {
                    palette.sync(&state, &text, cx);
                });
            }
            PaletteEvent::Run(command) => {
                self.composer.update(cx, |composer, cx| {
                    composer.set_text("", window, cx);
                });
                self.requests.push(Request::RunCommand(command));
            }
        }
        cx.notify();
    }

    /// Act on what the runtime controls reported.
    ///
    /// All three reach the agent over RPC, which lives behind `AgentRuntime`, so
    /// they queue as requests the same way the sidebar's do. A model switch is
    /// applied to state as well, since the picker has to keep showing the choice
    /// while it waits for the next prompt to take effect.
    fn handle_controls_event(
        &mut self,
        event: ControlsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ControlsEvent::SetModel(model) => {
                self.requests.push(Request::SetModel(model.clone()));
                self.apply(Action::SetPendingModel(Some(model)), window, cx);
            }
            ControlsEvent::SetThinkingLevel(level) => {
                self.requests.push(Request::SetThinkingLevel(level));
            }
            ControlsEvent::SetQueueModes {
                steering,
                follow_up,
            } => {
                self.requests.push(Request::SetQueueModes {
                    steering,
                    follow_up,
                });
            }
        }
    }

    /// Act on what the settings page reported.
    ///
    /// The preference toggles are applied through the reducer *and* queued as a
    /// request: the reducer makes the control reflect the click immediately, and
    /// the request writes it to the store. The provider and search-engine halves
    /// only queue, because the draft is not domain state until it is stored and
    /// read back.
    fn handle_settings_event(
        &mut self,
        event: SettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The preference toggles all take the same route, so which action and
        // which request each needs is decided by one function rather than by six
        // near-identical arms — and that function is assertable without a window.
        if let Some((action, request)) = preference_change(&event) {
            if let Some(action) = action {
                self.apply(action, window, cx);
            }
            self.requests.push(request);
            cx.notify();
            return;
        }

        match event {
            SettingsEvent::Close => {
                self.showing_settings = false;
                // Focus goes back to the prompt, not to whatever the settings
                // page left it on: the field it was in is no longer rendered, and
                // focus on an absent element means the next keystroke reaches
                // nothing at all.
                self.focus_composer(window, cx);
            }
            SettingsEvent::Show(section) => {
                self.settings
                    .update(cx, |settings, cx| settings.show(section, cx));
            }

            SettingsEvent::SelectProvider(id) => {
                self.settings.update(cx, |settings, cx| {
                    settings.edit_provider(id, window, cx);
                });
            }
            SettingsEvent::NewProvider => {
                self.settings
                    .update(cx, |settings, cx| settings.new_provider(window, cx));
            }
            SettingsEvent::SelectPreset(preset) => {
                self.settings.update(cx, |settings, cx| {
                    settings.apply_preset(preset, window, cx);
                });
            }
            SettingsEvent::SetProtocol(protocol) => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_protocol(protocol, window, cx);
                });
            }
            SettingsEvent::AddManualModel => {
                self.settings
                    .update(cx, |settings, cx| settings.add_manual_model(window, cx));
            }
            SettingsEvent::RemoveModel(id) => {
                self.settings
                    .update(cx, |settings, cx| settings.remove_model(&id, cx));
            }
            SettingsEvent::DiscoverModels => {
                let draft = self.settings.read(cx).provider_draft().clone();
                self.requests.push(Request::DiscoverProviderModels {
                    profile_id: draft.id,
                    provider_name: draft.name.trim().to_owned(),
                    base_url: draft.base_url.trim().to_owned(),
                    protocol: draft.protocol,
                    // Only what was typed: a stored key is looked up by the
                    // backend, which is the only side that can read the keychain.
                    api_key: draft.typed_api_key(),
                });
            }
            SettingsEvent::SaveProvider => {
                let draft = self.settings.read(cx).provider_draft().clone();
                self.requests.push(Request::SaveProvider {
                    profile: draft.to_profile(now_ms()),
                    api_key: draft.typed_api_key(),
                });
            }
            SettingsEvent::DeleteProvider(id) => {
                self.requests.push(Request::DeleteProvider(id));
                self.settings
                    .update(cx, |settings, cx| settings.new_provider(window, cx));
            }

            SettingsEvent::SelectSearchEngine(profile) => {
                self.settings.update(cx, |settings, cx| {
                    settings.edit_search_engine(&profile, window, cx);
                });
            }
            SettingsEvent::SetSearchEngineEnabled(enabled) => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_search_engine_enabled(enabled, cx);
                });
            }
            SettingsEvent::SaveSearchEngines(profiles) => {
                self.requests.push(Request::SaveSearchEngines(profiles));
            }
            SettingsEvent::SaveSearchEngine => {
                let profiles = self.settings.read(cx).search_engines_with_draft();
                self.requests.push(Request::SaveSearchEngines(profiles));
            }
            SettingsEvent::TestSearchEngine => {
                // Tested as it would be stored — trimmed URL, chosen protocol —
                // so a pass means the saved instance works, not just the typing.
                let draft = self.settings.read(cx).search_engine_draft().clone();
                let position = self.engine.get().search_engine_profiles.len() as u32;
                self.requests
                    .push(Request::TestSearchEngine(draft.to_profile(position)));
            }
            SettingsEvent::RemoveSearchEngine(index) => {
                let profiles = self.settings.read(cx).search_engines_without(index);
                self.requests.push(Request::SaveSearchEngines(profiles));
                self.settings.update(cx, |settings, cx| {
                    settings.clear_search_engine_draft(window, cx);
                });
            }
            SettingsEvent::MoveSearchEngine { index, delta } => {
                let profiles = self.settings.read(cx).search_engines_moved(index, delta);
                self.requests.push(Request::SaveSearchEngines(profiles));
            }

            // Handled above by `preference_change`, which returned `Some` for
            // exactly these and returned early.
            SettingsEvent::SetLanguage(_)
            | SettingsEvent::SetAutoCompaction(_)
            | SettingsEvent::SetBashPolicy(_)
            | SettingsEvent::SetBlockedPatterns(_)
            | SettingsEvent::SetAgentTeamConfig(_)
            | SettingsEvent::SetQueueModes { .. } => unreachable!("taken by preference_change"),
        }
        cx.notify();
    }

    /// Push the current rows into the sidebar.
    fn sync_sidebar(&mut self, cx: &mut Context<Self>) {
        let rows = chat::rows(self.engine.get(), &self.expanded_projects);
        let tokens = self.tokens;
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_tokens(tokens, cx);
            sidebar.set_rows(rows, cx);
        });
    }

    /// Push the current entries into the conversation.
    fn sync_conversation(&mut self, cx: &mut Context<Self>) {
        let messages = chat::visible_messages(self.engine.get());
        let tokens = self.tokens;
        self.conversation.update(cx, |conversation, cx| {
            conversation.set_tokens(tokens, cx);
            conversation.set_messages(messages, cx);
        });
    }

    /// Refresh the panes after state changed.
    fn sync_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_sidebar(cx);
        self.sync_conversation(cx);

        let tokens = self.tokens;
        let busy = matches!(
            self.engine.get().session_status,
            SessionStatus::Streaming | SessionStatus::Compacting
        );
        self.composer.update(cx, |composer, cx| {
            composer.set_tokens(tokens, cx);
            composer.set_busy(busy, cx);
        });

        // The controls reseed from state wholesale: which models exist, which
        // thinking levels this one offers, and what is selected all change
        // together when the agent reports back.
        let state = self.engine.get().clone();
        self.controls.update(cx, |controls, cx| {
            controls.set_tokens(tokens, cx);
            controls.sync(&state, window, cx);
        });

        self.prompts
            .update(cx, |prompts, cx| prompts.set_tokens(tokens, cx));
        self.rename
            .update(cx, |rename, cx| rename.set_tokens(tokens, cx));
        self.settings.update(cx, |settings, cx| {
            settings.set_tokens(tokens, cx);
            settings.sync(&state, window, cx);
        });
    }

    /// Whether the settings page is showing instead of the chat panes.
    pub fn showing_settings(&self) -> bool {
        self.showing_settings
    }

    /// Show or hide the settings page.
    pub fn show_settings(&mut self, showing: bool, cx: &mut Context<Self>) {
        self.showing_settings = showing;
        cx.notify();
    }

    /// Report what a provider said it offers.
    pub fn set_discovered_models(
        &mut self,
        models: Vec<pi_whim_core::ProviderModel>,
        cx: &mut Context<Self>,
    ) {
        self.settings.update(cx, |settings, cx| {
            settings.set_discovered_models(models, cx)
        });
    }

    /// Note the id a freshly stored provider was given, and whether its key
    /// actually landed in the keychain.
    pub fn provider_saved(
        &mut self,
        id: ProviderId,
        key_saved: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings.update(cx, |settings, cx| {
            settings.provider_saved(id, cx);
            settings.set_provider_key_status(id, key_saved, window, cx);
        });
    }

    /// Read-only domain state, for rendering.
    pub fn state(&self) -> &AppState {
        self.engine.get()
    }

    /// Apply `action` through the reducer.
    ///
    /// View-local follow-ups arrive as a [`ViewEffect`]; the shell currently
    /// caches nothing per message, so there is nothing to invalidate yet.
    pub fn apply(&mut self, action: Action, window: &mut Window, cx: &mut Context<Self>) {
        self.reduce(action, cx);
        self.sync_views(window, cx);
        cx.notify();
    }

    /// Apply a run of actions, syncing the views once at the end.
    ///
    /// What a translated agent event produces is several actions at a time, and
    /// `apply` would rebuild every view between each one — the sidebar rows and
    /// the conversation both get rebuilt per call, so a streaming turn would pay
    /// that several times per token.
    pub fn apply_all(
        &mut self,
        actions: impl IntoIterator<Item = Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut any = false;
        for action in actions {
            self.reduce(action, cx);
            any = true;
        }
        if !any {
            // Nothing changed, so nothing to rebuild or redraw.
            return;
        }
        self.sync_views(window, cx);
        cx.notify();
    }

    /// Run one action through the reducer and handle its view-local follow-up.
    ///
    /// Split from `apply` so a batch can share one sync. Deliberately does not
    /// sync or notify: the caller decides when the views are rebuilt.
    fn reduce(&mut self, action: Action, cx: &mut Context<Self>) {
        match self.engine.apply(action) {
            Some(ViewEffect::ConversationCleared) => {
                self.conversation
                    .update(cx, |conversation, cx| conversation.clear(cx));
            }
            // The settings page owns the provider and search-engine drafts these
            // describe, and sync_views rebuilds the project rows regardless, so
            // there is nothing for the shell to do with these yet.
            Some(
                ViewEffect::ProvidersReloaded(_)
                | ViewEffect::SearchEnginesReloaded(_)
                | ViewEffect::ProjectsLoaded(_),
            )
            | None => {}
        }
    }

    pub fn mode(&self) -> ThemeMode {
        self.tokens.mode
    }

    /// Switch to the other theme, pinning the preference so the choice sticks
    /// rather than being overwritten on the next appearance change.
    pub fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let next = self.tokens.mode.toggled();
        self.preference = ThemePreference::Fixed(next);
        self.tokens = Tokens::new(next);
        crate::theme::reapply(next, Some(window), cx);
        self.sync_views(window, cx);
        cx.notify();
    }

    /// Re-resolve after the system appearance changed. A pinned preference
    /// ignores it.
    pub fn system_appearance_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(self.preference, ThemePreference::System) {
            return;
        }
        // gpui-component already maps the platform appearance onto its own mode
        // enum, including the Vibrant variants, so defer to that.
        let system = match ComponentMode::from(window.appearance()) {
            ComponentMode::Dark => ThemeMode::Dark,
            ComponentMode::Light => ThemeMode::Light,
        };
        if system == self.tokens.mode {
            return;
        }
        self.tokens = Tokens::new(system);
        crate::theme::reapply(system, Some(window), cx);
        self.sync_views(window, cx);
        cx.notify();
    }
}

/// The banner a session status calls for, if any.
///
/// Failure takes precedence: if the session has broken, that matters more than
/// reporting that it is busy.
fn banner_for(status: &SessionStatus, tokens: Tokens) -> Option<Banner> {
    match status {
        SessionStatus::Failed(error) => Some(Banner::error(error.clone(), tokens)),
        SessionStatus::Compacting => Some(Banner::progress("Compacting the conversation…", tokens)),
        _ => None,
    }
}

/// The reducer action and backend request a preference change needs, if `event`
/// is one.
///
/// Returns `None` for the provider and search-engine events, which are drafts
/// rather than preferences and need the settings view rather than a table.
///
/// Most of these come in pairs: the action makes the control show the click at
/// once, and the request writes it where a restart will find it. Applying without
/// persisting would silently forget the choice; persisting without applying would
/// leave the control showing the old value until the store answered.
fn preference_change(event: &SettingsEvent) -> Option<(Option<Action>, Request)> {
    match event {
        SettingsEvent::SetLanguage(language) => Some((
            Some(Action::SetLanguage(*language)),
            Request::PersistLanguage(*language),
        )),
        // The only one with no action: auto-compaction is the agent's setting, not
        // the store's, and it comes back through `RuntimeControlsUpdated`. Guessing
        // it locally would show the switch flipped even if the agent refused.
        SettingsEvent::SetAutoCompaction(enabled) => {
            Some((None, Request::SetAutoCompaction(*enabled)))
        }
        SettingsEvent::SetBashPolicy(policy) => Some((
            Some(Action::SetBashPolicy(*policy)),
            Request::PersistBashPolicy(*policy),
        )),
        SettingsEvent::SetBlockedPatterns(patterns) => Some((
            Some(Action::SetBashBlockedPatterns(patterns.clone())),
            Request::PersistBlockedPatterns(patterns.clone()),
        )),
        SettingsEvent::SetAgentTeamConfig(config) => Some((
            Some(Action::SetAgentTeamConfig(config.clone())),
            Request::PersistAgentTeamConfig(config.clone()),
        )),
        // Queue modes are the agent's too, and the controls bar sends the same
        // request from beside the prompt.
        SettingsEvent::SetQueueModes {
            steering,
            follow_up,
        } => Some((
            None,
            Request::SetQueueModes {
                steering: *steering,
                follow_up: *follow_up,
            },
        )),
        _ => None,
    }
}

/// Toggle whether `id`'s sessions are listed, returning the new state.
fn toggle_expanded(expanded: &mut BTreeSet<ProjectId>, id: ProjectId) -> bool {
    if expanded.remove(&id) {
        false
    } else {
        expanded.insert(id);
        true
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        // Drained here rather than at the report site: pushing onto the window's
        // notification stack needs a window, and callers report failures from
        // places that have none — including before the first render.
        if !self.notices.is_empty() {
            crate::dialogs::show_notices(&mut self.notices, window, cx);
        }
        let state = self.engine.get();
        let status = state.session_status.clone();

        div()
            .size_full()
            // pi.dev fixes its graph paper to the page and lets the panels sit
            // over it translucently, so it belongs here rather than inside any
            // one pane. This box is the containing block it positions against.
            .relative()
            // Captured rather than merely observed: this runs before the focused
            // input handles the key, which is what lets an arrow move the palette
            // selection instead of the caret, and Enter run the highlighted
            // command instead of submitting the prompt. The composer keeps focus
            // throughout, so typing still filters.
            .capture_key_down(cx.listener(|workspace, event, _, cx| {
                let consumed = workspace
                    .palette
                    .update(cx, |palette, cx| palette.handle_key(event, cx));
                if consumed {
                    cx.stop_propagation();
                }
            }))
            .bg(tokens.bg_canvas.hsla())
            .child(GraphPaper::new(tokens))
            .child(
                div()
                    .relative()
                    .size_full()
                    .flex()
                    .flex_col()
                    .text_color(tokens.text.hsla())
                    .text_size(px(text::BODY_SIZE))
                    .child(
                        TopBar::new(status.clone(), tokens.mode, tokens)
                            .metrics(state.session_metrics.as_ref())
                            .on_toggle_theme(cx.listener(|workspace, _, window, cx| {
                                workspace.toggle_theme(window, cx);
                            }))
                            .on_open_settings(cx.listener(|workspace, _, _, cx| {
                                workspace.showing_settings = true;
                                cx.notify();
                            })),
                    )
                    // Settings replaces everything below the top bar, including
                    // the controls: they configure the running agent, and there
                    // is nothing to steer while the page is open.
                    .when(self.showing_settings, |this| {
                        this.child(div().flex_1().min_h(px(0.0)).child(self.settings.clone()))
                    })
                    .when(!self.showing_settings, |this| {
                        this
                            // Below the banner and above the panes: these
                            // configure the agent, so they belong with the window
                            // chrome rather than beside the prompt.
                            .child(self.controls.clone())
                            .when_some(banner_for(&status, tokens), |this, banner| {
                                this.child(banner)
                            })
                            // The conversation and sidebar fill whatever the
                            // chrome leaves.
                            .child(
                                div()
                                    .flex_1()
                                    .flex()
                                    .min_h(px(0.0))
                                    .child(self.sidebar.clone())
                                    .child(
                                        div()
                                            .flex_1()
                                            .flex()
                                            .flex_col()
                                            .min_w(px(0.0))
                                            .child(self.conversation.clone())
                                            .child(
                                                // The palette is absolutely
                                                // positioned against this box so it
                                                // floats over the conversation
                                                // rather than pushing the input
                                                // down as it grows.
                                                div()
                                                    .relative()
                                                    .flex()
                                                    .flex_col()
                                                    .items_center()
                                                    .child(self.palette.clone())
                                                    .child(self.composer.clone()),
                                            ),
                                    ),
                            )
                    }),
            )
            // Modals last, so they paint over the panes. Each renders nothing
            // when it has nothing to ask.
            .child(self.prompts.clone())
            .child(self.rename.clone())
    }
}

#[cfg(test)]
mod tests {
    use pi_whim_engine::mailbox::{RuntimeEvent, SessionToken};

    use super::*;
    use crate::chrome::Severity;

    #[test]
    fn an_idle_session_shows_no_banner() {
        let tokens = Tokens::light();
        assert!(banner_for(&SessionStatus::Offline, tokens).is_none());
        assert!(banner_for(&SessionStatus::Ready, tokens).is_none());
        assert!(banner_for(&SessionStatus::Streaming, tokens).is_none());
    }

    #[test]
    fn compaction_shows_a_progress_banner() {
        let banner = banner_for(&SessionStatus::Compacting, Tokens::light())
            .expect("a banner while compacting");
        assert_eq!(banner.severity(), Severity::Progress);
    }

    #[test]
    fn failure_shows_an_error_banner() {
        // A broken session matters more than reporting that it is busy, so this
        // is the variant that wins when both could apply.
        let banner = banner_for(&SessionStatus::Failed("boom".into()), Tokens::light())
            .expect("a banner after failure");
        assert_eq!(banner.severity(), Severity::Error);
    }

    #[test]
    fn toggling_a_project_flips_it_and_back() {
        let mut expanded = BTreeSet::new();
        let id = uuid::Uuid::new_v4();

        assert!(toggle_expanded(&mut expanded, id));
        assert!(expanded.contains(&id));

        assert!(!toggle_expanded(&mut expanded, id));
        assert!(!expanded.contains(&id));
    }

    #[test]
    fn a_stored_preference_is_both_applied_and_persisted() {
        // Applying without persisting forgets the choice on restart; persisting
        // without applying leaves the control showing the old value.
        let (action, request) =
            preference_change(&SettingsEvent::SetBashPolicy(BashPolicy::Deny)).expect("a change");
        assert_eq!(action, Some(Action::SetBashPolicy(BashPolicy::Deny)));
        assert_eq!(request, Request::PersistBashPolicy(BashPolicy::Deny));
    }

    #[test]
    fn the_agents_own_settings_are_not_applied_locally() {
        // Auto-compaction and the queue modes are the agent's to confirm. Guessing
        // them here would show the switch flipped even if the agent refused.
        let (action, _) =
            preference_change(&SettingsEvent::SetAutoCompaction(true)).expect("a change");
        assert_eq!(action, None);

        let (action, _) = preference_change(&SettingsEvent::SetQueueModes {
            steering: QueueMode::All,
            follow_up: QueueMode::OneAtATime,
        })
        .expect("a change");
        assert_eq!(action, None);
    }

    #[test]
    fn drafts_are_not_preference_changes() {
        // These need the settings view, not the table: a draft is not domain state
        // until it is stored and read back.
        assert!(preference_change(&SettingsEvent::NewProvider).is_none());
        assert!(preference_change(&SettingsEvent::SaveProvider).is_none());
        assert!(preference_change(&SettingsEvent::SaveSearchEngine).is_none());
        assert!(preference_change(&SettingsEvent::Close).is_none());
    }

    #[test]
    fn projects_expand_independently() {
        let mut expanded = BTreeSet::new();
        let first = uuid::Uuid::new_v4();
        let second = uuid::Uuid::new_v4();

        toggle_expanded(&mut expanded, first);
        toggle_expanded(&mut expanded, second);
        toggle_expanded(&mut expanded, first);

        assert!(!expanded.contains(&first));
        assert!(expanded.contains(&second));
    }

    /// A shell in a headless window, for the tests that need one.
    ///
    /// Goes through the crate's own `init` so the shell is built the way the app
    /// builds it, rather than against a second setup path that could drift.
    fn shell(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Workspace> {
        let preference = ThemePreference::default();
        cx.update(|cx| {
            crate::init(preference, cx).expect("the bundled fonts load");
        });
        cx.add_window(|window, cx| Workspace::new(preference, window, cx))
    }

    #[gpui::test]
    async fn a_session_event_reaches_the_shell_without_a_frame_loop(cx: &mut gpui::TestAppContext) {
        // The point of the pump: nothing polls, and the event still arrives.
        let shell = shell(cx);
        let (sender, events) = crossbeam_channel::unbounded();
        let token = SessionToken::next();
        shell
            .update(cx, |workspace, window, cx| {
                workspace.listen(events, window, cx)
            })
            .expect("the window is open");

        sender
            .send((token, RuntimeEvent::Stderr("boom".into())))
            .expect("the pump is listening");
        // Dropping the sender ends the blocking wait after this batch, so the
        // test's scheduler has nothing left parked on it.
        drop(sender);
        cx.run_until_parked();

        let delivered = shell
            .update(cx, |workspace, _, _| workspace.take_deliveries())
            .expect("the window is open");
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].0, token);
    }

    #[gpui::test]
    async fn taking_deliveries_leaves_none_behind(cx: &mut gpui::TestAppContext) {
        // Handling an event twice would replay it into the conversation.
        let shell = shell(cx);
        let (sender, events) = crossbeam_channel::unbounded();
        shell
            .update(cx, |workspace, window, cx| {
                workspace.listen(events, window, cx)
            })
            .expect("the window is open");

        sender
            .send((SessionToken::next(), RuntimeEvent::Stderr("once".into())))
            .expect("the pump is listening");
        drop(sender);
        cx.run_until_parked();

        shell
            .update(cx, |workspace, _, _| {
                assert_eq!(workspace.take_deliveries().len(), 1);
                assert!(workspace.take_deliveries().is_empty());
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn listening_again_replaces_the_running_pump(cx: &mut gpui::TestAppContext) {
        // Two pumps on two streams would deliver a session's events twice over
        // once the old stream was re-created. Replacing drops the old task, and a
        // dropped task is a cancelled one.
        let shell = shell(cx);
        let (stale_sender, stale_events) = crossbeam_channel::unbounded();
        let (live_sender, live_events) = crossbeam_channel::unbounded();
        shell
            .update(cx, |workspace, window, cx| {
                workspace.listen(stale_events, window, cx);
                workspace.listen(live_events, window, cx);
                assert!(workspace.is_listening());
            })
            .expect("the window is open");

        stale_sender
            .send((SessionToken::next(), RuntimeEvent::Stderr("stale".into())))
            .expect("the channel is open");
        drop(stale_sender);
        drop(live_sender);
        cx.run_until_parked();

        let delivered = shell
            .update(cx, |workspace, _, _| workspace.take_deliveries())
            .expect("the window is open");
        assert!(delivered.is_empty());
    }

    #[gpui::test]
    async fn a_batch_of_actions_all_land(cx: &mut gpui::TestAppContext) {
        // What a translated agent event produces is several actions at a time, so
        // dropping any of them would leave the view describing a state the engine
        // is not in.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                workspace.apply_all(
                    [
                        Action::SetSessionStatus(SessionStatus::Streaming),
                        Action::SetPendingModel(Some(ModelOption {
                            provider: "anthropic".into(),
                            provider_name: "Anthropic".into(),
                            id: "sonnet".into(),
                            name: "Sonnet".into(),
                        })),
                    ],
                    window,
                    cx,
                );

                assert_eq!(workspace.state().session_status, SessionStatus::Streaming);
                assert!(workspace.state().pending_model.is_some());
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn a_submitted_prompt_is_shown_and_sent(cx: &mut gpui::TestAppContext) {
        // Both halves matter: shown immediately so the prompt does not seem to
        // vanish while Pi starts, and sent because otherwise it only looks sent.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                workspace.handle_composer_event(
                    ComposerEvent::Submit {
                        content: "what changed?".to_owned(),
                        attachments: Vec::new(),
                        mode: SubmitMode::Prompt,
                    },
                    window,
                    cx,
                );

                let shown = workspace.state().conversation.last().expect("the prompt");
                assert_eq!(shown.full_text, "what changed?");
                assert_eq!(shown.role, ConversationRole::User);
                assert!(matches!(
                    workspace.take_requests().as_slice(),
                    [Request::SubmitPrompt { content, .. }] if content == "what changed?"
                ));
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn stopping_asks_the_agent_rather_than_reporting_it_stopped(
        cx: &mut gpui::TestAppContext,
    ) {
        // The status has to follow what the process did. Flipping it here would
        // show a stopped session while the agent kept streaming.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                workspace.apply(
                    Action::SetSessionStatus(SessionStatus::Streaming),
                    window,
                    cx,
                );

                workspace.handle_composer_event(ComposerEvent::Stop, window, cx);

                assert_eq!(workspace.state().session_status, SessionStatus::Streaming);
                assert_eq!(workspace.take_requests(), vec![Request::Stop]);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn opening_a_session_selects_it_and_asks_to_bind_the_process(
        cx: &mut gpui::TestAppContext,
    ) {
        // Without the request the sidebar selection would move while the
        // conversation kept showing the session that was open before.
        let shell = shell(cx);
        let project_id = uuid::Uuid::new_v4();

        shell
            .update(cx, |workspace, window, cx| {
                workspace.handle_sidebar_event(
                    SidebarEvent::OpenSession {
                        project_id,
                        pi_path: "/tmp/s.jsonl".to_owned(),
                    },
                    window,
                    cx,
                );

                assert_eq!(workspace.state().selected_project, Some(project_id));
                assert_eq!(
                    workspace.take_requests(),
                    vec![Request::ActivateSession {
                        project_id,
                        path: "/tmp/s.jsonl".to_owned(),
                    }]
                );
            })
            .expect("the window is open");
    }
}
