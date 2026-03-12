//! Minimal automatic update runner.
//! It applies text patches on workspace files, runs verification, and rolls back on failure.

use crate::core::error::RuntimeError;
use crate::kernel::evaluator::VerificationInput;
use crate::kernel::memory::ChangeVerdict;
use crate::kernel::r#loop::{IterationInput, IterationReport, SelfIterationKernel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FilePatch {
    /// Relative path under workspace root.
    pub path: String,
    /// Text to find once.
    pub find: String,
    /// Replacement text.
    pub replace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoUpdatePlan {
    pub issue_id: String,
    pub patch_id: String,
    pub manual_approved: bool,
    pub diff_lines: usize,
    pub patches: Vec<FilePatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AutoUpdateResult {
    pub report: IterationReport,
    pub changed_paths: Vec<String>,
    pub rolled_back: bool,
}

#[derive(Debug, Clone)]
pub struct AutoUpdater {
    workspace_root: PathBuf,
}

#[derive(Debug, Clone)]
struct AppliedBackup {
    abs_path: PathBuf,
    original: String,
}

impl AutoUpdater {
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
        }
    }

    /// Execute one automatic update transaction.
    /// - apply all patches
    /// - run verification callback
    /// - evaluate with kernel policy
    /// - rollback when verdict is Rollback
    pub fn execute<F>(
        &self,
        kernel: &mut SelfIterationKernel,
        plan: AutoUpdatePlan,
        verify: F,
    ) -> Result<AutoUpdateResult, RuntimeError>
    where
        F: FnOnce(&Path) -> Result<VerificationInput, RuntimeError>,
    {
        let mut backups = Vec::new();
        let mut changed_paths = BTreeSet::new();

        for patch in &plan.patches {
            let abs_path = self.resolve_patch_path(&patch.path)?;
            let original = fs::read_to_string(&abs_path).map_err(|e| RuntimeError::Io {
                path: abs_path.clone(),
                message: e.to_string(),
            })?;

            if !original.contains(&patch.find) {
                self.rollback(&backups)?;
                return Err(RuntimeError::AutoUpdatePatternNotFound {
                    path: abs_path,
                    pattern: patch.find.clone(),
                });
            }

            // Replace one occurrence to keep patch intent deterministic.
            let updated = original.replacen(&patch.find, &patch.replace, 1);
            fs::write(&abs_path, updated).map_err(|e| RuntimeError::Io {
                path: abs_path.clone(),
                message: e.to_string(),
            })?;

            backups.push(AppliedBackup { abs_path, original });
            changed_paths.insert(patch.path.clone());
        }

        let verification = match verify(&self.workspace_root) {
            Ok(v) => v,
            Err(err) => {
                self.rollback(&backups)?;
                return Err(RuntimeError::AutoUpdateVerifyFailed {
                    message: err.to_string(),
                });
            }
        };

        let report = kernel.run_once(IterationInput {
            issue_id: plan.issue_id,
            patch_id: plan.patch_id,
            changed_paths: changed_paths.iter().cloned().collect(),
            diff_lines: plan.diff_lines,
            manual_approved: plan.manual_approved,
            verification,
        });

        let rolled_back = report.verdict == ChangeVerdict::Rollback;
        if rolled_back {
            self.rollback(&backups)?;
        }

        Ok(AutoUpdateResult {
            report,
            changed_paths: changed_paths.into_iter().collect(),
            rolled_back,
        })
    }

    fn resolve_patch_path(&self, rel: &str) -> Result<PathBuf, RuntimeError> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(RuntimeError::AutoUpdateInvalidPath {
                path: rel.to_string(),
                reason: "absolute path is not allowed".to_string(),
            });
        }
        if rel_path.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(RuntimeError::AutoUpdateInvalidPath {
                path: rel.to_string(),
                reason: "parent directory traversal (..) is not allowed".to_string(),
            });
        }

        Ok(self.workspace_root.join(rel_path))
    }

    fn rollback(&self, backups: &[AppliedBackup]) -> Result<(), RuntimeError> {
        for backup in backups.iter().rev() {
            fs::write(&backup.abs_path, &backup.original).map_err(|e| RuntimeError::Io {
                path: backup.abs_path.clone(),
                message: e.to_string(),
            })?;
        }
        Ok(())
    }
}
