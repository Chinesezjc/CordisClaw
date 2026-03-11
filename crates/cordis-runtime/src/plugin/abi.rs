//! Runtime ABI surface shared by host and dylib plugins.
//! Host side resolves `cordis_plugin_api_rust_v2` and uses this table.

use crate::core::models::{AbiFingerprint, DylibAbiKind, PluginDocs};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AbiFingerprintStatic {
    /// Static values exported by dylib symbol table.
    pub rustc_version: &'static str,
    pub target_triple: &'static str,
    pub crate_hash: &'static str,
    pub api_hash: &'static str,
}

impl AbiFingerprintStatic {
    /// Convert static symbol payload into owned value for comparison/logging.
    pub fn to_owned(self) -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: self.rustc_version.to_string(),
            target_triple: self.target_triple.to_string(),
            crate_hash: self.crate_hash.to_string(),
            api_hash: self.api_hash.to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PluginRequest {
    /// Runtime request payload passed into plugin handler.
    pub payload: String,
}

#[derive(Debug, Clone)]
pub struct PluginResponse {
    /// Runtime response payload returned by plugin handler.
    pub payload: String,
}

#[derive(Debug, Clone)]
pub struct NodeMeta {
    /// Node id local to current plugin.
    pub id: &'static str,
    /// Type-like tokens consumed by this node.
    pub consumes: &'static [&'static str],
    /// Type-like tokens produced by this node.
    pub produces: &'static [&'static str],
    /// Scheduler priority.
    pub priority: i32,
}

pub trait RuntimePlugin: Send {
    /// Single plugin request entrypoint.
    fn handle(&mut self, req: PluginRequest) -> PluginResponse;
}

pub struct RustPluginApiV2 {
    /// Must be `DylibAbiKind::Rust`.
    pub abi_kind: DylibAbiKind,
    pub abi_fingerprint: AbiFingerprintStatic,
    /// Build plugin instance.
    pub init: fn() -> Box<dyn RuntimePlugin>,
    /// Expose node capability metadata.
    pub nodes: fn(&dyn RuntimePlugin) -> Vec<NodeMeta>,
    /// Expose agent/human-readable docs.
    pub docs: fn(&dyn RuntimePlugin) -> PluginDocs,
    /// Handle a runtime request.
    pub handle: fn(&mut dyn RuntimePlugin, PluginRequest) -> PluginResponse,
    /// Explicit destroy hook for plugin instance.
    pub drop: fn(Box<dyn RuntimePlugin>),
}
