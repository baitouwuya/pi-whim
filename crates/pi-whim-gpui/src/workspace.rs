//! Root view.
//!
//! Owns the domain state and the resolved theme, and arranges the chrome around
//! the space the conversation and sidebar will fill. Those two, along with the
//! settings page, land as their own modules.

mod state;

pub use state::{
    ConversationProjection, NavigationProjection, RuntimeProjection, SettingsProjection,
    WorkspaceStateSelections,
};

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::{
    input::{Enter, IndentInline, MoveDown, MoveUp},
    theme::{Theme as ComponentTheme, ThemeMode as ComponentMode},
};
use pi_whim_core::{
    AppState, Attachment, ProjectId, ProviderId, SessionId, SessionStatus, SubmitMode,
    strings::text as translate,
};
use pi_whim_engine::notice::Outbox;
use pi_whim_engine::session::now_ms;
use pi_whim_engine::slash_commands::{self, SlashCommand};
use pi_whim_engine::{
    commands::{AppCommand, ShellCommand, ShellPaste},
    dialogs::Prompt,
};
use pi_whim_signal::{Signal, SignalEmitter};
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, text};

use crate::{
    chat::{
        self, Composer, ComposerEvent, Controls, ControlsEvent, Conversation, ConversationEvent,
        Palette, PaletteEvent, PaletteKey, Paste, QueueStatus, Sidebar, SidebarEvent,
    },
    chrome::{Banner, TopBar},
    dialogs::{PromptEvent, Prompts, Rename, RenameEvent},
    elements::GraphPaper,
    settings::{Settings, SettingsEvent},
    theme::IntoHsla,
};

/// Two Escapes within this window read as one command: stop everything.
///
/// Wide enough to forgive a hesitant second press, narrow enough that a pause
/// between interrupts does not surprise the next one with a queue wipe.
const DOUBLE_ESCAPE: Duration = Duration::from_millis(600);

/// One Escape press, decided.
///
/// The decision is pure so the whole matrix can be tested without a window:
/// `handle_escape` only gathers the four inputs and runs the plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EscapePlan {
    /// Not the workspace's business: fall through to the stream-reveal
    /// shortcut (which may itself decline and let the input have the key).
    RevealLatest,
    /// Stop the running turn. `queue_draft` moves the composer's content
    /// behind the turn it just stopped instead of losing it.
    Interrupt { queue_draft: bool },
    /// Second press inside the window: stop the turn and wipe the queue.
    TerminateAll,
}

/// Decide what a single Escape means.
///
/// Interrupting only makes sense while a turn runs and the composer is where
/// the user's hands are; anything else keeps Escape's idle meaning. Two
/// presses escalate from "stop this turn" to "stop everything".
fn escape_plan(busy: bool, composer_focused: bool, has_draft: bool, double: bool) -> EscapePlan {
    if !(busy && composer_focused) {
        return EscapePlan::RevealLatest;
    }
    if double {
        return EscapePlan::TerminateAll;
    }
    EscapePlan::Interrupt {
        queue_draft: has_draft,
    }
}

/// Space around the prompt box.
///
/// `GAP` is the larger of the two: the distance from the last message to the top
/// of the prompt has to read as a separation between two regions, while the margin
/// to the window's edges only has to stop the border touching them.
const PROMPT_GAP: f32 = 16.0;
const PROMPT_MARGIN: f32 = 10.0;

/// The application shell emits framework-independent typed commands.
///
/// Domain commands and platform/credential commands use separate reliable
/// signals so only [`AppCommand`] can enter application Hook control.
pub struct Workspace {
    preference: ThemePreference,
    tokens: Tokens,
    /// Incremental compatibility cache assembled from feature projections.
    ///
    /// The application remains the reducer owner. No complete state snapshot
    /// crosses the Host boundary; each projection replaces only its own fields.
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
    /// Last session applied to the conversation feature, independent of the
    /// navigation projection's delivery order.
    conversation_session: Option<SessionId>,
    /// Projects whose sessions are listed. View-local: which projects a reader
    /// has open says nothing about the session.
    expanded_projects: BTreeSet<ProjectId>,
    app_commands: Signal<AppCommand>,
    app_command_emitter: SignalEmitter<AppCommand>,
    shell_commands: Signal<ShellCommand>,
    shell_command_emitter: SignalEmitter<ShellCommand>,
    /// When Escape last interrupted a turn, for telling one press from two.
    last_escape: Option<Instant>,
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
                    workspace.emit_app_command(
                        AppCommand::RunSlashCommand(SlashCommand::Fork(id.clone())),
                        cx,
                    );
                }
                ConversationEvent::ClearQueue => {
                    workspace.emit_app_command(AppCommand::ClearQueue, cx);
                }
                ConversationEvent::CopyAssistant(id) => {
                    let text = conversation
                        .read(cx)
                        .messages()
                        .iter()
                        .find(|message| message.id == *id)
                        .map(|message| message.full_text.clone());
                    if let Some(text) = text {
                        workspace.emit_shell_command(ShellCommand::CopyToClipboard(text), cx);
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

        let prompts = cx.new(|cx| Prompts::new(tokens, window, cx));
        cx.subscribe_in(&prompts, window, |workspace, _, event, window, cx| {
            let PromptEvent::Answered(answer) = event;
            // Straight through: the shell has no session pool, and an unanswered
            // question leaves the agent that asked it blocked.
            workspace.emit_app_command(AppCommand::AnswerPrompt(answer.clone()), cx);
            // The composer takes its place back once nothing is left to ask.
            if !workspace.prompts.read(cx).is_asking() {
                workspace.focus_composer(window, cx);
            }
        })
        .detach();

        let rename = cx.new(|cx| Rename::new(tokens, window, cx));
        cx.subscribe(&rename, |workspace, _, event, cx| {
            let RenameEvent::Renamed { path, title } = event;
            workspace.emit_app_command(
                AppCommand::RenameSession {
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

        let (app_commands, app_command_emitter) = Signal::channel();
        let (shell_commands, shell_command_emitter) = Signal::channel();
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
            conversation_session: None,
            expanded_projects: BTreeSet::new(),
            app_commands,
            app_command_emitter,
            shell_commands,
            shell_command_emitter,
            last_escape: None,
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

    /// Subscribe to reliable application-domain commands.
    pub fn app_commands(&self) -> Signal<AppCommand> {
        self.app_commands.clone()
    }

    /// Subscribe to reliable platform and credential-bearing commands.
    pub fn shell_commands(&self) -> Signal<ShellCommand> {
        self.shell_commands.clone()
    }

    fn emit_app_command(&self, command: AppCommand, _cx: &mut Context<Self>) {
        let _ = self.app_command_emitter.emit(command);
    }

    fn emit_shell_command(&self, command: ShellCommand, _cx: &mut Context<Self>) {
        let _ = self.shell_command_emitter.emit(command);
    }

    /// Drive the same typed path as a UI action from a debug probe.
    pub fn debug_app_command(&self, command: AppCommand) {
        let _ = self.app_command_emitter.emit(command);
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
            // typed commands rather than silently doing nothing.
            SidebarEvent::AddProject => self.emit_shell_command(ShellCommand::AddProject, cx),
            SidebarEvent::NewSession(id) => self.emit_app_command(AppCommand::NewSession(id), cx),
            SidebarEvent::ToggleProject(id) | SidebarEvent::OpenProject(id) => {
                // Whether the rows are listed is view-local, so it is decided
                // here; which project is selected is not, so it is asked for.
                // Both from one click, because a header that expanded without
                // selecting would leave the conversation on another project.
                toggle_expanded(&mut self.expanded_projects, id);
                self.emit_app_command(AppCommand::OpenProject(id), cx);
            }
            SidebarEvent::OpenSession {
                project_id,
                pi_path,
            } => {
                // Selection is not applied here either: the pool decides which
                // session is visible and the transcript has to be re-read, so a
                // local select would move the sidebar while the conversation kept
                // showing the previous session.
                self.emit_app_command(
                    AppCommand::ActivateSession {
                        project_id,
                        path: pi_path,
                    },
                    cx,
                );
            }
            // The rest of the row menu is the backend's: Finder, the store, the
            // clipboard, and the trash all sit behind the boundary this crate
            // does not cross.
            SidebarEvent::RevealProject(id) => {
                self.emit_shell_command(ShellCommand::RevealProject(id), cx)
            }
            SidebarEvent::RemoveProject(id) => {
                self.emit_app_command(AppCommand::RemoveProject(id), cx)
            }
            SidebarEvent::CloneSession => self.emit_app_command(AppCommand::CloneSession, cx),
            SidebarEvent::CopySessionId(id) => {
                self.emit_shell_command(ShellCommand::CopyToClipboard(id.to_string()), cx)
            }
            SidebarEvent::DeleteSession(path) => {
                self.emit_app_command(AppCommand::DeleteSession(path), cx)
            }
            SidebarEvent::RenameSession { pi_path, title } => {
                self.rename.update(cx, |rename, cx| {
                    rename.open(pi_path, &title, window, cx);
                });
            }
            SidebarEvent::SmartRenameSession {
                project_id,
                pi_path,
                title,
            } => self.emit_shell_command(
                ShellCommand::SmartRenameSession {
                    project_id,
                    path: pi_path,
                    title,
                },
                cx,
            ),
        }
        self.sync_sidebar(window, cx);
        cx.notify();
    }

    /// Act on what the composer reported.
    ///
    /// A submitted prompt crosses the boundary as one typed app command, retaining
    /// its original content, attachments, and mode for rejection recovery. The
    /// application sends it to Pi only after the active session is revalidated.
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
                self.emit_app_command(
                    AppCommand::SubmitPrompt {
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
                self.emit_app_command(AppCommand::Stop, cx);
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
                    self.emit_app_command(AppCommand::DiscardAttachment(path), cx);
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
                if let Some(paste) = shell_paste(paste) {
                    self.emit_shell_command(ShellCommand::AttachPaste(paste), cx);
                }
            }
            ComposerEvent::PickAttachments => {
                self.emit_shell_command(ShellCommand::PickAttachments, cx);
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
                // A query typed mid-sentence costs only its own token: the
                // sentence around it was the reader's draft, not the command's.
                let text = self.composer.read(cx).text(cx);
                let restored = slash_commands::without_trailing_query(&text).unwrap_or_default();
                self.composer.update(cx, |composer, cx| {
                    composer.set_text(&restored, window, cx);
                });
                self.emit_app_command(AppCommand::RunSlashCommand(command), cx);
            }
        }
        cx.notify();
    }

    /// Offer a captured navigation key to the palette.
    ///
    /// Only the composer's own palette may take it: the action was captured at
    /// the root, so with settings showing or a dialog's field focused the same
    /// key still belongs to that field, palette open or not.
    fn steer_palette(&mut self, key: PaletteKey, window: &mut Window, cx: &mut Context<Self>) {
        if self.showing_settings || !self.composer.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let consumed = self
            .palette
            .update(cx, |palette, cx| palette.handle_palette_key(key, cx));
        if consumed {
            cx.stop_propagation();
        }
    }

    /// What one Escape means depends on the turn.
    ///
    /// A waiting question comes first: the agent is blocked on its answer, so
    /// Escape picks the cautious answer the prompt names rather than
    /// interrupting past it. While the agent works it interrupts; a typed draft
    /// is not lost to that but queued behind the turn it just stopped. Two
    /// presses in a row stop everything, queue included. Idle, Escape stays the
    /// stream-reveal shortcut it always was.
    fn handle_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.prompts.read(cx).is_asking() {
            self.prompts.update(cx, |prompts, cx| prompts.dismiss(cx));
            cx.stop_propagation();
            return;
        }
        let busy = matches!(
            self.state.session_status,
            SessionStatus::Streaming | SessionStatus::Compacting
        );
        let composer_focused =
            !self.showing_settings && self.composer.read(cx).focus_handle(cx).is_focused(window);
        let has_draft = !self.composer.read(cx).text(cx).trim().is_empty()
            || !self.composer.read(cx).attachments().is_empty();
        self.run_escape_plan(busy, composer_focused, has_draft, window, cx);
    }

    /// Carry out one Escape, its context already read.
    ///
    /// Split from [`Workspace::handle_escape`] so the behavior is testable
    /// without focusing the composer: a focused input paints through the
    /// component root, which a headless test window does not have.
    fn run_escape_plan(
        &mut self,
        busy: bool,
        composer_focused: bool,
        has_draft: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let now = Instant::now();
        let double = self
            .last_escape
            .is_some_and(|previous| now.duration_since(previous) < DOUBLE_ESCAPE);
        let plan = escape_plan(busy, composer_focused, has_draft, double);
        if plan == EscapePlan::RevealLatest {
            self.last_escape = None;
            if self
                .conversation
                .update(cx, |conversation, cx| conversation.reveal_latest(cx))
            {
                cx.stop_propagation();
            }
            return;
        }
        self.last_escape = if double { None } else { Some(now) };

        // Interrupt either way; what else happens is what sets one press apart
        // from two.
        self.emit_app_command(AppCommand::Stop, cx);
        if plan == EscapePlan::TerminateAll {
            self.emit_app_command(AppCommand::ClearQueue, cx);
        } else {
            let draft = self.composer.read(cx).text(cx);
            let attachments = self.composer.read(cx).attachments().to_vec();
            if !draft.trim().is_empty() || !attachments.is_empty() {
                let paths: Vec<String> = attachments
                    .iter()
                    .map(|attachment| attachment.path.clone())
                    .collect();
                self.composer.update(cx, |composer, cx| {
                    composer.set_text("", window, cx);
                    for path in paths {
                        composer.remove_attachment(&path, cx);
                    }
                });
                self.emit_app_command(
                    AppCommand::SubmitPrompt {
                        content: draft,
                        attachments,
                        mode: SubmitMode::FollowUp,
                    },
                    cx,
                );
            }
        }
        cx.stop_propagation();
    }

    /// Act on what the runtime controls reported.
    ///
    /// All three cross the runtime boundary: model and thinking use Pi RPC while
    /// permission updates the live agent supervisor. They queue as requests the
    /// same way the sidebar's do.
    fn handle_controls_event(&mut self, event: ControlsEvent, cx: &mut Context<Self>) {
        // No `cx.notify()` at the end: every arm here is a request, and `request`
        // notifies. The picker keeps showing the old value until the projection
        // arrives, which is the point.
        match event {
            ControlsEvent::SetModel(model) => {
                // The host records the choice as pending — a switch waits for the
                // next prompt so the prior model compacts the history first — and
                // the picker shows it when that projection comes back.
                self.emit_app_command(AppCommand::SetModel(model), cx);
            }
            ControlsEvent::SetPermissionLevel(level) => {
                self.emit_app_command(AppCommand::SetPermissionLevel(level), cx);
            }
            ControlsEvent::SetThinkingLevel(level) => {
                self.emit_app_command(AppCommand::SetThinkingLevel(level), cx);
            }
        }
    }

    /// Act on what the settings page reported.
    ///
    /// Everything that changes domain state leaves as a request: the host applies
    /// it and the projection comes back, so a preference cannot end up showing as
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
            self.emit_app_command(request, cx);
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

            SettingsEvent::EditBackgroundAiTask(kind) => {
                self.settings.update(cx, |settings, cx| {
                    settings.edit_background_ai_task(kind, window, cx);
                });
            }
            SettingsEvent::CloseBackgroundAiTaskEditor => {
                self.settings.update(cx, |settings, cx| {
                    settings.close_background_ai_task_editor(window, cx);
                });
            }
            SettingsEvent::SetBackgroundAiTaskEnabled(enabled) => {
                self.settings.update(cx, |settings, cx| {
                    settings.set_background_ai_task_enabled(enabled, cx);
                });
            }
            SettingsEvent::SaveBackgroundAiTask => {
                let config = self.settings.read(cx).background_ai_config_with_draft();
                self.emit_app_command(AppCommand::SetOneShotAiConfig(config), cx);
                self.settings.update(cx, |settings, cx| {
                    settings.close_background_ai_task_editor(window, cx);
                });
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
                self.emit_shell_command(
                    ShellCommand::DiscoverProviderModels {
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
                self.emit_shell_command(
                    ShellCommand::SaveProvider {
                        profile: draft.to_profile(now_ms()),
                        api_key: draft.typed_api_key(),
                    },
                    cx,
                );
            }
            SettingsEvent::DeleteProvider(id) => {
                self.emit_app_command(AppCommand::DeleteProvider(id), cx);
                self.settings
                    .update(cx, |settings, cx| settings.new_provider(window, cx));
            }
            SettingsEvent::ConfigureModel(model_id) => {
                self.settings.update(cx, |settings, cx| {
                    settings.configure_model(&model_id, window, cx);
                });
            }
            SettingsEvent::CloseModelConfig => {
                self.settings.update(cx, |settings, cx| {
                    settings.close_model_config(cx);
                });
            }
            SettingsEvent::SaveModelConfig => {
                self.settings.update(cx, |settings, cx| {
                    settings.save_model_config(window, cx);
                });
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
                self.emit_app_command(AppCommand::SaveSearchEngines(profiles), cx);
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
                self.emit_shell_command(
                    ShellCommand::SaveSearchEngine {
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
                let profile = draft.to_profile(position);
                self.settings.update(cx, |settings, cx| {
                    settings.start_search_engine_test(profile.id, true, cx);
                });
                self.emit_shell_command(
                    ShellCommand::TestSearchEngine {
                        profile,
                        api_key: draft.typed_api_key(),
                        editor: true,
                    },
                    cx,
                );
            }
            SettingsEvent::QuickTestSearchEngine(profile) => {
                self.settings.update(cx, |settings, cx| {
                    settings.start_search_engine_test(profile.id, false, cx);
                });
                self.emit_shell_command(
                    ShellCommand::TestSearchEngine {
                        profile: profile.clone(),
                        api_key: None,
                        editor: false,
                    },
                    cx,
                );
            }
            SettingsEvent::RemoveSearchEngine(index) => {
                let profiles = self.settings.read(cx).search_engines_without(index);
                self.emit_app_command(AppCommand::SaveSearchEngines(profiles), cx);
                self.settings.update(cx, |settings, cx| {
                    settings.clear_search_engine_draft(window, cx);
                });
            }
            SettingsEvent::MoveSearchEngine { index, delta } => {
                let profiles = self.settings.read(cx).search_engines_moved(index, delta);
                self.emit_app_command(AppCommand::SaveSearchEngines(profiles), cx);
            }

            // Handled above by `preference_change`, which returned `Some` for
            // exactly these and returned early.
            SettingsEvent::SetLanguage(_)
            | SettingsEvent::SetAutoCompaction(_)
            | SettingsEvent::SetBashPolicy(_)
            | SettingsEvent::SetBlockedPatterns(_)
            | SettingsEvent::SetAgentTeamConfig(_)
            | SettingsEvent::ApproveProjectHooks { .. }
            | SettingsEvent::RevokeProjectHooks
            | SettingsEvent::SetOneShotAiConfig(_)
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
        let queued_steering = self.state.pending_steering.clone();
        let queued_follow_up = self.state.pending_follow_up.clone();
        self.conversation.update(cx, |conversation, cx| {
            conversation.set_tokens(tokens, cx);
            conversation.set_language(language, cx);
            conversation.set_has_project(has_project, cx);
            conversation.set_generating(generating, cx);
            conversation.set_queue(queued_steering, queued_follow_up, cx);
            conversation.set_messages(messages, cx);
        });
    }

    fn refresh_runtime_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

        // Controls compare the projected compatibility cache with their applied
        // values so unrelated commits do not reset an open picker.
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
    }

    fn refresh_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.state.clone();
        let tokens = self.tokens;
        self.settings.update(cx, |settings, cx| {
            settings.set_tokens(tokens, cx);
            settings.sync(&state, window, cx);
        });
    }

    fn refresh_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync_sidebar(window, cx);
        self.sync_conversation(cx);
        self.refresh_runtime_controls(window, cx);
        self.refresh_settings(window, cx);
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
    /// How the host answers a pasted attachment and the attach command: both
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

    /// Hand an asynchronous connection-test result back to the control that
    /// launched it. Stored rows and the editor intentionally keep separate state.
    pub fn search_engine_test_finished(
        &mut self,
        id: pi_whim_core::SearchEngineId,
        editor: bool,
        result: Result<(), String>,
        cx: &mut Context<Self>,
    ) {
        self.settings.update(cx, |settings, cx| {
            settings.finish_search_engine_test(id, editor, result, cx);
        });
    }

    /// Read-only domain state, for rendering.
    pub fn state(&self) -> &AppState {
        &self.state
    }

    /// Apply the navigation/sidebar slice replayed from committed state.
    pub fn apply_navigation_projection(
        &mut self,
        projection: NavigationProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        projection.apply_to(&mut self.state);
        self.sync_sidebar(window, cx);
        cx.notify();
    }

    /// Apply the visible conversation, queue, and session-runtime slice.
    pub fn apply_conversation_projection(
        &mut self,
        projection: ConversationProjection,
        cx: &mut Context<Self>,
    ) {
        let next_session = projection.selected_session();
        let switched = self.conversation_session != next_session;
        let cleared = projection.conversation_is_empty() && !self.state.conversation.is_empty();
        let status_changed = &self.state.session_status != projection.session_status();
        if switched || cleared {
            self.conversation
                .update(cx, |conversation, cx| conversation.clear(cx));
        }
        if status_changed {
            self.dismissed_error = None;
        }
        projection.apply_to(&mut self.state);
        self.conversation_session = next_session;
        self.sync_conversation(cx);
        cx.notify();
    }

    /// Apply runtime controls and composer/chrome status.
    pub fn apply_runtime_projection(
        &mut self,
        projection: RuntimeProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        projection.apply_to(&mut self.state);
        self.refresh_runtime_controls(window, cx);
        cx.notify();
    }

    /// Apply preferences, providers, search, Hooks, and AGENTS.md-related state.
    pub fn apply_settings_projection(
        &mut self,
        projection: SettingsProjection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        projection.apply_to(&mut self.state);
        self.refresh_settings(window, cx);
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
        self.refresh_theme(window, cx);
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
        self.refresh_theme(window, cx);
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
        // A waiting question takes the composer's place: the asking agent is
        // blocked on its answer, so that answer is the one thing typing can
        // mean right now.
        let asking = self.prompts.read(cx).is_asking();
        let footer = (!asking).then(|| {
            let (attach, send) = self.composer.update(cx, |composer, cx| {
                (composer.attach_button(cx), composer.send_button(cx))
            });

            // One line, never wrapped. Attach and the permission grant sit on the
            // left — both about what goes into the turn, not how it runs; model and
            // thinking gather at the right beside send, so the turn's own settings
            // read as one group next to the action they apply to.
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(attach)
                .when_some(
                    self.controls.read(cx).permission_indicator(),
                    |this, grant| this.child(grant),
                )
                // Takes the slack, so the group stays at the right edge however wide
                // the window gets.
                .child(div().flex_1().min_w(px(0.0)))
                .child(div().flex_none().child(self.controls.clone()))
                .child(send)
        });

        let queue_status = QueueStatus::from_state(&self.state, tokens).map(|status| {
            status.on_clear(cx.listener(|workspace, _, _, cx| {
                workspace.emit_app_command(AppCommand::ClearQueue, cx);
            }))
        });

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
            // The queue floats over the prompt's top-right corner, anchored
            // like the palette: inside the box it stole a row from the field,
            // and the box grew and shrank as messages queued and drained.
            .when_some(queue_status, |this, status| {
                this.child(
                    div()
                        .absolute()
                        .bottom_full()
                        .right_0()
                        .mb(px(6.0))
                        .child(status),
                )
            })
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
                    .when(asking, |this| this.child(self.prompts.clone()))
                    .when_some(footer, |this, footer| {
                        this.child(self.composer.clone()).child(footer)
                    }),
            )
    }
}

fn shell_paste(paste: Paste) -> Option<ShellPaste> {
    match paste {
        Paste::Insert => None,
        Paste::Files(paths) => Some(ShellPaste::Files(paths)),
        Paste::Image { extension, bytes } => Some(ShellPaste::Image { extension, bytes }),
        Paste::LongText(text) => Some(ShellPaste::LongText(text)),
    }
}

/// The application command a preference change needs, if `event` is one.
///
/// Returns `None` for the provider and search-engine events, which are drafts
/// rather than preferences and need the settings view rather than a table.
///
/// The host applies each of these and the projection comes back, so the control
/// shows what was actually stored rather than what was clicked. The two the agent
/// owns — auto-compaction and the queue modes — could never have been guessed
/// locally anyway: they arrive through `RuntimeControlsUpdated` and the agent is
/// free to refuse.
fn preference_change(event: &SettingsEvent) -> Option<AppCommand> {
    match event {
        SettingsEvent::SetLanguage(language) => Some(AppCommand::SetLanguage(*language)),
        SettingsEvent::SetAutoCompaction(enabled) => Some(AppCommand::SetAutoCompaction(*enabled)),
        SettingsEvent::SetBashPolicy(policy) => Some(AppCommand::SetBashPolicy(*policy)),
        SettingsEvent::SetBlockedPatterns(patterns) => {
            Some(AppCommand::SetBlockedPatterns(patterns.clone()))
        }
        SettingsEvent::SetAgentTeamConfig(config) => {
            Some(AppCommand::SetAgentTeamConfig(config.clone()))
        }
        SettingsEvent::ApproveProjectHooks {
            fingerprint,
            grants_hash,
        } => Some(AppCommand::ApproveProjectHooks {
            fingerprint: fingerprint.clone(),
            grants_hash: grants_hash.clone(),
        }),
        SettingsEvent::RevokeProjectHooks => Some(AppCommand::RevokeProjectHooks),
        SettingsEvent::SetOneShotAiConfig(config) => {
            Some(AppCommand::SetOneShotAiConfig(config.clone()))
        }
        // The controls bar sends the same request from beside the prompt.
        SettingsEvent::SetQueueModes {
            steering,
            follow_up,
        } => Some(AppCommand::SetQueueModes {
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
        // Only a failure earns the banner: compaction and streaming are ordinary
        // work the status pill already reports, and a banner for them pushed the
        // whole conversation down on every automatic compaction pass.
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
                                workspace.emit_shell_command(
                                    ShellCommand::CopyToClipboard(copy.clone()),
                                    cx,
                                );
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
            _ => None,
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
            // The composer keeps focus while the palette is open, so typing
            // still filters — but the input binds arrows, Enter, and Tab to its
            // own actions, and a bound action never reaches a key listener.
            // Capturing the actions instead is what lets an arrow move the
            // palette selection instead of the caret, and Enter run the
            // highlighted command instead of submitting the prompt.
            .capture_action(cx.listener(|workspace, _: &MoveUp, window, cx| {
                workspace.steer_palette(PaletteKey::Up, window, cx);
            }))
            .capture_action(cx.listener(|workspace, _: &MoveDown, window, cx| {
                workspace.steer_palette(PaletteKey::Down, window, cx);
            }))
            .capture_action(cx.listener(|workspace, action: &Enter, window, cx| {
                // Shift+Enter is the input's own newline, palette open or not.
                if action.secondary || action.shift {
                    return;
                }
                workspace.steer_palette(PaletteKey::Run, window, cx);
            }))
            .capture_action(cx.listener(|workspace, _: &IndentInline, window, cx| {
                workspace.steer_palette(PaletteKey::Run, window, cx);
            }))
            // Escape the input lets propagate, so it still arrives as a key:
            // first the palette's dismissal, then the turn's interrupt.
            .capture_key_down(cx.listener(|workspace, event, window, cx| {
                let consumed = workspace
                    .palette
                    .update(cx, |palette, cx| palette.handle_key(event, cx));
                if consumed {
                    cx.stop_propagation();
                    return;
                }
                if is_bare_escape(&event.keystroke) {
                    workspace.handle_escape(window, cx);
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
            // when it has nothing to ask. The question prompt is not among
            // them: it takes the composer's place inside `prompt_area` instead.
            .child(self.rename.clone())
    }
}

#[cfg(test)]
mod tests {
    use gpui::{KeyDownEvent, Keystroke};
    use pi_whim_core::{
        BashPolicy, ConversationItem, ConversationRole, ModelOption, OneShotAiConfig, QueueMode,
        SearchEngineProfile, stable_session_id,
    };
    use pi_whim_signal::Subscription;

    use super::*;

    struct SignalProbe<T> {
        receiver: crossbeam_channel::Receiver<T>,
        _subscription: Subscription,
    }

    impl<T> SignalProbe<T> {
        fn take(&self) -> Vec<T> {
            self.receiver.try_iter().collect()
        }
    }

    fn signal_probe<T>(signal: Signal<T>) -> SignalProbe<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let subscription = signal.subscribe_fn(move |command| {
            let _ = sender.send(command);
        });
        SignalProbe {
            receiver,
            _subscription: subscription,
        }
    }

    #[test]
    fn paste_conversion_is_lossless_and_insert_stays_composer_local() {
        let bytes = vec![0, 1, 2, 254, 255];
        assert_eq!(
            shell_paste(Paste::Image {
                extension: "png".into(),
                bytes: bytes.clone(),
            }),
            Some(ShellPaste::Image {
                extension: "png".into(),
                bytes,
            })
        );
        assert_eq!(
            shell_paste(Paste::Files(vec!["/tmp/a".into(), "/tmp/b".into()])),
            Some(ShellPaste::Files(vec!["/tmp/a".into(), "/tmp/b".into()]))
        );
        assert_eq!(
            shell_paste(Paste::LongText("private paste".into())),
            Some(ShellPaste::LongText("private paste".into()))
        );
        assert_eq!(shell_paste(Paste::Insert), None);
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
        // The host writes it and the projection comes back, so the control cannot
        // show a policy as set while the write that stores it failed.
        let command =
            preference_change(&SettingsEvent::SetBashPolicy(BashPolicy::Deny)).expect("a change");
        assert_eq!(command, AppCommand::SetBashPolicy(BashPolicy::Deny));
    }

    #[test]
    fn hook_approval_forwards_manifest_and_exact_grants_hashes() {
        assert_eq!(
            preference_change(&SettingsEvent::ApproveProjectHooks {
                fingerprint: "manifest-hash".into(),
                grants_hash: "grants-hash".into(),
            }),
            Some(AppCommand::ApproveProjectHooks {
                fingerprint: "manifest-hash".into(),
                grants_hash: "grants-hash".into(),
            })
        );
    }

    #[test]
    fn background_ai_config_is_forwarded_as_one_atomic_preference() {
        let mut config = OneShotAiConfig {
            max_concurrency: 8,
            queue_capacity: 128,
            timeout_secs: 20,
            ..OneShotAiConfig::default()
        };
        config.set_task(
            pi_whim_core::SESSION_TITLE_TASK_KIND,
            pi_whim_core::OneShotAiTaskConfig {
                enabled: true,
                ..Default::default()
            },
        );

        assert_eq!(
            preference_change(&SettingsEvent::SetOneShotAiConfig(config.clone())),
            Some(AppCommand::SetOneShotAiConfig(config))
        );
    }

    #[test]
    fn the_agents_own_settings_go_to_the_agent() {
        // Auto-compaction and the queue modes are the agent's to confirm, so these
        // are commands rather than writes to the store.
        assert_eq!(
            preference_change(&SettingsEvent::SetAutoCompaction(true)),
            Some(AppCommand::SetAutoCompaction(true))
        );
        assert_eq!(
            preference_change(&SettingsEvent::SetQueueModes {
                steering: QueueMode::All,
                follow_up: QueueMode::OneAtATime,
            }),
            Some(AppCommand::SetQueueModes {
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

    fn apply_state(
        workspace: &mut Workspace,
        state: AppState,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        workspace.apply_navigation_projection(NavigationProjection::from_state(&state), window, cx);
        workspace.apply_conversation_projection(ConversationProjection::from_state(&state), cx);
        workspace.apply_runtime_projection(RuntimeProjection::from_state(&state), window, cx);
        workspace.apply_settings_projection(SettingsProjection::from_state(&state), window, cx);
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
                apply_state(workspace, state_with_a_project(), window, cx);
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
                apply_state(workspace, state_with_a_project(), window, cx);
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
    ///
    /// Not wrapped in `gpui_component::Root` the way the app wraps it: the root
    /// installs a macOS hit-test forwarder a headless window does not have.
    /// Tests therefore never focus the composer — a focused input paints
    /// through the root — and drive focus-gated methods with the gate answered,
    /// the way [`Workspace::run_escape_plan`] takes its inputs.
    fn shell(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Workspace> {
        let preference = ThemePreference::default();
        cx.update(|cx| {
            crate::init(preference, cx).expect("the bundled fonts load");
        });
        cx.add_window(|window, cx| Workspace::new(preference, window, cx))
    }

    #[gpui::test]
    async fn testing_web_search_emits_one_typed_shell_command(cx: &mut gpui::TestAppContext) {
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                let commands = signal_probe(workspace.shell_commands());
                workspace.handle_settings_event(SettingsEvent::TestSearchEngine, window, cx);
                assert!(matches!(
                    commands.take().as_slice(),
                    [ShellCommand::TestSearchEngine { editor: true, .. }]
                ));
            })
            .expect("the workspace window is open");
    }

    #[gpui::test]
    async fn a_quick_search_test_targets_the_saved_row(cx: &mut gpui::TestAppContext) {
        let shell = shell(cx);
        let profile = SearchEngineProfile::new_doubao_global();
        let expected = profile.clone();

        shell
            .update(cx, |workspace, window, cx| {
                let commands = signal_probe(workspace.shell_commands());
                workspace.handle_settings_event(
                    SettingsEvent::QuickTestSearchEngine(profile),
                    window,
                    cx,
                );

                assert!(matches!(
                    commands.take().as_slice(),
                    [ShellCommand::TestSearchEngine {
                        profile,
                        api_key: None,
                        editor: false,
                    }] if profile == &expected
                ));
            })
            .expect("the workspace window is open");
    }

    #[gpui::test]
    async fn a_submitted_prompt_is_sent_and_not_shown_locally(cx: &mut gpui::TestAppContext) {
        // The application puts the prompt in the conversation as it sends it.
        // Showing it here as well would render it twice: once from the local copy
        // and again from the projection that comes back.
        let shell = shell(cx);

        shell
            .update(cx, |workspace, _, cx| {
                let commands = signal_probe(workspace.app_commands());
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
                    commands.take().as_slice(),
                    [AppCommand::SubmitPrompt { content, .. }] if content == "what changed?"
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
                let commands = signal_probe(workspace.shell_commands());
                workspace.handle_composer_event(ComposerEvent::PickAttachments, cx);

                assert_eq!(commands.take(), vec![ShellCommand::PickAttachments]);
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
                let commands = signal_probe(workspace.app_commands());
                apply_state(
                    workspace,
                    AppState {
                        session_status: SessionStatus::Streaming,
                        ..AppState::default()
                    },
                    window,
                    cx,
                );

                workspace.handle_composer_event(ComposerEvent::Stop, cx);

                assert_eq!(workspace.state().session_status, SessionStatus::Streaming);
                assert_eq!(commands.take(), vec![AppCommand::Stop]);
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
                let commands = signal_probe(workspace.app_commands());
                workspace.handle_controls_event(ControlsEvent::SetModel(model.clone()), cx);

                assert!(workspace.state().pending_model.is_none());
                assert_eq!(commands.take(), vec![AppCommand::SetModel(model)]);
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
                let commands = signal_probe(workspace.app_commands());
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
                    commands.take(),
                    vec![AppCommand::ActivateSession {
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
                let commands = signal_probe(workspace.app_commands());
                workspace.handle_sidebar_event(SidebarEvent::OpenProject(project_id), window, cx);

                assert!(workspace.expanded_projects.contains(&project_id));
                assert_eq!(commands.take(), vec![AppCommand::OpenProject(project_id)]);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn feature_projections_replace_what_is_shown(cx: &mut gpui::TestAppContext) {
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
                apply_state(workspace, state, window, cx);

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
                apply_state(
                    workspace,
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

                apply_state(
                    workspace,
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

    #[test]
    fn escape_only_interrupts_while_busy_with_hands_on_the_composer() {
        // The whole matrix, without a window: anything else keeps Escape's
        // idle meaning.
        assert_eq!(
            escape_plan(false, true, true, false),
            EscapePlan::RevealLatest
        );
        assert_eq!(
            escape_plan(true, false, true, false),
            EscapePlan::RevealLatest
        );
        assert_eq!(
            escape_plan(true, true, true, false),
            EscapePlan::Interrupt { queue_draft: true }
        );
        assert_eq!(
            escape_plan(true, true, false, false),
            EscapePlan::Interrupt { queue_draft: false }
        );
        // A second press ignores the draft: stop everything, queue included.
        assert_eq!(
            escape_plan(true, true, true, true),
            EscapePlan::TerminateAll
        );
        assert_eq!(
            escape_plan(false, true, true, true),
            EscapePlan::RevealLatest
        );
    }

    /// The shell with a streaming turn: the interrupt shortcuts live there.
    ///
    /// The composer is never focused — a focused input paints through the
    /// component root a headless window does not have — so the Escape tests
    /// answer the focus gate themselves via `run_escape_plan`.
    fn busy_shell(cx: &mut gpui::TestAppContext) -> gpui::WindowHandle<Workspace> {
        let shell = shell(cx);
        shell
            .update(cx, |workspace, window, cx| {
                apply_state(
                    workspace,
                    AppState {
                        session_status: SessionStatus::Streaming,
                        ..AppState::default()
                    },
                    window,
                    cx,
                );
            })
            .expect("the window is open");
        shell
    }

    #[gpui::test]
    async fn one_escape_interrupts_and_queues_the_draft(cx: &mut gpui::TestAppContext) {
        // What was typed is not lost to the interrupt: it waits behind the turn
        // it just stopped.
        let shell = busy_shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                let commands = signal_probe(workspace.app_commands());
                workspace.composer.update(cx, |composer, cx| {
                    composer.set_text("hold that thought", window, cx);
                });
                workspace.run_escape_plan(true, true, true, window, cx);

                let requests = commands.take();
                assert_eq!(requests.len(), 2);
                assert_eq!(requests[0], AppCommand::Stop);
                assert!(matches!(
                    &requests[1],
                    AppCommand::SubmitPrompt { content, mode: SubmitMode::FollowUp, .. }
                        if content == "hold that thought"
                ));
                assert_eq!(workspace.composer.read(cx).text(cx), "");
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn one_escape_without_a_draft_only_interrupts(cx: &mut gpui::TestAppContext) {
        let shell = busy_shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                let commands = signal_probe(workspace.app_commands());
                workspace.run_escape_plan(true, true, false, window, cx);
                assert_eq!(commands.take(), vec![AppCommand::Stop]);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn two_escapes_stop_everything_queue_included(cx: &mut gpui::TestAppContext) {
        let shell = busy_shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                let commands = signal_probe(workspace.app_commands());
                workspace.run_escape_plan(true, true, false, window, cx);
                workspace.run_escape_plan(true, true, false, window, cx);

                let requests = commands.take();
                assert_eq!(requests.len(), 3);
                assert_eq!(requests[0], AppCommand::Stop);
                assert_eq!(requests[1], AppCommand::Stop);
                assert_eq!(requests[2], AppCommand::ClearQueue);
            })
            .expect("the window is open");
    }

    #[gpui::test]
    async fn an_idle_escape_interrupts_nothing(cx: &mut gpui::TestAppContext) {
        let shell = shell(cx);

        shell
            .update(cx, |workspace, window, cx| {
                let commands = signal_probe(workspace.app_commands());
                apply_state(
                    workspace,
                    AppState {
                        session_status: SessionStatus::Ready,
                        ..AppState::default()
                    },
                    window,
                    cx,
                );
                workspace.run_escape_plan(false, true, true, window, cx);

                assert!(commands.take().is_empty());
            })
            .expect("the window is open");
    }
}
