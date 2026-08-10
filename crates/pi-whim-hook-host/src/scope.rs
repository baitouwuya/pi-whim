//! Scope sharing, lifecycle, and ordered invocation pipelines.

use crate::audit::{HookAuditEvent, HookAuditOutcome, HookHealthStatus, HookHostHealth};
use crate::executor;
use crate::invocation::{
    HookGateDecision, HookInvocation, HookInvocationContext, HookObserveReceipt, HookPayload,
    HookTransformResult,
};
use crate::manifest::{HookDefinition, HookKind, HookManifest};
use crate::persistent::{
    AuditReporter, HealthReporter, ObserveSubmit, PersistentHook, PersistentHookConfig,
};
use crate::registry::EventRegistry;
use crate::{HookHostError, HookHostResult};
use parking_lot::Mutex;
use pi_whim_signal::{Signal, StateSignal};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread;
use std::time::Instant;

const OBSERVE_QUEUE_CAPACITY: usize = 64;

/// A manifest and its caller-approved entrypoint fingerprints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovedHookManifest {
    /// Parsed and validated manifest.
    pub manifest: HookManifest,
    /// Revision approved by the caller.
    pub revision: String,
    /// SHA-256 hex fingerprints indexed by hook id.
    pub entrypoint_fingerprints: BTreeMap<String, String>,
}

impl ApprovedHookManifest {
    /// Creates an approved manifest envelope.
    pub fn new(
        manifest: HookManifest,
        revision: impl Into<String>,
        entrypoint_fingerprints: BTreeMap<String, String>,
    ) -> HookHostResult<Self> {
        let revision = revision.into();
        if revision.is_empty() || revision.len() > 128 {
            return Err(HookHostError::InvalidScope(
                "manifest revision must be 1..=128 bytes".to_owned(),
            ));
        }
        let hook_ids = manifest
            .hooks
            .iter()
            .map(|hook| hook.id.as_str())
            .collect::<HashSet<_>>();
        for (hook_id, fingerprint) in &entrypoint_fingerprints {
            if !hook_ids.contains(hook_id.as_str()) {
                return Err(HookHostError::InvalidScope(format!(
                    "entrypoint fingerprint supplied for unknown hook {hook_id}"
                )));
            }
            if fingerprint.len() != 64 || !fingerprint.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(HookHostError::InvalidScope(
                    "entrypoint fingerprints must be 64 hexadecimal bytes".to_owned(),
                ));
            }
        }
        Ok(Self {
            manifest,
            revision,
            entrypoint_fingerprints,
        })
    }
}

/// Canonical project root and approved manifest revision identifying one scope.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookScopeKey {
    /// Canonical project root. `None` identifies an isolated app scope.
    pub project_root: Option<PathBuf>,
    /// Approved project/global manifest revision.
    pub manifest_revision: String,
}

impl HookScopeKey {
    /// Creates an isolated app scope key.
    pub fn app(revision: impl Into<String>) -> HookHostResult<Self> {
        Self::build(None, revision.into())
    }

    /// Creates a canonical project scope key.
    pub fn project(
        project_root: impl AsRef<Path>,
        revision: impl Into<String>,
    ) -> HookHostResult<Self> {
        let root = std::fs::canonicalize(project_root.as_ref()).map_err(HookHostError::io)?;
        if !root.is_dir() {
            return Err(HookHostError::InvalidScope(
                "project root must be a directory".to_owned(),
            ));
        }
        Self::build(Some(root), revision.into())
    }

    /// Returns a stable digest suitable for audit records and process context.
    pub fn scope_id(&self) -> String {
        let mut digest = Sha256::new();
        if let Some(root) = &self.project_root {
            digest.update(root.to_string_lossy().as_bytes());
        }
        digest.update([0]);
        digest.update(self.manifest_revision.as_bytes());
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn build(project_root: Option<PathBuf>, manifest_revision: String) -> HookHostResult<Self> {
        if manifest_revision.is_empty() || manifest_revision.len() > 128 {
            return Err(HookHostError::InvalidScope(
                "manifest revision must be 1..=128 bytes".to_owned(),
            ));
        }
        Ok(Self {
            project_root,
            manifest_revision,
        })
    }
}

/// Typed reentrancy categories used by the host guard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ReentrancyKind {
    /// A gate/transform/observe invocation.
    Invocation,
    /// An internal host event such as health/audit publication.
    HostEvent,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct ReentrancyKey {
    kind: ReentrancyKind,
    event: String,
}

thread_local! {
    static ACTIVE_REENTRANCY: RefCell<Vec<ReentrancyKey>> = const { RefCell::new(Vec::new()) };
}

/// RAII guard preventing a hook invocation from recursively triggering itself.
pub struct ReentrancyGuard {
    key: ReentrancyKey,
}

impl ReentrancyGuard {
    /// Enters a typed event guard on the current thread.
    pub fn enter(kind: ReentrancyKind, event: impl Into<String>) -> HookHostResult<Self> {
        let key = ReentrancyKey {
            kind,
            event: event.into(),
        };
        let entered = ACTIVE_REENTRANCY.with(|active| {
            let mut active = active.borrow_mut();
            if active.contains(&key) {
                false
            } else {
                active.push(key.clone());
                true
            }
        });
        if entered {
            Ok(Self { key })
        } else {
            Err(HookHostError::ReentrantInvocation)
        }
    }
}

impl Drop for ReentrancyGuard {
    fn drop(&mut self) {
        ACTIVE_REENTRANCY.with(|active| {
            let mut active = active.borrow_mut();
            if let Some(index) = active.iter().rposition(|key| key == &self.key) {
                active.remove(index);
            }
        });
    }
}

/// Cloneable manager shared by app and multiple supervisors.
#[derive(Clone)]
pub struct HookHostManager {
    inner: Arc<ManagerInner>,
}

struct ManagerInner {
    registry: EventRegistry,
    global: ApprovedHookManifest,
    scopes: Mutex<HashMap<HookScopeKey, std::sync::Weak<HookScopeState>>>,
    audit_signal: Signal<HookAuditEvent>,
    audit_emitter: pi_whim_signal::SignalEmitter<HookAuditEvent>,
    health_signal: StateSignal<Vec<HookHostHealth>>,
}

impl HookHostManager {
    /// Creates a manager using the built-in registry and a global manifest.
    pub fn new(global_manifest: HookManifest) -> HookHostResult<Self> {
        let revision = if global_manifest.revision.is_empty() {
            "global".to_owned()
        } else {
            global_manifest.revision.clone()
        };
        let approved = ApprovedHookManifest::new(global_manifest, revision, BTreeMap::new())?;
        Self::new_with_registry(EventRegistry::default(), approved)
    }

    /// Creates an empty manager with no global definitions.
    pub fn empty() -> HookHostResult<Self> {
        Self::new(HookManifest::default().with_revision("global"))
    }

    /// Creates a manager with an explicit event registry and approved global manifest.
    pub fn new_with_registry(
        registry: EventRegistry,
        global: ApprovedHookManifest,
    ) -> HookHostResult<Self> {
        registry.validate_manifest(&global.manifest)?;
        let (audit_signal, audit_emitter) = Signal::channel();
        Ok(Self {
            inner: Arc::new(ManagerInner {
                registry,
                global,
                scopes: Mutex::new(HashMap::new()),
                audit_signal,
                audit_emitter,
                health_signal: StateSignal::new(Vec::new()),
            }),
        })
    }

    /// Returns the registry used for all scopes.
    pub fn registry(&self) -> EventRegistry {
        self.inner.registry.clone()
    }

    /// Returns the metadata-only audit stream.
    pub fn audit_signal(&self) -> Signal<HookAuditEvent> {
        self.inner.audit_signal.clone()
    }

    /// Returns the replayable health state signal.
    pub fn health_signal(&self) -> StateSignal<Vec<HookHostHealth>> {
        self.inner.health_signal.clone()
    }

    /// Opens or reuses a scope identified by its canonical key.
    pub fn open_scope(
        &self,
        key: HookScopeKey,
        project_manifest: Option<ApprovedHookManifest>,
    ) -> HookHostResult<HookScopeHandle> {
        if let Some(existing) = self
            .inner
            .scopes
            .lock()
            .get(&key)
            .and_then(std::sync::Weak::upgrade)
        {
            return Ok(HookScopeHandle { state: existing });
        }
        if let Some(project) = &project_manifest {
            self.inner.registry.validate_manifest(&project.manifest)?;
            if project.revision != key.manifest_revision {
                return Err(HookHostError::InvalidScope(
                    "project manifest revision does not match scope key".to_owned(),
                ));
            }
            if key.project_root.is_none() {
                return Err(HookHostError::InvalidScope(
                    "project manifest requires a project scope".to_owned(),
                ));
            }
        } else if key.project_root.is_none() && key.manifest_revision != self.inner.global.revision
        {
            return Err(HookHostError::InvalidScope(
                "app scope revision must match the global manifest revision".to_owned(),
            ));
        }
        let state = HookScopeState::new(self.inner.clone(), key.clone(), project_manifest)?;
        let handle = HookScopeHandle {
            state: state.clone(),
        };
        self.inner.scopes.lock().insert(key, Arc::downgrade(&state));
        Ok(handle)
    }

    /// Revokes a scope and stops all resident v2 processes.
    pub fn revoke_scope(&self, key: &HookScopeKey) -> bool {
        let state = self
            .inner
            .scopes
            .lock()
            .remove(key)
            .and_then(|weak| weak.upgrade());
        if let Some(state) = state {
            state.stop();
            true
        } else {
            false
        }
    }
}

/// Cloneable invocation handle for one scope.
#[derive(Clone)]
pub struct HookScopeHandle {
    state: Arc<HookScopeState>,
}

impl HookScopeHandle {
    /// Returns the canonical scope key.
    pub fn key(&self) -> HookScopeKey {
        self.state.key.clone()
    }

    /// Returns the stable scope digest.
    pub fn scope_id(&self) -> String {
        self.state.scope_id.clone()
    }

    /// Returns whether the scope has been revoked.
    pub fn is_revoked(&self) -> bool {
        self.state.revoked.load(Ordering::Acquire)
    }

    /// Revokes the scope and stops resident processes within the bounded budget.
    pub fn revoke(&self) {
        self.state.stop();
    }

    /// Runs the ordered gate chain. Execution failures become `FailedClosed`.
    pub fn gate(
        &self,
        event: impl Into<String>,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<HookGateDecision> {
        self.state.gate(event.into(), context, payload)
    }

    /// Runs the ordered transform chain. A failed transform preserves its prior payload.
    pub fn transform(
        &self,
        event: impl Into<String>,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<HookTransformResult> {
        self.state.transform(event.into(), context, payload)
    }

    /// Queues ordered observe notifications using bounded delivery.
    pub fn observe(
        &self,
        event: impl Into<String>,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<HookObserveReceipt> {
        self.state.observe(event.into(), context, payload)
    }

    /// Returns a current health snapshot for this scope.
    pub fn health(&self) -> Vec<HookHostHealth> {
        self.state
            .manager
            .health_signal
            .get()
            .into_iter()
            .filter(|health| health.scope_id == self.state.scope_id)
            .collect()
    }
}

struct HookScopeState {
    manager: Arc<ManagerInner>,
    key: HookScopeKey,
    scope_id: String,
    workspace_root: PathBuf,
    hooks: Vec<BoundHook>,
    revoked: AtomicBool,
    observe_sender: SyncSender<ObserveJob>,
}

struct ObserveJob {
    hook_index: usize,
    invocation: HookInvocation,
}

struct BoundHook {
    definition: HookDefinition,
    canonical_event: String,
    wire_event: String,
    version: u32,
    project_scoped: bool,
    persistent: Option<Arc<PersistentHook>>,
}

impl HookScopeState {
    fn new(
        manager: Arc<ManagerInner>,
        key: HookScopeKey,
        project_manifest: Option<ApprovedHookManifest>,
    ) -> HookHostResult<Arc<Self>> {
        let scope_id = key.scope_id();
        let workspace_root = match &key.project_root {
            Some(root) => root.clone(),
            None => {
                let root = std::env::temp_dir().join(format!(
                    "pi-whim-hook-app-{}",
                    uuid::Uuid::new_v4().simple()
                ));
                std::fs::create_dir_all(&root).map_err(HookHostError::io)?;
                root
            }
        };
        let mut hooks = Vec::new();
        append_bound_hooks(
            &manager,
            &mut hooks,
            &manager.global,
            false,
            &scope_id,
            &key.manifest_revision,
            &workspace_root,
        )?;
        if let Some(project) = &project_manifest {
            append_bound_hooks(
                &manager,
                &mut hooks,
                project,
                true,
                &scope_id,
                &key.manifest_revision,
                &workspace_root,
            )?;
        }
        let mut ids = HashSet::new();
        for hook in &hooks {
            if !ids.insert(hook.definition.id.clone()) {
                return Err(HookHostError::InvalidScope(format!(
                    "duplicate hook id {} across global/project definitions",
                    hook.definition.id
                )));
            }
        }
        let (observe_sender, observe_receiver) = mpsc::sync_channel(OBSERVE_QUEUE_CAPACITY);
        let state = Arc::new(Self {
            manager: manager.clone(),
            key,
            scope_id: scope_id.clone(),
            workspace_root,
            hooks,
            revoked: AtomicBool::new(false),
            observe_sender,
        });
        let weak = Arc::downgrade(&state);
        thread::Builder::new()
            .name(format!("pi-whim-hook-observe-scope-{scope_id}"))
            .spawn(move || observe_worker(weak, observe_receiver))
            .map_err(HookHostError::io)?;
        for hook in &state.hooks {
            let health = hook.health(&state);
            manager.health_signal.update(|values| {
                if let Some(existing) = values.iter_mut().find(|value| {
                    value.scope_id == health.scope_id && value.hook_id == health.hook_id
                }) {
                    *existing = health.clone();
                } else {
                    values.push(health.clone());
                }
                values.sort_by(|left, right| {
                    left.scope_id
                        .cmp(&right.scope_id)
                        .then_with(|| left.hook_id.cmp(&right.hook_id))
                });
            });
        }
        Ok(state)
    }

    fn gate(
        &self,
        event: String,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<HookGateDecision> {
        self.ensure_active(&context)?;
        let canonical_event = self.canonical_event(&event)?;
        let _guard = ReentrancyGuard::enter(ReentrancyKind::Invocation, canonical_event.clone())?;
        let mut request_id = uuid::Uuid::new_v4().simple().to_string();
        for hook in self.hooks.iter().filter(|hook| {
            hook.canonical_event == canonical_event
                && hook.definition.kind == HookKind::Gate
                && hook.definition.matcher.matches(payload.as_value())
        }) {
            let started = Instant::now();
            let filtered = match self.manager.registry.filter_payload(
                hook.version,
                &hook.definition,
                hook.project_scoped,
                payload.as_value(),
            ) {
                Ok(filtered) => filtered,
                Err(error) => {
                    self.audit(hook, HookAuditOutcome::Failed, started, &context, false);
                    return Ok(HookGateDecision::FailedClosed {
                        hook_id: hook.definition.id.clone(),
                        error,
                    });
                }
            };
            let invocation = HookInvocation::new(
                request_id.clone(),
                canonical_event.clone(),
                HookKind::Gate,
                context.clone(),
                filtered,
            )?;
            let response = self.invoke_hook(hook, &invocation);
            match response {
                Ok(response) => {
                    let decision = match gate_decision(&response) {
                        Ok(decision) => decision,
                        Err(error) => {
                            self.audit(hook, HookAuditOutcome::Failed, started, &context, false);
                            return Ok(HookGateDecision::FailedClosed {
                                hook_id: hook.definition.id.clone(),
                                error,
                            });
                        }
                    };
                    match decision {
                        Some(message) => {
                            self.audit(hook, HookAuditOutcome::Denied, started, &context, false);
                            return Ok(HookGateDecision::Deny {
                                hook_id: hook.definition.id.clone(),
                                message,
                            });
                        }
                        None => {
                            self.audit(hook, HookAuditOutcome::Allowed, started, &context, false);
                        }
                    }
                }
                Err(error) => {
                    let outcome = if matches!(error, HookHostError::Timeout { .. }) {
                        HookAuditOutcome::TimedOut
                    } else {
                        HookAuditOutcome::Failed
                    };
                    self.audit(hook, outcome, started, &context, false);
                    return Ok(HookGateDecision::FailedClosed {
                        hook_id: hook.definition.id.clone(),
                        error,
                    });
                }
            }
            request_id = uuid::Uuid::new_v4().simple().to_string();
        }
        Ok(HookGateDecision::Allow)
    }

    fn transform(
        &self,
        event: String,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<HookTransformResult> {
        self.ensure_active(&context)?;
        let canonical_event = self.canonical_event(&event)?;
        let _guard = ReentrancyGuard::enter(ReentrancyKind::Invocation, canonical_event.clone())?;
        let original = payload.clone();
        let mut current = payload;
        let mut failure: Option<(String, HookHostError)> = None;
        let mut changed = false;
        for index in 0..self.hooks.len() {
            let hook = &self.hooks[index];
            if !(hook.canonical_event == canonical_event
                && hook.definition.kind == HookKind::Transform
                && hook.definition.matcher.matches(current.as_value()))
            {
                continue;
            }
            let started = Instant::now();
            let filtered = match self.manager.registry.filter_payload(
                hook.version,
                &hook.definition,
                hook.project_scoped,
                current.as_value(),
            ) {
                Ok(filtered) => filtered,
                Err(error) => {
                    self.audit(hook, HookAuditOutcome::Preserved, started, &context, false);
                    failure = Some((hook.definition.id.clone(), error));
                    continue;
                }
            };
            let invocation = HookInvocation::new(
                uuid::Uuid::new_v4().simple().to_string(),
                canonical_event.clone(),
                HookKind::Transform,
                context.clone(),
                filtered,
            )?;
            match self.invoke_hook(hook, &invocation) {
                Ok(response) => match self.manager.registry.apply_transform(
                    hook.version,
                    &hook.definition,
                    hook.project_scoped,
                    &current,
                    &response,
                ) {
                    Ok(next) => {
                        changed |= next != current;
                        current = next;
                        self.audit(
                            hook,
                            HookAuditOutcome::Transformed,
                            started,
                            &context,
                            false,
                        );
                    }
                    Err(error) => {
                        self.audit(hook, HookAuditOutcome::Preserved, started, &context, false);
                        failure = Some((hook.definition.id.clone(), error));
                    }
                },
                Err(error) => {
                    let outcome = if matches!(error, HookHostError::Timeout { .. }) {
                        HookAuditOutcome::TimedOut
                    } else {
                        HookAuditOutcome::Preserved
                    };
                    self.audit(hook, outcome, started, &context, false);
                    failure = Some((hook.definition.id.clone(), error));
                }
            }
        }
        if let Some((hook_id, error)) = failure
            && !changed
        {
            return Ok(HookTransformResult::Preserved {
                hook_id: Some(hook_id),
                error: Some(error),
                payload: original,
            });
        }
        Ok(HookTransformResult::Transformed(current))
    }

    fn observe(
        &self,
        event: String,
        context: HookInvocationContext,
        payload: HookPayload,
    ) -> HookHostResult<HookObserveReceipt> {
        self.ensure_active(&context)?;
        let canonical_event = self.canonical_event(&event)?;
        let _guard = ReentrancyGuard::enter(ReentrancyKind::Invocation, canonical_event.clone())?;
        let mut accepted = 0;
        let mut dropped = 0;
        for (index, hook) in self.hooks.iter().enumerate().filter(|(_, hook)| {
            hook.canonical_event == canonical_event
                && hook.definition.kind == HookKind::Observe
                && hook.definition.matcher.matches(payload.as_value())
        }) {
            let started = Instant::now();
            let filtered = match self.manager.registry.filter_payload(
                hook.version,
                &hook.definition,
                hook.project_scoped,
                payload.as_value(),
            ) {
                Ok(filtered) => filtered,
                Err(error) => {
                    let _ = error;
                    dropped += 1;
                    self.audit(hook, HookAuditOutcome::Dropped, started, &context, true);
                    continue;
                }
            };
            let invocation = HookInvocation::new(
                uuid::Uuid::new_v4().simple().to_string(),
                canonical_event.clone(),
                HookKind::Observe,
                context.clone(),
                filtered,
            )?;
            if let Some(persistent) = &hook.persistent {
                match persistent.submit_observe(invocation) {
                    Ok(ObserveSubmit {
                        accepted: true,
                        dropped: queue_dropped,
                    }) => {
                        accepted += 1;
                        dropped += queue_dropped;
                        let outcome = if queue_dropped > 0 {
                            HookAuditOutcome::Dropped
                        } else {
                            HookAuditOutcome::Observed
                        };
                        self.audit(hook, outcome, started, &context, queue_dropped > 0);
                    }
                    Ok(ObserveSubmit {
                        accepted: false,
                        dropped: queue_dropped,
                    }) => {
                        dropped += queue_dropped;
                        self.audit(hook, HookAuditOutcome::Dropped, started, &context, true);
                    }
                    Err(error) => {
                        dropped += 1;
                        let _ = error;
                        self.audit(hook, HookAuditOutcome::Dropped, started, &context, true);
                    }
                }
            } else {
                let job = ObserveJob {
                    hook_index: index,
                    invocation,
                };
                match self.observe_sender.try_send(job) {
                    Ok(()) => {
                        accepted += 1;
                    }
                    Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {
                        dropped += 1;
                        self.audit(hook, HookAuditOutcome::Dropped, started, &context, true);
                    }
                }
            }
        }
        Ok(HookObserveReceipt { accepted, dropped })
    }

    fn invoke_hook(&self, hook: &BoundHook, invocation: &HookInvocation) -> HookHostResult<Value> {
        if let Some(persistent) = &hook.persistent {
            persistent.call(invocation)
        } else {
            executor::invoke_v1(
                &hook.definition,
                &hook.wire_event,
                invocation.payload.as_value(),
                &self.workspace_root,
            )
            .map_err(|error| match error {
                HookHostError::Timeout { .. } => HookHostError::Timeout {
                    hook_id: hook.definition.id.clone(),
                },
                other => other,
            })
        }
    }

    fn execute_observe(&self, job: ObserveJob) {
        let Some(hook) = self.hooks.get(job.hook_index) else {
            return;
        };
        if self.revoked.load(Ordering::Acquire) {
            return;
        }
        let started = Instant::now();
        let result = self.invoke_hook(hook, &job.invocation);
        let outcome = match result {
            Ok(_) => HookAuditOutcome::Observed,
            Err(HookHostError::Timeout { .. }) => HookAuditOutcome::TimedOut,
            Err(_) => HookAuditOutcome::Failed,
        };
        self.audit(hook, outcome, started, &job.invocation.context, false);
    }

    fn ensure_active(&self, context: &HookInvocationContext) -> HookHostResult<()> {
        if self.revoked.load(Ordering::Acquire) {
            return Err(HookHostError::ScopeRevoked);
        }
        if !context.authenticated {
            return Err(HookHostError::UnauthenticatedContext);
        }
        if context.scope_id != self.scope_id || context.revision != self.key.manifest_revision {
            return Err(HookHostError::UnauthenticatedContext);
        }
        let expected_project_root = self
            .key
            .project_root
            .as_ref()
            .map(|root| root.to_string_lossy().into_owned());
        if context.project_root != expected_project_root {
            return Err(HookHostError::UnauthenticatedContext);
        }
        Ok(())
    }

    fn canonical_event(&self, event: &str) -> HookHostResult<String> {
        self.manager
            .registry
            .canonical_event(event)
            .ok_or_else(|| HookHostError::UnknownEvent {
                event: event.to_owned(),
            })
    }

    fn audit(
        &self,
        hook: &BoundHook,
        outcome: HookAuditOutcome,
        started: Instant,
        context: &HookInvocationContext,
        dropped: bool,
    ) {
        let (restart_count, drop_count) = hook
            .persistent
            .as_ref()
            .map(|persistent| {
                let health = persistent.health();
                (health.restart_count, health.drop_count)
            })
            .unwrap_or((0, 0));
        let event = HookAuditEvent {
            hook_id: hook.definition.id.clone(),
            scope_id: self.scope_id.clone(),
            event: hook.canonical_event.clone(),
            kind: kind_name(hook.definition.kind).to_owned(),
            outcome,
            duration_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
            revision: self.key.manifest_revision.clone(),
            dropped,
            restart_count,
            drop_count,
            grants_hash: context.grants_hash.clone(),
        };
        self.manager.audit_emitter.emit(event);
    }

    fn stop(&self) {
        if self.revoked.swap(true, Ordering::AcqRel) {
            return;
        }
        for hook in &self.hooks {
            if let Some(persistent) = &hook.persistent {
                persistent.stop();
            }
        }
        self.manager
            .health_signal
            .update(|values| values.retain(|health| health.scope_id != self.scope_id));
        if self.key.project_root.is_none() {
            let _ = std::fs::remove_dir_all(&self.workspace_root);
        }
    }
}

impl Drop for HookScopeState {
    fn drop(&mut self) {
        self.stop();
    }
}

impl BoundHook {
    fn health(&self, state: &HookScopeState) -> HookHostHealth {
        if let Some(persistent) = &self.persistent {
            persistent.health()
        } else {
            HookHostHealth {
                hook_id: self.definition.id.clone(),
                scope_id: state.scope_id.clone(),
                event: self.canonical_event.clone(),
                status: HookHealthStatus::Ready,
                revision: state.key.manifest_revision.clone(),
                restart_count: 0,
                drop_count: 0,
                last_error: None,
            }
        }
    }
}

fn append_bound_hooks(
    manager: &Arc<ManagerInner>,
    hooks: &mut Vec<BoundHook>,
    approved: &ApprovedHookManifest,
    project_scoped: bool,
    scope_id: &str,
    revision: &str,
    workspace_root: &Path,
) -> HookHostResult<()> {
    for source_definition in &approved.manifest.hooks {
        let canonical_event = manager
            .registry
            .canonical_event(&source_definition.event)
            .ok_or_else(|| HookHostError::UnknownEvent {
                event: source_definition.event.clone(),
            })?;
        let mut definition = source_definition.clone();
        match approved.entrypoint_fingerprints.get(&definition.id) {
            Some(fingerprint) => definition.entrypoint_fingerprint = Some(fingerprint.clone()),
            None if project_scoped => {
                return Err(HookHostError::InvalidScope(format!(
                    "project hook {} has no approved entrypoint fingerprint",
                    definition.id
                )));
            }
            None => {}
        }
        let persistent = if definition.version_is_v2(approved.manifest.version) {
            let health_manager = manager.clone();
            let health_reporter: HealthReporter = Arc::new(move |health| {
                health_manager.health_signal.update(|values| {
                    if let Some(existing) = values.iter_mut().find(|value| {
                        value.scope_id == health.scope_id && value.hook_id == health.hook_id
                    }) {
                        *existing = health.clone();
                    } else {
                        values.push(health.clone());
                    }
                    values.sort_by(|left, right| {
                        left.scope_id
                            .cmp(&right.scope_id)
                            .then_with(|| left.hook_id.cmp(&right.hook_id))
                    });
                });
            });
            let audit_manager = manager.clone();
            let audit_reporter: AuditReporter = Arc::new(move |event| {
                let _ = audit_manager.audit_emitter.emit(event);
            });
            Some(PersistentHook::new(PersistentHookConfig {
                definition: definition.clone(),
                event: canonical_event.clone(),
                project_root: workspace_root.to_path_buf(),
                scope_id: scope_id.to_owned(),
                revision: revision.to_owned(),
                health_reporter,
                audit_reporter,
            })?)
        } else {
            None
        };
        hooks.push(BoundHook {
            definition,
            canonical_event,
            wire_event: source_definition.event.clone(),
            version: approved.manifest.version,
            project_scoped,
            persistent,
        });
    }
    Ok(())
}

fn observe_worker(weak: std::sync::Weak<HookScopeState>, receiver: Receiver<ObserveJob>) {
    while let Ok(job) = receiver.recv() {
        let Some(state) = weak.upgrade() else {
            break;
        };
        state.execute_observe(job);
    }
}

fn gate_decision(response: &Value) -> HookHostResult<Option<String>> {
    if response.is_null() {
        return Ok(None);
    }
    let object = response.as_object().ok_or_else(|| {
        HookHostError::InvalidInvocation("gate response must be an object".to_owned())
    })?;
    if let Some(unknown) = object
        .keys()
        .find(|key| *key != "decision" && *key != "message")
    {
        return Err(HookHostError::InvalidInvocation(format!(
            "gate response contains unknown field {unknown}"
        )));
    }
    let decision = object
        .get("decision")
        .and_then(Value::as_str)
        .unwrap_or("allow");
    match decision {
        "allow" => Ok(None),
        "deny" => Ok(Some(
            object
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("blocked by hook")
                .chars()
                .take(4096)
                .collect(),
        )),
        _ => Err(HookHostError::InvalidInvocation(
            "gate decision must be allow or deny".to_owned(),
        )),
    }
}

fn kind_name(kind: HookKind) -> &'static str {
    match kind {
        HookKind::Gate => "gate",
        HookKind::Transform => "transform",
        HookKind::Observe => "observe",
    }
}

trait DefinitionVersion {
    fn version_is_v2(&self, manifest_version: u32) -> bool;
}

impl DefinitionVersion for HookDefinition {
    fn version_is_v2(&self, manifest_version: u32) -> bool {
        manifest_version == 2
    }
}
