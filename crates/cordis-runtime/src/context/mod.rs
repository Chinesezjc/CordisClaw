//! Hierarchical context registry with Cordis-style provide/inject/dispose.
//! Injection order: Local(current -> parents with grants) -> Request -> Session -> Global.

use crate::core::error::RuntimeError;
use crate::core::models::PluginLoadResult;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
    subgraph_overlays: Arc<Mutex<BTreeMap<String, BTreeMap<ContextKey, Option<SlotEntry>>>>>,
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
    fn commit_session(&self, session_id: &str, expected_version: u64) -> Result<(), RuntimeError>;
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

/// 一致性修复：快照 `(active, overlays)` 必须满足不变量
/// `active == Some(id) ⟹ overlays 含 id`。clone 在两个互斥量上的读取之间若
/// 被并发事务撕裂（如 commit 已删 overlay 而未清 active），把不一致状态
/// 归一为 `(None, 空)`，避免克隆体后续 put/remove/commit 触发
/// "active subgraph overlay must exist" 断言崩溃。提取为纯函数以便直接
/// 单测撕裂输入。
type SubgraphOverlayMap = BTreeMap<String, BTreeMap<ContextKey, Option<SlotEntry>>>;
fn repaired_subgraph_state(
    active: Option<String>,
    overlays: SubgraphOverlayMap,
) -> (Option<String>, SubgraphOverlayMap) {
    let Some(id) = active else {
        return (None, BTreeMap::new());
    };
    let Some(overlay) = overlays.get(&id).cloned() else {
        return (None, BTreeMap::new());
    };
    let mut keep = BTreeMap::new();
    keep.insert(id, overlay);
    (keep.keys().next().cloned(), keep)
}

impl Clone for RuntimeContext {
    fn clone(&self) -> Self {
        // 快照 subgraph 状态时按事务同序（active → overlays）加锁并做一致性
        // 修复：此前按 overlays → active 反序加锁（可与 begin/commit/put/
        // remove 互相死锁）且两次加锁间存在撕裂窗口（可克隆出 active=Some
        // 而 overlay 缺失的快照，随后 put/remove 触发 expect 崩溃）。
        let (active_subgraph, subgraph_overlays) = {
            let active = self
                .active_subgraph
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let overlays = self
                .subgraph_overlays
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            repaired_subgraph_state(active.clone(), overlays.clone())
        };
        Self {
            global: self.global.clone(),
            session: self.session.clone(),
            request: self.request.clone(),
            local: self.local.clone(),
            global_slots: Arc::new(Mutex::new(
                self.global_slots
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone(),
            )),
            session_slots: Arc::new(Mutex::new(
                self.session_slots
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone(),
            )),
            request_slots: Arc::new(Mutex::new(
                self.request_slots
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone(),
            )),
            subgraph_overlays: Arc::new(Mutex::new(subgraph_overlays)),
            active_subgraph: Arc::new(Mutex::new(active_subgraph)),
            session_version: AtomicU64::new(self.session_version.load(Ordering::SeqCst)),
            skipped_nodes: Arc::new(Mutex::new(
                self.skipped_nodes
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .clone(),
            )),
            hierarchy: self.hierarchy.clone(),
            plugin_state: self.plugin_state.clone(),
            metrics: self.metrics.clone(),
        }
    }
}

/// Build the version-mismatch error for a requested `key` against an existing
/// slot's `actual` version. Extracted so the caller's guard stays a single line.
fn version_incompatible(key: &ContextKey, actual: u32) -> RuntimeError {
    RuntimeError::ContextVersionIncompatible {
        key: key.as_compact(),
        expected: key.version,
        actual,
    }
}

impl RuntimeContext {
    /// 从 request → session → global 整条查找链删除 `key`（与 lookup 同序
    /// 加锁，避免死锁）。此前只删 request 层：commit_session 会把 request
    /// 提升进 session，仅删 request 会让 session/global 承载的键"复活"。
    fn remove_slot_across_layers(&self, key: &ContextKey) {
        self.request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key);
        self.session_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key);
        self.global_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(key);
    }

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
            let overlay_hit = overlays.get(active_id).and_then(|overlay| overlay.get(key));
            if let Some(delta) = overlay_hit {
                return Ok(delta.clone());
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
        for existing in request.keys().chain(session.keys()).chain(global.keys()) {
            let same_slot = existing.namespace == key.namespace && existing.name == key.name;
            let major_conflict = existing.version / 100 != requested_major;
            if same_slot && major_conflict {
                return Err(version_incompatible(key, existing.version));
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
        // Walk Local(current) -> Local(parent...) and enforce grants at each
        // ancestor hit. Authorization ALWAYS uses the original requester's own
        // grant set: `grants_from_parent[plugin_path]` is the grant written on
        // the requester's parent edge, and the same set gates every ancestor
        // hop. So a depth-1 lookup checks the requester's parent edge (the
        // child_for_grant behavior was already == plugin_path there), and a
        // depth>=2 lookup (c -> b -> a) requires requester c itself to hold
        // the grant on its own c -> b edge — a grant on an intermediate edge
        // (b -> a) never authorizes c.
        let mut current = Some(plugin_path.to_string());

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
                        // Accessing an ancestor local service requires an
                        // explicit grant on the requester's own parent edge.
                        let allowed = self
                            .hierarchy
                            .grants_from_parent
                            .get(plugin_path)
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
                let removed = self
                    .local
                    .get_mut(path)
                    .map(|x| x.remove(id))
                    .unwrap_or(false);
                // 空作用域即回收：否则插件反复加载/卸载后空 ScopeStore 在
                // local map 里无限累积。
                if removed
                    && self
                        .local
                        .get(path)
                        .map(|x| x.services.is_empty())
                        .unwrap_or(false)
                {
                    self.local.remove(path);
                }
                removed
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
        // P1-2: canonical lock order — active → overlays → request → session
        // → global (mirrors `lookup_slot_entry`). Previously this method
        // acquired global→session→request→active→overlays; two parallel
        // executor workers, one calling list_by_ns and one calling
        // lookup_slot_entry, could deadlock on the reverse-order chain.
        let mut out = BTreeSet::new();
        let active = self
            .active_subgraph
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let active_id = active.clone();
        drop(active);
        let overlay_snapshot: Option<BTreeMap<ContextKey, Option<SlotEntry>>> =
            if let Some(id) = active_id.as_deref() {
                let overlays = self
                    .subgraph_overlays
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                overlays.get(id).cloned()
            } else {
                None
            };

        let request = self
            .request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for key in request.keys() {
            if key.namespace == namespace {
                out.insert(key.clone());
            }
        }
        drop(request);
        let session = self
            .session_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for key in session.keys() {
            if key.namespace == namespace {
                out.insert(key.clone());
            }
        }
        drop(session);
        let global = self
            .global_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for key in global.keys() {
            if key.namespace == namespace {
                out.insert(key.clone());
            }
        }
        drop(global);

        if let Some(overlay) = overlay_snapshot {
            for (key, delta) in overlay {
                if key.namespace != namespace {
                    continue;
                }
                if delta.is_some() {
                    out.insert(key.clone());
                } else {
                    out.remove(&key);
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
            self.remove_slot_across_layers(key);
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
        // Invariant: `active` is held locked for this whole method and is
        // mutated in lockstep with `overlays` (set here, cleared on
        // commit/rollback which also remove the overlay). So reaching here
        // with `active == None` means no overlay for any id exists. Asserted
        // rather than returned because it is structurally unreachable; the
        // `active.as_ref()` guard above already rejects a genuine
        // double-begin with `SubgraphAlreadyActive`.
        debug_assert!(
            !overlays.contains_key(subgraph_id),
            "overlay for {subgraph_id} exists while no subgraph is active"
        );
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
        // Invariant: the match above proved `active == Some(subgraph_id)`, and
        // `active` is set/cleared in lockstep with overlay insert/remove while
        // holding the `active` lock. So an active id always has its overlay.
        // `.expect` (not a returned error) makes this structurally unreachable
        // guard consistent with the sibling assertion in `put`/`remove`.
        let overlay = overlays
            .remove(subgraph_id)
            .expect("active subgraph overlay must exist");
        *active = None;
        drop(active);
        drop(overlays);

        let mut request = self
            .request_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut tombstones = Vec::new();
        for (key, delta) in overlay {
            match delta {
                Some(entry) => {
                    request.insert(key, entry);
                }
                None => {
                    request.remove(&key);
                    tombstones.push(key);
                }
            }
        }
        drop(request);
        // 墓碑删除须覆盖 session/global 层：commit_session 会把 request
        // 提升进 session，仅删 request 会让已提交的键"复活"。
        if !tombstones.is_empty() {
            let mut session = self
                .session_slots
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut global = self
                .global_slots
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            for key in tombstones {
                session.remove(&key);
                global.remove(&key);
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
        // Invariant (same as `commit_overlay`): an active subgraph id always
        // has a live overlay, since the two are mutated together under the
        // `active` lock. Asserted rather than returned as `SubgraphNotFound`
        // because the state is structurally unreachable.
        let removed = overlays.remove(subgraph_id).is_some();
        debug_assert!(
            removed,
            "active subgraph {subgraph_id} has no overlay to roll back"
        );
        *active = None;
        drop(active);
        drop(overlays);
        self.metrics.inc_overlay_rollback();
        Ok(())
    }

    fn commit_session(&self, session_id: &str, expected_version: u64) -> Result<(), RuntimeError> {
        // P1-1: single-step CAS on session_version. Previously this method
        // did `load` → work → `fetch_add`, so two threads at the same
        // `expected_version` could both pass the equality check and both
        // fetch-add — a lost-update bug that only showed up under the
        // parallel executor. compare_exchange atomically claims the version
        // slot; on failure the caller retries at the new version.
        let started_at = Instant::now();
        if self
            .session_version
            .compare_exchange(
                expected_version,
                expected_version + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            let actual = self.session_version.load(Ordering::Acquire);
            self.metrics.inc_commit_conflict();
            return Err(RuntimeError::CommitConflict {
                session_id: session_id.to_string(),
                expected_version,
                actual_version: actual,
            });
        }

        // From here we own the version bump. Any error would have to be
        // recovered by decrementing — but since the merge below is purely
        // lock-based (no fallible IO), it either succeeds or panics; in the
        // latter case the poisoned Mutex is fine to leave to the next caller.
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
/// A service whose `stop()` timed out. The stop thread is kept alive so
/// we can retry later via `kill_zombie_services`.
pub struct ZombieEntry {
    pub key: String,
    pub plugin_path: String,
    pub stuck_since: Instant,
    pub stop_handle: JoinHandle<Result<(), String>>,
}

pub struct ServiceRegistry {
    entries: Mutex<BTreeMap<String, ServiceEntry>>,
    zombies: Mutex<Vec<ZombieEntry>>,
}

/// 判定服务 key（"{plugin_path}::{node_id}"）是否属于给定插件（含其子孙
/// 子树）。用 `::` 或 `/` 边界匹配而非裸前缀：插件路径互为前缀时（如
/// "root" 与 "root2"），裸 `starts_with("root")` 会误停兄弟插件的服务。
fn service_key_belongs_to(key: &str, plugin_path: &str) -> bool {
    key.starts_with(&format!("{plugin_path}::")) || key.starts_with(&format!("{plugin_path}/"))
}

impl std::fmt::Debug for ServiceRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.len();
        f.debug_struct("ServiceRegistry")
            .field("service_count", &len)
            .finish()
    }
}

impl Default for ServiceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            zombies: Mutex::new(Vec::new()),
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
        // 先查重再启动：此前先 svc.start() 后查重，重复注册时已启动的实例
        // 随 drop 丢失且从不 stop()，后台线程成为无法停止的孤儿。
        {
            let guard = self
                .entries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if guard.contains_key(&key) {
                return Err(RuntimeError::DuplicateService {
                    plugin_path: plugin_path.to_string(),
                    service: key,
                });
            }
        }
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
        // 二次查重：start 期间另一线程可能已注册同 key。命中则 stop 掉刚
        // 启动的实例再返回 DuplicateService，避免泄漏后台 worker。
        let mut guard = self
            .entries
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.contains_key(&key) {
            let _ = entry.svc.stop();
            return Err(RuntimeError::DuplicateService {
                plugin_path: plugin_path.to_string(),
                service: key,
            });
        }
        guard.insert(key, entry);
        Ok(())
    }

    /// Stop all services belonging to `plugin_path` (and its descendants).
    /// Each `stop()` runs on a dedicated thread with a 5-second timeout.
    /// Services that time out are moved to the zombie list for later
    /// forced cleanup via [`kill_zombie_services`].
    pub fn stop_plugin_services(&self, plugin_path: &str) {
        // 锁内只摘取条目，锁外执行 stop()：用户 stop 代码持锁调用会自死锁
        // （stop 内回调 len/start_service 时 Mutex 不可重入），慢 stop 也会
        // 阻塞整个注册表与 reload。
        let entries: Vec<(String, _)> = {
            let mut guard = self
                .entries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard
                .keys()
                .filter(|k| service_key_belongs_to(k, plugin_path))
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .filter_map(|key| guard.remove(&key).map(|entry| (key, entry)))
                .collect()
        };
        for (key, entry) in entries {
            entry.running.store(false, Ordering::SeqCst);
            if let Err(e) = entry.svc.stop() {
                eprintln!("service {key} stop error: {e}");
            }
        }
    }

    /// Stop services with a 5-second per-service deadline.
    /// Services that don't stop in time are pushed to the zombie list.
    pub fn stop_plugin_services_timed(&self, plugin_path: &str) {
        const STOP_TIMEOUT: Duration = Duration::from_secs(5);

        let entries_to_stop: Vec<(String, Box<dyn Service>)> = {
            let mut guard = self
                .entries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let keys: Vec<String> = guard
                .keys()
                .filter(|k| service_key_belongs_to(k, plugin_path))
                .cloned()
                .collect();
            keys.iter()
                .filter_map(|k| {
                    guard.remove(k).map(|e| {
                        e.running.store(false, Ordering::SeqCst);
                        (k.clone(), e.svc)
                    })
                })
                .collect()
        };

        let now = Instant::now();
        for (key, svc) in entries_to_stop {
            let key_c = key.clone();
            let plugin_path_c = plugin_path.to_string();
            let handle = std::thread::spawn(move || svc.stop());
            // Busy-wait with sleep (simple, no extra deps).
            let deadline = Instant::now() + STOP_TIMEOUT;
            let finished = loop {
                if handle.is_finished() {
                    break true;
                }
                if Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            if finished {
                match handle.join() {
                    Ok(Ok(())) => eprintln!("service {key_c} stopped"),
                    Ok(Err(e)) => eprintln!("service {key_c} stop error: {e}"),
                    Err(_) => eprintln!("service {key_c} stop panicked"),
                }
            } else {
                eprintln!(
                    "service {key_c} stop timed out (>{}s), moving to zombie list",
                    STOP_TIMEOUT.as_secs()
                );
                self.zombies
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(ZombieEntry {
                        key: key_c,
                        plugin_path: plugin_path_c,
                        stuck_since: now,
                        stop_handle: handle,
                    });
            }
        }
    }

    /// Force-kill zombies matching `plugin_path` prefix.
    /// Returns the number of zombies killed.
    pub fn kill_zombie_services(&self, plugin_path: &str) -> usize {
        let mut zombies = self.zombies.lock().unwrap_or_else(|p| p.into_inner());
        let (matched, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut *zombies)
            .into_iter()
            .partition(|z| service_key_belongs_to(&z.key, plugin_path));

        let killed = matched.len();
        for z in matched {
            // Last attempt: try joining with zero timeout (non-blocking poll).
            if z.stop_handle.is_finished() {
                match z.stop_handle.join() {
                    Ok(Ok(())) => eprintln!("zombie {} recovered", z.key),
                    Ok(Err(e)) => eprintln!("zombie {} stop error: {e}", z.key),
                    Err(_) => eprintln!("zombie {} stop panicked", z.key),
                }
            } else {
                // Still stuck — drop the handle (thread becomes detached).
                eprintln!(
                    "zombie {} still stuck after {}s, abandoning",
                    z.key,
                    z.stuck_since.elapsed().as_secs()
                );
                // Handle dropped here — thread continues detached.
            }
        }
        *zombies = rest;
        killed
    }

    /// Number of zombie services currently tracked.
    pub fn zombie_count(&self) -> usize {
        self.zombies.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Stop and remove all registered services.
    pub fn stop_all(&self) {
        // 锁内只摘取条目、锁外执行 stop()：与 stop_plugin_services 一致，
        // 避免用户 stop 代码在持锁回调时自死锁（Drop 路径同样受益）。
        let drained: BTreeMap<_, _> = {
            let mut guard = self
                .entries
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            std::mem::take(&mut *guard)
        };
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

    pub fn is_empty(&self) -> bool {
        self.len() == 0
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

    /// True when the first parked zombie's stop handle has finished, or when
    /// there is no zombie to wait on. Keeps the poll loop condition on one line.
    fn zombie_stop_finished(registry: &ServiceRegistry) -> bool {
        registry
            .zombies
            .lock()
            .unwrap()
            .first()
            .map(|z| z.stop_handle.is_finished())
            .unwrap_or(true)
    }

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

    struct FailStartService;
    impl Service for FailStartService {
        fn start(&self) -> Result<(), String> {
            Err("boom on start".to_string())
        }
        fn stop(&self) -> Result<(), String> {
            Ok(())
        }
    }

    struct FailStopService;
    impl Service for FailStopService {
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
        fn stop(&self) -> Result<(), String> {
            Err("boom on stop".to_string())
        }
    }

    /// A service whose `stop()` blocks on a channel receive until the test
    /// sends (or drops the sender). Lets us deterministically drive the
    /// 5-second stop-timeout path in `stop_plugin_services_timed`.
    struct BlockingStopService {
        rx: Mutex<std::sync::mpsc::Receiver<()>>,
    }

    impl Service for BlockingStopService {
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
        fn stop(&self) -> Result<(), String> {
            let _ = self.rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
            Ok(())
        }
    }

    /// A service whose `stop()` panics. On the timed-stop path the stop
    /// thread finishes (panicked) quickly, so `handle.join()` returns `Err`,
    /// driving the "stop panicked" arm.
    struct PanicStopService;
    impl Service for PanicStopService {
        fn start(&self) -> Result<(), String> {
            Ok(())
        }
        fn stop(&self) -> Result<(), String> {
            panic!("stop panicked on purpose");
        }
    }

    fn slot_meta() -> SlotMeta {
        SlotMeta {
            required: false,
            ttl_ms: None,
            sensitivity: Sensitivity::Low,
            owner: "test".to_string(),
        }
    }

    fn key(ns: &str, name: &str, version: u32) -> ContextKey {
        ContextKey {
            namespace: ns.to_string(),
            name: name.to_string(),
            version,
        }
    }

    // ---------- ContextKey / ScopeStore ----------

    #[test]
    fn context_key_as_compact_format() {
        let k = key("agent", "budget", 3);
        assert_eq!(k.as_compact(), "agent/budget@v3");
    }

    // ---------- ContextRegistry: provide / inject / maybe / dispose ----------

    #[test]
    fn provide_inject_dispose_across_scopes() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Global, None, "g", 1u32).unwrap();
        ctx.provide(ContextScope::Session, None, "s", 2u32).unwrap();
        ctx.provide(ContextScope::Request, None, "r", 3u32).unwrap();

        assert_eq!(*ctx.inject::<u32>("p", "g").unwrap(), 1);
        assert_eq!(*ctx.inject::<u32>("p", "s").unwrap(), 2);
        assert_eq!(*ctx.inject::<u32>("p", "r").unwrap(), 3);

        // maybe: hit and miss.
        assert!(ctx.maybe::<u32>("p", "g").is_some());
        assert!(ctx.maybe::<u32>("p", "nope").is_none());

        ctx.dispose(ContextScope::Global, None, "g").unwrap();
        assert!(matches!(
            ctx.inject::<u32>("p", "g"),
            Err(RuntimeError::ServiceNotFound { .. })
        ));
    }

    #[test]
    fn provide_duplicate_in_scope_is_rejected() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Global, None, "dup", 1u32)
            .unwrap();
        let err = ctx
            .provide(ContextScope::Global, None, "dup", 2u32)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::DuplicateService { .. }));
    }

    #[test]
    fn provide_priority_request_over_session_over_global() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Global, None, "svc", 100u32)
            .unwrap();
        ctx.provide(ContextScope::Session, None, "svc", 200u32)
            .unwrap();
        ctx.provide(ContextScope::Request, None, "svc", 300u32)
            .unwrap();
        // Request wins the lookup chain.
        assert_eq!(*ctx.inject::<u32>("p", "svc").unwrap(), 300);
    }

    #[test]
    fn provide_local_requires_plugin_path() {
        let mut ctx = RuntimeContext::default();
        let err = ctx
            .provide(ContextScope::Local, None, "x", 1u32)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Invariant { .. }));
    }

    #[test]
    fn provide_local_duplicate_is_rejected() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Local, Some("pl"), "x", 1u32)
            .unwrap();
        let err = ctx
            .provide(ContextScope::Local, Some("pl"), "x", 2u32)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::DuplicateService { .. }));
    }

    #[test]
    fn inject_type_mismatch_is_structured_error() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Global, None, "svc", 7u32)
            .unwrap();
        let err = ctx.inject::<String>("p", "svc").unwrap_err();
        assert!(matches!(err, RuntimeError::ServiceTypeMismatch { .. }));
    }

    #[test]
    fn inject_unavailable_plugin_fails_fast() {
        let mut ctx = RuntimeContext::default();
        ctx.set_plugin_state(
            "p",
            PluginLoadResult::Unavailable(
                crate::core::models::PluginUnavailableReason::AbiMismatch,
            ),
        );
        let err = ctx.inject::<u32>("p", "svc").unwrap_err();
        assert!(matches!(err, RuntimeError::ContextPluginUnavailable { .. }));
    }

    #[test]
    fn dispose_local_requires_plugin_path_and_reports_missing() {
        let mut ctx = RuntimeContext::default();
        let err = ctx.dispose(ContextScope::Local, None, "x").unwrap_err();
        assert!(matches!(err, RuntimeError::Invariant { .. }));

        let err = ctx
            .dispose(ContextScope::Global, None, "ghost")
            .unwrap_err();
        assert!(matches!(err, RuntimeError::ServiceNotFound { .. }));
    }

    // ---------- Local hierarchy chain + grants ----------

    #[test]
    fn inject_local_current_scope_hit() {
        let mut ctx = RuntimeContext::default();
        ctx.ensure_local_scope("child");
        ctx.provide(ContextScope::Local, Some("child"), "own", 42u32)
            .unwrap();
        assert_eq!(*ctx.inject::<u32>("child", "own").unwrap(), 42);
    }

    #[test]
    fn inject_parent_local_requires_grant() {
        let mut hierarchy = PluginHierarchy::default();
        hierarchy
            .parent_of
            .insert("child".to_string(), "parent".to_string());
        let mut ctx = RuntimeContext::with_hierarchy(hierarchy);
        ctx.provide(ContextScope::Local, Some("parent"), "shared", 9u32)
            .unwrap();

        // Without a grant on the child->parent edge, injection is denied.
        let err = ctx.inject::<u32>("child", "shared").unwrap_err();
        assert!(matches!(err, RuntimeError::PermissionDenied { .. }));
    }

    #[test]
    fn inject_parent_local_with_grant_succeeds() {
        let mut hierarchy = PluginHierarchy::default();
        hierarchy
            .parent_of
            .insert("child".to_string(), "parent".to_string());
        let mut grant = BTreeSet::new();
        grant.insert("shared".to_string());
        hierarchy
            .grants_from_parent
            .insert("child".to_string(), grant);
        let mut ctx = RuntimeContext::with_hierarchy(hierarchy);
        ctx.provide(ContextScope::Local, Some("parent"), "shared", 9u32)
            .unwrap();
        assert_eq!(*ctx.inject::<u32>("child", "shared").unwrap(), 9);
    }

    #[test]
    fn inject_grandparent_local_requires_requester_own_grant() {
        // Chain a <- b <- c: b holds a grant for a's service on the b -> a
        // edge, but c has no grant on its own c -> b edge. c must NOT inject
        // a's local service: authorization checks the original requester's
        // grant set, never the intermediate node's grants.
        let mut hierarchy = PluginHierarchy::default();
        hierarchy.parent_of.insert("b".to_string(), "a".to_string());
        hierarchy.parent_of.insert("c".to_string(), "b".to_string());
        let mut b_grants = BTreeSet::new();
        b_grants.insert("shared".to_string());
        hierarchy
            .grants_from_parent
            .insert("b".to_string(), b_grants);
        let mut ctx = RuntimeContext::with_hierarchy(hierarchy);
        ctx.provide(ContextScope::Local, Some("a"), "shared", 9u32)
            .unwrap();

        let err = ctx.inject::<u32>("c", "shared").unwrap_err();
        assert!(matches!(err, RuntimeError::PermissionDenied { .. }));
    }

    #[test]
    fn inject_grandparent_local_with_requester_grant_succeeds() {
        // Chain a <- b <- c: c holds the grant for a's service on its own
        // c -> b edge, so injecting a's local service succeeds. Same grant
        // lookup as the depth-1 case (requester's own parent edge), so the
        // depth-1-with-grant behavior is preserved at every ancestor hop.
        let mut hierarchy = PluginHierarchy::default();
        hierarchy.parent_of.insert("b".to_string(), "a".to_string());
        hierarchy.parent_of.insert("c".to_string(), "b".to_string());
        let mut c_grants = BTreeSet::new();
        c_grants.insert("shared".to_string());
        hierarchy
            .grants_from_parent
            .insert("c".to_string(), c_grants);
        let mut ctx = RuntimeContext::with_hierarchy(hierarchy);
        ctx.provide(ContextScope::Local, Some("a"), "shared", 9u32)
            .unwrap();

        assert_eq!(*ctx.inject::<u32>("c", "shared").unwrap(), 9);
    }

    #[test]
    fn inject_local_chain_stops_at_unavailable_parent() {
        let mut hierarchy = PluginHierarchy::default();
        hierarchy
            .parent_of
            .insert("child".to_string(), "parent".to_string());
        let mut ctx = RuntimeContext::with_hierarchy(hierarchy);
        ctx.ensure_local_scope("child");
        ctx.set_plugin_state(
            "parent",
            PluginLoadResult::Unavailable(crate::core::models::PluginUnavailableReason::InitFailed),
        );
        let err = ctx.inject::<u32>("child", "missing").unwrap_err();
        assert!(matches!(err, RuntimeError::ContextPluginUnavailable { .. }));
    }

    // ---------- ContextRead / ContextWrite: slots ----------

    #[test]
    fn slot_put_get_contains_and_meta() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "answer", 1);
        let meta = SlotMeta {
            required: true,
            ttl_ms: Some(1_500),
            sensitivity: Sensitivity::Sensitive,
            owner: "writer".to_string(),
        };
        ctx.put(k.clone(), serde_json::json!({"v": 42}), meta.clone())
            .unwrap();
        assert!(ctx.contains(&k));
        let got: serde_json::Value = ctx.get(&k).unwrap().unwrap();
        assert_eq!(got["v"], 42);
        // meta round-trips including ttl_ms.
        let m = ctx.meta(&k).unwrap().unwrap();
        assert_eq!(m.ttl_ms, Some(1_500));
        assert_eq!(m.sensitivity, Sensitivity::Sensitive);
        assert!(m.required);
    }

    #[test]
    fn slot_get_missing_is_none() {
        let ctx = RuntimeContext::default();
        assert!(ctx
            .get::<serde_json::Value>(&key("ns", "absent", 1))
            .unwrap()
            .is_none());
        assert!(!ctx.contains(&key("ns", "absent", 1)));
        assert!(ctx.meta(&key("ns", "absent", 1)).unwrap().is_none());
    }

    #[test]
    fn slot_get_deserialize_type_error() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "obj", 1);
        ctx.put(k.clone(), serde_json::json!({"a": 1}), slot_meta())
            .unwrap();
        // Ask for a String but the stored value is an object.
        let err = ctx.get::<String>(&k).unwrap_err();
        assert!(matches!(err, RuntimeError::ContextDeserialize { .. }));
    }

    #[test]
    fn slot_remove_deletes() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "tmp", 1);
        ctx.put(k.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        assert!(ctx.contains(&k));
        ctx.remove(&k).unwrap();
        assert!(!ctx.contains(&k));
    }

    #[test]
    fn slot_version_incompatible_across_major() {
        let ctx = RuntimeContext::default();
        // Store major 1 (v100), request major 2 (v250) same ns/name.
        ctx.put(key("ns", "schema", 100), serde_json::json!(1), slot_meta())
            .unwrap();
        let err = ctx
            .get::<serde_json::Value>(&key("ns", "schema", 250))
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::ContextVersionIncompatible { .. }
        ));
    }

    #[test]
    fn mark_skipped_tracks_nodes() {
        let ctx = RuntimeContext::default();
        ctx.mark_skipped("node-a").unwrap();
        ctx.mark_skipped("node-b").unwrap();
        let skipped = ctx.skipped_nodes();
        assert!(skipped.contains("node-a") && skipped.contains("node-b"));
    }

    #[test]
    fn metrics_count_reads_and_writes() {
        let ctx = RuntimeContext::default();
        let before = ctx.metrics();
        ctx.put(key("ns", "m", 1), serde_json::json!(1), slot_meta())
            .unwrap();
        let _ = ctx.get::<serde_json::Value>(&key("ns", "m", 1)).unwrap();
        let after = ctx.metrics();
        assert!(after.context_write_total > before.context_write_total);
        assert!(after.context_read_total > before.context_read_total);
    }

    // ---------- ContextTxn: overlays ----------

    #[test]
    fn overlay_commit_promotes_writes_to_request() {
        let ctx = RuntimeContext::default();
        ctx.begin_subgraph("sg1").unwrap();
        let k = key("ns", "in_overlay", 1);
        ctx.put(k.clone(), serde_json::json!("v"), slot_meta())
            .unwrap();
        // Visible inside the active overlay.
        assert!(ctx.contains(&k));
        ctx.commit_overlay("sg1").unwrap();
        // After commit the overlay closes and the value lives in request scope.
        assert!(ctx.contains(&k));
        let got: serde_json::Value = ctx.get(&k).unwrap().unwrap();
        assert_eq!(got, serde_json::json!("v"));
    }

    #[test]
    fn list_by_ns_skips_overlay_keys_from_other_namespaces() {
        let ctx = RuntimeContext::default();
        ctx.begin_subgraph("sg-ns").unwrap();
        ctx.put(key("ns_a", "mine", 1), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.put(key("ns_b", "other", 1), serde_json::json!(2), slot_meta())
            .unwrap();
        // Listing ns_a while the overlay holds an ns_b entry walks the
        // namespace-filter continue for the foreign key.
        let keys = ctx.list_by_ns("ns_a");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "mine");
        ctx.rollback_overlay("sg-ns").unwrap();
    }

    #[test]
    fn overlay_commit_applies_removals() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "base", 1);
        ctx.put(k.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.begin_subgraph("sg").unwrap();
        ctx.remove(&k).unwrap();
        // Removal is masked inside the overlay.
        assert!(!ctx.contains(&k));
        ctx.commit_overlay("sg").unwrap();
        assert!(!ctx.contains(&k));
    }

    #[test]
    fn overlay_rollback_discards_writes_and_counts_metric() {
        let ctx = RuntimeContext::default();
        let before = ctx.metrics().context_overlay_rollback_total;
        ctx.begin_subgraph("sg").unwrap();
        let k = key("ns", "scratch", 1);
        ctx.put(k.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.rollback_overlay("sg").unwrap();
        // Overlay write is gone after rollback.
        assert!(!ctx.contains(&k));
        assert_eq!(ctx.metrics().context_overlay_rollback_total, before + 1);
    }

    #[test]
    fn begin_subgraph_rejects_second_active() {
        let ctx = RuntimeContext::default();
        ctx.begin_subgraph("a").unwrap();
        let err = ctx.begin_subgraph("b").unwrap_err();
        assert!(matches!(err, RuntimeError::SubgraphAlreadyActive { .. }));
    }

    #[test]
    fn session_version_starts_zero_and_tracks_commits() {
        let ctx = RuntimeContext::default();
        assert_eq!(ctx.session_version(), 0);
        ctx.commit_session("s", 0).unwrap();
        assert_eq!(ctx.session_version(), 1);
    }

    /// After `commit_session` promotes request writes into the session slot
    /// map (and clears request), a later `lookup_slot_entry` finds the value
    /// via the session branch — the only path that reaches it, since nothing
    /// else writes `session_slots`. Also drives the non-empty merge loop body.
    #[test]
    fn commit_session_merges_request_into_session_and_lookup_reads_it() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "promoted", 1);
        ctx.put(k.clone(), serde_json::json!("v"), slot_meta())
            .unwrap();
        ctx.commit_session("s", 0).unwrap();
        // Request was cleared by the commit; the value now resolves from the
        // session slot map.
        let got: serde_json::Value = ctx.get(&k).unwrap().unwrap();
        assert_eq!(got, serde_json::json!("v"));
        // list_by_ns also surfaces the session-scoped key (session branch).
        assert!(ctx.list_by_ns("ns").contains(&k));
    }

    /// Inside an active overlay, a key absent from the overlay but present in
    /// request must fall through the overlay lookup to the request slot map.
    #[test]
    fn lookup_inside_overlay_falls_through_to_request() {
        let ctx = RuntimeContext::default();
        let base = key("ns", "base", 1);
        ctx.put(base.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.begin_subgraph("sg").unwrap();
        // Overlay is active but does not contain `base`; the read still finds
        // it via the request fall-through.
        let got: serde_json::Value = ctx.get(&base).unwrap().unwrap();
        assert_eq!(got, serde_json::json!(1));
        ctx.rollback_overlay("sg").unwrap();
    }

    /// Populate `global_slots` directly (no public writer promotes into it) so
    /// the global-scope read branch of `lookup_slot_entry` and the global
    /// branch of `list_by_ns` are exercised. Same-module access is the only way
    /// to reach these, mirroring `commit_session`'s session-branch coverage.
    #[test]
    fn lookup_and_list_read_global_slot_scope() {
        let ctx = RuntimeContext::default();
        let k = key("gns", "gval", 1);
        ctx.global_slots.lock().unwrap().insert(
            k.clone(),
            SlotEntry {
                value: serde_json::json!("g"),
                meta: slot_meta(),
            },
        );
        // lookup_slot_entry: request/session miss, global hit.
        let got: serde_json::Value = ctx.get(&k).unwrap().unwrap();
        assert_eq!(got, serde_json::json!("g"));
        // list_by_ns: global keys surface for the namespace.
        assert!(ctx.list_by_ns("gns").contains(&k));
    }

    /// A stored key whose major version differs from a later request in the
    /// same namespace/name yields `ContextVersionIncompatible`. Storing in the
    /// global slot map drives the schema check across the global keys chain.
    #[test]
    fn global_slot_major_version_mismatch_is_incompatible() {
        let ctx = RuntimeContext::default();
        ctx.global_slots.lock().unwrap().insert(
            key("gns", "schema", 100),
            SlotEntry {
                value: serde_json::json!(1),
                meta: slot_meta(),
            },
        );
        let err = ctx
            .get::<serde_json::Value>(&key("gns", "schema", 250))
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::ContextVersionIncompatible { .. }
        ));
    }

    /// `list_by_ns` applies the active overlay on top of the base scopes:
    /// an overlay write adds a key, and an overlay removal (tombstone) drops a
    /// base key from the listing.
    #[test]
    fn list_by_ns_applies_overlay_writes_and_removals() {
        let ctx = RuntimeContext::default();
        let kept = key("ns", "kept", 1);
        let removed = key("ns", "removed", 1);
        ctx.put(kept.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.put(removed.clone(), serde_json::json!(2), slot_meta())
            .unwrap();
        ctx.begin_subgraph("sg").unwrap();
        // Overlay adds a new key and tombstones an existing one.
        let added = key("ns", "added", 1);
        ctx.put(added.clone(), serde_json::json!(3), slot_meta())
            .unwrap();
        ctx.remove(&removed).unwrap();
        let listed = ctx.list_by_ns("ns");
        assert!(listed.contains(&kept), "kept survives: {listed:?}");
        assert!(listed.contains(&added), "overlay write appears: {listed:?}");
        assert!(
            !listed.contains(&removed),
            "overlay tombstone drops key: {listed:?}"
        );
        ctx.rollback_overlay("sg").unwrap();
    }

    #[test]
    fn commit_and_rollback_unknown_subgraph_error() {
        let ctx = RuntimeContext::default();
        assert!(matches!(
            ctx.commit_overlay("ghost"),
            Err(RuntimeError::SubgraphNotFound { .. })
        ));
        assert!(matches!(
            ctx.rollback_overlay("ghost"),
            Err(RuntimeError::SubgraphNotFound { .. })
        ));
    }

    // ---------- Clone isolates slot maps ----------

    #[test]
    fn clone_snapshots_slots_independently() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "shared", 1);
        ctx.put(k.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        let cloned = ctx.clone();
        // Mutating the original must not leak into the clone's slot map.
        ctx.put(key("ns", "only_orig", 1), serde_json::json!(2), slot_meta())
            .unwrap();
        assert!(cloned.contains(&k));
        assert!(!cloned.contains(&key("ns", "only_orig", 1)));
    }

    // ---------- ServiceRegistry: extra branches ----------

    #[test]
    fn service_start_failure_reported() {
        // `stop` is never reached via the registry (start fails first), so call
        // it directly to keep the mock's method exercised.
        assert!(FailStartService.stop().is_ok());
        let registry = ServiceRegistry::new();
        let err = registry
            .start_service("p", "n", Box::new(FailStartService))
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Invariant { .. }));
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn service_stop_error_does_not_panic() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("p", "n", Box::new(FailStopService))
            .unwrap();
        // stop error is logged, not propagated; entry still removed.
        registry.stop_plugin_services("p");
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn service_registry_debug_reports_count() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("p", "n", Box::new(CounterService::new()))
            .unwrap();
        let dbg = format!("{registry:?}");
        assert!(dbg.contains("ServiceRegistry") && dbg.contains("service_count"));
    }

    #[test]
    fn stop_all_drains_registry() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("p", "a", Box::new(CounterService::new()))
            .unwrap();
        registry
            .start_service("p", "b", Box::new(FailStopService))
            .unwrap();
        registry.stop_all();
        assert!(registry.is_empty());
    }

    #[test]
    fn stop_timed_quick_service_leaves_no_zombie() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("p", "quick", Box::new(CounterService::new()))
            .unwrap();
        registry.stop_plugin_services_timed("p");
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.zombie_count(), 0);
    }

    #[test]
    fn kill_zombie_services_recovered_and_stuck_branches() {
        // Build zombies directly (same-module access) to exercise both
        // branches of kill_zombie_services without waiting on the real
        // 5-second stop timeout.
        let registry = ServiceRegistry::new();

        // Recovered: a handle that has already finished.
        // Park until the thread reports finished. `yield_now` runs on every
        // iteration including the first, so the wait loop has no never-taken
        // body (a `sleep`-guarded `while` usually exits before entering).
        let finished = std::thread::spawn(|| Ok::<(), String>(()));
        loop {
            if finished.is_finished() {
                break;
            }
            std::thread::yield_now();
        }

        // Stuck: a handle blocked on a channel we never signal until the end.
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let stuck = std::thread::spawn(move || {
            let _ = rx.recv();
            Ok::<(), String>(())
        });

        {
            let mut z = registry.zombies.lock().unwrap();
            z.push(ZombieEntry {
                key: "plug::recovered".to_string(),
                plugin_path: "plug".to_string(),
                stuck_since: Instant::now(),
                stop_handle: finished,
            });
            z.push(ZombieEntry {
                key: "plug::stuck".to_string(),
                plugin_path: "plug".to_string(),
                stuck_since: Instant::now(),
                stop_handle: stuck,
            });
        }
        assert_eq!(registry.zombie_count(), 2);

        let killed = registry.kill_zombie_services("plug");
        assert_eq!(killed, 2, "both matched zombies are removed from tracking");
        assert_eq!(registry.zombie_count(), 0);

        // Non-matching prefix removes nothing.
        assert_eq!(registry.kill_zombie_services("other"), 0);

        // Let the still-stuck detached thread exit cleanly.
        let _ = tx.send(());
    }

    /// Real end-to-end stop-timeout path: a service whose `stop()` blocks
    /// past the 5s deadline is moved to the zombie list, then recovered
    /// once unblocked. Slow (~5s) but exercises the timed-stop timeout code.
    #[test]
    fn stop_timed_slow_service_becomes_zombie_then_recovers() {
        let registry = ServiceRegistry::new();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        registry
            .start_service(
                "slow",
                "svc",
                Box::new(BlockingStopService { rx: Mutex::new(rx) }),
            )
            .unwrap();
        // Blocks ~5s waiting on the stuck stop, then parks it as a zombie.
        registry.stop_plugin_services_timed("slow");
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.zombie_count(), 1);

        // Unblock the stop thread so the handle finishes, then reap it.
        tx.send(()).unwrap();
        // Give the stop thread a moment to complete. Poll up to 50*20ms,
        // exiting via the loop condition (no diverging `break`) once the
        // zombie's stop handle reports finished.
        let mut polls = 0;
        while polls < 50 && !zombie_stop_finished(&registry) {
            std::thread::sleep(Duration::from_millis(20));
            polls += 1;
        }
        let killed = registry.kill_zombie_services("slow");
        assert_eq!(killed, 1);
        assert_eq!(registry.zombie_count(), 0);
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
    fn registry_is_empty_reflects_len() {
        let registry = ServiceRegistry::default();
        assert!(registry.is_empty());
        registry
            .start_service("root", "svc", Box::new(CounterService::new()))
            .expect("start");
        assert!(!registry.is_empty());
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

    // ---------- P1-1 / P1-2 concurrency regressions ----------

    /// P1-1: two concurrent `commit_session` calls at the same expected
    /// version must NOT both succeed. `compare_exchange` gives exactly
    /// one caller the version bump; the other sees `CommitConflict`.
    #[test]
    fn commit_session_cas_is_exclusive_under_race() {
        let ctx = std::sync::Arc::new(RuntimeContext::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let ctx = ctx.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                ctx.commit_session("s", 0)
            }));
        }
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let ok = results.iter().filter(|r| r.is_ok()).count();
        let conflict = results
            .iter()
            .filter(|r| matches!(r, Err(RuntimeError::CommitConflict { .. })))
            .count();
        assert_eq!(ok, 1, "exactly one commit_session should succeed");
        assert_eq!(conflict, 7, "other 7 must see CommitConflict");
    }

    #[test]
    fn commit_session_increments_version_monotonically() {
        let ctx = RuntimeContext::default();
        ctx.commit_session("s", 0).unwrap();
        ctx.commit_session("s", 1).unwrap();
        ctx.commit_session("s", 2).unwrap();
        // Wrong expected version rejected.
        let err = ctx.commit_session("s", 2).unwrap_err();
        assert!(matches!(err, RuntimeError::CommitConflict { .. }));
    }

    // `commit_session` with a NON-empty request map promotes each entry into
    // session scope (the `for (key, value)` merge loop, line 840). Afterwards
    // the value is served from the session slot — `lookup_slot_entry`'s
    // session-scope hit (line 358) — since the request scope was cleared by
    // the commit. `list_by_ns` must then surface the key from session scope,
    // and `session_version` reflects the bump.
    #[test]
    fn commit_session_promotes_request_to_session_scope() {
        let ctx = RuntimeContext::default();
        assert_eq!(ctx.session_version(), 0);
        let k = key("sess_ns", "promoted", 1);
        ctx.put(k.clone(), serde_json::json!("keep"), slot_meta())
            .unwrap();
        // Before commit the value lives in request scope.
        ctx.commit_session("s", 0).unwrap();
        assert_eq!(ctx.session_version(), 1);
        // Request scope was cleared; the value is now served from session
        // scope on read-back.
        let got: serde_json::Value = ctx.get(&k).unwrap().unwrap();
        assert_eq!(got, serde_json::json!("keep"));
        // Still visible; contains() also routes through the session-scope hit.
        assert!(ctx.contains(&k));
        // list_by_ns enumerates the session-scope key.
        let listed = ctx.list_by_ns("sess_ns");
        assert!(listed.contains(&k), "listed: {listed:?}");
    }

    /// P1-2: `list_by_ns` and `lookup_slot_entry` used to acquire
    /// context locks in opposite orders; two workers doing one each
    /// could deadlock. After the fix both go active → overlays →
    /// request → session → global. This test spawns two workers hitting
    /// each path in a tight loop; without the fix it would hang.
    #[test]
    fn list_by_ns_and_lookup_do_not_deadlock() {
        let ctx = std::sync::Arc::new(RuntimeContext::default());
        let meta = SlotMeta {
            required: false,
            ttl_ms: None,
            sensitivity: Sensitivity::Low,
            owner: "test".to_string(),
        };
        // Seed a couple of entries in different namespaces.
        for i in 0..5 {
            let key = ContextKey {
                namespace: format!("ns-{}", i % 2),
                name: format!("k{i}"),
                version: 1,
            };
            ctx.put(key, serde_json::json!({ "n": i }), meta.clone())
                .unwrap();
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_a = stop.clone();
        let ctx_a = ctx.clone();
        let a = std::thread::spawn(move || {
            let mut count = 0usize;
            while !stop_a.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = ctx_a.list_by_ns("ns-0");
                count += 1;
            }
            count
        });
        let stop_b = stop.clone();
        let ctx_b = ctx.clone();
        let b = std::thread::spawn(move || {
            let mut count = 0usize;
            let key = ContextKey {
                namespace: "ns-1".to_string(),
                name: "k1".to_string(),
                version: 1,
            };
            while !stop_b.load(std::sync::atomic::Ordering::Relaxed) {
                let _ = ctx_b.contains(&key);
                count += 1;
            }
            count
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let ca = a.join().unwrap();
        let cb = b.join().unwrap();
        assert!(ca > 0 && cb > 0, "workers should progress, got {ca} / {cb}");
    }

    // ---------- dispose: Session / Request / Local-hit removal arms ----------

    #[test]
    fn dispose_session_and_request_scopes_remove() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Session, None, "s", 1u32).unwrap();
        ctx.provide(ContextScope::Request, None, "r", 2u32).unwrap();
        // Both scopes dispose successfully (exercises the Session/Request arms).
        ctx.dispose(ContextScope::Session, None, "s").unwrap();
        ctx.dispose(ContextScope::Request, None, "r").unwrap();
        assert!(matches!(
            ctx.inject::<u32>("p", "s"),
            Err(RuntimeError::ServiceNotFound { .. })
        ));
        assert!(matches!(
            ctx.inject::<u32>("p", "r"),
            Err(RuntimeError::ServiceNotFound { .. })
        ));
    }

    #[test]
    fn dispose_local_scope_hit_removes() {
        let mut ctx = RuntimeContext::default();
        ctx.ensure_local_scope("pl");
        ctx.provide(ContextScope::Local, Some("pl"), "x", 5u32)
            .unwrap();
        // Local dispose with an existing scope+id hits the map get_mut/remove arm.
        ctx.dispose(ContextScope::Local, Some("pl"), "x").unwrap();
        assert!(matches!(
            ctx.inject::<u32>("pl", "x"),
            Err(RuntimeError::ServiceNotFound { .. })
        ));
        // Disposing a missing id from an existing scope reports ServiceNotFound.
        let err = ctx
            .dispose(ContextScope::Local, Some("pl"), "ghost")
            .unwrap_err();
        assert!(matches!(err, RuntimeError::ServiceNotFound { .. }));
    }

    // ---------- put: serialize failure ----------

    #[test]
    fn put_non_serializable_value_is_context_serialize_error() {
        let ctx = RuntimeContext::default();
        // A map with non-string keys cannot serialize to a JSON object; this
        // drives `to_value` into the error closure (ContextSerialize).
        let mut bad = std::collections::BTreeMap::new();
        bad.insert(vec![1u8, 2u8], "v");
        let err = ctx.put(key("ns", "bad", 1), bad, slot_meta()).unwrap_err();
        assert!(
            matches!(err, RuntimeError::ContextSerialize { .. }),
            "got {err:?}"
        );
    }

    // ---------- list_by_ns: overlay add + remove masking ----------

    #[test]
    fn list_by_ns_reflects_overlay_add_and_removal() {
        let ctx = RuntimeContext::default();
        let base = key("ns", "base", 1);
        ctx.put(base.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.begin_subgraph("sg").unwrap();
        // Overlay adds a new key and masks the base key with a removal.
        let added = key("ns", "added", 1);
        ctx.put(added.clone(), serde_json::json!(2), slot_meta())
            .unwrap();
        ctx.remove(&base).unwrap();
        let listed = ctx.list_by_ns("ns");
        assert!(
            listed.contains(&added),
            "overlay add must appear: {listed:?}"
        );
        assert!(
            !listed.contains(&base),
            "overlay removal must mask base: {listed:?}"
        );
    }

    // ---------- timed-stop: quick stop that errors / panics ----------

    #[test]
    fn stop_timed_quick_stop_error_leaves_no_zombie() {
        // FailStopService::stop returns Err immediately; the stop thread
        // finishes fast, so join() -> Ok(Err(..)) (the "stop error" arm) and
        // no zombie is parked.
        let registry = ServiceRegistry::new();
        registry
            .start_service("p", "failstop", Box::new(FailStopService))
            .unwrap();
        registry.stop_plugin_services_timed("p");
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.zombie_count(), 0);
    }

    #[test]
    fn stop_timed_quick_stop_panic_leaves_no_zombie() {
        // PanicStopService::stop panics; the stop thread finishes (unwound),
        // so join() -> Err(..) (the "stop panicked" arm) and no zombie is
        // parked because the handle is finished before the deadline.
        let registry = ServiceRegistry::new();
        registry
            .start_service("p", "panicstop", Box::new(PanicStopService))
            .unwrap();
        registry.stop_plugin_services_timed("p");
        assert_eq!(registry.len(), 0);
        assert_eq!(registry.zombie_count(), 0);
    }

    // ---------- kill_zombie_services: recovered-with-error / panicked arms ----

    #[test]
    fn kill_zombie_recovered_error_and_panic_arms() {
        let registry = ServiceRegistry::new();

        // A finished handle that returned Err -> "zombie stop error" arm.
        let errored = std::thread::spawn(|| Err::<(), String>("boom".to_string()));
        while !errored.is_finished() {
            std::thread::sleep(Duration::from_millis(5));
        }
        // A finished handle that panicked -> "zombie stop panicked" arm.
        let panicked = std::thread::spawn(|| -> Result<(), String> {
            panic!("zombie panic");
        });
        while !panicked.is_finished() {
            std::thread::sleep(Duration::from_millis(5));
        }

        {
            let mut z = registry.zombies.lock().unwrap();
            z.push(ZombieEntry {
                key: "plug::errored".to_string(),
                plugin_path: "plug".to_string(),
                stuck_since: Instant::now(),
                stop_handle: errored,
            });
            z.push(ZombieEntry {
                key: "plug::panicked".to_string(),
                plugin_path: "plug".to_string(),
                stuck_since: Instant::now(),
                stop_handle: panicked,
            });
        }
        let killed = registry.kill_zombie_services("plug");
        assert_eq!(killed, 2);
        assert_eq!(registry.zombie_count(), 0);
    }

    // ---------- P0 regression: clone / remove / stop-boundary ----------

    /// `repaired_subgraph_state` 把撕裂快照归一为不变量满足的状态：
    /// active=None → (None, 空)；active 有 overlay → 保留；active 无 overlay
    /// （撕裂）→ (None, 空)。
    #[test]
    fn repaired_subgraph_state_normalizes_torn_snapshots() {
        let empty: BTreeMap<String, BTreeMap<ContextKey, Option<SlotEntry>>> = BTreeMap::new();
        let (a, o) = repaired_subgraph_state(None, empty.clone());
        assert!(a.is_none() && o.is_empty());

        let mut delta = BTreeMap::new();
        delta.insert(key("ns", "x", 1), None);
        let mut overlays: BTreeMap<String, BTreeMap<ContextKey, Option<SlotEntry>>> =
            BTreeMap::new();
        overlays.insert("sg".to_string(), delta);

        let (a, o) = repaired_subgraph_state(Some("sg".to_string()), overlays.clone());
        assert_eq!(a.as_deref(), Some("sg"));
        assert!(o.contains_key("sg"));

        let (a, o) = repaired_subgraph_state(Some("ghost".to_string()), overlays);
        assert!(a.is_none() && o.is_empty());
    }

    /// clone 在活动子图期间必须保留一致的 subgraph 状态：克隆体可见 overlay
    /// 内容并可独立 commit，原上下文不受影响。
    #[test]
    fn clone_preserves_active_subgraph_consistently() {
        let ctx = RuntimeContext::default();
        ctx.begin_subgraph("sg-clone").unwrap();
        let k = key("ns", "in_overlay", 1);
        ctx.put(k.clone(), serde_json::json!("v"), slot_meta())
            .unwrap();
        let cloned = ctx.clone();
        let got: serde_json::Value = cloned.get(&k).unwrap().unwrap();
        assert_eq!(got, serde_json::json!("v"));
        cloned.commit_overlay("sg-clone").unwrap();
        assert!(cloned.contains(&k));
        // 原上下文仍处于活动子图，可独立回滚。
        assert!(ctx.contains(&k));
        ctx.rollback_overlay("sg-clone").unwrap();
    }

    /// remove 必须贯穿整条查找链：commit_session 已把键提升进 session、
    /// global_slots 直接承载的键，remove 后都必须消失（此前只删 request
    /// 层，已提交键"复活"）。
    #[test]
    fn slot_remove_reaches_session_and_global_layers() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "promoted", 1);
        ctx.put(k.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.commit_session("s", 0).unwrap();
        assert!(ctx.contains(&k));
        ctx.remove(&k).unwrap();
        assert!(!ctx.contains(&k));

        let g = key("ns", "glob", 1);
        ctx.global_slots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                g.clone(),
                SlotEntry {
                    value: serde_json::json!(9),
                    meta: slot_meta(),
                },
            );
        assert!(ctx.contains(&g));
        ctx.remove(&g).unwrap();
        assert!(!ctx.contains(&g));
    }

    /// 子图墓碑提交后必须删除 session 层承载的键（commit_session 提升后仅
    /// 删 request 会让键复活）。
    #[test]
    fn overlay_commit_tombstone_reaches_session_layer() {
        let ctx = RuntimeContext::default();
        let k = key("ns", "base", 1);
        ctx.put(k.clone(), serde_json::json!(1), slot_meta())
            .unwrap();
        ctx.commit_session("s", 0).unwrap();
        ctx.begin_subgraph("sg").unwrap();
        ctx.remove(&k).unwrap();
        ctx.commit_overlay("sg").unwrap();
        assert!(!ctx.contains(&k));
    }

    /// dispose 最后一个 local 服务后，空作用域条目应从 local map 回收。
    #[test]
    fn dispose_local_removes_empty_scope_entry() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Local, Some("p"), "s", 1u32)
            .unwrap();
        assert!(ctx.local.contains_key("p"));
        ctx.dispose(ContextScope::Local, Some("p"), "s")
            .unwrap();
        assert!(!ctx.local.contains_key("p"));
    }

    /// 还有其它服务时 dispose 不回收作用域。
    #[test]
    fn dispose_local_keeps_scope_when_services_remain() {
        let mut ctx = RuntimeContext::default();
        ctx.provide(ContextScope::Local, Some("p"), "a", 1u32)
            .unwrap();
        ctx.provide(ContextScope::Local, Some("p"), "b", 2u32)
            .unwrap();
        ctx.dispose(ContextScope::Local, Some("p"), "a")
            .unwrap();
        assert!(ctx.local.contains_key("p"));
        assert_eq!(*ctx.inject::<u32>("p", "b").unwrap(), 2);
    }

    /// 服务 key 前缀必须按 `::`/`/` 边界匹配：停止 "root" 不得误停
    /// "root2"（前缀重叠的兄弟插件）的服务，但 "root/child" 子孙仍被停。
    #[test]
    fn stop_plugin_services_respects_key_boundary() {
        let registry = ServiceRegistry::new();
        registry
            .start_service("root", "svc_a", Box::new(CounterService::new()))
            .expect("start");
        registry
            .start_service("root2", "svc_b", Box::new(CounterService::new()))
            .expect("start");
        registry
            .start_service("root/child", "svc_c", Box::new(CounterService::new()))
            .expect("start");
        assert_eq!(registry.len(), 3);
        registry.stop_plugin_services("root");
        assert_eq!(registry.len(), 1);
    }

    /// 并发注册同 key：两个线程都通过前置查重并各自 start 后，只有一个能
    /// insert；另一个在二次查重处命中，必须 stop 掉自己刚启动的实例（避免
    /// 泄漏后台 worker）并返回 DuplicateService。
    #[test]
    fn start_service_concurrent_duplicate_stops_new_instance() {
        use std::sync::Barrier;
        struct GatedService {
            barrier: Arc<Barrier>,
            stops: Arc<AtomicUsize>,
        }
        impl Service for GatedService {
            fn start(&self) -> Result<(), String> {
                self.barrier.wait();
                Ok(())
            }
            fn stop(&self) -> Result<(), String> {
                self.stops.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let registry = Arc::new(ServiceRegistry::new());
        let barrier = Arc::new(Barrier::new(3));
        let stops = Arc::new(AtomicUsize::new(0));
        let svc_a = Box::new(GatedService {
            barrier: barrier.clone(),
            stops: stops.clone(),
        }) as Box<dyn Service>;
        let svc_b = Box::new(GatedService {
            barrier: barrier.clone(),
            stops: stops.clone(),
        }) as Box<dyn Service>;
        let reg_a = registry.clone();
        let reg_b = registry.clone();
        let handle_a = std::thread::spawn(move || reg_a.start_service("p", "dup", svc_a));
        let handle_b = std::thread::spawn(move || reg_b.start_service("p", "dup", svc_b));
        barrier.wait();
        let results = [handle_a.join().unwrap(), handle_b.join().unwrap()];
        let oks = results.iter().filter(|r| r.is_ok()).count();
        let errs = results.iter().filter(|r| r.is_err()).count();
        assert_eq!(oks, 1);
        assert_eq!(errs, 1);
        // 败者 stop 掉自己刚启动的实例：恰好一次 stop。
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        assert_eq!(registry.len(), 1);
    }
}
