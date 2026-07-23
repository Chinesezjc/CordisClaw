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
        if let Some(plugin) = self
            .plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(plugin_path)
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
        if let Some(plugin) = self
            .plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(plugin_path)
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
        let mut guard = self
            .plugins
            .write()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(plugin) = guard.get_mut(plugin_path) {
            plugin.load_result = PluginLoadResult::Loaded;
            plugin.docs = Some(docs);
            plugin.abi_fingerprint = Some(abi_fingerprint);
            plugin.fingerprint_diff = Vec::new();
            true
        } else {
            false
        }
    }

    pub fn get(&self, plugin_path: &str) -> Option<RegisteredPlugin> {
        self.plugins
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(plugin_path)
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
        match err {
            RuntimeError::NodeFqnConflict {
                node_fqn,
                first,
                second,
            } => {
                assert_eq!(node_fqn, "shared::dup");
                assert_eq!(first, "shared");
                assert_eq!(second, "shared");
            }
            other => panic!("wrong variant: {other:?}"),
        }
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
}
