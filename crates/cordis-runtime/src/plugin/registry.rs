use crate::core::error::RuntimeError;
use crate::core::models::{
    AbiFingerprint, ArtifactKind, PluginDocs, PluginExecution, PluginLoadResult,
    PluginUnavailableReason,
};
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
}

#[derive(Debug, Default, Clone)]
pub struct PluginRegistry {
    plugins: Arc<RwLock<BTreeMap<String, RegisteredPlugin>>>,
}

#[derive(Debug, Default, Clone)]
pub struct NodeRegistry {
    nodes: BTreeMap<String, RegisteredNode>,
}

impl PluginRegistry {
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
            .expect("plugin registry lock poisoned")
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
            .expect("plugin registry lock poisoned")
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
            .expect("plugin registry lock poisoned")
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
            .expect("plugin registry lock poisoned")
            .get_mut(plugin_path)
        {
            plugin.load_result = PluginLoadResult::Unavailable(reason);
            plugin.fingerprint_diff = fingerprint_diff;
        }
    }

    pub fn get(&self, plugin_path: &str) -> Option<RegisteredPlugin> {
        self.plugins
            .read()
            .expect("plugin registry lock poisoned")
            .get(plugin_path)
            .cloned()
    }

    pub fn iter(&self) -> impl Iterator<Item = (String, RegisteredPlugin)> {
        self.plugins
            .read()
            .expect("plugin registry lock poisoned")
            .iter()
            .map(|(plugin_path, plugin)| (plugin_path.clone(), plugin.clone()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    pub fn len(&self) -> usize {
        self.plugins
            .read()
            .expect("plugin registry lock poisoned")
            .len()
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
                },
            );
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
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
}
