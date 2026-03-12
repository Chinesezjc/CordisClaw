//! Local verification helpers for guarded auto-update workflows.

use crate::core::error::RuntimeError;
use crate::kernel::evaluator::VerificationInput;
use crate::plugin::invoke::PluginInvoker;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

const DEFAULT_QUALITY_SCORE: u32 = 90;
const PLUGIN_COMMAND_PREFIX: &str = "plugin:";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum VerificationProfile {
    #[default]
    Default,
    RustWorkspace,
}

impl VerificationProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            VerificationProfile::Default => "default",
            VerificationProfile::RustWorkspace => "rust_workspace",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRunner {
    Shell,
    Plugin,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStageKind {
    StaticCheck,
    Tests,
    Safety,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStageStatus {
    Passed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationPlan {
    pub profile: VerificationProfile,
    pub static_check_command: Option<String>,
    pub tests_command: Option<String>,
    pub safety_command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandCheckResult {
    pub command: String,
    pub runner: VerificationRunner,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationStageResult {
    pub kind: VerificationStageKind,
    pub status: VerificationStageStatus,
    pub required: bool,
    pub check: Option<CommandCheckResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationReport {
    pub plan: VerificationPlan,
    pub stages: Vec<VerificationStageResult>,
    pub input: VerificationInput,
    pub tests: Option<CommandCheckResult>,
    pub safety: Option<CommandCheckResult>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
struct PluginCommandSpec {
    #[serde(default)]
    fixtures_root: Option<String>,
    plugin_path: String,
    node_id: String,
    #[serde(default = "default_plugin_payload_json")]
    payload_json: Value,
    #[serde(default)]
    expect_substring: Option<String>,
}

pub struct CommandVerifier;

impl CommandVerifier {
    pub fn resolve_plan(
        workspace_root: &Path,
        profile: VerificationProfile,
        tests_command: Option<&str>,
        safety_command: Option<&str>,
    ) -> VerificationPlan {
        let static_check_command = match profile {
            VerificationProfile::Default => None,
            VerificationProfile::RustWorkspace => {
                discover_rust_workspace_manifest(workspace_root).map(|manifest| {
                    let relative = manifest
                        .strip_prefix(workspace_root)
                        .unwrap_or(&manifest)
                        .display()
                        .to_string();
                    format!("cargo check --quiet --manifest-path {relative}")
                })
            }
        };

        VerificationPlan {
            profile,
            static_check_command,
            tests_command: normalize_optional_command(tests_command),
            safety_command: normalize_optional_command(safety_command),
        }
    }

    pub fn verify(
        workspace_root: &Path,
        profile: VerificationProfile,
        tests_command: Option<&str>,
        safety_command: Option<&str>,
        quality_score_override: Option<u32>,
    ) -> Result<VerificationReport, RuntimeError> {
        let plan = Self::resolve_plan(workspace_root, profile, tests_command, safety_command);
        let mut stages = Vec::new();

        let static_check = run_optional_stage(
            VerificationStageKind::StaticCheck,
            true,
            plan.static_check_command.as_deref(),
            workspace_root,
        )?;
        let tests = run_optional_stage(
            VerificationStageKind::Tests,
            true,
            plan.tests_command.as_deref(),
            workspace_root,
        )?;
        let safety = run_optional_stage(
            VerificationStageKind::Safety,
            true,
            plan.safety_command.as_deref(),
            workspace_root,
        )?;

        stages.push(static_check.stage);
        stages.push(tests.stage);
        stages.push(safety.stage);

        let tests_passed = static_check.success && tests.success;
        let safety_checks_passed = safety.success;
        let quality_score = quality_score_override.unwrap_or_else(|| {
            if tests_passed && safety_checks_passed {
                DEFAULT_QUALITY_SCORE
            } else {
                0
            }
        });

        Ok(VerificationReport {
            plan,
            stages,
            input: VerificationInput {
                tests_passed,
                safety_checks_passed,
                quality_score,
            },
            tests: tests.check,
            safety: safety.check,
        })
    }
}

#[derive(Debug)]
struct StageExecution {
    success: bool,
    check: Option<CommandCheckResult>,
    stage: VerificationStageResult,
}

fn default_plugin_payload_json() -> Value {
    Value::Object(Map::new())
}

fn normalize_optional_command(command: Option<&str>) -> Option<String> {
    command.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn discover_rust_workspace_manifest(workspace_root: &Path) -> Option<PathBuf> {
    let direct = workspace_root.join("Cargo.toml");
    if direct.exists() {
        return Some(direct);
    }

    let nested = workspace_root.join("plugins/Cargo.toml");
    if nested.exists() {
        return Some(nested);
    }

    None
}

fn run_optional_stage(
    kind: VerificationStageKind,
    required: bool,
    command: Option<&str>,
    current_dir: &Path,
) -> Result<StageExecution, RuntimeError> {
    let Some(command) = command else {
        return Ok(StageExecution {
            success: true,
            check: None,
            stage: VerificationStageResult {
                kind,
                status: VerificationStageStatus::Skipped,
                required,
                check: None,
            },
        });
    };

    let check = run_check_command(command, current_dir)?;
    let status = if check.success {
        VerificationStageStatus::Passed
    } else {
        VerificationStageStatus::Failed
    };
    Ok(StageExecution {
        success: check.success,
        check: Some(check.clone()),
        stage: VerificationStageResult {
            kind,
            status,
            required,
            check: Some(check),
        },
    })
}

fn run_check_command(command: &str, current_dir: &Path) -> Result<CommandCheckResult, RuntimeError> {
    if let Some(spec_json) = command.strip_prefix(PLUGIN_COMMAND_PREFIX) {
        return run_plugin_command(command, spec_json, current_dir);
    }
    run_shell_command(command, current_dir)
}

fn run_plugin_command(
    original_command: &str,
    spec_json: &str,
    current_dir: &Path,
) -> Result<CommandCheckResult, RuntimeError> {
    let spec: PluginCommandSpec =
        serde_json::from_str(spec_json).map_err(|err| RuntimeError::InvalidArgument {
            message: format!("invalid plugin verifier spec: {err}"),
        })?;
    let fixtures_root = resolve_plugin_fixtures_root(current_dir, spec.fixtures_root.as_deref());
    let payload =
        serde_json::to_string(&spec.payload_json).map_err(|err| RuntimeError::InvalidArgument {
            message: format!("plugin payload_json was not serializable: {err}"),
        })?;

    let invoker = match PluginInvoker::load(&fixtures_root) {
        Ok(invoker) => invoker,
        Err(err) => {
            return Ok(CommandCheckResult {
                command: original_command.to_string(),
                runner: VerificationRunner::Plugin,
                success: false,
                stdout: String::new(),
                stderr: err.to_string(),
            });
        }
    };

    let response = match invoker.invoke(&spec.plugin_path, &spec.node_id, payload) {
        Ok(response) => response,
        Err(err) => {
            return Ok(CommandCheckResult {
                command: original_command.to_string(),
                runner: VerificationRunner::Plugin,
                success: false,
                stdout: String::new(),
                stderr: err.to_string(),
            });
        }
    };

    let mut success = true;
    let mut stderr = String::new();
    if let Some(expected) = &spec.expect_substring {
        if !response.payload.contains(expected) {
            success = false;
            stderr = format!("plugin output missing expected substring: {expected}");
        }
    }

    Ok(CommandCheckResult {
        command: original_command.to_string(),
        runner: VerificationRunner::Plugin,
        success,
        stdout: response.payload,
        stderr,
    })
}

fn resolve_plugin_fixtures_root(current_dir: &Path, requested_root: Option<&str>) -> PathBuf {
    if let Some(root) = requested_root {
        let path = Path::new(root);
        return if path.is_absolute() {
            path.to_path_buf()
        } else {
            current_dir.join(path)
        };
    }

    if current_dir.join("plugins").exists() {
        return current_dir.to_path_buf();
    }

    let nested_fixtures = current_dir.join("fixtures");
    if nested_fixtures.join("plugins").exists() {
        return nested_fixtures;
    }

    current_dir.to_path_buf()
}

fn run_shell_command(command: &str, current_dir: &Path) -> Result<CommandCheckResult, RuntimeError> {
    #[cfg(windows)]
    let output = Command::new("cmd")
        .args(["/C", command])
        .current_dir(current_dir)
        .output();

    #[cfg(not(windows))]
    let output = Command::new("sh")
        .args(["-lc", command])
        .current_dir(current_dir)
        .output();

    let output = output.map_err(|err| RuntimeError::CommandFailed {
        program: shell_program().to_string(),
        args: shell_args(command),
        message: err.to_string(),
    })?;

    Ok(CommandCheckResult {
        command: command.to_string(),
        runner: VerificationRunner::Shell,
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

#[cfg(windows)]
fn shell_program() -> &'static str {
    "cmd"
}

#[cfg(not(windows))]
fn shell_program() -> &'static str {
    "sh"
}

#[cfg(windows)]
fn shell_args(command: &str) -> Vec<String> {
    vec!["/C".to_string(), command.to_string()]
}

#[cfg(not(windows))]
fn shell_args(command: &str) -> Vec<String> {
    vec!["-lc".to_string(), command.to_string()]
}

#[cfg(test)]
mod tests {
    use super::{CommandVerifier, VerificationProfile, VerificationRunner, VerificationStageKind, VerificationStageStatus};
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use tempfile::TempDir;

    #[test]
    fn verify_defaults_to_success_without_commands() {
        let report = CommandVerifier::verify(
            std::path::Path::new("."),
            VerificationProfile::Default,
            None,
            None,
            None,
        )
        .expect("verify should succeed");
        assert!(report.input.tests_passed);
        assert!(report.input.safety_checks_passed);
        assert_eq!(report.input.quality_score, 90);
        assert_eq!(report.stages.len(), 3);
        assert_eq!(report.stages[0].kind, VerificationStageKind::StaticCheck);
        assert_eq!(report.stages[0].status, VerificationStageStatus::Skipped);
    }

    #[test]
    fn verify_marks_failed_command() {
        let report = CommandVerifier::verify(
            std::path::Path::new("."),
            VerificationProfile::Default,
            Some("cargo --badflag"),
            None,
            None,
        )
        .expect("verify should return report");
        assert!(!report.input.tests_passed);
        assert_eq!(report.input.quality_score, 0);
        assert_eq!(
            report.tests.as_ref().map(|check| check.runner),
            Some(VerificationRunner::Shell)
        );
    }

    #[test]
    fn verify_supports_plugin_command_specs() {
        let spec = format!(
            "plugin:{}",
            json!({
                "fixtures_root": "../../fixtures",
                "plugin_path": "expr",
                "node_id": "expr_entry",
                "payload_json": {
                    "expression": "1 + 2 * 3"
                },
                "expect_substring": "\"value\":7.0"
            })
        );
        let report = CommandVerifier::verify(
            Path::new(env!("CARGO_MANIFEST_DIR")),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
        )
        .expect("plugin verification should succeed");
        assert!(report.input.tests_passed, "report: {report:?}");
        assert_eq!(
            report.tests.as_ref().map(|check| check.runner),
            Some(VerificationRunner::Plugin)
        );
    }

    #[test]
    fn verify_rust_workspace_profile_adds_static_check_stage() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write manifest");
        fs::create_dir_all(temp.path().join("src")).expect("src dir");
        fs::write(temp.path().join("src/lib.rs"), "pub fn demo() -> u32 { 1 }\n")
            .expect("write source");

        let report = CommandVerifier::verify(
            temp.path(),
            VerificationProfile::RustWorkspace,
            None,
            None,
            None,
        )
        .expect("rust workspace verification should succeed");
        assert!(report.input.tests_passed, "report: {report:?}");
        assert_eq!(report.plan.profile, VerificationProfile::RustWorkspace);
        assert_eq!(report.stages[0].status, VerificationStageStatus::Passed);
        assert!(
            report.stages[0]
                .check
                .as_ref()
                .expect("static check")
                .command
                .contains("cargo check --quiet --manifest-path Cargo.toml")
        );
    }
}
