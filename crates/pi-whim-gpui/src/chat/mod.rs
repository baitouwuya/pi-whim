//! The chat page: the sidebar of projects and sessions, the conversation, and
//! the prompt input.

mod composer;
mod controls;
mod conversation;
mod cross_task_message;
mod dropdown;
mod message_card;
mod message_disclosure;
mod message_layout;
mod model_picker;
mod palette;
mod paste;
mod queue_status;
mod rows;
mod sidebar;
mod tool_card;

pub use composer::{Composer, ComposerEvent};
pub use controls::{Controls, ControlsEvent};
pub use conversation::{Conversation, ConversationEvent, visible_messages};
pub use cross_task_message::CrossTaskMessage;
pub use message_card::MessageCard;
pub(crate) use message_layout::reading_lane;
pub use palette::{Palette, PaletteEvent, PaletteKey};
pub use paste::{Clipboard, Paste, classify};
pub use queue_status::QueueStatus;
pub use rows::{Row, rows, searchable_rows, session_title_or_default};
pub use sidebar::{Sidebar, SidebarEvent};
pub(crate) use tool_card::ToolCard;
