//! Backend orchestration between the UI and Pi.
//!
//! The UI talks to this crate; this crate owns the session pool, drives the
//! `AgentRuntime` boundary, and translates Pi's wire protocol into domain
//! actions. It holds no UI framework types, so it can be exercised headlessly
//! against a fake runtime.

pub mod composer;
pub mod controls;
pub mod pool;
pub mod protocol;
pub mod providers;
pub mod session;
pub mod state;
pub mod typewriter;
