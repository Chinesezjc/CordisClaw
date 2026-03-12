//! Runtime ABI surface shared by host and dylib plugins.
//! Shared wire types come from `cordis-plugin-sdk`; runtime keeps only host-local metadata
//! and traits in this module.

pub use cordis_plugin_sdk::{
    DylibAbiKind, PluginRequest, PluginResponse, RustPluginApiV2,
};

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
