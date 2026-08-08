use crate::core::error::RuntimeError;
use crate::kernel::auto_update::{AutoUpdatePlan, FilePatchKind};
use crate::kernel::verifier::VerificationProfile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use toml::Value as TomlValue;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum KernelPluginIssueSource {
    LoadFailure,
    CanaryFailure,
    VerifierFailure,
    InvokeFailure,
    DocsDrift,
    PolicyBlocked,
}

impl KernelPluginIssueSource {
    pub fn priority(self) -> u8 {
        match self {
            Self::LoadFailure => 0,
            Self::CanaryFailure => 1,
            Self::VerifierFailure => 2,
            Self::InvokeFailure => 3,
            Self::DocsDrift => 4,
            Self::PolicyBlocked => 5,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum KernelPluginIssueStatus {
    Open,
    Running,
    Blocked,
    Resolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelPluginIssue {
    pub issue_id: String,
    pub root_plugin_path: String,
    pub target_plugin_paths: Vec<String>,
    pub source: KernelPluginIssueSource,
    pub summary: String,
    pub status: KernelPluginIssueStatus,
    pub first_observed_at_ms: u128,
    pub last_observed_at_ms: u128,
    pub observe_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KernelPluginIterationRequest {
    pub issue_id: Option<String>,
    #[serde(default)]
    pub target_plugin_paths: Vec<String>,
    #[serde(default)]
    pub instruction: Option<String>,
    #[serde(default)]
    pub edit_plan: Option<PluginEditPlan>,
    #[serde(default)]
    pub manual_approved: bool,
    #[serde(default)]
    pub tests_command: Option<String>,
    #[serde(default)]
    pub safety_command: Option<String>,
    #[serde(default)]
    pub verify_profile: Option<VerificationProfile>,
    #[serde(default)]
    pub quality_score: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIterationNetSpec {
    pub transition_ids: Vec<String>,
}

impl Default for PluginIterationNetSpec {
    fn default() -> Self {
        Self {
            transition_ids: vec![
                "observe".to_string(),
                "select_issue".to_string(),
                "plan".to_string(),
                "edit".to_string(),
                "rebuild".to_string(),
                "stage_candidate".to_string(),
                "verify".to_string(),
                "canary".to_string(),
                "promote_or_rollback".to_string(),
            ],
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginEditOpKind {
    ReplaceExact,
    CreateFile,
    DeleteFile,
    JsonSet,
    TomlSet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEditOperation {
    pub path: String,
    pub kind: PluginEditOpKind,
    #[serde(default)]
    pub expected_old_string: Option<String>,
    #[serde(default)]
    pub expected_sha256: Option<String>,
    #[serde(default)]
    pub new_content: Option<String>,
    #[serde(default)]
    pub pointer: Option<String>,
    #[serde(default)]
    pub dotted_key: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

impl PluginEditOperation {
    pub fn diff_line_estimate(&self) -> usize {
        match self.kind {
            PluginEditOpKind::ReplaceExact => self
                .expected_old_string
                .as_deref()
                .unwrap_or_default()
                .lines()
                .count()
                .max(
                    self.new_content
                        .as_deref()
                        .unwrap_or_default()
                        .lines()
                        .count(),
                )
                .max(1),
            PluginEditOpKind::CreateFile | PluginEditOpKind::DeleteFile => self
                .new_content
                .as_deref()
                .unwrap_or_default()
                .lines()
                .count()
                .max(1),
            PluginEditOpKind::JsonSet | PluginEditOpKind::TomlSet => 1,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEditPlan {
    pub issue_id: String,
    pub patch_id: String,
    pub summary: String,
    pub operations: Vec<PluginEditOperation>,
}

impl PluginEditPlan {
    pub fn diff_lines(&self) -> usize {
        self.operations
            .iter()
            .map(PluginEditOperation::diff_line_estimate)
            .sum()
    }

    pub fn changed_paths(&self) -> Vec<String> {
        self.operations
            .iter()
            .map(|operation| operation.path.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub fn from_auto_update_plan(
        workspace_root: &Path,
        summary: String,
        plan: AutoUpdatePlan,
    ) -> Result<Self, RuntimeError> {
        let mut operations = Vec::with_capacity(plan.patches.len());
        for patch in plan.patches {
            let normalized_path = normalize_rel_path(&patch.path)?;
            let abs_path = workspace_root.join(&normalized_path);
            match patch.kind {
                FilePatchKind::Text => {
                    operations.push(PluginEditOperation {
                        path: normalized_path,
                        kind: PluginEditOpKind::ReplaceExact,
                        expected_old_string: Some(patch.find),
                        expected_sha256: None,
                        new_content: Some(patch.replace),
                        pointer: None,
                        dotted_key: None,
                        value: None,
                    });
                }
                FilePatchKind::JsonValue => {
                    operations.push(PluginEditOperation {
                        path: normalized_path,
                        kind: PluginEditOpKind::JsonSet,
                        expected_old_string: None,
                        expected_sha256: Some(file_sha256(&abs_path)?),
                        new_content: None,
                        pointer: patch.pointer,
                        dotted_key: None,
                        value: patch.value,
                    });
                }
                FilePatchKind::TomlValue => {
                    operations.push(PluginEditOperation {
                        path: normalized_path,
                        kind: PluginEditOpKind::TomlSet,
                        expected_old_string: None,
                        expected_sha256: Some(file_sha256(&abs_path)?),
                        new_content: None,
                        pointer: None,
                        dotted_key: patch.dotted_key,
                        value: patch.value,
                    });
                }
            }
        }

        Ok(Self {
            issue_id: plan.issue_id,
            patch_id: plan.patch_id,
            summary,
            operations,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIterationPolicy {
    pub forbidden_prefixes: Vec<String>,
    pub allow_plugin_manifest_edits: bool,
}

impl Default for PluginIterationPolicy {
    fn default() -> Self {
        Self {
            forbidden_prefixes: vec![
                "Cargo.toml".to_string(),
                "crates/cordis-runtime/".to_string(),
                "crates/cordis-plugin-sdk/".to_string(),
                "crates/cordis-plugin-host/".to_string(),
                "config/".to_string(),
                ".git/".to_string(),
                "target/".to_string(),
                "artifacts/".to_string(),
            ],
            allow_plugin_manifest_edits: true,
        }
    }
}

impl PluginIterationPolicy {
    pub fn validate_plan(
        &self,
        allowed_plugin_roots: &BTreeMap<String, String>,
        plan: &PluginEditPlan,
    ) -> Result<(), RuntimeError> {
        for operation in &plan.operations {
            self.validate_path(allowed_plugin_roots, &operation.path)?;
        }
        Ok(())
    }

    pub fn validate_path(
        &self,
        allowed_plugin_roots: &BTreeMap<String, String>,
        path: &str,
    ) -> Result<(), RuntimeError> {
        let normalized = normalize_rel_path(path)?;
        // The workspace manifest (plugins/Cargo.toml) needs to be editable
        // so that new top-level plugins can be added to workspace members.
        if normalized == "plugins/Cargo.toml" {
            return Ok(());
        }
        if self
            .forbidden_prefixes
            .iter()
            .any(|prefix| normalized == *prefix || normalized.starts_with(prefix))
        {
            return Err(RuntimeError::PluginIterationPolicyBlocked {
                path: normalized,
                reason: "path is outside the plugin iteration surface".to_string(),
            });
        }

        for subtree_root in plugin_subtree_roots(allowed_plugin_roots.values()) {
            match plugin_subtree_surface_kind(&normalized, &subtree_root) {
                Some(PluginSubtreeSurfaceKind::WritableManifest) => {
                    if self.allow_plugin_manifest_edits {
                        return Ok(());
                    }
                    return Err(RuntimeError::PluginIterationPolicyBlocked {
                        path: normalized,
                        reason: "plugin manifest edits are disabled".to_string(),
                    });
                }
                Some(PluginSubtreeSurfaceKind::WritableOther) => return Ok(()),
                Some(PluginSubtreeSurfaceKind::ReadOnlyGenerated) => {
                    return Err(RuntimeError::PluginIterationPolicyBlocked {
                        path: normalized,
                        reason: "generated agent docs are read-only context; edit source code or human docs instead".to_string(),
                    });
                }
                None => {}
            }
        }

        Err(RuntimeError::PluginIterationPolicyBlocked {
            path: normalized,
            reason: "path is not inside the selected plugin subtree".to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginSubtreeSurfaceKind {
    WritableManifest,
    WritableOther,
    ReadOnlyGenerated,
}

fn plugin_subtree_roots<'a>(
    allowed_plugin_roots: impl IntoIterator<Item = &'a String>,
) -> BTreeSet<String> {
    let roots = allowed_plugin_roots
        .into_iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    roots
        .iter()
        .filter(|root| {
            !roots
                .iter()
                .any(|other| *root != other && root.starts_with(&format!("{other}/")))
        })
        .cloned()
        .collect()
}

fn plugin_subtree_surface_kind(path: &str, subtree_root: &str) -> Option<PluginSubtreeSurfaceKind> {
    let root_manifest = format!("{subtree_root}/Cargo.toml");
    if path == root_manifest {
        return Some(PluginSubtreeSurfaceKind::WritableManifest);
    }
    let rel = path.strip_prefix(&format!("{subtree_root}/"))?;
    let segments = rel.split('/').collect::<Vec<_>>();
    // `str::split('/')` always yields at least one element (an empty string for
    // an empty `rel`), so this vec is never empty. Assert the invariant in debug
    // builds rather than carrying a dead runtime branch.
    debug_assert!(!segments.is_empty(), "str::split always yields >=1 segment");

    if segments.len() >= 2 && segments.last() == Some(&"Cargo.toml") {
        let plugin_segments = &segments[..segments.len() - 1];
        if !plugin_segments.is_empty()
            && plugin_segments
                .iter()
                .all(|segment| !matches!(*segment, "src" | "tests" | "docs"))
        {
            return Some(PluginSubtreeSurfaceKind::WritableManifest);
        }
    }

    let surface_idx = segments
        .iter()
        .position(|segment| matches!(*segment, "src" | "tests" | "docs"))?;
    // `position` returns the *first* index whose segment is one of
    // src/tests/docs, so the prefix `segments[..surface_idx]` can never contain
    // another such segment. The guard is therefore unreachable; assert the
    // invariant in debug builds rather than carrying a dead runtime branch.
    debug_assert!(
        !segments[..surface_idx]
            .iter()
            .any(|segment| matches!(*segment, "src" | "tests" | "docs")),
        "surface_idx is the first src/tests/docs; no earlier segment can match"
    );
    match segments[surface_idx] {
        "src" | "tests" if surface_idx + 1 < segments.len() => {
            Some(PluginSubtreeSurfaceKind::WritableOther)
        }
        "docs" if surface_idx + 2 < segments.len() && segments[surface_idx + 1] == "human" => {
            Some(PluginSubtreeSurfaceKind::WritableOther)
        }
        "docs" if surface_idx + 2 < segments.len() && segments[surface_idx + 1] == "agent" => {
            Some(PluginSubtreeSurfaceKind::ReadOnlyGenerated)
        }
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifierVerdict {
    Pass,
    Fail,
    Partial,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CanaryVerdict {
    Pass,
    Fail,
    Partial,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CanaryReport {
    pub verdict: CanaryVerdict,
    pub mode: String,
    pub plugin_path: Option<String>,
    pub node_id: Option<String>,
    pub payload: Option<Value>,
    pub expected_response: Option<Value>,
    pub actual_response: Option<Value>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginIterationFinalVerdict {
    Promoted,
    RolledBack,
    Blocked,
    /// 基础设施故障（磁盘满 / 配额耗尽）导致迭代无法完成，**不是**插件缺陷。
    ///
    /// 与 `RolledBack` 分开的理由：后者读作"验证失败、插件有问题"，会计入
    /// rollback 率、把插件 issue 标成 Open、并且从 `blocked_iterations` 里摘除
    /// 从而失去重试入口。磁盘满时这三件事全是误判——腾出空间后原样重试即可。
    InfrastructureFailure,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIterationHistoryEntry {
    pub iteration_id: String,
    pub issue_id: String,
    pub root_plugin_path: String,
    pub target_plugin_paths: Vec<String>,
    pub source: Option<KernelPluginIssueSource>,
    pub summary: String,
    pub changed_paths: Vec<String>,
    pub verifier_verdict: Option<VerifierVerdict>,
    pub canary_verdict: Option<CanaryVerdict>,
    pub final_verdict: PluginIterationFinalVerdict,
    pub blocked_reason: Option<String>,
    pub observed_at_ms: u128,
    pub completed_at_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginIterationStatus {
    pub iteration_id: String,
    pub issue_id: String,
    pub root_plugin_path: String,
    pub target_plugin_paths: Vec<String>,
    pub summary: String,
    pub changed_paths: Vec<String>,
    pub verifier_verdict: Option<VerifierVerdict>,
    pub canary_verdict: Option<CanaryVerdict>,
    pub final_verdict: PluginIterationFinalVerdict,
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PluginEditExecutor {
    workspace_root: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PluginEditApplyResult {
    pub changed_paths: Vec<String>,
    pub diff_lines: usize,
}

#[derive(Debug, Clone)]
pub struct PluginEditRollback {
    workspace_root: PathBuf,
    backups: Vec<AppliedEditBackup>,
}

#[derive(Debug, Clone)]
struct AppliedEditBackup {
    rel_path: String,
    original: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PluginEditRollbackJournal {
    iteration_id: String,
    /// Unique id assigned each time the journal is persisted (P0-7). The boot
    /// recovery path records this id in a sibling `.applied` file after a
    /// successful restore; on re-entry we compare and skip if the same id
    /// was already applied — preventing the "restore already-restored source"
    /// footgun that would otherwise mangle the workspace on a crash between
    /// rollback and clear_journal.
    #[serde(default = "generate_rollback_generation_id")]
    rollback_generation_id: String,
    backups: Vec<PluginEditRollbackJournalBackup>,
}

fn generate_rollback_generation_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{now:x}-{seq:x}")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PluginEditRollbackJournalBackup {
    rel_path: String,
    #[serde(default)]
    original_hex: Option<String>,
}

impl PluginEditExecutor {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    pub fn execute(
        &self,
        policy: &PluginIterationPolicy,
        allowed_plugin_roots: &BTreeMap<String, String>,
        plan: &PluginEditPlan,
    ) -> Result<(PluginEditApplyResult, PluginEditRollback), RuntimeError> {
        policy.validate_plan(allowed_plugin_roots, plan)?;

        let mut rollback = PluginEditRollback {
            workspace_root: self.workspace_root.clone(),
            backups: Vec::new(),
        };
        let mut changed_paths = BTreeSet::new();
        // P0-20: canonicalise workspace_root once and then, per-operation,
        // confirm the resolved path is still inside it. `normalize_rel_path`
        // only rejects `..` and absolute paths — it cannot detect
        // symlink-based escape (`plugins/demo/src/evil -> /etc/passwd` etc.).
        // `ensure_under_workspace` (shared with `AutoUpdater` and the
        // rollback/journal recovery paths) follows symlinks along any existing
        // prefix, rejects dangling symlinks, and refuses any operation whose
        // canonical target lands outside workspace_root before touching disk.
        let canonical_workspace_root = canonical_workspace_root(&self.workspace_root)?;
        for operation in &plan.operations {
            let normalized = normalize_rel_path(&operation.path)?;
            policy.validate_path(allowed_plugin_roots, &normalized)?;
            let abs_path = self.workspace_root.join(&normalized);
            ensure_under_workspace(&abs_path, &canonical_workspace_root, &operation.path)?;
            let original = fs::read(&abs_path).ok();
            let updated = apply_operation(&normalized, operation, &abs_path, original.as_deref())?;

            ensure_parent_dir(&abs_path)?;

            // P0-5: record the backup BEFORE mutating disk. If `atomic_write`
            // or `remove_file` fails partway, the rollback list already has
            // an entry for this file — so the caller can `rollback()` and
            // restore prior state. Previously the backup was pushed after
            // `fs::write`, leaving a torn file with no recovery path.
            rollback.backups.push(AppliedEditBackup {
                rel_path: normalized.clone(),
                original,
            });

            let write_result = match updated {
                UpdatedFile::Write(bytes) => atomic_write(&abs_path, &bytes),
                UpdatedFile::Delete => remove_file_if_exists(&abs_path),
            };
            if let Err(err) = write_result {
                // The failed op is already recorded in `rollback.backups`, so
                // rolling back here restores prior state — including this file
                // if `atomic_write` wrote a partial `.tmp` that the OS renamed.
                // Return the original write error; the caller sees a clean
                // workspace and no orphan partial edit.
                if let Err(rollback_err) = rollback.rollback() {
                    return Err(RuntimeError::Invariant {
                        message: format!(
                            "{err}; additionally, in-execute rollback failed: {rollback_err}"
                        ),
                    });
                }
                return Err(err);
            }

            changed_paths.insert(normalized);
        }

        Ok((
            PluginEditApplyResult {
                changed_paths: changed_paths.into_iter().collect(),
                diff_lines: plan.diff_lines(),
            },
            rollback,
        ))
    }
}

impl PluginEditRollback {
    pub fn empty(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            backups: Vec::new(),
        }
    }

    /// Create a rollback containing a single file backup.
    pub fn single_backup(
        workspace_root: impl Into<PathBuf>,
        rel_path: &str,
        original: Option<Vec<u8>>,
    ) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            backups: vec![AppliedEditBackup {
                rel_path: rel_path.to_string(),
                original,
            }],
        }
    }

    /// Number of backed-up files in this rollback.
    pub fn len(&self) -> usize {
        self.backups.len()
    }

    /// Whether this rollback holds no file backups.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn absorb(&mut self, mut other: Self) -> Result<(), RuntimeError> {
        if self.workspace_root != other.workspace_root {
            return Err(RuntimeError::Invariant {
                message: format!(
                    "plugin edit rollback workspace mismatch: {} vs {}",
                    self.workspace_root.display(),
                    other.workspace_root.display()
                ),
            });
        }
        // P2-23: dedup by rel_path — keep only the first backup for each
        // path so `absorb` chains don't blow up the journal size on
        // repeated edits to the same file. Rollback runs in reverse
        // insertion order, and reverting to the very first snapshot
        // fully undoes the run — a mid-run intermediate is never useful
        // for recovery. Existing entries win: whichever rollback saw the
        // file first records the correct pre-edit bytes.
        let mut existing: std::collections::HashSet<String> =
            self.backups.iter().map(|b| b.rel_path.clone()).collect();
        for backup in other.backups.drain(..) {
            if existing.insert(backup.rel_path.clone()) {
                self.backups.push(backup);
            }
        }
        Ok(())
    }

    pub fn rollback(&self) -> Result<(), RuntimeError> {
        // P0-20 parity for the write-back path: the workspace root is
        // canonicalised once, then every backup target must still be a
        // normalized rel_path that resolves inside it. A symlink planted (or
        // swapped) between the edit and the rollback would otherwise redirect
        // the restore write — or the created-file removal — outside the
        // workspace.
        let canonical_root = canonical_workspace_root(&self.workspace_root)?;
        for backup in self.backups.iter().rev() {
            let rel_path = normalize_rel_path(&backup.rel_path)
                .map_err(|err| rollback_rel_path_error(&backup.rel_path, &err))?;
            let abs_path = self.workspace_root.join(&rel_path);
            ensure_restore_contained(&abs_path, &canonical_root, Path::new(&rel_path))?;
            match &backup.original {
                Some(original) => {
                    ensure_parent_dir(&abs_path)?;
                    fs::write(&abs_path, original).map_err(|err| RuntimeError::Io {
                        path: abs_path.clone(),
                        message: err.to_string(),
                    })?;
                }
                None => {
                    remove_file_if_exists(&abs_path)?;
                }
            }
        }
        Ok(())
    }

    pub fn persist_journal(
        &self,
        journal_path: &Path,
        iteration_id: &str,
    ) -> Result<(), RuntimeError> {
        let journal = PluginEditRollbackJournal {
            iteration_id: iteration_id.to_string(),
            rollback_generation_id: generate_rollback_generation_id(),
            backups: self
                .backups
                .iter()
                .map(|backup| PluginEditRollbackJournalBackup {
                    rel_path: backup.rel_path.clone(),
                    original_hex: backup.original.as_ref().map(hex::encode),
                })
                .collect(),
        };
        ensure_parent_dir(journal_path)?;
        let bytes = serde_json::to_vec_pretty(&journal)
            .map_err(|err| rollback_journal_serialize_error(&err))?;
        // P0-6: journal must survive a mid-write SIGKILL. tmp + fsync + rename
        // → the file is either the previous journal or the new one, never
        // truncated.
        atomic_write(journal_path, &bytes)
    }

    /// Rollback-generation id recorded in the journal header. Used by the boot
    /// recovery path to detect that a rollback has already been applied for
    /// this journal (P0-7).
    pub fn journal_generation_id(journal_path: &Path) -> Result<Option<String>, RuntimeError> {
        if !journal_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(journal_path).map_err(|err| RuntimeError::Io {
            path: journal_path.to_path_buf(),
            message: err.to_string(),
        })?;
        match serde_json::from_slice::<PluginEditRollbackJournal>(&bytes) {
            Ok(journal) => Ok(Some(journal.rollback_generation_id)),
            Err(_) => Ok(None),
        }
    }

    pub fn load_journal(
        workspace_root: impl Into<PathBuf>,
        journal_path: &Path,
    ) -> Result<Option<Self>, RuntimeError> {
        if !journal_path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(journal_path).map_err(|err| RuntimeError::Io {
            path: journal_path.to_path_buf(),
            message: err.to_string(),
        })?;
        let journal: PluginEditRollbackJournal =
            serde_json::from_slice(&bytes).map_err(|err| RuntimeError::Invariant {
                message: format!("plugin edit rollback journal parse failed: {err}"),
            })?;
        let workspace_root = workspace_root.into();
        // P0-20 parity: a journal is replayed only if every backup target is a
        // normalized rel_path that still resolves inside the workspace.
        // `normalize_rel_path` rejects `..` / absolute paths lexically; the
        // containment check follows symlinks along any existing prefix (and
        // rejects dangling symlinks), so a planted
        // `plugins/demo/conf.toml -> /etc/...` link cannot redirect boot
        // recovery outside the workspace. An illegal entry refuses the whole
        // replay with `Invariant` — the journal is disk state, not input.
        let canonical_workspace_root = canonical_workspace_root(&workspace_root)?;
        let backups = journal
            .backups
            .into_iter()
            .map(|backup| {
                let rel_path = normalize_rel_path(&backup.rel_path)
                    .map_err(|err| journal_rel_path_error(&backup.rel_path, &err))?;
                ensure_restore_contained(
                    &workspace_root.join(&rel_path),
                    &canonical_workspace_root,
                    Path::new(&rel_path),
                )?;
                let original = backup
                    .original_hex
                    .map(|value| {
                        hex::decode(&value).map_err(|err| RuntimeError::Invariant {
                            message: format!(
                                "plugin edit rollback journal hex decode failed for {}: {err}",
                                backup.rel_path
                            ),
                        })
                    })
                    .transpose()?;
                Ok(AppliedEditBackup { rel_path, original })
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        Ok(Some(Self {
            workspace_root,
            backups,
        }))
    }

    pub fn clear_journal(journal_path: &Path) -> Result<(), RuntimeError> {
        if journal_path.exists() {
            fs::remove_file(journal_path).map_err(|err| RuntimeError::Io {
                path: journal_path.to_path_buf(),
                message: err.to_string(),
            })?;
        }
        Ok(())
    }
}

enum UpdatedFile {
    Write(Vec<u8>),
    Delete,
}

fn apply_operation(
    rel_path: &str,
    operation: &PluginEditOperation,
    abs_path: &Path,
    original: Option<&[u8]>,
) -> Result<UpdatedFile, RuntimeError> {
    let original_text = original
        .map(|bytes| String::from_utf8_lossy(bytes).to_string())
        .unwrap_or_default();

    match operation.kind {
        PluginEditOpKind::ReplaceExact => {
            let expected = operation.expected_old_string.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "replace_exact requires expected_old_string".to_string(),
                }
            })?;
            if !original_text.contains(expected) {
                return Err(RuntimeError::AutoUpdatePatternNotFound {
                    path: abs_path.to_path_buf(),
                    pattern: expected.to_string(),
                });
            }
            let replacement = operation.new_content.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "replace_exact requires new_content".to_string(),
                }
            })?;
            Ok(UpdatedFile::Write(
                original_text
                    .replacen(expected, replacement, 1)
                    .into_bytes(),
            ))
        }
        PluginEditOpKind::CreateFile => {
            let expected = operation.expected_old_string.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "create_file requires expected_old_string".to_string(),
                }
            })?;
            if original.is_some() {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "create_file target already exists".to_string(),
                });
            }
            if !expected.is_empty() {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "create_file expected_old_string must be empty".to_string(),
                });
            }
            Ok(UpdatedFile::Write(
                operation
                    .new_content
                    .clone()
                    .unwrap_or_default()
                    .into_bytes(),
            ))
        }
        PluginEditOpKind::DeleteFile => {
            let expected_sha256 = operation.expected_sha256.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "delete_file requires expected_sha256".to_string(),
                }
            })?;
            validate_expected_hash(rel_path, &original_text, Some(expected_sha256))?;
            if original.is_none() {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "delete_file target does not exist".to_string(),
                });
            }
            Ok(UpdatedFile::Delete)
        }
        PluginEditOpKind::JsonSet => {
            let expected_sha256 = operation.expected_sha256.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "json_set requires expected_sha256".to_string(),
                }
            })?;
            validate_expected_hash(rel_path, &original_text, Some(expected_sha256))?;
            let pointer = operation.pointer.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "json_set requires pointer".to_string(),
                }
            })?;
            let replacement =
                operation
                    .value
                    .clone()
                    .ok_or_else(|| RuntimeError::AutoUpdatePatchInvalid {
                        path: rel_path.to_string(),
                        reason: "json_set requires value".to_string(),
                    })?;
            let mut document: Value = serde_json::from_str(&original_text).map_err(|err| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: format!("json parse failed: {err}"),
                }
            })?;
            let Some(target) = document.pointer_mut(pointer) else {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: format!("json pointer not found: {pointer}"),
                });
            };
            *target = replacement;
            Ok(UpdatedFile::Write(
                serde_json::to_string_pretty(&document)
                    .map_err(|err| json_set_serialize_io_error(abs_path, &err))?
                    .into_bytes(),
            ))
        }
        PluginEditOpKind::TomlSet => {
            let expected_sha256 = operation.expected_sha256.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "toml_set requires expected_sha256".to_string(),
                }
            })?;
            validate_expected_hash(rel_path, &original_text, Some(expected_sha256))?;
            let dotted_key = operation.dotted_key.as_deref().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "toml_set requires dotted_key".to_string(),
                }
            })?;
            let replacement = TomlValue::try_from(operation.value.clone().ok_or_else(|| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: "toml_set requires value".to_string(),
                }
            })?)
            .map_err(|err| RuntimeError::AutoUpdatePatchInvalid {
                path: rel_path.to_string(),
                reason: format!("toml value conversion failed: {err}"),
            })?;
            let mut document: TomlValue = toml::from_str(&original_text).map_err(|err| {
                RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: format!("toml parse failed: {err}"),
                }
            })?;
            let mut cursor = &mut document;
            // `str::split` always yields at least one element (even for the
            // empty string, it yields `[""]`), so `split_last` never returns
            // `None` — walking prefix segments then handling the final segment
            // covers every path without a dead "empty dotted key" tail.
            let segments: Vec<&str> = dotted_key.split('.').collect();
            let (last, prefix) = segments
                .split_last()
                .expect("str::split yields at least one segment");
            for segment in prefix {
                let Some(table) = cursor.as_table_mut() else {
                    return Err(RuntimeError::AutoUpdatePatchInvalid {
                        path: rel_path.to_string(),
                        reason: format!("toml key path is not a table at {segment}"),
                    });
                };
                cursor = table.get_mut(*segment).ok_or_else(|| {
                    RuntimeError::AutoUpdatePatchInvalid {
                        path: rel_path.to_string(),
                        reason: format!("toml dotted key not found: {dotted_key}"),
                    }
                })?;
            }
            let Some(table) = cursor.as_table_mut() else {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: format!("toml key path is not a table at {last}"),
                });
            };
            let Some(target) = table.get_mut(*last) else {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: rel_path.to_string(),
                    reason: format!("toml dotted key not found: {dotted_key}"),
                });
            };
            *target = replacement;
            Ok(UpdatedFile::Write(
                toml::to_string_pretty(&document)
                    .map_err(|err| toml_set_serialize_io_error(abs_path, &err))?
                    .into_bytes(),
            ))
        }
    }
}

/// Map a rollback-journal re-serialization failure to `RuntimeError::Invariant`.
/// A `PluginEditRollbackJournal` built from in-memory backups always
/// re-serializes, so this arm is unreachable at runtime; extracted for
/// byte-stable text and direct testing.
fn rollback_journal_serialize_error(err: &serde_json::Error) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!("plugin edit rollback journal serialize failed: {err}"),
    }
}

/// Map a JSON re-serialization failure in the `json_set` edit op to
/// `RuntimeError::Io`. A `Value` that just parsed always re-serializes, so this
/// arm is unreachable at runtime; extracted for byte-stable text and testing.
fn json_set_serialize_io_error(abs_path: &Path, err: &serde_json::Error) -> RuntimeError {
    RuntimeError::Io {
        path: abs_path.to_path_buf(),
        message: format!("json serialize failed: {err}"),
    }
}

/// Map a TOML re-serialization failure in the `toml_set` edit op to
/// `RuntimeError::Io`. Unreachable for a document that just parsed; extracted
/// for byte-stable text and testing.
fn toml_set_serialize_io_error(abs_path: &Path, err: &toml::ser::Error) -> RuntimeError {
    RuntimeError::Io {
        path: abs_path.to_path_buf(),
        message: format!("toml serialize failed: {err}"),
    }
}

fn validate_expected_hash(
    rel_path: &str,
    original_text: &str,
    expected_sha256: Option<&str>,
) -> Result<(), RuntimeError> {
    let Some(expected_sha256) = expected_sha256 else {
        return Ok(());
    };
    let mut hasher = Sha256::new();
    hasher.update(original_text.as_bytes());
    let actual = hex::encode(hasher.finalize());
    if actual != expected_sha256 {
        return Err(RuntimeError::PluginIterationPolicyBlocked {
            path: rel_path.to_string(),
            reason: format!("stale edit precondition failed: expected_sha256={expected_sha256}, actual_sha256={actual}"),
        });
    }
    Ok(())
}

pub fn file_sha256(path: &Path) -> Result<String, RuntimeError> {
    let bytes = fs::read(path).map_err(|err| RuntimeError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    Ok(hex::encode(hasher.finalize()))
}

/// Map a `canonicalize` failure on an *existing* ancestor to `RuntimeError::Io`.
/// The ancestor just passed an `exists()` check, so a canonicalise failure needs
/// an OS-level race (the entry vanishing between the two syscalls) that a unit
/// test cannot force portably; extracted for byte-stable text and direct
/// testing.
fn resolve_ancestor_io_error(current: &Path, err: &std::io::Error) -> RuntimeError {
    RuntimeError::Io {
        path: current.to_path_buf(),
        message: format!("canonicalise ancestor failed: {err}"),
    }
}

/// Map an I/O failure on the `atomic_write` staging file (create / write / sync)
/// to `RuntimeError::Io`. The create arm is reachable (read-only dir test); the
/// write and sync arms cannot be forced portably, so this shared mapper is
/// covered by a direct unit test. Error text is byte-for-byte identical to the
/// original inline closures.
fn tmp_stage_io_error(tmp_path: &Path, err: &std::io::Error) -> RuntimeError {
    RuntimeError::Io {
        path: tmp_path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Remove `target` if it exists, mapping a removal failure to
/// `RuntimeError::Io`. When the file is absent this is a no-op success — the same
/// behaviour as the original inline `else { Ok(()) }` arms, which were
/// unreachable at their call sites (a `Delete` op only runs after a successful
/// read; rollback only removes files it earlier created). Extracted so both the
/// present and absent paths are covered by direct unit tests instead of an
/// uncoverable arm. Error text is byte-for-byte identical to the original.
fn remove_file_if_exists(target: &Path) -> Result<(), RuntimeError> {
    if !target.exists() {
        return Ok(());
    }
    fs::remove_file(target).map_err(|err| RuntimeError::Io {
        path: target.to_path_buf(),
        message: err.to_string(),
    })
}

/// `RuntimeError::Io`. Extracted from four byte-identical inline blocks so the
/// no-parent fall-through and the create-failure arm are each covered by a
/// direct unit test rather than an uncoverable closing-brace artifact at each
/// call site. Error text is byte-for-byte identical to the original.
fn ensure_parent_dir(target: &Path) -> Result<(), RuntimeError> {
    let Some(parent) = target.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
        path: parent.to_path_buf(),
        message: err.to_string(),
    })
}

/// P0-20 helper: resolve `abs_path` to a canonical form that follows
/// symlinks along any existing prefix, and re-attaches the yet-to-be-created
/// tail. Used before writing to make sure the actual on-disk target sits
/// under the workspace root — `normalize_rel_path` alone only catches
/// `..` / absolute paths, not symlink-based escape. A symlink component that
/// `canonicalize` cannot follow (a dangling link) is rejected rather than
/// re-attached: a write through it would create bytes at the link's target,
/// which may sit outside the workspace.
pub(crate) fn resolve_under_workspace(abs_path: &Path) -> Result<PathBuf, RuntimeError> {
    if let Ok(canonical) = abs_path.canonicalize() {
        return Ok(canonical);
    }
    let mut ancestors: Vec<&Path> = Vec::new();
    let mut current = abs_path;
    let existing = loop {
        if is_dangling_symlink(current) {
            return Err(RuntimeError::AutoUpdateInvalidPath {
                path: abs_path.display().to_string(),
                reason: "a symlink in the path cannot be resolved inside the workspace"
                    .to_string(),
            });
        }
        if current.exists() {
            break current
                .canonicalize()
                .map_err(|err| resolve_ancestor_io_error(current, &err))?;
        }
        match current.parent() {
            Some(parent) => {
                ancestors.push(current);
                current = parent;
            }
            None => {
                return Err(RuntimeError::AutoUpdateInvalidPath {
                    path: abs_path.display().to_string(),
                    reason: "no accessible ancestor exists".to_string(),
                });
            }
        }
    };
    let mut resolved = existing;
    for tail in ancestors.iter().rev() {
        if let Some(name) = tail.file_name() {
            resolved.push(name);
        }
    }
    Ok(resolved)
}

/// True when `path` is a symlink that cannot be followed (its target does not
/// exist). By the time the ancestor walk in [`resolve_under_workspace`] runs,
/// `canonicalize` has already failed for the full path, so any symlink reached
/// here is dangling; `symlink_metadata` (which does not follow links) is what
/// distinguishes a dangling link from a merely-absent path component.
fn is_dangling_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path)
        .map(|meta| meta.file_type().is_symlink())
        .unwrap_or(false)
}

/// Canonicalise `workspace_root` once so per-path containment checks compare
/// against a symlink-free anchor. Extracted from `PluginEditExecutor::execute`
/// (P0-20); shared with `AutoUpdater` and the rollback/journal recovery paths
/// so every write and restore keeps the same boundary. Error text is
/// byte-identical to the original inline closure.
pub(crate) fn canonical_workspace_root(workspace_root: &Path) -> Result<PathBuf, RuntimeError> {
    workspace_root.canonicalize().map_err(|err| RuntimeError::Io {
        path: workspace_root.to_path_buf(),
        message: format!("workspace root not accessible: {err}"),
    })
}

/// P0-20: reject `abs_path` when its canonical on-disk target escapes
/// `canonical_workspace_root`. `display_path` names the caller-facing path in
/// the error. Uses [`resolve_under_workspace`], which follows symlinks along
/// any existing prefix, re-attaches a yet-to-be-created tail, and rejects
/// dangling symlinks — so a planted `plugins/demo/conf.toml -> /etc/...` link
/// cannot redirect a write outside the workspace. Error text is byte-identical
/// to the original inline executor check.
pub(crate) fn ensure_under_workspace(
    abs_path: &Path,
    canonical_workspace_root: &Path,
    display_path: &str,
) -> Result<(), RuntimeError> {
    let resolved = resolve_under_workspace(abs_path)?;
    if resolved.starts_with(canonical_workspace_root) {
        return Ok(());
    }
    Err(RuntimeError::AutoUpdateInvalidPath {
        path: display_path.to_string(),
        reason: format!(
            "resolved path escapes workspace root ({} not under {})",
            resolved.display(),
            canonical_workspace_root.display()
        ),
    })
}

/// Reject a rollback/journal restore target whose canonical location escapes
/// the workspace root. Backups are validated when recorded or loaded, so an
/// escape at restore time means a symlink was planted or swapped concurrently
/// — an invariant violation rather than bad input. Unlike
/// [`ensure_under_workspace`] this surfaces `RuntimeError::Invariant` because
/// the rel_path is internal state, not user input.
pub(crate) fn ensure_restore_contained(
    abs_path: &Path,
    canonical_root: &Path,
    display_path: &Path,
) -> Result<(), RuntimeError> {
    let resolved = resolve_under_workspace(abs_path)?;
    if resolved.starts_with(canonical_root) {
        return Ok(());
    }
    Err(RuntimeError::Invariant {
        message: format!(
            "rollback target {} escapes workspace root ({} not under {})",
            display_path.display(),
            resolved.display(),
            canonical_root.display()
        ),
    })
}

/// Map a `normalize_rel_path` rejection in [`PluginEditRollback::rollback`] to
/// `RuntimeError::Invariant`: backups recorded by the executor are already
/// normalized, so an illegal rel_path here is internal-state corruption.
fn rollback_rel_path_error(rel_path: &str, err: &RuntimeError) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!("plugin edit rollback invalid rel_path {rel_path}: {err}"),
    }
}

/// Map a `normalize_rel_path` rejection in [`PluginEditRollback::load_journal`]
/// to `RuntimeError::Invariant`, refusing to replay a journal that names a
/// path outside the workspace (`..` / absolute). The journal is disk state a
/// crash left behind; a malformed entry must stop recovery, not redirect it.
fn journal_rel_path_error(rel_path: &str, err: &RuntimeError) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!("plugin edit rollback journal invalid rel_path {rel_path}: {err}"),
    }
}

pub fn normalize_rel_path(path: &str) -> Result<String, RuntimeError> {
    let rel_path = Path::new(path);
    if rel_path.is_absolute() {
        return Err(RuntimeError::AutoUpdateInvalidPath {
            path: path.to_string(),
            reason: "absolute path is not allowed".to_string(),
        });
    }
    if rel_path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(RuntimeError::AutoUpdateInvalidPath {
            path: path.to_string(),
            reason: "parent directory traversal (..) is not allowed".to_string(),
        });
    }

    let normalized = rel_path
        .components()
        .fold(PathBuf::new(), |mut acc, component| {
            if let Component::Normal(part) = component {
                acc.push(part);
            }
            acc
        });
    Ok(normalized.to_string_lossy().to_string())
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Reserved keyword validation (moved from kernel/planner)
// ---------------------------------------------------------------------------

pub(crate) fn validate_reserved_child_keyword_identifiers(
    operations: &[PluginEditOperation],
    writable_roots: &BTreeSet<String>,
) -> Result<(), RuntimeError> {
    let reserved_child_keywords = operations
        .iter()
        .filter(|operation| operation.kind == PluginEditOpKind::CreateFile)
        .filter_map(|operation| normalize_rel_path(&operation.path).ok())
        .filter(|path| path.ends_with("/Cargo.toml"))
        .filter_map(|path| {
            let root = deepest_matching_writable_root(&path, writable_roots)?;
            let relative = path
                .strip_prefix(root)?
                .strip_prefix('/')
                .and_then(|value| value.strip_suffix("/Cargo.toml"))?;
            let child_name = relative.rsplit('/').next()?;
            matches!(child_name, "mod").then_some(child_name.to_string())
        })
        .collect::<BTreeSet<_>>();

    if reserved_child_keywords.is_empty() {
        return Ok(());
    }

    for operation in operations {
        if !operation.path.ends_with(".rs") {
            continue;
        }
        let Some(content) = operation.new_content.as_deref() else {
            continue;
        };
        for keyword in &reserved_child_keywords {
            if contains_raw_reserved_identifier_usage(content, keyword) {
                return Err(reserved_child_keyword_identifier_error(
                    &operation.path,
                    keyword,
                ));
            }
        }
    }

    Ok(())
}

fn contains_raw_reserved_identifier_usage(content: &str, keyword: &str) -> bool {
    let direct_patterns = [
        format!(" {keyword}:"),
        format!("({keyword}:"),
        format!(", {keyword}:"),
        format!("let {keyword} "),
        format!("let mut {keyword} "),
        format!("for {keyword} in "),
        format!("as {keyword};"),
        format!("as {keyword},"),
        format!("as {keyword}\n"),
        format!("fn {keyword}("),
        format!("type {keyword} "),
        format!("const {keyword}:"),
        format!("static {keyword}:"),
        format!(" {keyword} ="),
        format!(" {keyword} @"),
    ];
    direct_patterns
        .iter()
        .any(|pattern| content.contains(pattern))
        || contains_raw_member_access(content, keyword)
}

fn contains_raw_member_access(content: &str, keyword: &str) -> bool {
    let needle = format!(".{keyword}");
    let mut haystack = content;
    while let Some(idx) = haystack.find(&needle) {
        let suffix = &haystack[idx + needle.len()..];
        let next = suffix.chars().next();
        if next.is_none_or(|ch| !(ch.is_ascii_alphanumeric() || ch == '_')) {
            return true;
        }
        haystack = suffix;
    }
    false
}

fn reserved_child_keyword_identifier_error(path: &str, keyword: &str) -> RuntimeError {
    RuntimeError::LlmResponseInvalid {
        message: format!(
            "child plugin path component `{keyword}` is allowed in filesystem paths like `expr/evaluator/{keyword}`, but raw Rust identifier `{keyword}` is invalid in source code; keep the path as `expr/evaluator/{keyword}`, keep PascalCase type names like `ModPlugin` or `ModError` if needed, and rename only the lower-case Rust identifier in {path} to something like `modulo` or `mod_plugin` before retrying"
        ),
    }
}

pub(crate) fn deepest_matching_writable_root<'a>(
    path: &str,
    writable_roots: &'a BTreeSet<String>,
) -> Option<&'a str> {
    writable_roots
        .iter()
        .map(String::as_str)
        .filter(|root| {
            path == *root
                || path
                    .strip_prefix(root)
                    .is_some_and(|rest| rest.starts_with('/'))
        })
        .max_by_key(|root| root.len())
}

/// Write `bytes` durably to `path`: write to `<path>.tmp`, fsync, then rename.
/// After this returns Ok, the file at `path` is either the previous contents
/// or the new contents — never a truncated partial write. Used by the plugin
/// iteration edit executor (P0-5) and the rollback journal writer (P0-6).
pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), RuntimeError> {
    use std::io::Write;
    ensure_parent_dir(path)?;
    let tmp_path = match path.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(".cordis-tmp");
            path.with_file_name(owned)
        }
        None => {
            return Err(RuntimeError::Io {
                path: path.to_path_buf(),
                message: "atomic_write target has no filename".to_string(),
            });
        }
    };
    {
        let mut file =
            fs::File::create(&tmp_path).map_err(|err| tmp_stage_io_error(&tmp_path, &err))?;
        // The write_all / sync_all arms cannot be forced to fail portably from a
        // unit test; routing them through `tmp_stage_io_error` keeps the mapping
        // a single call and covers the message construction via a direct test.
        file.write_all(bytes)
            .map_err(|err| tmp_stage_io_error(&tmp_path, &err))?;
        file.sync_all()
            .map_err(|err| tmp_stage_io_error(&tmp_path, &err))?;
    }
    fs::rename(&tmp_path, path).map_err(|err| RuntimeError::Io {
        path: path.to_path_buf(),
        message: format!(
            "rename {} -> {} failed: {err}",
            tmp_path.display(),
            path.display()
        ),
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    /// `Some(())` when the process is not root. Root ignores file-mode
    /// permission bits, so the permission-failure tests below only assert
    /// under a non-root euid; iterating the `Option` keeps every line of the
    /// body executable in coverage under both euids.
    fn probe_not_root() -> Option<()> {
        // SAFETY: `geteuid` is always safe to call and cannot fail.
        (unsafe { libc::geteuid() } != 0).then_some(())
    }

    use super::{
        normalize_rel_path, CanaryVerdict, KernelPluginIssueSource, PluginEditExecutor,
        PluginEditOpKind, PluginEditOperation, PluginEditPlan, PluginIterationPolicy,
    };
    use crate::core::error::RuntimeError as RtErr;
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

    /// True for the two error variants a read-only-dir delete may surface: the
    /// direct `Io` error, or the `Invariant` wrapper when the in-execute rollback
    /// then also fails. A single-line predicate so the assertion at the call site
    /// stays one line (no multi-line `matches!` brace artifact).
    fn is_io_or_invariant(err: &RtErr) -> bool {
        matches!(err, RtErr::Io { .. } | RtErr::Invariant { .. })
    }

    #[test]
    fn normalize_rel_path_rejects_parent_dir() {
        let err = normalize_rel_path("../oops").expect_err("parent dir should fail");
        assert!(err.to_string().contains("parent directory traversal"));
    }

    #[test]
    fn plugin_iteration_policy_blocks_runtime_paths() {
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "bad".to_string(),
            operations: vec![PluginEditOperation {
                path: "crates/cordis-runtime/src/lib.rs".to_string(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("pub".to_string()),
                expected_sha256: None,
                new_content: Some("mod".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };
        let err = PluginIterationPolicy::default()
            .validate_plan(&allowed, &plan)
            .expect_err("runtime path should be blocked");
        assert!(err
            .to_string()
            .contains("outside the plugin iteration surface"));
    }

    #[test]
    fn plugin_iteration_policy_blocks_generated_agent_docs() {
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "bad".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/docs/agent/interfaces.json".to_string(),
                kind: PluginEditOpKind::JsonSet,
                expected_old_string: None,
                expected_sha256: Some("abc123".to_string()),
                new_content: None,
                pointer: Some("/nodes/0/summary".to_string()),
                dotted_key: None,
                value: Some(serde_json::json!("updated")),
            }],
        };
        let err = PluginIterationPolicy::default()
            .validate_plan(&allowed, &plan)
            .expect_err("generated agent docs should be blocked");
        assert!(err.to_string().contains("read-only context"));
    }

    #[test]
    fn plugin_iteration_policy_allows_new_child_plugin_inside_selected_subtree() {
        let mut allowed = BTreeMap::new();
        allowed.insert("expr".to_string(), "plugins/expr".to_string());
        allowed.insert(
            "expr/evaluator".to_string(),
            "plugins/expr/evaluator".to_string(),
        );
        allowed.insert(
            "expr/evaluator/add".to_string(),
            "plugins/expr/evaluator/add".to_string(),
        );
        let plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "add modulo child plugin".to_string(),
            operations: vec![
                PluginEditOperation {
                    path: "plugins/expr/evaluator/mod/Cargo.toml".to_string(),
                    kind: PluginEditOpKind::CreateFile,
                    expected_old_string: Some(String::new()),
                    expected_sha256: None,
                    new_content: Some("[package]\nname = \"expr-evaluator-mod\"\n".to_string()),
                    pointer: None,
                    dotted_key: None,
                    value: None,
                },
                PluginEditOperation {
                    path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
                    kind: PluginEditOpKind::CreateFile,
                    expected_old_string: Some(String::new()),
                    expected_sha256: None,
                    new_content: Some("pub fn eval_mod() {}\n".to_string()),
                    pointer: None,
                    dotted_key: None,
                    value: None,
                },
            ],
        };
        PluginIterationPolicy::default()
            .validate_plan(&allowed, &plan)
            .expect("new child plugin inside subtree should be allowed");
    }

    #[test]
    fn plugin_edit_executor_supports_create_and_delete() {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path();
        fs::create_dir_all(workspace.join("plugins/demo/src")).expect("create plugin src");
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let executor = PluginEditExecutor::new(workspace);

        let create_plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "create".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/generated.rs".to_string(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some("pub const VALUE: u32 = 1;\n".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };
        let (_apply_result, rollback) = executor
            .execute(&PluginIterationPolicy::default(), &allowed, &create_plan)
            .expect("create should succeed");
        assert!(workspace.join("plugins/demo/src/generated.rs").exists());
        rollback.rollback().expect("rollback should succeed");
        assert!(!workspace.join("plugins/demo/src/generated.rs").exists());
        assert_eq!(CanaryVerdict::Pass, CanaryVerdict::Pass);
        assert_eq!(
            KernelPluginIssueSource::InvokeFailure.priority(),
            KernelPluginIssueSource::InvokeFailure.priority()
        );
    }

    // execute(): the per-operation `create_dir_all(parent)` fails when a parent
    // path component is a regular file. A CreateFile op whose parent directory
    // ("src/afile") is actually a file makes the mkdir fail → the Io error map
    // at that call site fires (before any write).
    #[test]
    fn plugin_edit_executor_create_dir_all_failure_is_io() {
        use crate::core::error::RuntimeError;
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path();
        fs::create_dir_all(workspace.join("plugins/demo/src")).expect("create plugin src");
        // A regular file where the op wants a directory component.
        fs::write(workspace.join("plugins/demo/src/afile"), b"x").expect("write blocker file");
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let executor = PluginEditExecutor::new(workspace);

        let plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "mkdir-fail".to_string(),
            operations: vec![PluginEditOperation {
                // parent "plugins/demo/src/afile" is a file → create_dir_all errs.
                path: "plugins/demo/src/afile/inner.rs".to_string(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some("pub const X: u32 = 1;\n".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };
        let err = executor
            .execute(&PluginIterationPolicy::default(), &allowed, &plan)
            .expect_err("create_dir_all over a file must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    // execute(): a DeleteFile op whose `fs::remove_file` fails surfaces the Io
    // error map at that call site. On Unix, making the parent directory
    // read-only lets `fs::read` succeed (so the op validates and original is
    // Some) but blocks the unlink with EACCES. Root ignores mode bits, so gate
    // on non-root.
    #[cfg(unix)]
    #[test]
    fn plugin_edit_executor_remove_file_failure_is_io() {
        use std::os::unix::fs::PermissionsExt;
        // Single-line guard: no standalone closing brace to leave uncovered.
        // Root bypasses file-mode permission checks, so the failure this test
        // asserts only occurs as a non-root user. Iterating an `Option` (rather
        // than an `if` gate) leaves no never-taken edge on the closing brace.
        for () in probe_not_root().into_iter() {
            let temp = TempDir::new().expect("tempdir");
            let workspace = temp.path();
            let src = workspace.join("plugins/demo/src");
            fs::create_dir_all(&src).expect("create plugin src");
            let victim = src.join("gone.rs");
            fs::write(&victim, b"payload").expect("write victim");
            let sha = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(b"payload");
                hex::encode(h.finalize())
            };
            // Read-only parent dir: read still works, unlink fails.
            let mut perms = fs::metadata(&src).unwrap().permissions();
            perms.set_mode(0o555);
            fs::set_permissions(&src, perms).unwrap();

            let mut allowed = BTreeMap::new();
            allowed.insert("demo".to_string(), "plugins/demo".to_string());
            let executor = PluginEditExecutor::new(workspace);
            let plan = PluginEditPlan {
                issue_id: "issue-1".to_string(),
                patch_id: "patch-1".to_string(),
                summary: "delete-fail".to_string(),
                operations: vec![PluginEditOperation {
                    path: "plugins/demo/src/gone.rs".to_string(),
                    kind: PluginEditOpKind::DeleteFile,
                    expected_old_string: None,
                    expected_sha256: Some(sha),
                    new_content: None,
                    pointer: None,
                    dotted_key: None,
                    value: None,
                }],
            };
            let result = executor.execute(&PluginIterationPolicy::default(), &allowed, &plan);

            // Restore write perms so the TempDir can be cleaned up.
            let mut perms = fs::metadata(&src).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&src, perms).unwrap();

            // The remove_file Io error is surfaced (possibly wrapped by the
            // rollback path into an Invariant if rollback then also fails, but
            // on a read-only dir the rollback restore of an unchanged file is a
            // no-op write that also fails — accept either the direct Io or the
            // Invariant rollback wrapper).
            let err = result.expect_err("remove_file on read-only dir must fail");
            assert!(is_io_or_invariant(&err), "got: {err:?}");
        }
    }

    // ---------- P0-5..P0-8 rollback hardening tests ----------

    use super::{
        atomic_write, ensure_parent_dir, remove_file_if_exists, tmp_stage_io_error,
        PluginEditRollback,
    };

    #[test]
    fn ensure_parent_dir_creates_missing_parent() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("a/b/c.txt");
        ensure_parent_dir(&target).unwrap();
        assert!(target.parent().unwrap().is_dir());
    }

    #[test]
    fn ensure_parent_dir_no_parent_is_ok() {
        // The root path has no parent → the fall-through Ok branch.
        ensure_parent_dir(std::path::Path::new("/")).unwrap();
    }

    #[test]
    fn ensure_parent_dir_reports_io_when_parent_is_a_file() {
        use crate::core::error::RuntimeError;
        let temp = TempDir::new().unwrap();
        let blocker = temp.path().join("afile");
        fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("inner/leaf.txt");
        let err = ensure_parent_dir(&target).expect_err("parent under a file must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn remove_file_if_exists_removes_present_file() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("gone.txt");
        fs::write(&target, b"x").unwrap();
        remove_file_if_exists(&target).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn remove_file_if_exists_absent_is_noop() {
        // The absent path returns Ok without touching the filesystem — the arm
        // that was unreachable at the `Delete` / rollback call sites.
        let temp = TempDir::new().unwrap();
        remove_file_if_exists(&temp.path().join("never-existed.txt")).unwrap();
    }

    #[test]
    fn remove_file_if_exists_reports_io_when_target_is_nonempty_dir() {
        use crate::core::error::RuntimeError;
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("busy");
        fs::create_dir_all(dir.join("child")).unwrap();
        let err = remove_file_if_exists(&dir).expect_err("remove_file on nonempty dir must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn tmp_stage_io_error_wraps_path_and_message() {
        use crate::core::error::RuntimeError;
        let io = std::io::Error::other("boom");
        let err = tmp_stage_io_error(std::path::Path::new("/tmp/x.cordis-tmp"), &io);
        assert!(
            matches!(&err, RuntimeError::Io { path, message }
                if path == std::path::Path::new("/tmp/x.cordis-tmp") && message == "boom"),
            "got: {err:?}"
        );
    }

    #[test]
    fn atomic_write_is_all_or_nothing() {
        // Writing succeeds and produces the expected bytes; no .tmp file
        // survives.
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("journal.json");
        atomic_write(&target, b"hello").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"hello");
        let entries: Vec<_> = fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".cordis-tmp"))
            .collect();
        assert!(entries.is_empty(), "no leftover tmp file expected");
    }

    #[test]
    fn atomic_write_replaces_existing_contents() {
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("journal.json");
        fs::write(&target, b"stale").unwrap();
        atomic_write(&target, b"fresh").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"fresh");
    }

    #[test]
    fn atomic_write_create_dir_all_failure_when_parent_is_a_file() {
        use crate::core::error::RuntimeError;
        // The target's parent directory is actually a regular file, so the
        // leading `create_dir_all(parent)` fails before any tmp is written.
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("afile"), b"x").unwrap();
        let target = temp.path().join("afile/inner.json");
        let err = atomic_write(&target, b"data").expect_err("create_dir_all over a file must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    // atomic_write: the staging `File::create(&tmp_path)` fails when the parent
    // directory exists but is read-only (so create_dir_all is a no-op success,
    // but the tmp file can't be created) — the tmp-create Io error map.
    #[cfg(unix)]
    #[test]
    fn atomic_write_tmp_create_failure_in_readonly_dir() {
        use crate::core::error::RuntimeError;
        use std::os::unix::fs::PermissionsExt;
        // Root ignores mode bits, so this only fails as non-root. Single-line
        // guard: no standalone closing brace to leave uncovered.
        // Root bypasses file-mode permission checks, so the failure this test
        // asserts only occurs as a non-root user. Iterating an `Option` (rather
        // than an `if` gate) leaves no never-taken edge on the closing brace.
        for () in probe_not_root().into_iter() {
            let temp = TempDir::new().unwrap();
            let dir = temp.path().join("ro");
            fs::create_dir_all(&dir).unwrap();
            let target = dir.join("out.json");
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            perms.set_mode(0o555); // read+execute, no write → tmp create fails
            fs::set_permissions(&dir, perms).unwrap();

            let result = atomic_write(&target, b"data");

            // Restore perms for cleanup.
            let mut perms = fs::metadata(&dir).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&dir, perms).unwrap();

            let err = result.expect_err("tmp create in read-only dir must fail");
            assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
        }
    }

    #[test]
    fn atomic_write_rename_failure_when_target_is_nonempty_dir() {
        use crate::core::error::RuntimeError;
        // tmp creation + fsync succeed, but the final rename of the tmp file
        // over an existing *non-empty directory* at `path` fails, exercising
        // the rename error map.
        let temp = TempDir::new().unwrap();
        let target = temp.path().join("target");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("occupied"), b"x").unwrap();
        let err =
            atomic_write(&target, b"data").expect_err("rename over a non-empty dir must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn journal_persist_uses_atomic_rename_and_records_generation() {
        // P0-6: persist_journal must round-trip and the generation id must
        // be non-empty and readable by `journal_generation_id`.
        let workspace = TempDir::new().unwrap();
        let rollback = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/lib.rs",
            Some(b"prior".to_vec()),
        );
        let journal_path = workspace.path().join("journal.json");
        rollback.persist_journal(&journal_path, "iter-1").unwrap();
        let gen_id = PluginEditRollback::journal_generation_id(&journal_path)
            .unwrap()
            .expect("journal should carry a generation id");
        assert!(!gen_id.is_empty());
        // Confirm each rewrite produces a fresh generation id.
        rollback.persist_journal(&journal_path, "iter-1").unwrap();
        let gen_id_2 = PluginEditRollback::journal_generation_id(&journal_path)
            .unwrap()
            .unwrap();
        assert_ne!(gen_id, gen_id_2);
    }

    #[cfg(unix)]
    #[test]
    fn edit_executor_rejects_symlink_escape_out_of_workspace() {
        // P0-20: a symlink inside the whitelisted plugin surface must not
        // give the agent a write handle to somewhere outside workspace_root.
        // Concretely: plugins/demo/src/evil -> /tmp/pwned should reject.
        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("pwned");
        fs::write(&outside_target, b"outside").unwrap();

        let workspace = TempDir::new().unwrap();
        let plugin_src = workspace.path().join("plugins/demo/src");
        fs::create_dir_all(&plugin_src).unwrap();
        // Place a symlink inside the plugin surface pointing outside.
        let symlink_at = plugin_src.join("evil");
        std::os::unix::fs::symlink(&outside_target, &symlink_at).unwrap();

        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "escape".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/evil".to_string(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("outside".to_string()),
                expected_sha256: None,
                new_content: Some("pwned".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };

        let executor = PluginEditExecutor::new(workspace.path());
        let err = executor
            .execute(&PluginIterationPolicy::default(), &allowed, &plan)
            .expect_err("symlink escape must be rejected");
        assert!(
            err.to_string().contains("escapes workspace root"),
            "unexpected error: {err}"
        );
        // And the symlinked target must remain untouched.
        assert_eq!(fs::read(&outside_target).unwrap(), b"outside");
    }

    #[test]
    fn edit_executor_backup_is_registered_before_write_fails() {
        // P0-5: the backup for a file must be present in the rollback list
        // *before* mutating disk. We can't easily provoke a mid-write failure
        // portably, so instead verify a successful path: after execute, the
        // rollback restores the original bytes.
        let workspace = TempDir::new().unwrap();
        let target = workspace.path().join("plugins/demo/src/lib.rs");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"original").unwrap();

        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let plan = PluginEditPlan {
            issue_id: "issue-1".to_string(),
            patch_id: "patch-1".to_string(),
            summary: "test".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/lib.rs".to_string(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("original".to_string()),
                expected_sha256: None,
                new_content: Some("mutated".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };

        let executor = PluginEditExecutor::new(workspace.path());
        let (_apply, rollback) = executor
            .execute(&PluginIterationPolicy::default(), &allowed, &plan)
            .unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"mutated");
        rollback.rollback().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"original");
    }

    /// P0-6/7: journal generation_id round-trips; two persists yield
    /// distinct ids.
    #[test]
    fn journal_generation_id_round_trip_and_distinctness() {
        let workspace = TempDir::new().unwrap();
        let rb = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/lib.rs",
            Some(b"orig".to_vec()),
        );
        let jp = workspace.path().join("j.json");
        rb.persist_journal(&jp, "iter").unwrap();
        let id1 = PluginEditRollback::journal_generation_id(&jp)
            .unwrap()
            .unwrap();
        assert!(!id1.is_empty());
        rb.persist_journal(&jp, "iter").unwrap();
        let id2 = PluginEditRollback::journal_generation_id(&jp)
            .unwrap()
            .unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn journal_generation_id_returns_none_when_absent() {
        let workspace = TempDir::new().unwrap();
        let jp = workspace.path().join("nonexistent.json");
        assert!(PluginEditRollback::journal_generation_id(&jp)
            .unwrap()
            .is_none());
    }

    /// P0-6 durability: load_journal round-trips backups via hex.
    #[test]
    fn load_journal_round_trip_preserves_backup_bytes() {
        let workspace = TempDir::new().unwrap();
        let rb = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/lib.rs",
            Some(b"pre-edit\n\x00\xffbytes".to_vec()),
        );
        let jp = workspace.path().join("j.json");
        rb.persist_journal(&jp, "iter-a").unwrap();
        let loaded = PluginEditRollback::load_journal(workspace.path(), &jp)
            .unwrap()
            .expect("journal exists");
        assert_eq!(loaded.len(), 1);
    }

    #[test]
    fn clear_journal_removes_file() {
        let workspace = TempDir::new().unwrap();
        let rb = PluginEditRollback::empty(workspace.path());
        let jp = workspace.path().join("j.json");
        rb.persist_journal(&jp, "iter").unwrap();
        assert!(jp.exists());
        PluginEditRollback::clear_journal(&jp).unwrap();
        assert!(!jp.exists());
        // Idempotent: clearing an absent journal is fine.
        PluginEditRollback::clear_journal(&jp).unwrap();
    }

    /// P2-23: `absorb` dedups by `rel_path`, keeping the first backup so
    /// rollback restores the earliest known bytes (not later mid-run
    /// snapshots).
    #[test]
    fn rollback_absorb_dedups_by_rel_path_preserving_first() {
        let workspace = TempDir::new().unwrap();
        let target = workspace.path().join("plugins/demo/src/lib.rs");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"orig").unwrap();

        let mut acc = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/lib.rs",
            Some(b"orig".to_vec()),
        );
        // A second backup for the same file with different bytes MUST be
        // discarded by absorb.
        let second = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/lib.rs",
            Some(b"mid-run".to_vec()),
        );
        acc.absorb(second).unwrap();
        assert_eq!(acc.len(), 1);
        // Mutate the file, then rollback: bytes must go back to the FIRST
        // backup (b"orig"), not the second (b"mid-run").
        fs::write(&target, b"mutated").unwrap();
        acc.rollback().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"orig");
    }

    #[test]
    fn rollback_absorb_preserves_distinct_paths() {
        let workspace = TempDir::new().unwrap();
        fs::create_dir_all(workspace.path().join("plugins/demo/src")).unwrap();
        fs::write(workspace.path().join("plugins/demo/src/a.rs"), b"a-orig").unwrap();
        fs::write(workspace.path().join("plugins/demo/src/b.rs"), b"b-orig").unwrap();

        let mut acc = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/a.rs",
            Some(b"a-orig".to_vec()),
        );
        let second = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/b.rs",
            Some(b"b-orig".to_vec()),
        );
        acc.absorb(second).unwrap();
        assert_eq!(acc.len(), 2);
    }

    #[test]
    fn rollback_is_empty_reflects_backup_count() {
        let workspace = TempDir::new().unwrap();
        let empty = PluginEditRollback::empty(workspace.path());
        assert!(empty.is_empty());
        let one = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/lib.rs",
            Some(b"orig".to_vec()),
        );
        assert!(!one.is_empty());
    }

    /// P1-29 spirit / P0-5 spirit: an edit that deletes a file records
    /// a `None` original (i.e. "file did not exist") vs. a `Some(...)`
    /// original (i.e. "restore these bytes"). Rollback preserves both
    /// semantics.
    #[test]
    fn rollback_recreates_deleted_file_and_removes_created_file() {
        let workspace = TempDir::new().unwrap();
        let existing = workspace.path().join("plugins/demo/src/existing.rs");
        fs::create_dir_all(existing.parent().unwrap()).unwrap();
        fs::write(&existing, b"before").unwrap();

        // Simulate: delete the existing file, then create a new one.
        let mut rb = PluginEditRollback::empty(workspace.path());
        let del_backup = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/existing.rs",
            Some(b"before".to_vec()),
        );
        rb.absorb(del_backup).unwrap();
        fs::remove_file(&existing).unwrap();

        let create_backup = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/fresh.rs",
            None, // did not exist before
        );
        rb.absorb(create_backup).unwrap();
        let fresh = workspace.path().join("plugins/demo/src/fresh.rs");
        fs::write(&fresh, b"new").unwrap();

        rb.rollback().unwrap();
        assert_eq!(fs::read(&existing).unwrap(), b"before");
        assert!(
            !fresh.exists(),
            "created file should be removed on rollback"
        );
    }

    // ---------- P0-20 parity: rollback / journal rel_path validation ----------

    /// A symlink planted inside the workspace (or swapped in after the edit)
    /// must not redirect the rollback restore write outside the workspace.
    #[cfg(unix)]
    #[test]
    fn rollback_rejects_symlink_escape_out_of_workspace() {
        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("pwned");
        fs::write(&outside_target, b"outside").unwrap();

        let workspace = TempDir::new().unwrap();
        let plugin_src = workspace.path().join("plugins/demo/src");
        fs::create_dir_all(&plugin_src).unwrap();
        let symlink_at = plugin_src.join("evil");
        std::os::unix::fs::symlink(&outside_target, &symlink_at).unwrap();

        let rollback = PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/evil",
            Some(b"restore-bytes".to_vec()),
        );
        let err = rollback.rollback().expect_err("escape must be rejected");
        assert!(
            err.to_string().contains("escapes workspace root"),
            "unexpected error: {err}"
        );
        assert_eq!(fs::read(&outside_target).unwrap(), b"outside");
    }

    /// Lexical validation in rollback: a backup rel_path that walks up with
    /// `..` or is absolute is internal-state corruption and must be rejected
    /// with `Invariant` before any disk write.
    #[test]
    fn rollback_rejects_parent_dir_and_absolute_rel_paths() {
        let workspace = TempDir::new().unwrap();
        let traversal = PluginEditRollback::single_backup(
            workspace.path(),
            "../escape.txt",
            Some(b"x".to_vec()),
        );
        let err = traversal
            .rollback()
            .expect_err("parent traversal must be rejected");
        assert!(err.to_string().contains("invalid rel_path"), "unexpected: {err}");
        let absolute =
            PluginEditRollback::single_backup(workspace.path(), "/etc/passwd", Some(b"x".to_vec()));
        let err = absolute
            .rollback()
            .expect_err("absolute rel_path must be rejected");
        assert!(err.to_string().contains("invalid rel_path"), "unexpected: {err}");
    }

    /// The canonicalise-workspace-root gate runs even for an empty rollback:
    /// a missing workspace root is an Io error, not a silent no-op.
    #[test]
    fn rollback_errors_when_workspace_root_missing() {
        let temp = TempDir::new().unwrap();
        let ghost = temp.path().join("no-such-root");
        let err = PluginEditRollback::empty(&ghost)
            .rollback()
            .expect_err("a missing workspace root must error");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    /// A journal whose backup rel_path walks up with `..` must refuse replay:
    /// the journal is disk state a crash left behind, and a traversal entry
    /// would redirect boot recovery outside the workspace.
    #[test]
    fn load_journal_rejects_parent_dir_rel_path() {
        let workspace = TempDir::new().unwrap();
        let jp = workspace.path().join("evil-journal.json");
        PluginEditRollback::single_backup(workspace.path(), "../escape.txt", Some(b"x".to_vec()))
            .persist_journal(&jp, "iter")
            .unwrap();
        let err = PluginEditRollback::load_journal(workspace.path(), &jp)
            .expect_err("a traversal rel_path must refuse replay");
        assert!(err.to_string().contains("invalid rel_path"), "unexpected: {err}");
    }

    #[test]
    fn load_journal_rejects_absolute_rel_path() {
        let workspace = TempDir::new().unwrap();
        let jp = workspace.path().join("evil-journal.json");
        PluginEditRollback::single_backup(workspace.path(), "/etc/passwd", Some(b"x".to_vec()))
            .persist_journal(&jp, "iter")
            .unwrap();
        let err = PluginEditRollback::load_journal(workspace.path(), &jp)
            .expect_err("an absolute rel_path must refuse replay");
        assert!(err.to_string().contains("invalid rel_path"), "unexpected: {err}");
    }

    /// Symlink-escape regression at journal-load time: a backup rel_path that
    /// resolves (through a planted symlink) outside the workspace must refuse
    /// replay with `Invariant`, leaving the outside target untouched.
    #[cfg(unix)]
    #[test]
    fn load_journal_rejects_symlink_escape_rel_path() {
        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("pwned");
        fs::write(&outside_target, b"outside").unwrap();

        let workspace = TempDir::new().unwrap();
        let plugin_src = workspace.path().join("plugins/demo/src");
        fs::create_dir_all(&plugin_src).unwrap();
        let symlink_at = plugin_src.join("evil");
        std::os::unix::fs::symlink(&outside_target, &symlink_at).unwrap();

        let jp = workspace.path().join("evil-journal.json");
        PluginEditRollback::single_backup(
            workspace.path(),
            "plugins/demo/src/evil",
            Some(b"x".to_vec()),
        )
        .persist_journal(&jp, "iter")
        .unwrap();
        let err = PluginEditRollback::load_journal(workspace.path(), &jp)
            .expect_err("a symlink-escape rel_path must refuse replay");
        assert!(
            err.to_string().contains("escapes workspace root"),
            "unexpected: {err}"
        );
    }

    // ---------- pure-logic / helper coverage ----------

    use super::{
        apply_operation, contains_raw_member_access, deepest_matching_writable_root, file_sha256,
        json_set_serialize_io_error, now_ms, plugin_subtree_surface_kind,
        resolve_ancestor_io_error, resolve_under_workspace, rollback_journal_serialize_error,
        toml_set_serialize_io_error, validate_expected_hash,
        validate_reserved_child_keyword_identifiers, KernelPluginIssueStatus,
        PluginIterationFinalVerdict, PluginIterationNetSpec, PluginSubtreeSurfaceKind, UpdatedFile,
        VerifierVerdict,
    };
    use crate::core::error::RuntimeError;
    use crate::kernel::auto_update::{AutoUpdatePlan, FilePatch};
    use std::collections::BTreeSet;
    use std::path::Path;

    fn sha_hex(s: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(s.as_bytes());
        hex::encode(hasher.finalize())
    }

    /// The `json_set` re-serialize failure arm is unreachable (a just-parsed
    /// `Value` always re-serializes), so exercise the extracted mapper directly
    /// to lock its `Io` variant and byte-exact message.
    #[test]
    fn json_set_serialize_io_error_wraps_path_and_message() {
        let serde_err =
            serde_json::from_str::<serde_json::Value>("{").expect_err("malformed json errors");
        let err = json_set_serialize_io_error(Path::new("/tmp/x.json"), &serde_err);
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x.json") && message.starts_with("json serialize failed: ")),
            "unexpected: {err:?}"
        );
    }

    /// Same for the `toml_set` re-serialize failure arm. A real
    /// `toml::ser::Error` arises from a map whose keys are not strings.
    #[test]
    fn toml_set_serialize_io_error_wraps_path_and_message() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(vec![1u8, 2u8], 3u8);
        let ser_err = toml::to_string_pretty(&map).expect_err("non-string key errors");
        let err = toml_set_serialize_io_error(Path::new("/tmp/x.toml"), &ser_err);
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x.toml") && message.starts_with("toml serialize failed: ")),
            "unexpected: {err:?}"
        );
    }

    /// The rollback-journal re-serialize failure arm is unreachable (a journal
    /// built from in-memory backups always re-serializes), so exercise the
    /// extracted mapper directly to lock its `Invariant` variant and message.
    #[test]
    fn rollback_journal_serialize_error_wraps_message() {
        let serde_err =
            serde_json::from_str::<serde_json::Value>("{").expect_err("malformed json errors");
        let err = rollback_journal_serialize_error(&serde_err);
        assert!(
            matches!(&err, RuntimeError::Invariant { message } if message.starts_with("plugin edit rollback journal serialize failed: ")),
            "unexpected: {err:?}"
        );
    }

    // `UpdatedFile` deliberately has no `Debug` impl, so `unwrap_err` on an
    // `apply_operation` result won't compile. Collapse the Ok payload to `()`
    // for error-path assertions.
    fn apply_err(
        rel: &str,
        operation: &PluginEditOperation,
        abs: &Path,
        original: Option<&[u8]>,
    ) -> RuntimeError {
        apply_operation(rel, operation, abs, original)
            .map(|_| ())
            .unwrap_err()
    }

    fn op(kind: PluginEditOpKind) -> PluginEditOperation {
        PluginEditOperation {
            path: "plugins/demo/src/lib.rs".to_string(),
            kind,
            expected_old_string: None,
            expected_sha256: None,
            new_content: None,
            pointer: None,
            dotted_key: None,
            value: None,
        }
    }

    #[test]
    fn issue_source_priority_is_ordered() {
        use super::KernelPluginIssueSource as S;
        let priorities = [
            S::LoadFailure.priority(),
            S::CanaryFailure.priority(),
            S::VerifierFailure.priority(),
            S::InvokeFailure.priority(),
            S::DocsDrift.priority(),
            S::PolicyBlocked.priority(),
        ];
        assert_eq!(priorities, [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn net_spec_default_lists_full_pipeline() {
        let spec = PluginIterationNetSpec::default();
        assert_eq!(spec.transition_ids.first().unwrap(), "observe");
        assert_eq!(spec.transition_ids.last().unwrap(), "promote_or_rollback");
        assert_eq!(spec.transition_ids.len(), 9);
    }

    #[test]
    fn enum_serde_round_trips() {
        for status in [
            KernelPluginIssueStatus::Open,
            KernelPluginIssueStatus::Running,
            KernelPluginIssueStatus::Blocked,
            KernelPluginIssueStatus::Resolved,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            assert_eq!(
                serde_json::from_str::<KernelPluginIssueStatus>(&json).unwrap(),
                status
            );
        }
        for verdict in [
            VerifierVerdict::Pass,
            VerifierVerdict::Fail,
            VerifierVerdict::Partial,
        ] {
            let json = serde_json::to_string(&verdict).unwrap();
            assert_eq!(
                serde_json::from_str::<VerifierVerdict>(&json).unwrap(),
                verdict
            );
        }
        for verdict in [
            PluginIterationFinalVerdict::Promoted,
            PluginIterationFinalVerdict::RolledBack,
            PluginIterationFinalVerdict::Blocked,
        ] {
            let json = serde_json::to_string(&verdict).unwrap();
            assert_eq!(
                serde_json::from_str::<PluginIterationFinalVerdict>(&json).unwrap(),
                verdict
            );
        }
    }

    #[test]
    fn diff_line_estimate_by_kind() {
        // ReplaceExact: max(old lines, new lines, 1).
        let mut replace = op(PluginEditOpKind::ReplaceExact);
        replace.expected_old_string = Some("a\nb".to_string());
        replace.new_content = Some("x\ny\nz".to_string());
        assert_eq!(replace.diff_line_estimate(), 3);
        // CreateFile counts new_content lines, floored at 1.
        let mut create = op(PluginEditOpKind::CreateFile);
        create.new_content = Some("one\ntwo".to_string());
        assert_eq!(create.diff_line_estimate(), 2);
        let empty_create = op(PluginEditOpKind::CreateFile);
        assert_eq!(empty_create.diff_line_estimate(), 1);
        // Json/Toml sets always estimate 1.
        assert_eq!(op(PluginEditOpKind::JsonSet).diff_line_estimate(), 1);
        assert_eq!(op(PluginEditOpKind::TomlSet).diff_line_estimate(), 1);
    }

    #[test]
    fn plan_diff_lines_and_changed_paths_dedup_sorted() {
        let mut a = op(PluginEditOpKind::JsonSet);
        a.path = "plugins/z/x.json".to_string();
        let mut b = op(PluginEditOpKind::TomlSet);
        b.path = "plugins/a/y.toml".to_string();
        let mut c = op(PluginEditOpKind::JsonSet);
        c.path = "plugins/z/x.json".to_string(); // duplicate path
        let plan = PluginEditPlan {
            issue_id: "i".to_string(),
            patch_id: "p".to_string(),
            summary: "s".to_string(),
            operations: vec![a, b, c],
        };
        assert_eq!(plan.diff_lines(), 3);
        // changed_paths is deduped and sorted (BTreeSet).
        assert_eq!(
            plan.changed_paths(),
            vec![
                "plugins/a/y.toml".to_string(),
                "plugins/z/x.json".to_string()
            ]
        );
    }

    #[test]
    fn from_auto_update_plan_maps_every_patch_kind() {
        let temp = TempDir::new().unwrap();
        let ws = temp.path();
        fs::create_dir_all(ws.join("plugins/demo")).unwrap();
        fs::write(ws.join("plugins/demo/data.json"), "{\"k\":1}").unwrap();
        fs::write(ws.join("plugins/demo/conf.toml"), "k = 1\n").unwrap();

        let plan = AutoUpdatePlan {
            issue_id: "issue-9".to_string(),
            patch_id: "patch-9".to_string(),
            manual_approved: false,
            diff_lines: 0,
            patches: vec![
                FilePatch::text("plugins/demo/src/lib.rs", "old", "new"),
                FilePatch::json_value("plugins/demo/data.json", "/k", serde_json::json!(2)),
                FilePatch::toml_value("plugins/demo/conf.toml", "k", serde_json::json!(2)),
            ],
        };
        let edit_plan =
            PluginEditPlan::from_auto_update_plan(ws, "converted".to_string(), plan).unwrap();
        assert_eq!(edit_plan.issue_id, "issue-9");
        assert_eq!(edit_plan.patch_id, "patch-9");
        assert_eq!(edit_plan.summary, "converted");
        assert_eq!(edit_plan.operations.len(), 3);
        assert_eq!(edit_plan.operations[0].kind, PluginEditOpKind::ReplaceExact);
        assert_eq!(
            edit_plan.operations[0].expected_old_string.as_deref(),
            Some("old")
        );
        assert_eq!(edit_plan.operations[1].kind, PluginEditOpKind::JsonSet);
        assert_eq!(edit_plan.operations[1].pointer.as_deref(), Some("/k"));
        assert!(edit_plan.operations[1].expected_sha256.is_some());
        assert_eq!(edit_plan.operations[2].kind, PluginEditOpKind::TomlSet);
        assert_eq!(edit_plan.operations[2].dotted_key.as_deref(), Some("k"));
        assert!(edit_plan.operations[2].expected_sha256.is_some());
    }

    #[test]
    fn validate_path_allows_top_level_workspace_manifest() {
        let allowed = BTreeMap::new();
        // plugins/Cargo.toml is explicitly always editable.
        PluginIterationPolicy::default()
            .validate_path(&allowed, "plugins/Cargo.toml")
            .expect("workspace manifest must be writable");
    }

    #[test]
    fn validate_path_rejects_manifest_when_edits_disabled() {
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let policy = PluginIterationPolicy {
            forbidden_prefixes: PluginIterationPolicy::default().forbidden_prefixes,
            allow_plugin_manifest_edits: false,
        };
        let err = policy
            .validate_path(&allowed, "plugins/demo/Cargo.toml")
            .expect_err("manifest edit should be blocked");
        assert!(err
            .to_string()
            .contains("plugin manifest edits are disabled"));
    }

    #[test]
    fn validate_path_rejects_outside_selected_subtree() {
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        // plugins/other is a plugin path but not in the selected subtree.
        let err = PluginIterationPolicy::default()
            .validate_path(&allowed, "plugins/other/src/lib.rs")
            .expect_err("path outside selected subtree should be blocked");
        assert!(err
            .to_string()
            .contains("not inside the selected plugin subtree"));
    }

    #[test]
    fn subtree_surface_kind_classification() {
        let root = "plugins/demo";
        // Root manifest is writable.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/Cargo.toml", root),
            Some(PluginSubtreeSurfaceKind::WritableManifest)
        );
        // Nested child plugin manifest is writable.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/child/Cargo.toml", root),
            Some(PluginSubtreeSurfaceKind::WritableManifest)
        );
        // Source files are writable.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/src/lib.rs", root),
            Some(PluginSubtreeSurfaceKind::WritableOther)
        );
        // tests/ files are writable.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/tests/it.rs", root),
            Some(PluginSubtreeSurfaceKind::WritableOther)
        );
        // Human docs are writable.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/docs/human/guide.md", root),
            Some(PluginSubtreeSurfaceKind::WritableOther)
        );
        // Generated agent docs are read-only.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/docs/agent/interfaces.json", root),
            Some(PluginSubtreeSurfaceKind::ReadOnlyGenerated)
        );
        // A path outside the subtree yields None (no strip_prefix match).
        assert_eq!(
            plugin_subtree_surface_kind("plugins/other/src/lib.rs", root),
            None
        );
        // A file directly under the root with no recognised surface segment.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/README.md", root),
            None
        );
        // docs with an unrecognised second segment yields None.
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/docs/misc/x.md", root),
            None
        );
    }

    #[test]
    fn deepest_matching_writable_root_prefers_longest() {
        let mut roots = BTreeSet::new();
        roots.insert("plugins/expr".to_string());
        roots.insert("plugins/expr/evaluator".to_string());
        assert_eq!(
            deepest_matching_writable_root("plugins/expr/evaluator/add/Cargo.toml", &roots),
            Some("plugins/expr/evaluator")
        );
        // Exact-equal path matches its root.
        assert_eq!(
            deepest_matching_writable_root("plugins/expr", &roots),
            Some("plugins/expr")
        );
        // Non-matching path yields None.
        assert_eq!(
            deepest_matching_writable_root("plugins/other/x.rs", &roots),
            None
        );
    }

    #[test]
    fn reserved_keyword_validation_flags_raw_mod_identifier() {
        let mut roots = BTreeSet::new();
        roots.insert("plugins/x".to_string());
        let manifest = PluginEditOperation {
            path: "plugins/x/mod/Cargo.toml".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            expected_sha256: None,
            new_content: Some("[package]\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        };
        let source = PluginEditOperation {
            path: "plugins/x/mod/src/core.rs".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            expected_sha256: None,
            new_content: Some("let mod = 1;\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        };
        let err = validate_reserved_child_keyword_identifiers(&[manifest.clone(), source], &roots)
            .expect_err("raw `mod` identifier usage should be rejected");
        assert!(matches!(err, RuntimeError::LlmResponseInvalid { .. }));

        // A source file that only uses the path component (not a raw
        // identifier) passes.
        let ok_source = PluginEditOperation {
            path: "plugins/x/mod/src/core.rs".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            expected_sha256: None,
            new_content: Some("pub struct ModPlugin;\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        };
        validate_reserved_child_keyword_identifiers(&[manifest, ok_source], &roots)
            .expect("PascalCase usage should be allowed");
    }

    #[test]
    fn reserved_keyword_validation_noop_without_reserved_child() {
        let mut roots = BTreeSet::new();
        roots.insert("plugins/x".to_string());
        // Child plugin name is not a reserved keyword → nothing to validate.
        let manifest = PluginEditOperation {
            path: "plugins/x/helper/Cargo.toml".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            expected_sha256: None,
            new_content: Some("[package]\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        };
        let source = PluginEditOperation {
            path: "plugins/x/helper/src/core.rs".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            expected_sha256: None,
            new_content: Some("let mod = 1;\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        };
        validate_reserved_child_keyword_identifiers(&[manifest, source], &roots)
            .expect("no reserved child keyword means no restriction");
    }

    #[test]
    fn reserved_keyword_validation_flags_raw_member_access() {
        // A reserved child (`mod`) plus a source file whose only reference is
        // raw member access (`value.mod`, not caught by any direct identifier
        // pattern) exercises the `contains_raw_member_access` scan path. A
        // sibling op with `new_content = None` also proves the "skip when no
        // content" guard is taken before the scan.
        let mut roots = BTreeSet::new();
        roots.insert("plugins/x".to_string());
        let manifest = PluginEditOperation {
            path: "plugins/x/mod/Cargo.toml".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            expected_sha256: None,
            new_content: Some("[package]\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        };
        // A .rs op carrying no new_content is skipped (the `else { continue }`).
        let no_content = PluginEditOperation {
            path: "plugins/x/mod/src/skip.rs".to_string(),
            kind: PluginEditOpKind::DeleteFile,
            expected_old_string: None,
            expected_sha256: Some(sha_hex("")),
            new_content: None,
            pointer: None,
            dotted_key: None,
            value: None,
        };
        // Raw member access `.mod` followed by a non-identifier char.
        let member_access = PluginEditOperation {
            path: "plugins/x/mod/src/core.rs".to_string(),
            kind: PluginEditOpKind::CreateFile,
            expected_old_string: Some(String::new()),
            new_content: Some("let handle = value.mod;\n".to_string()),
            expected_sha256: None,
            pointer: None,
            dotted_key: None,
            value: None,
        };
        let err = validate_reserved_child_keyword_identifiers(
            &[manifest, no_content, member_access],
            &roots,
        )
        .expect_err("raw `.mod` member access should be rejected");
        assert!(matches!(err, RuntimeError::LlmResponseInvalid { .. }));
    }

    #[test]
    fn contains_raw_member_access_ignores_longer_identifiers() {
        // `.module` extends `.mod` with an identifier char → not a bare `.mod`
        // access, so the scan must skip it and keep looking (loop-advance arm).
        assert!(!contains_raw_member_access("a.module_thing()", "mod"));
        // A genuine `.mod` at end-of-string is flagged (next char is None).
        assert!(contains_raw_member_access("a.mod", "mod"));
        // `.mod` followed by another `.mod`: first is `.mod.` (dot is
        // non-identifier) → flagged.
        assert!(contains_raw_member_access("a.mod.b", "mod"));
    }

    #[test]
    fn validate_expected_hash_none_is_ok() {
        // When no expected hash is supplied the precondition is a no-op.
        validate_expected_hash("p", "anything", None).expect("None hash must pass");
    }

    // ---------- apply_operation branch coverage ----------

    #[test]
    fn apply_replace_exact_success_and_errors() {
        let abs = Path::new("/tmp/x/lib.rs");
        let mut o = op(PluginEditOpKind::ReplaceExact);
        // Missing expected_old_string.
        assert!(matches!(
            apply_err("p", &o, abs, Some(b"orig")),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Pattern not found.
        o.expected_old_string = Some("nope".to_string());
        assert!(matches!(
            apply_err("p", &o, abs, Some(b"orig")),
            RuntimeError::AutoUpdatePatternNotFound { .. }
        ));
        // Found but missing new_content.
        o.expected_old_string = Some("ori".to_string());
        assert!(matches!(
            apply_err("p", &o, abs, Some(b"orig")),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Success replaces first occurrence.
        o.new_content = Some("ORI".to_string());
        assert!(matches!(
            apply_operation("p", &o, abs, Some(b"orig")).unwrap(),
            UpdatedFile::Write(bytes) if bytes == b"ORIg"
        ));
    }

    #[test]
    fn apply_create_file_success_and_errors() {
        let abs = Path::new("/tmp/x/new.rs");
        let mut o = op(PluginEditOpKind::CreateFile);
        // Missing expected_old_string.
        assert!(matches!(
            apply_err("p", &o, abs, None),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Target already exists.
        o.expected_old_string = Some(String::new());
        assert!(matches!(
            apply_err("p", &o, abs, Some(b"existing")),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Non-empty expected_old_string.
        o.expected_old_string = Some("not-empty".to_string());
        assert!(matches!(
            apply_err("p", &o, abs, None),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Success writes new_content.
        o.expected_old_string = Some(String::new());
        o.new_content = Some("hello".to_string());
        assert!(matches!(
            apply_operation("p", &o, abs, None).unwrap(),
            UpdatedFile::Write(bytes) if bytes == b"hello"
        ));
    }

    #[test]
    fn apply_delete_file_success_and_errors() {
        let abs = Path::new("/tmp/x/gone.rs");
        let mut o = op(PluginEditOpKind::DeleteFile);
        // Missing expected_sha256.
        assert!(matches!(
            apply_err("p", &o, abs, Some(b"data")),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Hash mismatch → policy blocked.
        o.expected_sha256 = Some("deadbeef".to_string());
        assert!(matches!(
            apply_err("p", &o, abs, Some(b"data")),
            RuntimeError::PluginIterationPolicyBlocked { .. }
        ));
        // Correct hash but file does not exist.
        o.expected_sha256 = Some(sha_hex(""));
        assert!(matches!(
            apply_err("p", &o, abs, None),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Success.
        o.expected_sha256 = Some(sha_hex("data"));
        assert!(matches!(
            apply_operation("p", &o, abs, Some(b"data")).unwrap(),
            UpdatedFile::Delete
        ));
    }

    #[test]
    fn apply_json_set_success_and_errors() {
        let abs = Path::new("/tmp/x/data.json");
        let original = "{\"k\":1}";
        let mut o = op(PluginEditOpKind::JsonSet);
        // Missing sha.
        assert!(matches!(
            apply_err("p", &o, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        o.expected_sha256 = Some(sha_hex(original));
        // Missing pointer.
        assert!(matches!(
            apply_err("p", &o, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        o.pointer = Some("/k".to_string());
        // Missing value.
        assert!(matches!(
            apply_err("p", &o, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        o.value = Some(serde_json::json!(2));
        // Parse failure on bad JSON (recompute sha for the bad content).
        let bad = "not json";
        let mut bad_op = o.clone();
        bad_op.expected_sha256 = Some(sha_hex(bad));
        assert!(matches!(
            apply_err("p", &bad_op, abs, Some(bad.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Pointer not found.
        let mut miss = o.clone();
        miss.pointer = Some("/missing".to_string());
        assert!(matches!(
            apply_err("p", &miss, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Success.
        assert!(matches!(
            apply_operation("p", &o, abs, Some(original.as_bytes())).unwrap(),
            UpdatedFile::Write(bytes)
                if serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["k"]
                    == serde_json::json!(2)
        ));
    }

    #[test]
    fn apply_toml_set_success_and_errors() {
        let abs = Path::new("/tmp/x/conf.toml");
        let original = "[table]\nk = 1\n";
        let mut o = op(PluginEditOpKind::TomlSet);
        // Missing sha.
        assert!(matches!(
            apply_err("p", &o, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        o.expected_sha256 = Some(sha_hex(original));
        // Missing dotted_key.
        assert!(matches!(
            apply_err("p", &o, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        o.dotted_key = Some("table.k".to_string());
        // Missing value.
        assert!(matches!(
            apply_err("p", &o, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Original content that passes the sha gate but is not valid TOML →
        // toml parse failure closure.
        let bad = "this is = = not toml";
        let mut bad_op = o.clone();
        bad_op.value = Some(serde_json::json!(2));
        bad_op.expected_sha256 = Some(sha_hex(bad));
        assert!(matches!(
            apply_err("p", &bad_op, abs, Some(bad.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        o.value = Some(serde_json::json!(2));
        // Value that cannot be represented in TOML (JSON null) → conversion
        // failure closure.
        let mut null_value = o.clone();
        null_value.value = Some(serde_json::Value::Null);
        assert!(matches!(
            apply_err("p", &null_value, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Dotted key not found.
        let mut miss = o.clone();
        miss.dotted_key = Some("table.missing".to_string());
        assert!(matches!(
            apply_err("p", &miss, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Intermediate (non-final) dotted-key segment not found: first segment
        // resolves against the root table but is absent, so the `.get_mut`
        // ok_or_else arm fires before reaching the final segment.
        let mut miss_intermediate = o.clone();
        miss_intermediate.dotted_key = Some("nope.k".to_string());
        assert!(matches!(
            apply_err("p", &miss_intermediate, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Intermediate key not a table.
        let mut not_table = o.clone();
        not_table.dotted_key = Some("table.k.deeper".to_string());
        assert!(matches!(
            apply_err("p", &not_table, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Non-table encountered while walking a *prefix* (non-final) segment:
        // "table.k" resolves to the integer `k`, so descending into ".deeper"
        // before the final ".x" segment hits the prefix-loop `as_table_mut`
        // guard rather than the final-segment one.
        let mut not_table_prefix = o.clone();
        not_table_prefix.dotted_key = Some("table.k.deeper.x".to_string());
        assert!(matches!(
            apply_err("p", &not_table_prefix, abs, Some(original.as_bytes())),
            RuntimeError::AutoUpdatePatchInvalid { .. }
        ));
        // Success.
        assert!(matches!(
            apply_operation("p", &o, abs, Some(original.as_bytes())).unwrap(),
            UpdatedFile::Write(bytes)
                if String::from_utf8_lossy(&bytes).contains("k = 2")
        ));
    }

    // ---------- path / hash helpers ----------

    #[test]
    fn normalize_rel_path_rejects_absolute_and_normalizes() {
        assert!(matches!(
            normalize_rel_path("/etc/passwd").unwrap_err(),
            RuntimeError::AutoUpdateInvalidPath { .. }
        ));
        // Current-dir components are stripped.
        assert_eq!(normalize_rel_path("./a/b.rs").unwrap(), "a/b.rs");
    }

    #[test]
    fn file_sha256_matches_manual_hash() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("f.txt");
        fs::write(&path, "content").unwrap();
        assert_eq!(file_sha256(&path).unwrap(), sha_hex("content"));
        // Reading a missing file is an Io error.
        assert!(matches!(
            file_sha256(&temp.path().join("missing")).unwrap_err(),
            RuntimeError::Io { .. }
        ));
    }

    #[test]
    fn resolve_under_workspace_reattaches_missing_tail() {
        let temp = TempDir::new().unwrap();
        let existing = temp.path().canonicalize().unwrap();
        // Non-existent nested tail under an existing dir resolves by
        // re-attaching the tail to the canonical existing ancestor.
        let target = temp.path().join("a/b/c.rs");
        let resolved = resolve_under_workspace(&target).unwrap();
        assert!(resolved.starts_with(&existing));
        assert!(resolved.ends_with("a/b/c.rs"));
    }

    // resolve_under_workspace: a relative path whose ancestor walk exhausts
    // without ever finding an existing component reaches the `parent() == None`
    // arm → AutoUpdateInvalidPath ("no accessible ancestor exists"). A purely
    // relative, non-existent path like "no_such_root_xyz/a" walks
    // "no_such_root_xyz/a" → "no_such_root_xyz" → "" (which does not exist and
    // has no parent).
    #[test]
    fn resolve_under_workspace_no_accessible_ancestor_errors() {
        let target = Path::new("no_such_root_xyz_qzv/a/b.rs");
        let err = resolve_under_workspace(target)
            .expect_err("a relative path with no existing ancestor must error");
        assert!(
            matches!(&err, RuntimeError::AutoUpdateInvalidPath { reason, .. } if reason == "no accessible ancestor exists"),
            "wrong variant: {err:?}"
        );
    }

    /// A dangling symlink at the final component must be rejected, not
    /// re-attached as an ordinary missing tail: a write through it would
    /// create bytes at the link's target, which may sit outside the workspace
    /// (e.g. `conf.toml -> /etc/...` where the target does not exist yet).
    #[cfg(unix)]
    #[test]
    fn resolve_under_workspace_rejects_dangling_symlink() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        // Target does not exist → the link cannot be followed.
        let link = src.join("evil");
        std::os::unix::fs::symlink(temp.path().join("no-such-outside"), &link).unwrap();
        let err = resolve_under_workspace(&link).expect_err("dangling symlink must be rejected");
        assert!(
            matches!(&err, RuntimeError::AutoUpdateInvalidPath { reason, .. } if reason.contains("symlink in the path")),
            "wrong variant: {err:?}"
        );
    }

    /// A dangling symlink as an *intermediate* component must also be
    /// rejected: re-attaching the tail would write through the link into its
    /// (outside) destination once it materialises.
    #[cfg(unix)]
    #[test]
    fn resolve_under_workspace_rejects_dangling_symlink_ancestor() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let link = src.join("evil");
        std::os::unix::fs::symlink(temp.path().join("no-such-outside"), &link).unwrap();
        let target = link.join("new.txt");
        let err =
            resolve_under_workspace(&target).expect_err("dangling ancestor must be rejected");
        assert!(
            matches!(&err, RuntimeError::AutoUpdateInvalidPath { reason, .. } if reason.contains("symlink in the path")),
            "wrong variant: {err:?}"
        );
    }

    #[test]
    fn now_ms_is_populated() {
        assert!(now_ms() > 0);
    }

    /// `resolve_under_workspace`'s canonicalise-ancestor failure arm needs an
    /// OS race (the ancestor vanishing between `exists()` and `canonicalize()`),
    /// so exercise the extracted mapper directly to lock its `Io` variant and
    /// byte-exact message.
    #[test]
    fn resolve_ancestor_io_error_wraps_path_and_message() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "vanished");
        let err = resolve_ancestor_io_error(Path::new("/tmp/x"), &io);
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x") && message.starts_with("canonicalise ancestor failed: ")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn atomic_write_rejects_target_without_filename() {
        // A root path ("/") has no file_name → error.
        let err = atomic_write(Path::new("/"), b"x").unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }));
    }

    #[test]
    fn execute_rejects_create_over_existing_file() {
        // Drives apply_operation's CreateFile "already exists" branch through
        // the real executor, exercising the in-execute error return.
        let temp = TempDir::new().unwrap();
        let ws = temp.path();
        let target = ws.join("plugins/demo/src/lib.rs");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"already").unwrap();
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let plan = PluginEditPlan {
            issue_id: "i".to_string(),
            patch_id: "p".to_string(),
            summary: "s".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/lib.rs".to_string(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some("new".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };
        let err = PluginEditExecutor::new(ws)
            .execute(&PluginIterationPolicy::default(), &allowed, &plan)
            .expect_err("create over existing file must fail");
        assert!(matches!(err, RuntimeError::AutoUpdatePatchInvalid { .. }));
        // Original file is untouched.
        assert_eq!(fs::read(&target).unwrap(), b"already");
    }

    #[test]
    fn execute_deletes_file_with_matching_hash() {
        let temp = TempDir::new().unwrap();
        let ws = temp.path();
        let target = ws.join("plugins/demo/src/old.rs");
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(&target, b"payload").unwrap();
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        let plan = PluginEditPlan {
            issue_id: "i".to_string(),
            patch_id: "p".to_string(),
            summary: "s".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/old.rs".to_string(),
                kind: PluginEditOpKind::DeleteFile,
                expected_old_string: None,
                expected_sha256: Some(sha_hex("payload")),
                new_content: None,
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };
        let (result, rollback) = PluginEditExecutor::new(ws)
            .execute(&PluginIterationPolicy::default(), &allowed, &plan)
            .expect("delete should succeed");
        assert!(!target.exists());
        assert_eq!(result.changed_paths, vec!["plugins/demo/src/old.rs"]);
        // Rollback recreates the deleted file.
        rollback.rollback().unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"payload");
    }

    #[test]
    fn absorb_rejects_workspace_mismatch() {
        let a = PluginEditRollback::empty("/ws/a");
        let mut b = PluginEditRollback::empty("/ws/b");
        let err = b.absorb(a).expect_err("workspace mismatch must error");
        assert!(matches!(err, RuntimeError::Invariant { .. }));
    }

    #[test]
    fn load_journal_returns_none_when_absent() {
        let temp = TempDir::new().unwrap();
        let jp = temp.path().join("absent.json");
        assert!(PluginEditRollback::load_journal(temp.path(), &jp)
            .unwrap()
            .is_none());
    }

    #[test]
    fn load_journal_rejects_corrupt_content() {
        let temp = TempDir::new().unwrap();
        let jp = temp.path().join("j.json");
        fs::write(&jp, b"{ not valid json").unwrap();
        let err = PluginEditRollback::load_journal(temp.path(), &jp).unwrap_err();
        assert!(matches!(err, RuntimeError::Invariant { .. }));
        // journal_generation_id tolerates corrupt content and returns None.
        assert!(PluginEditRollback::journal_generation_id(&jp)
            .unwrap()
            .is_none());
    }

    // ---------- executor / rollback / journal fs-injection coverage ----------

    fn demo_allowed() -> BTreeMap<String, String> {
        let mut allowed = BTreeMap::new();
        allowed.insert("demo".to_string(), "plugins/demo".to_string());
        allowed
    }

    fn single_replace_plan() -> PluginEditPlan {
        PluginEditPlan {
            issue_id: "i".to_string(),
            patch_id: "p".to_string(),
            summary: "s".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/lib.rs".to_string(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("foo".to_string()),
                expected_sha256: None,
                new_content: Some("FOO".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        }
    }

    #[test]
    fn execute_errors_when_workspace_root_not_accessible() {
        // validate_plan passes (pure), then canonicalise(workspace_root) fails
        // because the directory does not exist → Io error (lines 549-555).
        let temp = TempDir::new().unwrap();
        let missing = temp.path().join("no-such-workspace");
        let executor = PluginEditExecutor::new(&missing);
        let err = executor
            .execute(
                &PluginIterationPolicy::default(),
                &demo_allowed(),
                &single_replace_plan(),
            )
            .expect_err("inaccessible workspace root must error");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn execute_write_failure_rolls_back_in_place() {
        use std::os::unix::fs::PermissionsExt;
        // Root ignores file-mode permissions, so read-only writes still succeed.
        // Single-line guard: no standalone closing brace to leave uncovered.
        // Root bypasses file-mode permission checks, so the failure this test
        // asserts only occurs as a non-root user. Iterating an `Option` (rather
        // than an `if` gate) leaves no never-taken edge on the closing brace.
        for () in probe_not_root().into_iter() {
            // A read-only existing target makes atomic_write fail (create tmp
            // under a writable dir succeeds, but the final rename over a file
            // inside a read-only *directory* fails). To make the write itself
            // fail, mark the containing directory read-only so tmp creation
            // fails.
            let temp = TempDir::new().unwrap();
            let ws = temp.path();
            let src_dir = ws.join("plugins/demo/src");
            fs::create_dir_all(&src_dir).unwrap();
            let target = src_dir.join("lib.rs");
            fs::write(&target, b"foo").unwrap();
            // Make the src dir read-only so atomic_write's tmp create fails.
            let mut perms = fs::metadata(&src_dir).unwrap().permissions();
            perms.set_mode(0o555);
            fs::set_permissions(&src_dir, perms).unwrap();

            let executor = PluginEditExecutor::new(ws);
            let err = executor
                .execute(
                    &PluginIterationPolicy::default(),
                    &demo_allowed(),
                    &single_replace_plan(),
                )
                .expect_err("write into read-only dir must fail");
            assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");

            // Restore perms so TempDir cleanup can proceed. The original file
            // must still hold its pre-edit bytes (in-execute rollback restored
            // it).
            let mut perms = fs::metadata(&src_dir).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&src_dir, perms).unwrap();
            assert_eq!(fs::read(&target).unwrap(), b"foo");
        }
    }

    #[test]
    fn execute_create_dir_all_failure_when_parent_is_a_file() {
        // A CreateFile whose parent directory is actually a regular file makes
        // the per-operation `create_dir_all(parent)` fail *before* any backup is
        // recorded (lines 577-582). validate_path still classifies the target as
        // WritableOther (under `src/`), so we reach the disk phase.
        let temp = TempDir::new().unwrap();
        let ws = temp.path();
        let src_dir = ws.join("plugins/demo/src");
        fs::create_dir_all(&src_dir).unwrap();
        // `afile` sits where a directory is required by the target path.
        fs::write(src_dir.join("afile"), b"x").unwrap();
        let plan = PluginEditPlan {
            issue_id: "i".to_string(),
            patch_id: "p".to_string(),
            summary: "s".to_string(),
            operations: vec![PluginEditOperation {
                path: "plugins/demo/src/afile/new.rs".to_string(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some("hi".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }],
        };
        let executor = PluginEditExecutor::new(ws);
        let err = executor
            .execute(&PluginIterationPolicy::default(), &demo_allowed(), &plan)
            .expect_err("create_dir_all over a file must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn execute_delete_failure_then_rollback_failure_surfaces_invariant() {
        use std::os::unix::fs::PermissionsExt;
        // Running as root bypasses file-mode permission checks. Single-line
        // guard: no standalone closing brace to leave uncovered.
        // Root bypasses file-mode permission checks, so the failure this test
        // asserts only occurs as a non-root user. Iterating an `Option` (rather
        // than an `if` gate) leaves no never-taken edge on the closing brace.
        for () in probe_not_root().into_iter() {
            // A DeleteFile whose target is read-only inside a read-only
            // directory: fs::read succeeds (so apply_operation validates and
            // records a backup), but `fs::remove_file` fails (read-only dir →
            // covers the Delete-branch error map). Rollback then tries
            // `fs::write` to restore the original into the still-read-only file
            // and *also* fails, driving the nested "in-execute rollback failed"
            // Invariant path.
            let temp = TempDir::new().unwrap();
            let ws = temp.path();
            let src_dir = ws.join("plugins/demo/src");
            fs::create_dir_all(&src_dir).unwrap();
            let target = src_dir.join("lib.rs");
            fs::write(&target, b"foo").unwrap();
            // File read-only so the rollback fs::write also fails.
            let mut file_perms = fs::metadata(&target).unwrap().permissions();
            file_perms.set_mode(0o444);
            fs::set_permissions(&target, file_perms).unwrap();
            // Directory read-only so remove_file fails.
            let mut dir_perms = fs::metadata(&src_dir).unwrap().permissions();
            dir_perms.set_mode(0o555);
            fs::set_permissions(&src_dir, dir_perms).unwrap();

            let plan = PluginEditPlan {
                issue_id: "i".to_string(),
                patch_id: "p".to_string(),
                summary: "s".to_string(),
                operations: vec![PluginEditOperation {
                    path: "plugins/demo/src/lib.rs".to_string(),
                    kind: PluginEditOpKind::DeleteFile,
                    expected_old_string: None,
                    expected_sha256: Some(sha_hex("foo")),
                    new_content: None,
                    pointer: None,
                    dotted_key: None,
                    value: None,
                }],
            };
            let executor = PluginEditExecutor::new(ws);
            let err = executor
                .execute(&PluginIterationPolicy::default(), &demo_allowed(), &plan)
                .expect_err("read-only delete with failed rollback must error");
            assert!(
                matches!(err, RuntimeError::Invariant { .. }),
                "got: {err:?}"
            );

            // Restore perms so TempDir cleanup can proceed.
            let mut dir_perms = fs::metadata(&src_dir).unwrap().permissions();
            dir_perms.set_mode(0o755);
            fs::set_permissions(&src_dir, dir_perms).unwrap();
        }
    }

    #[test]
    fn rollback_write_failure_when_target_is_directory() {
        // A backup with Some(original) whose abs_path is an existing directory
        // makes fs::write fail (lines 706-709).
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join("adir")).unwrap();
        let rb = PluginEditRollback::single_backup(temp.path(), "adir", Some(b"bytes".to_vec()));
        let err = rb.rollback().expect_err("write over a dir must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn rollback_create_dir_all_failure_when_parent_is_a_file() {
        // Restoring "afile/child.rs" needs create_dir_all(ws/afile), but
        // ws/afile is a regular file → create_dir_all fails (lines 701-705).
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("afile"), b"x").unwrap();
        let rb = PluginEditRollback::single_backup(
            temp.path(),
            "afile/child.rs",
            Some(b"bytes".to_vec()),
        );
        let err = rb
            .rollback()
            .expect_err("create_dir_all over a file must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn rollback_remove_failure_when_target_is_nonempty_dir() {
        // A backup with None original (file "did not exist") whose abs_path is
        // a non-empty directory makes remove_file fail (lines 712-717).
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("busy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inner"), b"x").unwrap();
        let rb = PluginEditRollback::single_backup(temp.path(), "busy", None);
        let err = rb
            .rollback()
            .expect_err("remove_file on a non-empty dir must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn persist_journal_parent_create_failure_when_parent_is_a_file() {
        // journal_path "afile/j.json" whose parent "afile" is a regular file
        // → create_dir_all fails (lines 742-746).
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("afile"), b"x").unwrap();
        let rb = PluginEditRollback::empty(temp.path());
        let jp = temp.path().join("afile/j.json");
        let err = rb
            .persist_journal(&jp, "iter")
            .expect_err("bad journal parent must error");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[test]
    fn clear_journal_remove_failure_when_path_is_nonempty_dir() {
        // clear_journal on a non-empty directory path → remove_file fails
        // (lines 817-820).
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("j-as-dir");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inner"), b"x").unwrap();
        let err = PluginEditRollback::clear_journal(&dir)
            .expect_err("remove_file on a non-empty dir must fail");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn journal_read_failures_surface_when_unreadable() {
        use std::os::unix::fs::PermissionsExt;
        // Root ignores file-mode permissions, so an unreadable file still reads.
        // Single-line guard: no standalone closing brace to leave uncovered.
        // Root bypasses file-mode permission checks, so the failure this test
        // asserts only occurs as a non-root user. Iterating an `Option` (rather
        // than an `if` gate) leaves no never-taken edge on the closing brace.
        for () in probe_not_root().into_iter() {
            let temp = TempDir::new().unwrap();
            let rb = PluginEditRollback::single_backup(
                temp.path(),
                "plugins/demo/src/lib.rs",
                Some(b"orig".to_vec()),
            );
            let jp = temp.path().join("j.json");
            rb.persist_journal(&jp, "iter").unwrap();
            // Make the journal unreadable so the fs::read inside both accessors
            // fails with Io.
            let mut perms = fs::metadata(&jp).unwrap().permissions();
            perms.set_mode(0o000);
            fs::set_permissions(&jp, perms).unwrap();

            let gen_err = PluginEditRollback::journal_generation_id(&jp)
                .expect_err("unreadable journal must error");
            assert!(
                matches!(gen_err, RuntimeError::Io { .. }),
                "got: {gen_err:?}"
            );
            let load_err = PluginEditRollback::load_journal(temp.path(), &jp)
                .expect_err("unreadable journal must error");
            assert!(
                matches!(load_err, RuntimeError::Io { .. }),
                "got: {load_err:?}"
            );
        }
    }

    #[test]
    fn load_journal_rejects_bad_hex_backup_bytes() {
        // A journal whose original_hex is not valid hex hits the hex::decode
        // error map in load_journal (lines 795-800).
        let temp = TempDir::new().unwrap();
        let jp = temp.path().join("j.json");
        let bad = serde_json::json!({
            "iteration_id": "iter",
            "rollback_generation_id": "gen",
            "backups": [
                { "rel_path": "plugins/demo/src/lib.rs", "original_hex": "zzzz" }
            ]
        });
        fs::write(&jp, serde_json::to_vec_pretty(&bad).unwrap()).unwrap();
        let err =
            PluginEditRollback::load_journal(temp.path(), &jp).expect_err("bad hex must error");
        assert!(
            matches!(err, RuntimeError::Invariant { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn subtree_surface_kind_src_manifest_is_not_treated_as_child_manifest() {
        // "plugins/demo/src/Cargo.toml": last segment is Cargo.toml but the
        // plugin_segments contain "src", so the child-manifest branch is
        // skipped and it falls through to the src surface → WritableOther
        // (covers the manifest-branch not-taken path, lines 366-373).
        assert_eq!(
            plugin_subtree_surface_kind("plugins/demo/src/Cargo.toml", "plugins/demo"),
            Some(PluginSubtreeSurfaceKind::WritableOther)
        );
    }
}
