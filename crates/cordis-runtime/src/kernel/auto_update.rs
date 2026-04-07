//! Minimal automatic update runner.
//! It applies text patches on workspace files, runs verification, and rolls back on failure.

use crate::core::error::RuntimeError;
use crate::kernel::evaluator::VerificationInput;
use crate::kernel::memory::ChangeVerdict;
use crate::kernel::r#loop::{IterationInput, IterationReport, SelfIterationKernel};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use toml::Value as TomlValue;

fn default_file_patch_kind() -> FilePatchKind {
    FilePatchKind::Text
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FilePatchKind {
    Text,
    JsonValue,
    TomlValue,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FilePatch {
    /// Relative path under workspace root.
    pub path: String,
    #[serde(default = "default_file_patch_kind")]
    pub kind: FilePatchKind,
    /// Text to find once.
    #[serde(default)]
    pub find: String,
    /// Replacement text.
    #[serde(default)]
    pub replace: String,
    #[serde(default)]
    pub pointer: Option<String>,
    #[serde(default)]
    pub dotted_key: Option<String>,
    #[serde(default)]
    pub value: Option<Value>,
}

impl FilePatch {
    pub fn text(
        path: impl Into<String>,
        find: impl Into<String>,
        replace: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            kind: FilePatchKind::Text,
            find: find.into(),
            replace: replace.into(),
            pointer: None,
            dotted_key: None,
            value: None,
        }
    }

    pub fn json_value(path: impl Into<String>, pointer: impl Into<String>, value: Value) -> Self {
        Self {
            path: path.into(),
            kind: FilePatchKind::JsonValue,
            find: String::new(),
            replace: String::new(),
            pointer: Some(pointer.into()),
            dotted_key: None,
            value: Some(value),
        }
    }

    pub fn toml_value(
        path: impl Into<String>,
        dotted_key: impl Into<String>,
        value: Value,
    ) -> Self {
        Self {
            path: path.into(),
            kind: FilePatchKind::TomlValue,
            find: String::new(),
            replace: String::new(),
            pointer: None,
            dotted_key: Some(dotted_key.into()),
            value: Some(value),
        }
    }

    pub fn patch_kind_name(&self) -> &'static str {
        match self.kind {
            FilePatchKind::Text => "text",
            FilePatchKind::JsonValue => "json_value",
            FilePatchKind::TomlValue => "toml_value",
        }
    }

    pub fn diff_line_estimate(&self) -> usize {
        match self.kind {
            FilePatchKind::Text => self
                .find
                .lines()
                .count()
                .max(self.replace.lines().count())
                .max(1),
            FilePatchKind::JsonValue | FilePatchKind::TomlValue => 1,
        }
    }

    pub fn validate_shape(&self) -> Result<(), RuntimeError> {
        match self.kind {
            FilePatchKind::Text => {
                if self.find.is_empty() {
                    return Err(RuntimeError::LlmResponseInvalid {
                        message: format!("text patch for {} is missing `find`", self.path),
                    });
                }
            }
            FilePatchKind::JsonValue => {
                if self.pointer.as_deref().unwrap_or_default().is_empty() {
                    return Err(RuntimeError::LlmResponseInvalid {
                        message: format!("json_value patch for {} is missing `pointer`", self.path),
                    });
                }
                if self.value.is_none() {
                    return Err(RuntimeError::LlmResponseInvalid {
                        message: format!("json_value patch for {} is missing `value`", self.path),
                    });
                }
            }
            FilePatchKind::TomlValue => {
                if self.dotted_key.as_deref().unwrap_or_default().is_empty() {
                    return Err(RuntimeError::LlmResponseInvalid {
                        message: format!(
                            "toml_value patch for {} is missing `dotted_key`",
                            self.path
                        ),
                    });
                }
                if self.value.is_none() {
                    return Err(RuntimeError::LlmResponseInvalid {
                        message: format!("toml_value patch for {} is missing `value`", self.path),
                    });
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoUpdatePlan {
    pub issue_id: String,
    pub patch_id: String,
    pub manual_approved: bool,
    pub diff_lines: usize,
    pub patches: Vec<FilePatch>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AutoUpdateResult {
    pub report: IterationReport,
    pub changed_paths: Vec<String>,
    pub rolled_back: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationEnvelope {
    pub input: VerificationInput,
    pub verification_profile: Option<String>,
}

impl From<VerificationInput> for VerificationEnvelope {
    fn from(input: VerificationInput) -> Self {
        Self {
            input,
            verification_profile: None,
        }
    }
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
        F: FnOnce(&Path) -> Result<VerificationEnvelope, RuntimeError>,
    {
        let mut backups = Vec::new();
        let mut changed_paths = BTreeSet::new();
        let patch_kind = summarize_patch_kinds(&plan.patches);

        for patch in &plan.patches {
            patch.validate_shape()?;
            let abs_path = self.resolve_patch_path(&patch.path)?;
            let original = fs::read_to_string(&abs_path).map_err(|e| RuntimeError::Io {
                path: abs_path.clone(),
                message: e.to_string(),
            })?;

            let updated = match patch.kind {
                FilePatchKind::Text => apply_text_patch(patch, &abs_path, &original)?,
                FilePatchKind::JsonValue => apply_json_patch(patch, &abs_path, &original)?,
                FilePatchKind::TomlValue => apply_toml_patch(patch, &abs_path, &original)?,
            };
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
            patch_kind,
            changed_paths: changed_paths.iter().cloned().collect(),
            diff_lines: plan.diff_lines,
            manual_approved: plan.manual_approved,
            verification_profile: verification.verification_profile,
            verification: verification.input,
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
        if rel_path
            .components()
            .any(|c| matches!(c, Component::ParentDir))
        {
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

fn summarize_patch_kinds(patches: &[FilePatch]) -> String {
    let mut kinds = patches
        .iter()
        .map(|patch| patch.patch_kind_name())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if kinds.is_empty() {
        return "none".to_string();
    }
    if kinds.len() == 1 {
        return kinds.remove(0).to_string();
    }
    "mixed".to_string()
}

fn apply_text_patch(
    patch: &FilePatch,
    abs_path: &Path,
    original: &str,
) -> Result<String, RuntimeError> {
    if !original.contains(&patch.find) {
        return Err(RuntimeError::AutoUpdatePatternNotFound {
            path: abs_path.to_path_buf(),
            pattern: patch.find.clone(),
        });
    }

    Ok(original.replacen(&patch.find, &patch.replace, 1))
}

fn apply_json_patch(
    patch: &FilePatch,
    abs_path: &Path,
    original: &str,
) -> Result<String, RuntimeError> {
    let pointer = patch.pointer.as_deref().unwrap_or_default();
    let replacement = patch
        .value
        .clone()
        .ok_or_else(|| RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: "json_value patch missing replacement value".to_string(),
        })?;
    let mut document: Value =
        serde_json::from_str(original).map_err(|err| RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: format!("json parse failed: {err}"),
        })?;

    let Some(target) = document.pointer_mut(pointer) else {
        return Err(RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: format!("json pointer not found: {pointer}"),
        });
    };
    *target = replacement;

    serde_json::to_string_pretty(&document).map_err(|err| RuntimeError::Io {
        path: abs_path.to_path_buf(),
        message: format!("json serialize failed: {err}"),
    })
}

fn apply_toml_patch(
    patch: &FilePatch,
    abs_path: &Path,
    original: &str,
) -> Result<String, RuntimeError> {
    let dotted_key = patch.dotted_key.as_deref().unwrap_or_default();
    let replacement = patch
        .value
        .clone()
        .ok_or_else(|| RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: "toml_value patch missing replacement value".to_string(),
        })?;
    let replacement =
        TomlValue::try_from(replacement).map_err(|err| RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: format!("toml value conversion failed: {err}"),
        })?;
    let mut document: TomlValue =
        toml::from_str(original).map_err(|err| RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: format!("toml parse failed: {err}"),
        })?;

    let mut cursor = &mut document;
    let mut segments = dotted_key.split('.').peekable();
    while let Some(segment) = segments.next() {
        let Some(table) = cursor.as_table_mut() else {
            return Err(RuntimeError::AutoUpdatePatchInvalid {
                path: patch.path.clone(),
                reason: format!("toml key path is not a table at {segment}"),
            });
        };
        if segments.peek().is_none() {
            let Some(target) = table.get_mut(segment) else {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: patch.path.clone(),
                    reason: format!("toml dotted key not found: {dotted_key}"),
                });
            };
            *target = replacement;
            return toml::to_string_pretty(&document).map_err(|err| RuntimeError::Io {
                path: abs_path.to_path_buf(),
                message: format!("toml serialize failed: {err}"),
            });
        }

        cursor = table
            .get_mut(segment)
            .ok_or_else(|| RuntimeError::AutoUpdatePatchInvalid {
                path: patch.path.clone(),
                reason: format!("toml dotted key not found: {dotted_key}"),
            })?;
    }

    Err(RuntimeError::AutoUpdatePatchInvalid {
        path: patch.path.clone(),
        reason: "toml dotted key must not be empty".to_string(),
    })
}
