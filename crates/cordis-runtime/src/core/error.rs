use crate::core::models::{AbiFingerprint, PluginUnavailableReason};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("workspace manifest missing or invalid at {path}")]
    InvalidWorkspace { path: PathBuf },

    #[error("failed to parse Cargo.toml at {path}: {message}")]
    CargoParse { path: PathBuf, message: String },

    #[error("missing package.metadata.cordis in {path}")]
    MissingCordisMetadata { path: PathBuf },

    #[error("plugin_path mismatch for {path}: expected {expected}, got {actual}")]
    PluginPathMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("crate name mismatch for {path}: expected {expected}, got {actual}")]
    CrateNameMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("invalid child source {child_source} under {parent}: {reason}")]
    InvalidChildSource {
        parent: String,
        child_source: String,
        reason: String,
    },

    #[error("child plugin path does not exist under {parent}: {child_source}")]
    ChildNotFound {
        parent: String,
        child_source: String,
    },

    #[error("duplicate plugin_path detected: {plugin_path} at {first} and {second}")]
    DuplicatePluginPath {
        plugin_path: String,
        first: PathBuf,
        second: PathBuf,
    },

    #[error("plugin graph cycle detected: {cycle:?}")]
    CycleDetected { cycle: Vec<String> },

    #[error("missing plugin scaffold for {plugin_path}: {missing:?}")]
    MissingScaffold {
        plugin_path: String,
        missing: Vec<String>,
    },

    #[error("docs contract invalid for {plugin_path}: {message}")]
    DocsContract {
        plugin_path: String,
        message: String,
    },

    #[error("artifact index parse failed at {path}: {message}")]
    ArtifactIndexParse { path: PathBuf, message: String },

    #[error("config parse failed at {path}: {message}")]
    ConfigParse { path: PathBuf, message: String },

    #[error("artifact index missing entry for plugin {plugin_path}")]
    ArtifactIndexMissing { plugin_path: String },

    #[error("artifact file missing for plugin {plugin_path}: {artifact_path}")]
    ArtifactFileMissing {
        plugin_path: String,
        artifact_path: PathBuf,
    },

    #[error("artifact hash mismatch for plugin {plugin_path}: expected {expected}, actual {actual}")]
    ArtifactHashMismatch {
        plugin_path: String,
        expected: String,
        actual: String,
    },

    #[error(
        "ABI mismatch for plugin {plugin_path}: expected={expected:?}, actual={actual:?}, diff={fingerprint_diff:?}"
    )]
    AbiMismatch {
        plugin_path: String,
        expected: AbiFingerprint,
        actual: AbiFingerprint,
        fingerprint_diff: Vec<String>,
    },

    #[error("plugin unavailable: {plugin_path}, reason={reason:?}, required={required}")]
    PluginUnavailable {
        plugin_path: String,
        reason: PluginUnavailableReason,
        required: bool,
    },

    #[error("plugin not registered: {plugin_path}")]
    PluginNotRegistered { plugin_path: String },

    #[error("plugin execution unsupported for {plugin_path}: artifact={artifact_path}")]
    PluginExecutionUnsupported {
        plugin_path: String,
        artifact_path: PathBuf,
    },

    #[error("plugin invocation failed for {plugin_path}: {message}")]
    PluginInvocationFailed {
        plugin_path: String,
        message: String,
    },

    #[error("budget exceeded: max_total_plugins={max_total_plugins}, max_total_nodes={max_total_nodes}, actual_plugins={actual_plugins}, actual_nodes={actual_nodes}")]
    BudgetExceeded {
        max_total_plugins: usize,
        max_total_nodes: usize,
        actual_plugins: usize,
        actual_nodes: usize,
    },

    #[error("loader timeout exceeded: limit_ms={limit_ms}, elapsed_ms={elapsed_ms}")]
    LoadTimeout { limit_ms: u64, elapsed_ms: u128 },

    #[error("node_fqn conflict: {node_fqn} first seen in {first}, again in {second}")]
    NodeFqnConflict {
        node_fqn: String,
        first: String,
        second: String,
    },

    #[error("dag build failed: {message}")]
    DagBuild { message: String },

    #[error("execution failed: execution_id={execution_id}, message={message}")]
    ExecutionFailed {
        execution_id: String,
        message: String,
    },

    #[error("plugin docs not found: {plugin_path}")]
    PluginDocsNotFound { plugin_path: String },

    #[error("node docs not found: {plugin_path}::{node_id}")]
    NodeDocsNotFound { plugin_path: String, node_id: String },

    #[error("invalid docs route path: {path}")]
    InvalidDocsRoute { path: String },

    #[error("service permission denied for plugin {plugin_path}: {service}")]
    PermissionDenied { plugin_path: String, service: String },

    #[error("plugin unavailable in context: {plugin_path}")]
    ContextPluginUnavailable { plugin_path: String },

    #[error("service not found in context for plugin {plugin_path}: {service}")]
    ServiceNotFound { plugin_path: String, service: String },

    #[error("service type mismatch in context for plugin {plugin_path}: {service}")]
    ServiceTypeMismatch { plugin_path: String, service: String },

    #[error("duplicate service in same scope for plugin {plugin_path}: {service}")]
    DuplicateService { plugin_path: String, service: String },

    #[error("context serialize failed for key {key}: {message}")]
    ContextSerialize { key: String, message: String },

    #[error("context deserialize failed for key {key}: {message}")]
    ContextDeserialize { key: String, message: String },

    #[error("context schema version incompatible for key {key}: expected={expected}, actual={actual}")]
    ContextVersionIncompatible {
        key: String,
        expected: u32,
        actual: u32,
    },

    #[error("subgraph already active: {current}")]
    SubgraphAlreadyActive { current: String },

    #[error("subgraph not found: {subgraph_id}")]
    SubgraphNotFound { subgraph_id: String },

    #[error("session commit conflict for session={session_id}: expected={expected_version}, actual={actual_version}")]
    CommitConflict {
        session_id: String,
        expected_version: u64,
        actual_version: u64,
    },

    #[error("auto update invalid patch path {path}: {reason}")]
    AutoUpdateInvalidPath { path: String, reason: String },

    #[error("auto update patch pattern not found in {path}: {pattern}")]
    AutoUpdatePatternNotFound { path: PathBuf, pattern: String },

    #[error("auto update patch invalid for {path}: {reason}")]
    AutoUpdatePatchInvalid { path: String, reason: String },

    #[error("auto update verify failed: {message}")]
    AutoUpdateVerifyFailed { message: String },

    #[error("artifact build lock timeout at {path}: waited {waited_ms}ms")]
    ArtifactBuildLockTimeout { path: PathBuf, waited_ms: u128 },

    #[error("command failed: {program} {args:?}: {message}")]
    CommandFailed {
        program: String,
        args: Vec<String>,
        message: String,
    },

    #[error("invalid argument: {message}")]
    InvalidArgument { message: String },

    #[error("LLM API key missing: set {env_name} or config llm_api.api_key")]
    MissingLlmApiKey { env_name: String },

    #[error("LLM provider unsupported: {provider}")]
    UnsupportedLlmProvider { provider: String },

    #[error("LLM request failed: {message}")]
    LlmRequestFailed { message: String },

    #[error("LLM response invalid: {message}")]
    LlmResponseInvalid { message: String },

    #[error("internal invariant broken: {message}")]
    Invariant { message: String },

    #[error("I/O at {path}: {message}")]
    Io { path: PathBuf, message: String },
}
