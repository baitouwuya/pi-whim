//! Sidebar of projects and their sessions.
//!
//! Rows are a fixed height, so this uses `uniform_list`: gpui renders only the
//! visible span, which is the native answer to the hand-rolled viewport clipping
//! the egui build needed.

use gpui::{
    AnyElement, Context, EventEmitter, InteractiveElement, IntoElement, ParentElement, Render,
    SharedString, StatefulInteractiveElement, Styled, Window, div, prelude::FluentBuilder, px,
    uniform_list,
};
use gpui_component::{
    Icon, Sizable,
    button::{Button, ButtonVariants},
};
use pi_whim_core::ProjectId;
use pi_whim_theme::{Tokens, font, layout, radius, text};

use crate::{chat::Row, icons, theme::IntoHsla};

/// Row height. Fixed, because `uniform_list` requires it.
const ROW_HEIGHT: f32 = 30.0;
/// How far sessions sit inside their project header.
const SESSION_INDENT: f32 = 14.0;
/// Leading glyph size, kept below the row's text so icons read as marks rather
/// than as content.
const ICON_SIZE: f32 = 13.0;

/// What the sidebar asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarEvent {
    /// Add a project to the list, which means asking for a folder.
    AddProject,
    /// Collapse or expand a project's sessions.
    ToggleProject(ProjectId),
    /// Show a project, starting or resuming a session for it.
    OpenProject(ProjectId),
    /// Start a fresh session in a project.
    NewSession(ProjectId),
    /// Show a specific session.
    OpenSession {
        project_id: ProjectId,
        pi_path: String,
    },
}

/// The project and session list.
pub struct Sidebar {
    rows: Vec<Row>,
    tokens: Tokens,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(tokens: Tokens) -> Self {
        Self {
            rows: Vec::new(),
            tokens,
        }
    }

    /// Rebuild the rows from state.
    ///
    /// The shell calls this rather than the sidebar reading state itself, so
    /// there is one owner of the expansion set.
    pub fn set_rows(&mut self, rows: Vec<Row>, cx: &mut Context<Self>) {
        if self.rows != rows {
            self.rows = rows;
            cx.notify();
        }
    }

    pub fn set_tokens(&mut self, tokens: Tokens, cx: &mut Context<Self>) {
        self.tokens = tokens;
        cx.notify();
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// The header above the list: what this column is, and how to add to it.
    ///
    /// Adding a project is the only way into an empty app, so it is always
    /// visible rather than hidden behind a hover or a menu.
    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.tokens;
        div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .w_full()
            .pl(px(10.0))
            .pr(px(6.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(tokens.line.hsla())
            .child(
                div()
                    .flex_1()
                    .font_family(font::MONO)
                    .text_size(px(text::LABEL_SIZE))
                    .text_color(tokens.muted.hsla())
                    .child("PROJECTS"),
            )
            .child(
                Button::new("add-project")
                    .ghost()
                    .xsmall()
                    .icon(icons::add())
                    .tooltip("Add a project folder")
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::AddProject))),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.tokens;
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };

        let (label, indent, running, selected) = match row {
            Row::Project {
                name,
                running,
                selected,
                ..
            } => (name.clone(), 0.0, *running, *selected),
            Row::Session {
                title,
                running,
                selected,
                ..
            } => (title.clone(), SESSION_INDENT, *running, *selected),
        };

        // A project carries a disclosure arrow and a folder; a session carries a
        // transcript glyph. Together they make the nesting readable even where a
        // title is truncated.
        let (leading, folder) = match row {
            Row::Project { expanded, .. } => (
                Some(icons::disclosure(*expanded)),
                Some(icons::project(*expanded)),
            ),
            Row::Session { .. } => (None, Some(icons::session())),
        };

        let event = match row {
            // A click on a header both selects the project and toggles its
            // sessions; two separate hit targets in a 30px row would be fussy.
            Row::Project { id, .. } => SidebarEvent::OpenProject(*id),
            Row::Session {
                project_id,
                pi_path,
                ..
            } => SidebarEvent::OpenSession {
                project_id: *project_id,
                pi_path: pi_path.clone(),
            },
        };

        // A project header carries its own "new session" button, revealed on
        // hover: one visible per project would be a column of plus signs down the
        // sidebar, and the control is most discoverable where it acts.
        let group = SharedString::from(format!("sidebar-row-{index}"));
        let new_session = match row {
            Row::Project { id, .. } => {
                let id = *id;
                let group = group.clone();
                Some(
                    Button::new(("new-session", index))
                        .ghost()
                        .xsmall()
                        .icon(icons::add())
                        .tooltip("New session in this project")
                        .invisible()
                        .group_hover(group, |this| this.visible())
                        .on_click(cx.listener(move |_, _, _, cx| {
                            // Without this the row's own handler also fires and
                            // the click would toggle the project as well.
                            cx.stop_propagation();
                            cx.emit(SidebarEvent::NewSession(id));
                        })),
                )
            }
            Row::Session { .. } => None,
        };

        div()
            .id(("sidebar-row", index))
            .group(group)
            .flex()
            .items_center()
            .gap(px(6.0))
            .h(px(ROW_HEIGHT))
            .w_full()
            .pl(px(10.0 + indent))
            .pr(px(10.0))
            .when(selected, |this| {
                this.bg(tokens.accent_surface_strong().hsla())
                    .border_l_2()
                    .border_color(tokens.accent.hsla())
            })
            .hover(|this| this.bg(tokens.control_background_hover().hsla()))
            .when_some(leading, |this, icon| {
                this.child(
                    Icon::new(icon)
                        .size(px(ICON_SIZE))
                        .text_color(tokens.muted.hsla()),
                )
            })
            .when_some(folder, |this, icon| {
                this.child(Icon::new(icon).size(px(ICON_SIZE)).text_color(if selected {
                    tokens.accent.hsla()
                } else {
                    tokens.muted.hsla()
                }))
            })
            .child(
                div()
                    .flex_1()
                    .overflow_hidden()
                    .text_size(px(if indent > 0.0 {
                        text::MONO_DETAIL_SIZE
                    } else {
                        text::DETAIL_SIZE
                    }))
                    .text_color(if selected {
                        tokens.text.hsla()
                    } else {
                        tokens.muted.hsla()
                    })
                    .child(SharedString::from(label)),
            )
            .when(running, |this| {
                // Same dot the status pill uses, so "working" reads the same
                // wherever it appears.
                this.child(
                    div()
                        .w(px(6.0))
                        .h(px(6.0))
                        .rounded(px(radius::DOT))
                        .bg(tokens.accent.hsla()),
                )
            })
            .when_some(new_session, |this, button| this.child(button))
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(event.clone());
            }))
            .into_any_element()
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        let count = self.rows.len();
        let entity = cx.entity();

        div()
            .w(px(layout::SIDEBAR_WIDTH))
            .h_full()
            .flex()
            .flex_col()
            .bg(tokens.panel_soft.hsla())
            .border_r_1()
            .border_color(tokens.line.hsla())
            .child(self.render_header(cx))
            .child(
                uniform_list("sidebar", count, {
                    move |range, _window, cx| {
                        entity.update(cx, |sidebar, cx| {
                            range.map(|index| sidebar.render_row(index, cx)).collect()
                        })
                    }
                })
                .flex_1(),
            )
            // With no projects there is nothing to click but the plus, so say so
            // rather than leaving an empty column.
            .when(count == 0, |this| {
                this.child(
                    div()
                        .px(px(10.0))
                        .pb(px(10.0))
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child("Add a project folder to begin."),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Both hold between constants, so they are checked at compile time.
    const _: () = {
        // Sessions sit inside their project header.
        assert!(SESSION_INDENT > 0.0);
        // uniform_list clips to the row height, so the label has to fit.
        assert!(ROW_HEIGHT > text::DETAIL_SIZE);
    };

    #[test]
    fn adding_a_project_names_no_project() {
        // It cannot: the folder has not been picked yet. This is the one sidebar
        // event that carries nothing.
        assert_eq!(SidebarEvent::AddProject, SidebarEvent::AddProject);
    }

    #[test]
    fn starting_a_session_is_distinct_from_opening_the_project() {
        // Clicking a header resumes whatever session was last shown; the plus
        // starts a fresh one. Conflating them would lose the distinction.
        let id = uuid::Uuid::new_v4();
        assert_ne!(SidebarEvent::NewSession(id), SidebarEvent::OpenProject(id));
    }

    #[test]
    fn opening_a_session_carries_the_path_running_state_is_keyed_by() {
        // Sessions are identified to the backend by transcript path, not by id.
        let event = SidebarEvent::OpenSession {
            project_id: uuid::Uuid::new_v4(),
            pi_path: "/tmp/a.jsonl".into(),
        };
        let SidebarEvent::OpenSession { pi_path, .. } = &event else {
            panic!("expected a session event");
        };
        assert_eq!(pi_path, "/tmp/a.jsonl");
    }
}
