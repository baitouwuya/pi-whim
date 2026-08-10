//! Typed supervisor Hook pipeline backed exclusively by `pi-whim-hook-host`.

use std::{
    any::Any,
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Instant,
};

use pi_whim_core::{HookAuditOutcome as CoreAuditOutcome, HookAuditRecord, HookConfig, HookEvent};
use pi_whim_hook_host::{
    ApprovedHookManifest, EventRegistry, HookAuditEvent, HookAuditOutcome as HostAuditOutcome,
    HookDataClass, HookEventSpec, HookFieldSpec, HookGateDecision, HookHostManager,
    HookInvocationContext, HookKind as HostHookKind, HookKindSpec, HookManifest, HookPayload,
    HookScopeHandle, HookScopeKey, HookTransformResult,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::model::AgentDescriptor;

const BUILTIN_REVISION: &str = "builtin";
const LEGACY_EMPTY_REVISION: &str = "legacy-v1";

struct AuditSubscription {
    _subscription: Box<dyn Any + Send + Sync>,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct AgentEventContext {
    agent_id: String,
    agent_level: u8,
    team_id: String,
    session_id: String,
    parent_session_id: Option<String>,
    parent_agent_id: Option<String>,
    request_id: Option<String>,
    agent_name: String,
    agent_role: String,
}

impl AgentEventContext {
    pub(crate) fn new(descriptor: &AgentDescriptor, request_id: Option<&str>) -> Self {
        Self {
            agent_id: descriptor.id.to_string(),
            agent_level: descriptor.level,
            team_id: descriptor.team_id.to_string(),
            session_id: descriptor.session_id.to_string(),
            parent_session_id: descriptor.parent_session_id.map(|id| id.to_string()),
            parent_agent_id: descriptor.parent_id.map(|id| id.to_string()),
            request_id: request_id.map(str::to_owned),
            agent_name: descriptor.name.clone(),
            agent_role: descriptor.role.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ToolDispatchInput {
    pub(crate) actor: AgentEventContext,
    pub(crate) tool: String,
    pub(crate) arguments: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct AgentLaunchInput {
    pub(crate) actor: AgentEventContext,
    pub(crate) fields: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct PermissionResolutionInput {
    pub(crate) actor: AgentEventContext,
    pub(crate) fields: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct InteractionResolutionInput {
    pub(crate) actor: AgentEventContext,
    pub(crate) fields: Value,
}

#[derive(Clone, Debug)]
pub(crate) struct HookObservation {
    event: HookEvent,
    actor: AgentEventContext,
    fields: Value,
}

impl HookObservation {
    pub(crate) fn new(event: HookEvent, actor: AgentEventContext, fields: Value) -> Self {
        Self {
            event,
            actor,
            fields,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HookControlError {
    Denied(String),
    InvalidPayload(String),
}

impl HookControlError {
    pub(crate) fn message(self) -> String {
        match self {
            Self::Denied(message) | Self::InvalidPayload(message) => message,
        }
    }
}

#[derive(Clone)]
pub(crate) struct SupervisorHooks {
    scope: Option<HookScopeHandle>,
    context: Option<HookInvocationContext>,
    unavailable: Option<String>,
    audit_sender: mpsc::SyncSender<HookAuditRecord>,
    _audit_subscription: Option<Arc<AuditSubscription>>,
    observers_stopped: Arc<AtomicBool>,
}

impl std::fmt::Debug for SupervisorHooks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SupervisorHooks")
            .field(
                "scope_id",
                &self.scope.as_ref().map(HookScopeHandle::scope_id),
            )
            .field("unavailable", &self.unavailable)
            .field(
                "observers_stopped",
                &self.observers_stopped.load(Ordering::Acquire),
            )
            .finish()
    }
}

impl SupervisorHooks {
    pub(crate) fn from_v1_config(
        config: HookConfig,
        project_root: PathBuf,
        audit_sender: mpsc::SyncSender<HookAuditRecord>,
    ) -> Self {
        if config.hooks.is_empty() {
            return Self::inactive(audit_sender);
        }
        match build_v1_scope(&config, &project_root, &audit_sender) {
            Ok((scope, context, subscription)) => {
                Self::active(scope, context, audit_sender, Some(subscription))
            }
            Err(error) => Self::unavailable(audit_sender, error),
        }
    }

    pub(crate) fn from_scope(
        scope: HookScopeHandle,
        project_root: &Path,
        audit_sender: mpsc::SyncSender<HookAuditRecord>,
    ) -> Result<Self, String> {
        if scope.is_revoked() {
            return Err("hook scope has been revoked".to_owned());
        }
        let canonical_root =
            std::fs::canonicalize(project_root).map_err(|error| error.to_string())?;
        let key = scope.key();
        if key.project_root.as_ref() != Some(&canonical_root) {
            return Err("hook scope project root does not match the supervisor project".to_owned());
        }
        let context = HookInvocationContext::project(
            scope.scope_id(),
            key.manifest_revision.clone(),
            canonical_root.to_string_lossy().into_owned(),
        );
        let context = match scope.grants_hash() {
            Some(grants_hash) => context.with_grants_hash(grants_hash),
            None => context,
        };
        Ok(Self::active(scope, context, audit_sender, None))
    }

    fn inactive(audit_sender: mpsc::SyncSender<HookAuditRecord>) -> Self {
        Self {
            scope: None,
            context: None,
            unavailable: None,
            audit_sender,
            _audit_subscription: None,
            observers_stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn unavailable(audit_sender: mpsc::SyncSender<HookAuditRecord>, error: String) -> Self {
        Self {
            scope: None,
            context: None,
            unavailable: Some(error),
            audit_sender,
            _audit_subscription: None,
            observers_stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    fn active(
        scope: HookScopeHandle,
        context: HookInvocationContext,
        audit_sender: mpsc::SyncSender<HookAuditRecord>,
        subscription: Option<AuditSubscription>,
    ) -> Self {
        Self {
            scope: Some(scope),
            context: Some(context),
            unavailable: None,
            audit_sender,
            _audit_subscription: subscription.map(Arc::new),
            observers_stopped: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn control_tool_dispatch<F>(
        &self,
        input: ToolDispatchInput,
        validate: F,
    ) -> Result<Value, HookControlError>
    where
        F: Fn(&Value) -> Result<(), String>,
    {
        self.control_arguments(HookEvent::ToolDispatching, input, validate)
    }

    pub(crate) fn control_agent_spawn<F>(
        &self,
        input: ToolDispatchInput,
        validate: F,
    ) -> Result<Value, HookControlError>
    where
        F: Fn(&Value) -> Result<(), String>,
    {
        self.control_arguments(HookEvent::AgentSpawning, input, validate)
    }

    pub(crate) fn control_message_send<F>(
        &self,
        input: ToolDispatchInput,
        validate: F,
    ) -> Result<Value, HookControlError>
    where
        F: Fn(&Value) -> Result<(), String>,
    {
        self.control_arguments(HookEvent::MessageSending, input, validate)
    }

    fn control_arguments<F>(
        &self,
        event: HookEvent,
        input: ToolDispatchInput,
        validate: F,
    ) -> Result<Value, HookControlError>
    where
        F: Fn(&Value) -> Result<(), String>,
    {
        validate(&input.arguments).map_err(HookControlError::InvalidPayload)?;
        let original = control_payload(&input.actor, &input.tool, &input.arguments)
            .map_err(HookControlError::InvalidPayload)?;
        self.safety_floor("builtin.safety_floor", event, &original)
            .map_err(HookControlError::Denied)?;

        let public_original = redact_private(&original);
        let transformed = self.transform(event, public_original.clone());
        let candidate = if transformed == public_original {
            original.clone()
        } else if validate_transformed_payload(event, &public_original, &transformed).is_ok() {
            restore_private(&original, &transformed)
        } else {
            self.audit_builtin(
                "builtin.transform_validation",
                event,
                CoreAuditOutcome::Failed,
                Instant::now(),
            );
            original.clone()
        };
        let arguments = candidate.get("arguments").cloned().ok_or_else(|| {
            HookControlError::InvalidPayload("hook payload lost arguments".to_owned())
        })?;
        let final_payload = if validate(&arguments).is_ok()
            && self
                .safety_floor("builtin.safety_floor.final", event, &candidate)
                .is_ok()
        {
            candidate
        } else {
            self.audit_builtin(
                "builtin.transform_validation",
                event,
                CoreAuditOutcome::Failed,
                Instant::now(),
            );
            original
        };
        self.run_gate(event, &redact_private(&final_payload))
            .map_err(HookControlError::Denied)?;
        final_payload.get("arguments").cloned().ok_or_else(|| {
            HookControlError::InvalidPayload("hook payload lost arguments".to_owned())
        })
    }

    pub(crate) fn gate_agent_launch(
        &self,
        input: AgentLaunchInput,
    ) -> Result<(), HookControlError> {
        self.gate_only(
            HookEvent::AgentLaunching,
            envelope(&input.actor, input.fields)?,
        )
    }

    pub(crate) fn gate_permission_resolution(
        &self,
        input: PermissionResolutionInput,
    ) -> Result<(), HookControlError> {
        self.gate_only(
            HookEvent::PermissionResolving,
            envelope(&input.actor, input.fields)?,
        )
    }

    fn gate_only(&self, event: HookEvent, payload: Value) -> Result<(), HookControlError> {
        self.safety_floor("builtin.safety_floor", event, &payload)
            .map_err(HookControlError::Denied)?;
        self.run_gate(event, &redact_private(&payload))
            .map_err(HookControlError::Denied)
    }

    pub(crate) fn transform_interaction_resolution(
        &self,
        input: InteractionResolutionInput,
    ) -> Option<String> {
        let original = envelope(&input.actor, input.fields).ok()?;
        if self
            .safety_floor(
                "builtin.safety_floor",
                HookEvent::InteractionResolving,
                &original,
            )
            .is_err()
        {
            return None;
        }
        let transformed =
            self.transform(HookEvent::InteractionResolving, redact_private(&original));
        if validate_transformed_payload(
            HookEvent::InteractionResolving,
            &redact_private(&original),
            &transformed,
        )
        .is_err()
            || self
                .safety_floor(
                    "builtin.safety_floor.final",
                    HookEvent::InteractionResolving,
                    &transformed,
                )
                .is_err()
        {
            return None;
        }
        transformed
            .get("arguments")?
            .get("decision")?
            .as_str()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    pub(crate) fn observe(&self, observation: HookObservation) {
        if !self.observers_stopped.load(Ordering::Acquire) {
            self.observe_inner(observation);
        }
    }

    pub(crate) fn finalize(&self, observation: HookObservation) {
        self.observe_inner(observation);
    }

    pub(crate) fn stop_observers(&self) {
        self.observers_stopped.store(true, Ordering::Release);
    }

    fn observe_inner(&self, observation: HookObservation) {
        let Ok(payload) = envelope(&observation.actor, observation.fields) else {
            return;
        };
        let Ok((scope, context)) = self.active_scope() else {
            return;
        };
        let Ok(payload) = HookPayload::from_value(redact_private(&payload)) else {
            return;
        };
        let _ = scope.observe(event_name(observation.event), context.clone(), payload);
    }

    fn run_gate(&self, event: HookEvent, payload: &Value) -> Result<(), String> {
        if self.scope.is_none() && self.unavailable.is_none() {
            return Ok(());
        }
        let (scope, context) = self.active_scope()?;
        let payload = HookPayload::from_value(payload.clone())
            .map_err(|error| format!("hook payload rejected: {error}"))?;
        match scope.gate(event_name(event), context.clone(), payload) {
            Ok(HookGateDecision::Allow) => Ok(()),
            Ok(HookGateDecision::Deny { hook_id, message }) => {
                Err(format!("hook {hook_id} denied: {message}"))
            }
            Ok(HookGateDecision::FailedClosed { hook_id, error }) => {
                Err(format!("hook {hook_id} failed: {error}"))
            }
            Err(error) => Err(format!("hook gate failed closed: {error}")),
        }
    }

    fn transform(&self, event: HookEvent, payload: Value) -> Value {
        if self.scope.is_none() && self.unavailable.is_none() {
            return payload;
        }
        let Ok((scope, context)) = self.active_scope() else {
            return payload;
        };
        let Ok(host_payload) = HookPayload::from_value(payload.clone()) else {
            return payload;
        };
        match scope.transform(event_name(event), context.clone(), host_payload) {
            Ok(HookTransformResult::Transformed(payload))
            | Ok(HookTransformResult::Preserved { payload, .. }) => payload.into_value(),
            Err(_) => payload,
        }
    }

    fn active_scope(&self) -> Result<(&HookScopeHandle, &HookInvocationContext), String> {
        match (&self.scope, &self.context) {
            (Some(scope), Some(context)) if !scope.is_revoked() => Ok((scope, context)),
            _ => Err(self
                .unavailable
                .clone()
                .unwrap_or_else(|| "hook scope is unavailable".to_owned())),
        }
    }

    fn safety_floor(
        &self,
        hook_id: &'static str,
        event: HookEvent,
        payload: &Value,
    ) -> Result<(), String> {
        let started = Instant::now();
        let result = safety_floor(event, payload);
        self.audit_builtin(
            hook_id,
            event,
            if result.is_ok() {
                CoreAuditOutcome::Allowed
            } else {
                CoreAuditOutcome::Denied
            },
            started,
        );
        result
    }

    fn audit_builtin(
        &self,
        hook_id: &str,
        event: HookEvent,
        outcome: CoreAuditOutcome,
        started: Instant,
    ) {
        let _ = self.audit_sender.try_send(HookAuditRecord {
            hook_id: hook_id.to_owned(),
            event,
            outcome,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            output_truncated: false,
            revision: BUILTIN_REVISION.to_owned(),
            scope_id: None,
            kind: None,
            dropped: false,
            restart_count: 0,
            drop_count: 0,
            grants_hash: None,
        });
    }

    #[cfg(test)]
    fn scope_id(&self) -> Option<String> {
        self.scope.as_ref().map(HookScopeHandle::scope_id)
    }

    #[cfg(test)]
    fn adapts_external_audit(&self) -> bool {
        self._audit_subscription.is_some()
    }
}

fn control_payload(
    actor: &AgentEventContext,
    tool: &str,
    arguments: &Value,
) -> Result<Value, String> {
    let mut value = envelope(actor, json!({"tool": tool, "arguments": arguments}))
        .map_err(HookControlError::message)?;
    if !value.is_object() {
        return Err("hook payload must be an object".to_owned());
    }
    Ok(value.take())
}

fn envelope(actor: &AgentEventContext, fields: Value) -> Result<Value, HookControlError> {
    let mut object = fields.as_object().cloned().ok_or_else(|| {
        HookControlError::InvalidPayload("hook event fields must be an object".to_owned())
    })?;
    let context = serde_json::to_value(actor)
        .map_err(|error| HookControlError::InvalidPayload(error.to_string()))?;
    let context = context.as_object().ok_or_else(|| {
        HookControlError::InvalidPayload("agent hook context must be an object".to_owned())
    })?;
    for (key, value) in context {
        object.entry(key.clone()).or_insert_with(|| value.clone());
    }
    Ok(Value::Object(object))
}

fn redact_private(value: &Value) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(key, _)| !private_field_name(key))
                .map(|(key, value)| (key.clone(), redact_private(value)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.iter().map(redact_private).collect()),
        _ => value.clone(),
    }
}

fn restore_private(original: &Value, transformed: &Value) -> Value {
    match (original, transformed) {
        (Value::Object(original), Value::Object(transformed)) => {
            let mut restored = transformed.clone();
            for (key, value) in original {
                if private_field_name(key) {
                    restored.insert(key.clone(), value.clone());
                } else if let Some(candidate) = restored.get(key).cloned() {
                    restored.insert(key.clone(), restore_private(value, &candidate));
                }
            }
            Value::Object(restored)
        }
        _ => transformed.clone(),
    }
}

fn private_field_name(name: &str) -> bool {
    let normalized = name.to_ascii_lowercase().replace('-', "_");
    normalized.contains("capability")
        || normalized.contains("environment")
        || normalized == "env"
        || normalized.contains("api_key")
        || normalized.contains("provider_key")
        || normalized.contains("approval_ticket")
        || normalized.contains("endpoint")
        || normalized.contains("authorization")
        || normalized.contains("token")
        || normalized.contains("secret")
}

fn build_v1_scope(
    config: &HookConfig,
    project_root: &Path,
    audit_sender: &mpsc::SyncSender<HookAuditRecord>,
) -> Result<(HookScopeHandle, HookInvocationContext, AuditSubscription), String> {
    config.validate()?;
    let revision = effective_revision(config);
    let manifest_json = serde_json::to_string(config).map_err(|error| error.to_string())?;
    let fingerprints = config
        .hooks
        .iter()
        .filter_map(|hook| {
            hook.entrypoint_fingerprint
                .as_ref()
                .map(|fingerprint| (hook.id.clone(), fingerprint.clone()))
        })
        .collect::<BTreeMap<_, _>>();
    let manifest = HookManifest::parse_json(&manifest_json)
        .and_then(|manifest| {
            manifest
                .with_revision(revision.clone())
                .with_entrypoint_fingerprints(&fingerprints)
        })
        .map_err(|error| error.to_string())?;
    let approved = ApprovedHookManifest::new(manifest, revision.clone(), fingerprints)
        .map_err(|error| error.to_string())?;
    let manager = HookHostManager::new_with_registry(
        supervisor_registry().map_err(|error| error.to_string())?,
        approved,
    )
    .map_err(|error| error.to_string())?;
    let adapter_sender = audit_sender.clone();
    let subscription = manager
        .audit_signal()
        .subscribe_fn(move |event| adapt_host_audit(&adapter_sender, event));
    let key =
        HookScopeKey::project(project_root, revision.clone()).map_err(|error| error.to_string())?;
    let scope = manager
        .open_scope(key.clone(), None)
        .map_err(|error| error.to_string())?;
    let canonical_root = key
        .project_root
        .as_ref()
        .map(|root| root.to_string_lossy().into_owned())
        .ok_or_else(|| "project hook scope lost its canonical root".to_owned())?;
    let context = HookInvocationContext::project(scope.scope_id(), revision, canonical_root);
    Ok((
        scope,
        context,
        AuditSubscription {
            _subscription: Box::new(subscription),
        },
    ))
}

fn supervisor_registry() -> Result<EventRegistry, pi_whim_hook_host::HookHostError> {
    let controls = [
        HookEvent::ToolDispatching,
        HookEvent::AgentSpawning,
        HookEvent::MessageSending,
    ];
    let gate_only = [HookEvent::PermissionResolving, HookEvent::AgentLaunching];
    let transform_only = [HookEvent::InteractionResolving];
    let observes = [
        HookEvent::SupervisorStarted,
        HookEvent::SupervisorStopping,
        HookEvent::SessionPublished,
        HookEvent::SessionExpired,
        HookEvent::ToolCompleted,
        HookEvent::ToolDenied,
        HookEvent::AgentStarted,
        HookEvent::AgentFinished,
        HookEvent::MessageDelivered,
        HookEvent::InteractionCreated,
        HookEvent::InteractionResolved,
        HookEvent::TeamReset,
    ];
    let mut specs = Vec::new();
    for event in controls {
        specs.push(event_spec(
            event,
            &[HostHookKind::Gate, HostHookKind::Transform],
        )?);
    }
    for event in gate_only {
        specs.push(event_spec(event, &[HostHookKind::Gate])?);
    }
    for event in transform_only {
        specs.push(event_spec(event, &[HostHookKind::Transform])?);
    }
    for event in observes {
        specs.push(event_spec(event, &[HostHookKind::Observe])?);
    }
    EventRegistry::new(specs)
}

fn event_spec(
    event: HookEvent,
    kinds: &[HostHookKind],
) -> Result<HookEventSpec, pi_whim_hook_host::HookHostError> {
    let mut spec = HookEventSpec::new(event_name(event)).with_alias(core_event_alias(event));
    for kind in kinds {
        spec = spec.with_kind(*kind, HookKindSpec::new(*kind));
    }
    for key in [
        "tools",
        "agent_levels",
        "source",
        "agent_id",
        "project_id",
        "operation",
    ] {
        spec = spec.with_matcher_key(key);
    }
    for (name, class, transformable) in registry_fields() {
        spec = spec.with_field(HookFieldSpec::new(name, class, transformable, true)?);
    }
    Ok(spec)
}

fn registry_fields() -> Vec<(&'static str, HookDataClass, bool)> {
    vec![
        ("arguments", HookDataClass::UserContent, true),
        ("tool", HookDataClass::PublicString, false),
        ("agent_id", HookDataClass::PublicString, false),
        ("agent_level", HookDataClass::Number, false),
        ("team_id", HookDataClass::PublicString, false),
        ("session_id", HookDataClass::PublicString, false),
        ("parent_session_id", HookDataClass::UserContent, false),
        ("parent_agent_id", HookDataClass::UserContent, false),
        ("request_id", HookDataClass::UserContent, false),
        ("agent_name", HookDataClass::PublicString, false),
        ("agent_role", HookDataClass::PublicString, false),
        ("root_agent_id", HookDataClass::PublicString, false),
        ("sender_id", HookDataClass::PublicString, false),
        ("requester_id", HookDataClass::PublicString, false),
        ("owner_id", HookDataClass::PublicString, false),
        ("target", HookDataClass::PublicString, false),
        ("name", HookDataClass::PublicString, false),
        ("task", HookDataClass::UserContent, false),
        ("message", HookDataClass::UserContent, false),
        ("reason", HookDataClass::UserContent, false),
        ("status", HookDataClass::PublicString, false),
        ("success", HookDataClass::Boolean, false),
        ("interrupted", HookDataClass::Boolean, false),
        ("exit_code", HookDataClass::UserContent, false),
        ("duration_ms", HookDataClass::Number, false),
        ("decision", HookDataClass::UserContent, false),
        ("kind", HookDataClass::PublicString, false),
        ("title", HookDataClass::UserContent, false),
        ("options", HookDataClass::UserContent, false),
        ("default_option", HookDataClass::UserContent, false),
        ("operation_hash", HookDataClass::PublicString, false),
        ("delivery", HookDataClass::UserContent, false),
        ("response", HookDataClass::UserContent, false),
        ("effective_policy", HookDataClass::UserContent, false),
        ("delegated_models", HookDataClass::UserContent, false),
        ("spawn", HookDataClass::UserContent, false),
        ("source", HookDataClass::PublicString, false),
        ("operation", HookDataClass::PublicString, false),
    ]
}

fn effective_revision(config: &HookConfig) -> String {
    if config.revision.is_empty() {
        LEGACY_EMPTY_REVISION.to_owned()
    } else {
        config.revision.clone()
    }
}

fn adapt_host_audit(sender: &mpsc::SyncSender<HookAuditRecord>, event: HookAuditEvent) {
    let Some(core_event) = event_from_name(&event.event) else {
        return;
    };
    let outcome = match event.outcome {
        HostAuditOutcome::Allowed => CoreAuditOutcome::Allowed,
        HostAuditOutcome::Denied => CoreAuditOutcome::Denied,
        HostAuditOutcome::Transformed
        | HostAuditOutcome::Observed
        | HostAuditOutcome::Restarted => CoreAuditOutcome::Succeeded,
        HostAuditOutcome::TimedOut => CoreAuditOutcome::TimedOut,
        HostAuditOutcome::Preserved | HostAuditOutcome::Failed | HostAuditOutcome::Dropped => {
            CoreAuditOutcome::Failed
        }
    };
    let _ = sender.try_send(HookAuditRecord {
        hook_id: event.hook_id,
        event: core_event,
        outcome,
        duration_ms: event.duration_ms,
        output_truncated: false,
        revision: event.revision,
        scope_id: Some(event.scope_id),
        kind: Some(event.kind),
        dropped: event.dropped,
        restart_count: event.restart_count,
        drop_count: event.drop_count,
        grants_hash: event.grants_hash,
    });
}

fn safety_floor(event: HookEvent, payload: &Value) -> Result<(), String> {
    let arguments = || {
        payload
            .get("arguments")
            .and_then(Value::as_object)
            .ok_or_else(|| "hook event arguments must be an object".to_owned())
    };
    match event {
        HookEvent::MessageSending => {
            let message = arguments()?
                .get("message")
                .and_then(Value::as_str)
                .ok_or_else(|| "message must be a string".to_owned())?;
            if message.trim().is_empty() || message.len() > crate::MAX_MESSAGE_BYTES {
                return Err("message violates supervisor size constraints".to_owned());
            }
        }
        HookEvent::AgentSpawning => {
            let arguments = arguments()?;
            for field in ["name", "task"] {
                if arguments
                    .get(field)
                    .and_then(Value::as_str)
                    .is_none_or(|value| value.trim().is_empty())
                {
                    return Err(format!("agent {field} cannot be empty"));
                }
            }
        }
        HookEvent::ToolDispatching
        | HookEvent::PermissionResolving
        | HookEvent::AgentLaunching
        | HookEvent::InteractionResolving => {}
        _ => return Ok(()),
    }
    Ok(())
}

fn validate_transformed_payload(
    event: HookEvent,
    original: &Value,
    transformed: &Value,
) -> Result<(), ()> {
    let mut original_envelope = original.as_object().cloned().ok_or(())?;
    let mut transformed_envelope = transformed.as_object().cloned().ok_or(())?;
    let original_arguments = original_envelope.remove("arguments").ok_or(())?;
    let transformed_arguments = transformed_envelope.remove("arguments").ok_or(())?;
    if original_envelope != transformed_envelope {
        return Err(());
    }
    validate_transform(event, &original_arguments, &transformed_arguments)
}

fn validate_transform(event: HookEvent, original: &Value, transformed: &Value) -> Result<(), ()> {
    let original = original.as_object().ok_or(())?;
    let transformed = transformed.as_object().ok_or(())?;
    match event {
        HookEvent::ToolDispatching => Ok(()),
        HookEvent::MessageSending => {
            let mut original = original.clone();
            let mut transformed = transformed.clone();
            let message_valid = transformed_message_is_valid(transformed.get("message"));
            original.remove("message");
            transformed.remove("message");
            (original == transformed && message_valid)
                .then_some(())
                .ok_or(())
        }
        HookEvent::AgentSpawning => validate_spawn_transform(original, transformed),
        HookEvent::InteractionResolving => {
            let mut original = original.clone();
            let mut transformed = transformed.clone();
            let decision_valid = match transformed.get("decision") {
                None | Some(Value::Null) => true,
                Some(value) => value
                    .as_str()
                    .is_some_and(|decision| !decision.trim().is_empty() && decision.len() <= 4096),
            };
            original.remove("decision");
            transformed.remove("decision");
            (original == transformed && decision_valid)
                .then_some(())
                .ok_or(())
        }
        _ => Err(()),
    }
}

fn transformed_message_is_valid(message: Option<&Value>) -> bool {
    message.and_then(Value::as_str).is_some_and(|message| {
        !message.trim().is_empty() && message.len() <= crate::MAX_MESSAGE_BYTES
    })
}

fn validate_spawn_transform(
    original: &Map<String, Value>,
    transformed: &Map<String, Value>,
) -> Result<(), ()> {
    const POLICY_FIELDS: &[&str] = &[
        "permission_level",
        "enabled_tools",
        "trusted_extensions",
        "allowed_models",
    ];
    let mut original_identity = original.clone();
    let mut transformed_identity = transformed.clone();
    for field in POLICY_FIELDS {
        original_identity.remove(*field);
        transformed_identity.remove(*field);
    }
    if original_identity != transformed_identity {
        return Err(());
    }
    if original.get("permission_level") != transformed.get("permission_level") {
        let original = original
            .get("permission_level")
            .and_then(Value::as_str)
            .and_then(permission_rank)
            .unwrap_or(3);
        let requested = transformed
            .get("permission_level")
            .and_then(Value::as_str)
            .and_then(permission_rank)
            .unwrap_or(0);
        if requested > original {
            return Err(());
        }
    }
    for field in ["enabled_tools", "trusted_extensions"] {
        if original.get(field) != transformed.get(field) {
            let original = string_set(original.get(field)).ok_or(())?;
            let transformed = string_set(transformed.get(field)).ok_or(())?;
            if transformed.is_empty() || !transformed.is_subset(&original) {
                return Err(());
            }
        }
    }
    if original.get("allowed_models") != transformed.get("allowed_models") {
        let original = json_set(original.get("allowed_models")).ok_or(())?;
        let transformed = json_set(transformed.get("allowed_models")).ok_or(())?;
        if transformed.is_empty() || !transformed.is_subset(&original) {
            return Err(());
        }
    }
    Ok(())
}

fn permission_rank(value: &str) -> Option<u8> {
    match value {
        "read_only" => Some(1),
        "controlled" => Some(2),
        "full" => Some(3),
        _ => None,
    }
}
fn string_set(value: Option<&Value>) -> Option<std::collections::HashSet<&str>> {
    value?.as_array()?.iter().map(Value::as_str).collect()
}
fn json_set(value: Option<&Value>) -> Option<std::collections::HashSet<String>> {
    value?
        .as_array()?
        .iter()
        .map(|value| serde_json::to_string(value).ok())
        .collect()
}

fn event_name(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SupervisorStarted => "pi.supervisor.started",
        HookEvent::SupervisorStopping => "pi.supervisor.stopping",
        HookEvent::SessionPublished => "pi.session.published",
        HookEvent::SessionExpired => "pi.session.expired",
        HookEvent::ToolDispatching => "pi.tool.dispatching",
        HookEvent::AgentSpawning => "pi.agent.spawning",
        HookEvent::MessageSending => "pi.message.sending",
        HookEvent::PermissionResolving => "pi.permission.resolving",
        HookEvent::AgentLaunching => "pi.agent.launching",
        HookEvent::ToolCompleted => "pi.tool.completed",
        HookEvent::ToolDenied => "pi.tool.denied",
        HookEvent::AgentStarted => "pi.agent.started",
        HookEvent::AgentFinished => "pi.agent.finished",
        HookEvent::MessageDelivered => "pi.message.delivered",
        HookEvent::InteractionCreated => "pi.interaction.created",
        HookEvent::InteractionResolving => "pi.interaction.resolving",
        HookEvent::InteractionResolved => "pi.interaction.resolved",
        HookEvent::TeamReset => "pi.team.reset",
    }
}
fn core_event_alias(event: HookEvent) -> &'static str {
    match event {
        HookEvent::SupervisorStarted => "supervisor_started",
        HookEvent::SupervisorStopping => "supervisor_stopping",
        HookEvent::SessionPublished => "session_published",
        HookEvent::SessionExpired => "session_expired",
        HookEvent::ToolDispatching => "tool_dispatching",
        HookEvent::AgentSpawning => "agent_spawning",
        HookEvent::MessageSending => "message_sending",
        HookEvent::PermissionResolving => "permission_resolving",
        HookEvent::AgentLaunching => "agent_launching",
        HookEvent::ToolCompleted => "tool_completed",
        HookEvent::ToolDenied => "tool_denied",
        HookEvent::AgentStarted => "agent_started",
        HookEvent::AgentFinished => "agent_finished",
        HookEvent::MessageDelivered => "message_delivered",
        HookEvent::InteractionCreated => "interaction_created",
        HookEvent::InteractionResolving => "interaction_resolving",
        HookEvent::InteractionResolved => "interaction_resolved",
        HookEvent::TeamReset => "team_reset",
    }
}
fn event_from_name(event: &str) -> Option<HookEvent> {
    Some(match event {
        "pi.supervisor.started" => HookEvent::SupervisorStarted,
        "pi.supervisor.stopping" => HookEvent::SupervisorStopping,
        "pi.session.published" => HookEvent::SessionPublished,
        "pi.session.expired" => HookEvent::SessionExpired,
        "pi.tool.dispatching" => HookEvent::ToolDispatching,
        "pi.agent.spawning" => HookEvent::AgentSpawning,
        "pi.message.sending" => HookEvent::MessageSending,
        "pi.permission.resolving" => HookEvent::PermissionResolving,
        "pi.agent.launching" => HookEvent::AgentLaunching,
        "pi.tool.completed" => HookEvent::ToolCompleted,
        "pi.tool.denied" => HookEvent::ToolDenied,
        "pi.agent.started" => HookEvent::AgentStarted,
        "pi.agent.finished" => HookEvent::AgentFinished,
        "pi.message.delivered" => HookEvent::MessageDelivered,
        "pi.interaction.created" => HookEvent::InteractionCreated,
        "pi.interaction.resolving" => HookEvent::InteractionResolving,
        "pi.interaction.resolved" => HookEvent::InteractionResolved,
        "pi.team.reset" => HookEvent::TeamReset,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pi_whim_core::{
        AgentPermissionLevel, HookDefinition, HookKind as CoreHookKind, HookMatcher,
    };
    use pi_whim_hook_host::{ReentrancyGuard, ReentrancyKind};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{fs, os::unix::fs::PermissionsExt, time::Duration};
    use uuid::Uuid;

    fn sandbox_available() -> bool {
        Path::new("/usr/bin/sandbox-exec").is_file()
    }

    fn write_executable(path: &Path, content: &str) {
        fs::write(path, content).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).unwrap();
    }

    fn definition(
        id: &str,
        event: HookEvent,
        kind: CoreHookKind,
        command: Vec<String>,
    ) -> HookDefinition {
        HookDefinition {
            id: id.to_owned(),
            event,
            kind,
            command,
            timeout_ms: Some(1_000),
            matcher: HookMatcher::default(),
            entrypoint_fingerprint: None,
        }
    }

    fn config(hooks: Vec<HookDefinition>) -> HookConfig {
        HookConfig {
            version: 1,
            hooks,
            revision: "revision-test".to_owned(),
        }
    }

    fn test_pipeline(
        root: &Path,
        hooks: Vec<HookDefinition>,
    ) -> (SupervisorHooks, mpsc::Receiver<HookAuditRecord>) {
        let (sender, receiver) = mpsc::sync_channel(128);
        (
            SupervisorHooks::from_v1_config(config(hooks), root.to_path_buf(), sender),
            receiver,
        )
    }

    fn actor() -> AgentEventContext {
        AgentEventContext::new(
            &AgentDescriptor {
                id: Uuid::from_u128(1),
                session_id: Uuid::from_u128(2),
                team_id: Uuid::from_u128(3),
                parent_id: Some(Uuid::from_u128(4)),
                parent_session_id: Some(Uuid::from_u128(5)),
                level: 2,
                name: "reviewer".to_owned(),
                role: "code review".to_owned(),
                status: crate::model::AgentStatus::Running,
                permission_level: AgentPermissionLevel::Controlled,
            },
            Some("request-42"),
        )
    }

    fn tool_input(tool: &str, arguments: Value) -> ToolDispatchInput {
        ToolDispatchInput {
            actor: actor(),
            tool: tool.to_owned(),
            arguments,
        }
    }

    fn validate_object(value: &Value) -> Result<(), String> {
        value
            .is_object()
            .then_some(())
            .ok_or_else(|| "arguments must be an object".to_owned())
    }

    fn approved(config: &HookConfig) -> ApprovedHookManifest {
        let revision = effective_revision(config);
        let fingerprints = config
            .hooks
            .iter()
            .filter_map(|hook| {
                hook.entrypoint_fingerprint
                    .as_ref()
                    .map(|fingerprint| (hook.id.clone(), fingerprint.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let manifest = HookManifest::parse_json(&serde_json::to_string(config).unwrap())
            .unwrap()
            .with_revision(revision.clone())
            .with_entrypoint_fingerprints(&fingerprints)
            .unwrap();
        ApprovedHookManifest::new(manifest, revision, fingerprints).unwrap()
    }

    #[test]
    fn v1_conversion_preserves_order_matcher_timeout_fingerprint_and_revision() {
        let directory = tempfile::tempdir().unwrap();
        let first = definition(
            "first",
            HookEvent::ToolDispatching,
            CoreHookKind::Gate,
            vec!["/usr/bin/true".to_owned()],
        );
        let mut second = definition(
            "second",
            HookEvent::MessageSending,
            CoreHookKind::Transform,
            vec!["/usr/bin/true".to_owned()],
        );
        second.timeout_ms = Some(321);
        second.matcher.tools = vec!["send_message".to_owned()];
        second.matcher.agent_levels = vec![2];
        second.entrypoint_fingerprint = Some("ab".repeat(32));
        let config = HookConfig {
            version: 1,
            hooks: vec![first, second],
            revision: "revision-7".to_owned(),
        };
        let approved = approved(&config);
        assert_eq!(approved.revision, "revision-7");
        assert_eq!(approved.manifest.hooks[0].id, "first");
        assert_eq!(approved.manifest.hooks[1].id, "second");
        assert_eq!(approved.manifest.hooks[1].timeout_ms, Some(321));
        assert_eq!(approved.manifest.hooks[1].matcher.tools, ["send_message"]);
        assert_eq!(approved.manifest.hooks[1].matcher.agent_levels, [2]);
        assert_eq!(
            approved.entrypoint_fingerprints.get("second"),
            Some(&"ab".repeat(32))
        );
        assert!(build_v1_scope(&config, directory.path(), &mpsc::sync_channel(8).0).is_ok());
    }

    #[test]
    fn supplied_scope_handle_is_reused_without_another_manager() {
        let directory = tempfile::tempdir().unwrap();
        let project = config(Vec::new());
        let global = ApprovedHookManifest::new(
            HookManifest::default().with_revision("global"),
            "global",
            BTreeMap::new(),
        )
        .unwrap();
        let manager =
            HookHostManager::new_with_registry(supervisor_registry().unwrap(), global).unwrap();
        let key = HookScopeKey::project(directory.path(), project.revision.clone()).unwrap();
        let scope = manager.open_scope(key, Some(approved(&project))).unwrap();
        let expected_scope_id = scope.scope_id();
        let first =
            SupervisorHooks::from_scope(scope.clone(), directory.path(), mpsc::sync_channel(8).0)
                .unwrap();
        let second =
            SupervisorHooks::from_scope(scope, directory.path(), mpsc::sync_channel(8).0).unwrap();
        assert_eq!(
            first.scope_id().as_deref(),
            Some(expected_scope_id.as_str())
        );
        assert_eq!(second.scope_id(), first.scope_id());
        assert!(!first.adapts_external_audit());
        assert!(!second.adapts_external_audit());
        drop(manager);
    }

    #[test]
    fn shared_scope_does_not_forward_manager_audit_to_supervisors() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let global_config = config(vec![definition(
            "shared-allow",
            HookEvent::ToolDispatching,
            CoreHookKind::Gate,
            vec!["/bin/echo".to_owned(), "{}".to_owned()],
        )]);
        let manager = HookHostManager::new_with_registry(
            supervisor_registry().unwrap(),
            approved(&global_config)
                .with_grants_hash("a".repeat(64))
                .unwrap(),
        )
        .unwrap();
        let manager_events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let captured_events = manager_events.clone();
        let _app_subscription = manager.audit_signal().subscribe_fn(move |event| {
            captured_events.lock().unwrap().push(event);
        });
        let key = HookScopeKey::project(directory.path(), global_config.revision).unwrap();
        let scope = manager.open_scope(key, None).unwrap();
        let expected_grants_hash = scope.grants_hash().unwrap();
        let (first_sender, first_receiver) = mpsc::sync_channel(16);
        let (second_sender, second_receiver) = mpsc::sync_channel(16);
        let first =
            SupervisorHooks::from_scope(scope.clone(), directory.path(), first_sender).unwrap();
        let second = SupervisorHooks::from_scope(scope, directory.path(), second_sender).unwrap();

        first
            .control_tool_dispatch(tool_input("read", json!({})), validate_object)
            .unwrap();
        second
            .control_tool_dispatch(tool_input("read", json!({})), validate_object)
            .unwrap();

        let manager_events = manager_events.lock().unwrap();
        assert_eq!(
            manager_events
                .iter()
                .filter(|event| event.hook_id == "shared-allow")
                .count(),
            2
        );
        assert!(
            manager_events.iter().all(|event| {
                event.grants_hash.as_deref() == Some(expected_grants_hash.as_str())
            })
        );
        for receiver in [first_receiver, second_receiver] {
            let records = receiver.try_iter().collect::<Vec<_>>();
            assert!(
                records
                    .iter()
                    .any(|record| record.revision == BUILTIN_REVISION)
            );
            assert!(
                records
                    .iter()
                    .all(|record| record.hook_id != "shared-allow")
            );
        }
    }

    #[test]
    fn non_matching_gate_is_skipped() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let mut hook = definition(
            "bash-only",
            HookEvent::ToolDispatching,
            CoreHookKind::Gate,
            vec!["/bin/echo".to_owned(), r#"{"decision":"deny"}"#.to_owned()],
        );
        hook.matcher.tools = vec!["bash".to_owned()];
        let (pipeline, _) = test_pipeline(directory.path(), vec![hook]);
        assert_eq!(
            pipeline
                .control_tool_dispatch(tool_input("read", json!({})), validate_object)
                .unwrap(),
            json!({})
        );
    }

    #[test]
    fn gate_denial_can_only_reject() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "deny",
                HookEvent::ToolDispatching,
                CoreHookKind::Gate,
                vec![
                    "/bin/echo".to_owned(),
                    r#"{"decision":"deny","message":"blocked"}"#.to_owned(),
                ],
            )],
        );
        assert!(matches!(
            pipeline.control_tool_dispatch(tool_input("read", json!({})), validate_object),
            Err(HookControlError::Denied(message)) if message.contains("blocked")
        ));
    }

    #[test]
    fn gate_execution_failure_fails_closed() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "failure",
                HookEvent::ToolDispatching,
                CoreHookKind::Gate,
                vec!["/usr/bin/false".to_owned()],
            )],
        );
        assert!(matches!(
            pipeline.control_tool_dispatch(tool_input("read", json!({})), validate_object),
            Err(HookControlError::Denied(message)) if message.contains("failure")
        ));
    }

    #[test]
    fn malformed_gate_response_fails_closed() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "malformed",
                HookEvent::ToolDispatching,
                CoreHookKind::Gate,
                vec!["/bin/echo".to_owned(), "not-json".to_owned()],
            )],
        );
        assert!(
            pipeline
                .control_tool_dispatch(tool_input("read", json!({})), validate_object)
                .is_err()
        );
    }

    #[test]
    fn failed_transform_preserves_original_payload() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "failure",
                HookEvent::ToolDispatching,
                CoreHookKind::Transform,
                vec!["/usr/bin/false".to_owned()],
            )],
        );
        let original = json!({"path":"README.md"});
        assert_eq!(
            pipeline
                .control_tool_dispatch(tool_input("read", original.clone()), validate_object)
                .unwrap(),
            original
        );
    }

    #[test]
    fn invalid_transformed_handler_input_preserves_then_validates_original() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "invalid",
                HookEvent::ToolDispatching,
                CoreHookKind::Transform,
                vec![
                    "/bin/echo".to_owned(),
                    r#"{"arguments":"not-an-object"}"#.to_owned(),
                ],
            )],
        );
        let original = json!({"path":"README.md"});
        assert_eq!(
            pipeline
                .control_tool_dispatch(tool_input("read", original.clone()), validate_object)
                .unwrap(),
            original
        );
    }

    #[test]
    fn message_transform_changes_only_body_not_target() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let changed_target = definition(
            "target",
            HookEvent::MessageSending,
            CoreHookKind::Transform,
            vec![
                "/bin/echo".to_owned(),
                r#"{"arguments":{"target":"other","message":"changed"}}"#.to_owned(),
            ],
        );
        let (pipeline, _) = test_pipeline(directory.path(), vec![changed_target]);
        let original = json!({"target":"parent","message":"original"});
        assert_eq!(
            pipeline
                .control_message_send(
                    tool_input("send_message", original.clone()),
                    validate_object
                )
                .unwrap(),
            original
        );

        let body_only = definition(
            "body",
            HookEvent::MessageSending,
            CoreHookKind::Transform,
            vec![
                "/bin/echo".to_owned(),
                r#"{"arguments":{"target":"parent","message":"changed"}}"#.to_owned(),
            ],
        );
        let (pipeline, _) = test_pipeline(directory.path(), vec![body_only]);
        assert_eq!(
            pipeline
                .control_message_send(tool_input("send_message", original), validate_object)
                .unwrap(),
            json!({"target":"parent","message":"changed"})
        );
    }

    #[test]
    fn spawn_transform_may_tighten_but_not_widen_policy() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let original = json!({
            "name":"worker", "role":"review", "task":"check",
            "provider":null, "model":null, "permission_level":"full",
            "enabled_tools":["read","write"], "trusted_extensions":["safe"]
        });
        let tighten = definition(
            "tighten",
            HookEvent::AgentSpawning,
            CoreHookKind::Transform,
            vec![
                "/bin/echo".to_owned(),
                r#"{"arguments":{"name":"worker","role":"review","task":"check","provider":null,"model":null,"permission_level":"controlled","enabled_tools":["read"],"trusted_extensions":["safe"]}}"#.to_owned(),
            ],
        );
        let (pipeline, _) = test_pipeline(directory.path(), vec![tighten]);
        let tightened = pipeline
            .control_agent_spawn(tool_input("spawn_agent", original), validate_object)
            .unwrap();
        assert_eq!(tightened["permission_level"], "controlled");
        assert_eq!(tightened["enabled_tools"], json!(["read"]));

        let widen = definition(
            "widen",
            HookEvent::AgentSpawning,
            CoreHookKind::Transform,
            vec![
                "/bin/echo".to_owned(),
                r#"{"arguments":{"name":"worker","role":"review","task":"check","provider":null,"model":null,"permission_level":"full","enabled_tools":["read","write"],"trusted_extensions":["safe"]}}"#.to_owned(),
            ],
        );
        let (pipeline, _) = test_pipeline(directory.path(), vec![widen]);
        let controlled = json!({
            "name":"worker", "role":"review", "task":"check",
            "provider":null, "model":null, "permission_level":"controlled",
            "enabled_tools":["read"], "trusted_extensions":["safe"]
        });
        assert_eq!(
            pipeline
                .control_agent_spawn(
                    tool_input("spawn_agent", controlled.clone()),
                    validate_object
                )
                .unwrap(),
            controlled
        );
    }

    #[test]
    fn spawn_transform_cannot_change_name_task_or_model_identity() {
        let original = json!({"name":"worker","task":"check","provider":"p","model":"m"});
        assert!(
            validate_transform(
                HookEvent::AgentSpawning,
                &original,
                &json!({"name":"admin","task":"escape","provider":"p2","model":"m2"})
            )
            .is_err()
        );
    }

    #[test]
    fn interaction_transform_requires_nonempty_bounded_answer() {
        let original = json!({"decision":null});
        assert!(
            validate_transform(
                HookEvent::InteractionResolving,
                &original,
                &json!({"decision":"answer"})
            )
            .is_ok()
        );
        assert!(
            validate_transform(
                HookEvent::InteractionResolving,
                &original,
                &json!({"decision":""})
            )
            .is_err()
        );
        assert!(
            validate_transform(
                HookEvent::InteractionResolving,
                &original,
                &json!({"decision":"x".repeat(4097)})
            )
            .is_err()
        );
    }

    #[test]
    fn builtin_floor_rejects_invalid_message_and_spawn() {
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(directory.path(), Vec::new());
        assert!(
            pipeline
                .control_message_send(
                    tool_input("send_message", json!({"target":"parent","message":" "})),
                    validate_object,
                )
                .is_err()
        );
        assert!(
            pipeline
                .control_agent_spawn(
                    tool_input("spawn_agent", json!({"name":"","task":"work"})),
                    validate_object,
                )
                .is_err()
        );
    }

    #[test]
    fn host_audit_adapter_preserves_rich_metadata_only_outcome() {
        let (sender, receiver) = mpsc::sync_channel(4);
        adapt_host_audit(
            &sender,
            HookAuditEvent {
                hook_id: "allow".into(),
                scope_id: "scope".into(),
                event: "pi.tool.dispatching".into(),
                kind: "gate".into(),
                outcome: HostAuditOutcome::Allowed,
                duration_ms: 7,
                revision: "revision-test".into(),
                dropped: true,
                restart_count: 2,
                drop_count: 3,
                grants_hash: Some("grant".into()),
            },
        );
        let record = receiver.try_recv().unwrap();
        assert_eq!(record.hook_id, "allow");
        assert_eq!(record.event, HookEvent::ToolDispatching);
        assert_eq!(record.outcome, CoreAuditOutcome::Allowed);
        assert!(!record.output_truncated);
        assert_eq!(record.scope_id.as_deref(), Some("scope"));
        assert_eq!(record.kind.as_deref(), Some("gate"));
        assert!(record.dropped);
        assert_eq!(record.restart_count, 2);
        assert_eq!(record.drop_count, 3);
        assert_eq!(record.grants_hash.as_deref(), Some("grant"));
    }

    #[test]
    fn observe_stop_and_finalize_are_bounded() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "observe",
                HookEvent::ToolCompleted,
                CoreHookKind::Observe,
                vec!["/usr/bin/true".to_owned()],
            )],
        );
        let started = Instant::now();
        pipeline.observe(HookObservation::new(
            HookEvent::ToolCompleted,
            actor(),
            json!({"tool":"read","success":true}),
        ));
        pipeline.stop_observers();
        pipeline.observe(HookObservation::new(
            HookEvent::ToolCompleted,
            actor(),
            json!({"tool":"read","success":false}),
        ));
        pipeline.finalize(HookObservation::new(
            HookEvent::ToolCompleted,
            actor(),
            json!({"tool":"read","success":true}),
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn authenticated_context_is_separate_and_private_arguments_are_restored() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("inspect.sh");
        write_executable(
            &script,
            "#!/bin/sh\nread request\ncase \"$request\" in *approval-secret*|*capability-secret*) printf '{\"decision\":\"deny\",\"message\":\"secret leaked\"}\\n' ;; *) printf '{}\\n' ;; esac\n",
        );
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "inspect",
                HookEvent::ToolDispatching,
                CoreHookKind::Gate,
                vec![script.to_string_lossy().into_owned()],
            )],
        );
        let original = json!({
            "command":"echo ok",
            "approval_ticket":"approval-secret",
            "nested":{"capability":"capability-secret"}
        });
        assert_eq!(
            pipeline
                .control_tool_dispatch(tool_input("bash", original.clone()), validate_object)
                .unwrap(),
            original
        );
    }

    #[test]
    fn gate_observes_final_transformed_payload_not_pretransform_payload() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let gate_script = directory.path().join("gate.sh");
        write_executable(
            &gate_script,
            "#!/bin/sh\nread request\ncase \"$request\" in *\\\"changed\\\":true*) printf '{\"decision\":\"deny\",\"message\":\"saw final payload\"}\\n' ;; *) printf '{}\\n' ;; esac\n",
        );
        let hooks = vec![
            definition(
                "gate",
                HookEvent::ToolDispatching,
                CoreHookKind::Gate,
                vec![gate_script.to_string_lossy().into_owned()],
            ),
            definition(
                "transform",
                HookEvent::ToolDispatching,
                CoreHookKind::Transform,
                vec![
                    "/bin/echo".to_owned(),
                    r#"{"arguments":{"changed":true}}"#.to_owned(),
                ],
            ),
        ];
        let (pipeline, _) = test_pipeline(directory.path(), hooks);
        assert!(matches!(
            pipeline.control_tool_dispatch(
                tool_input("read", json!({"changed":false})),
                validate_object
            ),
            Err(HookControlError::Denied(message)) if message.contains("saw final payload")
        ));
    }

    #[test]
    fn same_event_reentrancy_fails_closed_without_recursive_invocation() {
        if !sandbox_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let (pipeline, _) = test_pipeline(
            directory.path(),
            vec![definition(
                "allow",
                HookEvent::ToolDispatching,
                CoreHookKind::Gate,
                vec!["/bin/echo".to_owned(), "{}".to_owned()],
            )],
        );
        let _guard = ReentrancyGuard::enter(
            ReentrancyKind::Invocation,
            event_name(HookEvent::ToolDispatching),
        )
        .unwrap();
        assert!(
            pipeline
                .control_tool_dispatch(tool_input("read", json!({})), validate_object)
                .is_err()
        );
    }

    #[test]
    fn entrypoint_fingerprint_uses_sha256_contract() {
        let fingerprint = Sha256::digest(b"#!/bin/sh\nprintf '{}\\n'\n")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(fingerprint.len(), 64);
        assert!(fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn external_executor_is_not_duplicated_in_agent_team() {
        let source = include_str!("hooks.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains(&["Command", "::new"].concat()));
        assert!(!production.contains(&["sandbox", "_profile"].concat()));
        assert!(!production.contains(&["std::", "process"].concat()));
        assert!(!production.contains(&["fn ", "invoke("].concat()));
    }
}
