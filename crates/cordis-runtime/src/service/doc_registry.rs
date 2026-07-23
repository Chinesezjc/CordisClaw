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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::models::ArtifactKind;
    use cordis_plugin_sdk::{node_doc, plugin_docs, AbiFingerprint};
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn sample_docs(plugin_path: &str) -> PluginDocs {
        plugin_docs(
            plugin_path.replace('/', "_"),
            plugin_path,
            "0.1.0",
            None,
            vec![node_doc(
                "n0",
                "test node",
                serde_json::json!({"type": "object"}),
                serde_json::json!({"type": "object"}),
                &[],
                &[],
            )],
            None,
        )
    }

    fn registry_with(plugin_path: &str) -> PluginRegistry {
        let registry = PluginRegistry::default();
        registry.insert_loaded(
            plugin_path.to_string(),
            None,
            true,
            BTreeSet::new(),
            sample_docs(plugin_path),
            PathBuf::from("/tmp/artifact.json"),
            ArtifactKind::Json,
            AbiFingerprint::current_build("crate_test", "api_v2"),
            None,
        );
        registry
    }

    #[test]
    fn from_plugin_registry_indexes_only_docs_bearing_plugins() {
        let registry = registry_with("root/plugin");
        // A plugin without docs must be skipped by `from_plugin_registry`.
        registry.insert_unavailable(
            "root/broken".to_string(),
            None,
            true,
            BTreeSet::new(),
            crate::core::models::PluginUnavailableReason::ArtifactMissing,
            Vec::new(),
        );

        let docs = DocRegistry::from_plugin_registry(&registry);
        assert!(docs.get_plugin_docs("root/plugin").is_ok());
        assert!(matches!(
            docs.get_plugin_docs("root/broken"),
            Err(RuntimeError::PluginDocsNotFound { .. })
        ));
    }

    #[test]
    fn get_plugin_docs_returns_registered_entry() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        let got = docs.get_plugin_docs("root/plugin").expect("docs present");
        assert_eq!(got.plugin_path, "root/plugin");
        assert_eq!(got.nodes.len(), 1);
    }

    #[test]
    fn get_plugin_docs_missing_yields_not_found() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        match docs.get_plugin_docs("root/absent") {
            Err(RuntimeError::PluginDocsNotFound { plugin_path }) => {
                assert_eq!(plugin_path, "root/absent");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn get_node_docs_found_and_missing() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        let node = docs
            .get_node_docs("root/plugin", "n0")
            .expect("node present");
        assert_eq!(node.id, "n0");
        match docs.get_node_docs("root/plugin", "absent") {
            Err(RuntimeError::NodeDocsNotFound {
                plugin_path,
                node_id,
            }) => {
                assert_eq!(plugin_path, "root/plugin");
                assert_eq!(node_id, "absent");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn handle_get_plugin_docs_route() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        let value = docs
            .handle_get("/plugins/root/plugin/docs")
            .expect("plugin docs route");
        assert_eq!(value["plugin_path"], "root/plugin");
    }

    #[test]
    fn handle_get_node_docs_route() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        let value = docs
            .handle_get("/plugins/root/plugin/nodes/n0/docs")
            .expect("node docs route");
        assert_eq!(value["id"], "n0");
    }

    #[test]
    fn handle_get_invalid_route() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        match docs.handle_get("/not/a/known/route") {
            Err(RuntimeError::InvalidDocsRoute { path }) => {
                assert_eq!(path, "/not/a/known/route");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn handle_get_plugin_docs_route_missing_plugin_propagates_not_found() {
        let docs = DocRegistry::from_plugin_registry(&registry_with("root/plugin"));
        assert!(matches!(
            docs.handle_get("/plugins/absent/docs"),
            Err(RuntimeError::PluginDocsNotFound { .. })
        ));
    }

    #[test]
    fn parse_plugin_docs_path_variants() {
        assert_eq!(
            parse_plugin_docs_path("/plugins/root/child/docs"),
            Some("root/child".to_string())
        );
        // node route must not be parsed as a plugin docs route.
        assert_eq!(parse_plugin_docs_path("/plugins/root/nodes/n0/docs"), None);
        // empty plugin path.
        assert_eq!(parse_plugin_docs_path("/plugins//docs"), None);
        // wrong prefix / suffix.
        assert_eq!(parse_plugin_docs_path("/other/root/docs"), None);
        assert_eq!(parse_plugin_docs_path("/plugins/root/info"), None);
    }

    #[test]
    fn parse_node_docs_path_variants() {
        assert_eq!(
            parse_node_docs_path("/plugins/root/child/nodes/n0/docs"),
            Some(("root/child".to_string(), "n0".to_string()))
        );
        // missing nodes separator.
        assert_eq!(parse_node_docs_path("/plugins/root/docs"), None);
        // empty node id.
        assert_eq!(parse_node_docs_path("/plugins/root/nodes//docs"), None);
        // empty plugin path.
        assert_eq!(parse_node_docs_path("/plugins//nodes/n0/docs"), None);
        // wrong suffix.
        assert_eq!(parse_node_docs_path("/plugins/root/nodes/n0/info"), None);
    }
}
