use crate::{WaitError, WaitHub, WaitSourceDescriptor, WaitSourceHandle, WaitSourceId};
use parking_lot::{Mutex, RwLock};
use pi_whim_signal::{Subscription, SubscriptionScope};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Authenticated routing metadata attached to one session runtime wait scope.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct WaitRuntimeMetadata {
    project_id: Option<String>,
    session_key: Option<String>,
    session_id: Option<String>,
    hook_scope_id: Option<String>,
}

impl WaitRuntimeMetadata {
    pub fn new(project_id: Option<String>, hook_scope_id: Option<String>) -> Self {
        Self {
            project_id,
            hook_scope_id,
            ..Self::default()
        }
    }

    pub fn project_id(&self) -> Option<&str> {
        self.project_id.as_deref()
    }

    pub fn session_key(&self) -> Option<&str> {
        self.session_key.as_deref()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn hook_scope_id(&self) -> Option<&str> {
        self.hook_scope_id.as_deref()
    }
}

impl fmt::Debug for WaitRuntimeMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WaitRuntimeMetadata")
            .field("project_id", &self.project_id)
            .field("session_id", &self.session_id)
            .field("hook_scope_id", &self.hook_scope_id)
            .finish()
    }
}

struct WaitRuntimeInner {
    hub: WaitHub,
    metadata: RwLock<WaitRuntimeMetadata>,
    sources: Mutex<BTreeMap<WaitSourceId, WaitSourceHandle>>,
    exported_signals: Mutex<BTreeSet<WaitSourceId>>,
    subscriptions: SubscriptionScope,
    closed: AtomicBool,
}

/// Cloneable owner for one session runtime's shared wait hub and exports.
///
/// Source producer leases and upstream subscriptions are closed together. The
/// hub itself remains usable by the agent supervisor long enough to settle
/// pending tasks as `source_closed`.
#[derive(Clone)]
pub struct WaitRuntimeScope {
    inner: Arc<WaitRuntimeInner>,
}

impl WaitRuntimeScope {
    pub fn new(metadata: WaitRuntimeMetadata) -> Result<Self, WaitError> {
        Ok(Self::with_hub(WaitHub::new()?, metadata))
    }

    pub fn with_hub(hub: WaitHub, metadata: WaitRuntimeMetadata) -> Self {
        Self {
            inner: Arc::new(WaitRuntimeInner {
                hub,
                metadata: RwLock::new(metadata),
                sources: Mutex::new(BTreeMap::new()),
                exported_signals: Mutex::new(BTreeSet::new()),
                subscriptions: SubscriptionScope::new(),
                closed: AtomicBool::new(false),
            }),
        }
    }

    pub fn hub(&self) -> WaitHub {
        self.inner.hub.clone()
    }

    pub fn same_hub(&self, hub: &WaitHub) -> bool {
        self.inner.hub.same_instance(hub)
    }

    pub fn metadata(&self) -> WaitRuntimeMetadata {
        self.inner.metadata.read().clone()
    }

    pub fn bind_session(&self, session_key: Option<String>, session_id: Option<String>) {
        let mut metadata = self.inner.metadata.write();
        metadata.session_key = session_key;
        metadata.session_id = session_id;
    }

    pub fn register_source(
        &self,
        descriptor: WaitSourceDescriptor,
        exported_signal: bool,
    ) -> Result<WaitSourceHandle, WaitError> {
        let mut sources = self.inner.sources.lock();
        if self.is_closed() {
            return Err(WaitError::HubClosed);
        }
        let handle = self.inner.hub.register_source(descriptor)?;
        let source_id = handle.source_id().clone();
        if exported_signal {
            self.inner.exported_signals.lock().insert(source_id.clone());
        }
        sources.insert(source_id, handle.clone());
        Ok(handle)
    }

    pub fn source(&self, name: &str) -> Option<WaitSourceHandle> {
        self.inner
            .sources
            .lock()
            .iter()
            .find(|(source_id, _)| source_id.as_str() == name)
            .map(|(_, handle)| handle.clone())
    }

    pub fn exported_signal_source(&self, name: &str) -> Option<WaitSourceId> {
        self.inner
            .exported_signals
            .lock()
            .iter()
            .find(|source_id| source_id.as_str() == name)
            .cloned()
    }

    pub fn source_names(&self) -> Vec<String> {
        self.inner
            .sources
            .lock()
            .keys()
            .map(|source_id| source_id.as_str().to_owned())
            .collect()
    }

    pub fn exported_signal_names(&self) -> Vec<String> {
        self.inner
            .exported_signals
            .lock()
            .iter()
            .map(|source_id| source_id.as_str().to_owned())
            .collect()
    }

    pub fn retain(&self, subscription: Subscription) {
        self.inner.subscriptions.add(subscription);
        if self.is_closed() {
            self.inner.subscriptions.unsubscribe_all();
        }
    }

    pub fn subscription_count(&self) -> usize {
        self.inner.subscriptions.len()
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.inner.subscriptions.unsubscribe_all();
        for source in self.inner.sources.lock().values() {
            source.close();
        }
    }
}

impl fmt::Debug for WaitRuntimeScope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metadata = self.metadata();
        formatter
            .debug_struct("WaitRuntimeScope")
            .field("project_id", &metadata.project_id())
            .field("session_id", &metadata.session_id())
            .field("hook_scope_id", &metadata.hook_scope_id())
            .field("source_names", &self.source_names())
            .field("exported_signal_names", &self.exported_signal_names())
            .field("subscription_count", &self.subscription_count())
            .field("closed", &self.is_closed())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WaitMatcher, WaitOwnerId, WaitQuery, WaitSourceSelection, WaitStatus};
    use serde_json::json;
    use std::time::{Duration, Instant};

    fn descriptor(name: &str) -> WaitSourceDescriptor {
        WaitSourceDescriptor::new(
            WaitSourceId::new(name).unwrap(),
            ["event", "value"],
            ["event"],
        )
        .unwrap()
    }

    #[test]
    fn scope_reuses_hub_and_enforces_exported_signal_allowlist() {
        let scope = WaitRuntimeScope::new(WaitRuntimeMetadata::new(
            Some("project".into()),
            Some("hook-scope".into()),
        ))
        .unwrap();
        let hub = scope.hub();
        assert!(scope.same_hub(&hub));
        scope
            .register_source(descriptor("app.change_set"), true)
            .unwrap();
        scope
            .register_source(descriptor("hook.audit"), false)
            .unwrap();
        assert!(scope.exported_signal_source("app.change_set").is_some());
        assert!(scope.exported_signal_source("hook.audit").is_none());
        assert!(scope.source("hook.audit").is_some());
    }

    #[test]
    fn dropping_scope_closes_sources_and_settles_background_wait() {
        let scope = WaitRuntimeScope::new(WaitRuntimeMetadata::default()).unwrap();
        let source = scope
            .register_source(descriptor("app.change_set"), true)
            .unwrap();
        let source_id = source.source_id().clone();
        drop(source);
        let hub = scope.hub();
        let owner = WaitOwnerId::new("owner").unwrap();
        let query = WaitQuery::after(
            WaitSourceSelection::source(source_id),
            WaitMatcher::empty(),
            hub.current_sequence(),
        );
        let task = hub
            .start_background(owner.clone(), query, Duration::from_secs(2))
            .unwrap();

        drop(scope);

        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let snapshot = hub.task_status(&owner, task).unwrap();
            if snapshot.status.is_terminal() {
                assert_eq!(snapshot.status, WaitStatus::SourceClosed);
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn debug_contains_metadata_but_never_event_payloads() {
        let scope = WaitRuntimeScope::new(WaitRuntimeMetadata::new(
            Some("project-id".into()),
            Some("scope-id".into()),
        ))
        .unwrap();
        scope.bind_session(Some("session-key".into()), Some("session-id".into()));
        let source = scope
            .register_source(descriptor("app.change_set"), true)
            .unwrap();
        source
            .publish(json!({"event": "commit", "value": "payload-secret-29fd"}))
            .unwrap();

        let debug = format!("{scope:?}");
        assert!(debug.contains("app.change_set"));
        assert!(debug.contains("scope-id"));
        assert!(!debug.contains("payload-secret-29fd"));
        assert!(!debug.contains("session-key"));
        assert!(!format!("{:?}", scope.metadata()).contains("session-key"));
    }
}
