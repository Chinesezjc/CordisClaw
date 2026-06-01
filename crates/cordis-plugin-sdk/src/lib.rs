use serde::{Deserialize, Serialize};

pub mod workflow;

pub use workflow::{
    session, AskUserSpec, CallSpec, EventSpec, JoinPolicy, JoinSpec, RacePolicy, RaceSpec,
    SleepSpec, WaitFuture, WaitHandle, WaitKind, WaitOutcome, WaitSpec, WorkflowError,
    WorkflowErrorKind, WorkflowRuntime, WorkflowSession,
};

pub const RUST_PLUGIN_ENTRY_SYMBOL: &str = "cordis_plugin_api_rust_v2";
pub const DEFAULT_ABI_VERSION: u32 = 2;

#[repr(u8)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DylibAbiKind {
    Rust,
}

impl Default for DylibAbiKind {
    fn default() -> Self {
        Self::Rust
    }
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AbiFingerprint {
    pub rustc_version: String,
    pub target_triple: String,
    pub crate_hash: String,
    pub api_hash: String,
}

impl AbiFingerprint {
    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        if self.rustc_version != other.rustc_version {
            out.push(format!(
                "rustc_version:{}!={}",
                self.rustc_version, other.rustc_version
            ));
        }
        if self.target_triple != other.target_triple {
            out.push(format!(
                "target_triple:{}!={}",
                self.target_triple, other.target_triple
            ));
        }
        if self.crate_hash != other.crate_hash {
            out.push(format!(
                "crate_hash:{}!={}",
                self.crate_hash, other.crate_hash
            ));
        }
        if self.api_hash != other.api_hash {
            out.push(format!("api_hash:{}!={}", self.api_hash, other.api_hash));
        }
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDocs {
    pub plugin_id: String,
    pub plugin_path: String,
    pub plugin_version: String,
    pub abi_version: u32,
    #[serde(default)]
    pub command_name: Option<String>,
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeDoc {
    pub id: String,
    pub summary: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    #[serde(default)]
    pub side_effects: Vec<String>,
    #[serde(default)]
    pub failure_modes: Vec<String>,
    /// Node type: Task (long-running background service), Router, Gate, or
    /// Terminal.  Defaults to Router for backward compatibility.
    #[serde(default)]
    pub node_type: NodeType,
}

/// Class of execution semantics for a plugin node.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    /// Long-running background service with lifecycle (start/stop).
    Task,
    /// Conditionally routes execution to one of several downstream nodes.
    Router,
    /// Guards a subgraph behind a policy check.
    Gate,
    /// Terminal node — produces a final output and ends the execution.
    Terminal,
}

impl Default for NodeType {
    fn default() -> Self {
        NodeType::Router
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct PluginRequest {
    pub payload: String,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct PluginResponse {
    pub payload: String,
}

#[repr(C)]
pub struct RustPluginApiV2 {
    pub abi_kind: DylibAbiKind,
    pub abi_fingerprint: fn() -> PluginResponse,
    pub docs: fn() -> PluginResponse,
    pub handle: fn(PluginRequest) -> PluginResponse,
}

pub fn json_response<T: Serialize>(value: &T) -> PluginResponse {
    PluginResponse {
        payload: serde_json::to_string(value).expect("plugin sdk serialize response"),
    }
}

pub fn pretty_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("plugin sdk serialize pretty json")
}

pub fn plugin_docs(
    plugin_id: impl Into<String>,
    plugin_path: impl Into<String>,
    plugin_version: impl Into<String>,
    command_name: Option<&str>,
    nodes: Vec<NodeDoc>,
) -> PluginDocs {
    PluginDocs {
        plugin_id: plugin_id.into(),
        plugin_path: plugin_path.into(),
        plugin_version: plugin_version.into(),
        abi_version: DEFAULT_ABI_VERSION,
        command_name: command_name.map(ToString::to_string),
        nodes,
    }
}

pub fn node_doc(
    id: impl Into<String>,
    summary: impl Into<String>,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    side_effects: &[&str],
    failure_modes: &[&str],
) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        summary: summary.into(),
        input_schema,
        output_schema,
        side_effects: side_effects.iter().map(|v| (*v).to_string()).collect(),
        failure_modes: failure_modes.iter().map(|v| (*v).to_string()).collect(),
        node_type: NodeType::Router,
    }
}

pub fn task_node_doc(
    id: impl Into<String>,
    summary: impl Into<String>,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    side_effects: &[&str],
    failure_modes: &[&str],
) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        summary: summary.into(),
        input_schema,
        output_schema,
        side_effects: side_effects.iter().map(|v| (*v).to_string()).collect(),
        failure_modes: failure_modes.iter().map(|v| (*v).to_string()).collect(),
        node_type: NodeType::Task,
    }
}

#[macro_export]
macro_rules! export_plugin_api {
    (
        abi_fingerprint = $abi_fingerprint:expr,
        docs = $docs:expr,
        handle = $handle:path $(,)?
    ) => {
        fn __cordis_sdk_abi_fingerprint() -> $crate::PluginResponse {
            $crate::json_response(&$abi_fingerprint)
        }

        fn __cordis_sdk_docs() -> $crate::PluginResponse {
            $crate::json_response(&$docs)
        }

        #[no_mangle]
        pub static cordis_plugin_api_rust_v2: $crate::RustPluginApiV2 = $crate::RustPluginApiV2 {
            abi_kind: $crate::DylibAbiKind::Rust,
            abi_fingerprint: __cordis_sdk_abi_fingerprint,
            docs: __cordis_sdk_docs,
            handle: $handle,
        };
    };
}
