//! Sidebar of projects and their sessions.
//!
//! Rows are a fixed height, so this uses `uniform_list`: gpui renders only the
//! visible span, which is the native answer to the hand-rolled viewport clipping
//! the egui build needed.

use std::{
    collections::{BTreeSet, HashMap},
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, AppContext, Context, Entity, EventEmitter, InteractiveElement, IntoElement,
    ParentElement, Render, ScrollHandle, SharedString, StatefulInteractiveElement, Styled, Window,
    div, point, prelude::FluentBuilder, px, uniform_list,
};
use gpui_component::{
    Icon, Sizable,
    button::{Button, ButtonVariants},
    input::{Input, InputEvent, InputState},
    menu::{ContextMenuExt, PopupMenuItem},
    tooltip::Tooltip,
};
use pi_whim_core::{Language, ProjectId, SessionId, strings::text as translate};
use pi_whim_theme::{Tokens, font, layout, radius, text};

use crate::{chat::Row, icons, theme::IntoHsla};

/// Row height. Fixed, because `uniform_list` requires it.
const ROW_HEIGHT: f32 = 30.0;
/// How far sessions sit inside their project header.
const SESSION_INDENT: f32 = 14.0;
/// Leading glyph size, kept below the row's text so icons read as marks rather
/// than as content.
const ICON_SIZE: f32 = 13.0;
const MARQUEE_DELAY: Duration = Duration::from_millis(350);
const MARQUEE_SPEED: f32 = 48.0;

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
    /// Show the project's folder in Finder.
    RevealProject(ProjectId),
    /// Forget a project, which stops its sessions but leaves the folder alone.
    RemoveProject(ProjectId),
    /// Give the session at `pi_path` a new title, starting from `title`.
    RenameSession { pi_path: String, title: String },
    /// Copy the visible session's transcript into a new one.
    CloneSession,
    /// Put the session id on the clipboard.
    CopySessionId(SessionId),
    /// Move the session's transcript to the trash.
    DeleteSession(String),
}

/// What a row's context menu offers, in order.
///
/// Split out as a pure function so the menu can be asserted without a window:
/// what a right-click can reach is the part worth pinning, not how it is drawn.
///
/// Labels come back translated. `&'static str` still holds because the string
/// table is static — nothing here allocates a label per call.
fn row_actions(row: &Row, language: Language) -> Vec<(&'static str, SidebarEvent)> {
    let label = |key| translate(key, language);
    match row {
        Row::Project { id, .. } => vec![
            (label("show-finder"), SidebarEvent::RevealProject(*id)),
            (label("remove"), SidebarEvent::RemoveProject(*id)),
        ],
        Row::Session {
            id, pi_path, title, ..
        } => vec![
            (
                label("rename"),
                SidebarEvent::RenameSession {
                    pi_path: pi_path.clone(),
                    title: title.clone(),
                },
            ),
            (label("clone"), SidebarEvent::CloneSession),
            (label("copy-session-id"), SidebarEvent::CopySessionId(*id)),
            // Last, and separated from the rest by being last: it moves the
            // transcript to the trash.
            (
                label("delete"),
                SidebarEvent::DeleteSession(pi_path.clone()),
            ),
        ],
    }
}

/// The project and session list.
pub struct Sidebar {
    rows: Vec<Row>,
    /// Rows shown when no query is active, with collapsed sessions omitted.
    default_rows: Vec<Row>,
    /// The complete tree searched, including sessions under collapsed projects.
    search_rows: Vec<Row>,
    search: Entity<InputState>,
    /// The language the column's headings, menus, and empty state are read in.
    language: Language,
    tokens: Tokens,
    title_scrolls: HashMap<String, ScrollHandle>,
    hovered_title: Option<String>,
    marquee_epoch: u64,
}

impl EventEmitter<SidebarEvent> for Sidebar {}

impl Sidebar {
    pub fn new(tokens: Tokens, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(translate("search-projects", Language::default()))
        });
        cx.subscribe_in(&search, window, |sidebar, input, event, _, cx| {
            if matches!(event, InputEvent::Change) {
                sidebar.filter_rows(&input.read(cx).value(), cx);
            }
        })
        .detach();
        Self {
            rows: Vec::new(),
            default_rows: Vec::new(),
            search_rows: Vec::new(),
            search,
            language: Language::default(),
            tokens,
            title_scrolls: HashMap::new(),
            hovered_title: None,
            marquee_epoch: 0,
        }
    }

    pub fn set_language(
        &mut self,
        language: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.language != language {
            self.language = language;
            self.search.update(cx, |search, cx| {
                search.set_placeholder(translate("search-projects", language), window, cx);
            });
            cx.notify();
        }
    }

    /// Rebuild the rows from state.
    ///
    /// The shell calls this rather than the sidebar reading state itself, so
    /// there is one owner of the expansion set.
    pub fn set_rows(
        &mut self,
        default_rows: Vec<Row>,
        search_rows: Vec<Row>,
        cx: &mut Context<Self>,
    ) {
        let keys: BTreeSet<_> = default_rows
            .iter()
            .chain(&search_rows)
            .map(row_key)
            .collect();
        self.title_scrolls.retain(|key, _| keys.contains(key));
        for key in keys {
            self.title_scrolls.entry(key).or_default();
        }
        self.default_rows = default_rows;
        self.search_rows = search_rows;
        let query = self.search.read(cx).value().to_string();
        self.filter_rows(&query, cx);
    }

    fn filter_rows(&mut self, query: &str, cx: &mut Context<Self>) {
        let rows = if query.trim().is_empty() {
            self.default_rows.clone()
        } else {
            filtered_rows(&self.search_rows, query)
        };
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

    fn set_title_hovered(
        &mut self,
        key: String,
        hovered: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marquee_epoch = self.marquee_epoch.wrapping_add(1);
        let epoch = self.marquee_epoch;
        let Some(scroll) = self.title_scrolls.get(&key).cloned() else {
            return;
        };
        if !hovered {
            scroll.set_offset(point(px(0.0), px(0.0)));
            if self.hovered_title.as_deref() == Some(&key) {
                self.hovered_title = None;
                cx.notify();
            }
            return;
        }
        for (other_key, other_scroll) in &self.title_scrolls {
            if other_key != &key {
                other_scroll.set_offset(point(px(0.0), px(0.0)));
            }
        }
        self.hovered_title = Some(key);
        cx.notify();
        if cx.reduce_motion() {
            return;
        }
        cx.spawn_in(window, async move |sidebar, cx| {
            cx.background_executor().timer(MARQUEE_DELAY).await;
            let started = Instant::now();
            let _ = cx.update(|window, app| {
                let _ = sidebar.update(app, |sidebar, cx| {
                    sidebar.advance_marquee(epoch, scroll, started, window, cx);
                });
            });
        })
        .detach();
    }

    fn advance_marquee(
        &mut self,
        epoch: u64,
        scroll: ScrollHandle,
        started: Instant,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marquee_epoch != epoch {
            return;
        }
        let maximum = scroll.max_offset().x;
        if maximum <= px(0.0) {
            return;
        }

        // Use wall-clock progress on GPUI animation frames. A fixed timer step
        // accumulates scheduling jitter and makes the text visibly stutter.
        let duration = f32::from(maximum) / MARQUEE_SPEED;
        let progress = (started.elapsed().as_secs_f32() / duration).clamp(0.0, 1.0);
        let eased = progress * progress * (3.0 - 2.0 * progress);
        let offset = maximum * eased;
        scroll.set_offset(point(-offset, px(0.0)));
        cx.notify();

        if progress < 1.0 {
            cx.on_next_frame(window, move |sidebar, window, cx| {
                sidebar.advance_marquee(epoch, scroll, started, window, cx);
            });
            window.request_animation_frame();
        }
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
                    .child(translate("projects", self.language)),
            )
            .child(
                Button::new("add-project")
                    .ghost()
                    .xsmall()
                    .icon(icons::add())
                    .tooltip(translate("add-project", self.language))
                    .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarEvent::AddProject))),
            )
            .into_any_element()
    }

    fn render_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let tokens = self.tokens;
        let Some(row) = self.rows.get(index) else {
            return div().into_any_element();
        };
        let key = row_key(row);
        let title_id = key.clone();
        let title_hovered = self.hovered_title.as_deref() == Some(&key);
        let title_scroll = self.title_scrolls.get(&key).cloned().unwrap_or_default();

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

        let title_tooltip = label.clone();
        let hover_key = key.clone();
        let title = div()
            .id(SharedString::from(format!("sidebar-title:{title_id}")))
            .flex_1()
            .min_w(px(0.0))
            .h_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .whitespace_nowrap()
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
            .tooltip(move |window, cx| Tooltip::new(title_tooltip.clone()).build(window, cx))
            .on_hover(cx.listener(move |sidebar, hovered, window, cx| {
                sidebar.set_title_hovered(hover_key.clone(), *hovered, window, cx);
            }))
            .when(title_hovered, |title| {
                title
                    .overflow_x_scroll()
                    .track_scroll(&title_scroll)
                    .child(div().flex_none().child(SharedString::from(label.clone())))
            })
            .when(!title_hovered, |title| {
                title
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(SharedString::from(label))
            });

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
                        .tooltip(translate("new-session", self.language))
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
            .child(title)
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
            // Right-click reaches everything a row can do beyond opening it.
            // These were the only way to rename, clone, or delete a session in
            // the egui build too; keeping them here means no second surface for
            // per-row actions.
            .context_menu({
                let entity = cx.entity();
                let actions = row_actions(row, self.language);
                move |menu, _, _| {
                    actions.iter().fold(menu, |menu, (label, event)| {
                        let entity = entity.clone();
                        let event = event.clone();
                        menu.item(PopupMenuItem::new(*label).on_click(move |_, _, cx| {
                            entity.update(cx, |_, cx| cx.emit(event.clone()));
                        }))
                    })
                }
            })
            .into_any_element()
    }
}

fn row_key(row: &Row) -> String {
    match row {
        Row::Project { id, .. } => format!("project:{id}"),
        Row::Session { pi_path, .. } => format!("session:{pi_path}"),
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let tokens = self.tokens;
        let count = self.rows.len();
        let has_projects = self
            .search_rows
            .iter()
            .any(|row| matches!(row, Row::Project { .. }));
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
                div().px(px(8.0)).py(px(6.0)).child(
                    Input::new(&self.search)
                        .prefix(Icon::new(icons::search()).size(px(12.0)))
                        .bordered(false),
                ),
            )
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
            .when(count == 0 && !has_projects, |this| {
                this.child(
                    div()
                        .px(px(10.0))
                        .pb(px(10.0))
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(translate("empty-projects", self.language)),
                )
            })
            .when(count == 0 && has_projects, |this| {
                this.child(
                    div()
                        .px(px(10.0))
                        .pb(px(10.0))
                        .text_size(px(text::LABEL_SIZE))
                        .text_color(tokens.muted.hsla())
                        .child(translate("no-search-results", self.language)),
                )
            })
    }
}

/// Search a flattened tree without losing the project parent of a matching
/// session. Matching a project keeps all of its currently expanded sessions.
fn filtered_rows(rows: &[Row], query: &str) -> Vec<Row> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return rows.to_vec();
    }

    let project_matches: BTreeSet<_> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Project { id, name, .. } if name.to_lowercase().contains(&query) => Some(*id),
            _ => None,
        })
        .collect();
    let session_matches: BTreeSet<_> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Session {
                project_id, title, ..
            } if title.to_lowercase().contains(&query) => Some(*project_id),
            _ => None,
        })
        .collect();

    rows.iter()
        .filter(|row| match row {
            Row::Project { id, .. } => project_matches.contains(id) || session_matches.contains(id),
            Row::Session {
                project_id, title, ..
            } => project_matches.contains(project_id) || title.to_lowercase().contains(&query),
        })
        .cloned()
        .collect()
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

    fn project_row() -> Row {
        Row::Project {
            id: uuid::Uuid::new_v4(),
            name: "pi-whim".into(),
            expanded: true,
            running: false,
            selected: false,
        }
    }

    fn session_row() -> Row {
        Row::Session {
            id: uuid::Uuid::new_v4(),
            project_id: uuid::Uuid::new_v4(),
            pi_path: "/tmp/a.jsonl".into(),
            title: "Migrate the UI".into(),
            running: false,
            selected: false,
        }
    }

    fn session_row_for(project_id: ProjectId, title: &str) -> Row {
        Row::Session {
            id: uuid::Uuid::new_v4(),
            project_id,
            pi_path: format!("/tmp/{title}.jsonl"),
            title: title.into(),
            running: false,
            selected: false,
        }
    }

    #[test]
    fn search_keeps_the_parent_of_a_matching_session() {
        let project_id = uuid::Uuid::new_v4();
        let rows = vec![
            Row::Project {
                id: project_id,
                name: "alpha".into(),
                expanded: false,
                running: false,
                selected: false,
            },
            session_row_for(project_id, "Migrate GPUI"),
        ];

        let filtered = filtered_rows(&rows, "gpui");
        assert_eq!(filtered.len(), 2);
        assert!(matches!(filtered[0], Row::Project { .. }));
        assert!(matches!(filtered[1], Row::Session { .. }));
    }

    #[test]
    fn matching_a_project_keeps_its_expanded_sessions() {
        let project_id = uuid::Uuid::new_v4();
        let rows = vec![
            Row::Project {
                id: project_id,
                name: "alpha".into(),
                expanded: true,
                running: false,
                selected: false,
            },
            session_row_for(project_id, "Unrelated title"),
        ];

        assert_eq!(filtered_rows(&rows, "alpha"), rows);
    }

    #[test]
    fn a_project_and_a_session_offer_different_actions() {
        // Renaming a project or cloning it would mean nothing; the row menu is
        // the only place either kind's actions are reachable.
        let project: Vec<_> = row_actions(&project_row(), Language::English)
            .into_iter()
            .map(|(label, _)| label)
            .collect();
        let session: Vec<_> = row_actions(&session_row(), Language::English)
            .into_iter()
            .map(|(label, _)| label)
            .collect();

        assert_eq!(project, vec!["Show in Finder", "Remove"]);
        assert_eq!(
            session,
            vec![
                "Rename",
                "Clone session",
                "Copy session ID",
                "Move to trash"
            ]
        );
    }

    #[test]
    fn the_row_menu_is_translated() {
        // Every entry, not just the ones with an obvious translation: a menu that
        // switches language except for one item reads as a bug in that item.
        let chinese = row_actions(&session_row(), Language::SimplifiedChinese);
        let english = row_actions(&session_row(), Language::English);

        for ((chinese_label, _), (english_label, _)) in chinese.iter().zip(&english) {
            assert_ne!(
                chinese_label, english_label,
                "{english_label} is the same in both languages"
            );
            // A missing key renders as "?", which would leave a blank-looking row.
            assert_ne!(*chinese_label, "?");
        }
    }

    #[test]
    fn renaming_starts_from_the_title_the_session_has() {
        // Most renames edit an auto-generated title, so the dialog opens seeded
        // rather than blank.
        let actions = row_actions(&session_row(), Language::English);
        let (_, event) = &actions[0];
        let SidebarEvent::RenameSession { pi_path, title } = event else {
            panic!("expected a rename event");
        };
        assert_eq!(pi_path, "/tmp/a.jsonl");
        assert_eq!(title, "Migrate the UI");
    }

    #[test]
    fn moving_a_session_to_the_trash_is_last() {
        // It is the one entry that destroys something, so it does not sit next to
        // the ones that do not.
        let actions = row_actions(&session_row(), Language::English);
        let (label, _) = actions.last().expect("a last action");
        assert_eq!(*label, "Move to trash");
    }

    #[test]
    fn every_row_offers_something() {
        // A right-click that opens an empty menu reads as a broken control.
        assert!(!row_actions(&project_row(), Language::English).is_empty());
        assert!(!row_actions(&session_row(), Language::English).is_empty());
    }
}
