//! Local verification helpers for guarded auto-update workflows.

use crate::core::error::RuntimeError;
use crate::kernel::evaluator::VerificationInput;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::process::Command;

const DEFAULT_QUALITY_SCORE: u32 = 90;

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

pub struct CommandVerifier;

impl CommandVerifier {
    pub fn verify(
        workspace_root: &Path,
        tests_command: Option<&str>,
        safety_command: Option<&str>,
        quality_score_override: Option<u32>,
    ) -> Result<VerificationReport, RuntimeError> {
        let tests = match tests_command {
            Some(command) => Some(run_shell_command(command, workspace_root)?),
            None => None,
        };
        let safety = match safety_command {
            Some(command) => Some(run_shell_command(command, workspace_root)?),
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
}
