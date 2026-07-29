//! Drawing primitives that are not views.
//!
//! Anything here paints texture or chrome that no domain state flows through, so
//! it takes tokens and geometry and nothing else.

mod graph_paper;
mod isolated_scroll;

pub use graph_paper::GraphPaper;
pub use isolated_scroll::{isolated_manual_vertical_scroll_area, isolated_vertical_scroll_area};
