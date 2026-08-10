use std::{
    any::Any,
    collections::{BTreeMap, HashMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use pi_whim_core::HookConfig;
use pi_whim_hook_host::{
    ApprovedHookManifest, EventRegistry, HookAuditEvent, HookAuditOutcome, HookHostHealth,
    HookHostManager, HookManifest, HookScopeKey,
};
use pi_whim_persistence::hook_manifest_fingerprint;
use pi_whim_runtime::RuntimeHookScope;

const EMPTY_GLOBAL_REVISION: &str = "v1:global-empty";

#[derive(Clone, Debug)]
pub(super) struct LoadedHooks {
    pub(super) legacy: HookConfig,
    pub(super) global: HookConfig,
    pub(super) project: Option<HookConfig>,
}

impl LoadedHooks {
    pub(super) fn global_only(global: HookConfig) -> Self {
        Self {
            legacy: global.clone(),
            global,
            project: None,
        }
    }

    pub(super) fn revision(&self) -> &str {
        &self.legacy.revision
    }
}

#[derive(Clone)]
pub(super) struct AppHookAudit {
    pub(super) project_path: String,
    pub(super) hook_id: String,
    pub(super) event: String,
    pub(super) outcome: String,
    pub(super) duration_ms: u64,
    pub(super) revision: String,
}

struct ManagerOwner {
    manager: HookHostManager,
    latest_health: Arc<Mutex<Vec<HookHostHealth>>>,
    _audit_subscription: Box<dyn Any + Send + Sync>,
    _health_subscription: Box<dyn Any + Send + Sync>,
}

/// Application-lifetime owner for shared external Hook hosts.
///
/// One manager is retained per global manifest revision. Each manager is
/// subscribed exactly once here; supervisors receive only cloneable scopes and
/// therefore cannot duplicate external audit forwarding.
pub(super) struct ApplicationHookHost {
    owners: HashMap<String, ManagerOwner>,
    scope_projects: HashMap<String, String>,
    audit_sender: Sender<HookAuditEvent>,
    audit_receiver: Receiver<HookAuditEvent>,
    pending_unmapped_audits: VecDeque<HookAuditEvent>,
    pending_mapped_audits: VecDeque<AppHookAudit>,
}

impl std::fmt::Debug for ApplicationHookHost {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ApplicationHookHost")
            .field("manager_count", &self.owners.len())
            .field("scope_count", &self.scope_projects.len())
            .finish()
    }
}

impl Default for ApplicationHookHost {
    fn default() -> Self {
        let (audit_sender, audit_receiver) = unbounded();
        Self {
            owners: HashMap::new(),
            scope_projects: HashMap::new(),
            audit_sender,
            audit_receiver,
            pending_unmapped_audits: VecDeque::new(),
            pending_mapped_audits: VecDeque::new(),
        }
    }
}

impl ApplicationHookHost {
    pub(super) fn resolve_scope(
        &mut self,
        project_path: &Path,
        global: &HookConfig,
        project: Option<&HookConfig>,
    ) -> Result<Option<RuntimeHookScope>, String> {
        if global.hooks.is_empty() && project.is_none_or(|config| config.hooks.is_empty()) {
            return Ok(None);
        }

        let global_revision = effective_global_revision(global);
        if !self.owners.contains_key(&global_revision) {
            let approved_global = approved_manifest(global, &global_revision)?;
            let manager =
                HookHostManager::new_with_registry(EventRegistry::default(), approved_global)
                    .map_err(|error| error.to_string())?;
            let audit_sender = self.audit_sender.clone();
            let audit_subscription = manager.audit_signal().subscribe_fn(move |event| {
                let _ = audit_sender.send(event);
            });
            let latest_health = Arc::new(Mutex::new(Vec::new()));
            let health_sink = latest_health.clone();
            let health_subscription = manager.health_signal().subscribe_fn(move |snapshot| {
                if let Ok(mut latest) = health_sink.lock() {
                    *latest = snapshot;
                }
            });
            self.owners.insert(
                global_revision.clone(),
                ManagerOwner {
                    manager,
                    latest_health,
                    _audit_subscription: Box::new(audit_subscription),
                    _health_subscription: Box::new(health_subscription),
                },
            );
        }

        let scope_revision = combined_revision(global, project)?;
        let key = HookScopeKey::project(project_path, scope_revision.clone())
            .map_err(|error| error.to_string())?;
        let project_manifest = project
            .map(|config| approved_manifest(config, &scope_revision))
            .transpose()?;
        let manager = self
            .owners
            .get(&global_revision)
            .map(|owner| owner.manager.clone())
            .ok_or_else(|| "hook host manager was not retained".to_owned())?;
        let scope = manager
            .open_scope(key, project_manifest)
            .map_err(|error| error.to_string())?;
        self.scope_projects.insert(
            scope.scope_id(),
            project_path.to_string_lossy().into_owned(),
        );
        Ok(Some(RuntimeHookScope::new(manager, scope)))
    }

    pub(super) fn drain_audits(&mut self) -> Vec<AppHookAudit> {
        self.pending_unmapped_audits
            .extend(self.audit_receiver.try_iter());
        let mut unmapped = VecDeque::new();
        while let Some(event) = self.pending_unmapped_audits.pop_front() {
            let Some(project_path) = self.scope_projects.get(&event.scope_id).cloned() else {
                unmapped.push_back(event);
                continue;
            };
            self.pending_mapped_audits.push_back(AppHookAudit {
                project_path,
                hook_id: event.hook_id,
                event: event.event,
                outcome: audit_outcome_name(event.outcome).to_owned(),
                duration_ms: event.duration_ms,
                revision: event.revision,
            });
        }
        self.pending_unmapped_audits = unmapped;
        self.pending_mapped_audits.drain(..).collect()
    }

    pub(super) fn requeue_audits(&mut self, audits: Vec<AppHookAudit>) {
        for audit in audits.into_iter().rev() {
            self.pending_mapped_audits.push_front(audit);
        }
    }

    /// Retained for the forthcoming UI health surface. Reading snapshots does
    /// not create another subscription.
    #[allow(dead_code)]
    pub(super) fn health_snapshot(&self) -> Vec<HookHostHealth> {
        self.owners
            .values()
            .filter_map(|owner| owner.latest_health.lock().ok())
            .flat_map(|snapshot| snapshot.clone())
            .collect()
    }
}

fn approved_manifest(config: &HookConfig, revision: &str) -> Result<ApprovedHookManifest, String> {
    config.validate()?;
    let fingerprints = config
        .hooks
        .iter()
        .filter_map(|hook| {
            hook.entrypoint_fingerprint
                .as_ref()
                .map(|fingerprint| (hook.id.clone(), fingerprint.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let json = serde_json::to_string(config).map_err(|error| error.to_string())?;
    let manifest = HookManifest::parse_json(&json)
        .and_then(|manifest| {
            manifest
                .with_revision(revision)
                .with_entrypoint_fingerprints(&fingerprints)
        })
        .map_err(|error| error.to_string())?;
    ApprovedHookManifest::new(manifest, revision, fingerprints).map_err(|error| error.to_string())
}

fn effective_global_revision(config: &HookConfig) -> String {
    if config.revision.is_empty() {
        EMPTY_GLOBAL_REVISION.to_owned()
    } else {
        config.revision.clone()
    }
}

fn combined_revision(global: &HookConfig, project: Option<&HookConfig>) -> Result<String, String> {
    let project_revision = project.map(|config| config.revision.as_str()).unwrap_or("");
    let encoded = serde_json::to_vec(&(effective_global_revision(global), project_revision))
        .map_err(|error| error.to_string())?;
    Ok(format!("sha256:{}", hook_manifest_fingerprint(&encoded)))
}

fn audit_outcome_name(outcome: HookAuditOutcome) -> &'static str {
    match outcome {
        HookAuditOutcome::Allowed => "allowed",
        HookAuditOutcome::Denied => "denied",
        HookAuditOutcome::Transformed => "transformed",
        HookAuditOutcome::Preserved => "preserved",
        HookAuditOutcome::Observed => "observed",
        HookAuditOutcome::Failed => "failed",
        HookAuditOutcome::TimedOut => "timed_out",
        HookAuditOutcome::Dropped => "dropped",
        HookAuditOutcome::Restarted => "restarted",
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use pi_whim_core::{HookDefinition, HookEvent, HookKind, HookMatcher};
    use pi_whim_hook_host::{HookGateDecision, HookInvocationContext, HookPayload};
    use serde_json::json;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn script(directory: &TempDir, name: &str, decision: &str) -> Result<String, String> {
        let path = directory.path().join(name);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{{\"decision\":\"{decision}\"}}'\n"
            ),
        )
        .map_err(|error| error.to_string())?;
        let mut permissions = fs::metadata(&path)
            .map_err(|error| error.to_string())?
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).map_err(|error| error.to_string())?;
        Ok(path.to_string_lossy().into_owned())
    }

    fn gate_config(id: &str, command: String, revision: &str) -> HookConfig {
        let entrypoint_fingerprint = fs::read(&command)
            .map(|source| hook_manifest_fingerprint(&source))
            .ok();
        HookConfig {
            version: 1,
            hooks: vec![HookDefinition {
                id: id.to_owned(),
                event: HookEvent::ToolDispatching,
                kind: HookKind::Gate,
                command: vec![command],
                timeout_ms: Some(1_000),
                matcher: HookMatcher::default(),
                entrypoint_fingerprint,
            }],
            revision: revision.to_owned(),
        }
    }

    fn gate(scope: &RuntimeHookScope) -> Result<HookGateDecision, String> {
        let key = scope.key();
        let project_root = key
            .project_root
            .as_ref()
            .ok_or_else(|| "scope lost project root".to_owned())?
            .to_string_lossy()
            .into_owned();
        scope
            .scope()
            .gate(
                "pi.tool.dispatching",
                HookInvocationContext::project(
                    scope.scope_id(),
                    key.manifest_revision,
                    project_root,
                ),
                HookPayload::from_value(json!({"tool": "shell", "arguments": {}}))
                    .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())
    }

    #[test]
    fn same_project_and_revision_reuse_scope() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = HookConfig::default();
        let project = gate_config(
            "project",
            script(&directory, "allow.sh", "allow")?,
            "project-r1",
        );
        let first = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected first scope".to_owned())?;
        let second = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected second scope".to_owned())?;
        assert_eq!(first.scope_id(), second.scope_id());
        assert_eq!(host.owners.len(), 1);
        Ok(())
    }

    #[test]
    fn project_revision_changes_scope_but_reuses_global_owner() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = HookConfig::default();
        let command = script(&directory, "allow.sh", "allow")?;
        let first = host
            .resolve_scope(
                directory.path(),
                &global,
                Some(&gate_config("project", command.clone(), "project-r1")),
            )?
            .ok_or_else(|| "expected first scope".to_owned())?;
        let second = host
            .resolve_scope(
                directory.path(),
                &global,
                Some(&gate_config("project", command, "project-r2")),
            )?
            .ok_or_else(|| "expected second scope".to_owned())?;
        assert_ne!(first.scope_id(), second.scope_id());
        assert_eq!(host.owners.len(), 1);
        Ok(())
    }

    #[test]
    fn project_manifest_is_not_promoted_to_global() -> Result<(), String> {
        let project_a = tempfile::tempdir().map_err(|error| error.to_string())?;
        let project_b = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = HookConfig::default();
        let project = gate_config(
            "project-deny",
            script(&project_a, "deny.sh", "deny")?,
            "project-r1",
        );
        let scoped = host
            .resolve_scope(project_a.path(), &global, Some(&project))?
            .ok_or_else(|| "expected project scope".to_owned())?;
        assert!(matches!(gate(&scoped)?, HookGateDecision::Deny { .. }));

        let global_only = gate_config(
            "global-allow",
            script(&project_b, "allow.sh", "allow")?,
            "global-r1",
        );
        let other = host
            .resolve_scope(project_b.path(), &global_only, None)?
            .ok_or_else(|| "expected global scope".to_owned())?;
        assert!(matches!(gate(&other)?, HookGateDecision::Allow));
        Ok(())
    }

    #[test]
    fn one_manager_subscription_emits_one_external_audit() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = HookConfig::default();
        let project = gate_config(
            "project",
            script(&directory, "allow.sh", "allow")?,
            "project-r1",
        );
        let first = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected first scope".to_owned())?;
        let second = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected second scope".to_owned())?;
        assert_eq!(first.scope_id(), second.scope_id());
        assert!(matches!(gate(&first)?, HookGateDecision::Allow));
        let audits = host.drain_audits();
        assert_eq!(audits.len(), 1);
        assert_eq!(audits[0].hook_id, "project");
        assert_eq!(audits[0].project_path, directory.path().to_string_lossy());
        Ok(())
    }

    #[test]
    fn scope_failure_leaves_legacy_config_available() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let project_file = directory.path().join("not-a-project-directory");
        fs::write(&project_file, b"not a directory").map_err(|error| error.to_string())?;
        let global = gate_config(
            "global",
            script(&directory, "allow.sh", "allow")?,
            "global-r1",
        );
        let loaded = LoadedHooks::global_only(global);
        let expected_legacy = loaded.legacy.clone();
        let mut host = ApplicationHookHost::default();
        assert!(
            host.resolve_scope(&project_file, &loaded.global, loaded.project.as_ref())
                .is_err()
        );
        assert_eq!(loaded.legacy, expected_legacy);
        Ok(())
    }

    #[test]
    fn no_hooks_do_not_create_a_manager_or_scope() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        assert!(
            host.resolve_scope(directory.path(), &HookConfig::default(), None)?
                .is_none()
        );
        assert!(host.owners.is_empty());
        Ok(())
    }
}
