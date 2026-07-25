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
use gpui_component::Icon;
use pi_whim_core::ProjectId;
use pi_whim_theme::{Tokens, layout, radius, text};

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
    /// Collapse or expand a project's sessions.
    ToggleProject(ProjectId),
    /// Show a project, starting or resuming a session for it.
    OpenProject(ProjectId),
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

        div()
            .id(("sidebar-row", index))
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
