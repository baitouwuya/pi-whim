//! v1 execution adapter.

use crate::HookHostResult;
use crate::manifest::HookDefinition;
use crate::sandbox;
use serde_json::Value;
use std::path::Path;

pub(crate) fn invoke_v1(
    definition: &HookDefinition,
    event: &str,
    payload: &Value,
    project_root: &Path,
) -> HookHostResult<Value> {
    sandbox::invoke_one_shot(definition, event, payload, project_root)
}
