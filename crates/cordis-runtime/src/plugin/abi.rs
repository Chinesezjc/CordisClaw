//! Runtime ABI surface shared by host and dylib plugins.
//! Host side resolves `cordis_plugin_api_rust_v2` and uses this table.

use crate::core::models::DylibAbiKind;

#[repr(C)]
#[derive(Debug, Clone)]
pub struct PluginRequest {
    /// Runtime request payload passed into plugin handler.
    pub payload: String,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct PluginResponse {
    /// Runtime response payload returned by plugin handler.
    pub payload: String,
}

#[repr(C)]
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

#[repr(C)]
pub struct RustPluginApiV2 {
    /// Must be `DylibAbiKind::Rust`.
    pub abi_kind: DylibAbiKind,
    /// Expose ABI fingerprint as JSON payload.
    pub abi_fingerprint: fn() -> PluginResponse,
    /// Expose agent/human-readable docs as JSON payload.
    pub docs: fn() -> PluginResponse,
    /// Handle a runtime request.
    pub handle: fn(PluginRequest) -> PluginResponse,
}
