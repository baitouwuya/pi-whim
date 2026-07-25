//! Drawing primitives that are not views.
//!
//! Anything here paints texture or chrome that no domain state flows through, so
//! it takes tokens and geometry and nothing else.

mod graph_paper;

pub use graph_paper::GraphPaper;
