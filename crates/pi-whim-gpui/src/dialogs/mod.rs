//! Modals and notifications.
//!
//! These lived in the app crate in the egui build, which meant the composition
//! root also drew windows and reached into wire JSON at render time. What a
//! request *means* now lives in `engine::dialogs`; what it looks like lives here.

mod notices;
mod prompt;
mod rename;

pub use notices::{notification, show as show_notices};
pub use prompt::{PromptEvent, Prompts};
pub use rename::{Rename, RenameEvent};
