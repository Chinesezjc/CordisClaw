//! Machine-readable docs registry and route-style query helpers.
//! Supported paths:
//! - `GET /plugins/{plugin_path}/docs`
//! - `GET /plugins/{plugin_path}/nodes/{node_id}/docs`

use crate::core::error::RuntimeError;
use crate::core::models::{NodeDoc, PluginDocs};
use crate::plugin::registry::PluginRegistry;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone)]
pub struct DocRegistry {
    by_plugin_path: BTreeMap<String, PluginDocs>,
}

impl DocRegistry {
    pub fn from_plugin_registry(registry: &PluginRegistry) -> Self {
        let mut by_plugin_path = BTreeMap::new();
        for (plugin_path, plugin) in registry.iter() {
            if let Some(docs) = &plugin.docs {
                by_plugin_path.insert(plugin_path.clone(), docs.clone());
            }
        }
        Self { by_plugin_path }
    }

    pub fn get_plugin_docs(&self, plugin_path: &str) -> Result<&PluginDocs, RuntimeError> {
        self.by_plugin_path
            .get(plugin_path)
            .ok_or_else(|| RuntimeError::PluginDocsNotFound {
                plugin_path: plugin_path.to_string(),
            })
    }

    pub fn get_node_docs(
        &self,
        plugin_path: &str,
        node_id: &str,
    ) -> Result<&NodeDoc, RuntimeError> {
        let docs = self.get_plugin_docs(plugin_path)?;
        docs.nodes
            .iter()
            .find(|node| node.id == node_id)
            .ok_or_else(|| RuntimeError::NodeDocsNotFound {
                plugin_path: plugin_path.to_string(),
                node_id: node_id.to_string(),
            })
    }

    pub fn handle_get(&self, path: &str) -> Result<Value, RuntimeError> {
        if let Some(plugin_path) = parse_plugin_docs_path(path) {
            let docs = self.get_plugin_docs(&plugin_path)?;
            return serde_json::to_value(docs).map_err(|err| RuntimeError::Invariant {
                message: format!("serialize plugin docs failed: {err}"),
            });
        }

        if let Some((plugin_path, node_id)) = parse_node_docs_path(path) {
            let docs = self.get_node_docs(&plugin_path, &node_id)?;
            return serde_json::to_value(docs).map_err(|err| RuntimeError::Invariant {
                message: format!("serialize node docs failed: {err}"),
            });
        }

        Err(RuntimeError::InvalidDocsRoute {
            path: path.to_string(),
        })
    }
}

fn parse_plugin_docs_path(path: &str) -> Option<String> {
    let prefix = "/plugins/";
    let suffix = "/docs";
    if !path.starts_with(prefix) || !path.ends_with(suffix) {
        return None;
    }
    if path.contains("/nodes/") {
        return None;
    }
    let middle = &path[prefix.len()..path.len().saturating_sub(suffix.len())];
    if middle.is_empty() {
        return None;
    }
    Some(middle.to_string())
}

fn parse_node_docs_path(path: &str) -> Option<(String, String)> {
    let prefix = "/plugins/";
    let nodes_sep = "/nodes/";
    let suffix = "/docs";
    if !path.starts_with(prefix) || !path.ends_with(suffix) {
        return None;
    }
    let body = &path[prefix.len()..path.len().saturating_sub(suffix.len())];
    let idx = body.find(nodes_sep)?;
    let plugin_path = &body[..idx];
    let node_id = &body[idx + nodes_sep.len()..];
    if plugin_path.is_empty() || node_id.is_empty() {
        return None;
    }
    Some((plugin_path.to_string(), node_id.to_string()))
}
