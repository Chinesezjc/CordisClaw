//! Core data contracts shared across resolver/loader/context.
//! Shared ABI/docs contracts are sourced from `cordis-plugin-sdk` so runtime and plugins
//! compile against the same symbol table and JSON schema types.

use serde::{Deserialize, Serialize};

pub use cordis_plugin_sdk::{
    AbiFingerprint, DylibAbiKind, NodeDoc, PluginDocs, PluginRequest, PluginResponse,
    RustPluginApiV2, RUST_PLUGIN_ENTRY_SYMBOL,
};

pub const ARTIFACT_INDEX_SCHEMA_VERSION: u32 = 2;

fn default_required() -> bool {
    true
}

fn default_artifact_index_schema_version() -> u32 {
    ARTIFACT_INDEX_SCHEMA_VERSION
}

fn default_artifact_kind() -> ArtifactKind {
    ArtifactKind::Json
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
    /// P1-48: opt-in to allow the runtime to synthesise a docs stub when
    /// `docs/agent/interfaces.json` is missing. Previously ANY dylib
    /// plugin could skip the docs contract just by declaring
    /// `crate-type=["dylib"]`. Now the plugin author must explicitly say
    /// `allow_generated_docs = true` in `[package.metadata.cordis]` —
    /// defaulting to false keeps drift discoverable.
    #[serde(default)]
    pub allow_generated_docs: bool,
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
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Dylib,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct InputProbe {
    #[serde(default)]
    pub files: Vec<InputProbeFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct InputProbeFile {
    pub path: String,
    pub size: u64,
    pub modified_at_ms: u128,
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
    #[serde(default)]
    pub parent: Option<String>,
    #[serde(default = "default_required")]
    pub required: bool,
    #[serde(default)]
    pub grants_from_parent: Vec<String>,
    pub docs: PluginDocs,
    #[serde(default)]
    pub exports: Vec<String>,
    #[serde(default)]
    pub execution: Option<PluginExecution>,
    #[serde(default = "default_artifact_kind")]
    pub artifact_kind: ArtifactKind,
    pub build_fingerprint: String,
    #[serde(default)]
    pub input_probe: InputProbe,
    #[serde(default)]
    pub local_path_deps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArtifactIndex {
    #[serde(default = "default_artifact_index_schema_version")]
    pub schema_version: u32,
    pub generated_at: String,
    #[serde(default)]
    pub topo_order: Vec<String>,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Exercises `default_required` via serde: `required` omitted must
    // deserialize to `true`, and `grants` omitted to an empty vec.
    #[test]
    fn child_plugin_spec_defaults_required_true_and_empty_grants() {
        let spec: ChildPluginSpec =
            serde_json::from_str(r#"{"source": "child"}"#).expect("deserialize ChildPluginSpec");
        assert_eq!(spec.source, "child");
        assert!(spec.required, "default_required must yield true");
        assert!(spec.grants.is_empty(), "grants must default to empty");
    }

    // Explicit `required: false` must override the default.
    #[test]
    fn child_plugin_spec_required_can_be_overridden() {
        let spec: ChildPluginSpec =
            serde_json::from_str(r#"{"source": "child", "required": false}"#)
                .expect("deserialize ChildPluginSpec");
        assert!(!spec.required);
    }

    // Exercises `default_artifact_index_schema_version`: omitting
    // `schema_version` must yield `ARTIFACT_INDEX_SCHEMA_VERSION`.
    #[test]
    fn artifact_index_defaults_schema_version() {
        let index: ArtifactIndex = serde_json::from_str(
            r#"{"generated_at": "now", "entries": []}"#,
        )
        .expect("deserialize ArtifactIndex");
        assert_eq!(index.schema_version, ARTIFACT_INDEX_SCHEMA_VERSION);
        assert_eq!(index.schema_version, 2);
        assert!(index.topo_order.is_empty());
    }

    // A supplied `schema_version` must be preserved (default fn not called).
    #[test]
    fn artifact_index_preserves_explicit_schema_version() {
        let index: ArtifactIndex = serde_json::from_str(
            r#"{"schema_version": 9, "generated_at": "now", "entries": []}"#,
        )
        .expect("deserialize ArtifactIndex");
        assert_eq!(index.schema_version, 9);
    }

    // Exercises `default_artifact_kind`: omitting `artifact_kind` on an
    // entry must yield `ArtifactKind::Json`, and omitting `required`
    // must yield `true`.
    #[test]
    fn artifact_index_entry_defaults_artifact_kind_json_and_required_true() {
        let entry: ArtifactIndexEntry = serde_json::from_str(
            r#"{
                "plugin_path": "p",
                "version": "0.1.0",
                "abi_fingerprint": {
                    "rustc_version": "rustc",
                    "target_triple": "triple",
                    "crate_hash": "c",
                    "api_hash": "api_v2"
                },
                "artifact_path": "p.json",
                "sha256": "deadbeef",
                "built_at": "now",
                "docs": {
                    "plugin_id": "p",
                    "plugin_path": "p",
                    "plugin_version": "0.1.0",
                    "abi_version": 2,
                    "nodes": []
                },
                "build_fingerprint": "bf"
            }"#,
        )
        .expect("deserialize ArtifactIndexEntry");
        assert_eq!(entry.artifact_kind, ArtifactKind::Json);
        assert!(entry.required, "default_required must yield true for entry");
        assert!(entry.parent.is_none());
        assert!(entry.exports.is_empty());
    }
}
