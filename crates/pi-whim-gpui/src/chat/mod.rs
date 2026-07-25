//! The chat page: the sidebar of projects and sessions, and the conversation.

mod rows;
mod sidebar;

pub use rows::{Row, rows};
pub use sidebar::{Sidebar, SidebarEvent};
