//! Local verification helpers for guarded auto-update workflows.

use crate::core::error::RuntimeError;
use crate::kernel::evaluator::VerificationInput;
use crate::plugin::abi::PluginResponse;
use crate::plugin::invoke::PluginInvoker;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_QUALITY_SCORE: u32 = 90;
const PLUGIN_COMMAND_PREFIX: &str = "plugin:";
const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 600;

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
    /// Sha256 of the source tree at verification time. Used by the caller to
    /// detect concurrent mutation between verify and promote (P0-3 TOCTOU).
    pub source_tree_hash: Option<String>,
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

/// Delegate that dispatches a plugin invocation to the *staged candidate*
/// registry rather than the live one. When provided (P0-4), `plugin:`
/// verifier commands go through this closure so the verifier actually
/// exercises the code being promoted.
pub type CandidateInvoker<'a> =
    &'a (dyn Fn(&str, &str, String) -> Result<PluginResponse, RuntimeError> + Send + Sync);

/// Options passed to [`CommandVerifier::verify_with_options`] that control
/// safety-critical behaviour of the verification run.
#[derive(Default)]
pub struct VerifyOptions<'a> {
    /// If set, `plugin:` commands dispatch through this closure — pointing at
    /// the staged candidate snapshot — instead of loading a fresh
    /// [`PluginInvoker`] from the live fixtures directory.
    pub candidate_invoker: Option<CandidateInvoker<'a>>,
    /// Timeout for each shell/plugin command. `None` = default 600s.
    pub command_timeout: Option<Duration>,
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
            VerificationProfile::RustWorkspace => discover_rust_workspace_manifest(workspace_root)
                .map(|manifest| {
                    let relative = manifest
                        .strip_prefix(workspace_root)
                        .unwrap_or(&manifest)
                        .to_string_lossy()
                        .into_owned();
                    // Encoded as a shell-words parseable string; the runner
                    // splits it back into argv without invoking a shell.
                    format!(
                        "cargo check --quiet --manifest-path {}",
                        shell_quote(&relative)
                    )
                }),
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
        Self::verify_with_options(
            workspace_root,
            profile,
            tests_command,
            safety_command,
            quality_score_override,
            &VerifyOptions::default(),
        )
    }

    pub fn verify_with_options(
        workspace_root: &Path,
        profile: VerificationProfile,
        tests_command: Option<&str>,
        safety_command: Option<&str>,
        quality_score_override: Option<u32>,
        options: &VerifyOptions<'_>,
    ) -> Result<VerificationReport, RuntimeError> {
        let plan = Self::resolve_plan(workspace_root, profile, tests_command, safety_command);
        let timeout = options
            .command_timeout
            .unwrap_or_else(|| Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS));
        let mut stages = Vec::new();

        let static_check = run_optional_stage(
            VerificationStageKind::StaticCheck,
            true,
            plan.static_check_command.as_deref(),
            workspace_root,
            options,
            timeout,
        )?;
        let tests = run_optional_stage(
            VerificationStageKind::Tests,
            true,
            plan.tests_command.as_deref(),
            workspace_root,
            options,
            timeout,
        )?;
        let safety = run_optional_stage(
            VerificationStageKind::Safety,
            true,
            plan.safety_command.as_deref(),
            workspace_root,
            options,
            timeout,
        )?;

        // P0-2: at least one stage must have really executed (not Skipped).
        // If every stage was skipped, treat as verifier failure — a plan
        // without any command is a rubber stamp.
        let any_executed = matches!(
            static_check.stage.status,
            VerificationStageStatus::Passed | VerificationStageStatus::Failed
        ) || matches!(
            tests.stage.status,
            VerificationStageStatus::Passed | VerificationStageStatus::Failed
        ) || matches!(
            safety.stage.status,
            VerificationStageStatus::Passed | VerificationStageStatus::Failed
        );

        stages.push(static_check.stage);
        stages.push(tests.stage);
        stages.push(safety.stage);

        let tests_passed = any_executed && static_check.success && tests.success;
        let safety_checks_passed = any_executed && safety.success;
        let quality_score = quality_score_override.unwrap_or({
            if tests_passed && safety_checks_passed {
                DEFAULT_QUALITY_SCORE
            } else {
                0
            }
        });

        // P0-3: hash the source tree AFTER commands ran; caller compares this
        // against a re-hash right before promote to detect mid-flight edits.
        let source_tree_hash = hash_source_tree(workspace_root).ok();

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
            source_tree_hash,
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
    options: &VerifyOptions<'_>,
    timeout: Duration,
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

    let check = run_check_command(command, current_dir, options, timeout)?;
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

fn run_check_command(
    command: &str,
    current_dir: &Path,
    options: &VerifyOptions<'_>,
    timeout: Duration,
) -> Result<CommandCheckResult, RuntimeError> {
    if let Some(spec_json) = command.strip_prefix(PLUGIN_COMMAND_PREFIX) {
        return run_plugin_command(command, spec_json, current_dir, options);
    }
    run_shell_command(command, current_dir, timeout)
}

/// Map a serde_json serialization failure of a plugin's `payload_json` into
/// the `InvalidArgument` error surfaced to the verifier caller. Extracted so
/// the (in practice unreachable — a `Value` round-trips) error text stays
/// byte-for-byte stable and is unit-testable without provoking a real serde
/// failure.
fn payload_not_serializable(err: serde_json::Error) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: format!("plugin payload_json was not serializable: {err}"),
    }
}

fn run_plugin_command(
    original_command: &str,
    spec_json: &str,
    current_dir: &Path,
    options: &VerifyOptions<'_>,
) -> Result<CommandCheckResult, RuntimeError> {
    let spec: PluginCommandSpec =
        serde_json::from_str(spec_json).map_err(|err| RuntimeError::InvalidArgument {
            message: format!("invalid plugin verifier spec: {err}"),
        })?;
    let payload = serde_json::to_string(&spec.payload_json).map_err(payload_not_serializable)?;

    // P0-4: route through the caller-supplied candidate invoker when present.
    // Falling back to `PluginInvoker::load` would verify the currently-running
    // plugins — the exact bug this option exists to fix.
    let response = if let Some(invoker) = options.candidate_invoker {
        match invoker(&spec.plugin_path, &spec.node_id, payload) {
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
        }
    } else {
        let fixtures_root =
            resolve_plugin_fixtures_root(current_dir, spec.fixtures_root.as_deref());
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
        match invoker.invoke(&spec.plugin_path, &spec.node_id, payload) {
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
        }
    };

    let mut success = true;
    let mut stderr = String::new();
    if let Some(expected) = &spec.expect_substring {
        // P2-26: `contains` is intentionally a substring check — some
        // plugins produce structured JSON where the caller wants
        // `"value":7.0` to hit regardless of surrounding context. To
        // avoid the historical false-positive (`"value":7.0X"` matches
        // `"value":7.0`), support two disambiguation prefixes:
        //   * `exact:` — the response payload must equal the substring
        //     after the prefix verbatim.
        //   * `line:`  — one of the payload's `\n`-separated lines must
        //     equal the substring after the prefix verbatim.
        // Any other value keeps legacy `contains` behaviour.
        let ok = if let Some(needle) = expected.strip_prefix("exact:") {
            response.payload.trim() == needle
        } else if let Some(needle) = expected.strip_prefix("line:") {
            response.payload.lines().any(|l| l.trim() == needle)
        } else {
            response.payload.contains(expected)
        };
        if !ok {
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
        } else if current_dir.ends_with(path) {
            current_dir.to_path_buf()
        } else if let Some(parent) = current_dir.parent() {
            let sibling = parent.join(path);
            if sibling.join("plugins").exists() {
                sibling
            } else {
                current_dir.join(path)
            }
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

/// Wrap a `Child::try_wait` failure into the `CommandFailed` error returned by
/// `run_shell_command`. This poll-loop arm only fires if the OS refuses to
/// report the child's status (a kernel-level condition not reproducible from a
/// unit test), so the mapper is extracted to keep the error text byte-for-byte
/// stable and independently testable.
fn command_wait_error(program: &str, args: &[String], err: &std::io::Error) -> RuntimeError {
    RuntimeError::CommandFailed {
        program: program.to_string(),
        args: args.to_vec(),
        message: format!("wait failed: {err}"),
    }
}

/// P0-1: run the command as a real argv, NEVER through `bash -lc`.
/// The command string is split via `shell_words` (POSIX-shell tokenisation
/// with quoting, no expansion of `$VAR` / backticks / `$()`), then dispatched
/// to `Command::new(argv[0])` directly. Any attempt to inject a subshell,
/// command substitution, redirect, or backtick therefore lands in argv as a
/// literal string and does nothing.
///
/// The child is monitored with a wall-clock timeout. On expiry it is killed
/// and reaped so a hung `cargo test` cannot stall the whole iteration.
fn run_shell_command(
    command: &str,
    current_dir: &Path,
    timeout: Duration,
) -> Result<CommandCheckResult, RuntimeError> {
    let argv = match shell_words::split(command) {
        Ok(a) => a,
        Err(err) => {
            return Ok(CommandCheckResult {
                command: command.to_string(),
                runner: VerificationRunner::Shell,
                success: false,
                stdout: String::new(),
                stderr: format!("command tokenisation failed: {err}"),
            });
        }
    };
    let Some((program, args)) = argv.split_first() else {
        return Ok(CommandCheckResult {
            command: command.to_string(),
            runner: VerificationRunner::Shell,
            success: false,
            stdout: String::new(),
            stderr: "command was empty after tokenisation".to_string(),
        });
    };
    if program.is_empty() {
        return Ok(CommandCheckResult {
            command: command.to_string(),
            runner: VerificationRunner::Shell,
            success: false,
            stdout: String::new(),
            stderr: "command program was empty".to_string(),
        });
    }

    let mut child = Command::new(program)
        .args(args)
        .current_dir(current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| RuntimeError::CommandFailed {
            program: program.clone(),
            args: args.to_vec(),
            message: err.to_string(),
        })?;

    // Poll for exit up to `timeout`. On expiry, kill + reap.
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break std::process::ExitStatus::default();
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => {
                return Err(command_wait_error(program, args, &err));
            }
        }
    };

    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_end(&mut stdout_buf);
    }
    if let Some(mut err) = child.stderr.take() {
        use std::io::Read;
        let _ = err.read_to_end(&mut stderr_buf);
    }

    let success = !timed_out && status.success();
    let stderr = if timed_out {
        let base = String::from_utf8_lossy(&stderr_buf).trim().to_string();
        if base.is_empty() {
            format!("command timed out after {:?}", timeout)
        } else {
            format!("command timed out after {:?}; stderr: {base}", timeout)
        }
    } else {
        String::from_utf8_lossy(&stderr_buf).trim().to_string()
    };

    Ok(CommandCheckResult {
        command: command.to_string(),
        runner: VerificationRunner::Shell,
        success,
        stdout: String::from_utf8_lossy(&stdout_buf).trim().to_string(),
        stderr,
    })
}

/// Quote a single argument for later `shell_words::split` — since we control
/// both sides we only need enough escaping that round-tripping is stable.
fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b'='))
    {
        s.to_string()
    } else {
        let escaped = s.replace('\\', "\\\\").replace('\'', "'\\''");
        format!("'{}'", escaped)
    }
}

/// P0-3: compute a stable digest of every regular file inside `root` (skipping
/// `target/` and hidden dirs). Returned as a lowercase hex string. Caller
/// compares before/after verify to detect concurrent mutation.
///
/// The hash is deterministic: we sort files by relative path, then feed
/// `path\0len\0bytes` into a single sha256. Symlinks are followed (they must
/// resolve to a file). Missing / unreadable entries surface as an Err so the
/// caller can decide policy.
pub fn hash_source_tree(root: &Path) -> Result<String, RuntimeError> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut entries: BTreeMap<String, PathBuf> = BTreeMap::new();
    collect_source_tree(&root, &root, &mut entries).map_err(|err| RuntimeError::Io {
        path: root.clone(),
        message: format!("hash_source_tree walk failed: {err}"),
    })?;

    let mut hasher = Sha256::new();
    for (rel, abs) in &entries {
        let bytes = fs::read(abs).map_err(|err| RuntimeError::Io {
            path: abs.clone(),
            message: format!("hash_source_tree read failed: {err}"),
        })?;
        hasher.update(rel.as_bytes());
        hasher.update([0u8]);
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update([0u8]);
        hasher.update(&bytes);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn collect_source_tree(
    root: &Path,
    current: &Path,
    entries: &mut BTreeMap<String, PathBuf>,
) -> std::io::Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if matches!(
                name_str.as_ref(),
                "target" | "node_modules" | ".cordis-drafts"
            ) {
                continue;
            }
            collect_source_tree(root, &path, entries)?;
        } else if file_type.is_file() || file_type.is_symlink() {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                entries.insert(rel_str, path);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        collect_source_tree, command_wait_error, discover_rust_workspace_manifest,
        hash_source_tree, normalize_optional_command, payload_not_serializable,
        resolve_plugin_fixtures_root, run_shell_command, shell_quote, CommandVerifier,
        PluginResponse, RuntimeError, VerificationProfile, VerificationRunner,
        VerificationStageKind, VerificationStageStatus, VerifyOptions,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tempfile::TempDir;

    /// `command_wait_error` builds the `CommandFailed` error the poll loop
    /// returns when `Child::try_wait` fails. That arm is unreachable from a
    /// unit test (the OS would have to refuse status reporting for a live
    /// child), so this asserts the mapper's byte-exact message and field
    /// propagation directly.
    #[test]
    fn command_wait_error_wraps_program_args_and_message() {
        let io = std::io::Error::other("boom");
        let err = command_wait_error("cargo", &["test".to_string(), "-q".to_string()], &io);
        assert!(
            matches!(&err, RuntimeError::CommandFailed { program, args, message } if program == "cargo" && args == &vec!["test".to_string(), "-q".to_string()] && message == "wait failed: boom"),
            "unexpected: {err:?}"
        );
    }

    /// `payload_not_serializable` maps a serde_json failure into
    /// `InvalidArgument`. Serializing a `Value` never fails in practice, so the
    /// mapper is tested with a synthetic serde error to lock the message text.
    #[test]
    fn payload_not_serializable_wraps_serde_error() {
        // Serializing a `Value` never fails in practice, so fabricate a real
        // `serde_json::Error` from a malformed parse to lock the message text.
        let serde_err = serde_json::from_str::<serde_json::Value>("{")
            .expect_err("malformed json should error");
        let err = payload_not_serializable(serde_err);
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message } if message.starts_with("plugin payload_json was not serializable: ")),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn verify_without_any_command_now_fails() {
        // P0-2: no test/safety command → verifier used to rubber-stamp Pass.
        // Now it must report tests_passed=false.
        let report = CommandVerifier::verify(
            Path::new("."),
            VerificationProfile::Default,
            None,
            None,
            None,
        )
        .expect("verify should return report");
        assert!(!report.input.tests_passed);
        assert!(!report.input.safety_checks_passed);
        assert_eq!(report.input.quality_score, 0);
    }

    #[test]
    fn verify_marks_failed_command() {
        let report = CommandVerifier::verify(
            Path::new("."),
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
    fn shell_injection_via_tests_command_is_blocked() {
        // P0-1: `; touch <marker>` used to be evaluated by bash -lc. Now the
        // whole string tokenises to argv, and `echo` runs literally with those
        // characters — no subshell.
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("pwned");
        let payload = format!("echo hello ; touch {}", marker.display());
        let report = CommandVerifier::verify(
            temp.path(),
            VerificationProfile::Default,
            Some(&payload),
            None,
            None,
        )
        .expect("verify should return report");
        // The runner may treat `;` as an argument or fail; the only invariant
        // that matters is that no shell interpreted it.
        assert!(
            !marker.exists(),
            "shell metachars must not spawn a subshell"
        );
        let _ = report;
    }

    #[test]
    fn shell_command_honors_timeout() {
        let temp = TempDir::new().expect("tempdir");
        let result = run_shell_command("sleep 5", temp.path(), Duration::from_millis(200))
            .expect("timeout path should not panic");
        assert!(!result.success);
        assert!(
            result.stderr.contains("timed out"),
            "stderr: {}",
            result.stderr
        );
    }

    #[test]
    fn verify_supports_plugin_command_specs() {
        if cordis_plugin_sdk::CORDIS_TARGET != "x86_64-unknown-linux-gnu" {
            eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
            return;
        }
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
    fn resolve_plugin_fixtures_root_uses_current_fixtures_dir_without_duplication() {
        let resolved = resolve_plugin_fixtures_root(Path::new("fixtures"), Some("fixtures"));
        assert_eq!(resolved, Path::new("fixtures"));
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
        fs::write(
            temp.path().join("src/lib.rs"),
            "pub fn demo() -> u32 { 1 }\n",
        )
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
        assert!(report.stages[0]
            .check
            .as_ref()
            .expect("static check")
            .command
            .contains("cargo check --quiet --manifest-path Cargo.toml"));
    }

    #[test]
    fn hash_source_tree_is_stable_and_detects_change() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("src")).unwrap();
        fs::write(temp.path().join("src/lib.rs"), "fn a() {}\n").unwrap();

        let h1 = hash_source_tree(temp.path()).unwrap();
        let h2 = hash_source_tree(temp.path()).unwrap();
        assert_eq!(h1, h2);

        fs::write(temp.path().join("src/lib.rs"), "fn b() {}\n").unwrap();
        let h3 = hash_source_tree(temp.path()).unwrap();
        assert_ne!(h1, h3);
    }

    // Preserve legacy assertion: an explicit `true` tests command *does* pass.
    #[test]
    fn verify_passes_when_all_stages_execute_and_succeed() {
        let report = CommandVerifier::verify(
            Path::new("."),
            VerificationProfile::Default,
            Some("true"),
            Some("true"),
            None,
        )
        .expect("verify should return report");
        assert!(report.input.tests_passed);
        assert!(report.input.safety_checks_passed);
        assert_eq!(report.input.quality_score, 90);
        assert_eq!(report.stages[1].kind, VerificationStageKind::Tests);
        assert_eq!(report.stages[1].status, VerificationStageStatus::Passed);
    }

    #[test]
    fn verification_profile_as_str_maps_both_variants() {
        assert_eq!(VerificationProfile::Default.as_str(), "default");
        assert_eq!(
            VerificationProfile::RustWorkspace.as_str(),
            "rust_workspace"
        );
        // Default derive resolves to the Default variant.
        assert_eq!(VerificationProfile::default(), VerificationProfile::Default);
    }

    #[test]
    fn normalize_optional_command_trims_and_drops_blank() {
        assert_eq!(normalize_optional_command(None), None);
        assert_eq!(normalize_optional_command(Some("   ")), None);
        assert_eq!(normalize_optional_command(Some("")), None);
        assert_eq!(
            normalize_optional_command(Some("  /bin/echo hi  ")),
            Some("/bin/echo hi".to_string())
        );
    }

    #[test]
    fn shell_quote_passes_safe_tokens_and_escapes_specials() {
        // Alphanumeric plus the safe punctuation set is returned verbatim.
        assert_eq!(shell_quote("Cargo.toml"), "Cargo.toml");
        assert_eq!(shell_quote("a/b-c_d.=e"), "a/b-c_d.=e");
        // Empty string is not "safe" (guarded by `!s.is_empty()`) → quoted.
        assert_eq!(shell_quote(""), "''");
        // Spaces force single-quoting.
        assert_eq!(shell_quote("a b"), "'a b'");
        // Embedded single-quote uses the '\'' escape and round-trips through
        // shell_words::split back to the original token.
        let quoted = shell_quote("it's a $VAR");
        let split = shell_words::split(&quoted).expect("quoted token must re-split");
        assert_eq!(split, vec!["it's a $VAR".to_string()]);
        // Backslash forces quoting and is doubled inside the single-quoted
        // form. (Backslash round-trip fidelity is not a contract of this
        // helper; it only quotes safe manifest paths.)
        assert_eq!(shell_quote("a\\b"), "'a\\\\b'");
    }

    #[test]
    fn discover_rust_workspace_manifest_prefers_direct_then_nested_then_none() {
        // No manifest anywhere → None.
        let empty = TempDir::new().expect("tempdir");
        assert_eq!(discover_rust_workspace_manifest(empty.path()), None);

        // Nested plugins/Cargo.toml is discovered when the direct one is absent.
        let nested = TempDir::new().expect("tempdir");
        fs::create_dir_all(nested.path().join("plugins")).expect("plugins dir");
        fs::write(nested.path().join("plugins/Cargo.toml"), "[workspace]\n").expect("nested toml");
        assert_eq!(
            discover_rust_workspace_manifest(nested.path()),
            Some(nested.path().join("plugins/Cargo.toml"))
        );

        // A direct Cargo.toml wins over the nested one.
        let direct = TempDir::new().expect("tempdir");
        fs::write(direct.path().join("Cargo.toml"), "[workspace]\n").expect("direct toml");
        fs::create_dir_all(direct.path().join("plugins")).expect("plugins dir");
        fs::write(direct.path().join("plugins/Cargo.toml"), "[workspace]\n").expect("nested toml");
        assert_eq!(
            discover_rust_workspace_manifest(direct.path()),
            Some(direct.path().join("Cargo.toml"))
        );
    }

    #[test]
    fn resolve_plan_default_profile_has_no_static_check() {
        let temp = TempDir::new().expect("tempdir");
        let plan = CommandVerifier::resolve_plan(
            temp.path(),
            VerificationProfile::Default,
            Some("  echo t  "),
            None,
        );
        assert_eq!(plan.profile, VerificationProfile::Default);
        assert_eq!(plan.static_check_command, None);
        // tests_command is normalized (trimmed); empty safety stays None.
        assert_eq!(plan.tests_command.as_deref(), Some("echo t"));
        assert_eq!(plan.safety_command, None);
    }

    #[test]
    fn resolve_plan_rust_workspace_encodes_relative_manifest() {
        let temp = TempDir::new().expect("tempdir");
        fs::write(temp.path().join("Cargo.toml"), "[workspace]\n").expect("cargo toml");
        let plan = CommandVerifier::resolve_plan(
            temp.path(),
            VerificationProfile::RustWorkspace,
            None,
            None,
        );
        assert_eq!(
            plan.static_check_command.as_deref(),
            Some("cargo check --quiet --manifest-path Cargo.toml")
        );
    }

    #[test]
    fn run_shell_command_reports_empty_after_tokenisation() {
        let temp = TempDir::new().expect("tempdir");
        let result = run_shell_command("   ", temp.path(), Duration::from_secs(5))
            .expect("empty command should not panic");
        assert!(!result.success);
        assert!(
            result.stderr.contains("empty after tokenisation"),
            "stderr: {}",
            result.stderr
        );
        assert_eq!(result.runner, VerificationRunner::Shell);
    }

    // A quoted empty token (`''`) tokenises to a single empty-string argv
    // element — `split_first` succeeds (non-empty argv) but the program name
    // itself is empty, exercising the `program.is_empty()` guard, distinct
    // from the empty-argv "after tokenisation" branch above.
    #[test]
    fn run_shell_command_reports_empty_program_name() {
        let temp = TempDir::new().expect("tempdir");
        let result = run_shell_command("''", temp.path(), Duration::from_secs(5))
            .expect("empty program should not panic");
        assert!(!result.success);
        assert!(
            result.stderr.contains("command program was empty"),
            "stderr: {}",
            result.stderr
        );
        assert_eq!(result.runner, VerificationRunner::Shell);
    }

    #[test]
    fn run_shell_command_reports_tokenisation_failure() {
        let temp = TempDir::new().expect("tempdir");
        // Unbalanced quote — shell_words::split returns Err.
        let result = run_shell_command("echo 'unterminated", temp.path(), Duration::from_secs(5))
            .expect("tokenisation failure should not panic");
        assert!(!result.success);
        assert!(
            result.stderr.contains("tokenisation failed"),
            "stderr: {}",
            result.stderr
        );
    }

    #[test]
    fn run_shell_command_captures_success_stdout() {
        let temp = TempDir::new().expect("tempdir");
        let result =
            run_shell_command("/bin/echo hello world", temp.path(), Duration::from_secs(5))
                .expect("echo should run");
        assert!(result.success);
        assert_eq!(result.stdout, "hello world");
        assert!(result.stderr.is_empty());
        assert_eq!(result.runner, VerificationRunner::Shell);
    }

    #[test]
    fn run_shell_command_reports_failure_exit_code() {
        let temp = TempDir::new().expect("tempdir");
        // `false` exits non-zero with no output.
        let result =
            run_shell_command("false", temp.path(), Duration::from_secs(5)).expect("false runs");
        assert!(!result.success);
    }

    #[test]
    fn run_shell_command_spawn_error_surfaces_as_err() {
        let temp = TempDir::new().expect("tempdir");
        let err = run_shell_command(
            "/nonexistent/definitely/not/here arg",
            temp.path(),
            Duration::from_secs(5),
        )
        .expect_err("missing program must surface a spawn error");
        assert!(matches!(err, RuntimeError::CommandFailed { .. }));
    }

    #[test]
    fn plugin_command_uses_candidate_invoker_and_exact_prefix() {
        // exact: prefix requires verbatim equality of the trimmed payload.
        let invoker = |_plugin: &str, _node: &str, _payload: String| {
            Ok(PluginResponse {
                payload: "  42  ".to_string(),
            })
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&invoker),
            command_timeout: None,
        };
        let spec = format!(
            "plugin:{}",
            json!({
                "plugin_path": "p",
                "node_id": "n",
                "expect_substring": "exact:42"
            })
        );
        let report = CommandVerifier::verify_with_options(
            Path::new("."),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("verify should return a report");
        assert!(report.input.tests_passed);
        let check = report.tests.as_ref().expect("tests check present");
        assert_eq!(check.runner, VerificationRunner::Plugin);
        assert_eq!(check.stdout, "  42  ");
    }

    #[test]
    fn plugin_command_exact_prefix_mismatch_fails_with_reason() {
        let invoker = |_p: &str, _n: &str, _pl: String| {
            Ok(PluginResponse {
                payload: "43".to_string(),
            })
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&invoker),
            command_timeout: None,
        };
        let spec = format!(
            "plugin:{}",
            json!({"plugin_path": "p", "node_id": "n", "expect_substring": "exact:42"})
        );
        let report = CommandVerifier::verify_with_options(
            Path::new("."),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        assert!(!report.input.tests_passed);
        let check = report.tests.as_ref().expect("check");
        assert!(!check.success);
        assert!(check.stderr.contains("missing expected substring"));
    }

    #[test]
    fn plugin_command_line_prefix_matches_one_line() {
        let invoker = |_p: &str, _n: &str, _pl: String| {
            Ok(PluginResponse {
                payload: "alpha\n  target  \nbeta".to_string(),
            })
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&invoker),
            command_timeout: None,
        };
        let spec = format!(
            "plugin:{}",
            json!({"plugin_path": "p", "node_id": "n", "expect_substring": "line:target"})
        );
        let report = CommandVerifier::verify_with_options(
            Path::new("."),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        assert!(report.input.tests_passed, "report: {report:?}");
    }

    #[test]
    fn plugin_command_contains_prefix_is_default_substring() {
        let invoker = |_p: &str, _n: &str, _pl: String| {
            Ok(PluginResponse {
                payload: "prefix-NEEDLE-suffix".to_string(),
            })
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&invoker),
            command_timeout: None,
        };
        let spec = format!(
            "plugin:{}",
            json!({"plugin_path": "p", "node_id": "n", "expect_substring": "NEEDLE"})
        );
        let report = CommandVerifier::verify_with_options(
            Path::new("."),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        assert!(report.input.tests_passed);
    }

    #[test]
    fn plugin_command_no_expect_substring_always_succeeds() {
        let invoker = |_p: &str, _n: &str, payload: String| {
            // The default payload_json is an empty object.
            assert_eq!(payload, "{}");
            Ok(PluginResponse {
                payload: "anything".to_string(),
            })
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&invoker),
            command_timeout: None,
        };
        let spec = format!("plugin:{}", json!({"plugin_path": "p", "node_id": "n"}));
        let report = CommandVerifier::verify_with_options(
            Path::new("."),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        assert!(report.input.tests_passed);
    }

    #[test]
    fn plugin_command_invoker_error_becomes_failed_check() {
        let invoker = |_p: &str, _n: &str, _pl: String| {
            Err(RuntimeError::Invariant {
                message: "candidate blew up".to_string(),
            })
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&invoker),
            command_timeout: None,
        };
        let spec = format!("plugin:{}", json!({"plugin_path": "p", "node_id": "n"}));
        let report = CommandVerifier::verify_with_options(
            Path::new("."),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        assert!(!report.input.tests_passed);
        let check = report.tests.as_ref().expect("check");
        assert!(!check.success);
        assert!(check.stderr.contains("candidate blew up"));
        assert_eq!(check.runner, VerificationRunner::Plugin);
    }

    #[test]
    fn plugin_command_invalid_spec_json_is_invalid_argument() {
        let options = VerifyOptions::default();
        let report = super::run_check_command(
            "plugin:{not valid json",
            Path::new("."),
            &options,
            Duration::from_secs(5),
        );
        let err = report.expect_err("bad spec json must be an error");
        assert!(matches!(err, RuntimeError::InvalidArgument { .. }));
    }

    #[test]
    fn plugin_command_without_candidate_loads_invoker_and_reports_failure() {
        // No candidate_invoker → the fallback path builds a real
        // PluginInvoker from the resolved fixtures root. Point it at an empty
        // artifact index so the load succeeds but the invoke of an unknown
        // plugin fails, surfacing as a failed (not errored) check.
        let temp = TempDir::new().expect("tempdir");
        let artifacts = temp.path().join("artifacts");
        fs::create_dir_all(&artifacts).expect("artifacts dir");
        fs::write(
            artifacts.join("index.json"),
            r#"{"generated_at":"1970-01-01T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");

        let options = VerifyOptions::default();
        let spec = format!("plugin:{}", json!({"plugin_path": "ghost", "node_id": "n"}));
        let report = CommandVerifier::verify_with_options(
            temp.path(),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        let check = report.tests.as_ref().expect("tests check present");
        assert_eq!(check.runner, VerificationRunner::Plugin);
        assert!(!check.success, "unknown plugin invoke should fail");
        assert!(!check.stderr.is_empty());
    }

    #[test]
    fn resolve_plugin_fixtures_root_absolute_requested_returned_as_is() {
        let abs = if cfg!(windows) {
            PathBuf::from("C:/abs/root")
        } else {
            PathBuf::from("/abs/root")
        };
        let resolved =
            resolve_plugin_fixtures_root(Path::new("current"), Some(&abs.to_string_lossy()));
        assert_eq!(resolved, abs);
    }

    #[test]
    fn resolve_plugin_fixtures_root_sibling_with_plugins_wins() {
        // parent/<requested>/plugins exists → sibling path selected.
        let temp = TempDir::new().expect("tempdir");
        let parent = temp.path();
        fs::create_dir_all(parent.join("shared/plugins")).expect("sibling plugins");
        let current = parent.join("workdir");
        fs::create_dir_all(&current).expect("current dir");
        let resolved = resolve_plugin_fixtures_root(&current, Some("shared"));
        assert_eq!(resolved, parent.join("shared"));
    }

    #[test]
    fn resolve_plugin_fixtures_root_falls_back_to_current_join_when_no_sibling() {
        let temp = TempDir::new().expect("tempdir");
        let current = temp.path().join("workdir");
        fs::create_dir_all(&current).expect("current dir");
        // No sibling `shared/plugins` exists, so it joins under current.
        let resolved = resolve_plugin_fixtures_root(&current, Some("shared"));
        assert_eq!(resolved, current.join("shared"));
    }

    #[test]
    fn resolve_plugin_fixtures_root_none_prefers_current_plugins_dir() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("plugins")).expect("plugins");
        let resolved = resolve_plugin_fixtures_root(temp.path(), None);
        assert_eq!(resolved, temp.path());
    }

    #[test]
    fn resolve_plugin_fixtures_root_none_falls_back_to_nested_fixtures() {
        let temp = TempDir::new().expect("tempdir");
        fs::create_dir_all(temp.path().join("fixtures/plugins")).expect("nested fixtures");
        let resolved = resolve_plugin_fixtures_root(temp.path(), None);
        assert_eq!(resolved, temp.path().join("fixtures"));
    }

    #[test]
    fn resolve_plugin_fixtures_root_none_defaults_to_current_dir() {
        let temp = TempDir::new().expect("tempdir");
        // Neither plugins/ nor fixtures/plugins present → current dir.
        let resolved = resolve_plugin_fixtures_root(temp.path(), None);
        assert_eq!(resolved, temp.path());
    }

    #[test]
    fn collect_source_tree_skips_hidden_and_ignored_dirs() {
        let temp = TempDir::new().expect("tempdir");
        let root = temp.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn a() {}").unwrap();
        fs::write(root.join("top.txt"), "hi").unwrap();
        // These must all be skipped by the walker.
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("target/artifact"), "x").unwrap();
        fs::create_dir_all(root.join("node_modules")).unwrap();
        fs::write(root.join("node_modules/dep"), "x").unwrap();
        fs::create_dir_all(root.join(".cordis-drafts")).unwrap();
        fs::write(root.join(".cordis-drafts/j"), "x").unwrap();
        fs::create_dir_all(root.join(".hidden")).unwrap();
        fs::write(root.join(".hidden/secret"), "x").unwrap();

        let mut entries: BTreeMap<String, PathBuf> = BTreeMap::new();
        collect_source_tree(root, root, &mut entries).expect("walk should succeed");
        let keys: Vec<&str> = entries.keys().map(|s| s.as_str()).collect();
        assert!(keys.contains(&"src/lib.rs"));
        assert!(keys.contains(&"top.txt"));
        assert!(
            keys.iter().all(|k| !k.contains("target")
                && !k.contains("node_modules")
                && !k.contains(".cordis-drafts")
                && !k.contains(".hidden")),
            "ignored dirs leaked: {keys:?}"
        );
    }

    #[test]
    fn hash_source_tree_errors_when_root_missing() {
        let temp = TempDir::new().expect("tempdir");
        let missing = temp.path().join("does-not-exist");
        let err = hash_source_tree(&missing).expect_err("missing root must error");
        assert!(matches!(err, RuntimeError::Io { .. }));
    }

    // ---------- residual branch coverage ----------

    #[test]
    fn verify_propagates_shell_spawn_error() {
        // A tests_command whose program does not exist makes run_shell_command
        // return Err(CommandFailed); run_optional_stage propagates it with `?`,
        // so verify_with_options returns Err rather than a report. Covers the
        // `?`-propagation on the stage results.
        let temp = TempDir::new().expect("tempdir");
        let err = CommandVerifier::verify(
            temp.path(),
            VerificationProfile::Default,
            Some("/nonexistent/definitely/not/here run"),
            None,
            None,
        )
        .expect_err("missing program must surface as verify error");
        assert!(matches!(err, RuntimeError::CommandFailed { .. }));
    }

    #[test]
    fn resolve_plugin_fixtures_root_root_dir_without_parent_joins() {
        // current_dir "/" has no parent and does not end_with the relative
        // request, so the final `else` joins under current_dir.
        let root = Path::new("/");
        if root.parent().is_some() {
            eprintln!("[skip] platform root has a parent; branch not applicable");
            return;
        }
        let resolved = resolve_plugin_fixtures_root(root, Some("shared"));
        assert_eq!(resolved, root.join("shared"));
    }

    #[test]
    fn run_shell_command_captures_stderr_on_success() {
        // `sh -c 'echo err >&2'` runs sh as a real program (argv, no injection)
        // and writes to stderr while exiting 0 — covers the stderr read path.
        let temp = TempDir::new().expect("tempdir");
        let result = run_shell_command(
            "sh -c 'echo boom 1>&2'",
            temp.path(),
            Duration::from_secs(5),
        )
        .expect("sh should run");
        assert!(result.success);
        assert!(result.stderr.contains("boom"), "stderr: {}", result.stderr);
    }

    #[test]
    fn shell_command_timeout_includes_captured_stderr() {
        // Child writes to stderr, then hangs past the timeout. The timeout
        // branch must fold the captured stderr into the message.
        let temp = TempDir::new().expect("tempdir");
        let result = run_shell_command(
            "sh -c 'echo pre-timeout 1>&2; sleep 5'",
            temp.path(),
            Duration::from_millis(300),
        )
        .expect("timeout path should not panic");
        assert!(!result.success);
        assert!(
            result.stderr.contains("timed out"),
            "stderr: {}",
            result.stderr
        );
        assert!(
            result.stderr.contains("pre-timeout"),
            "captured stderr should be folded in: {}",
            result.stderr
        );
    }

    #[test]
    fn plugin_command_fallback_load_failure_reports_failed_check() {
        // No candidate_invoker and a fixtures root that has no artifact index
        // → PluginInvoker::load itself fails (not the invoke), covering the
        // load-error arm that returns a failed (not errored) check.
        let temp = TempDir::new().expect("tempdir");
        // current_dir with neither plugins/ nor fixtures/plugins → resolves to
        // itself, and load finds no artifacts/index.json.
        let options = VerifyOptions::default();
        let spec = format!("plugin:{}", json!({"plugin_path": "p", "node_id": "n"}));
        let report = CommandVerifier::verify_with_options(
            temp.path(),
            VerificationProfile::Default,
            Some(&spec),
            None,
            None,
            &options,
        )
        .expect("report");
        let check = report.tests.as_ref().expect("tests check present");
        assert_eq!(check.runner, VerificationRunner::Plugin);
        assert!(
            !check.success,
            "missing artifact index should fail the load"
        );
        assert!(!check.stderr.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn hash_source_tree_read_failure_surfaces_io() {
        use std::os::unix::fs::PermissionsExt;
        // Root ignores file-mode permissions, so an unreadable file still reads.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("[skip] running as root; unreadable file would still read");
            return;
        }
        let temp = TempDir::new().expect("tempdir");
        let secret = temp.path().join("secret.txt");
        fs::write(&secret, "top secret").unwrap();
        let mut perms = fs::metadata(&secret).unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&secret, perms).unwrap();
        // The walk lists the file, but the read inside hash_source_tree fails.
        let err = hash_source_tree(temp.path()).expect_err("unreadable file must error");
        assert!(matches!(err, RuntimeError::Io { .. }), "got: {err:?}");
    }
}
