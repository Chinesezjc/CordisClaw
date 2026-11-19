//! The gate a self-proposed change has to pass before anyone believes it.
//!
//! The reflection loop can write a patch for a failure it noticed. Without a
//! check, "I fixed it" is just the model's opinion — and the failure this whole
//! effort started from was exactly that: a change that looked fine and broke
//! something else.
//!
//! So a proposal is judged by a counterfactual: the regression test it ships
//! must **fail on the untouched tree** and **pass once the patch is applied**.
//! A test that passes either way proves nothing, which is the failure mode a
//! plain "run the tests" gate cannot see.
//!
//! Every command goes through [`Runner`], so the decision logic is testable
//! without shelling out to git or cargo.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// What the reflection loop hands to the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Proposal {
    /// Signature id from `selfcheck` — what failure this claims to fix.
    pub signature: String,
    /// One line, for the report.
    pub summary: String,
    /// Cargo package the test lives in, e.g. `cordis-agent`.
    pub package: String,
    /// Test name filter, e.g. `imp::tests::history_never_clears`.
    pub test: String,
    /// Unified diff, relative to the proposal directory.
    pub patch: String,
}

impl Proposal {
    /// Read `proposal.json` from a directory.
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("proposal.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let proposal: Proposal = serde_json::from_str(&text)
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        proposal.validate(dir)?;
        Ok(proposal)
    }

    fn validate(&self, dir: &Path) -> Result<(), String> {
        if self.signature.trim().is_empty() {
            return Err("proposal has no signature".to_string());
        }
        if self.package.trim().is_empty() || self.test.trim().is_empty() {
            return Err("proposal names no package or test".to_string());
        }
        let patch = dir.join(&self.patch);
        if !patch.is_file() {
            return Err(format!("patch {} is missing", patch.display()));
        }
        Ok(())
    }

    pub fn patch_path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.patch)
    }
}

/// Runs one shell command in a directory.
pub trait Runner {
    /// `(success, combined output)`.
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> (bool, String);
}

/// The real one: git and cargo.
pub struct ShellRunner;

impl Runner for ShellRunner {
    fn run(&self, cwd: &Path, program: &str, args: &[&str]) -> (bool, String) {
        match Command::new(program).current_dir(cwd).args(args).output() {
            Ok(output) => {
                let mut text = String::from_utf8_lossy(&output.stdout).to_string();
                text.push_str(&String::from_utf8_lossy(&output.stderr));
                (output.status.success(), text)
            }
            Err(error) => (false, format!("{program}: {error}")),
        }
    }
}

/// Why a proposal was refused, or that it stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum Verdict {
    /// The test fails without the patch and passes with it.
    Verified { test: String },
    /// The patch does not apply to a clean tree.
    PatchRejected { reason: String },
    /// The test passes even without the patch: it does not reproduce the bug.
    TestDoesNotReproduce { reason: String },
    /// The patch fixes the test but breaks something else.
    Regressed { reason: String },
    /// The test does not pass even with the patch.
    StillFailing { reason: String },
}

impl Verdict {
    pub fn is_verified(&self) -> bool {
        matches!(self, Verdict::Verified { .. })
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Verdict::Verified { .. } => None,
            Verdict::PatchRejected { reason }
            | Verdict::TestDoesNotReproduce { reason }
            | Verdict::Regressed { reason }
            | Verdict::StillFailing { reason } => Some(reason),
        }
    }
}

/// How long a single cargo invocation may take.
const GATE_TIMEOUT_HINT: &str = "cargo test/clippy can take minutes";

/// Judge a proposal in a scratch checkout.
///
/// The caller provides `scratch`: a clean worktree of the unpatched tree. This
/// function mutates it (applies and reverts the patch), so it must not be the
/// working tree the operator is using.
pub fn verify<R: Runner>(runner: &R, scratch: &Path, dir: &Path, proposal: &Proposal) -> Verdict {
    let patch = proposal.patch_path(dir);
    let patch_arg = patch.to_string_lossy().to_string();

    // 1. The patch must apply at all.
    let (applied, output) = runner.run(scratch, "git", &["apply", "--check", &patch_arg]);
    if !applied {
        return Verdict::PatchRejected {
            reason: first_lines(&output),
        };
    }

    // 2. The shipped test must fail on the clean tree, or it proves nothing.
    let (passes_before, before) = run_test(runner, scratch, &proposal.package, &proposal.test);
    if passes_before {
        return Verdict::TestDoesNotReproduce {
            reason: format!(
                "`{}` already passes without the patch: {GATE_TIMEOUT_HINT}\n{}",
                proposal.test,
                first_lines(&before)
            ),
        };
    }

    // 3. Apply, then the test must pass.
    let (applied, output) = runner.run(scratch, "git", &["apply", &patch_arg]);
    if !applied {
        return Verdict::PatchRejected {
            reason: first_lines(&output),
        };
    }
    let (passes_after, after) = run_test(runner, scratch, &proposal.package, &proposal.test);
    if !passes_after {
        let _ = runner.run(scratch, "git", &["checkout", "--", "."]);
        return Verdict::StillFailing {
            reason: first_lines(&after),
        };
    }

    // 4. The patch must not break the rest of the crate.
    let (clean, output) = runner.run(
        scratch,
        "cargo",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--",
            "-D",
            "warnings",
        ],
    );

    // Always leave the scratch tree clean.
    let _ = runner.run(scratch, "git", &["checkout", "--", "."]);

    if !clean {
        return Verdict::Regressed {
            reason: first_lines(&output),
        };
    }
    Verdict::Verified {
        test: proposal.test.clone(),
    }
}

fn run_test<R: Runner>(runner: &R, cwd: &Path, package: &str, test: &str) -> (bool, String) {
    runner.run(
        cwd,
        "cargo",
        &["test", "-p", package, "--lib", test, "--", "--exact"],
    )
}

fn first_lines(output: &str) -> String {
    let text: String = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .take(8)
        .collect::<Vec<_>>()
        .join("\n");
    text.chars().take(800).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Scripted runner: each call pops the next `(success, output)`.
    struct Scripted {
        calls: RefCell<Vec<String>>,
        replies: RefCell<VecDeque<(bool, String)>>,
    }

    impl Scripted {
        fn new(replies: Vec<(bool, String)>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                replies: RefCell::new(replies.into()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl Runner for Scripted {
        fn run(&self, _cwd: &Path, program: &str, args: &[&str]) -> (bool, String) {
            self.calls
                .borrow_mut()
                .push(format!("{program} {}", args.join(" ")));
            self.replies
                .borrow_mut()
                .pop_front()
                .unwrap_or((false, "no scripted reply".to_string()))
        }
    }

    fn temp_proposal(dir: &Path) -> Proposal {
        std::fs::write(dir.join("patch.diff"), "diff --git a b\n").unwrap();
        Proposal {
            signature: "empty-abc".to_string(),
            summary: "keep history non-empty".to_string(),
            package: "cordis-agent".to_string(),
            test: "imp::tests::history_never_clears".to_string(),
            patch: "patch.diff".to_string(),
        }
    }

    #[test]
    fn a_test_that_passes_without_the_patch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = temp_proposal(dir.path());
        let runner = Scripted::new(vec![
            (true, String::new()),
            (true, "test result: ok".to_string()),
        ]);
        let verdict = verify(&runner, Path::new("/scratch"), dir.path(), &proposal);
        assert_eq!(
            verdict,
            Verdict::TestDoesNotReproduce {
                reason: verdict.reason().unwrap().to_string()
            }
        );
        assert!(verdict.reason().unwrap().contains("already passes"));
    }

    #[test]
    fn a_patch_that_reproduces_then_fixes_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = temp_proposal(dir.path());
        let runner = Scripted::new(vec![
            (true, String::new()),
            (false, "FAILED".to_string()),
            (true, String::new()),
            (true, "test result: ok".to_string()),
            (true, String::new()),
            (true, String::new()),
        ]);
        let verdict = verify(&runner, Path::new("/scratch"), dir.path(), &proposal);
        assert_eq!(
            verdict,
            Verdict::Verified {
                test: proposal.test.clone()
            }
        );
        // The scratch tree is always restored.
        assert!(runner
            .calls()
            .iter()
            .any(|call| call.starts_with("git checkout")));
    }

    #[test]
    fn a_failing_clippy_run_is_a_regression() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = temp_proposal(dir.path());
        let runner = Scripted::new(vec![
            (true, String::new()),
            (false, "FAILED".to_string()),
            (true, String::new()),
            (true, "test result: ok".to_string()),
            (false, "error: unused import".to_string()),
            (true, String::new()),
        ]);
        let verdict = verify(&runner, Path::new("/scratch"), dir.path(), &proposal);
        assert!(matches!(verdict, Verdict::Regressed { .. }));
        assert!(verdict.reason().unwrap().contains("unused import"));
    }

    #[test]
    fn a_test_that_still_fails_after_the_patch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = temp_proposal(dir.path());
        let runner = Scripted::new(vec![
            (true, String::new()),
            (false, "FAILED".to_string()),
            (true, String::new()),
            (false, "still broken".to_string()),
            (true, String::new()),
        ]);
        let verdict = verify(&runner, Path::new("/scratch"), dir.path(), &proposal);
        assert!(matches!(verdict, Verdict::StillFailing { .. }));
        assert!(verdict.reason().unwrap().contains("still broken"));
    }

    #[test]
    fn a_patch_that_does_not_apply_is_rejected_early() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = temp_proposal(dir.path());
        let runner = Scripted::new(vec![(false, "error: patch does not apply".to_string())]);
        let verdict = verify(&runner, Path::new("/scratch"), dir.path(), &proposal);
        assert!(matches!(verdict, Verdict::PatchRejected { .. }));
        // No test was run: the patch never applied.
        assert_eq!(runner.calls().len(), 1);
    }

    #[test]
    fn a_proposal_without_a_patch_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("proposal.json"),
            r#"{"signature":"s","summary":"x","package":"cordis-agent","test":"t","patch":"missing.diff"}"#,
        )
        .unwrap();
        let error = Proposal::load(dir.path()).expect_err("must be refused");
        assert!(error.contains("is missing"), "{error}");
    }

    #[test]
    fn a_proposal_without_a_signature_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("patch.diff"), "x").unwrap();
        std::fs::write(
            dir.path().join("proposal.json"),
            r#"{"signature":"","summary":"x","package":"cordis-agent","test":"t","patch":"patch.diff"}"#,
        )
        .unwrap();
        let error = Proposal::load(dir.path()).expect_err("must be refused");
        assert!(error.contains("no signature"), "{error}");
    }

    #[test]
    fn a_proposal_without_a_package_or_test_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("patch.diff"), "x").unwrap();
        std::fs::write(
            dir.path().join("proposal.json"),
            r#"{"signature":"s","summary":"x","package":"","test":"t","patch":"patch.diff"}"#,
        )
        .unwrap();
        let error = Proposal::load(dir.path()).expect_err("must be refused");
        assert!(error.contains("no package or test"), "{error}");
    }

    #[test]
    fn malformed_json_is_reported_with_the_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("proposal.json"), "{not json").unwrap();
        let error = Proposal::load(dir.path()).expect_err("must be refused");
        assert!(error.contains("proposal.json"), "{error}");
    }

    #[test]
    fn a_verified_verdict_carries_no_reason() {
        let verdict = Verdict::Verified {
            test: "t".to_string(),
        };
        assert!(verdict.is_verified());
        assert!(verdict.reason().is_none());
    }

    // The success half of `Proposal::load` and `validate`: every other test
    // above only ever sees a refusal.
    #[test]
    fn a_complete_proposal_loads_and_keeps_its_patch_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("patch.diff"), "diff --git a b\n").unwrap();
        std::fs::write(
            dir.path().join("proposal.json"),
            r#"{"signature":"s","summary":"x","package":"cordis-agent","test":"t","patch":"patch.diff"}"#,
        )
        .unwrap();
        let proposal = Proposal::load(dir.path()).expect("a complete proposal loads");
        assert_eq!(proposal.signature, "s");
        assert_eq!(proposal.test, "t");
        assert_eq!(
            proposal.patch_path(dir.path()),
            dir.path().join("patch.diff")
        );
    }

    // `ShellRunner` is the only `Runner` that touches a real process, so its
    // two arms need a real command each: one that spawns, one that does not.
    #[test]
    fn the_shell_runner_reports_a_real_command_and_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let (succeeded, output) = ShellRunner.run(dir.path(), "echo", &["hello"]);
        assert!(succeeded, "echo should succeed: {output}");
        assert!(output.contains("hello"), "{output}");
        let (succeeded, output) = ShellRunner.run(dir.path(), "cordis-no-such-program-zzz", &[]);
        assert!(!succeeded);
        assert!(output.contains("cordis-no-such-program-zzz"), "{output}");
    }

    // `Verdict::reason` for the arm the other cases never select.
    #[test]
    fn a_patch_rejected_verdict_reports_the_git_output() {
        let verdict = Verdict::PatchRejected {
            reason: "error: patch does not apply".to_string(),
        };
        assert!(!verdict.is_verified());
        assert_eq!(verdict.reason(), Some("error: patch does not apply"));
    }

    // The patch applied to a clean checkout but not to the tree the failing
    // test left behind: rejected, and the test is not run a second time.
    #[test]
    fn a_patch_that_stops_applying_after_the_test_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = temp_proposal(dir.path());
        let runner = Scripted::new(vec![
            (true, String::new()),
            (false, "FAILED".to_string()),
            (false, "error: patch does not apply".to_string()),
        ]);
        let verdict = verify(&runner, Path::new("/scratch"), dir.path(), &proposal);
        assert_eq!(
            verdict,
            Verdict::PatchRejected {
                reason: "error: patch does not apply".to_string(),
            }
        );
        assert_eq!(
            runner.calls().len(),
            3,
            "the test must not run again after a failed apply"
        );
    }
}
