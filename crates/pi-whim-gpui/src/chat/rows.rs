//! Flattening projects and their sessions into sidebar rows.
//!
//! The sidebar is a tree, but `uniform_list` virtualizes a flat list of
//! equal-height rows. This turns one into the other, and it is plain data —
//! testable without a window, which is where the collapsing and selection rules
//! are worth pinning down.

use pi_whim_core::{AppState, ProjectId, SessionId};
use std::collections::BTreeSet;

/// One line in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// A project header. Clicking it collapses or expands its sessions.
    Project {
        id: ProjectId,
        name: String,
        /// Whether its sessions are listed below.
        expanded: bool,
        /// True when any of its sessions is mid-turn, including ones not shown.
        running: bool,
        selected: bool,
    },
    /// A session under its project.
    Session {
        id: SessionId,
        project_id: ProjectId,
        /// Pi transcript path, which is how running state is keyed.
        pi_path: String,
        title: String,
        running: bool,
        selected: bool,
    },
}

impl Row {
    /// Which project this row belongs to, header or session alike.
    pub fn project_id(&self) -> ProjectId {
        match self {
            Row::Project { id, .. } => *id,
            Row::Session { project_id, .. } => *project_id,
        }
    }

    pub fn is_session(&self) -> bool {
        matches!(self, Row::Session { .. })
    }
}

/// Build the sidebar's rows from state and the set of expanded projects.
///
/// Projects keep the order state holds them in; a collapsed project contributes
/// its header only. A project's header shows a running dot when any of its
/// sessions is working, so collapsing a project does not hide that something is
/// still going on inside it.
pub fn rows(state: &AppState, expanded: &BTreeSet<ProjectId>) -> Vec<Row> {
    let mut rows = Vec::new();
    for project in &state.projects {
        let sessions = state.sessions.get(&project.id);
        let running = sessions.is_some_and(|sessions| {
            sessions
                .iter()
                .any(|session| state.running_sessions.contains(&session.pi_path))
        });
        let is_expanded = expanded.contains(&project.id);

        rows.push(Row::Project {
            id: project.id,
            name: project.name.clone(),
            expanded: is_expanded,
            running,
            selected: state.selected_project == Some(project.id),
        });

        if !is_expanded {
            continue;
        }
        for session in sessions.into_iter().flatten() {
            rows.push(Row::Session {
                id: session.id,
                project_id: project.id,
                pi_path: session.pi_path.clone(),
                title: session.title.clone(),
                running: state.running_sessions.contains(&session.pi_path),
                selected: state.selected_session == Some(session.id),
            });
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{Action, Project, SessionSummary, stable_session_id};
    use uuid::Uuid;

    fn project(name: &str) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.into(),
            path: format!("/tmp/{name}"),
            pinned: false,
            last_opened_ms: 1,
        }
    }

    fn session(project_id: ProjectId, path: &str, title: &str) -> SessionSummary {
        SessionSummary {
            id: stable_session_id(path),
            project_id,
            pi_path: path.into(),
            title: title.into(),
            preview: String::new(),
            updated_at_ms: 1,
        }
    }

    /// State with one project holding two sessions.
    fn populated() -> (AppState, ProjectId) {
        let mut state = AppState::default();
        let project = project("alpha");
        let id = project.id;
        state.dispatch(Action::ProjectsLoaded(vec![project]));
        state.dispatch(Action::SessionsLoaded {
            project_id: id,
            sessions: vec![
                session(id, "/tmp/alpha/a.jsonl", "First"),
                session(id, "/tmp/alpha/b.jsonl", "Second"),
            ],
        });
        (state, id)
    }

    #[test]
    fn a_collapsed_project_contributes_only_its_header() {
        let (state, _) = populated();
        let rows = rows(&state, &BTreeSet::new());

        assert_eq!(rows.len(), 1);
        assert!(!rows[0].is_session());
    }

    #[test]
    fn expanding_lists_the_sessions_under_their_project() {
        let (state, id) = populated();
        let rows = rows(&state, &BTreeSet::from([id]));

        assert_eq!(rows.len(), 3);
        assert!(!rows[0].is_session());
        assert!(rows[1].is_session());
        assert!(rows[2].is_session());
        // Sessions belong to the header above them.
        assert!(rows.iter().all(|row| row.project_id() == id));
    }

    #[test]
    fn projects_keep_the_order_state_holds_them_in() {
        let mut state = AppState::default();
        let first = project("alpha");
        let second = project("beta");
        let (first_id, second_id) = (first.id, second.id);
        state.dispatch(Action::ProjectsLoaded(vec![first, second]));

        let rows = rows(&state, &BTreeSet::new());
        assert_eq!(rows[0].project_id(), first_id);
        assert_eq!(rows[1].project_id(), second_id);
    }

    #[test]
    fn a_collapsed_project_still_shows_that_work_is_happening_inside_it() {
        // Otherwise collapsing a project would hide a running agent.
        let (mut state, _) = populated();
        state.dispatch(Action::SessionRunning {
            path: "/tmp/alpha/a.jsonl".into(),
            running: true,
        });

        let rows = rows(&state, &BTreeSet::new());
        let Row::Project { running, .. } = &rows[0] else {
            panic!("expected a project header");
        };
        assert!(*running);
    }

    #[test]
    fn running_state_is_per_session() {
        let (mut state, id) = populated();
        state.dispatch(Action::SessionRunning {
            path: "/tmp/alpha/a.jsonl".into(),
            running: true,
        });

        let rows = rows(&state, &BTreeSet::from([id]));
        let running: Vec<bool> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Session { running, .. } => Some(*running),
                _ => None,
            })
            .collect();
        assert_eq!(running, vec![true, false]);
    }

    #[test]
    fn selection_is_marked_on_both_kinds_of_row() {
        let (mut state, id) = populated();
        let session_id = stable_session_id("/tmp/alpha/b.jsonl");
        state.dispatch(Action::SelectProject(id));
        state.dispatch(Action::SelectSession(session_id));

        let rows = rows(&state, &BTreeSet::from([id]));
        let Row::Project { selected, .. } = &rows[0] else {
            panic!("expected a project header");
        };
        assert!(*selected);

        let selected_sessions: Vec<bool> = rows
            .iter()
            .filter_map(|row| match row {
                Row::Session { selected, .. } => Some(*selected),
                _ => None,
            })
            .collect();
        assert_eq!(selected_sessions, vec![false, true]);
    }

    #[test]
    fn a_project_with_no_sessions_expands_to_nothing() {
        let mut state = AppState::default();
        let project = project("empty");
        let id = project.id;
        state.dispatch(Action::ProjectsLoaded(vec![project]));

        assert_eq!(rows(&state, &BTreeSet::from([id])).len(), 1);
    }

    #[test]
    fn no_projects_means_no_rows() {
        assert!(rows(&AppState::default(), &BTreeSet::new()).is_empty());
    }
}
