//! Minimal automatic update runner.
//! It applies text patches on workspace files, runs verification, and rolls back on failure.

use crate::core::error::RuntimeError;
use crate::kernel::evaluator::VerificationInput;
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
    pub changed_paths: Vec<String>,
    pub rolled_back: bool,
    pub tests_passed: bool,
    pub safety_checks_passed: bool,
    pub quality_score: u32,
    pub verdict: &'static str,
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
    /// - rollback on verification failure or if tests/safety fail
    pub fn execute<F>(
        &self,
        plan: AutoUpdatePlan,
        verify: F,
    ) -> Result<AutoUpdateResult, RuntimeError>
    where
        F: FnOnce(&Path) -> Result<VerificationEnvelope, RuntimeError>,
    {
        let mut backups = Vec::new();
        let mut changed_paths = BTreeSet::new();

        // P1-18: helper closure that runs one patch. On any per-patch error
        // we roll back what has been applied so far before propagating. The
        // previous implementation used `?` in this loop, leaving files
        // modified with no rollback trigger and no way for the caller to
        // restore prior state.
        let apply_one = |patch: &crate::kernel::auto_update::FilePatch,
                         backups: &mut Vec<AppliedBackup>,
                         changed_paths: &mut BTreeSet<String>|
         -> Result<(), RuntimeError> {
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
            // Record the backup BEFORE mutating disk so a failed write
            // is still recoverable (mirrors the P0-5 rollback pattern).
            backups.push(AppliedBackup {
                abs_path: abs_path.clone(),
                original,
            });
            fs::write(&abs_path, updated).map_err(|e| RuntimeError::Io {
                path: abs_path.clone(),
                message: e.to_string(),
            })?;
            changed_paths.insert(patch.path.clone());
            Ok(())
        };

        for patch in &plan.patches {
            if let Err(err) = apply_one(patch, &mut backups, &mut changed_paths) {
                if let Err(rollback_err) = self.rollback(&backups) {
                    return Err(RuntimeError::Invariant {
                        message: format!(
                            "{err}; additionally, patch rollback failed: {rollback_err}"
                        ),
                    });
                }
                return Err(err);
            }
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

        let tests_passed = verification.input.tests_passed;
        let safety_checks_passed = verification.input.safety_checks_passed;
        let quality_score = verification.input.quality_score;
        let accepted = tests_passed && safety_checks_passed;

        let rolled_back = !accepted;
        if rolled_back {
            self.rollback(&backups)?;
        }

        Ok(AutoUpdateResult {
            changed_paths: changed_paths.into_iter().collect(),
            rolled_back,
            tests_passed,
            safety_checks_passed,
            quality_score,
            verdict: if accepted { "promote" } else { "rollback" },
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

fn apply_text_patch(
    patch: &FilePatch,
    abs_path: &Path,
    original: &str,
) -> Result<String, RuntimeError> {
    // P2-27: require `find` to occur exactly once. `replacen(..., 1)`
    // used to silently mis-target when the pattern appeared >1 time, so
    // an ambiguous find string produced by an LLM would edit the first
    // instance — often the wrong one. Force the caller to supply enough
    // context to disambiguate.
    let occurrences = original.matches(patch.find.as_str()).count();
    if occurrences == 0 {
        return Err(RuntimeError::AutoUpdatePatternNotFound {
            path: abs_path.to_path_buf(),
            pattern: patch.find.clone(),
        });
    }
    if occurrences > 1 {
        return Err(RuntimeError::AutoUpdatePatchInvalid {
            path: abs_path.display().to_string(),
            reason: format!(
                "text patch `find` appears {occurrences} times; supply more surrounding context so the target is unique"
            ),
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

    serde_json::to_string_pretty(&document).map_err(|err| json_serialize_io_error(abs_path, &err))
}

/// Map a JSON re-serialization failure to `RuntimeError::Io`. Serializing a
/// `Value` that was just parsed never fails in practice, so this arm is
/// unreachable at runtime; extracting it keeps the error text byte-stable and
/// unit-testable.
fn json_serialize_io_error(abs_path: &Path, err: &serde_json::Error) -> RuntimeError {
    RuntimeError::Io {
        path: abs_path.to_path_buf(),
        message: format!("json serialize failed: {err}"),
    }
}

/// Map a TOML re-serialization failure to `RuntimeError::Io`. As with the JSON
/// counterpart this is unreachable for a document that just parsed; extracted
/// for byte-stable text and direct testing.
fn toml_serialize_io_error(abs_path: &Path, err: &toml::ser::Error) -> RuntimeError {
    RuntimeError::Io {
        path: abs_path.to_path_buf(),
        message: format!("toml serialize failed: {err}"),
    }
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
    let document: TomlValue =
        toml::from_str(original).map_err(|err| RuntimeError::AutoUpdatePatchInvalid {
            path: patch.path.clone(),
            reason: format!("toml parse failed: {err}"),
        })?;

    replace_toml_dotted_value(
        document,
        dotted_key.split('.').peekable(),
        &patch.path,
        dotted_key,
        replacement,
        abs_path,
    )
}

/// Navigate `document` along the `.`-separated `segments` and overwrite the
/// terminal value with `replacement`, returning the re-serialized document.
///
/// Extracted from [`apply_toml_patch`] so the empty-segments terminal `Err`
/// (last statement) is directly reachable in a unit test: `str::split('.')`
/// always yields at least one item, so the `while` loop never falls through
/// when called from `apply_toml_patch` — feeding an empty iterator is the only
/// way to exercise the fail-closed "dotted key must not be empty" arm.
fn replace_toml_dotted_value<'a>(
    mut document: TomlValue,
    mut segments: std::iter::Peekable<impl Iterator<Item = &'a str>>,
    patch_path: &str,
    dotted_key: &str,
    replacement: TomlValue,
    abs_path: &Path,
) -> Result<String, RuntimeError> {
    let mut cursor = &mut document;
    while let Some(segment) = segments.next() {
        let Some(table) = cursor.as_table_mut() else {
            return Err(RuntimeError::AutoUpdatePatchInvalid {
                path: patch_path.to_string(),
                reason: format!("toml key path is not a table at {segment}"),
            });
        };
        if segments.peek().is_none() {
            let Some(target) = table.get_mut(segment) else {
                return Err(RuntimeError::AutoUpdatePatchInvalid {
                    path: patch_path.to_string(),
                    reason: format!("toml dotted key not found: {dotted_key}"),
                });
            };
            *target = replacement;
            return toml::to_string_pretty(&document)
                .map_err(|err| toml_serialize_io_error(abs_path, &err));
        }

        cursor = table
            .get_mut(segment)
            .ok_or_else(|| RuntimeError::AutoUpdatePatchInvalid {
                path: patch_path.to_string(),
                reason: format!("toml dotted key not found: {dotted_key}"),
            })?;
    }

    Err(RuntimeError::AutoUpdatePatchInvalid {
        path: patch_path.to_string(),
        reason: "toml dotted key must not be empty".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::evaluator::VerificationInput;
    use std::fs;
    use tempfile::TempDir;

    fn plan(patches: Vec<FilePatch>) -> AutoUpdatePlan {
        AutoUpdatePlan {
            issue_id: "issue".to_string(),
            patch_id: "patch".to_string(),
            manual_approved: false,
            diff_lines: 0,
            patches,
        }
    }

    fn verify_ok(_: &Path) -> Result<VerificationEnvelope, RuntimeError> {
        Ok(VerificationEnvelope::from(VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 100,
        }))
    }

    /// P2-27: `find` must appear exactly once. Zero → not-found; >1 →
    /// ambiguous, refuse to touch the file.
    #[test]
    fn text_patch_rejects_when_find_appears_zero_times() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("f.txt"), "unrelated").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![FilePatch::text("f.txt", "nope", "yes")]),
                verify_ok,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::AutoUpdatePatternNotFound { .. }
        ));
    }

    #[test]
    fn text_patch_rejects_when_find_is_ambiguous() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("f.txt"), "hello hello").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![FilePatch::text("f.txt", "hello", "world")]),
                verify_ok,
            )
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("appears") && msg.contains("2"),
            "unexpected: {msg}"
        );
        // File must be unchanged.
        assert_eq!(
            fs::read_to_string(ws.path().join("f.txt")).unwrap(),
            "hello hello"
        );
    }

    #[test]
    fn text_patch_applies_when_unique_and_verify_ok() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("a.txt"), "foo bar").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let result = updater
            .execute(
                plan(vec![FilePatch::text("a.txt", "foo", "FOO")]),
                verify_ok,
            )
            .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(
            fs::read_to_string(ws.path().join("a.txt")).unwrap(),
            "FOO bar"
        );
    }

    /// P1-18: if patch N fails, patches 1..N-1 must be rolled back —
    /// the earlier ones are otherwise silently left on disk.
    #[test]
    fn per_patch_failure_rolls_back_preceding_writes() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("a.txt"), "before").unwrap();
        fs::write(ws.path().join("b.txt"), "no-match-here").unwrap();
        let updater = AutoUpdater::new(ws.path());
        // First patch succeeds (writes "after"), second patch fails
        // (pattern not found). Expected: after error, a.txt back to
        // "before".
        let err = updater
            .execute(
                plan(vec![
                    FilePatch::text("a.txt", "before", "after"),
                    FilePatch::text("b.txt", "nope", "yes"),
                ]),
                verify_ok,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            RuntimeError::AutoUpdatePatternNotFound { .. }
        ));
        assert_eq!(
            fs::read_to_string(ws.path().join("a.txt")).unwrap(),
            "before",
            "first patch must be rolled back on later failure"
        );
    }

    #[test]
    fn verify_failure_rolls_back_applied_patches() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("a.txt"), "x").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let verify_fail = |_: &Path| -> Result<VerificationEnvelope, RuntimeError> {
            Err(RuntimeError::Invariant {
                message: "verify said no".to_string(),
            })
        };
        let err = updater
            .execute(plan(vec![FilePatch::text("a.txt", "x", "y")]), verify_fail)
            .unwrap_err();
        assert!(matches!(err, RuntimeError::AutoUpdateVerifyFailed { .. }));
        assert_eq!(fs::read_to_string(ws.path().join("a.txt")).unwrap(), "x");
    }

    #[test]
    fn default_file_patch_kind_is_text() {
        assert_eq!(super::default_file_patch_kind(), FilePatchKind::Text);
    }

    #[test]
    fn file_patch_constructors_and_kind_names() {
        let text = FilePatch::text("a", "f", "r");
        assert_eq!(text.kind, FilePatchKind::Text);
        assert_eq!(text.patch_kind_name(), "text");
        assert_eq!(text.find, "f");
        assert_eq!(text.replace, "r");

        let jv = FilePatch::json_value("a.json", "/x/y", serde_json::json!(5));
        assert_eq!(jv.kind, FilePatchKind::JsonValue);
        assert_eq!(jv.patch_kind_name(), "json_value");
        assert_eq!(jv.pointer.as_deref(), Some("/x/y"));
        assert_eq!(jv.value, Some(serde_json::json!(5)));

        let tv = FilePatch::toml_value("a.toml", "pkg.name", serde_json::json!("demo"));
        assert_eq!(tv.kind, FilePatchKind::TomlValue);
        assert_eq!(tv.patch_kind_name(), "toml_value");
        assert_eq!(tv.dotted_key.as_deref(), Some("pkg.name"));
    }

    #[test]
    fn diff_line_estimate_counts_lines_for_text_and_one_for_structured() {
        // Text: max(find lines, replace lines), min 1.
        let single = FilePatch::text("a", "one", "two");
        assert_eq!(single.diff_line_estimate(), 1);
        let multi = FilePatch::text("a", "l1\nl2", "r1\nr2\nr3");
        assert_eq!(multi.diff_line_estimate(), 3);
        // Structured kinds are always 1.
        assert_eq!(
            FilePatch::json_value("a", "/p", serde_json::json!(1)).diff_line_estimate(),
            1
        );
        assert_eq!(
            FilePatch::toml_value("a", "k", serde_json::json!(1)).diff_line_estimate(),
            1
        );
    }

    #[test]
    fn validate_shape_text_requires_find() {
        let mut p = FilePatch::text("a", "", "r");
        let err = p.validate_shape().unwrap_err();
        assert!(matches!(err, RuntimeError::LlmResponseInvalid { .. }));
        p.find = "x".to_string();
        assert!(p.validate_shape().is_ok());
    }

    #[test]
    fn validate_shape_json_requires_pointer_and_value() {
        // Missing pointer.
        let mut p = FilePatch::json_value("a", "", serde_json::json!(1));
        assert!(matches!(
            p.validate_shape().unwrap_err(),
            RuntimeError::LlmResponseInvalid { .. }
        ));
        // Pointer present but value None.
        p.pointer = Some("/x".to_string());
        p.value = None;
        assert!(matches!(
            p.validate_shape().unwrap_err(),
            RuntimeError::LlmResponseInvalid { .. }
        ));
        p.value = Some(serde_json::json!(1));
        assert!(p.validate_shape().is_ok());
    }

    #[test]
    fn validate_shape_toml_requires_dotted_key_and_value() {
        let mut p = FilePatch::toml_value("a", "", serde_json::json!(1));
        assert!(matches!(
            p.validate_shape().unwrap_err(),
            RuntimeError::LlmResponseInvalid { .. }
        ));
        p.dotted_key = Some("k".to_string());
        p.value = None;
        assert!(matches!(
            p.validate_shape().unwrap_err(),
            RuntimeError::LlmResponseInvalid { .. }
        ));
        p.value = Some(serde_json::json!(1));
        assert!(p.validate_shape().is_ok());
    }

    #[test]
    fn verification_envelope_from_input_has_no_profile() {
        let env = VerificationEnvelope::from(VerificationInput {
            tests_passed: true,
            safety_checks_passed: false,
            quality_score: 10,
        });
        assert_eq!(env.verification_profile, None);
        assert!(env.input.tests_passed);
        assert!(!env.input.safety_checks_passed);
    }

    #[test]
    fn resolve_patch_path_rejects_absolute() {
        let ws = TempDir::new().unwrap();
        let updater = AutoUpdater::new(ws.path());
        let abs = if cfg!(windows) { "C:/x" } else { "/etc/passwd" };
        let err = updater.resolve_patch_path(abs).unwrap_err();
        assert!(matches!(err, RuntimeError::AutoUpdateInvalidPath { .. }));
    }

    #[test]
    fn resolve_patch_path_rejects_parent_traversal() {
        let ws = TempDir::new().unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater.resolve_patch_path("../escape.txt").unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdateInvalidPath { reason, .. } if reason.contains("parent directory")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn resolve_patch_path_accepts_relative() {
        let ws = TempDir::new().unwrap();
        let updater = AutoUpdater::new(ws.path());
        let resolved = updater.resolve_patch_path("sub/dir/file.txt").unwrap();
        assert_eq!(resolved, ws.path().join("sub/dir/file.txt"));
    }

    #[test]
    fn json_value_patch_applies_and_promotes() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("c.json"), r#"{"a":{"b":1}}"#).unwrap();
        let updater = AutoUpdater::new(ws.path());
        let result = updater
            .execute(
                plan(vec![FilePatch::json_value(
                    "c.json",
                    "/a/b",
                    serde_json::json!(42),
                )]),
                verify_ok,
            )
            .unwrap();
        assert!(!result.rolled_back);
        assert_eq!(result.verdict, "promote");
        let doc: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(ws.path().join("c.json")).unwrap()).unwrap();
        assert_eq!(doc.pointer("/a/b"), Some(&serde_json::json!(42)));
    }

    #[test]
    fn json_value_patch_bad_pointer_rolls_back() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("c.json"), r#"{"a":1}"#).unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![FilePatch::json_value(
                    "c.json",
                    "/missing",
                    serde_json::json!(1),
                )]),
                verify_ok,
            )
            .unwrap_err();
        assert!(matches!(err, RuntimeError::AutoUpdatePatchInvalid { .. }));
        // Unchanged after rollback.
        assert_eq!(
            fs::read_to_string(ws.path().join("c.json")).unwrap(),
            r#"{"a":1}"#
        );
    }

    #[test]
    fn json_value_patch_invalid_json_document_errors() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("c.json"), "not json").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![FilePatch::json_value(
                    "c.json",
                    "/a",
                    serde_json::json!(1),
                )]),
                verify_ok,
            )
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("json parse failed")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn toml_value_patch_applies_nested_key() {
        let ws = TempDir::new().unwrap();
        fs::write(
            ws.path().join("Cargo.toml"),
            "[package]\nname = \"old\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let updater = AutoUpdater::new(ws.path());
        let result = updater
            .execute(
                plan(vec![FilePatch::toml_value(
                    "Cargo.toml",
                    "package.name",
                    serde_json::json!("new"),
                )]),
                verify_ok,
            )
            .unwrap();
        assert!(!result.rolled_back);
        let doc: toml::Value =
            toml::from_str(&fs::read_to_string(ws.path().join("Cargo.toml")).unwrap()).unwrap();
        assert_eq!(
            doc.get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str()),
            Some("new")
        );
    }

    #[test]
    fn toml_value_patch_missing_key_rolls_back() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("t.toml"), "[package]\nname = \"x\"\n").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![FilePatch::toml_value(
                    "t.toml",
                    "package.missing",
                    serde_json::json!("v"),
                )]),
                verify_ok,
            )
            .unwrap_err();
        assert!(matches!(err, RuntimeError::AutoUpdatePatchInvalid { .. }));
    }

    #[test]
    fn toml_value_patch_non_table_segment_errors() {
        let ws = TempDir::new().unwrap();
        // `name` is a string, so descending into `name.deeper` hits the
        // "not a table" branch.
        fs::write(ws.path().join("t.toml"), "[package]\nname = \"x\"\n").unwrap();
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![FilePatch::toml_value(
                    "t.toml",
                    "package.name.deeper",
                    serde_json::json!("v"),
                )]),
                verify_ok,
            )
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("not a table")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn quality_failure_verdict_is_rollback() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("a.txt"), "keep").unwrap();
        let updater = AutoUpdater::new(ws.path());
        // verify succeeds structurally but reports tests_passed=false, so
        // the result must be rolled back with verdict "rollback".
        let verify_reject = |_: &Path| {
            Ok(VerificationEnvelope::from(VerificationInput {
                tests_passed: false,
                safety_checks_passed: true,
                quality_score: 0,
            }))
        };
        let result = updater
            .execute(
                plan(vec![FilePatch::text("a.txt", "keep", "changed")]),
                verify_reject,
            )
            .unwrap();
        assert!(result.rolled_back);
        assert_eq!(result.verdict, "rollback");
        assert!(!result.tests_passed);
        // File restored.
        assert_eq!(fs::read_to_string(ws.path().join("a.txt")).unwrap(), "keep");
    }

    #[test]
    fn missing_target_file_rolls_back_prior_patches() {
        let ws = TempDir::new().unwrap();
        fs::write(ws.path().join("a.txt"), "one").unwrap();
        // missing.txt does not exist → read_to_string fails inside apply_one,
        // which triggers rollback of the a.txt write.
        let updater = AutoUpdater::new(ws.path());
        let err = updater
            .execute(
                plan(vec![
                    FilePatch::text("a.txt", "one", "ONE"),
                    FilePatch::text("missing.txt", "x", "y"),
                ]),
                verify_ok,
            )
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }));
        assert_eq!(fs::read_to_string(ws.path().join("a.txt")).unwrap(), "one");
    }

    // ---------- direct helper error-branch coverage ----------

    #[test]
    fn apply_json_patch_missing_value_errors() {
        // value = None reaches the ok_or_else "missing replacement value".
        let patch = FilePatch {
            path: "c.json".to_string(),
            kind: FilePatchKind::JsonValue,
            find: String::new(),
            replace: String::new(),
            pointer: Some("/k".to_string()),
            dotted_key: None,
            value: None,
        };
        let err = apply_json_patch(&patch, Path::new("/tmp/c.json"), r#"{"k":1}"#).unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("missing replacement value")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn apply_toml_patch_missing_value_errors() {
        let patch = FilePatch {
            path: "c.toml".to_string(),
            kind: FilePatchKind::TomlValue,
            find: String::new(),
            replace: String::new(),
            pointer: None,
            dotted_key: Some("k".to_string()),
            value: None,
        };
        let err = apply_toml_patch(&patch, Path::new("/tmp/c.toml"), "k = 1\n").unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("missing replacement value")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn apply_toml_patch_value_conversion_fails() {
        // serde_json null has no TOML representation → try_from errors.
        let patch = FilePatch::toml_value("c.toml", "k", serde_json::Value::Null);
        let err = apply_toml_patch(&patch, Path::new("/tmp/c.toml"), "k = 1\n").unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("toml value conversion failed")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn apply_toml_patch_parse_failure() {
        let patch = FilePatch::toml_value("c.toml", "k", serde_json::json!(2));
        // Not valid TOML.
        let err = apply_toml_patch(&patch, Path::new("/tmp/c.toml"), "this is : not = toml [[[")
            .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("toml parse failed")),
            "unexpected: {err:?}"
        );
    }

    /// The JSON serialize failure arm in `apply_json_patch` is unreachable
    /// (a just-parsed `Value` always re-serializes), so exercise the extracted
    /// mapper directly to lock its `Io` variant and byte-exact message.
    #[test]
    fn json_serialize_io_error_wraps_path_and_message() {
        let serde_err =
            serde_json::from_str::<serde_json::Value>("{").expect_err("malformed json errors");
        let err = json_serialize_io_error(Path::new("/tmp/x.json"), &serde_err);
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x.json") && message.starts_with("json serialize failed: ")),
            "unexpected: {err:?}"
        );
    }

    /// Same for the TOML serialize failure arm in `apply_toml_patch`. A real
    /// `toml::ser::Error` arises from a map whose keys are not strings, which
    /// TOML cannot represent.
    #[test]
    fn toml_serialize_io_error_wraps_path_and_message() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(vec![1u8, 2u8], 3u8);
        let ser_err = toml::to_string_pretty(&map).expect_err("non-string key errors");
        let err = toml_serialize_io_error(Path::new("/tmp/x.toml"), &ser_err);
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x.toml") && message.starts_with("toml serialize failed: ")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn apply_toml_patch_intermediate_key_missing() {
        // dotted_key "a.b": first segment "a" is absent at a non-final position,
        // hitting the intermediate get_mut None branch.
        let patch = FilePatch::toml_value("c.toml", "a.b", serde_json::json!(2));
        let err =
            apply_toml_patch(&patch, Path::new("/tmp/c.toml"), "[other]\nx = 1\n").unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason.contains("dotted key not found")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn rollback_write_failure_surfaces_io() {
        // A backup whose abs_path is an existing directory makes fs::write fail
        // with an Io error, covering the rollback write-error closure.
        let ws = TempDir::new().unwrap();
        let dir_as_target = ws.path().join("iamdir");
        fs::create_dir(&dir_as_target).unwrap();
        let updater = AutoUpdater::new(ws.path());
        let backups = vec![AppliedBackup {
            abs_path: dir_as_target,
            original: "restore".to_string(),
        }];
        let err = updater.rollback(&backups).unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }));
    }

    // The terminal "dotted key must not be empty" arm of
    // `replace_toml_dotted_value` is unreachable through `apply_toml_patch`
    // (`str::split('.')` always yields ≥1 segment), so drive it directly with
    // an empty segment iterator. The document/replacement are irrelevant since
    // the loop body never runs.
    #[test]
    fn replace_toml_dotted_value_empty_segments_is_patch_invalid() {
        let doc: TomlValue = toml::from_str("k = 1").unwrap();
        let empty: std::iter::Peekable<std::vec::IntoIter<&str>> =
            Vec::<&str>::new().into_iter().peekable();
        let err = replace_toml_dotted_value(
            doc,
            empty,
            "some/Cargo.toml",
            "",
            TomlValue::Integer(9),
            Path::new("/abs/Cargo.toml"),
        )
        .unwrap_err();
        assert!(
            matches!(&err, RuntimeError::AutoUpdatePatchInvalid { reason, .. } if reason == "toml dotted key must not be empty"),
            "unexpected: {err:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn text_patch_write_failure_then_rollback_failure_surfaces_invariant() {
        use std::os::unix::fs::PermissionsExt;
        // Running as root bypasses file-mode permission checks: a 0o444 file is
        // still writable, so the failure this test drives can't occur.
        // Single-line guard: no standalone closing brace to leave uncovered.
        // Root bypasses file-mode permission checks, so the failure this test
        // asserts only occurs as a non-root user. Wrapping the body (instead of
        // an early return) keeps every line executable under both euids.
        if unsafe { libc::geteuid() } != 0 {
            // A read-only target: read_to_string succeeds, but the fs::write inside
            // apply_one fails (covers the write error map). Rollback then tries to
            // write the original back to the same read-only file and *also* fails,
            // covering both the rollback write-error map and the
            // "additionally, patch rollback failed" Invariant path.
            let ws = TempDir::new().unwrap();
            let target = ws.path().join("ro.txt");
            fs::write(&target, "foo bar").unwrap();
            let mut perms = fs::metadata(&target).unwrap().permissions();
            perms.set_mode(0o444);
            fs::set_permissions(&target, perms).unwrap();

            let updater = AutoUpdater::new(ws.path());
            let err = updater
                .execute(
                    plan(vec![FilePatch::text("ro.txt", "foo", "FOO")]),
                    verify_ok,
                )
                .unwrap_err();
            assert!(
                matches!(&err, RuntimeError::Invariant { message } if message.contains("rollback failed")),
                "expected Invariant, got: {err:?}"
            );

            // Restore perms for TempDir cleanup.
            let mut perms = fs::metadata(&target).unwrap().permissions();
            perms.set_mode(0o644);
            fs::set_permissions(&target, perms).unwrap();
        }
    }
}
