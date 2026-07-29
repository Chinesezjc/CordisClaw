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

    #[error(
        "artifact hash mismatch for plugin {plugin_path}: expected {expected}, actual {actual}"
    )]
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
        // Boxed: two inline `AbiFingerprint`s (4 Strings each, ~96B apiece)
        // made this the largest variant and inflated every
        // `Result<_, RuntimeError>` on its success path (clippy
        // result_large_err). Boxing keeps the enum small; ABI mismatches
        // are cold-path so the extra allocation is irrelevant.
        expected: Box<AbiFingerprint>,
        actual: Box<AbiFingerprint>,
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

    #[error("net build failed: {message}")]
    NetBuild { message: String },

    #[error("execution failed: execution_id={execution_id}, message={message}")]
    ExecutionFailed {
        execution_id: String,
        message: String,
    },

    #[error("plugin docs not found: {plugin_path}")]
    PluginDocsNotFound { plugin_path: String },

    #[error("node docs not found: {plugin_path}::{node_id}")]
    NodeDocsNotFound {
        plugin_path: String,
        node_id: String,
    },

    #[error("invalid docs route path: {path}")]
    InvalidDocsRoute { path: String },

    #[error("service permission denied for plugin {plugin_path}: {service}")]
    PermissionDenied {
        plugin_path: String,
        service: String,
    },

    #[error("plugin unavailable in context: {plugin_path}")]
    ContextPluginUnavailable { plugin_path: String },

    #[error("candidate snapshot not staged")]
    CandidateSnapshotMissing,

    #[error("plugin iteration already active: {iteration_id}")]
    PluginIterationActive { iteration_id: String },

    #[error("plugin iteration issue not found: {issue_id}")]
    PluginIterationIssueNotFound { issue_id: String },

    #[error("plugin iteration status not found: {iteration_id}")]
    PluginIterationStatusNotFound { iteration_id: String },

    #[error("plugin iteration policy blocked path {path}: {reason}")]
    PluginIterationPolicyBlocked { path: String, reason: String },

    #[error("agent session not found: {session_id}")]
    AgentSessionNotFound { session_id: String },

    #[error("service not found in context for plugin {plugin_path}: {service}")]
    ServiceNotFound {
        plugin_path: String,
        service: String,
    },

    #[error("service type mismatch in context for plugin {plugin_path}: {service}")]
    ServiceTypeMismatch {
        plugin_path: String,
        service: String,
    },

    #[error("duplicate service in same scope for plugin {plugin_path}: {service}")]
    DuplicateService {
        plugin_path: String,
        service: String,
    },

    #[error("context serialize failed for key {key}: {message}")]
    ContextSerialize { key: String, message: String },

    #[error("context deserialize failed for key {key}: {message}")]
    ContextDeserialize { key: String, message: String },

    #[error(
        "context schema version incompatible for key {key}: expected={expected}, actual={actual}"
    )]
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

/// errno 描述文本，用于把"基础设施故障"从"插件代码有问题"里分出来。
///
/// 判据基于错误文本而非 errno 字段：全仓 `RuntimeError::Io` 有 175 处结构体
/// 字面量构造，加必填字段的改动面不可接受；而 `std::io::Error::to_string()`
/// 本身就内嵌 errno 描述（"No space left on device (os error 28)"），因此那
/// 175 处无需改造即可被识别。cargo/rustc 的 ENOSPC 更是只以 stderr 文本形式
/// 存活（`rebuild_plugin_workspace` 把 stderr 塞进 `InvalidArgument.message`），
/// 除文本外没有别的信号可用。
const INFRASTRUCTURE_ERROR_MARKERS: &[&str] = &[
    // ENOSPC
    "no space left on device",
    // EDQUOT
    "disk quota exceeded",
    "quota exceeded",
    // EFBIG
    "file too large",
];

impl RuntimeError {
    /// 是否为基础设施故障（磁盘满 / 配额耗尽 / 文件过大）而非插件自身的缺陷。
    ///
    /// plugin-iteration 用它把 ENOSPC 判成 `InfrastructureFailure` 而不是
    /// `RolledBack`：后者读作"验证失败、插件有问题"，会污染 rollback 率、把
    /// 插件 issue 标成 Open，并且丢掉重试入口。
    pub fn is_infrastructure_failure(&self) -> bool {
        let message = match self {
            Self::Io { message, .. } => message,
            Self::InvalidArgument { message } => message,
            Self::CommandFailed { message, .. } => message,
            Self::AutoUpdateVerifyFailed { message } => message,
            _ => return false,
        };
        let lowered = message.to_ascii_lowercase();
        INFRASTRUCTURE_ERROR_MARKERS
            .iter()
            .any(|marker| lowered.contains(marker))
    }
}

/// 精确 errno 判定，供仍持有 `std::io::Error` 的收口点使用
/// （`region_io_error` / `host_io_error`）。`ErrorKind::StorageFull` 在 stable
/// 尚未稳定，故直接比对 raw errno。
#[cfg(unix)]
pub fn io_error_is_infrastructure(err: &std::io::Error) -> bool {
    matches!(
        err.raw_os_error(),
        Some(libc::ENOSPC) | Some(libc::EDQUOT) | Some(libc::EFBIG)
    )
}

#[cfg(not(unix))]
pub fn io_error_is_infrastructure(_err: &std::io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{io_error_is_infrastructure, RuntimeError};
    use std::path::PathBuf;

    fn io(message: &str) -> RuntimeError {
        RuntimeError::Io {
            path: PathBuf::from("/tmp/x"),
            message: message.to_string(),
        }
    }

    #[test]
    fn is_infrastructure_failure_detects_enospc_text() {
        // `io::Error::to_string()` 的实际形状。
        assert!(io("No space left on device (os error 28)").is_infrastructure_failure());
        // 大小写无关。
        assert!(io("NO SPACE LEFT ON DEVICE").is_infrastructure_failure());
        assert!(io("Disk quota exceeded (os error 69)").is_infrastructure_failure());
        assert!(io("File too large (os error 27)").is_infrastructure_failure());
    }

    #[test]
    fn is_infrastructure_failure_covers_cargo_stderr_variants() {
        // `rebuild_plugin_workspace` 把 cargo stderr 包成 InvalidArgument。
        assert!(RuntimeError::InvalidArgument {
            message: "cargo build -p demo failed: error: No space left on device".to_string(),
        }
        .is_infrastructure_failure());
        assert!(RuntimeError::CommandFailed {
            program: "cargo".to_string(),
            args: vec!["build".to_string()],
            message: "no space left on device".to_string(),
        }
        .is_infrastructure_failure());
    }

    #[test]
    fn is_infrastructure_failure_rejects_genuine_plugin_failures() {
        // 普通编译错：插件自己的问题，必须仍走 RolledBack。
        assert!(!RuntimeError::InvalidArgument {
            message: "cargo build -p demo failed: error[E0308]: mismatched types".to_string(),
        }
        .is_infrastructure_failure());
        assert!(!io("No such file or directory (os error 2)").is_infrastructure_failure());
        // 非文本承载型变体一律 false。
        assert!(!RuntimeError::PluginIterationPolicyBlocked {
            path: "plugins/demo".to_string(),
            reason: "outside the plugin iteration surface".to_string(),
        }
        .is_infrastructure_failure());
        assert!(!RuntimeError::Invariant {
            message: "journal missing".to_string(),
        }
        .is_infrastructure_failure());
    }

    #[cfg(unix)]
    #[test]
    fn io_error_is_infrastructure_matches_enospc_errno() {
        assert!(io_error_is_infrastructure(
            &std::io::Error::from_raw_os_error(libc::ENOSPC)
        ));
        assert!(io_error_is_infrastructure(
            &std::io::Error::from_raw_os_error(libc::EDQUOT)
        ));
        assert!(!io_error_is_infrastructure(
            &std::io::Error::from_raw_os_error(libc::ENOENT)
        ));
        // 没有 raw errno 的合成错误不应被误判。
        assert!(!io_error_is_infrastructure(&std::io::Error::other(
            "synthetic"
        )));
    }
}
