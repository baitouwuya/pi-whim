//! Unified v1/v2 Hook manifest loading and exact approval metadata.

use std::{collections::BTreeMap, fs, path::Path};

use pi_whim_core::{
    HookConfig, HookGrantDelivery, HookGrantDeliveryMode, HookGrantDescriptor, HookGrantKind,
    HookGrantMatcher, HookGrantRestart,
};
use pi_whim_hook_host::{
    ApprovedHookManifest, DeliveryMode, EventRegistry, HookKind, HookManifest,
};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug)]
pub(crate) struct PreparedHookManifest {
    pub(crate) approved: ApprovedHookManifest,
    pub(crate) legacy: HookConfig,
    pub(crate) fingerprint: String,
    pub(crate) grants_hash: String,
    pub(crate) grants: Vec<HookGrantDescriptor>,
}

impl PreparedHookManifest {
    pub(crate) fn revision(&self) -> &str {
        &self.approved.revision
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.approved.manifest.hooks.is_empty()
    }
}

pub(crate) fn empty_manifest(revision: &str) -> PreparedHookManifest {
    let manifest = HookManifest::new(1, Vec::new()).with_revision(revision);
    let legacy = HookConfig {
        revision: revision.to_owned(),
        ..HookConfig::default()
    };
    PreparedHookManifest {
        approved: ApprovedHookManifest {
            manifest,
            revision: revision.to_owned(),
            entrypoint_fingerprints: BTreeMap::new(),
        },
        legacy,
        fingerprint: sha256_hex(br#"{"version":1,"hooks":[]}"#),
        grants_hash: sha256_hex(b"[]"),
        grants: Vec::new(),
    }
}

pub(crate) fn prepare_manifest(
    source: &[u8],
    project_scoped: bool,
    revision_override: Option<&str>,
) -> Result<PreparedHookManifest, String> {
    let source_text = std::str::from_utf8(source).map_err(|error| error.to_string())?;
    let registry = EventRegistry::default();
    let parsed = HookManifest::parse_json(source_text).map_err(|error| error.to_string())?;
    parsed
        .validate(&registry)
        .map_err(|error| error.to_string())?;

    let mut combined = Sha256::new();
    combined.update((source.len() as u64).to_le_bytes());
    combined.update(source);
    let mut entrypoint_fingerprints = BTreeMap::new();
    for hook in &parsed.hooks {
        let program = hook
            .command
            .first()
            .ok_or_else(|| format!("hook {} has no entrypoint", hook.id))?;
        let bytes = fs::read(Path::new(program)).map_err(|error| error.to_string())?;
        let entrypoint = sha256_hex(&bytes);
        entrypoint_fingerprints.insert(hook.id.clone(), entrypoint);
        combined.update((program.len() as u64).to_le_bytes());
        combined.update(program.as_bytes());
        combined.update((bytes.len() as u64).to_le_bytes());
        combined.update(&bytes);
    }
    let fingerprint = hex_digest(combined.finalize());
    let revision = revision_override
        .map(str::to_owned)
        .unwrap_or_else(|| format!("sha256:{fingerprint}"));
    let manifest = parsed
        .clone()
        .with_revision(&revision)
        .with_entrypoint_fingerprints(&entrypoint_fingerprints)
        .map_err(|error| error.to_string())?;

    let grants = parsed
        .hooks
        .iter()
        .map(|hook| {
            let event = registry
                .canonical_event(&hook.event)
                .ok_or_else(|| format!("unknown hook event {}", hook.event))?;
            let fields = registry
                .effective_fields(parsed.version, hook, project_scoped)
                .map_err(|error| error.to_string())?
                .into_iter()
                .map(|field| field.name)
                .collect();
            let entrypoint_sha256 = entrypoint_fingerprints
                .get(&hook.id)
                .cloned()
                .ok_or_else(|| format!("missing entrypoint fingerprint for {}", hook.id))?;
            Ok(HookGrantDescriptor {
                hook_id: hook.id.clone(),
                event,
                kind: match hook.kind {
                    HookKind::Gate => HookGrantKind::Gate,
                    HookKind::Transform => HookGrantKind::Transform,
                    HookKind::Observe => HookGrantKind::Observe,
                },
                fields,
                matcher: HookGrantMatcher {
                    tools: hook.matcher.tools.clone(),
                    agent_levels: hook.matcher.agent_levels.clone(),
                    metadata: hook.matcher.extra.clone(),
                },
                delivery: HookGrantDelivery {
                    mode: match hook.delivery.mode {
                        DeliveryMode::RequestResponse => HookGrantDeliveryMode::RequestResponse,
                        DeliveryMode::StateLatest => HookGrantDeliveryMode::StateLatest,
                        DeliveryMode::Telemetry => HookGrantDeliveryMode::Telemetry,
                    },
                    capacity: hook.delivery.capacity,
                },
                restart: HookGrantRestart {
                    max_restarts: hook.restart.max_restarts,
                    initial_backoff_ms: hook.restart.initial_backoff_ms,
                    max_backoff_ms: hook.restart.max_backoff_ms,
                },
                entrypoint_sha256,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let grants_json = serde_json::to_vec(&grants).map_err(|error| error.to_string())?;
    let grants_hash = sha256_hex(&grants_json);

    let legacy = if parsed.version == 1 {
        let mut config =
            serde_json::from_slice::<HookConfig>(source).map_err(|error| error.to_string())?;
        config.validate()?;
        for hook in &mut config.hooks {
            hook.entrypoint_fingerprint = entrypoint_fingerprints.get(&hook.id).cloned();
        }
        config.revision = revision.clone();
        config
    } else {
        HookConfig::default()
    };
    let approved = ApprovedHookManifest::new(manifest, revision, entrypoint_fingerprints)
        .map_err(|error| error.to_string())?;
    Ok(PreparedHookManifest {
        approved,
        legacy,
        fingerprint,
        grants_hash,
        grants,
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::*;

    #[test]
    fn v1_expands_registry_fields_and_retains_legacy_fallback() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let entrypoint = directory.path().join("legacy-hook");
        fs::write(&entrypoint, b"legacy").map_err(|error| error.to_string())?;
        let source = serde_json::to_vec(&json!({
            "version": 1,
            "hooks": [{
                "id": "legacy",
                "event": "tool_dispatching",
                "kind": "gate",
                "command": [entrypoint]
            }]
        }))
        .map_err(|error| error.to_string())?;

        let prepared = prepare_manifest(&source, true, None)?;
        assert_eq!(prepared.approved.manifest.version, 1);
        assert_eq!(prepared.legacy.hooks.len(), 1);
        assert_eq!(prepared.grants[0].event, "pi.tool.dispatching");
        assert!(prepared.grants[0].fields.contains(&"tool".to_owned()));
        assert!(prepared.grants[0].fields.contains(&"arguments".to_owned()));
        Ok(())
    }

    #[test]
    fn v2_is_shared_only_and_exact_grants_track_manifest_and_entrypoint() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let entrypoint = directory.path().join("resident-hook");
        fs::write(&entrypoint, b"version-one").map_err(|error| error.to_string())?;
        let source = |fields: &[&str]| {
            serde_json::to_vec(&json!({
                "version": 2,
                "hooks": [{
                    "id": "resident",
                    "event": "pi.ui.command.submitting",
                    "kind": "gate",
                    "command": [entrypoint],
                    "fields": fields,
                    "matcher": {"source": "ui"},
                    "delivery": {"mode": "request_response", "capacity": 1},
                    "restart": {
                        "max_restarts": 2,
                        "initial_backoff_ms": 250,
                        "max_backoff_ms": 1000
                    }
                }]
            }))
            .map_err(|error| error.to_string())
        };
        let first = prepare_manifest(&source(&["command_name", "arguments"])?, true, None)?;
        assert!(first.legacy.hooks.is_empty());
        assert_eq!(first.grants[0].fields, ["command_name", "arguments"]);

        let field_changed = prepare_manifest(&source(&["arguments"])?, true, None)?;
        assert_ne!(first.fingerprint, field_changed.fingerprint);
        assert_ne!(first.grants_hash, field_changed.grants_hash);

        fs::write(&entrypoint, b"version-two").map_err(|error| error.to_string())?;
        let entrypoint_changed =
            prepare_manifest(&source(&["command_name", "arguments"])?, true, None)?;
        assert_ne!(first.fingerprint, entrypoint_changed.fingerprint);
        assert_ne!(first.grants_hash, entrypoint_changed.grants_hash);
        Ok(())
    }

    #[test]
    fn exact_grants_hash_tracks_matcher_delivery_and_restart_authority() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let entrypoint = directory.path().join("resident-hook");
        fs::write(&entrypoint, b"resident").map_err(|error| error.to_string())?;
        let base = json!({
            "version": 2,
            "hooks": [{
                "id": "resident",
                "event": "pi.ui.command.submitting",
                "kind": "gate",
                "command": [entrypoint],
                "fields": ["command_name", "arguments"],
                "matcher": {"source": "ui"},
                "delivery": {"mode": "request_response", "capacity": 1},
                "restart": {
                    "max_restarts": 1,
                    "initial_backoff_ms": 250,
                    "max_backoff_ms": 1000
                }
            }]
        });
        let prepare = |manifest: &serde_json::Value| {
            let source = serde_json::to_vec(manifest).map_err(|error| error.to_string())?;
            prepare_manifest(&source, true, None)
        };
        let original = prepare(&base)?;

        let mut matcher_changed = base.clone();
        matcher_changed["hooks"][0]["matcher"]["source"] = json!("system");
        let mut delivery_changed = base.clone();
        delivery_changed["hooks"][0]["delivery"]["capacity"] = json!(2);
        let mut restart_changed = base;
        restart_changed["hooks"][0]["restart"]["max_restarts"] = json!(2);

        for changed in [matcher_changed, delivery_changed, restart_changed] {
            let prepared = prepare(&changed)?;
            assert_ne!(original.fingerprint, prepared.fingerprint);
            assert_ne!(original.grants_hash, prepared.grants_hash);
            assert_ne!(original.grants, prepared.grants);
        }
        Ok(())
    }
}
