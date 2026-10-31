use super::*;

/// Why a tested change still waits: nothing replayed.
const BLOCKED_NO_CANARY_EVIDENCE: &str = "this change has no canary evidence: no \
recent invocation replayed cleanly and no verifier node is declared, so the \
behaviour is unproven. Ask an operator to approve this iteration (approve_iteration).";

impl RuntimeHost {
    pub(super) fn start_plugin_iteration_agent_session(
        &self,
        prepared: PreparedPluginIteration,
        context_paths: PluginIterationContextPaths,
    ) -> Result<String, RuntimeError> {
        self.start_iteration_session(prepared, context_paths)
    }

    pub(super) fn plugin_iteration_agent_snapshot(
        &self,
        session_id: &str,
    ) -> Result<PluginIterationAgentRun, RuntimeError> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let managed = guard
            .get(session_id)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        let ManagedAgentState::PluginIteration(state) = &managed.state else {
            return Err(RuntimeError::InvalidArgument {
                message: format!("agent session {session_id} is not a plugin iteration session"),
            });
        };
        Ok(PluginIterationAgentRun {
            llm_driven: true,
            session_id: Some(session_id.to_string()),
            tool_summary: None,
            transcript_excerpt: Vec::new(),
            recorded_summary: state.recorded_summary.clone(),
            requires_approval: state.requires_approval,
            changed_paths: state.derived_edit_plan().changed_paths(),
            rollback: state.rollback.clone(),
            derived_edit_plan: state.derived_edit_plan(),
        })
    }

    pub fn iterate_plugins(
        &self,
        request: KernelPluginIterationRequest,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let snapshot = self.current_snapshot();
        let prepared = self
            .kernel
            .begin_plugin_iteration(snapshot.as_ref(), &request)?;
        let iteration_id = prepared.iteration_id.clone();

        // Wrap the entire iteration body in a panic guard: if any step panics
        // (e.g. inside the agent loop, rebuild, or verification), we catch it
        // and perform emergency rollback instead of crashing the server.
        let result: std::thread::Result<Result<KernelPluginIterationResult, RuntimeError>> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // `assert!(!injected, msg)` panics with exactly `msg` (a `&str`
                // payload), same as the previous `if injected { panic!(msg) }`,
                // but without a not-taken block whose closing brace llvm-cov
                // can never reach.
                #[cfg(test)]
                let injected =
                    TEST_ITERATION_PANIC_INJECTION.swap(false, std::sync::atomic::Ordering::SeqCst);
                #[cfg(test)]
                assert!(!injected, "test panic injection: plugin iteration");
                let mut state = PluginIterationRunState::new(prepared.clone());

                // Step 1: Run the agent loop — the agent freely decides what to do.
                match self.run_plugin_iteration_agent(&state.prepared) {
                    Ok(agent) => {
                        state.changed_paths = agent.changed_paths.clone();
                        state.rollback = Some(agent.rollback.clone());
                        if agent.recorded_summary.is_none() {
                            let sid = agent.session_id.as_deref().unwrap_or("unknown-session");
                            let err = missing_summary_error(sid);
                            self.fail_stage(&mut state, "agent", &err);
                        }
                        state.agent = Some(agent);
                    }
                    Err(err) => {
                        self.fail_stage(&mut state, "agent", &err);
                    }
                }

                // Step 2: Persist the rollback journal.
                if state.stage_error.is_none() {
                    // Both outcomes funnel into one `fail_stage` call site.
                    // `state.rollback` is `Some` on every path Step 1 can take
                    // (the edit-plan branch starts from
                    // `PluginEditRollback::empty(..)` and the agent branch reads
                    // one out of the session snapshot), so the fallback is
                    // purely defensive — kept as a named, unit-tested
                    // constructor on the same line as the covered
                    // `unwrap_or_else` call rather than as its own arm.
                    let journal_error = state
                        .rollback
                        .as_ref()
                        .map(|rollback| {
                            // 注入的 ENOSPC 与真实 persist 走同一条表达式：
                            // `Option::map_or_else` 没有"未走到的臂"，而
                            // `if { return .. }` 的收尾在 if 不成立时不执行，
                            // 会在 100% 行覆盖门槛下留一行永久缺口。
                            injected_journal_enospc(&plugin_iteration_journal_path(
                                &self.snapshot_root,
                            ))
                            .map_or_else(
                                || {
                                    rollback.persist_journal(
                                        &plugin_iteration_journal_path(&self.snapshot_root),
                                        &state.prepared.iteration_id,
                                    )
                                },
                                Err,
                            )
                        })
                        .unwrap_or_else(|| Err(plugin_iteration_missing_rollback_error()))
                        .err();
                    for err in journal_error.iter() {
                        self.fail_stage(&mut state, "edit", err);
                    }
                }

                // Step 3: Rebuild the target plugin only — UNLESS the iteration
                // touched any Cargo.toml. A manifest change is structural: a
                // scaffolded child plugin (new crate + parent children entry)
                // exists neither in the cargo build graph of `-p <target>` nor
                // in artifacts/index.json, so a target-only rebuild would
                // silently drop it and the subsequent candidate load could
                // never register it (the `45febba` regression). Manifest-touching
                // iterations take the full `rebuild_fixture_artifacts` path,
                // which re-resolves the plugin graph and regenerates the index.
                if state.stage_error.is_none() {
                    let manifest_changed = state
                        .changed_paths
                        .iter()
                        .any(|path| path.ends_with("Cargo.toml"));
                    let plugin_path = if manifest_changed {
                        "/".to_string()
                    } else {
                        format!("/{}", state.prepared.root_plugin_path)
                    };
                    // P0-8: capture the current `.so` before rebuild so rollback
                    // reverts BOTH source and compiled artifact. Otherwise
                    // source-code rollback runs but the new `.so` stays on disk,
                    // producing silent behaviour drift on the next run.
                    //
                    // Backup → journal re-persist → rebuild is one `and_then`
                    // chain, so all three fallible steps share a single
                    // `fail_stage("rebuild", ..)` call site and each step still
                    // short-circuits the ones after it exactly as the previous
                    // `if state.stage_error.is_none()` gates did.
                    let journal = plugin_iteration_journal_path(&self.snapshot_root);
                    let iteration_id = state.prepared.iteration_id.clone();
                    let root_plugin_path = state.prepared.root_plugin_path.clone();
                    let rebuilt = state
                        .rollback
                        .as_mut()
                        .map(|rollback| {
                            backup_artifacts_into_rollback(
                                &self.fixtures_root,
                                &root_plugin_path,
                                rollback,
                            )
                            // Re-persist the journal now that artifact backups
                            // landed in the rollback; otherwise a crash after
                            // rebuild would leave the artifact rollback-able
                            // only in memory.
                            .and_then(|()| rollback.persist_journal(&journal, &iteration_id))
                        })
                        .unwrap_or_else(|| Err(plugin_iteration_missing_rollback_error()))
                        .and_then(|()| {
                            let builder = plugin_builder().ok_or_else(missing_builder_error)?;
                            builder.rebuild_plugin(&self.fixtures_root, &plugin_path)
                        });
                    match rebuilt {
                        Ok(artifacts) => {
                            state.rebuilt_artifacts = artifacts;
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "rebuild", &err);
                        }
                    }
                }

                // Step 4: Stage candidate snapshot.
                if state.stage_error.is_none() {
                    match self.reload_candidate() {
                        Ok(candidate) => {
                            state.candidate = Some(candidate);
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "stage_candidate", &err);
                        }
                    }
                }

                // Step 5: Verify.
                if state.stage_error.is_none() {
                    match self.verify_plugin_iteration(&state) {
                        Ok(report) => {
                            let verdict =
                                if report.input.tests_passed && report.input.safety_checks_passed {
                                    VerifierVerdict::Pass
                                } else {
                                    VerifierVerdict::Fail
                                };
                            if verdict == VerifierVerdict::Fail {
                                self.kernel.observe_plugin_issue(
                                KernelPluginIssueSource::VerifierFailure,
                                state.prepared.root_plugin_path.clone(),
                                format!(
                                    "plugin verifier failed for {}: tests_passed={}, safety_checks_passed={}",
                                    state.prepared.root_plugin_path,
                                    report.input.tests_passed,
                                    report.input.safety_checks_passed,
                                ),
                            );
                            }
                            state.verification = Some(report);
                            state.verifier_verdict = Some(verdict);
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "verify", &err);
                        }
                    }
                }

                // Step 6: Canary replay.
                if state.stage_error.is_none() {
                    match self.run_plugin_canary(&state) {
                        Ok(report) => {
                            if report.verdict == CanaryVerdict::Fail {
                                self.kernel.observe_plugin_issue(
                                    KernelPluginIssueSource::CanaryFailure,
                                    state.prepared.root_plugin_path.clone(),
                                    format!(
                                        "plugin canary failed for {}: {}",
                                        state.prepared.root_plugin_path, report.message
                                    ),
                                );
                            }
                            state.canary = Some(report);
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "canary", &err);
                        }
                    }
                }

                // Step 7: Promote or rollback (always runs, even after stage errors).
                let _final_verdict = self.finalize_plugin_iteration(&mut state)?;

                let net_output = ExecutionOutput {
                    execution_id: format!("plugin-iteration-{iteration_id}"),
                    order: vec![
                        "plugin_iteration::agent".to_string(),
                        "plugin_iteration::edit".to_string(),
                        "plugin_iteration::rebuild".to_string(),
                        "plugin_iteration::stage_candidate".to_string(),
                        "plugin_iteration::verify".to_string(),
                        "plugin_iteration::canary".to_string(),
                        "plugin_iteration::promote_or_rollback".to_string(),
                    ],
                    outcomes: std::collections::BTreeMap::new(),
                    keyed_outcomes: std::collections::BTreeMap::new(),
                    metrics: crate::execution::engine::ExecutionMetrics::default(),
                };
                state.into_result(net_output)
            }));

        self.kernel.finish_plugin_iteration(&iteration_id);

        match result {
            // No panic: record the outcome iff the body produced one, then
            // clean up and hand the body's own `Result` back unchanged. Written
            // as one arm over `Result::iter()` rather than separate `Ok(Ok)` /
            // `Ok(Err)` arms because the cleanup and the return value are
            // identical in both — only the `record_plugin_iteration_outcome`
            // call is conditional, and it is conditional on exactly the same
            // `Ok`-ness the iterator encodes.
            Ok(outcome) => {
                for result in outcome.iter() {
                    self.kernel.record_plugin_iteration_outcome(result);
                }
                self.cleanup_retired_snapshots();
                outcome
            }
            Err(panic_payload) => {
                // Emergency cleanup: restore workspace files, rollback candidate,
                // and clear journal so the system stays in a consistent state.
                let _ = restore_plugin_iteration_workspace(
                    &self.fixtures_root,
                    &self.snapshot_root,
                    None,
                );
                if self.candidate_snapshot().is_some() {
                    let _ = self.rollback_candidate();
                }
                self.cleanup_retired_snapshots();
                Err(plugin_iteration_panic_error(&panic_payload))
            }
        }
    }

    pub(super) fn run_plugin_iteration_agent(
        &self,
        prepared: &PreparedPluginIteration,
    ) -> Result<PluginIterationAgentRun, RuntimeError> {
        if let Some(plan) = &prepared.edit_plan {
            self.kernel
                .plugin_iteration_policy
                .validate_plan(&prepared.allowed_plugin_roots, plan)?;
            let mut rollback = PluginEditRollback::empty(&self.fixtures_root);
            let executor = PluginEditExecutor::new(&self.fixtures_root);
            for (idx, operation) in plan.operations.iter().enumerate() {
                let single = PluginEditPlan {
                    issue_id: plan.issue_id.clone(),
                    patch_id: format!("{}-manual-{idx}", prepared.iteration_id),
                    summary: plan.summary.clone(),
                    operations: vec![operation.clone()],
                };
                // Bound out of the `?` so each fallible call is a single line:
                // a `?` spanning several lines leaves llvm-cov a zero-hit
                // region on the continuation line no test can reach.
                let policy = &self.kernel.plugin_iteration_policy;
                let roots = &prepared.allowed_plugin_roots;
                let executed = executor.execute(policy, roots, &single);
                let (_, op_rollback) = executed?;
                rollback.absorb(op_rollback)?;
            }
            return Ok(PluginIterationAgentRun {
                llm_driven: false,
                session_id: None,
                tool_summary: None,
                transcript_excerpt: Vec::new(),
                recorded_summary: Some(plan.summary.clone()),
                // An edit-plan iteration goes through the same test policy as
                // an agent one; it does not get a silent bypass.
                requires_approval: !test_changes(&plan.operations).modified.is_empty(),
                changed_paths: plan.changed_paths(),
                rollback,
                derived_edit_plan: plan.clone(),
            });
        }
        let collected = collect_plugin_context_paths(
            &self.fixtures_root,
            &prepared.root_plugin_path,
            &prepared.target_plugin_paths,
        );
        let context_paths = collected?;
        let session_id =
            self.start_plugin_iteration_agent_session(prepared.clone(), context_paths)?;
        let input = prepared
            .instruction
            .clone()
            .unwrap_or_else(|| prepared.summary.clone());
        if let Err(err) = self.agent_send(&session_id, &input) {
            let transcript_excerpt = self
                .agent_transcript(&session_id)
                .map(|transcript| transcript_excerpt(&transcript, 12))
                .unwrap_or_default();
            let tool_summary = self
                .agent_status(&session_id)
                .ok()
                .and_then(|_| self.iteration_session_tool_summary(&session_id));
            // H2: excerpt/tool_summary captured above; terminate the failed
            // session and reclaim its map entries before returning the error.
            self.drop_session(&session_id);
            return Err(enrich_plugin_iteration_agent_error(
                err,
                &session_id,
                tool_summary.as_ref(),
                &transcript_excerpt,
            ));
        }
        let run = self.plugin_iteration_agent_snapshot(&session_id)?;
        let transcript = self.agent_transcript(&session_id)?;
        let transcript_excerpt = transcript_excerpt(&transcript, 12);
        let tool_summary = self
            .agent_status(&session_id)
            .ok()
            .and_then(|_| self.iteration_session_tool_summary(&session_id));
        // H2: transcript/snapshot are fully read above; terminate the session
        // and reclaim its map entries before returning success.
        self.drop_session(&session_id);
        Ok(PluginIterationAgentRun {
            llm_driven: true,
            session_id: Some(session_id),
            tool_summary,
            transcript_excerpt,
            ..run
        })
    }

    pub(super) fn verify_plugin_iteration(
        &self,
        state: &PluginIterationRunState,
    ) -> Result<VerificationReport, RuntimeError> {
        let tests_command = state
            .tests_command
            .clone()
            .or_else(|| state.prepared.tests_command.clone())
            .or_else(|| Some("cargo test --quiet --manifest-path plugins/Cargo.toml".to_string()));
        let safety_command = state
            .safety_command
            .clone()
            .or_else(|| state.prepared.safety_command.clone());
        // P0-4: `plugin:` commands must exercise the staged candidate. Wire an
        // invoker closure that dispatches into the candidate snapshot rather
        // than loading a fresh live invoker inside the verifier.
        let candidate_invoker = |plugin_path: &str,
                                 node_id: &str,
                                 payload: String|
         -> Result<PluginResponse, RuntimeError> {
            self.invoke_candidate(plugin_path, node_id, payload)
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&candidate_invoker),
            command_timeout: None,
        };
        // Bound out of the `?` (single-line fallible expression) so the error
        // edge shares a line with the call it guards.
        let verified = CommandVerifier::verify_with_options(
            &self.fixtures_root,
            state.prepared.verify_profile,
            tests_command.as_deref(),
            safety_command.as_deref(),
            state.prepared.quality_score,
            &options,
        );
        verified
    }

    pub(super) fn run_plugin_canary(
        &self,
        state: &PluginIterationRunState,
    ) -> Result<CanaryReport, RuntimeError> {
        let target_plugins = state
            .prepared
            .target_plugin_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let samples = self
            .invocation_samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        let current_snapshot = self.current_snapshot();
        for sample in samples {
            if !target_plugins.contains(&sample.plugin_path) {
                continue;
            }
            // Skip invocations that mutate state (replaying a pull would
            // consume pity twice) and probe the rest for determinism: a node
            // whose response changes between two runs of the *same* build
            // cannot serve as evidence, and failing canary on it would revert
            // every iteration that touches a random plugin.
            let mutating =
                node_declares_side_effects(&current_snapshot, &sample.plugin_path, &sample.node_id);
            if mutating {
                continue;
            }
            // Both fallible steps are bound out of their `?` onto a single line
            // each: a `?` whose call spans several lines leaves llvm-cov a
            // zero-hit region on the continuation lines.
            let encoded = serde_json::to_string(&sample.payload)
                .map_err(|err| canary_payload_serialize_error(&err));
            let payload = encoded?;
            let first = self
                .invoke_candidate(&sample.plugin_path, &sample.node_id, payload.clone())
                .map(|response| parse_response_payload(&response.payload))?;
            let second = self
                .invoke_candidate(&sample.plugin_path, &sample.node_id, payload.clone())
                .map(|response| parse_response_payload(&response.payload))?;
            let Some(verdict) =
                crate::iteration::policy::canary_probe_verdict(&first, &second, &sample.response)
            else {
                // Nondeterministic (random / clock dependent): no evidence.
                continue;
            };
            let actual = first;
            return Ok(CanaryReport {
                verdict,
                mode: "recent_successful_invocation_replay".to_string(),
                plugin_path: Some(sample.plugin_path),
                node_id: Some(sample.node_id),
                payload: Some(sample.payload),
                expected_response: Some(sample.response),
                actual_response: Some(actual.clone()),
                message: if verdict == CanaryVerdict::Pass {
                    "candidate replay matched current response".to_string()
                } else {
                    "candidate replay response diverged from current response".to_string()
                },
            });
        }

        for candidate in self.candidate_snapshot().into_iter() {
            for plugin_path in &state.prepared.target_plugin_paths {
                // Registry miss and a plugin without docs are both "no
                // evidence here, try the next target". Chained through
                // `and_then` so the two skips share one `for`-over-`Option`
                // gate instead of two `let ... else { continue }` guards whose
                // not-taken edges sit on their own lines.
                let declared = candidate
                    .plugin_registry()
                    .get(plugin_path)
                    .and_then(|plugin| plugin.docs)
                    .and_then(|docs| {
                        docs.nodes
                            .into_iter()
                            .find(|node| node.id.contains("canary") || node.id.contains("verify"))
                    });
                if let Some(node) = declared {
                    let invoked = self.invoke_candidate(plugin_path, &node.id, "{}".to_string());
                    let response = invoked?;
                    let actual = parse_response_payload(&response.payload);
                    return Ok(CanaryReport {
                        verdict: CanaryVerdict::Pass,
                        mode: "declared_plugin_verifier_node".to_string(),
                        plugin_path: Some(plugin_path.clone()),
                        node_id: Some(node.id.clone()),
                        payload: Some(Value::Object(Map::new())),
                        expected_response: None,
                        actual_response: Some(actual),
                        message: "plugin-declared canary/verifier node completed successfully"
                            .to_string(),
                    });
                }
            }
        }

        Ok(CanaryReport {
            verdict: CanaryVerdict::Partial,
            mode: "no_canary_evidence".to_string(),
            plugin_path: None,
            node_id: None,
            payload: None,
            expected_response: None,
            actual_response: None,
            message: "no recent successful invocation or declared canary/verifier node found"
                .to_string(),
        })
    }

    pub(super) fn finalize_plugin_iteration(
        &self,
        state: &mut PluginIterationRunState,
    ) -> Result<PluginIterationFinalVerdict, RuntimeError> {
        // R3: 本迭代 Step 4 暂存候选时记录的身份。promote / rollback 用它
        // 校验 staged 候选没被并发 `reload_candidate` 替换；候选从未暂存
        // （stage 失败）时为 None → 不校验（回滚路径仍清理任何残留候选）。
        let staged_candidate_id = state
            .candidate
            .as_ref()
            .map(|candidate| candidate.candidate_snapshot_id.as_str());
        if let Some(stage_error) = state.stage_error.clone() {
            let candidate_rollback = self.rollback_candidate_if_staged(staged_candidate_id);
            let workspace_restore = restore_plugin_iteration_workspace(
                &self.fixtures_root,
                &self.snapshot_root,
                state.rollback.as_ref(),
            )
            .map(|_| ());
            state.blocked_reason = Some(aggregate_rollback_failure(
                stage_error,
                candidate_rollback,
                workspace_restore,
            ));
            // 基础设施故障（磁盘满等）与"验证失败"分开定级。回滚照常先跑
            // （上面已执行），回滚自身失败也不改变这个判定——错误文本已由
            // `aggregate_rollback_failure` 拼进 blocked_reason，journal 留在盘上
            // 供人工恢复。
            let verdict = if state.stage_error_is_infrastructure {
                PluginIterationFinalVerdict::InfrastructureFailure
            } else {
                PluginIterationFinalVerdict::RolledBack
            };
            state.final_verdict = Some(verdict);
            return Ok(verdict);
        }
        let verifier_verdict = state.verifier_verdict.unwrap_or(VerifierVerdict::Partial);

        // P0-3: guard against concurrent source-tree mutation between verify
        // and promote. The verifier hashes the tree; here we rehash and
        // compare. Any drift → downgrade the verdict to a rollback so we
        // don't promote code we never verified.
        let mut effective_verifier = verifier_verdict;
        let source_drift_reason = detect_plugin_source_drift(
            verifier_verdict,
            state
                .verification
                .as_ref()
                .and_then(|r| r.source_tree_hash.as_deref()),
            state
                .verification
                .as_ref()
                .and_then(|r| r.hash_error.as_deref()),
            || hash_source_tree(&self.fixtures_root),
        );
        if source_drift_reason.is_some() {
            effective_verifier = VerifierVerdict::Fail;
        }
        if let Some(reason) = source_drift_reason.as_deref() {
            self.kernel.observe_plugin_issue(
                KernelPluginIssueSource::VerifierFailure,
                state.prepared.root_plugin_path.clone(),
                format!(
                    "plugin verifier TOCTOU guard tripped for {}: {}",
                    state.prepared.root_plugin_path, reason
                ),
            );
        }

        // Tests are the contract: the decision lives in `iteration::policy`.
        // This function only carries out the side effects of that decision.
        // A canary *failure* is positive evidence of breakage: the replay
        // diverged, so the change cannot land no matter what the tests say.
        let canary_failed = state
            .canary
            .as_ref()
            .is_some_and(|report| report.verdict == CanaryVerdict::Fail);
        if canary_failed {
            effective_verifier = VerifierVerdict::Fail;
        }

        // A blind edit (no agent behind it) needs replayable evidence: the
        // canary proves the change still behaves. An agent-driven change
        // carries a transcript instead, and a brand new plugin has nothing to
        // replay at all.
        let canary_unproven = state.agent.as_ref().is_none_or(|run| !run.llm_driven)
            && state
                .canary
                .as_ref()
                .is_some_and(|report| report.verdict == CanaryVerdict::Partial);
        let decision = promotion_verdict(
            effective_verifier,
            canary_unproven
                || state
                    .agent
                    .as_ref()
                    .is_some_and(|agent| agent.requires_approval),
            state.prepared.manual_approved,
        );
        let final_verdict = match decision {
            PluginIterationFinalVerdict::Promoted => {
                // P2-25: promote failure used to `?` propagate straight out,
                // leaving the workspace modified with journal on disk but no
                // rollback trigger. On failure, roll back + restore explicitly.
                match self.promote_candidate_matching(staged_candidate_id) {
                    Ok(_) => PluginIterationFinalVerdict::Promoted,
                    Err(err) => {
                        let candidate_rollback =
                            self.rollback_candidate_if_staged(staged_candidate_id);
                        let workspace_restore = restore_plugin_iteration_workspace(
                            &self.fixtures_root,
                            &self.snapshot_root,
                            state.rollback.as_ref(),
                        )
                        .map(|_| ());
                        state.blocked_reason = Some(aggregate_rollback_failure(
                            format!("promote failed: {err}"),
                            candidate_rollback,
                            workspace_restore,
                        ));
                        return Err(err);
                    }
                }
            }
            PluginIterationFinalVerdict::Blocked => {
                // Keep the candidate alive: `approve_iteration` promotes it.
                state.blocked_reason = Some(if canary_unproven {
                    BLOCKED_NO_CANARY_EVIDENCE.to_string()
                } else {
                    crate::iteration::policy::BLOCKED_EDITS_EXISTING_TEST.to_string()
                });
                PluginIterationFinalVerdict::Blocked
            }
            _ => {
                if let Some(reason) = source_drift_reason.as_ref() {
                    state.blocked_reason = Some(reason.clone());
                }
                let candidate_error = self
                    .rollback_candidate_if_staged(staged_candidate_id)
                    .and_then(|outcome| outcome.err())
                    .map(|err| verdict_rollback_partial_cleanup_reason(&err));
                let restored = restore_plugin_iteration_workspace(
                    &self.fixtures_root,
                    &self.snapshot_root,
                    state.rollback.as_ref(),
                );
                restored?;
                state.blocked_reason = candidate_error.or(state.blocked_reason.take());
                PluginIterationFinalVerdict::RolledBack
            }
        };
        state.final_verdict = Some(final_verdict);
        Ok(final_verdict)
    }

    pub(crate) fn record_invocation_sample(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: &str,
        response_payload: &str,
    ) {
        let payload =
            serde_json::from_str(payload).unwrap_or_else(|_| Value::String(payload.to_string()));
        let response = parse_response_payload(response_payload);
        let mut samples = self
            .invocation_samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        samples.push_front(InvocationSample {
            plugin_path: plugin_path.to_string(),
            node_id: node_id.to_string(),
            payload,
            response,
            observed_at_ms: now_ms(),
        });
        while samples.len() > 64 {
            samples.pop_back();
        }
    }

    pub(super) fn fail_stage(
        &self,
        state: &mut PluginIterationRunState,
        stage: &str,
        err: &RuntimeError,
    ) {
        self.observe_plugin_iteration_failure(&state.prepared, stage, err);
        state.stage_error = Some(err.to_string());
        // 同时记下这次失败是否为基础设施故障（磁盘满 / 配额耗尽）。所有 stage
        // 的失败都经本函数，故此处是唯一需要标记的地方；收尾时据此给
        // `InfrastructureFailure` 而不是把 ENOSPC 误报成"验证失败"。
        state.stage_error_is_infrastructure = err.is_infrastructure_failure();
    }

    pub(super) fn observe_plugin_iteration_failure(
        &self,
        prepared: &PreparedPluginIteration,
        stage: &str,
        err: &RuntimeError,
    ) {
        // 基础设施故障（磁盘满等）不是插件的问题：此前 rebuild /
        // stage_candidate 阶段的任何错误都被记成 LoadFailure 归咎于插件，
        // 于 ENOSPC 时会凭空产生一条"插件加载失败"的 kernel issue。
        if err.is_infrastructure_failure() {
            return;
        }
        let source = match err {
            RuntimeError::PluginIterationPolicyBlocked { .. } => {
                Some(KernelPluginIssueSource::PolicyBlocked)
            }
            _ if matches!(stage, "rebuild" | "stage_candidate") => {
                Some(KernelPluginIssueSource::LoadFailure)
            }
            _ => None,
        };
        let Some(source) = source else {
            return;
        };
        self.kernel.observe_plugin_issue(
            source,
            prepared.root_plugin_path.clone(),
            format!(
                "plugin iteration {stage} failed for {}: {err}",
                prepared.root_plugin_path
            ),
        );
    }

    pub(crate) fn observe_snapshot_plugin_issues(
        &self,
        snapshot: &RuntimeSnapshot,
        report: &ReloadReport,
        stage: &str,
    ) {
        for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
            let changed_reasons = report
                .changed_plugin_reasons
                .get(&plugin_path)
                .cloned()
                .unwrap_or_default();
            match plugin.load_result {
                PluginLoadResult::Unavailable(reason) => {
                    let source = match reason {
                        PluginUnavailableReason::ContractViolation => {
                            KernelPluginIssueSource::DocsDrift
                        }
                        _ => KernelPluginIssueSource::LoadFailure,
                    };
                    self.kernel.observe_plugin_issue(
                        source,
                        plugin_path.clone(),
                        format!("{stage} observed plugin {plugin_path} unavailable: {reason:?}"),
                    );
                }
                PluginLoadResult::Loaded => {
                    if changed_reasons.iter().any(|reason| {
                        matches!(reason.as_str(), "docs_changed" | "fingerprint_diff_changed")
                    }) {
                        self.kernel.observe_plugin_issue(
                            KernelPluginIssueSource::DocsDrift,
                            plugin_path.clone(),
                            format!(
                                "{stage} detected docs/contract drift for {plugin_path}: {}",
                                changed_reasons.join(", ")
                            ),
                        );
                    }
                }
            }
        }
    }
}
