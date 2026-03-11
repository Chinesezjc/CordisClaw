//! Core data contracts shared across resolver/loader/context.
//! These types mirror the architecture contract in `plan.md`.

use serde::{Deserialize, Serialize};

/// Fixed symbol name required by the Rust ABI dylib contract.
pub const RUST_PLUGIN_ENTRY_SYMBOL: &str = "cordis_plugin_api_rust_v2";

fn default_required() -> bool {
    true
}

#[repr(u8)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DylibAbiKind {
    /// Only pure Rust ABI is accepted for `dylib` in this runtime.
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
    /// Rust compiler version used to build the artifact.
    pub rustc_version: String,
    /// Target platform triple used for build.
    pub target_triple: String,
    /// Crate-level hash used to detect binary drift.
    pub crate_hash: String,
    /// API-level hash used to detect interface drift.
    pub api_hash: String,
}

impl AbiFingerprint {
    /// Human-readable mismatch list for logs/telemetry.
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
            out.push(format!("crate_hash:{}!={}", self.crate_hash, other.crate_hash));
        }
        if self.api_hash != other.api_hash {
            out.push(format!("api_hash:{}!={}", self.api_hash, other.api_hash));
        }
        out
    }
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

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDocs {
    pub plugin_id: String,
    pub plugin_path: String,
    pub plugin_version: String,
    pub abi_version: u32,
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
