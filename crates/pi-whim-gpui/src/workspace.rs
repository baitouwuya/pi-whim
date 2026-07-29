//! Root view.
//!
//! Owns the domain state and the resolved theme, and arranges the chrome around
//! the space the conversation and sidebar will fill. Those two, along with the
//! settings page, land as their own modules.

use std::collections::BTreeSet;

use gpui::{
    AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement, ParentElement,
    Render, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_core::{
    AgentPermissionLevel, AgentTeamConfig, AppState, Attachment, BashPolicy, Language, ModelOption,
    ProjectId, ProviderId, ProviderProfile, ProviderProtocol, QueueMode, SearchEngineProfile,
    SessionStatus, SubmitMode, ThinkingLevel, strings::text as translate,
};
use pi_whim_engine::dialogs::{Answer, Prompt};
use pi_whim_engine::notice::Outbox;
use pi_whim_engine::session::now_ms;
use pi_whim_engine::slash_commands::SlashCommand;
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, text};

use crate::{
    chat::{
        self, Composer, ComposerEvent, Controls, ControlsEvent, Conversation, ConversationEvent,
        Palette, PaletteEvent, Paste, QueueStatus, Sidebar, SidebarEvent,
    },
    chrome::{Banner, TopBar},
    dialogs::{PromptEvent, Prompts, Rename, RenameEvent},
    elements::GraphPaper,
    settings::{Settings, SettingsEvent},
    theme::IntoHsla,
};

/// Space around the prompt box.
///
/// `GAP` is the larger of the two: the distance from the last message to the top
/// of the prompt has to read as a separation between two regions, while the margin
/// to the window's edges only has to stop the border touching them.
const PROMPT_GAP: f32 = 16.0;
const PROMPT_MARGIN: f32 = 10.0;

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
    /// Show a project and make sure it has a running session.
    ///
    /// Separate from [`Request::NewSession`] because opening reuses whatever is
    /// already there — only an empty project gets a new process.
    OpenProject(ProjectId),
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
    ///
    /// Kept as a request rather than written here: gpui can write the clipboard,
    /// but what goes on it — a session id, the last reply — is decided beside the
    /// data, and the host is where that is.
    CopyToClipboard(String),
    /// Move a session's transcript to the trash.
    DeleteSession(String),
    /// Send a decision back to the agent that asked for it.
    AnswerPrompt(Answer),
    /// Turn a paste into an attachment. Copied files need canonicalizing and
    /// pasted bytes need writing, both of which need the attachment store.
    AttachPaste(Paste),
    /// Ask for things on disk to attach — files, folders, or both.
    ///
    /// The picker is the platform's, and opening one needs the window, so this
    /// crosses the boundary rather than happening where a view is rendered.
    PickAttachments,
    /// Delete an attachment the app wrote, now that the draft has dropped it.
    ///
    /// Only for the generated ones — a pasted image, a long paste saved to a file.
    /// A file the reader attached from disk is theirs, and removing it from the
    /// draft must not remove it from their computer.
    DiscardAttachment(String),
    /// Send the drafted prompt to the agent.
    ///
    /// The application owns both the optimistic transcript entry and the RPC so
    /// the visible message cannot diverge from the accepted submission.
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

    // The settings page's requests. Preferences are stored as well as applied,
    // and some of them are process launch flags, so each of these is a change the
    // host makes rather than one the shell can make and report.
    SetLanguage(Language),
    SetBashPolicy(BashPolicy),
    SetBlockedPatterns(Vec<String>),
    /// Change the default permission for agents spawned from live sessions.
    ///
    /// Kept separate from `SetAgentTeamConfig`: this setting can be applied to
    /// running supervisors without restarting Pi or disturbing a turn.
    SetPermissionLevel(AgentPermissionLevel),
    SetAgentTeamConfig(AgentTeamConfig),
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
    /// Store one search engine and its newly typed credential, if any.
    SaveSearchEngine {
        profile: SearchEngineProfile,
        api_key: Option<String>,
    },
    /// Check that a configured search adapter returns valid results.
    TestSearchEngine {
        profile: SearchEngineProfile,
        api_key: Option<String>,
    },
    /// Ask a provider which models it has.
    DiscoverProviderModels {
        profile_id: Option<ProviderId>,
        provider_name: String,
        base_url: String,
        protocol: ProviderProtocol,
        api_key: Option<String>,
    },
}

/// The shell reporting that it has something for the host to carry out.
///
/// A bare signal rather than the request itself: it is already on a queue the
/// host drains, and carrying it in the event as well would give two paths to the
/// same work. Notification is not enough on its own — the host answers by handing
/// back a snapshot, which notifies again, and an observer would loop.
pub struct RequestsRaised;

/// The application shell.
pub struct Workspace {
    preference: ThemePreference,
    tokens: Tokens,
    /// What is on screen.
    ///
    /// A projection, not the truth: the host owns the reducer and hands whole
    /// snapshots to [`Workspace::set_state`]. A second reducer here could
    /// disagree with the one the agent's events go through.
    state: AppState,
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
    /// The failure banner the reader dismissed, until the status changes.
    dismissed_error: Option<String>,
    /// Projects whose sessions are listed. View-local: which projects a reader
    /// has open says nothing about the session.
    expanded_projects: BTreeSet<ProjectId>,
    /// Requests waiting for the backend owner to drain.
    requests: Vec<Request>,
}

impl EventEmitter<RequestsRaised> for Workspace {}

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
        let sidebar = cx.new(|cx| Sidebar::new(tokens, window, cx));
        cx.subscribe_in(&sidebar, window, |workspace, _, event, window, cx| {
            workspace.handle_sidebar_event(event.clone(), window, cx);
        })
        .detach();

        let conversation = cx.new(|_| Conversation::new(tokens));
        cx.subscribe(&conversation, |workspace, conversation, event, cx| {
            match event {
                ConversationEvent::ToggleToolReport(id) => {
                    let id = id.clone();
                    conversation.update(cx, |conversation, cx| {
                        conversation.toggle_tool_report(&id, cx);
                    });
                }
                ConversationEvent::ToggleToolDetails(id) => {
                    let id = id.clone();
                    conversation.update(cx, |conversation, cx| {
                        conversation.toggle_tool_details(&id, cx);
                    });
                }
                ConversationEvent::ToggleThinking { id, segment } => {
                    let id = id.clone();
                    let segment = *segment;
                    conversation.update(cx, |conversation, cx| {
                        conversation.toggle_thinking(&id, segment, cx);
                    });
                }
                ConversationEvent::RevealAll(id) => {
                    let id = id.clone();
                    conversation.update(cx, |conversation, cx| {
                        conversation.reveal_all(&id, cx);
                    });
                }
                ConversationEvent::ForkAt(id) => {
                    // The backend owns the transcript and Pi's `fork` request.
                    // Routing through the existing slash command keeps session
                    // re-keying and indexing in one place.
                    workspace.request(Request::RunCommand(SlashCommand::Fork(id.clone())), cx);
                }
                ConversationEvent::CopyAssistant(id) => {
                    let text = conversation
                        .read(cx)
                        .messages()
                        .iter()
                        .find(|message| message.id == *id)
                        .map(|message| message.full_text.clone());
                    if let Some(text) = text {
                        workspace.request(Request::CopyToClipboard(text), cx);
                    }
                }
            }
            cx.notify();
        })
        .detach();

        let composer = cx.new(|cx| Composer::new(tokens, window, cx));
        cx.subscribe(&composer, |workspace, _, event, cx| {
            workspace.handle_composer_event(event.clone(), cx);
        })
        .detach();

        let controls = cx.new(|cx| Controls::new(tokens, window, cx));
        cx.subscribe(&controls, |workspace, _, event, cx| {
            workspace.handle_controls_event(event.clone(), cx);
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
            workspace.request(Request::AnswerPrompt(answer.clone()), cx);
        })
        .detach();

        let rename = cx.new(|cx| Rename::new(tokens, window, cx));
        cx.subscribe(&rename, |workspace, _, event, cx| {
            let RenameEvent::Renamed { path, title } = event;
            workspace.request(
                Request::RenameSession {
                    path: path.clone(),
                    title: title.clone(),
                },
                cx,
            );
        })
        .detach();

        let state = AppState::default();
        let settings = cx.new(|cx| Settings::new(tokens, state.clone(), window, cx));
        cx.subscribe_in(&settings, window, |workspace, _, event, window, cx| {
            workspace.handle_settings_event(event.clone(), window, cx);
        })
        .detach();

        Self {
            preference,
            tokens,
            state,
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
            dismissed_error: None,
            expanded_projects: BTreeSet::new(),
            requests: Vec::new(),
        }
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

    /// Queue something for the host, and tell it there is something to take.
    ///
    /// Every request goes through here so the signal cannot be forgotten at one
    /// call site out of twenty-six — a queued request the host is never told about
    /// waits until the next unrelated one arrives.
    fn request(&mut self, request: Request, cx: &mut Context<Self>) {
        self.requests.push(request);
        cx.emit(RequestsRaised);
        cx.notify();
    }

    /// Put a rejected submission back into the prompt instead of losing it.
    pub fn restore_submission(
        &mut self,
        content: String,
        attachments: Vec<Attachment>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.composer.update(cx, |composer, cx| {
            composer.restore_draft(content, attachments, window, cx);
        });
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
            SidebarEvent::AddProject => self.request(Request::AddProject, cx),
            SidebarEvent::NewSession(id) => self.request(Request::NewSession(id), cx),
            SidebarEvent::ToggleProject(id) | SidebarEvent::OpenProject(id) => {
                // Whether the rows are listed is view-local, so it is decided
                // here; which project is selected is not, so it is asked for.
                // Both from one click, because a header that expanded without
                // selecting would leave the conversation on another project.
                toggle_expanded(&mut self.expanded_projects, id);
                self.request(Request::OpenProject(id), cx);
            }
            SidebarEvent::OpenSession {
                project_id,
                pi_path,
            } => {
                // Selection is not applied here either: the pool decides which
                // session is visible and the transcript has to be re-read, so a
                // local select would move the sidebar while the conversation kept
                // showing the previous session.
                self.request(
                    Request::ActivateSession {
                        project_id,
                        path: pi_path,
                    },
                    cx,
                );
            }
            // The rest of the row menu is the backend's: Finder, the store, the
            // clipboard, and the trash all sit behind the boundary this crate
            // does not cross.
            SidebarEvent::RevealProject(id) => self.request(Request::RevealProject(id), cx),
            SidebarEvent::RemoveProject(id) => self.request(Request::RemoveProject(id), cx),
            SidebarEvent::CloneSession => self.request(Request::CloneSession, cx),
            SidebarEvent::CopySessionId(id) => {
                self.request(Request::CopyToClipboard(id.to_string()), cx)
            }
            SidebarEvent::DeleteSession(path) => self.request(Request::DeleteSession(path), cx),
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
    /// A submitted prompt crosses the boundary as one request. The application
    /// adds the optimistic transcript entry and sends it to Pi only after the
    /// active session is revalidated.
    fn handle_composer_event(&mut self, event: ComposerEvent, cx: &mut Context<Self>) {
        match event {
            ComposerEvent::Submit {
                content,
                attachments,
                mode,
            } => {
                // Composer::add_attachment already keeps paths unique, so the
                // list arrives deduplicated. The application owns transcript
                // insertion so the sent and shown messages cannot diverge.
                self.request(
                    Request::SubmitPrompt {
                        content,
                        attachments,
                        mode,
                    },
                    cx,
                );
            }
            ComposerEvent::Stop => {
                // Requested, not applied: the status has to follow what the agent
                // actually did. Setting it Ready here would show a stopped session
                // while the process kept streaming.
                self.request(Request::Stop, cx);
            }
            ComposerEvent::RemoveAttachment(path) => {
                // The draft is view-local, so the row goes now. The file behind it
                // is not: only the ones the app generated are its to delete, and
                // only it knows which those are.
                let generated = self
                    .composer
                    .read(cx)
                    .attachments()
                    .iter()
                    .any(|attachment| attachment.path == path && attachment.generated_by_app);
                self.composer.update(cx, |composer, cx| {
                    composer.remove_attachment(&path, cx);
                });
                if generated {
                    self.request(Request::DiscardAttachment(path), cx);
                }
            }
            ComposerEvent::TextChanged(text) => {
                // The palette is a function of what is typed: no open/close state
                // to leave stale, so a `/` opens it and a backspace closes it.
                let state = self.state.clone();
                self.palette.update(cx, |palette, cx| {
                    palette.sync(&state, &text, cx);
                });
            }
            ComposerEvent::AttachPaste(paste) => {
                // Straight through: the copied files have to be canonicalized and
                // the pasted bytes written somewhere Pi can read them, and both
                // need the store.
                self.request(Request::AttachPaste(paste), cx);
            }
            ComposerEvent::PickAttachments => {
                self.request(Request::PickAttachments, cx);
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
                let state = self.state.clone();
                self.palette.update(cx, |palette, cx| {
                    palette.sync(&state, &text, cx);
                });
            }
            PaletteEvent::Run(command) => {
                self.composer.update(cx, |composer, cx| {
                    composer.set_text("", window, cx);
                });
                self.request(Request::RunCommand(command), cx);
            }
        }
        cx.notify();
    }

    /// Act on what the runtime controls reported.
    ///
    /// All three cross the runtime boundary: model and thinking use Pi RPC while
    /// permission updates the live agent supervisor. They queue as requests the
    /// same way the sidebar's do.
    fn handle_controls_event(&mut self, event: ControlsEvent, cx: &mut Context<Self>) {
        // No `cx.notify()` at the end: every arm here is a request, and `request`
        // notifies. The picker keeps showing the old value until the snapshot
        // arrives, which is the point.
        match event {
            ControlsEvent::SetModel(model) => {
                // The host records the choice as pending — a switch waits for the
                // next prompt so the prior model compacts the history first — and
                // the picker shows it when that snapshot comes back.
                self.request(Request::SetModel(model), cx);
            }
            ControlsEvent::SetPermissionLevel(level) => {
                self.request(Request::SetPermissionLevel(level), cx);
            }
            ControlsEvent::SetThinkingLevel(level) => {
                self.request(Request::SetThinkingLevel(level), cx);
            }
        }
    }

    /// Act on what the settings page reported.
    ///
    /// Everything that changes domain state leaves as a request: the host applies
    /// it and the snapshot comes back, so a preference cannot end up showing as
    /// set while the write that stores it failed. The provider and search-engine
    /// halves queue too, because a draft is not domain state until it is stored
    /// and read back.
    fn handle_settings_event(
        &mut self,
        event: SettingsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The preference toggles all take the same route, so which request each
        // needs is decided by one function rather than by six near-identical
        // arms — and that function is assertable without a window.
        if let Some(request) = preference_change(&event) {
            self.request(request, cx);
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
                self.request(
                    Request::DiscoverProviderModels {
                        profile_id: draft.id,
                        provider_name: draft.name.trim().to_owned(),
                        base_url: draft.base_url.trim().to_owned(),
                        protocol: draft.protocol,
                        // Only what was typed: a stored key is looked up by the
                        // backend, which is the only side that can read the keychain.
                        api_key: draft.typed_api_key(),
                    },
                    cx,
                );
            }
            SettingsEvent::SaveProvider => {
                let draft = self.settings.read(cx).provider_draft().clone();
                self.request(
                    Request::SaveProvider {
                        profile: draft.to_profile(now_ms()),
                        api_key: draft.typed_api_key(),
                    },
                    cx,
                );
            }
            SettingsEvent::DeleteProvider(id) => {
                self.request(Request::DeleteProvider(id), cx);
                self.settings
                    .update(cx, |settings, cx| settings.new_provider(window, cx));
            }

            SettingsEvent::SelectSearchEngine(profile) => {
                self.settings.update(cx, |settings, cx| {
                    settings.edit_search_engine(&profile, window, cx);
                });
            }
            SettingsEvent::NewSearchEngine => {
                self.settings.update(cx, |settings, cx| {
                    settings.new_search_engine(window, cx);
                });
            }
            SettingsEvent::CloseSearchEngineEditor => {
                self.settings.update(cx, |settings, cx| {
                    settings.close_search_engine_editor(window, cx);
                });
            }
            SettingsEvent::SetSearchEngineKind(kind) => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_search_engine_kind(kind, window, cx);
                });
            }
            SettingsEvent::SetSearchEngineEnabled(enabled) => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_search_engine_enabled(enabled, cx);
                });
            }
            SettingsEvent::SaveSearchEngines(profiles) => {
                self.request(Request::SaveSearchEngines(profiles), cx);
            }
            SettingsEvent::SaveSearchEngine => {
                let draft = self.settings.read(cx).search_engine_draft().clone();
                let position = draft
                    .id
                    .and_then(|id| {
                        self.state
                            .search_engine_profiles
                            .iter()
                            .find(|profile| profile.id == id)
                            .map(|profile| profile.position)
                    })
                    .unwrap_or(self.state.search_engine_profiles.len() as u32);
                self.request(
                    Request::SaveSearchEngine {
                        profile: draft.to_profile(position),
                        api_key: draft.typed_api_key(),
                    },
                    cx,
                );
            }
            SettingsEvent::TestSearchEngine => {
                // Tested as it would be stored — trimmed URL, chosen protocol —
                // so a pass means the saved instance works, not just the typing.
                let draft = self.settings.read(cx).search_engine_draft().clone();
                let position = self.state.search_engine_profiles.len() as u32;
                self.request(
                    Request::TestSearchEngine {
                        profile: draft.to_profile(position),
                        api_key: draft.typed_api_key(),
                    },
                    cx,
                );
            }
            SettingsEvent::RemoveSearchEngine(index) => {
                let profiles = self.settings.read(cx).search_engines_without(index);
                self.request(Request::SaveSearchEngines(profiles), cx);
                self.settings.update(cx, |settings, cx| {
                    settings.clear_search_engine_draft(window, cx);
                });
            }
            SettingsEvent::MoveSearchEngine { index, delta } => {
                let profiles = self.settings.read(cx).search_engines_moved(index, delta);
                self.request(Request::SaveSearchEngines(profiles), cx);
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
    fn sync_sidebar(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rows = chat::rows(&self.state, &self.expanded_projects);
        let search_rows = chat::searchable_rows(&self.state, &self.expanded_projects);
        let tokens = self.tokens;
        let language = self.state.language;
        self.sidebar.update(cx, |sidebar, cx| {
            sidebar.set_tokens(tokens, cx);
            sidebar.set_language(language, window, cx);
            sidebar.set_rows(rows, search_rows, cx);
        });
    }

    /// Push the current entries into the conversation.
    fn sync_conversation(&mut self, cx: &mut Context<Self>) {
        let messages = chat::visible_messages(&self.state);
        let tokens = self.tokens;
        let language = self.state.language;
        let has_project = self.state.selected_project.is_some();
        let generating = matches!(self.state.session_status, SessionStatus::Streaming);
        self.conversation.update(cx, |conversation, cx| {
            conversation.set_tokens(tokens, cx);
            conversation.set_language(language, cx);
            conversation.set_has_project(has_project, cx);
            conversation.set_generating(generating, cx);
            conversation.set_messages(messages, cx);
        });
    }

    /// Refresh the panes after state changed.
    fn sync_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_sidebar(window, cx);
        self.sync_conversation(cx);

        let tokens = self.tokens;
        let busy = matches!(
            self.state.session_status,
            SessionStatus::Streaming | SessionStatus::Compacting
        );
        let ready = self.state.selected_project.is_some()
            && matches!(
                self.state.session_status,
                SessionStatus::Ready | SessionStatus::Streaming | SessionStatus::Compacting
            );
        let language = self.state.language;
        self.composer.update(cx, |composer, cx| {
            composer.set_tokens(tokens, cx);
            composer.set_language(language, window, cx);
            composer.set_busy(busy, cx);
            composer.set_ready(ready, cx);
        });

        // The controls compare this snapshot with the values already applied.
        // That lets an open model menu preserve its search and scroll position
        // across unrelated transcript/status snapshots.
        let state = self.state.clone();
        self.controls.update(cx, |controls, cx| {
            controls.set_tokens(tokens, cx);
            controls.sync(&state, window, cx);
        });

        self.prompts.update(cx, |prompts, cx| {
            prompts.set_tokens(tokens, cx);
            prompts.set_language(language, cx);
        });
        self.rename.update(cx, |rename, cx| {
            rename.set_tokens(tokens, cx);
            rename.set_language(language, window, cx);
        });
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
        if showing {
            self.settings
                .update(cx, |settings, cx| settings.reset_scroll(cx));
        }
        self.showing_settings = showing;
        cx.notify();
    }

    /// Stage a file on the prompt draft.
    ///
    /// How the host answers [`Request::AttachPaste`] and the attach command: both
    /// end in a file the store has written or canonicalized, and the draft that
    /// carries it to the next prompt is the composer's.
    pub fn attach(&mut self, attachment: Attachment, cx: &mut Context<Self>) {
        self.composer.update(cx, |composer, cx| {
            composer.add_attachment(attachment, cx);
        });
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

    /// Close the editor only after metadata and any credential were stored.
    pub fn search_engine_saved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.settings.update(cx, |settings, cx| {
            settings.close_search_engine_editor(window, cx)
        });
    }

    /// Read-only domain state, for rendering.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Show `state` instead of what is on screen now.
    ///
    /// The whole snapshot rather than a diff: `sync_views` already hands each
    /// view its slice wholesale, so there is nothing to save by sending less.
    /// This is how the shell learns about anything it did not do itself — an
    /// agent's reply, a session it asked to be activated — without keeping a
    /// second reducer that could disagree with the one that owns the state.
    pub fn set_state(&mut self, state: AppState, window: &mut Window, cx: &mut Context<Self>) {
        // What the conversation caches per message — reveal progress, which tool
        // cards are open — is keyed by message id, so it has to go when the
        // messages it describes do. A changed session counts as much as an
        // emptied conversation: switching clears and reloads in one step, so the
        // empty moment in between never arrives as its own snapshot.
        let previous = &self.state;
        let switched = previous.selected_session != state.selected_session;
        let cleared = state.conversation.is_empty() && !previous.conversation.is_empty();
        let status_changed = previous.session_status != state.session_status;
        if switched || cleared {
            self.conversation
                .update(cx, |conversation, cx| conversation.clear(cx));
        }
        if status_changed {
            self.dismissed_error = None;
        }
        self.state = state;
        self.sync_views(window, cx);
        cx.notify();
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

    /// The prompt and everything that describes the turn it will start.
    ///
    /// One bordered box: the field on top, and beneath it a single row carrying
    /// attach, the permission level, the model, the thinking level, and send. The
    /// controls used to be a full-width bar under the top chrome, where they
    /// wrapped onto three rows and left most of each one empty; they describe the
    /// turn about to be sent, so this is where they belong.
    ///
    /// Assembled here rather than inside [`Composer`] because a single row cannot
    /// be split across two entities — the composer's own buttons share it with the
    /// controls.
    ///
    /// Square, like everything else with an edge: the whole app carries pi.dev's
    /// `border-radius: 0`, and the only round thing on this row is the permission
    /// dot.
    fn prompt_area(&self, tokens: Tokens, cx: &mut Context<Self>) -> impl IntoElement {
        let (attach, send) = self.composer.update(cx, |composer, cx| {
            (composer.attach_button(cx), composer.send_button(cx))
        });

        // One line, never wrapped. Attach sits alone on the left; everything that
        // describes the turn — permission, model, thinking — gathers at the right
        // beside send, so the settings read as one group next to the action they
        // apply to rather than trailing off from the attach button.
        let footer = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(attach)
            // Takes the slack, so the group stays at the right edge however wide
            // the window gets.
            .child(div().flex_1().min_w(px(0.0)))
            .child(div().flex_none().child(self.controls.clone()))
            .child(send);

        let queue_status = QueueStatus::from_state(&self.state, tokens);

        div()
            // The containing block the palette anchors against.
            .relative()
            .flex()
            .flex_col()
            .items_center()
            // Clear of the transcript, and of the window's edges. The prompt used
            // to sit flush against the last message and the bottom of the window,
            // which left the newest reply looking like part of the input.
            .px(px(PROMPT_MARGIN))
            .pb(px(PROMPT_MARGIN))
            .pt(px(PROMPT_GAP))
            .child(
                // `bottom_full` puts this box's bottom edge on the prompt's top
                // edge, so the list opens upward over the conversation. Absolute
                // rather than in the flow, where a growing list would push the
                // input down as the reader typed.
                div()
                    .absolute()
                    .bottom_full()
                    .mb(px(6.0))
                    .flex()
                    .justify_center()
                    .w_full()
                    .child(self.palette.clone()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .w_full()
                    .p(px(8.0))
                    .bg(tokens.panel.hsla())
                    // All four edges, not just the top: the field inside is
                    // borderless now, so this is the only thing saying where the
                    // prompt begins and ends.
                    .border_1()
                    .border_color(tokens.line.hsla())
                    .child(self.composer.clone())
                    .when_some(queue_status, |this, status| this.child(status))
                    .child(footer),
            )
    }
}

/// The banner a session status calls for, if any.
///
/// Failure takes precedence: if the session has broken, that matters more than
/// reporting that it is busy.
fn banner_for(status: &SessionStatus, language: Language, tokens: Tokens) -> Option<Banner> {
    match status {
        // The error text is the agent's own, so it travels through untranslated;
        // everything the app says around it does not.
        SessionStatus::Failed(error) => Some(Banner::error(error.clone(), tokens)),
        SessionStatus::Compacting => Some(
            Banner::progress(translate("compacting-banner", language), tokens)
                // The headline names the condition; the line under it says the
                // conversation survives it, which is the part worth knowing.
                .detail(translate("compacting-detail", language)),
        ),
        _ => None,
    }
}

/// The backend request a preference change needs, if `event` is one.
///
/// Returns `None` for the provider and search-engine events, which are drafts
/// rather than preferences and need the settings view rather than a table.
///
/// The host applies each of these and the snapshot comes back, so the control
/// shows what was actually stored rather than what was clicked. The two the agent
/// owns — auto-compaction and the queue modes — could never have been guessed
/// locally anyway: they arrive through `RuntimeControlsUpdated` and the agent is
/// free to refuse.
fn preference_change(event: &SettingsEvent) -> Option<Request> {
    match event {
        SettingsEvent::SetLanguage(language) => Some(Request::SetLanguage(*language)),
        SettingsEvent::SetAutoCompaction(enabled) => Some(Request::SetAutoCompaction(*enabled)),
        SettingsEvent::SetBashPolicy(policy) => Some(Request::SetBashPolicy(*policy)),
        SettingsEvent::SetBlockedPatterns(patterns) => {
            Some(Request::SetBlockedPatterns(patterns.clone()))
        }
        SettingsEvent::SetAgentTeamConfig(config) => {
            Some(Request::SetAgentTeamConfig(config.clone()))
        }
        // The controls bar sends the same request from beside the prompt.
        SettingsEvent::SetQueueModes {
            steering,
            follow_up,
        } => Some(Request::SetQueueModes {
            steering: *steering,
            follow_up: *follow_up,
        }),
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

fn is_bare_escape(keystroke: &gpui::Keystroke) -> bool {
    let modifiers = &keystroke.modifiers;
    keystroke.key.as_str() == "escape"
        && !modifiers.shift
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.platform
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
        let language = self.state.language;
        let status = self.state.session_status.clone();
        let banner = match &status {
            SessionStatus::Failed(error)
                if self.dismissed_error.as_deref() == Some(error.as_str()) =>
            {
                None
            }
            SessionStatus::Failed(error) => {
                let copy = error.clone();
                let dismiss = error.clone();
                Some(
                    Banner::error(error.clone(), tokens)
                        .on_copy(
                            translate("copy-error", language),
                            cx.listener(move |workspace, _, _, cx| {
                                workspace.request(Request::CopyToClipboard(copy.clone()), cx);
                            }),
                        )
                        .on_dismiss(
                            translate("dismiss", language),
                            cx.listener(move |workspace, _, _, cx| {
                                workspace.dismissed_error = Some(dismiss.clone());
                                cx.notify();
                            }),
                        ),
                )
            }
            _ => banner_for(&status, language, tokens),
        };
        let state = &self.state;
        let project_name = state.selected_project.and_then(|id| {
            state
                .projects
                .iter()
                .find(|project| project.id == id)
                .map(|project| project.name.as_str())
        });
        let session_title = state.selected_project.and_then(|project_id| {
            state.sessions.get(&project_id).and_then(|sessions| {
                sessions
                    .iter()
                    .find(|session| Some(session.id) == state.selected_session)
                    .map(|session| chat::session_title_or_default(&session.title, language))
            })
        });

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
                    return;
                }
                if is_bare_escape(&event.keystroke)
                    && workspace
                        .conversation
                        .update(cx, |conversation, cx| conversation.reveal_latest(cx))
                {
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
                        TopBar::new(status.clone(), tokens.mode, state.language, tokens)
                            .location(project_name, session_title.as_deref())
                            .metrics(state.session_metrics.as_ref())
                            .on_toggle_theme(cx.listener(|workspace, _, window, cx| {
                                workspace.toggle_theme(window, cx);
                            }))
                            .on_open_settings(cx.listener(|workspace, _, _, cx| {
                                workspace.show_settings(true, cx);
                            })),
                    )
                    // Settings replaces everything below the top bar, including
                    // the controls: they configure the running agent, and there
                    // is nothing to steer while the page is open.
                    .when(self.showing_settings, |this| {
                        this.child(div().flex_1().min_h(px(0.0)).child(self.settings.clone()))
                    })
                    .when(!self.showing_settings, |this| {
                        this.when_some(banner, |this, banner| this.child(banner))
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
                                            .child(self.prompt_area(tokens, cx)),
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
    use std::{cell::Cell, rc::Rc};

    use gpui::{KeyDownEvent, Keystroke};
    use pi_whim_core::{ConversationItem, ConversationRole, stable_session_id};

    use super::*;
    use crate::chrome::Severity;

    #[test]
    fn an_idle_session_shows_no_banner() {
        let tokens = Tokens::light();
        assert!(banner_for(&SessionStatus::Offline, Language::English, tokens).is_none());
        assert!(banner_for(&SessionStatus::Ready, Language::English, tokens).is_none());
        assert!(banner_for(&SessionStatus::Streaming, Language::English, tokens).is_none());
    }

    #[test]
    fn compaction_shows_a_progress_banner() {
        let banner = banner_for(
            &SessionStatus::Compacting,
            Language::English,
            Tokens::light(),
        )
        .expect("a banner while compacting");
        assert_eq!(banner.severity(), Severity::Progress);
    }

    #[test]
    fn failure_shows_an_error_banner() {
        // A broken session matters more than reporting that it is busy, so this
        // is the variant that wins when both could apply.
        let banner = banner_for(
            &SessionStatus::Failed("boom".into()),
            Language::English,
            Tokens::light(),
        )
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
    fn a_stored_preference_is_persisted_rather_than_assumed() {
        // The host writes it and the snapshot comes back, so the control cannot
        // show a policy as set while the write that stores it failed.
        let request =
            preference_change(&SettingsEvent::SetBashPolicy(BashPolicy::Deny)).expect("a change");
        assert_eq!(request, Request::SetBashPolicy(BashPolicy::Deny));
    }

    #[test]
    fn the_agents_own_settings_go_to_the_agent() {
        // Auto-compaction and the queue modes are the agent's to confirm, so these
        // are requests rather than writes to the store.
        assert_eq!(
            preference_change(&SettingsEvent::SetAutoCompaction(true)),
            Some(Request::SetAutoCompaction(true))
        );
        assert_eq!(
            preference_change(&SettingsEvent::SetQueueModes {
                steering: QueueMode::All,
                follow_up: QueueMode::OneAtATime,
            }),
            Some(Request::SetQueueModes {
                steering: QueueMode::All,
                follow_up: QueueMode::OneAtATime,
            })
        );
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

    #[test]
    fn only_a_bare_escape_is_the_stream_reveal_shortcut() {
        assert!(is_bare_escape(&Keystroke::parse("escape").unwrap()));
        assert!(!is_bare_escape(&Keystroke::parse("shift-escape").unwrap()));
        assert!(!is_bare_escape(&Keystroke::parse("enter").unwrap()));
    }

    /// A state with one project selected, so the palette has commands to offer.
    fn state_with_a_project() -> AppState {
        AppState {
            selected_project: Some(uuid::Uuid::new_v4()),
            ..Default::default()
        }
    }

    #[gpui::test]
    async fn the_palette_runs_a_command_from_the_keyboard(cx: &mut gpui::TestAppContext) {
        // The keys are captured on the workspace's own element while the composer
        // keeps focus, so this exercises the same path a keystroke takes: open the
        // list by typing, move, then run. Enter reached the input instead of the
        // palette once, which submitted the raw `/` text as a prompt.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                workspace.set_state(state_with_a_project(), window, cx);
                workspace.handle_composer_event(ComposerEvent::TextChanged("/".to_owned()), cx);

                let open = workspace.palette.read(cx).is_open();
                assert!(open, "typing `/` should offer the commands");

                let down = KeyDownEvent {
                    keystroke: Keystroke::parse("down").expect("a valid keystroke"),
                    is_held: false,
                    prefer_character_input: false,
                };
                let consumed = workspace
                    .palette
                    .update(cx, |palette, cx| palette.handle_key(&down, cx));
                assert!(
                    consumed,
                    "an arrow belongs to the open palette, not the caret"
                );
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn escape_closes_the_palette_without_clearing_the_draft(cx: &mut gpui::TestAppContext) {
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                workspace.set_state(state_with_a_project(), window, cx);
                workspace.handle_composer_event(ComposerEvent::TextChanged("/ex".to_owned()), cx);
                assert!(workspace.palette.read(cx).is_open());

                let escape = KeyDownEvent {
                    keystroke: Keystroke::parse("escape").expect("a valid keystroke"),
                    is_held: false,
                    prefer_character_input: false,
                };
                let consumed = workspace
                    .palette
                    .update(cx, |palette, cx| palette.handle_key(&escape, cx));

                assert!(consumed, "escape is the palette's while it is open");
                assert!(!workspace.palette.read(cx).is_open());
            })
            .expect("the window is open");
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

    struct RequestProbe;

    #[gpui::test]
    async fn testing_web_search_wakes_the_request_host(cx: &mut gpui::TestAppContext) {
        let shell = shell(cx);
        let workspace = shell
            .update(cx, |_, _, cx| cx.entity())
            .expect("the workspace window is open");
        let raised = Rc::new(Cell::new(0));
        let observed = raised.clone();
        let _probe = cx.update(|cx| {
            cx.new(|cx| {
                cx.subscribe(&workspace, move |_, _, _: &RequestsRaised, _| {
                    observed.set(observed.get() + 1);
                })
                .detach();
                RequestProbe
            })
        });

        shell
            .update(cx, |workspace, window, cx| {
                workspace.handle_settings_event(SettingsEvent::TestSearchEngine, window, cx);
            })
            .expect("the workspace window is open");

        assert_eq!(raised.get(), 1);
    }

    #[gpui::test]
    async fn a_submitted_prompt_is_sent_and_not_shown_locally(cx: &mut gpui::TestAppContext) {
        // The application puts the prompt in the conversation as it sends it.
        // Showing it here as well would render it twice: once from the local copy
        // and again from the snapshot that comes back.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, _, cx| {
                workspace.handle_composer_event(
                    ComposerEvent::Submit {
                        content: "what changed?".to_owned(),
                        attachments: Vec::new(),
                        mode: SubmitMode::Prompt,
                    },
                    cx,
                );

                assert!(workspace.state().conversation.is_empty());
                assert!(matches!(
                    workspace.take_requests().as_slice(),
                    [Request::SubmitPrompt { content, .. }] if content == "what changed?"
                ));
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn attaching_from_disk_asks_for_the_picker(cx: &mut gpui::TestAppContext) {
        // Opening the platform picker needs the window, so the shell asks rather
        // than doing it. One ask, not one per kind: the dialog takes files and
        // folders together, and a menu choosing between them only delayed it.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, _, cx| {
                workspace.handle_composer_event(ComposerEvent::PickAttachments, cx);

                assert_eq!(workspace.take_requests(), vec![Request::PickAttachments]);
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
                workspace.set_state(
                    AppState {
                        session_status: SessionStatus::Streaming,
                        ..AppState::default()
                    },
                    window,
                    cx,
                );

                workspace.handle_composer_event(ComposerEvent::Stop, cx);

                assert_eq!(workspace.state().session_status, SessionStatus::Streaming);
                assert_eq!(workspace.take_requests(), vec![Request::Stop]);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn choosing_a_model_asks_rather_than_showing_it_chosen(cx: &mut gpui::TestAppContext) {
        // A switch waits for the next prompt so the prior model compacts the
        // history first. Showing it as pending here would claim the deferral
        // happened even if the request never reached the agent.
        let shell = shell(cx);
        let model = ModelOption {
            provider: "anthropic".into(),
            provider_name: "Anthropic".into(),
            id: "sonnet".into(),
            name: "Sonnet".into(),
        };

        shell
            .update(cx, |workspace, _, cx| {
                workspace.handle_controls_event(ControlsEvent::SetModel(model.clone()), cx);

                assert!(workspace.state().pending_model.is_none());
                assert_eq!(workspace.take_requests(), vec![Request::SetModel(model)]);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn opening_a_session_asks_rather_than_selecting_it(cx: &mut gpui::TestAppContext) {
        // Selecting locally would move the sidebar while the conversation kept
        // showing the session that was open before: the transcript has to be
        // re-read and the pool decides which process is bound, neither of which
        // the shell can do.
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

                assert_eq!(workspace.state().selected_project, None);
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

    #[gpui::test]
    async fn opening_a_project_lists_its_rows_and_asks_for_the_session(
        cx: &mut gpui::TestAppContext,
    ) {
        // Expanding is view-local and happens at once; selecting is not, so it is
        // a request. A header that only expanded would leave the conversation on
        // whichever project was open before.
        let shell = shell(cx);
        let project_id = uuid::Uuid::new_v4();

        shell
            .update(cx, |workspace, window, cx| {
                workspace.handle_sidebar_event(SidebarEvent::OpenProject(project_id), window, cx);

                assert!(workspace.expanded_projects.contains(&project_id));
                assert_eq!(
                    workspace.take_requests(),
                    vec![Request::OpenProject(project_id)]
                );
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn a_snapshot_replaces_what_is_shown(cx: &mut gpui::TestAppContext) {
        // The host owns the reducer, so this is how the shell learns about
        // anything it did not do itself.
        let shell = shell(cx);
        let project_id = uuid::Uuid::new_v4();

        shell
            .update(cx, |workspace, window, cx| {
                let state = AppState {
                    selected_project: Some(project_id),
                    session_status: SessionStatus::Streaming,
                    ..AppState::default()
                };
                workspace.set_state(state, window, cx);

                assert_eq!(workspace.state().selected_project, Some(project_id));
                assert_eq!(workspace.state().session_status, SessionStatus::Streaming);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn switching_sessions_drops_the_conversations_caches(cx: &mut gpui::TestAppContext) {
        // Reveal progress and which tool cards are open are keyed by message id,
        // so carrying them across a switch would show the new session's first
        // messages already revealed — or worse, expanded from the old session's
        // ids colliding.
        let shell = shell(cx);
        let message = |id: &str| ConversationItem {
            id: id.into(),
            role: ConversationRole::Assistant,
            full_text: "hello".into(),
            streaming: false,
            tool_name: None,
            tool_report: None,
            tool_details: None,
            is_error: false,
            model: None,
            attachments: Vec::new(),
        };

        shell
            .update(cx, |workspace, window, cx| {
                workspace.set_state(
                    AppState {
                        selected_session: Some(stable_session_id("/tmp/first.jsonl")),
                        conversation: vec![message("m1")],
                        ..AppState::default()
                    },
                    window,
                    cx,
                );
                workspace.conversation.update(cx, |conversation, cx| {
                    conversation.toggle_tool_report("m1", cx);
                    conversation.toggle_tool_details("m1", cx);
                    conversation.toggle_thinking("m1", 0, cx);
                });

                workspace.set_state(
                    AppState {
                        selected_session: Some(stable_session_id("/tmp/second.jsonl")),
                        conversation: vec![message("m1")],
                        ..AppState::default()
                    },
                    window,
                    cx,
                );

                let conversation = workspace.conversation.read(cx);
                assert!(!conversation.is_tool_report_expanded("m1"));
                assert!(!conversation.is_tool_details_expanded("m1"));
                assert!(!conversation.is_thinking_expanded("m1", 0));
            })
            .expect("the window is open");
    }
}
