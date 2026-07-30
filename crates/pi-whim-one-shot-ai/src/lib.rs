//! Bounded, tool-free background AI requests without starting a Pi process.

mod protocol;
mod service;
mod task;

pub use pi_whim_core::{OneShotAiConfig, OneShotAiTaskConfig};
pub use service::{
    OneShotAiService, OneShotCompletion, OneShotErrorKind, OneShotRequestId, OneShotServiceError,
    OneShotStats, OneShotSubmitError, ResolvedOneShotAiConfig,
};
pub use task::{
    MAX_ONE_SHOT_INPUT_BYTES, OneShotTask, SessionTitleTask, fallback_session_title,
    normalize_session_title,
};
