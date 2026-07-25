//! The icons this app uses, named by purpose.
//!
//! gpui-component ships 107 SVGs and generates an `IconName` variant per file.
//! Views name a purpose here rather than a file, so a glyph can be swapped
//! without hunting through call sites — and so the set stays small and
//! deliberate instead of growing one `IconName::Whatever` at a time.
//!
//! The egui build drew fifteen icons by hand as vector paths. Twelve have a
//! direct equivalent in the bundled set; the three that do not are noted below.

use gpui_component::IconName;
use pi_whim_core::{ConversationRole, SessionStatus};

/// Toggle to the other theme. Shows where the toggle goes, not where it is.
pub fn theme_toggle(is_dark: bool) -> IconName {
    // Currently dark, so the control offers daylight.
    if is_dark {
        IconName::Sun
    } else {
        IconName::Moon
    }
}

pub fn settings() -> IconName {
    IconName::Settings
}

pub fn add() -> IconName {
    IconName::Plus
}

pub fn close() -> IconName {
    IconName::Close
}

pub fn copy() -> IconName {
    IconName::Copy
}

pub fn search() -> IconName {
    IconName::Search
}

/// Send the drafted prompt.
pub fn send() -> IconName {
    IconName::ArrowUp
}

/// Interrupt the turn in flight.
pub fn stop() -> IconName {
    IconName::Pause
}

/// Compact the conversation. The egui build drew a bespoke "compress" glyph;
/// minimize is the closest bundled equivalent.
pub fn compact() -> IconName {
    IconName::Minimize
}

/// A project, open or collapsed.
pub fn project(expanded: bool) -> IconName {
    if expanded {
        IconName::FolderOpen
    } else {
        IconName::Folder
    }
}

/// A disclosure arrow, pointing down when the section is open.
pub fn disclosure(expanded: bool) -> IconName {
    if expanded {
        IconName::ChevronDown
    } else {
        IconName::ChevronRight
    }
}

/// A file staged onto a prompt.
pub fn attachment() -> IconName {
    IconName::File
}

/// A session transcript.
pub fn session() -> IconName {
    IconName::File
}

/// The icon for a conversation entry's role.
pub fn role(role: &ConversationRole) -> Option<IconName> {
    match role {
        // A prompt needs no marker: it is already set apart by alignment and
        // width, and a glyph on every one would be noise.
        ConversationRole::User => None,
        // The egui build drew a bespoke "brain"; bot is the bundled stand-in.
        ConversationRole::Assistant => Some(IconName::Bot),
        // The egui build drew a cube; a terminal reads more plainly as "a tool
        // ran".
        ConversationRole::Tool => Some(IconName::SquareTerminal),
        ConversationRole::System => Some(IconName::Info),
    }
}

/// The icon for a session's status, where one adds anything.
///
/// Idle states get none: the coloured dot already says as much, and a second
/// mark beside it would be redundant.
pub fn status(status: &SessionStatus) -> Option<IconName> {
    match status {
        SessionStatus::Failed(_) => Some(IconName::TriangleAlert),
        SessionStatus::Starting | SessionStatus::Streaming => Some(IconName::LoaderCircle),
        SessionStatus::Compacting => Some(IconName::Minimize),
        SessionStatus::Ready | SessionStatus::Offline => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_component::IconNamed;

    /// The embedded SVG path, which is what actually gets drawn.
    ///
    /// `IconName` derives neither `PartialEq` nor `Debug`, so comparing paths is
    /// how these assertions identify a glyph.
    fn path(icon: IconName) -> String {
        icon.path().to_string()
    }

    #[test]
    fn the_theme_toggle_offers_the_other_mode() {
        // Dark now, so the control shows a sun.
        assert_eq!(path(theme_toggle(true)), path(IconName::Sun));
        assert_eq!(path(theme_toggle(false)), path(IconName::Moon));
    }

    #[test]
    fn a_project_shows_whether_it_is_open() {
        assert_ne!(path(project(true)), path(project(false)));
        assert_eq!(path(project(true)), path(IconName::FolderOpen));
    }

    #[test]
    fn disclosure_points_down_when_open() {
        assert_eq!(path(disclosure(true)), path(IconName::ChevronDown));
        assert_eq!(path(disclosure(false)), path(IconName::ChevronRight));
    }

    #[test]
    fn prompts_carry_no_role_icon() {
        // Alignment and width already set them apart; a glyph on every prompt
        // would be noise.
        assert!(role(&ConversationRole::User).is_none());
        for with_icon in [
            ConversationRole::Assistant,
            ConversationRole::Tool,
            ConversationRole::System,
        ] {
            assert!(role(&with_icon).is_some(), "{with_icon:?}");
        }
    }

    #[test]
    fn roles_are_visually_distinct_from_each_other() {
        let assistant = path(role(&ConversationRole::Assistant).unwrap());
        let tool = path(role(&ConversationRole::Tool).unwrap());
        let system = path(role(&ConversationRole::System).unwrap());

        assert_ne!(assistant, tool);
        assert_ne!(tool, system);
        assert_ne!(assistant, system);
    }

    #[test]
    fn idle_states_add_no_icon_beside_the_dot() {
        // The coloured dot already says as much.
        assert!(status(&SessionStatus::Ready).is_none());
        assert!(status(&SessionStatus::Offline).is_none());
    }

    #[test]
    fn states_worth_noticing_get_an_icon() {
        for noticeable in [
            SessionStatus::Failed("boom".into()),
            SessionStatus::Starting,
            SessionStatus::Streaming,
            SessionStatus::Compacting,
        ] {
            assert!(status(&noticeable).is_some(), "{noticeable:?}");
        }
    }

    #[test]
    fn failure_does_not_look_like_progress() {
        let failed = path(status(&SessionStatus::Failed("boom".into())).unwrap());
        let working = path(status(&SessionStatus::Streaming).unwrap());
        assert_ne!(failed, working);
    }

    #[test]
    fn send_and_stop_are_different_glyphs() {
        // The button swaps between them while the agent works.
        assert_ne!(path(send()), path(stop()));
    }

    #[test]
    fn every_icon_resolves_to_a_bundled_svg() {
        // A path outside the assets directory would silently draw nothing.
        for icon in [
            settings(),
            add(),
            close(),
            copy(),
            search(),
            send(),
            stop(),
            compact(),
            session(),
            theme_toggle(true),
            project(true),
            disclosure(true),
        ] {
            let path = path(icon);
            assert!(path.starts_with("icons/"), "unexpected path {path}");
            assert!(path.ends_with(".svg"), "unexpected path {path}");
        }
    }
}
