//! Core data contracts shared across resolver/loader/context.
//! Shared ABI/docs contracts are sourced from `cordis-plugin-sdk` so runtime and plugins
//! compile against the same symbol table and JSON schema types.

use serde::{Deserialize, Serialize};

pub use cordis_plugin_sdk::{
    AbiFingerprint, DylibAbiKind, NodeDoc, PluginDocs, PluginRequest, PluginResponse,
    RustPluginApiV2, RUST_PLUGIN_ENTRY_SYMBOL,
};

fn default_required() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChildPluginSpec {
    /// Relative path from parent plugin dir (must be direct child).
    pub source: String,
    /// If true, child init failure propagates to parent chain.
    #[serde(default = "default_required")]
    pub required: bool,
    /// Parent-to-child explicit service allowlist.
    #[serde(default)]
    pub grants: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CordisMetadata {
    /// Canonical plugin path (`root/child/...`), must match directory.
    pub plugin_path: String,
    #[serde(default)]
    pub abi_kind: DylibAbiKind,
    /// Strict ABI identity; all fields must match at load time.
    pub abi_fingerprint: AbiFingerprint,
    #[serde(default)]
    pub children: Vec<ChildPluginSpec>,
    /// Optional declared nodes for contract-level checks.
    #[serde(default)]
    pub declared_nodes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LoaderBudget {
    /// Hard safety budget for total discovered plugins.
    pub max_total_plugins: usize,
    /// Hard safety budget for total declared nodes.
    pub max_total_nodes: usize,
    /// Max load phase wall-clock budget.
    pub load_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactIndexEntry {
    /// Identity key for lookup.
    pub plugin_path: String,
    pub version: String,
    pub abi_fingerprint: AbiFingerprint,
    /// Path to prebuilt artifact (relative to index file or absolute).
    pub artifact_path: String,
    /// Content hash used to prevent tampering/drift.
    pub sha256: String,
    pub built_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactIndex {
    pub entries: Vec<ArtifactIndexEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginUnavailableReason {
    AbiMismatch,
    SymbolMissing,
    InitFailed,
    BudgetExceeded,
    ArtifactMissing,
    HashMismatch,
    ContractViolation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginLoadResult {
    /// Plugin is available and registered.
    Loaded,
    /// Plugin was discovered but not available for injection/execution.
    Unavailable(PluginUnavailableReason),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginArtifact {
    /// Must match the plugin being instantiated.
    pub plugin_path: String,
    pub abi_fingerprint: AbiFingerprint,
    /// Runtime docs exposed to registry/agent.
    pub docs: PluginDocs,
    /// Local services exported by the plugin for child injection.
    #[serde(default)]
    pub exports: Vec<String>,
    /// Optional execution strategy used by runtime invocation.
    #[serde(default)]
    pub execution: Option<PluginExecution>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PluginExecution {
    Process {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeOutcome {
    Success,
    Failure,
    Timeout,
    Cancelled,
    Skipped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GatePolicy {
    AllOf,
    AnyOf,
    FirstSuccess,
    FirstCompleted,
    AtLeast(usize),
}
