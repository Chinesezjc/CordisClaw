use crate::core::error::RuntimeError;
use crate::core::models::{
    AbiFingerprint, ArtifactKind, PluginDocs, PluginExecution, PluginLoadResult,
    PluginUnavailableReason,
};
use cordis_plugin_sdk::NodeType;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone)]
pub struct RegisteredPlugin {
    pub plugin_path: String,
    pub parent: Option<String>,
    pub required: bool,
    pub grants_from_parent: BTreeSet<String>,
    pub load_result: PluginLoadResult,
    pub docs: Option<PluginDocs>,
    pub artifact_path: Option<PathBuf>,
    pub artifact_kind: Option<ArtifactKind>,
    pub abi_fingerprint: Option<AbiFingerprint>,
    pub execution: Option<PluginExecution>,
    pub fingerprint_diff: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct RegisteredNode {
    pub node_fqn: String,
    pub plugin_path: String,
    pub node_id: String,
    pub node_type: NodeType,
}

/// Normalise a plugin path as spelled by an external caller (in practice: the
/// model behind `invoke_plugin`, which happily emits directory-looking
/// spellings like `"/qq"`).
///
/// The registry is keyed by the canonical `plugin_path` recorded in the plugin
/// docs, so a slash-prefixed or slash-suffixed spelling of the same path used to
/// miss the map and fail the whole agent turn with
/// `plugin not registered: /qq`. Every lookup normalises first: trim
/// surrounding whitespace, squeeze each run of `/` into a single separator, and
/// drop leading/trailing separators. Case is deliberately **not** folded —
/// plugin paths are case-sensitive.
///
/// This is a pure function on purpose: it is the one place the spelling rules
/// live, so the invoke path and the host's `check_agent_accessible` gate can
/// never drift apart.
pub fn normalize_plugin_path(raw: &str) -> String {
    raw.trim()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// Registry of all discovered plugins, indexed by plugin path.
///
/// Wraps the inner map in `Arc<RwLock<>>` for two reasons:
///
/// 1. **Interior mutability during loading.**  Methods like
///    [`insert_unavailable`](Self::insert_unavailable) and
///    [`mark_unavailable`](Self::mark_unavailable) take `&self` so they can
///    be called from contexts that already hold a `&mut NodeRegistry`.
/// 2. **Snapshot sharing.**  The `Arc` allows cheap cloning for snapshot
///    captures; the `RwLock` allows concurrent reads after loading completes.
#[derive(Debug, Default, Clone)]
pub struct PluginRegistry {
    plugins: Arc<RwLock<BTreeMap<String, RegisteredPlugin>>>,
}

/// Registry of all discovered nodes, indexed by fully-qualified name
/// (format: `"{plugin_path}::{node_id}"`).
///
/// Uses a plain `BTreeMap` (no locking) because nodes are populated once
/// during the loading phase behind `&mut self` and are treated as read-only
/// thereafter.  References returned by [`get`](Self::get) borrow from the
/// inner map directly, avoiding the clone overhead required by
/// [`PluginRegistry`]'s `RwLock`-wrapped map.
#[derive(Debug, Default, Clone)]
pub struct NodeRegistry {
    nodes: BTreeMap<String, RegisteredNode>,
}

impl PluginRegistry {
    // 装载登记的全部字段一次性写入；拆结构体只是把 10 个参数换成 10 个字段赋值，不减少调用点耦合。
    #[expect(clippy::too_many_arguments)]
    pub fn insert_loaded(
        &self,
        plugin_path: String,
        parent: Option<String>,
        required: bool,
        grants_from_parent: BTreeSet<String>,
        docs: PluginDocs,
        artifact_path: PathBuf,
        artifact_kind: ArtifactKind,
        abi_fingerprint: AbiFingerprint,
        execution: Option<PluginExecution>,
    ) {
        // Store under the canonical spelling too: a `get` for `/qq` now hits,
        // and a later `mark_runtime_unavailable("/qq")` must be able to find
        // the same row. Keeping the raw spelling in the key would bring the
        // miss back through the mutator door.
        let plugin_path = normalize_plugin_path(&plugin_path);
        self.plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                plugin_path.clone(),
                RegisteredPlugin {
                    plugin_path,
                    parent,
                    required,
                    grants_from_parent,
                    load_result: PluginLoadResult::Loaded,
                    docs: Some(docs),
                    artifact_path: Some(artifact_path),
                    artifact_kind: Some(artifact_kind),
                    abi_fingerprint: Some(abi_fingerprint),
                    execution,
                    fingerprint_diff: Vec::new(),
                },
            );
    }

    pub fn insert_unavailable(
        &self,
        plugin_path: String,
        parent: Option<String>,
        required: bool,
        grants_from_parent: BTreeSet<String>,
        reason: PluginUnavailableReason,
        fingerprint_diff: Vec<String>,
    ) {
        let plugin_path = normalize_plugin_path(&plugin_path);
        self.plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                plugin_path.clone(),
                RegisteredPlugin {
                    plugin_path,
                    parent,
                    required,
                    grants_from_parent,
                    load_result: PluginLoadResult::Unavailable(reason),
                    docs: None,
                    artifact_path: None,
                    artifact_kind: None,
                    abi_fingerprint: None,
                    execution: None,
                    fingerprint_diff,
                },
            );
    }

    pub fn mark_unavailable(&self, plugin_path: &str, reason: PluginUnavailableReason) {
        let normalized = normalize_plugin_path(plugin_path);
        if let Some(plugin) = self
            .plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(&normalized)
        {
            plugin.load_result = PluginLoadResult::Unavailable(reason);
            plugin.docs = None;
            plugin.artifact_path = None;
            plugin.artifact_kind = None;
            plugin.abi_fingerprint = None;
            plugin.execution = None;
            plugin.fingerprint_diff.clear();
        }
    }

    pub fn mark_runtime_unavailable(
        &self,
        plugin_path: &str,
        reason: PluginUnavailableReason,
        fingerprint_diff: Vec<String>,
    ) {
        let normalized = normalize_plugin_path(plugin_path);
        if let Some(plugin) = self
            .plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(&normalized)
        {
            plugin.load_result = PluginLoadResult::Unavailable(reason);
            plugin.fingerprint_diff = fingerprint_diff;
        }
    }

    /// Update an existing plugin entry after reloading its dylib.
    pub fn reload_plugin_entry(
        &self,
        plugin_path: &str,
        docs: PluginDocs,
        abi_fingerprint: AbiFingerprint,
    ) -> bool {
        let normalized = normalize_plugin_path(plugin_path);
        let mut guard = self
            .plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(plugin) = guard.get_mut(&normalized) {
            plugin.load_result = PluginLoadResult::Loaded;
            plugin.docs = Some(docs);
            plugin.abi_fingerprint = Some(abi_fingerprint);
            plugin.fingerprint_diff = Vec::new();
            true
        } else {
            false
        }
    }

    /// Look up a plugin by path, tolerating the caller's spelling: the query is
    /// run through [`normalize_plugin_path`] first, so `"/qq"`, `"qq/"` and
    /// `"//qq"` all resolve to the entry registered as `"qq"`.
    ///
    /// Normalisation lives here — the single choke point every path lookup goes
    /// through — rather than at each call site, so `invoke_registered_plugin`
    /// and `RuntimeHost::check_agent_accessible` both benefit. A miss still
    /// returns `None`; call sites keep building their error text from the
    /// *original* (un-normalised) argument.
    pub fn get(&self, plugin_path: &str) -> Option<RegisteredPlugin> {
        let normalized = normalize_plugin_path(plugin_path);
        self.plugins
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(&normalized)
            .cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (String, RegisteredPlugin)> {
        self.plugins
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .map(|(plugin_path, plugin)| (plugin_path.clone(), plugin.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn len(&self) -> usize {
        self.plugins
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Insert a fully hand-built [`RegisteredPlugin`], bypassing the field
    /// invariants the public `insert_*` constructors enforce.
    ///
    /// The public constructors always co-populate the artifact-derived fields
    /// (`insert_loaded` fills every `Some`, `insert_unavailable` clears them),
    /// so a `Loaded` entry with a *missing* docs / artifact_kind /
    /// abi_fingerprint is unconstructible through the normal API. The invoke
    /// path nonetheless carries fail-closed `Invariant` guards for each of
    /// those "impossible" states; this test-only door lets unit tests drive
    /// them directly without weakening the production constructors.
    #[cfg(test)]
    pub(crate) fn insert_raw(&self, mut plugin: RegisteredPlugin) {
        plugin.plugin_path = normalize_plugin_path(&plugin.plugin_path);
        self.plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(plugin.plugin_path.clone(), plugin);
    }
}

impl NodeRegistry {
    pub fn get(&self, node_fqn: &str) -> Option<&RegisteredNode> {
        self.nodes.get(node_fqn)
    }

    pub fn register_from_docs(
        &mut self,
        plugin_path: &str,
        docs: &PluginDocs,
    ) -> Result<(), RuntimeError> {
        for node in &docs.nodes {
            let node_fqn = format!("{}::{}", plugin_path, node.id);
            if let Some(existing) = self.nodes.get(&node_fqn) {
                return Err(RuntimeError::NodeFqnConflict {
                    node_fqn,
                    first: existing.plugin_path.clone(),
                    second: plugin_path.to_string(),
                });
            }
            self.nodes.insert(
                node_fqn.clone(),
                RegisteredNode {
                    node_fqn,
                    plugin_path: plugin_path.to_string(),
                    node_id: node.id.clone(),
                    node_type: node.node_type,
                },
            );
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn remove_by_plugin(&mut self, plugin_path: &str) {
        self.nodes
            .retain(|_, node| node.plugin_path.as_str() != plugin_path);
    }

    pub fn contains(&self, node_fqn: &str) -> bool {
        self.nodes.contains_key(node_fqn)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &RegisteredNode)> {
        self.nodes.iter()
    }

    /// Return fully-qualified names of all nodes declared as [`NodeType::Task`].
    pub fn task_node_fqns(&self) -> Vec<String> {
        self.nodes
            .iter()
            .filter(|(_, node)| node.node_type == NodeType::Task)
            .map(|(fqn, _)| fqn.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::AbiFingerprint;
    use crate::plugin::invoke::invoke_registered_plugin;
    use cordis_plugin_sdk::{NodeDoc, PluginDocs};

    fn sample_docs(plugin_path: &str) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 1,
            command_name: None,
            nodes: vec![NodeDoc {
                id: "n0".to_string(),
                summary: "test node".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                side_effects: vec![],
                failure_modes: vec![],
                node_type: NodeType::Router,
                agent_accessible: true,
            }],
            system_hint: None,
        }
    }

    // Docs carrying a mix of node types, for exercising `task_node_fqns`
    // and multi-node registration.
    fn docs_with_nodes(plugin_path: &str, nodes: &[(&str, NodeType)]) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 1,
            command_name: None,
            nodes: nodes
                .iter()
                .map(|(id, node_type)| NodeDoc {
                    id: (*id).to_string(),
                    summary: "n".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: serde_json::json!({"type": "object"}),
                    side_effects: vec![],
                    failure_modes: vec![],
                    node_type: *node_type,
                    agent_accessible: true,
                })
                .collect(),
            system_hint: None,
        }
    }

    fn sample_fingerprint() -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: "rustc-test".to_string(),
            target_triple: "test-triple".to_string(),
            crate_hash: "crate_test_v1".to_string(),
            api_hash: "api_v2".to_string(),
        }
    }

    fn insert_sample_loaded(registry: &PluginRegistry, plugin_path: &str) {
        registry.insert_loaded(
            plugin_path.to_string(),
            None,
            true,
            BTreeSet::new(),
            sample_docs(plugin_path),
            PathBuf::from(format!("/tmp/{}.dylib", plugin_path.replace('/', "_"))),
            ArtifactKind::Dylib,
            sample_fingerprint(),
            None,
        );
    }

    #[test]
    fn plugin_registry_is_empty_reflects_len() {
        let registry = PluginRegistry::default();
        assert!(registry.is_empty());
        registry.insert_unavailable(
            "sample/plugin".to_string(),
            None,
            true,
            BTreeSet::new(),
            PluginUnavailableReason::ArtifactMissing,
            Vec::new(),
        );
        assert!(!registry.is_empty());
    }

    // insert_loaded stores a fully-populated Loaded entry; get returns a clone
    // whose fields round-trip. iter reflects the same set; len counts entries.
    #[test]
    fn insert_loaded_populates_entry_and_iter() {
        let registry = PluginRegistry::default();
        insert_sample_loaded(&registry, "loaded/one");
        insert_sample_loaded(&registry, "loaded/two");

        assert_eq!(registry.len(), 2);
        let plugin = registry.get("loaded/one").expect("entry present");
        assert!(matches!(plugin.load_result, PluginLoadResult::Loaded));
        assert!(plugin.docs.is_some());
        assert_eq!(plugin.artifact_kind, Some(ArtifactKind::Dylib));
        assert!(plugin.abi_fingerprint.is_some());
        assert!(plugin.fingerprint_diff.is_empty());

        let paths: Vec<String> = registry.iter().map(|(path, _)| path).collect();
        assert_eq!(paths, vec!["loaded/one", "loaded/two"]);
        assert!(registry.get("missing").is_none());
    }

    // mark_unavailable flips a Loaded entry to Unavailable and clears all the
    // artifact-derived fields (docs / path / kind / fingerprint / diff).
    #[test]
    fn mark_unavailable_clears_artifact_fields() {
        let registry = PluginRegistry::default();
        insert_sample_loaded(&registry, "flip/me");

        registry.mark_unavailable("flip/me", PluginUnavailableReason::InitFailed);

        let plugin = registry.get("flip/me").expect("still present");
        assert!(matches!(
            plugin.load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
        ));
        assert!(plugin.docs.is_none());
        assert!(plugin.artifact_path.is_none());
        assert!(plugin.artifact_kind.is_none());
        assert!(plugin.abi_fingerprint.is_none());
        assert!(plugin.execution.is_none());
        assert!(plugin.fingerprint_diff.is_empty());
    }

    // mark_unavailable on an unknown path is a no-op (the `if let` guard must
    // not insert anything).
    #[test]
    fn mark_unavailable_on_unknown_is_noop() {
        let registry = PluginRegistry::default();
        registry.mark_unavailable("nope", PluginUnavailableReason::InitFailed);
        assert!(registry.is_empty());
    }

    // mark_runtime_unavailable sets the reason and records the fingerprint
    // diff while (unlike mark_unavailable) leaving other fields untouched.
    #[test]
    fn mark_runtime_unavailable_records_diff() {
        let registry = PluginRegistry::default();
        insert_sample_loaded(&registry, "runtime/bad");

        registry.mark_runtime_unavailable(
            "runtime/bad",
            PluginUnavailableReason::AbiMismatch,
            vec!["api_hash:api_v2!=api_v3".to_string()],
        );

        let plugin = registry.get("runtime/bad").expect("present");
        assert!(matches!(
            plugin.load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch)
        ));
        assert_eq!(plugin.fingerprint_diff, vec!["api_hash:api_v2!=api_v3"]);
    }

    #[test]
    fn mark_runtime_unavailable_on_unknown_is_noop() {
        let registry = PluginRegistry::default();
        registry.mark_runtime_unavailable(
            "nope",
            PluginUnavailableReason::AbiMismatch,
            vec!["x".to_string()],
        );
        assert!(registry.is_empty());
    }

    // reload_plugin_entry returns true and re-marks a previously-unavailable
    // entry as Loaded, refreshing docs / fingerprint and clearing the diff.
    #[test]
    fn reload_plugin_entry_updates_existing_returns_true() {
        let registry = PluginRegistry::default();
        registry.insert_unavailable(
            "reload/me".to_string(),
            None,
            true,
            BTreeSet::new(),
            PluginUnavailableReason::SymbolMissing,
            vec!["stale-diff".to_string()],
        );

        let ok = registry.reload_plugin_entry(
            "reload/me",
            sample_docs("reload/me"),
            sample_fingerprint(),
        );
        assert!(ok, "reload of existing entry must return true");

        let plugin = registry.get("reload/me").expect("present");
        assert!(matches!(plugin.load_result, PluginLoadResult::Loaded));
        assert!(plugin.docs.is_some());
        assert_eq!(plugin.abi_fingerprint, Some(sample_fingerprint()));
        assert!(plugin.fingerprint_diff.is_empty());
    }

    // reload_plugin_entry returns false for a path that was never registered.
    #[test]
    fn reload_plugin_entry_missing_returns_false() {
        let registry = PluginRegistry::default();
        let ok = registry.reload_plugin_entry(
            "never/registered",
            sample_docs("never/registered"),
            sample_fingerprint(),
        );
        assert!(!ok);
        assert!(registry.is_empty());
    }

    // The eight spelling shapes the invoke path actually receives from the
    // model: slash-prefixed, slash-suffixed, doubled slashes, whitespace-only,
    // already-canonical, empty, bare slash, and non-ASCII with padding.
    #[test]
    fn normalize_plugin_path_cases() {
        let cases = [
            ("/qq", "qq"),
            ("qq/", "qq"),
            ("//expr//lexer/", "expr/lexer"),
            ("  ", ""),
            ("expr/lexer", "expr/lexer"),
            ("", ""),
            ("/", ""),
            ("  配置/插件  ", "配置/插件"),
        ];
        for (raw, expected) in cases {
            assert_eq!(normalize_plugin_path(raw), expected, "raw input {raw:?}");
        }
    }

    // `get` is the single choke point: every slash spelling of a registered
    // path must resolve to the same entry, and an unknown path must still miss
    // (so call sites can build their error text from the original argument).
    #[test]
    fn get_tolerates_slash_spellings_of_registered_path() {
        let registry = PluginRegistry::default();
        insert_sample_loaded(&registry, "qq");

        for spelling in ["/qq", "qq/", "//qq", "/qq/", "  qq  "] {
            let hit = registry.get(spelling).is_some();
            assert!(hit, "expected {spelling:?} to resolve the qq entry");
        }
        let miss = registry.get("/nope");
        assert!(miss.is_none());
    }

    // End-to-end proof for the incident at the invoke entry: a slash-prefixed
    // path must reach the registered entry (a registered-but-unavailable plugin
    // reports `PluginUnavailable`, never `PluginNotRegistered`), and the error
    // still echoes the caller's *original* spelling verbatim.
    #[test]
    fn invoke_with_slash_prefixed_path_reaches_registry_entry() {
        let registry = PluginRegistry::default();
        registry.insert_unavailable(
            "qq".to_string(),
            None,
            true,
            BTreeSet::new(),
            PluginUnavailableReason::ArtifactMissing,
            Vec::new(),
        );
        let err = invoke_registered_plugin(&registry, "/qq", "qq_send", "{}".to_string())
            .expect_err("unavailable plugin must error");
        assert_eq!(
            err.to_string(),
            "plugin unavailable: /qq, reason=ArtifactMissing, required=true"
        );
    }

    #[test]
    fn node_registry_is_empty_reflects_len() {
        let mut registry = NodeRegistry::default();
        assert!(registry.is_empty());
        let docs = sample_docs("sample/plugin");
        registry
            .register_from_docs("sample/plugin", &docs)
            .expect("register nodes");
        assert!(!registry.is_empty());
    }

    // register_from_docs builds fqns as "{plugin_path}::{node_id}"; get and
    // contains locate them; len counts each declared node.
    #[test]
    fn node_registry_get_and_contains() {
        let mut registry = NodeRegistry::default();
        let docs = docs_with_nodes(
            "svc/plug",
            &[("a", NodeType::Router), ("b", NodeType::Task)],
        );
        registry
            .register_from_docs("svc/plug", &docs)
            .expect("register");

        assert_eq!(registry.len(), 2);
        assert!(registry.contains("svc/plug::a"));
        assert!(!registry.contains("svc/plug::missing"));

        let node = registry.get("svc/plug::b").expect("node present");
        assert_eq!(node.plugin_path, "svc/plug");
        assert_eq!(node.node_id, "b");
        assert_eq!(node.node_type, NodeType::Task);
        assert_eq!(node.node_fqn, "svc/plug::b");
        assert!(registry.get("svc/plug::missing").is_none());

        // iter yields all registered nodes.
        let count = registry.iter().count();
        assert_eq!(count, 2);
    }

    // Two plugins declaring the same fqn must surface NodeFqnConflict with
    // both offending plugin paths.
    #[test]
    fn register_from_docs_detects_fqn_conflict() {
        let mut registry = NodeRegistry::default();
        // Both plugins are keyed as "shared" so their node fqns collide.
        let first = docs_with_nodes("shared", &[("dup", NodeType::Router)]);
        registry
            .register_from_docs("shared", &first)
            .expect("first registration ok");

        let second = docs_with_nodes("shared", &[("dup", NodeType::Router)]);
        let err = registry
            .register_from_docs("shared", &second)
            .expect_err("duplicate fqn must conflict");
        assert!(
            matches!(&err, RuntimeError::NodeFqnConflict { node_fqn, first, second } if node_fqn == "shared::dup" && first == "shared" && second == "shared"),
            "wrong variant: {err:?}"
        );
    }

    // remove_by_plugin drops exactly the nodes owned by the given plugin and
    // leaves other plugins' nodes intact.
    #[test]
    fn remove_by_plugin_drops_only_matching_nodes() {
        let mut registry = NodeRegistry::default();
        registry
            .register_from_docs("keep", &docs_with_nodes("keep", &[("k", NodeType::Router)]))
            .expect("register keep");
        registry
            .register_from_docs(
                "drop",
                &docs_with_nodes("drop", &[("d1", NodeType::Router), ("d2", NodeType::Task)]),
            )
            .expect("register drop");
        assert_eq!(registry.len(), 3);

        registry.remove_by_plugin("drop");

        assert_eq!(registry.len(), 1);
        assert!(registry.contains("keep::k"));
        assert!(!registry.contains("drop::d1"));
        assert!(!registry.contains("drop::d2"));
    }

    // task_node_fqns returns fqns of Task nodes only, across all plugins.
    #[test]
    fn task_node_fqns_filters_task_type() {
        let mut registry = NodeRegistry::default();
        registry
            .register_from_docs(
                "p1",
                &docs_with_nodes("p1", &[("r", NodeType::Router), ("t1", NodeType::Task)]),
            )
            .expect("register p1");
        registry
            .register_from_docs("p2", &docs_with_nodes("p2", &[("t2", NodeType::Task)]))
            .expect("register p2");

        let mut fqns = registry.task_node_fqns();
        fqns.sort();
        assert_eq!(fqns, vec!["p1::t1".to_string(), "p2::t2".to_string()]);
    }

    // Poison the inner RwLock (panic while holding the write guard) and confirm
    // every PluginRegistry accessor still recovers via
    // `unwrap_or_else(|poison| poison.into_inner())` rather than propagating the
    // poison as a panic. One poisoned registry, cloned across the Arc, exercises
    // the recovery arm on the read path (get/iter/len/is_empty) and the write
    // path (insert/mark/reload).
    #[test]
    fn poisoned_lock_is_recovered_on_every_accessor() {
        let registry = PluginRegistry::default();
        insert_sample_loaded(&registry, "before/poison");

        // Poison the lock: take the write guard on a clone, then panic.
        let clone = registry.clone();
        let poisoned = std::thread::spawn(move || {
            let _guard = clone.plugins.write().unwrap();
            panic!("deliberately poison the registry lock");
        })
        .join();
        assert!(poisoned.is_err(), "the spawned thread must have panicked");

        // Read paths recover and still see the pre-poison entry.
        assert_eq!(registry.len(), 1);
        assert!(!registry.is_empty());
        assert!(registry.get("before/poison").is_some());
        assert_eq!(registry.iter().count(), 1);

        // Write paths recover too: insert, mark, reload all proceed.
        insert_sample_loaded(&registry, "after/poison");
        assert_eq!(registry.len(), 2);
        registry.mark_unavailable("after/poison", PluginUnavailableReason::InitFailed);
        registry.mark_runtime_unavailable(
            "before/poison",
            PluginUnavailableReason::AbiMismatch,
            vec!["diff".to_string()],
        );
        assert!(registry.reload_plugin_entry(
            "after/poison",
            sample_docs("after/poison"),
            sample_fingerprint(),
        ));
    }
}

/// Registry-level normalisation alone already makes `PluginRegistry::get`
/// tolerant, but the incident's *first* gate is
/// `RuntimeHost::check_agent_accessible`, which resolves through a snapshot's
/// registry. This module drives that host-level entry point with a
/// slash-prefixed path to prove the whole chain benefits — kept in its own
/// module (rather than inside `mod tests`) so concurrent edits to the primary
/// test module never collide.
#[cfg(test)]
mod host_check_normalizes_plugin_path_tests {
    use super::*;
    use crate::host::RuntimeHost;
    use cordis_plugin_sdk::NodeDoc;
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    fn fingerprint() -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: "rustc-test".to_string(),
            target_triple: "test-triple".to_string(),
            crate_hash: "crate_test_v1".to_string(),
            api_hash: "api_v2".to_string(),
        }
    }

    /// An `artifacts/index.json` with zero entries: `RuntimeHost::boot`
    /// registers no plugins and never touches a dylib, so booting is
    /// near-instant on any host target.
    const EMPTY_ARTIFACT_INDEX: &str = r#"{
  "schema_version": 2,
  "generated_at": "2026-07-24T00:00:00Z",
  "topo_order": [],
  "entries": []
}
"#;

    /// Build a throwaway fixtures tree, with `config/runtime.yaml` pinning the
    /// snapshot staging root inside the `TempDir` (`discover_config_dir` looks
    /// for the sibling `config` dir of a root named `fixtures`).
    fn empty_fixtures() -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let fixtures = temp.path().join("fixtures");
        let artifacts = fixtures.join("artifacts");
        fs::create_dir_all(&artifacts).expect("create artifacts dir");
        fs::write(artifacts.join("index.json"), EMPTY_ARTIFACT_INDEX)
            .expect("write empty artifact index");
        let config_dir = temp.path().join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(
            config_dir.join("runtime.yaml"),
            format!(
                "runtime:\n  snapshot_root: {}\n",
                temp.path().join("snapshots").display()
            ),
        )
        .expect("write runtime.yaml");
        (temp, fixtures)
    }

    fn docs_with_node(plugin_path: &str, node_id: &str) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 1,
            command_name: None,
            nodes: vec![NodeDoc {
                id: node_id.to_string(),
                summary: "test node".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                side_effects: vec![],
                failure_modes: vec![],
                node_type: NodeType::Task,
                agent_accessible: true,
            }],
            system_hint: None,
        }
    }

    // The incident, replayed at the host gate: the model called
    // `invoke_plugin("plugin_path": "/qq", "node_id": "qq_send")`, and
    // `check_agent_accessible` rejected the turn with
    // `plugin not registered: /qq`. A registered `qq` must now be reachable
    // through the slash-prefixed spelling: a permitted node resolves (`Ok`), a
    // node-level rejection stays `InvalidArgument`, and only a genuinely
    // unregistered path is `PluginNotRegistered` — carrying the caller's
    // original spelling so existing `plugin not registered: /nope` assertions
    // keep passing.
    #[test]
    #[serial]
    fn host_check_agent_accessible_accepts_slash_prefixed_plugin_path() {
        let (_temp, fixtures) = empty_fixtures();
        let host = RuntimeHost::boot(&fixtures).expect("host should boot on an empty index");
        host.current_snapshot().plugin_registry().insert_loaded(
            "qq".to_string(),
            None,
            true,
            BTreeSet::new(),
            docs_with_node("qq", "qq_send"),
            PathBuf::from("/tmp/qq.dylib"),
            ArtifactKind::Dylib,
            fingerprint(),
            None,
        );

        let allowed = host.check_agent_accessible("/qq", "qq_send");
        let permitted = allowed.is_ok();
        assert!(permitted, "slash-prefixed path must resolve: {allowed:?}");

        let err = host
            .check_agent_accessible("/qq", "not_a_node")
            .expect_err("undeclared node must be rejected");
        let not_registered = matches!(&err, RuntimeError::PluginNotRegistered { .. });
        assert!(!not_registered, "plugin is registered: {err:?}");

        let err = host
            .check_agent_accessible("/nope", "qq_send")
            .expect_err("unregistered plugin must be rejected");
        assert_eq!(err.to_string(), "plugin not registered: /nope");
    }
}

/// Mutator-side normalisation: `get` alone would still let a slash-prefixed
/// spelling miss every *write* path, which is how `invoke_plugin("/qq", …)`
/// could resolve the entry, fail to open the dylib, and then have
/// `mark_runtime_unavailable("/qq", …)` silently no-op — leaving a broken
/// plugin stamped `Loaded` and the failure invisible to later lookups.
#[cfg(test)]
mod mutator_normalization_tests {
    use super::*;
    use cordis_plugin_sdk::{NodeDoc, PluginDocs};

    // Self-contained copies: `mod tests` keeps its helpers private, and these
    // cases only need one accessible node plus a placeholder fingerprint.
    fn sample_docs(plugin_path: &str) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 1,
            command_name: None,
            nodes: vec![NodeDoc {
                id: "n0".to_string(),
                summary: "test node".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                side_effects: vec![],
                failure_modes: vec![],
                node_type: NodeType::Router,
                agent_accessible: true,
            }],
            system_hint: None,
        }
    }

    fn sample_fingerprint() -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: "rustc-test".to_string(),
            target_triple: "test-triple".to_string(),
            crate_hash: "crate_test_v1".to_string(),
            api_hash: "api_v2".to_string(),
        }
    }

    #[test]
    fn insert_loaded_stores_the_canonical_spelling() {
        let registry = PluginRegistry::default();
        registry.insert_loaded(
            "/qq/".to_string(),
            None,
            true,
            BTreeSet::new(),
            sample_docs("qq"),
            PathBuf::from("/tmp/qq.dylib"),
            ArtifactKind::Dylib,
            sample_fingerprint(),
            None,
        );
        let stored = registry.get("qq").expect("canonical lookup");
        assert_eq!(stored.plugin_path, "qq");
        let slash = registry.get("/qq").is_some();
        assert!(slash);
    }

    #[test]
    fn mark_runtime_unavailable_accepts_a_slash_spelling() {
        let registry = PluginRegistry::default();
        registry.insert_loaded(
            "qq".to_string(),
            None,
            true,
            BTreeSet::new(),
            sample_docs("qq"),
            PathBuf::from("/tmp/qq.dylib"),
            ArtifactKind::Dylib,
            sample_fingerprint(),
            None,
        );
        registry.mark_runtime_unavailable(
            "/qq",
            PluginUnavailableReason::SymbolMissing,
            vec!["diff".to_string()],
        );
        let after = registry.get("qq").expect("entry survives the mark");
        let unavailable = matches!(after.load_result, PluginLoadResult::Unavailable(_));
        assert!(unavailable, "mark must land on the canonical row");
        assert_eq!(after.fingerprint_diff, vec!["diff".to_string()]);
    }

    #[test]
    fn reload_entry_accepts_a_slash_spelling() {
        let registry = PluginRegistry::default();
        registry.insert_unavailable(
            "qq".to_string(),
            None,
            true,
            BTreeSet::new(),
            PluginUnavailableReason::ArtifactMissing,
            Vec::new(),
        );
        let reloaded = registry.reload_plugin_entry("/qq", sample_docs("qq"), sample_fingerprint());
        assert!(reloaded);
        let entry = registry.get("qq").expect("entry after reload");
        let loaded = matches!(entry.load_result, PluginLoadResult::Loaded);
        assert!(loaded);
    }

    #[test]
    fn insert_raw_stores_the_canonical_spelling() {
        let registry = PluginRegistry::default();
        registry.insert_raw(RegisteredPlugin {
            plugin_path: "//qq/".to_string(),
            parent: None,
            required: true,
            grants_from_parent: BTreeSet::new(),
            load_result: PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing),
            docs: None,
            artifact_path: None,
            artifact_kind: None,
            abi_fingerprint: None,
            execution: None,
            fingerprint_diff: Vec::new(),
        });
        let entry = registry
            .get("qq")
            .expect("canonical lookup after insert_raw");
        assert_eq!(entry.plugin_path, "qq");
    }
}
