//! Staging what a Pi process needs before it starts.
//!
//! Configuration only: the process launch itself stays with whoever owns the
//! session pool. Kept apart from `providers` because this writes files into the
//! agent directory rather than describing providers.

use std::{collections::HashMap, fs, path::Path};

use pi_whim_core::ProviderProfile;
use pi_whim_persistence::SecretStore;
use serde_json::{Value, json};

use crate::providers::{
    configured_provider_environment, pi_models_json, provider_keychain_account,
};

/// Stage the configuration a Pi process reads on startup.
///
/// Writes `models.json` (which references API keys by environment variable name,
/// never by value) and lowers pi-mono's `keepRecentTokens` so that small sessions
/// can still be compacted. Returns the environment the process should inherit.
pub fn prepare_pi_configuration(
    agent_directory_override: Option<&Path>,
    profiles: Vec<ProviderProfile>,
    secrets: &dyn SecretStore,
) -> Result<HashMap<String, String>, String> {
    const PI_COMPACTION_KEEP_RECENT_TOKENS: u64 = 100;
    let agent_directory = agent_directory_override
        .map(|path| Ok(path.to_path_buf()))
        .unwrap_or_else(crate::session::pi_agent_directory)?;
    fs::create_dir_all(&agent_directory).map_err(|error| error.to_string())?;
    // Profiles arrive already enriched with model capabilities: that needs the
    // catalog, which is the caller's to own.
    let (configured_profiles, mut environment) =
        configured_provider_environment(profiles, |profile_id| {
            secrets
                .get(&provider_keychain_account(profile_id))
                .map_err(|error| error.to_string())
        })?;
    fs::write(
        agent_directory.join("models.json"),
        serde_json::to_vec_pretty(&pi_models_json(&configured_profiles))
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    // Lower pi-mono's keepRecentTokens (default 20000) so small sessions can be compacted.
    let settings_path = agent_directory.join("settings.json");
    let mut settings: Value = fs::read(&settings_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(obj) = settings.as_object_mut() {
        let compaction = obj
            .entry("compaction".to_string())
            .or_insert_with(|| json!({}));
        if let Some(compaction) = compaction.as_object_mut() {
            compaction.insert(
                "keepRecentTokens".to_string(),
                Value::from(PI_COMPACTION_KEEP_RECENT_TOKENS),
            );
        }
    }
    fs::write(
        &settings_path,
        serde_json::to_vec_pretty(&settings).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;

    environment.insert(
        "PI_CODING_AGENT_DIR".into(),
        agent_directory.to_string_lossy().into_owned(),
    );
    Ok(environment)
}
