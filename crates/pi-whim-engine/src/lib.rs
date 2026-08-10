//! Backend orchestration between the UI and Pi.
//!
//! The UI talks to this crate; this crate owns the session pool, drives the
//! `AgentRuntime` boundary, and translates Pi's wire protocol into domain
//! actions. It holds no UI framework types, so it can be exercised headlessly
//! against a fake runtime.

pub mod changes;
pub mod composer;
pub mod controls;
pub mod dialogs;
pub mod events;
pub mod launch;
pub mod mailbox;
pub mod notice;
pub mod pool;
pub mod protocol;
pub mod providers;
pub mod replay;
pub mod session;
pub mod settings;
pub mod slash_commands;
pub mod state;
pub mod thinking;
pub mod typewriter;

pub use replay::{ReplaySelection, StateSelector, StateSelectorError};

pub use changes::{
    ChangeSet, CommitContext, CommitError, CommitScope, CommitSource, SessionIdentity, StateTopic,
    TransactionRevision,
};
