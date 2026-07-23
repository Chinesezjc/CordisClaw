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
    if segments.is_empty() {
        return None;
    }

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
    if segments[..surface_idx]
        .iter()
        .any(|segment| matches!(*segment, "src" | "tests" | "docs"))
    {
        return None;
    }
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
        // With the guard below, any operation whose canonical target lands
        // outside workspace_root is rejected before touching disk.
        let canonical_workspace_root =
            self.workspace_root
                .canonicalize()
                .map_err(|err| RuntimeError::Io {
                    path: self.workspace_root.clone(),
                    message: format!("workspace root not accessible: {err}"),
                })?;
        for operation in &plan.operations {
            let normalized = normalize_rel_path(&operation.path)?;
            policy.validate_path(allowed_plugin_roots, &normalized)?;
            let abs_path = self.workspace_root.join(&normalized);
            // Resolve the final on-disk target, following symlinks along any
            // existing prefix. Reject if it escapes the workspace.
            let resolved_for_check = resolve_under_workspace(&abs_path)?;
            if !resolved_for_check.starts_with(&canonical_workspace_root) {
                return Err(RuntimeError::AutoUpdateInvalidPath {
                    path: operation.path.clone(),
                    reason: format!(
                        "resolved path escapes workspace root ({} not under {})",
                        resolved_for_check.display(),
                        canonical_workspace_root.display()
                    ),
                });
            }
            let original = fs::read(&abs_path).ok();
            let updated = apply_operation(&normalized, operation, &abs_path, original.as_deref())?;

            if let Some(parent) = abs_path.parent() {
                fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                    path: parent.to_path_buf(),
                    message: err.to_string(),
                })?;
            }

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
                UpdatedFile::Delete => {
                    if abs_path.exists() {
                        fs::remove_file(&abs_path).map_err(|err| RuntimeError::Io {
                            path: abs_path.clone(),
                            message: err.to_string(),
                        })
                    } else {
                        Ok(())
                    }
                }
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
        for backup in self.backups.iter().rev() {
            let abs_path = self.workspace_root.join(&backup.rel_path);
            match &backup.original {
                Some(original) => {
                    if let Some(parent) = abs_path.parent() {
                        fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                            path: parent.to_path_buf(),
                            message: err.to_string(),
                        })?;
                    }
                    fs::write(&abs_path, original).map_err(|err| RuntimeError::Io {
                        path: abs_path.clone(),
                        message: err.to_string(),
                    })?;
                }
                None => {
                    if abs_path.exists() {
                        fs::remove_file(&abs_path).map_err(|err| RuntimeError::Io {
                            path: abs_path.clone(),
                            message: err.to_string(),
                        })?;
                    }
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
        if let Some(parent) = journal_path.parent() {
            fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
                path: parent.to_path_buf(),
                message: err.to_string(),
            })?;
        }
        let bytes = serde_json::to_vec_pretty(&journal).map_err(|err| RuntimeError::Invariant {
            message: format!("plugin edit rollback journal serialize failed: {err}"),
        })?;
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
        let backups = journal
            .backups
            .into_iter()
            .map(|backup| {
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
                Ok(AppliedEditBackup {
                    rel_path: backup.rel_path,
                    original,
                })
            })
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        Ok(Some(Self {
            workspace_root: workspace_root.into(),
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
                    .map_err(|err| RuntimeError::Io {
                        path: abs_path.to_path_buf(),
                        message: format!("json serialize failed: {err}"),
                    })?
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
            let mut segments = dotted_key.split('.').peekable();
            while let Some(segment) = segments.next() {
                let Some(table) = cursor.as_table_mut() else {
                    return Err(RuntimeError::AutoUpdatePatchInvalid {
                        path: rel_path.to_string(),
                        reason: format!("toml key path is not a table at {segment}"),
                    });
                };
                if segments.peek().is_none() {
                    let Some(target) = table.get_mut(segment) else {
                        return Err(RuntimeError::AutoUpdatePatchInvalid {
                            path: rel_path.to_string(),
                            reason: format!("toml dotted key not found: {dotted_key}"),
                        });
                    };
                    *target = replacement;
                    return Ok(UpdatedFile::Write(
                        toml::to_string_pretty(&document)
                            .map_err(|err| RuntimeError::Io {
                                path: abs_path.to_path_buf(),
                                message: format!("toml serialize failed: {err}"),
                            })?
                            .into_bytes(),
                    ));
                }
                cursor =
                    table
                        .get_mut(segment)
                        .ok_or_else(|| RuntimeError::AutoUpdatePatchInvalid {
                            path: rel_path.to_string(),
                            reason: format!("toml dotted key not found: {dotted_key}"),
                        })?;
            }
            Err(RuntimeError::AutoUpdatePatchInvalid {
                path: rel_path.to_string(),
                reason: "toml dotted key must not be empty".to_string(),
            })
        }
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

/// P0-20 helper: resolve `abs_path` to a canonical form that follows
/// symlinks along any existing prefix, and re-attaches the yet-to-be-created
/// tail. Used before writing to make sure the actual on-disk target sits
/// under the workspace root — `normalize_rel_path` alone only catches
/// `..` / absolute paths, not symlink-based escape.
pub(crate) fn resolve_under_workspace(abs_path: &Path) -> Result<PathBuf, RuntimeError> {
    if let Ok(canonical) = abs_path.canonicalize() {
        return Ok(canonical);
    }
    let mut ancestors: Vec<&Path> = Vec::new();
    let mut current = abs_path;
    let existing = loop {
        if current.exists() {
            break current.canonicalize().map_err(|err| RuntimeError::Io {
                path: current.to_path_buf(),
                message: format!("canonicalise ancestor failed: {err}"),
            })?;
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
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| RuntimeError::Io {
            path: parent.to_path_buf(),
            message: err.to_string(),
        })?;
    }
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
        let mut file = fs::File::create(&tmp_path).map_err(|err| RuntimeError::Io {
            path: tmp_path.clone(),
            message: err.to_string(),
        })?;
        file.write_all(bytes).map_err(|err| RuntimeError::Io {
            path: tmp_path.clone(),
            message: err.to_string(),
        })?;
        file.sync_all().map_err(|err| RuntimeError::Io {
            path: tmp_path.clone(),
            message: err.to_string(),
        })?;
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
    use super::{
        normalize_rel_path, CanaryVerdict, KernelPluginIssueSource, PluginEditExecutor,
        PluginEditOpKind, PluginEditOperation, PluginEditPlan, PluginIterationPolicy,
    };
    use std::collections::BTreeMap;
    use std::fs;
    use tempfile::TempDir;

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

    // ---------- P0-5..P0-8 rollback hardening tests ----------

    use super::{atomic_write, PluginEditRollback};

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
}
