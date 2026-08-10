use std::{
    any::Any,
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{Arc, Mutex},
};

use crossbeam_channel::{Receiver, Sender, unbounded};
use pi_whim_hook_host::{
    EventRegistry, HookAuditEvent, HookAuditOutcome, HookHostHealth, HookHostManager,
    HookInvocationContext, HookScopeKey,
};
use pi_whim_persistence::hook_manifest_fingerprint;
use pi_whim_runtime::RuntimeHookScope;

#[cfg(test)]
static HOOK_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(super) fn hook_test_lock() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    HOOK_TEST_LOCK.lock().map_err(|error| error.to_string())
}

#[allow(
    dead_code,
    reason = "the Host will call the typed command controller in the next integration task"
)]
mod commands;
pub(super) mod manifest;

#[allow(
    unused_imports,
    reason = "the public error is consumed with the controller by the next Host task"
)]
pub(crate) use commands::{CommandHookController, CommandHookError};

#[derive(Clone, Debug)]
pub(super) struct LoadedHooks {
    pub(super) legacy: pi_whim_core::HookConfig,
    pub(super) global: manifest::PreparedHookManifest,
    pub(super) project: Option<manifest::PreparedHookManifest>,
    revision: String,
}

impl LoadedHooks {
    pub(super) fn global_only(global: manifest::PreparedHookManifest) -> Self {
        Self {
            legacy: global.legacy.clone(),
            revision: global.revision().to_owned(),
            global,
            project: None,
        }
    }

    pub(super) fn approved(
        global: manifest::PreparedHookManifest,
        project: manifest::PreparedHookManifest,
    ) -> Result<Self, String> {
        let revision = combined_revision(&global, Some(&project))?;
        let mut legacy = global.legacy.clone();
        legacy.hooks.extend(project.legacy.hooks.clone());
        legacy.revision = revision.clone();
        legacy.validate()?;
        Ok(Self {
            legacy,
            global,
            project: Some(project),
            revision,
        })
    }

    pub(super) fn revision(&self) -> &str {
        &self.revision
    }

    pub(super) fn requires_shared_scope(&self) -> bool {
        (!self.global.is_empty() && self.global.approved.manifest.version == 2)
            || self.project.as_ref().is_some_and(|project| {
                !project.is_empty() && project.approved.manifest.version == 2
            })
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
    latest_scopes: HashMap<String, RuntimeHookScope>,
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
            .field("latest_project_count", &self.latest_scopes.len())
            .finish()
    }
}

impl Default for ApplicationHookHost {
    fn default() -> Self {
        let (audit_sender, audit_receiver) = unbounded();
        Self {
            owners: HashMap::new(),
            scope_projects: HashMap::new(),
            latest_scopes: HashMap::new(),
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
        global: &manifest::PreparedHookManifest,
        project: Option<&manifest::PreparedHookManifest>,
    ) -> Result<Option<RuntimeHookScope>, String> {
        let project_key = project_path.to_string_lossy().into_owned();
        if global.is_empty() && project.is_none_or(manifest::PreparedHookManifest::is_empty) {
            self.latest_scopes.remove(&project_key);
            return Ok(None);
        }

        let global_revision = effective_global_revision(global);
        if !self.owners.contains_key(&global_revision) {
            let manager = HookHostManager::new_with_registry(
                EventRegistry::default(),
                global.approved.clone(),
            )
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
        let project_manifest = project.map(|prepared| {
            let mut approved = prepared.approved.clone();
            approved.revision = scope_revision.clone();
            approved.manifest.revision = scope_revision.clone();
            approved
        });
        let manager = self
            .owners
            .get(&global_revision)
            .map(|owner| owner.manager.clone())
            .ok_or_else(|| "hook host manager was not retained".to_owned())?;
        let scope = manager
            .open_scope(key, project_manifest)
            .map_err(|error| error.to_string())?;
        self.scope_projects
            .insert(scope.scope_id(), project_key.clone());
        let runtime_scope = RuntimeHookScope::new(manager, scope);
        self.latest_scopes
            .insert(project_key, runtime_scope.clone());
        Ok(Some(runtime_scope))
    }

    pub(super) fn revoke_project(&mut self, project_path: &Path) {
        let project_key = project_path.to_string_lossy();
        if let Some(runtime_scope) = self.latest_scopes.remove(project_key.as_ref()) {
            runtime_scope.scope().revoke();
        }
    }

    pub(super) fn controller(&self, project_path: &Path) -> Option<CommandHookController> {
        let project_key = project_path.to_string_lossy();
        let runtime_scope = self.latest_scopes.get(project_key.as_ref())?;
        let scope = runtime_scope.scope();
        let key = scope.key();
        let project_root = key.project_root?.to_string_lossy().into_owned();
        let context =
            HookInvocationContext::project(scope.scope_id(), key.manifest_revision, project_root);
        Some(CommandHookController::new(
            scope,
            context,
            self.audit_sender.clone(),
        ))
    }

    #[cfg(test)]
    pub(super) fn retain_scope_for_test(
        &mut self,
        project_path: &Path,
        runtime_scope: RuntimeHookScope,
    ) {
        let project_key = project_path.to_string_lossy().into_owned();
        self.scope_projects
            .insert(runtime_scope.scope_id(), project_key.clone());
        self.latest_scopes.insert(project_key, runtime_scope);
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

fn effective_global_revision(config: &manifest::PreparedHookManifest) -> String {
    config.revision().to_owned()
}

fn combined_revision(
    global: &manifest::PreparedHookManifest,
    project: Option<&manifest::PreparedHookManifest>,
) -> Result<String, String> {
    let project_revision = project
        .map(manifest::PreparedHookManifest::revision)
        .unwrap_or("");
    let encoded = serde_json::to_vec(&(global.revision(), project_revision))
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
    use pi_whim_hook_host::{HookGateDecision, HookPayload};
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

    fn gate_manifest(
        id: &str,
        command: String,
        revision: &str,
        project_scoped: bool,
    ) -> Result<manifest::PreparedHookManifest, String> {
        let config = pi_whim_core::HookConfig {
            version: 1,
            hooks: vec![HookDefinition {
                id: id.to_owned(),
                event: HookEvent::ToolDispatching,
                kind: HookKind::Gate,
                command: vec![command],
                timeout_ms: Some(1_000),
                matcher: HookMatcher::default(),
                entrypoint_fingerprint: None,
            }],
            revision: String::new(),
        };
        let source = serde_json::to_vec(&config).map_err(|error| error.to_string())?;
        manifest::prepare_manifest(&source, project_scoped, Some(revision))
    }

    fn v2_command_manifest(
        id: &str,
        command: String,
        revision: &str,
    ) -> Result<manifest::PreparedHookManifest, String> {
        let source = serde_json::to_vec(&json!({
            "version": 2,
            "hooks": [{
                "id": id,
                "event": "pi.ui.command.submitting",
                "kind": "gate",
                "command": [command],
                "fields": ["command_id", "command_name", "source", "project_id", "arguments"]
            }]
        }))
        .map_err(|error| error.to_string())?;
        manifest::prepare_manifest(&source, true, Some(revision))
    }

    fn empty_global() -> manifest::PreparedHookManifest {
        manifest::empty_manifest("v1:global-empty")
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
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = empty_global();
        let project = gate_manifest(
            "project",
            script(&directory, "allow.sh", "allow")?,
            "project-r1",
            true,
        )?;
        let first = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected first scope".to_owned())?;
        let second = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected second scope".to_owned())?;
        assert_eq!(first.scope_id(), second.scope_id());
        let controller = host
            .controller(directory.path())
            .ok_or_else(|| "expected shared command controller".to_owned())?;
        assert_eq!(controller.scope_id(), first.scope_id());
        assert_eq!(host.latest_scopes.len(), 1);
        assert_eq!(host.owners.len(), 1);
        Ok(())
    }

    #[test]
    fn v2_project_manifest_is_shared_only() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = empty_global();
        let project = v2_command_manifest(
            "project-v2",
            script(&directory, "resident.sh", "allow")?,
            "project-v2-r1",
        )?;
        let loaded = LoadedHooks::approved(global, project)?;

        assert!(loaded.legacy.hooks.is_empty());
        assert!(loaded.requires_shared_scope());
        let scope = host
            .resolve_scope(directory.path(), &loaded.global, loaded.project.as_ref())?
            .ok_or_else(|| "expected v2 shared scope".to_owned())?;
        assert!(!scope.scope().is_revoked());
        assert!(host.controller(directory.path()).is_some());
        Ok(())
    }

    #[test]
    fn revoking_project_immediately_revokes_shared_scope_and_controller() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = empty_global();
        let project = gate_manifest(
            "project",
            script(&directory, "allow-revoke.sh", "allow")?,
            "project-r1",
            true,
        )?;
        let scope = host
            .resolve_scope(directory.path(), &global, Some(&project))?
            .ok_or_else(|| "expected project scope".to_owned())?;
        let retained = scope.scope();
        assert!(host.controller(directory.path()).is_some());

        host.revoke_project(directory.path());

        assert!(retained.is_revoked());
        assert!(host.controller(directory.path()).is_none());
        Ok(())
    }

    #[test]
    fn project_revision_changes_scope_but_reuses_global_owner() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = empty_global();
        let command = script(&directory, "allow.sh", "allow")?;
        let first = host
            .resolve_scope(
                directory.path(),
                &global,
                Some(&gate_manifest(
                    "project",
                    command.clone(),
                    "project-r1",
                    true,
                )?),
            )?
            .ok_or_else(|| "expected first scope".to_owned())?;
        let second = host
            .resolve_scope(
                directory.path(),
                &global,
                Some(&gate_manifest("project", command, "project-r2", true)?),
            )?
            .ok_or_else(|| "expected second scope".to_owned())?;
        assert_ne!(first.scope_id(), second.scope_id());
        let controller = host
            .controller(directory.path())
            .ok_or_else(|| "expected latest command controller".to_owned())?;
        assert_eq!(controller.scope_id(), second.scope_id());
        assert_eq!(host.latest_scopes.len(), 1);
        assert_eq!(host.owners.len(), 1);
        Ok(())
    }

    #[test]
    fn project_manifest_is_not_promoted_to_global() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let project_a = tempfile::tempdir().map_err(|error| error.to_string())?;
        let project_b = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = empty_global();
        let project = gate_manifest(
            "project-deny",
            script(&project_a, "deny.sh", "deny")?,
            "project-r1",
            true,
        )?;
        let scoped = host
            .resolve_scope(project_a.path(), &global, Some(&project))?
            .ok_or_else(|| "expected project scope".to_owned())?;
        assert!(matches!(gate(&scoped)?, HookGateDecision::Deny { .. }));

        let global_only = gate_manifest(
            "global-allow",
            script(&project_b, "allow.sh", "allow")?,
            "global-r1",
            false,
        )?;
        let other = host
            .resolve_scope(project_b.path(), &global_only, None)?
            .ok_or_else(|| "expected global scope".to_owned())?;
        assert!(matches!(gate(&other)?, HookGateDecision::Allow));
        Ok(())
    }

    #[test]
    fn one_manager_subscription_emits_one_external_audit() -> Result<(), String> {
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        let global = empty_global();
        let project = gate_manifest(
            "project",
            script(&directory, "allow.sh", "allow")?,
            "project-r1",
            true,
        )?;
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
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let project_file = directory.path().join("not-a-project-directory");
        fs::write(&project_file, b"not a directory").map_err(|error| error.to_string())?;
        let global = gate_manifest(
            "global",
            script(&directory, "allow.sh", "allow")?,
            "global-r1",
            false,
        )?;
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
        let _guard = hook_test_lock()?;
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let mut host = ApplicationHookHost::default();
        assert!(
            host.resolve_scope(directory.path(), &empty_global(), None)?
                .is_none()
        );
        assert!(host.owners.is_empty());
        Ok(())
    }
}
