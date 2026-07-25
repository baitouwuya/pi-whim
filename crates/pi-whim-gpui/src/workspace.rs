//! Root view.
//!
//! Owns the domain state and the resolved theme, and arranges the chrome around
//! the space the conversation and sidebar will fill. Those two, along with the
//! settings page, land as their own modules.

use std::collections::BTreeSet;

use gpui::{
    AppContext, Context, Entity, InteractiveElement, IntoElement, ParentElement, Render, Styled,
    Window, div, prelude::FluentBuilder, px,
};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_core::{
    Action, AppState, ConversationItem, ConversationRole, ModelOption, ProjectId, QueueMode,
    SessionStatus, ThinkingLevel, stable_session_id,
};
use pi_whim_engine::dialogs::{Answer, Prompt};
use pi_whim_engine::notice::Outbox;
use pi_whim_engine::slash_commands::SlashCommand;
use pi_whim_engine::state::{EngineState, ViewEffect};
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, text};

use crate::{
    chat::{
        self, Composer, ComposerEvent, Controls, ControlsEvent, Conversation, ConversationEvent,
        Palette, PaletteEvent, Sidebar, SidebarEvent,
    },
    chrome::{Banner, TopBar},
    dialogs::{PromptEvent, Prompts, Rename, RenameEvent},
    elements::GraphPaper,
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

        Self {
            preference,
            tokens,
            engine: EngineState::new(),
            sidebar,
            conversation,
            composer,
            controls,
            palette,
            prompts,
            rename,
            notices: Outbox::new(),
            expanded_projects: BTreeSet::new(),
            requests: Vec::new(),
        }
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
    /// Submitting is the shell's to forward to the backend, which the egui app
    /// still owns; for now the prompt lands in the conversation so the round trip
    /// is visible.
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
                ..
            } => {
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
                self.apply(Action::SetSessionStatus(SessionStatus::Ready), window, cx);
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
                            .on_open_settings(cx.listener(|_, _, _, _| {
                                // The settings page lands in a later change.
                            })),
                    )
                    // Below the banner and above the panes: these configure the
                    // agent, so they belong with the window chrome rather than
                    // beside the prompt.
                    .child(self.controls.clone())
                    .when_some(banner_for(&status, tokens), |this, banner| {
                        this.child(banner)
                    })
                    // The conversation and sidebar fill whatever the chrome
                    // leaves.
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
                                        // The palette is absolutely positioned
                                        // against this box so it floats over the
                                        // conversation rather than pushing the
                                        // input down as it grows.
                                        div()
                                            .relative()
                                            .flex()
                                            .flex_col()
                                            .items_center()
                                            .child(self.palette.clone())
                                            .child(self.composer.clone()),
                                    ),
                            ),
                    ),
            )
            // Modals last, so they paint over the panes. Each renders nothing
            // when it has nothing to ask.
            .child(self.prompts.clone())
            .child(self.rename.clone())
    }
}

#[cfg(test)]
mod tests {
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
}
