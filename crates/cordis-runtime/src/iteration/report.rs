//! How an iteration result is reported to the agent.
//!
//! Kept out of the tool layer so the vocabulary is in one place: a rolled-back
//! iteration must not read as a success (that mistake made the agent describe
//! files it never changed).

use crate::kernel::plugin_iteration::{CanaryVerdict, PluginIterationFinalVerdict};
use serde_json::Value;

/// Shape the iteration result for the agent.
///
/// `ok` is only true when the change was actually **promoted**: a rolled-back
/// iteration restored the workspace, so reporting success would leave the agent
/// describing files it never changed.
pub fn iteration_result_json(result: &crate::host::KernelPluginIterationResult) -> Value {
    let final_verdict = result.final_verdict;
    let promoted = final_verdict == PluginIterationFinalVerdict::Promoted;
    let canary_verdict = result.canary.as_ref().map(|report| report.verdict);
    let mut value = serde_json::json!({
        "ok": promoted,
        "iteration_id": result.iteration_id,
        "verdict": format!("{final_verdict:?}"),
        "final_verdict": format!("{final_verdict:?}"),
        "verifier_verdict": result.verifier_verdict.map(|verdict| format!("{verdict:?}")),
        "canary_verdict": canary_verdict.map(|verdict| format!("{verdict:?}")),
        "changed_paths": result.changed_paths,
        "summary": result.summary,
    });
    if !promoted {
        let reason = result
            .blocked_reason
            .clone()
            .unwrap_or_else(|| "no reason recorded".to_string());
        let hint = match (final_verdict, canary_verdict) {
            (PluginIterationFinalVerdict::RolledBack, Some(CanaryVerdict::Fail)) => {
                "the canary replayed recorded invocations against the new build and their \
                 responses changed, so the workspace was restored. Changing behavior \
                 intentionally needs operator approval; ask them to approve this iteration."
            }
            (PluginIterationFinalVerdict::RolledBack, _) => {
                "verification failed, so the workspace was restored (your edits are gone)."
            }
            (PluginIterationFinalVerdict::Blocked, _) => {
                "the iteration is staged as a candidate and is waiting for operator approval; \
                 the working tree still holds your edits, but they are NOT live."
            }
            (PluginIterationFinalVerdict::InfrastructureFailure, _) => {
                "an infrastructure failure (disk, IO) aborted the iteration."
            }
            (PluginIterationFinalVerdict::Promoted, _) => "",
        };
        value["error"] = serde_json::json!(format!(
            "iteration did not promote ({final_verdict:?}): {reason}. {hint} \
             Do NOT report the change as done."
        ));
        value["rolled_back"] =
            serde_json::json!(final_verdict == PluginIterationFinalVerdict::RolledBack);
    }
    value
}

/// Approval gate for `agent_approve_iteration`.
///
/// The decision is deliberately not made here: `can_approve_iteration` is
/// produced by the channel plugin (operator allowlist / room owner / admin).
/// The kernel only enforces what it was told.
pub fn approve_iteration_permission(
    caller: Option<crate::agent::SessionCaller>,
) -> Result<(), &'static str> {
    match caller {
        None => Err("approve_iteration: unknown agent session (no caller identity)"),
        Some(caller) if !caller.can_approve_iteration => Err(
            "approve_iteration: this speaker may not approve iterations. The channel must \
             report the capability (operator allowlist or the room's owner/admin); ask an \
             operator to approve it instead.",
        ),
        Some(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{approve_iteration_permission, iteration_result_json};
    use crate::agent::SessionCaller;
    use crate::host::KernelPluginIterationResult;
    use crate::kernel::plugin_iteration::{
        CanaryReport, CanaryVerdict, PluginEditPlan, PluginIterationFinalVerdict, VerifierVerdict,
    };

    fn result(final_verdict: PluginIterationFinalVerdict) -> KernelPluginIterationResult {
        KernelPluginIterationResult {
            iteration_id: "it-1".to_string(),
            issue_id: "issue-1".to_string(),
            root_plugin_path: "gacha".to_string(),
            target_plugin_paths: vec!["gacha".to_string()],
            source: None,
            summary: "remove the mapping".to_string(),
            agent_session_id: None,
            tool_execution_summary: None,
            derived_edit_plan: PluginEditPlan {
                issue_id: "issue-1".to_string(),
                patch_id: "patch-1".to_string(),
                summary: "remove the mapping".to_string(),
                operations: Vec::new(),
            },
            transcript_excerpt: Vec::new(),
            changed_paths: vec!["plugins/gacha/src/lib.rs".to_string()],
            rebuilt_artifacts: Vec::new(),
            candidate: None,
            verification: None,
            verifier_verdict: Some(VerifierVerdict::Pass),
            canary: Some(CanaryReport {
                verdict: CanaryVerdict::Fail,
                mode: "replay".to_string(),
                plugin_path: Some("gacha".to_string()),
                node_id: Some("gacha_entry".to_string()),
                payload: None,
                expected_response: None,
                actual_response: None,
                message: "replay mismatch".to_string(),
            }),
            final_verdict,
            blocked_reason: Some("canary replay mismatch".to_string()),
            net_output: crate::execution::engine::ExecutionOutput {
                execution_id: "it-1".to_string(),
                order: Vec::new(),
                outcomes: std::collections::BTreeMap::new(),
                keyed_outcomes: std::collections::BTreeMap::new(),
                metrics: crate::execution::engine::ExecutionMetrics::default(),
            },
        }
    }

    #[test]
    fn rolled_back_iteration_is_reported_as_failure_with_the_reason() {
        let value = iteration_result_json(&result(PluginIterationFinalVerdict::RolledBack));
        assert_eq!(value["ok"], false);
        assert_eq!(value["verdict"], "RolledBack");
        assert_eq!(value["verifier_verdict"], "Pass");
        assert_eq!(value["canary_verdict"], "Fail");
        assert_eq!(value["rolled_back"], true);
        let error = value["error"].as_str().expect("error");
        assert!(error.contains("canary"), "error: {error}");
        assert!(error.contains("operator approval"), "error: {error}");
    }

    #[test]
    fn promoted_iteration_reports_success_and_no_error() {
        let value = iteration_result_json(&result(PluginIterationFinalVerdict::Promoted));
        assert_eq!(value["ok"], true);
        assert!(value.get("error").is_none());
    }

    #[test]
    fn blocked_iteration_explains_it_awaits_approval() {
        let value = iteration_result_json(&result(PluginIterationFinalVerdict::Blocked));
        assert_eq!(value["ok"], false);
        let error = value["error"].as_str().expect("error");
        assert!(error.contains("approval"), "error: {error}");
        assert_eq!(value["rolled_back"], false);
    }

    #[test]
    fn approval_requires_the_channel_to_grant_the_capability() {
        let denied = SessionCaller {
            is_group: true,
            is_admin: false,
            can_approve_iteration: false,
        };
        assert!(approve_iteration_permission(Some(denied)).is_err());
        let allowed = SessionCaller {
            can_approve_iteration: true,
            ..denied
        };
        assert!(approve_iteration_permission(Some(allowed)).is_ok());
        assert!(approve_iteration_permission(None).is_err());
    }
}
