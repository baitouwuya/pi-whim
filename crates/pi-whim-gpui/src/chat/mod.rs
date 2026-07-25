//! The chat page: the sidebar of projects and sessions, the conversation, and
//! the prompt input.

mod composer;
mod controls;
mod conversation;
mod message_card;
mod palette;
mod paste;
mod rows;
mod sidebar;

pub use composer::{Composer, ComposerEvent};
pub use controls::{Controls, ControlsEvent};
pub use conversation::{Conversation, ConversationEvent, visible_messages};
pub use message_card::MessageCard;
pub use palette::{Palette, PaletteEvent};
pub use paste::{Clipboard, Paste, classify};
pub use rows::{Row, rows};
pub use sidebar::{Sidebar, SidebarEvent};
