//! Window chrome: the title row, status indicators, and banners that frame the
//! conversation.
//!
//! Each piece is its own type rather than a method on one struct, which is what
//! keeps this crate from growing into the single 3.5k-line file the egui build
//! ended up with.

mod banner;
mod metrics;
mod status_pill;
mod top_bar;

pub use banner::{Banner, Severity};
pub use metrics::{SessionMeter, format_cost, format_tokens};
pub use status_pill::{StatusPill, status_label};
pub use top_bar::TopBar;
