//! Hierarchical context registry with Cordis-style provide/inject/dispose.
//! Injection order: Local(current -> parents with grants) -> Request -> Session -> Global.

use crate::core::error::RuntimeError;
use crate::core::models::PluginLoadResult;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextScope {
    /// Process-level shared scope.
    Global,
    /// Session-level reusable scope.
    Session,
    /// Request-level transient scope.
    Request,
    /// Plugin-local scope (per plugin_path).
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContextKey {
    pub namespace: String,
    pub name: String,
    pub version: u32,
}

impl ContextKey {
    pub fn as_compact(&self) -> String {
        format!("{}/{}@v{}", self.namespace, self.name, self.version)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Low,
    Internal,
    Sensitive,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SlotMeta {
    pub required: bool,
    pub ttl_ms: Option<u64>,
    pub sensitivity: Sensitivity,
    pub owner: String,
}

#[derive(Debug, Clone)]
struct SlotEntry {
    value: serde_json::Value,
    meta: SlotMeta,
}

#[derive(Debug, Default, Clone)]
struct ScopeStore {
    /// Heterogeneous service container keyed by service id.
    services: BTreeMap<String, Arc<dyn Any + Send + Sync>>,
}

impl ScopeStore {
    fn provide<T: Send + Sync + 'static>(
        &mut self,
        id: &str,
        service: T,
        allow_override: bool,
    ) -> Result<(), RuntimeError> {
        // Default behavior is fail-fast on duplicates.
        if self.services.contains_key(id) && !allow_override {
            return Err(RuntimeError::DuplicateService {
                plugin_path: "<scope>".to_string(),
                service: id.to_string(),
            });
        }
        self.services.insert(id.to_string(), Arc::new(service));
        Ok(())
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Any + Send + Sync>> {
        self.services.get(id).cloned()
    }

    fn remove(&mut self, id: &str) -> bool {
        self.services.remove(id).is_some()
    }
}

#[derive(Debug, Default, Clone)]
pub struct PluginHierarchy {
    /// child -> parent mapping
    pub parent_of: BTreeMap<String, String>,
    /// child -> grants inherited from direct parent edge
    pub grants_from_parent: BTreeMap<String, BTreeSet<String>>, // key=child path
}

#[derive(Debug, Default)]
struct ContextMetricsInner {
    context_read_total: AtomicU64,
    context_write_total: AtomicU64,
    context_overlay_rollback_total: AtomicU64,
    session_commit_conflict_total: AtomicU64,
    session_commit_latency_ms: AtomicU64,
}

#[derive(Debug, Default, Clone)]
pub struct ContextMetrics {
    inner: Arc<ContextMetricsInner>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ContextMetricsSnapshot {
    pub context_read_total: u64,
    pub context_write_total: u64,
    pub context_overlay_rollback_total: u64,
    pub session_commit_conflict_total: u64,
    pub session_commit_latency_ms: u64,
}

impl ContextMetrics {
    fn inc_read(&self) {
        self.inner
            .context_read_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_write(&self) {
        self.inner
            .context_write_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_overlay_rollback(&self) {
        self.inner
            .context_overlay_rollback_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn inc_commit_conflict(&self) {
        self.inner
            .session_commit_conflict_total
            .fetch_add(1, Ordering::Relaxed);
    }

    fn add_commit_latency_ms(&self, elapsed_ms: u64) {
        self.inner
            .session_commit_latency_ms
            .fetch_add(elapsed_ms, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> ContextMetricsSnapshot {
        ContextMetricsSnapshot {
            context_read_total: self.inner.context_read_total.load(Ordering::Relaxed),
            context_write_total: self.inner.context_write_total.load(Ordering::Relaxed),
            context_overlay_rollback_total: self
                .inner
                .context_overlay_rollback_total
                .load(Ordering::Relaxed),
            session_commit_conflict_total: self
                .inner
                .session_commit_conflict_total
                .load(Ordering::Relaxed),
            session_commit_latency_ms: self.inner.session_commit_latency_ms.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug)]
pub struct RuntimeContext {
    global: ScopeStore,
    session: ScopeStore,
    request: ScopeStore,
    local: BTreeMap<String, ScopeStore>,
    // Slot maps use Arc<Mutex<>> for thread-safe interior mutability,
    // enabling &self writes from parallel runner closures.
    #[allow(clippy::type_complexity)]
    global_slots: Arc<Mutex<BTreeMap<ContextKey, SlotEntry>>>,
    #[allow(clippy::type_complexity)]
    session_slots: Arc<Mutex<BTreeMap<ContextKey, SlotEntry>>>,
    #[allow(clippy::type_complexity)]
    request_slots: Arc<Mutex<BTreeMap<ContextKey, SlotEntry>>>,
    #[allow(clippy::type_complexity)]
    subgraph_overlays:
        Arc<Mutex<BTreeMap<String, BTreeMap<ContextKey, Option<SlotEntry>>>>>,
    active_subgraph: Arc<Mutex<Option<String>>>,
    session_version: AtomicU64,
    skipped_nodes: Arc<Mutex<BTreeSet<String>>>,
    hierarchy: PluginHierarchy,
    /// Plugin availability snapshot; Unavailable plugin cannot be injected from.
    plugin_state: BTreeMap<String, PluginLoadResult>,
    metrics: ContextMetrics,
}

pub trait ContextRegistry {
    /// Register a service into the chosen scope.
    fn provide<T: Send + Sync + 'static>(
        &mut self,
        scope: ContextScope,
        plugin_path: Option<&str>,
        id: &str,
        service: T,
    ) -> Result<(), RuntimeError>;

    /// Resolve a typed service by id following the full lookup chain.
    fn inject<T: Send + Sync + 'static>(
        &self,
        plugin_path: &str,
        id: &str,
    ) -> Result<Arc<T>, RuntimeError>;

    /// Optional form of `inject`.
    fn maybe<T: Send + Sync + 'static>(&self, plugin_path: &str, id: &str) -> Option<Arc<T>>;

    /// Remove a service from scope.
    fn dispose(
        &mut self,
        scope: ContextScope,
        plugin_path: Option<&str>,
        id: &str,
    ) -> Result<(), RuntimeError>;
}

pub trait ContextRead {
    fn get<T: DeserializeOwned>(&self, key: &ContextKey) -> Result<Option<T>, RuntimeError>;
    fn contains(&self, key: &ContextKey) -> bool;
    fn list_by_ns(&self, namespace: &str) -> Vec<ContextKey>;
}

pub trait ContextWrite {
    fn put<T: Serialize>(
        &self,
        key: ContextKey,
        value: T,
        meta: SlotMeta,
    ) -> Result<(), RuntimeError>;
    fn remove(&self, key: &ContextKey) -> Result<(), RuntimeError>;
    fn mark_skipped(&self, node_id: &str) -> Result<(), RuntimeError>;
}

pub trait ContextTxn {
    fn begin_subgraph(&self, subgraph_id: &str) -> Result<(), RuntimeError>;
    fn commit_overlay(&self, subgraph_id: &str) -> Result<(), RuntimeError>;
    fn rollback_overlay(&self, subgraph_id: &str) -> Result<(), RuntimeError>;
    fn commit_session(
        &self,
        session_id: &str,
        expected_version: u64,
    ) -> Result<(), RuntimeError>;
}

impl Default for RuntimeContext {
    fn default() -> Self {
        Self {
            global: ScopeStore::default(),
            session: ScopeStore::default(),
            request: ScopeStore::default(),
            local: BTreeMap::new(),
            global_slots: Arc::new(Mutex::new(BTreeMap::new())),
            session_slots: Arc::new(Mutex::new(BTreeMap::new())),
            request_slots: Arc::new(Mutex::new(BTreeMap::new())),
            subgraph_overlays: Arc::new(Mutex::new(BTreeMap::new())),
            active_subgraph: Arc::new(Mutex::new(None)),
            session_version: AtomicU64::new(0),
            skipped_nodes: Arc::new(Mutex::new(BTreeSet::new())),
            hierarchy: PluginHierarchy::default(),
            plugin_state: BTreeMap::new(),
            metrics: ContextMetrics::default(),
        }
    }
}

impl Clone for RuntimeContext {
    fn clone(&self) -> Self {
        Self {
            global: self.global.clone(),
            session: self.session.clone(),
            request: self.request.clone(),
            local: self.local.clone(),
            global_slots: Arc::new(Mutex::new(
                self.global_slots.lock().unwrap().clone(),
            )),
            session_slots: Arc::new(Mutex::new(
                self.session_slots.lock().unwrap().clone(),
            )),
            request_slots: Arc::new(Mutex::new(
                self.request_slots.lock().unwrap().clone(),
            )),
            subgraph_overlays: Arc::new(Mutex::new(
                self.subgraph_overlays.lock().unwrap().clone(),
            )),
            active_subgraph: Arc::new(Mutex::new(
                self.active_subgraph.lock().unwrap().clone(),
            )),
            session_version: AtomicU64::new(self.session_version.load(Ordering::SeqCst)),
            skipped_nodes: Arc::new(Mutex::new(
                self.skipped_nodes.lock().unwrap().clone(),
            )),
            hierarchy: self.hierarchy.clone(),
            plugin_state: self.plugin_state.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

impl RuntimeContext {
    pub fn with_hierarchy(hierarchy: PluginHierarchy) -> Self {
        Self {
            hierarchy,
            ..Self::default()
        }
    }

    pub fn set_plugin_state(&mut self, plugin_path: &str, state: PluginLoadResult) {
        self.plugin_state.insert(plugin_path.to_string(), state);
    }

    pub fn ensure_local_scope(&mut self, plugin_path: &str) {
        self.local.entry(plugin_path.to_string()).or_default();
    }

    pub fn session_version(&self) -> u64 {
        self.session_version.load(Ordering::SeqCst)
    }

    pub fn metrics(&self) -> ContextMetricsSnapshot {
        self.metrics.snapshot()
    }

    /// Return a snapshot of currently skipped node ids.
    pub fn skipped_nodes(&self) -> BTreeSet<String> {
        self.skipped_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn meta(&self, key: &ContextKey) -> Result<Option<SlotMeta>, RuntimeError> {
        Ok(self.lookup_slot_entry(key)?.map(|x| x.meta.clone()))
    }

    fn lookup_slot_entry(&self, key: &ContextKey) -> Result<Option<SlotEntry>, RuntimeError> {
        let active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(active_id) = active.as_ref() {
            let overlays = self
                .subgraph_overlays
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(overlay) = overlays.get(active_id) {
                if let Some(delta) = overlay.get(key) {
                    return Ok(delta.clone());
                }
            }
        }

        let request = self
            .request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(entry) = request.get(key) {
            return Ok(Some(entry.clone()));
        }
        let session = self
            .session_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(entry) = session.get(key) {
            return Ok(Some(entry.clone()));
        }
        let global = self
            .global_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(entry) = global.get(key) {
            return Ok(Some(entry.clone()));
        }

        // Schema compatibility check.
        let requested_major = key.version / 100;
        for existing in request
            .keys()
            .chain(session.keys())
            .chain(global.keys())
        {
            if existing.namespace == key.namespace && existing.name == key.name {
                let existing_major = existing.version / 100;
                if existing_major != requested_major {
                    return Err(RuntimeError::ContextVersionIncompatible {
                        key: key.as_compact(),
                        expected: key.version,
                        actual: existing.version,
                    });
                }
            }
        }
        Ok(None)
    }

    fn cast_arc<T: Send + Sync + 'static>(
        plugin_path: &str,
        id: &str,
        value: Arc<dyn Any + Send + Sync>,
    ) -> Result<Arc<T>, RuntimeError> {
        // Type-safe downcast with structured error instead of panic.
        Arc::downcast::<T>(value).map_err(|_| RuntimeError::ServiceTypeMismatch {
            plugin_path: plugin_path.to_string(),
            service: id.to_string(),
        })
    }

    fn inject_local_chain<T: Send + Sync + 'static>(
        &self,
        plugin_path: &str,
        id: &str,
    ) -> Result<Option<Arc<T>>, RuntimeError> {
        // Walk Local(current) -> Local(parent...) and enforce grants at each parent hop.
        let mut current = Some(plugin_path.to_string());
        let mut child_for_grant = plugin_path.to_string();

        while let Some(path) = current {
            if matches!(
                self.plugin_state.get(&path),
                Some(PluginLoadResult::Unavailable(_))
            ) {
                // Parent/local unavailable should fail explicitly, not silently skip.
                return Err(RuntimeError::ContextPluginUnavailable { plugin_path: path });
            }

            if let Some(scope) = self.local.get(&path) {
                if let Some(raw) = scope.get(id) {
                    if path != plugin_path {
                        // Accessing parent local service requires explicit grant on edge.
                        let allowed = self
                            .hierarchy
                            .grants_from_parent
                            .get(&child_for_grant)
                            .map(|x| x.contains(id))
                            .unwrap_or(false);
                        if !allowed {
                            return Err(RuntimeError::PermissionDenied {
                                plugin_path: plugin_path.to_string(),
                                service: id.to_string(),
                            });
                        }
                    }
                    return Self::cast_arc(plugin_path, id, raw).map(Some);
                }
            }

            current = self.hierarchy.parent_of.get(&path).cloned();
            child_for_grant = path;
        }

        Ok(None)
    }
}

impl ContextRegistry for RuntimeContext {
    fn provide<T: Send + Sync + 'static>(
        &mut self,
        scope: ContextScope,
        plugin_path: Option<&str>,
        id: &str,
        service: T,
    ) -> Result<(), RuntimeError> {
        match scope {
            ContextScope::Global => self.global.provide(id, service, false),
            ContextScope::Session => self.session.provide(id, service, false),
            ContextScope::Request => self.request.provide(id, service, false),
            ContextScope::Local => {
                let path = plugin_path.ok_or_else(|| RuntimeError::Invariant {
                    message: "local scope provide requires plugin_path".to_string(),
                })?;
                let scope = self.local.entry(path.to_string()).or_default();
                scope
                    .provide(id, service, false)
                    .map_err(|_| RuntimeError::DuplicateService {
                        plugin_path: path.to_string(),
                        service: id.to_string(),
                    })
            }
        }
    }

    fn inject<T: Send + Sync + 'static>(
        &self,
        plugin_path: &str,
        id: &str,
    ) -> Result<Arc<T>, RuntimeError> {
        if matches!(
            self.plugin_state.get(plugin_path),
            Some(PluginLoadResult::Unavailable(_))
        ) {
            return Err(RuntimeError::ContextPluginUnavailable {
                plugin_path: plugin_path.to_string(),
            });
        }

        // Priority order is fixed for deterministic behavior.
        if let Some(local_hit) = self.inject_local_chain(plugin_path, id)? {
            return Ok(local_hit);
        }

        if let Some(req) = self.request.get(id) {
            return Self::cast_arc(plugin_path, id, req);
        }

        if let Some(sess) = self.session.get(id) {
            return Self::cast_arc(plugin_path, id, sess);
        }

        if let Some(global) = self.global.get(id) {
            return Self::cast_arc(plugin_path, id, global);
        }

        Err(RuntimeError::ServiceNotFound {
            plugin_path: plugin_path.to_string(),
            service: id.to_string(),
        })
    }

    fn maybe<T: Send + Sync + 'static>(&self, plugin_path: &str, id: &str) -> Option<Arc<T>> {
        self.inject(plugin_path, id).ok()
    }

    fn dispose(
        &mut self,
        scope: ContextScope,
        plugin_path: Option<&str>,
        id: &str,
    ) -> Result<(), RuntimeError> {
        let removed = match scope {
            ContextScope::Global => self.global.remove(id),
            ContextScope::Session => self.session.remove(id),
            ContextScope::Request => self.request.remove(id),
            ContextScope::Local => {
                let path = plugin_path.ok_or_else(|| RuntimeError::Invariant {
                    message: "local scope dispose requires plugin_path".to_string(),
                })?;
                self.local
                    .get_mut(path)
                    .map(|x| x.remove(id))
                    .unwrap_or(false)
            }
        };

        if removed {
            Ok(())
        } else {
            Err(RuntimeError::ServiceNotFound {
                plugin_path: plugin_path.unwrap_or("<scope>").to_string(),
                service: id.to_string(),
            })
        }
    }
}

impl ContextRead for RuntimeContext {
    fn get<T: DeserializeOwned>(&self, key: &ContextKey) -> Result<Option<T>, RuntimeError> {
        self.metrics.inc_read();
        let Some(entry) = self.lookup_slot_entry(key)? else {
            return Ok(None);
        };
        serde_json::from_value::<T>(entry.value.clone())
            .map(Some)
            .map_err(|e| RuntimeError::ContextDeserialize {
                key: key.as_compact(),
                message: e.to_string(),
            })
    }

    fn contains(&self, key: &ContextKey) -> bool {
        self.metrics.inc_read();
        self.lookup_slot_entry(key).ok().flatten().is_some()
    }

    fn list_by_ns(&self, namespace: &str) -> Vec<ContextKey> {
        self.metrics.inc_read();
        let mut out = BTreeSet::new();
        let global = self
            .global_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session = self
            .session_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let request = self
            .request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for key in global.keys().chain(session.keys()).chain(request.keys()) {
            if key.namespace == namespace {
                out.insert(key.clone());
            }
        }
        drop(request);
        drop(session);
        drop(global);
        let active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(active_id) = active.as_ref() {
            let overlays = self
                .subgraph_overlays
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(overlay) = overlays.get(active_id) {
                for (key, delta) in overlay {
                    if key.namespace == namespace {
                        if delta.is_some() {
                            out.insert(key.clone());
                        } else {
                            out.remove(key);
                        }
                    }
                }
            }
        }
        out.into_iter().collect()
    }
}

impl ContextWrite for RuntimeContext {
    fn put<T: Serialize>(
        &self,
        key: ContextKey,
        value: T,
        meta: SlotMeta,
    ) -> Result<(), RuntimeError> {
        self.metrics.inc_write();
        let value = serde_json::to_value(value).map_err(|e| RuntimeError::ContextSerialize {
            key: key.as_compact(),
            message: e.to_string(),
        })?;
        let entry = SlotEntry { value, meta };
        let active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(active_id) = active.as_ref() {
            let mut overlays = self
                .subgraph_overlays
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let overlay = overlays
                .get_mut(active_id)
                .expect("active subgraph overlay must exist");
            overlay.insert(key, Some(entry));
        } else {
            self.request_slots
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(key, entry);
        }
        Ok(())
    }

    fn remove(&self, key: &ContextKey) -> Result<(), RuntimeError> {
        self.metrics.inc_write();
        let active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(active_id) = active.as_ref() {
            let mut overlays = self
                .subgraph_overlays
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let overlay = overlays
                .get_mut(active_id)
                .expect("active subgraph overlay must exist");
            overlay.insert(key.clone(), None);
        } else {
            self.request_slots
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(key);
        }
        Ok(())
    }

    fn mark_skipped(&self, node_id: &str) -> Result<(), RuntimeError> {
        self.skipped_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(node_id.to_string());
        Ok(())
    }
}

impl ContextTxn for RuntimeContext {
    fn begin_subgraph(&self, subgraph_id: &str) -> Result<(), RuntimeError> {
        let mut active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(current) = active.as_ref() {
            return Err(RuntimeError::SubgraphAlreadyActive {
                current: current.clone(),
            });
        }
        let mut overlays = self
            .subgraph_overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if overlays.contains_key(subgraph_id) {
            return Err(RuntimeError::SubgraphAlreadyActive {
                current: subgraph_id.to_string(),
            });
        }
        overlays.insert(subgraph_id.to_string(), BTreeMap::new());
        *active = Some(subgraph_id.to_string());
        Ok(())
    }

    fn commit_overlay(&self, subgraph_id: &str) -> Result<(), RuntimeError> {
        let mut active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match active.as_ref() {
            Some(current) if current == subgraph_id => {}
            _ => {
                return Err(RuntimeError::SubgraphNotFound {
                    subgraph_id: subgraph_id.to_string(),
                });
            }
        }
        let mut overlays = self
            .subgraph_overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let overlay = overlays.remove(subgraph_id).ok_or_else(|| {
            RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            }
        })?;
        *active = None;
        drop(active);
        drop(overlays);

        let mut request = self
            .request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (key, delta) in overlay {
            match delta {
                Some(entry) => {
                    request.insert(key, entry);
                }
                None => {
                    request.remove(&key);
                }
            }
        }
        Ok(())
    }

    fn rollback_overlay(&self, subgraph_id: &str) -> Result<(), RuntimeError> {
        let mut active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        match active.as_ref() {
            Some(current) if current == subgraph_id => {}
            _ => {
                return Err(RuntimeError::SubgraphNotFound {
                    subgraph_id: subgraph_id.to_string(),
                });
            }
        }
        let mut overlays = self
            .subgraph_overlays
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if overlays.remove(subgraph_id).is_none() {
            return Err(RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            });
        }
        *active = None;
        drop(active);
        drop(overlays);
        self.metrics.inc_overlay_rollback();
        Ok(())
    }

    fn commit_session(
        &self,
        session_id: &str,
        expected_version: u64,
    ) -> Result<(), RuntimeError> {
        // session_version is not behind a Mutex — it's only accessed from
        // the main thread in the engine.  For parallel safety, it should
        // eventually be AtomicU64, but for now the single-threaded path
        // is sufficient.
        // Safety: this is a &self method; the caller must ensure external
        // synchronization if called from multiple threads.
        let started_at = Instant::now();
        let current_version = self.session_version.load(Ordering::SeqCst);
        if current_version != expected_version {
            self.metrics.inc_commit_conflict();
            return Err(RuntimeError::CommitConflict {
                session_id: session_id.to_string(),
                expected_version,
                actual_version: current_version,
            });
        }
        let request = self
            .request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut session = self
            .session_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for (key, value) in request.iter() {
            session.insert(key.clone(), value.clone());
        }
        drop(request);
        self.request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clear();
        self.session_version.fetch_add(1, Ordering::SeqCst);
        self.metrics.add_commit_latency_ms(
            started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        );
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Service lifecycle — Task nodes that run as background threads.
// ---------------------------------------------------------------------------

/// A long-running background service attached to a plugin `Task` node.
///
/// Implementations should return quickly from `start()` after spawning any
/// worker threads.  `stop()` should signal shutdown and join threads.
pub trait Service: Send + Sync {
    /// Start the service.  Called when the owning plugin is loaded.
    fn start(&self) -> Result<(), String>;
    /// Signal the service to stop and wait for workers to exit.
    fn stop(&self) -> Result<(), String>;
}

/// A named service handle that tracks running state.
#[allow(dead_code)]
struct ServiceEntry {
    name: String,
    plugin_path: String,
    svc: Box<dyn Service>,
    running: AtomicBool,
}

/// Registry of background services, keyed by `"plugin_path::node_id"`.
pub struct ServiceRegistry {
    entries: Mutex<BTreeMap<String, ServiceEntry>>,
}

impl std::fmt::Debug for ServiceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.len();
        f.debug_struct("ServiceRegistry")
            .field("service_count", &len)
            .finish()
    }
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
        }
    }

    /// Register and immediately start a service for a plugin Task node.
    pub fn start_service(
        &self,
        plugin_path: &str,
        node_id: &str,
        svc: Box<dyn Service>,
    ) -> Result<(), RuntimeError> {
        let key = format!("{plugin_path}::{node_id}");
        let entry = ServiceEntry {
            name: node_id.to_string(),
            plugin_path: plugin_path.to_string(),
            svc,
            running: AtomicBool::new(false),
        };
        if let Err(e) = entry.svc.start() {
            return Err(RuntimeError::Invariant {
                message: format!("service {key} failed to start: {e}"),
            });
        }
        entry.running.store(true, Ordering::SeqCst);
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.contains_key(&key) {
            return Err(RuntimeError::DuplicateService {
                plugin_path: plugin_path.to_string(),
                service: key,
            });
        }
        guard.insert(key, entry);
        Ok(())
    }

    /// Stop all services belonging to `plugin_path` (and its descendants).
    pub fn stop_plugin_services(&self, plugin_path: &str) {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let keys: Vec<String> = guard
            .keys()
            .filter(|k| k.starts_with(plugin_path))
            .cloned()
            .collect();
        for key in keys {
            if let Some(entry) = guard.remove(&key) {
                entry.running.store(false, Ordering::SeqCst);
                if let Err(e) = entry.svc.stop() {
                    eprintln!("service {key} stop error: {e}");
                }
            }
        }
    }

    /// Stop and remove all registered services.
    pub fn stop_all(&self) {
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let drained: BTreeMap<_, _> = std::mem::take(&mut *guard);
        for (key, entry) in drained {
            entry.running.store(false, Ordering::SeqCst);
            if let Err(e) = entry.svc.stop() {
                eprintln!("service {key} stop error: {e}");
            }
        }
    }

    /// Return the number of registered (running) services.
    pub fn len(&self) -> usize {
        self.entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }
}

impl Drop for ServiceRegistry {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct CounterService {
        starts: AtomicUsize,
        stops: AtomicUsize,
    }

    impl CounterService {
        fn new() -> Self {
            Self {
                starts: AtomicUsize::new(0),
                stops: AtomicUsize::new(0),
            }
        }
    }

    impl Service for CounterService {
        fn start(&self) -> Result<(), String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn stop(&self) -> Result<(), String> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn service_start_stop_lifecycle() {
        let registry = ServiceRegistry::new();
        let svc = CounterService::new();
        registry
            .start_service("test/plugin", "bg_worker", Box::new(svc))
            .expect("start");

        assert_eq!(registry.len(), 1);

        registry.stop_plugin_services("test/plugin");

        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn service_stop_subtree() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("root", "svc_a", Box::new(CounterService::new()))
            .expect("start");
        registry
            .start_service("root/child", "svc_b", Box::new(CounterService::new()))
            .expect("start");
        registry
            .start_service("other", "svc_c", Box::new(CounterService::new()))
            .expect("start");
        assert_eq!(registry.len(), 3);

        // Stopping "root" should stop root and root/child, but not "other".
        registry.stop_plugin_services("root");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn duplicate_service_rejected() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("root", "dup", Box::new(CounterService::new()))
            .expect("start");
        let err = registry
            .start_service("root", "dup", Box::new(CounterService::new()))
            .expect_err("should reject");
        assert!(err.to_string().contains("dup"));
    }
}
