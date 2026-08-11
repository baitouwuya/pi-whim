//! Domain-neutral, bounded waiting over sanitized typed event sources.

mod coordinator;
mod hub;
mod runtime;
mod types;

pub use hub::{WaitHub, WaitSourceHandle};
pub use runtime::{WaitRuntimeMetadata, WaitRuntimeScope};
pub use types::{
    MAX_WAIT_CLAUSES, WaitClause, WaitError, WaitEvent, WaitMatcher, WaitOwnerId, WaitQuery,
    WaitSourceDescriptor, WaitSourceId, WaitSourceSelection, WaitStatus, WaitTaskId,
    WaitTaskMetadata, WaitTaskSnapshot,
};

#[cfg(test)]
mod tests;
