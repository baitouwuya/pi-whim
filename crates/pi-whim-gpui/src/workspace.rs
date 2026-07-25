//! Root view.
//!
//! Owns the domain state and the resolved theme, and arranges the chrome around
//! the space the conversation and sidebar will fill. Those two, along with the
//! settings page, land as their own modules.

use std::collections::BTreeSet;

use gpui::{
    AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window, div,
    prelude::FluentBuilder, px,
};
use gpui_component::theme::{Theme as ComponentTheme, ThemeMode as ComponentMode};
use pi_whim_core::{Action, AppState, ProjectId, SessionStatus, stable_session_id};
use pi_whim_engine::state::{EngineState, ViewEffect};
use pi_whim_theme::{ThemeMode, ThemePreference, Tokens, text};

use crate::{
    chat::{self, Conversation, ConversationEvent, Sidebar, SidebarEvent},
    chrome::{Banner, StatusStrip, TopBar},
    theme::IntoHsla,
};

/// The application shell.
pub struct Workspace {
    preference: ThemePreference,
    tokens: Tokens,
    engine: EngineState,
    sidebar: Entity<Sidebar>,
    conversation: Entity<Conversation>,
    /// Projects whose sessions are listed. View-local: which projects a reader
    /// has open says nothing about the session.
    expanded_projects: BTreeSet<ProjectId>,
}

impl Workspace {
    pub fn new(preference: ThemePreference, cx: &mut Context<Self>) -> Self {
        let mode = if ComponentTheme::global(cx).is_dark() {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        };
        let tokens = Tokens::new(mode);
        let sidebar = cx.new(|_| Sidebar::new(tokens));
        cx.subscribe(&sidebar, |workspace, _, event, cx| {
            workspace.handle_sidebar_event(event.clone(), cx);
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

        Self {
            preference,
            tokens,
            engine: EngineState::new(),
            sidebar,
            conversation,
            expanded_projects: BTreeSet::new(),
        }
    }

    /// Act on what the sidebar reported, and refresh its rows.
    fn handle_sidebar_event(&mut self, event: SidebarEvent, cx: &mut Context<Self>) {
        match event {
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
        }
        self.sync_views(cx);
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

    /// Refresh both panes after state changed.
    fn sync_views(&mut self, cx: &mut Context<Self>) {
        self.sync_sidebar(cx);
        self.sync_conversation(cx);
    }

    /// Read-only domain state, for rendering.
    pub fn state(&self) -> &AppState {
        self.engine.get()
    }

    /// Apply `action` through the reducer.
    ///
    /// View-local follow-ups arrive as a [`ViewEffect`]; the shell currently
    /// caches nothing per message, so there is nothing to invalidate yet.
    pub fn apply(&mut self, action: Action, cx: &mut Context<Self>) {
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
        self.sync_views(cx);
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
        self.sync_views(cx);
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
        self.sync_views(cx);
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        let state = self.engine.get();
        let status = state.session_status.clone();

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(tokens.bg_canvas.hsla())
            .text_color(tokens.text.hsla())
            .text_size(px(text::BODY_SIZE))
            .child(
                TopBar::new(status.clone(), tokens.mode, tokens)
                    .on_toggle_theme(cx.listener(|workspace, _, window, cx| {
                        workspace.toggle_theme(window, cx);
                    }))
                    .on_open_settings(cx.listener(|_, _, _, _| {
                        // The settings page lands in a later change.
                    })),
            )
            .when_some(banner_for(&status, tokens), |this, banner| {
                this.child(banner)
            })
            // The conversation and sidebar fill whatever the chrome leaves.
            .child(
                div()
                    .flex_1()
                    .flex()
                    .min_h(px(0.0))
                    .child(self.sidebar.clone())
                    .child(self.conversation.clone()),
            )
            .child(StatusStrip::from_state(self.engine.get(), tokens))
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
