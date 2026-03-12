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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommandCheckResult {
    pub command: String,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VerificationReport {
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
    pub fn verify(
        workspace_root: &Path,
        tests_command: Option<&str>,
        safety_command: Option<&str>,
        quality_score_override: Option<u32>,
    ) -> Result<VerificationReport, RuntimeError> {
        let tests = match tests_command {
            Some(command) => Some(run_check_command(command, workspace_root)?),
            None => None,
        };
        let safety = match safety_command {
            Some(command) => Some(run_check_command(command, workspace_root)?),
            None => None,
        };

        let tests_passed = tests.as_ref().map_or(true, |result| result.success);
        let safety_checks_passed = safety.as_ref().map_or(true, |result| result.success);
        let quality_score = quality_score_override.unwrap_or_else(|| {
            if tests_passed && safety_checks_passed {
                DEFAULT_QUALITY_SCORE
            } else {
                0
            }
        });

        Ok(VerificationReport {
            input: VerificationInput {
                tests_passed,
                safety_checks_passed,
                quality_score,
            },
            tests,
            safety,
        })
    }
}

fn default_plugin_payload_json() -> Value {
    Value::Object(Map::new())
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
    let payload = serde_json::to_string(&spec.payload_json).map_err(|err| RuntimeError::InvalidArgument {
        message: format!("plugin payload_json was not serializable: {err}"),
    })?;

    let invoker = match PluginInvoker::load(&fixtures_root) {
        Ok(invoker) => invoker,
        Err(err) => {
            return Ok(CommandCheckResult {
                command: original_command.to_string(),
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
    use super::CommandVerifier;
    use serde_json::json;
    use std::path::Path;

    #[test]
    fn verify_defaults_to_success_without_commands() {
        let report = CommandVerifier::verify(std::path::Path::new("."), None, None, None)
            .expect("verify should succeed");
        assert!(report.input.tests_passed);
        assert!(report.input.safety_checks_passed);
        assert_eq!(report.input.quality_score, 90);
    }

    #[test]
    fn verify_marks_failed_command() {
        let report = CommandVerifier::verify(
            std::path::Path::new("."),
            Some("cargo --badflag"),
            None,
            None,
        )
        .expect("verify should return report");
        assert!(!report.input.tests_passed);
        assert_eq!(report.input.quality_score, 0);
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
        let report = CommandVerifier::verify(Path::new(env!("CARGO_MANIFEST_DIR")), Some(&spec), None, None)
            .expect("plugin verification should succeed");
        assert!(report.input.tests_passed, "report: {report:?}");
        let tests = report.tests.expect("tests report should exist");
        assert!(tests.success, "tests: {tests:?}");
        assert!(tests.stdout.contains("\"value\":7.0"), "stdout: {}", tests.stdout);
    }
}
