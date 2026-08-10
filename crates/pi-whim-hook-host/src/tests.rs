use crate::{
    ApprovedHookManifest, DeliveryMode, EventRegistry, HookAuditEvent, HookAuditOutcome,
    HookDataClass, HookEventSpec, HookFieldSpec, HookGateDecision, HookHealthStatus, HookHostError,
    HookHostManager, HookInvocationContext, HookKind, HookKindSpec, HookManifest, HookPayload,
    HookRestartPolicy, HookScopeKey, HookTransformResult, HookWireMessage, ReentrancyGuard,
    ReentrancyKind,
};
use parking_lot::Mutex;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

type TestResult = Result<(), Box<dyn Error>>;

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new(label: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "pi-whim-hook-host-test-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(unix)]
    fn script(&self, name: &str, body: &str) -> std::io::Result<PathBuf> {
        let path = self.path.join(name);
        fs::write(&path, body)?;
        let mut permissions = fs::metadata(&path)?.permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions)?;
        Ok(path)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn manifest(value: Value) -> Result<HookManifest, HookHostError> {
    HookManifest::parse_json(&value.to_string())
}

fn app_scope(
    global: HookManifest,
) -> Result<(HookHostManager, crate::HookScopeHandle), HookHostError> {
    let revision = global.revision.clone();
    let manager = HookHostManager::new(global)?;
    let handle = manager.open_scope(HookScopeKey::app(revision)?, None)?;
    Ok((manager, handle))
}

fn app_context(handle: &crate::HookScopeHandle) -> HookInvocationContext {
    let key = handle.key();
    let context = HookInvocationContext::app(handle.scope_id(), key.manifest_revision);
    match handle.grants_hash() {
        Some(grants_hash) => context.with_grants_hash(grants_hash),
        None => context,
    }
}

fn payload(value: Value) -> Result<HookPayload, HookHostError> {
    HookPayload::from_value(value)
}

fn sha256(path: &Path) -> std::io::Result<String> {
    let bytes = fs::read(path)?;
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[test]
fn manifest_v1_and_v2_are_strict_and_v2_fields_are_exact() -> TestResult {
    let v1 = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "legacy",
            "event": "tool_dispatching",
            "kind": "gate",
            "command": ["/bin/true"],
            "timeout_ms": 250,
            "matcher": {"tools": ["shell"], "agent_levels": [1]}
        }]
    }))?;
    assert_eq!(v1.version, 1);
    assert_eq!(v1.hooks[0].event, "tool_dispatching");
    let registry = EventRegistry::default();
    assert_eq!(
        registry.canonical_event(&v1.hooks[0].event).as_deref(),
        Some("pi.tool.dispatching")
    );
    let v1_fields = registry
        .effective_fields(v1.version, &v1.hooks[0], true)?
        .into_iter()
        .map(|field| field.name)
        .collect::<Vec<_>>();
    assert!(v1_fields.contains(&"tool".to_owned()));
    assert!(v1_fields.contains(&"arguments".to_owned()));
    assert!(v1_fields.len() > 2);

    for invalid in [
        json!({"version": 1, "hooks": [], "unknown": true}),
        json!({
            "version": 1,
            "hooks": [{
                "id": "legacy", "event": "tool_dispatching", "command": ["/bin/true"],
                "unknown": true
            }]
        }),
        json!({
            "version": 1,
            "hooks": [{
                "id": "legacy", "event": "tool_dispatching", "command": ["/bin/true"],
                "matcher": {"source": "app"}
            }]
        }),
    ] {
        assert!(manifest(invalid).is_err());
    }

    let v2 = manifest(json!({
        "version": 2,
        "hooks": [{
            "id": "resident",
            "event": "pi.tool.dispatching",
            "kind": "gate",
            "command": ["/bin/true", "--flag"],
            "timeout_ms": 1234,
            "fields": ["tool", "arguments"],
            "matcher": {"tools": ["shell"], "source": "app"},
            "delivery": {"mode": "request_response", "capacity": 1},
            "restart": {
                "max_restarts": 2,
                "initial_backoff_ms": 250,
                "max_backoff_ms": 1000
            }
        }]
    }))?;
    let definition = &v2.hooks[0];
    assert_eq!(definition.fields, ["tool", "arguments"]);
    assert_eq!(definition.delivery.mode, DeliveryMode::RequestResponse);
    assert_eq!(definition.restart.max_restarts, 2);
    assert_eq!(definition.matcher.extra.get("source"), Some(&json!("app")));

    for invalid in [
        json!({
            "version": 2,
            "hooks": [{
                "id": "resident", "event": "pi.tool.dispatching", "command": ["/bin/true"],
                "fields": [], "unknown": true
            }]
        }),
        json!({
            "version": 2,
            "hooks": [{
                "id": "resident", "event": "pi.tool.dispatching", "command": ["/bin/true"],
                "fields": [], "delivery": {"mode": "request_response", "capacity": 1, "extra": 1}
            }]
        }),
        json!({
            "version": 2,
            "hooks": [{
                "id": "resident", "event": "pi.tool.dispatching", "command": ["/bin/true"],
                "fields": [], "matcher": {"not_registered": true}
            }]
        }),
        json!({
            "version": 2,
            "hooks": [{
                "id": "resident", "event": "pi.tool.dispatching", "command": ["/bin/true"],
                "fields": [],
                "restart": {"max_restarts": 3, "initial_backoff_ms": 249, "max_backoff_ms": 5000}
            }]
        }),
    ] {
        assert!(manifest(invalid).is_err());
    }
    Ok(())
}

#[test]
fn ui_v2_event_matrix_is_exact_and_has_no_legacy_aliases() -> TestResult {
    let registry = EventRegistry::default();
    let submitting = registry
        .spec("pi.ui.command.submitting")
        .expect("the UI submitting event is registered");
    assert!(submitting.project_visible);
    assert!(submitting.aliases.is_empty());
    assert_eq!(
        submitting.kinds.keys().copied().collect::<Vec<_>>(),
        vec![HookKind::Gate, HookKind::Transform]
    );
    assert_eq!(
        submitting
            .fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "arguments",
            "command_id",
            "command_name",
            "project_id",
            "source"
        ]
    );
    assert_eq!(
        submitting
            .fields
            .iter()
            .filter_map(|(name, field)| field.transformable.then_some(name.as_str()))
            .collect::<Vec<_>>(),
        vec!["arguments"]
    );
    assert_eq!(
        submitting
            .matcher_keys
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["command_name", "project_id", "source"]
    );
    assert_eq!(
        submitting.fields["command_id"].data_class,
        HookDataClass::PublicString
    );
    assert_eq!(
        submitting.fields["project_id"].data_class,
        HookDataClass::ProjectMetadata
    );
    assert_eq!(
        submitting.fields["arguments"].data_class,
        HookDataClass::UserContent
    );

    let lifecycle = registry
        .spec("pi.ui.command.lifecycle")
        .expect("the UI lifecycle event is registered");
    assert!(lifecycle.project_visible);
    assert!(lifecycle.aliases.is_empty());
    assert_eq!(
        lifecycle.kinds.keys().copied().collect::<Vec<_>>(),
        vec![HookKind::Observe]
    );
    assert_eq!(
        lifecycle
            .fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "command_id",
            "command_name",
            "diagnostic",
            "project_id",
            "source",
            "stage"
        ]
    );
    assert!(lifecycle.fields.values().all(|field| !field.transformable));
    assert_eq!(
        lifecycle
            .matcher_keys
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["command_name", "project_id", "source", "stage"]
    );

    let committed = registry
        .spec("pi.state.committed")
        .expect("the state commit event is registered");
    assert!(committed.project_visible);
    assert!(committed.aliases.is_empty());
    assert_eq!(
        committed.kinds.keys().copied().collect::<Vec<_>>(),
        vec![HookKind::Observe]
    );
    assert_eq!(
        committed
            .fields
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec![
            "action_count",
            "coalesced",
            "commit_source",
            "project_id",
            "revision",
            "scope",
            "topics"
        ]
    );
    assert!(committed.fields.values().all(|field| !field.transformable));
    assert_eq!(
        committed
            .matcher_keys
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        vec!["commit_source", "project_id", "scope"]
    );

    for event in [
        "ui_command_submitting",
        "ui_command_lifecycle",
        "state_committed",
        "pi.ui.render",
        "pi.ui.layout",
        "pi.ui.frame",
        "pi.render",
        "pi.layout",
        "pi.frame",
    ] {
        assert!(registry.spec(event).is_none(), "unexpected event {event}");
    }
    Ok(())
}

#[test]
fn ui_v2_manifests_accept_only_the_registered_kind_and_fields() -> TestResult {
    let valid = manifest(json!({
        "version": 2,
        "hooks": [
            {
                "id": "command-gate",
                "event": "pi.ui.command.submitting",
                "kind": "gate",
                "command": ["/bin/true"],
                "fields": ["command_id", "command_name", "source", "project_id", "arguments"],
                "matcher": {"command_name": "prompt.submit", "source": "ui", "project_id": "project-1"}
            },
            {
                "id": "command-transform",
                "event": "pi.ui.command.submitting",
                "kind": "transform",
                "command": ["/bin/true"],
                "fields": ["command_id", "command_name", "source", "project_id", "arguments"]
            },
            {
                "id": "command-lifecycle",
                "event": "pi.ui.command.lifecycle",
                "kind": "observe",
                "command": ["/bin/true"],
                "fields": ["command_id", "command_name", "source", "project_id", "stage", "diagnostic"],
                "matcher": {"command_name": "prompt.submit", "source": "ui", "project_id": "project-1", "stage": "submitted"}
            },
            {
                "id": "state-commit",
                "event": "pi.state.committed",
                "kind": "observe",
                "command": ["/bin/true"],
                "fields": ["revision", "topics", "action_count", "coalesced", "scope", "commit_source", "project_id"],
                "matcher": {"scope": "global", "commit_source": "user_command", "project_id": "project-1"}
            }
        ]
    }))?;
    assert_eq!(valid.hooks.len(), 4);

    for (event, kind) in [
        ("pi.ui.command.submitting", "observe"),
        ("pi.ui.command.lifecycle", "gate"),
        ("pi.ui.command.lifecycle", "transform"),
        ("pi.state.committed", "gate"),
        ("pi.state.committed", "transform"),
    ] {
        let invalid = manifest(json!({
            "version": 2,
            "hooks": [{
                "id": "wrong-kind",
                "event": event,
                "kind": kind,
                "command": ["/bin/true"],
                "fields": []
            }]
        }));
        assert!(matches!(invalid, Err(HookHostError::DisallowedKind { .. })));
    }

    for field in ["payload", "clipboard", "session_path"] {
        let invalid = manifest(json!({
            "version": 2,
            "hooks": [{
                "id": "unauthorized-field",
                "event": "pi.ui.command.submitting",
                "kind": "gate",
                "command": ["/bin/true"],
                "fields": [field]
            }]
        }));
        assert!(matches!(
            invalid,
            Err(HookHostError::UnauthorizedField { .. })
        ));
    }
    for field in ["api_key", "endpoint", "token", "secret"] {
        assert!(
            manifest(json!({
                "version": 2,
                "hooks": [{
                    "id": "forbidden-field",
                    "event": "pi.ui.command.submitting",
                    "kind": "gate",
                    "command": ["/bin/true"],
                    "fields": [field]
                }]
            }))
            .is_err()
        );
    }
    Ok(())
}

#[test]
fn ui_command_transform_can_change_only_arguments() -> TestResult {
    let registry = EventRegistry::default();
    let manifest = manifest(json!({
        "version": 2,
        "hooks": [{
            "id": "command-transform",
            "event": "pi.ui.command.submitting",
            "kind": "transform",
            "command": ["/bin/true"],
            "fields": ["command_id", "command_name", "source", "project_id", "arguments"]
        }]
    }))?;
    let definition = &manifest.hooks[0];
    let previous = registry.filter_payload(
        2,
        definition,
        true,
        &json!({
            "command_id": "command-1",
            "command_name": "prompt.submit",
            "source": "ui",
            "project_id": "project-1",
            "arguments": {"content": "before"},
            "payload": "must-not-export",
            "clipboard": "must-not-export",
            "api_key": "must-not-export"
        }),
    )?;
    assert_eq!(
        previous.as_value().as_object().map(|object| object.len()),
        Some(5)
    );
    assert!(previous.as_value().get("payload").is_none());
    assert!(previous.as_value().get("clipboard").is_none());
    assert!(previous.as_value().get("api_key").is_none());

    let transformed = registry.apply_transform(
        2,
        definition,
        true,
        &previous,
        &json!({"payload": {"arguments": {"content": "after"}}}),
    )?;
    assert_eq!(transformed.as_value()["command_id"], json!("command-1"));
    assert_eq!(
        transformed.as_value()["command_name"],
        json!("prompt.submit")
    );
    assert_eq!(transformed.as_value()["source"], json!("ui"));
    assert_eq!(transformed.as_value()["project_id"], json!("project-1"));
    assert_eq!(
        transformed.as_value()["arguments"],
        json!({"content": "after"})
    );

    for (field, value) in [
        ("command_id", json!("command-2")),
        ("command_name", json!("project.remove")),
        ("source", json!("hook_replay")),
        ("project_id", json!("project-2")),
    ] {
        let mut output = serde_json::Map::new();
        output.insert(field.to_owned(), value);
        let result = registry.apply_transform(
            2,
            definition,
            true,
            &previous,
            &json!({"payload": Value::Object(output)}),
        );
        assert!(matches!(result, Err(HookHostError::InvalidInvocation(_))));
    }
    for field in ["payload", "clipboard", "session_path"] {
        let mut output = serde_json::Map::new();
        output.insert(field.to_owned(), json!("forbidden"));
        let result = registry.apply_transform(
            2,
            definition,
            true,
            &previous,
            &json!({"payload": Value::Object(output)}),
        );
        assert!(matches!(
            result,
            Err(HookHostError::UnauthorizedField { .. })
        ));
    }
    let nested_secret = registry.apply_transform(
        2,
        definition,
        true,
        &previous,
        &json!({"payload": {"arguments": {"api_key": "never-export"}}}),
    );
    assert!(matches!(
        nested_secret,
        Err(HookHostError::ForbiddenField { .. })
    ));
    Ok(())
}

#[test]
fn legacy_agent_aliases_remain_valid_for_v1_manifests() -> TestResult {
    for (index, (event, kind)) in [
        ("supervisor_started", "observe"),
        ("supervisor_stopping", "observe"),
        ("session_published", "observe"),
        ("session_expired", "observe"),
        ("tool_completed", "observe"),
        ("tool_denied", "observe"),
        ("agent_started", "observe"),
        ("agent_finished", "observe"),
        ("message_delivered", "observe"),
        ("interaction_created", "observe"),
        ("interaction_resolved", "observe"),
        ("team_reset", "observe"),
        ("tool_dispatching", "gate"),
        ("agent_spawning", "gate"),
        ("message_sending", "gate"),
        ("permission_resolving", "gate"),
        ("agent_launching", "gate"),
        ("interaction_resolving", "transform"),
    ]
    .into_iter()
    .enumerate()
    {
        let parsed = manifest(json!({
            "version": 1,
            "hooks": [{
                "id": format!("legacy-{index}"),
                "event": event,
                "kind": kind,
                "command": ["/bin/true"]
            }]
        }))?;
        assert_eq!(parsed.hooks[0].event, event);
    }
    Ok(())
}

#[test]
fn registry_rejects_forbidden_nested_and_project_hidden_fields() -> TestResult {
    assert!(matches!(
        HookFieldSpec::new("endpoint_url", HookDataClass::PublicString, false, true),
        Err(HookHostError::ForbiddenField { .. })
    ));

    let forbidden_spec = HookEventSpec::new("test.forbidden")
        .with_kind(HookKind::Gate, HookKindSpec::new(HookKind::Gate))
        .with_field(HookFieldSpec {
            name: "apparently_safe".to_owned(),
            data_class: HookDataClass::Secret,
            transformable: false,
            project_visible: true,
        });
    assert!(matches!(
        EventRegistry::new(vec![forbidden_spec]),
        Err(HookHostError::ForbiddenField { .. })
    ));

    let nested_manifest = manifest(json!({
        "version": 2,
        "hooks": [{
            "id": "nested",
            "event": "pi.tool.dispatching",
            "kind": "gate",
            "command": ["/bin/true"],
            "fields": ["arguments"]
        }]
    }))?;
    let nested = EventRegistry::default().filter_payload(
        2,
        &nested_manifest.hooks[0],
        false,
        &json!({"arguments": {"safe": true, "api_key": "never-export"}}),
    );
    assert!(matches!(nested, Err(HookHostError::ForbiddenField { .. })));

    let project_spec = HookEventSpec::new("test.project")
        .with_kind(HookKind::Gate, HookKindSpec::new(HookKind::Gate))
        .with_field(HookFieldSpec::new(
            "host_only",
            HookDataClass::PublicString,
            false,
            false,
        )?);
    let registry = EventRegistry::new(vec![project_spec])?;
    let project_manifest: HookManifest = serde_json::from_value(json!({
        "version": 2,
        "hooks": [{
            "id": "project", "event": "test.project", "kind": "gate",
            "command": ["/bin/true"], "fields": ["host_only"]
        }]
    }))?;
    registry.validate_manifest(&project_manifest)?;
    let filtered = registry.filter_payload(
        2,
        &project_manifest.hooks[0],
        true,
        &json!({"host_only": "private"}),
    );
    assert!(matches!(
        filtered,
        Err(HookHostError::UnauthorizedField { .. })
    ));
    Ok(())
}

#[test]
fn failed_gate_is_closed_and_failed_transform_preserves_payload() -> TestResult {
    let gate_manifest = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "missing-gate", "event": "tool_dispatching", "kind": "gate",
            "command": ["/definitely/not/a/pi-whim-hook"]
        }]
    }))?
    .with_revision("gate-r1");
    let (_manager, gate_scope) = app_scope(gate_manifest)?;
    let gate = gate_scope.gate(
        "pi.tool.dispatching",
        app_context(&gate_scope),
        payload(json!({"tool": "shell"}))?,
    )?;
    assert!(matches!(
        gate,
        HookGateDecision::FailedClosed {
            error: HookHostError::Process(_),
            ..
        }
    ));

    let transform_manifest = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "missing-transform", "event": "message_sending", "kind": "transform",
            "command": ["/definitely/not/a/pi-whim-hook"]
        }]
    }))?
    .with_revision("transform-r1");
    let (_manager, transform_scope) = app_scope(transform_manifest)?;
    let original = payload(json!({"message": "unchanged"}))?;
    let transformed = transform_scope.transform(
        "pi.message.sending",
        app_context(&transform_scope),
        original.clone(),
    )?;
    match transformed {
        HookTransformResult::Preserved { payload, .. } => assert_eq!(payload, original),
        HookTransformResult::Transformed(_) => panic!("failed transform changed the payload"),
    }
    Ok(())
}

#[test]
fn scope_reuse_reentrancy_and_project_root_authentication_are_enforced() -> TestResult {
    let directory = TestDirectory::new("scope")?;
    let manager = HookHostManager::empty()?;
    let key = HookScopeKey::project(directory.path(), "project-r1")?;
    let approved =
        ApprovedHookManifest::new(HookManifest::default(), "project-r1", BTreeMap::new())?;
    let first = manager.open_scope(key.clone(), Some(approved.clone()))?;
    let second = manager.open_scope(key.clone(), Some(approved))?;
    assert_eq!(first.scope_id(), second.scope_id());

    let wrong_context = HookInvocationContext::project(
        first.scope_id(),
        key.manifest_revision.clone(),
        "/not/the/canonical/project",
    );
    assert!(matches!(
        first.gate(
            "pi.tool.dispatching",
            wrong_context,
            payload(json!({"tool": "shell"}))?
        ),
        Err(HookHostError::UnauthenticatedContext)
    ));

    let guard = ReentrancyGuard::enter(ReentrancyKind::Invocation, "pi.tool.dispatching")?;
    assert!(matches!(
        ReentrancyGuard::enter(ReentrancyKind::Invocation, "pi.tool.dispatching"),
        Err(HookHostError::ReentrantInvocation)
    ));
    let host_guard = ReentrancyGuard::enter(ReentrancyKind::HostEvent, "pi.tool.dispatching")?;
    drop(host_guard);
    drop(guard);
    ReentrancyGuard::enter(ReentrancyKind::Invocation, "pi.tool.dispatching")?;

    first.revoke();
    assert!(second.is_revoked());
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn revoked_scope_reopens_as_fresh_active_state_before_old_handle_drops() -> TestResult {
    let directory = TestDirectory::new("revoked-reopen")?;
    let script = directory.script(
        "allow.sh",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\n' '{\"decision\":\"allow\"}'\n",
    )?;
    let project = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "project",
            "event": "tool_dispatching",
            "kind": "gate",
            "command": [script]
        }]
    }))?;
    let mut fingerprints = BTreeMap::new();
    fingerprints.insert("project".to_owned(), sha256(&script)?);
    let approved = ApprovedHookManifest::new(project, "project-r1", fingerprints)?
        .with_grants_hash("c".repeat(64))?;
    let manager = HookHostManager::empty()?;
    let key = HookScopeKey::project(directory.path(), "project-r1")?;
    let first = manager.open_scope(key.clone(), Some(approved.clone()))?;

    first.revoke();
    assert!(first.is_revoked());
    assert_eq!(first.health()[0].status, HookHealthStatus::Stopped);

    let replacement = manager.open_scope(key.clone(), Some(approved))?;
    assert!(first.is_revoked());
    assert!(!replacement.is_revoked());
    assert_eq!(replacement.health()[0].status, HookHealthStatus::Ready);

    let project_root = key
        .project_root
        .as_ref()
        .ok_or("project key lost its root")?
        .to_string_lossy()
        .into_owned();
    let grants_hash = replacement
        .grants_hash()
        .ok_or("replacement scope lost grants hash")?;
    let decision = replacement.gate(
        "pi.tool.dispatching",
        HookInvocationContext::project(replacement.scope_id(), key.manifest_revision, project_root)
            .with_grants_hash(grants_hash),
        payload(json!({"tool": "shell"}))?,
    )?;
    assert!(matches!(decision, HookGateDecision::Allow));
    Ok(())
}

#[test]
fn protocol_rejects_unknown_response_fields() {
    let parsed = HookWireMessage::parse_line(
        br#"{"type":"response","request_id":"r","hook_id":"h","event":"pi.tool.dispatching","response":{"kind":"gate","decision":"allow"},"extra":true}"#,
    );
    assert!(matches!(parsed, Err(HookHostError::Json(_))));
}

#[test]
fn audit_wire_shape_contains_metadata_only() -> TestResult {
    let event = HookAuditEvent {
        hook_id: "hook".to_owned(),
        scope_id: "scope".to_owned(),
        event: "pi.tool.dispatching".to_owned(),
        kind: "gate".to_owned(),
        outcome: HookAuditOutcome::Allowed,
        duration_ms: 1,
        revision: "r1".to_owned(),
        dropped: false,
        restart_count: 0,
        drop_count: 0,
        grants_hash: Some("digest".to_owned()),
    };
    let serialized = serde_json::to_string(&event)?;
    assert!(!serialized.contains("payload"));
    assert!(!serialized.contains("output"));
    assert!(!serialized.contains("credential"));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn global_hooks_run_before_project_hooks_and_scope_is_shared() -> TestResult {
    let directory = TestDirectory::new("ordered")?;
    let global_script = directory.script(
        "global.sh",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"decision\":\"allow\"}'\n",
    )?;
    let project_script = directory.script(
        "project.sh",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"decision\":\"deny\",\"message\":\"project denied\"}'\n",
    )?;
    let global = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "global", "event": "tool_dispatching", "kind": "gate",
            "command": [global_script]
        }]
    }))?;
    let manager = HookHostManager::new_with_registry(
        EventRegistry::default(),
        ApprovedHookManifest::new(global, "global-r1", BTreeMap::new())?
            .with_grants_hash("a".repeat(64))?,
    )?;
    let project = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "project", "event": "tool_dispatching", "kind": "gate",
            "command": [project_script]
        }]
    }))?;
    let mut fingerprints = BTreeMap::new();
    fingerprints.insert("project".to_owned(), sha256(&project_script)?);
    let approved = ApprovedHookManifest::new(project, "project-r1", fingerprints)?
        .with_grants_hash("b".repeat(64))?;
    let key = HookScopeKey::project(directory.path(), "project-r1")?;
    let first = manager.open_scope(key.clone(), Some(approved.clone()))?;
    let second = manager.open_scope(key.clone(), Some(approved))?;
    assert_eq!(first.scope_id(), second.scope_id());

    let audits = Arc::new(Mutex::new(Vec::new()));
    let audit_sink = audits.clone();
    let _subscription = manager
        .audit_signal()
        .subscribe_fn(move |event| audit_sink.lock().push(event));
    let project_root = key
        .project_root
        .as_ref()
        .ok_or("project key lost its root")?
        .to_string_lossy()
        .into_owned();
    let grants_hash = first.grants_hash().ok_or("scope lost exact grants hash")?;
    assert_eq!(second.grants_hash().as_deref(), Some(grants_hash.as_str()));
    let decision = first.gate(
        "pi.tool.dispatching",
        HookInvocationContext::project(first.scope_id(), "project-r1", project_root)
            .with_grants_hash(grants_hash.clone()),
        payload(json!({"tool": "shell"}))?,
    )?;
    assert!(matches!(
        decision,
        HookGateDecision::Deny { ref hook_id, .. } if hook_id == "project"
    ));
    let hook_ids = audits
        .lock()
        .iter()
        .map(|event| event.hook_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(hook_ids, ["global", "project"]);
    assert!(
        audits
            .lock()
            .iter()
            .all(|event| event.grants_hash.as_deref() == Some(grants_hash.as_str()))
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn invalid_gate_response_is_failed_closed() -> TestResult {
    let directory = TestDirectory::new("invalid-gate")?;
    let script = directory.script(
        "invalid.sh",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s\\n' '{\"unexpected\":true}'\n",
    )?;
    let global = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "invalid", "event": "tool_dispatching", "kind": "gate",
            "command": [script]
        }]
    }))?
    .with_revision("invalid-r1");
    let (_manager, scope) = app_scope(global)?;
    let decision = scope.gate(
        "pi.tool.dispatching",
        app_context(&scope),
        payload(json!({"tool": "shell"}))?,
    )?;
    assert!(matches!(
        decision,
        HookGateDecision::FailedClosed {
            error: HookHostError::InvalidInvocation(_),
            ..
        }
    ));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn v1_observe_emits_one_completion_audit() -> TestResult {
    let directory = TestDirectory::new("observe-audit")?;
    let script = directory.script("observe.sh", "#!/bin/sh\ncat >/dev/null\n")?;
    let global = manifest(json!({
        "version": 1,
        "hooks": [{
            "id": "observe", "event": "tool_completed", "kind": "observe",
            "command": [script]
        }]
    }))?
    .with_revision("observe-r1");
    let (manager, scope) = app_scope(global)?;
    let audits = Arc::new(Mutex::new(Vec::new()));
    let audit_sink = audits.clone();
    let _subscription = manager
        .audit_signal()
        .subscribe_fn(move |event| audit_sink.lock().push(event));
    let receipt = scope.observe(
        "pi.tool.completed",
        app_context(&scope),
        payload(json!({"tool": "shell", "status": "ok"}))?,
    )?;
    assert_eq!(receipt.accepted, 1);
    let deadline = Instant::now() + Duration::from_secs(7);
    while audits.lock().is_empty() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(audits.lock().len(), 1);
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn v2_requires_exact_hello_id_and_request_id() -> TestResult {
    let directory = TestDirectory::new("v2-identity")?;
    let wrong_hello = directory.script(
        "wrong-hello.sh",
        r#"#!/bin/sh
IFS= read -r hello || exit 2
printf '%s\n' '{"type":"ready","hook_id":"wrong-hello","event":"pi.tool.dispatching","kind":"gate","hello_id":"wrong"}'
"#,
    )?;
    let wrong_hello_manifest = manifest(json!({
        "version": 2,
        "hooks": [{
            "id": "wrong-hello", "event": "pi.tool.dispatching", "kind": "gate",
            "command": [wrong_hello], "fields": ["tool"],
            "restart": {"max_restarts": 0, "initial_backoff_ms": 250, "max_backoff_ms": 5000}
        }]
    }))?
    .with_revision("wrong-hello-r1");
    let (_manager, wrong_hello_scope) = app_scope(wrong_hello_manifest)?;
    let health = wrong_hello_scope.health();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].status, HookHealthStatus::Unhealthy);
    assert!(
        health[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("hello ready identity mismatch"))
    );

    let wrong_request = directory.script(
        "wrong-request.sh",
        r#"#!/bin/sh
IFS= read -r hello || exit 2
hello_id=$(printf '%s\n' "$hello" | sed -n 's/.*"hello_id":"\([^"]*\)".*/\1/p')
printf '{"type":"ready","hook_id":"wrong-request","event":"pi.tool.dispatching","kind":"gate","hello_id":"%s"}\n' "$hello_id"
IFS= read -r request || exit 3
printf '%s\n' '{"type":"response","request_id":"wrong","hook_id":"wrong-request","event":"pi.tool.dispatching","response":{"kind":"gate","decision":"allow"}}'
"#,
    )?;
    let wrong_request_manifest = manifest(json!({
        "version": 2,
        "hooks": [{
            "id": "wrong-request", "event": "pi.tool.dispatching", "kind": "gate",
            "command": [wrong_request], "fields": ["tool"],
            "restart": {"max_restarts": 0, "initial_backoff_ms": 250, "max_backoff_ms": 5000}
        }]
    }))?
    .with_revision("wrong-request-r1");
    let (_manager, wrong_request_scope) = app_scope(wrong_request_manifest)?;
    let decision = wrong_request_scope.gate(
        "pi.tool.dispatching",
        app_context(&wrong_request_scope),
        payload(json!({"tool": "shell"}))?,
    )?;
    assert!(matches!(
        decision,
        HookGateDecision::FailedClosed {
            error: HookHostError::UnexpectedResponse { ref reason, .. },
            ..
        } if reason == "request_id mismatch"
    ));
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
fn persistent_restart_budget_is_bounded() -> TestResult {
    let directory = TestDirectory::new("restart")?;
    let script = directory.script(
        "crash.sh",
        r#"#!/bin/sh
IFS= read -r hello || exit 2
hello_id=$(printf '%s\n' "$hello" | sed -n 's/.*"hello_id":"\([^"]*\)".*/\1/p')
printf '{"type":"ready","hook_id":"crash","event":"pi.tool.dispatching","kind":"gate","hello_id":"%s"}\n' "$hello_id"
IFS= read -r request || exit 3
exit 7
"#,
    )?;
    let global = manifest(json!({
        "version": 2,
        "hooks": [{
            "id": "crash", "event": "pi.tool.dispatching", "kind": "gate",
            "command": [script], "fields": ["tool"],
            "restart": {"max_restarts": 3, "initial_backoff_ms": 250, "max_backoff_ms": 1000}
        }]
    }))?
    .with_revision("restart-r1");
    let (_manager, scope) = app_scope(global)?;
    for _ in 0..4 {
        let decision = scope.gate(
            "pi.tool.dispatching",
            app_context(&scope),
            payload(json!({"tool": "shell"}))?,
        )?;
        assert!(matches!(decision, HookGateDecision::FailedClosed { .. }));
    }
    let health = scope.health();
    assert_eq!(health.len(), 1);
    assert_eq!(health[0].restart_count, 3);
    assert_eq!(health[0].status, HookHealthStatus::Unhealthy);

    let policy = HookRestartPolicy {
        max_restarts: 3,
        initial_backoff_ms: 250,
        max_backoff_ms: 1000,
    };
    assert_eq!(policy.delay_for(0), Duration::from_millis(250));
    assert_eq!(policy.delay_for(1), Duration::from_millis(500));
    assert_eq!(policy.delay_for(2), Duration::from_millis(1000));
    assert_eq!(policy.delay_for(3), Duration::from_millis(1000));
    Ok(())
}
