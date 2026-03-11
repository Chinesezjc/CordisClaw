//! Hierarchical context registry with Cordis-style provide/inject/dispose.
//! Injection order: Local(current -> parents with grants) -> Request -> Session -> Global.

use crate::core::error::RuntimeError;
use crate::core::models::PluginLoadResult;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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
        self.inner.context_read_total.fetch_add(1, Ordering::Relaxed);
    }

    fn inc_write(&self) {
        self.inner.context_write_total.fetch_add(1, Ordering::Relaxed);
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

#[derive(Debug, Default, Clone)]
pub struct RuntimeContext {
    global: ScopeStore,
    session: ScopeStore,
    request: ScopeStore,
    local: BTreeMap<String, ScopeStore>,
    global_slots: BTreeMap<ContextKey, SlotEntry>,
    session_slots: BTreeMap<ContextKey, SlotEntry>,
    request_slots: BTreeMap<ContextKey, SlotEntry>,
    subgraph_overlays: BTreeMap<String, BTreeMap<ContextKey, Option<SlotEntry>>>,
    active_subgraph: Option<String>,
    session_version: u64,
    skipped_nodes: BTreeSet<String>,
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
        &mut self,
        key: ContextKey,
        value: T,
        meta: SlotMeta,
    ) -> Result<(), RuntimeError>;
    fn remove(&mut self, key: &ContextKey) -> Result<(), RuntimeError>;
    fn mark_skipped(&mut self, node_id: &str) -> Result<(), RuntimeError>;
}

pub trait ContextTxn {
    fn begin_subgraph(&mut self, subgraph_id: &str) -> Result<(), RuntimeError>;
    fn commit_overlay(&mut self, subgraph_id: &str) -> Result<(), RuntimeError>;
    fn rollback_overlay(&mut self, subgraph_id: &str) -> Result<(), RuntimeError>;
    fn commit_session(&mut self, session_id: &str, expected_version: u64)
        -> Result<(), RuntimeError>;
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
        self.session_version
    }

    pub fn metrics(&self) -> ContextMetricsSnapshot {
        self.metrics.snapshot()
    }

    pub fn skipped_nodes(&self) -> &BTreeSet<String> {
        &self.skipped_nodes
    }

    pub fn meta(&self, key: &ContextKey) -> Result<Option<SlotMeta>, RuntimeError> {
        Ok(self.lookup_slot_entry(key)?.map(|x| x.meta.clone()))
    }

    fn with_active_overlay_mut(
        &mut self,
    ) -> Option<&mut BTreeMap<ContextKey, Option<SlotEntry>>> {
        let active = self.active_subgraph.clone()?;
        self.subgraph_overlays.get_mut(&active)
    }

    fn lookup_slot_entry(&self, key: &ContextKey) -> Result<Option<&SlotEntry>, RuntimeError> {
        if let Some(active) = &self.active_subgraph {
            if let Some(overlay) = self.subgraph_overlays.get(active) {
                if let Some(delta) = overlay.get(key) {
                    return Ok(delta.as_ref());
                }
            }
        }

        if let Some(entry) = self.request_slots.get(key) {
            return Ok(Some(entry));
        }
        if let Some(entry) = self.session_slots.get(key) {
            return Ok(Some(entry));
        }
        if let Some(entry) = self.global_slots.get(key) {
            return Ok(Some(entry));
        }

        // Schema compatibility check: same namespace/name with different major version
        // should report incompatibility instead of silent miss.
        let requested_major = key.version / 100;
        for existing in self
            .request_slots
            .keys()
            .chain(self.session_slots.keys())
            .chain(self.global_slots.keys())
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
                return Err(RuntimeError::ContextPluginUnavailable {
                    plugin_path: path,
                });
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
                scope.provide(id, service, false).map_err(|_| RuntimeError::DuplicateService {
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
                self.local.get_mut(path).map(|x| x.remove(id)).unwrap_or(false)
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
        serde_json::from_value::<T>(entry.value.clone()).map(Some).map_err(|e| {
            RuntimeError::ContextDeserialize {
                key: key.as_compact(),
                message: e.to_string(),
            }
        })
    }

    fn contains(&self, key: &ContextKey) -> bool {
        self.metrics.inc_read();
        self.lookup_slot_entry(key).ok().flatten().is_some()
    }

    fn list_by_ns(&self, namespace: &str) -> Vec<ContextKey> {
        self.metrics.inc_read();
        let mut out = BTreeSet::new();
        for key in self
            .global_slots
            .keys()
            .chain(self.session_slots.keys())
            .chain(self.request_slots.keys())
        {
            if key.namespace == namespace {
                out.insert(key.clone());
            }
        }
        if let Some(active) = &self.active_subgraph {
            if let Some(overlay) = self.subgraph_overlays.get(active) {
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
        &mut self,
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
        if let Some(overlay) = self.with_active_overlay_mut() {
            overlay.insert(key, Some(entry));
        } else {
            self.request_slots.insert(key, entry);
        }
        Ok(())
    }

    fn remove(&mut self, key: &ContextKey) -> Result<(), RuntimeError> {
        self.metrics.inc_write();
        if let Some(overlay) = self.with_active_overlay_mut() {
            overlay.insert(key.clone(), None);
        } else {
            self.request_slots.remove(key);
        }
        Ok(())
    }

    fn mark_skipped(&mut self, node_id: &str) -> Result<(), RuntimeError> {
        self.skipped_nodes.insert(node_id.to_string());
        Ok(())
    }
}

impl ContextTxn for RuntimeContext {
    fn begin_subgraph(&mut self, subgraph_id: &str) -> Result<(), RuntimeError> {
        if let Some(current) = &self.active_subgraph {
            return Err(RuntimeError::SubgraphAlreadyActive {
                current: current.clone(),
            });
        }
        if self.subgraph_overlays.contains_key(subgraph_id) {
            return Err(RuntimeError::SubgraphAlreadyActive {
                current: subgraph_id.to_string(),
            });
        }
        self.subgraph_overlays
            .insert(subgraph_id.to_string(), BTreeMap::new());
        self.active_subgraph = Some(subgraph_id.to_string());
        Ok(())
    }

    fn commit_overlay(&mut self, subgraph_id: &str) -> Result<(), RuntimeError> {
        let Some(active) = &self.active_subgraph else {
            return Err(RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            });
        };
        if active != subgraph_id {
            return Err(RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            });
        }
        let overlay = self
            .subgraph_overlays
            .remove(subgraph_id)
            .ok_or_else(|| RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            })?;

        for (key, delta) in overlay {
            match delta {
                Some(entry) => {
                    self.request_slots.insert(key, entry);
                }
                None => {
                    self.request_slots.remove(&key);
                }
            }
        }
        self.active_subgraph = None;
        Ok(())
    }

    fn rollback_overlay(&mut self, subgraph_id: &str) -> Result<(), RuntimeError> {
        let Some(active) = &self.active_subgraph else {
            return Err(RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            });
        };
        if active != subgraph_id {
            return Err(RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            });
        }
        if self.subgraph_overlays.remove(subgraph_id).is_none() {
            return Err(RuntimeError::SubgraphNotFound {
                subgraph_id: subgraph_id.to_string(),
            });
        }
        self.active_subgraph = None;
        self.metrics.inc_overlay_rollback();
        Ok(())
    }

    fn commit_session(
        &mut self,
        session_id: &str,
        expected_version: u64,
    ) -> Result<(), RuntimeError> {
        let started_at = Instant::now();
        if self.session_version != expected_version {
            self.metrics.inc_commit_conflict();
            return Err(RuntimeError::CommitConflict {
                session_id: session_id.to_string(),
                expected_version,
                actual_version: self.session_version,
            });
        }
        for (key, value) in &self.request_slots {
            self.session_slots.insert(key.clone(), value.clone());
        }
        self.request_slots.clear();
        self.session_version += 1;
        self.metrics
            .add_commit_latency_ms(started_at.elapsed().as_millis().try_into().unwrap_or(u64::MAX));
        Ok(())
    }
}
