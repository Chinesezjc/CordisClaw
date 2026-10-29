use super::*;

impl RuntimeHost {
    pub fn set_startup_invocations(&self, invocations: Vec<StartupInvocation>) {
        *self
            .startup_invocations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = invocations;
    }

    pub(crate) fn restart_services_for(&self, plugin_paths: &[String]) {
        let invocations = self
            .startup_invocations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let wanted: Vec<&StartupInvocation> = invocations
            .iter()
            .filter(|invocation| {
                plugin_paths.iter().any(|target| {
                    invocation.plugin_path == *target
                        || invocation.plugin_path.starts_with(&format!("{target}/"))
                })
            })
            .collect();
        for invocation in wanted {
            let outcome = self.invoke(
                &invocation.plugin_path,
                &invocation.node_id,
                invocation.payload.clone(),
            );
            match outcome {
                Ok(response) => eprintln!(
                    "[reload] restarted {}::{} ok={}",
                    invocation.plugin_path, invocation.node_id, response.payload
                ),
                Err(err) => eprintln!(
                    "[reload] restart {}::{} failed: {err}",
                    invocation.plugin_path, invocation.node_id
                ),
            }
        }
    }

    pub fn candidate_snapshot(&self) -> Option<Arc<RuntimeSnapshot>> {
        self.candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|candidate| candidate.snapshot.clone())
    }

    pub fn candidate_status(&self) -> Option<CandidateSnapshotStatus> {
        self.candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|candidate| candidate.status.clone())
    }

    pub fn reload(&self, plugin_path: &str) -> Result<ReloadReport, RuntimeError> {
        let result = if plugin_path == "/" {
            self.reload_internal()
        } else {
            self.reload_subtree(plugin_path)
        };
        match result {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt);
                let snapshot = self.current_snapshot();
                self.observe_snapshot_plugin_issues(snapshot.as_ref(), &report, "reload");
                self.notify_sessions_of_reload(&report);
                Ok(report)
            }
            Err((err, attempt)) => {
                self.record_reload_attempt(*attempt);
                self.observe_reload_error("reload", &err);
                Err(err)
            }
        }
    }

    pub fn reload_with_diagnostics(&self, plugin_path: &str) -> ReloadAttemptReport {
        let result = if plugin_path == "/" {
            self.reload_internal()
        } else {
            self.reload_subtree(plugin_path)
        };
        match result {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt.clone());
                let snapshot = self.current_snapshot();
                self.observe_snapshot_plugin_issues(snapshot.as_ref(), &report, "reload");
                self.notify_sessions_of_reload(&report);
                attempt
            }
            Err((err, attempt)) => {
                let attempt = *attempt;
                self.record_reload_attempt(attempt.clone());
                self.observe_reload_error("reload", &err);
                attempt
            }
        }
    }

    pub(crate) fn reload_subtree(
        &self,
        prefix: &str,
    ) -> Result<(ReloadReport, ReloadAttemptReport), (RuntimeError, Box<ReloadAttemptReport>)> {
        let normalized = prefix.trim_start_matches('/');
        let previous_snapshot = self.current_snapshot();
        let started_at = Instant::now();

        // Collect target plugins (match prefix).
        let targets: Vec<String> = previous_snapshot
            .plugin_registry()
            .iter()
            .map(|(p, _)| p)
            .filter(|p| {
                if normalized.is_empty() {
                    true
                } else {
                    p.as_str() == normalized || p.starts_with(&format!("{}/", normalized))
                }
            })
            .collect();

        if targets.is_empty() {
            // A plugin that was just built is not in the running snapshot yet,
            // so a subtree reload has nothing to match. Fall back to a full
            // reload (which rebuilds the snapshot from the index) instead of
            // reporting a no-op success.
            let artifacts_dir = self.fixtures_root().join("artifacts");
            let index_path = artifacts_dir.join("index.json");
            let known_on_disk = crate::plugin::artifact::load_artifact_index(&index_path)
                .ok()
                .and_then(|index| {
                    crate::plugin::artifact::artifact_index_map(&index)
                        .get(normalized.trim_end_matches('/'))
                        .map(|_| ())
                })
                .is_some();
            if !normalized.is_empty() && known_on_disk {
                eprintln!(
                    "reload_subtree: {normalized} is not in the running snapshot; falling back to a full reload"
                );
                return self.reload_internal();
            }
            let attempt = ReloadAttemptReport {
                status: ReloadAttemptStatus::Reloaded,
                from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                to_snapshot_id: Some(previous_snapshot.snapshot_id().to_string()),
                snapshot_root: self.snapshot_root.display().to_string(),
                staged_artifact_root: String::new(),
                elapsed_ms: started_at.elapsed().as_millis(),
                plugin_count: None,
                node_count: None,
                added_plugins: Vec::new(),
                removed_plugins: Vec::new(),
                changed_plugins: Vec::new(),
                changed_plugin_reasons: BTreeMap::new(),
                failure_summary: None,
            };
            return Ok((
                ReloadReport {
                    from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                    to_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                    snapshot_root: self.snapshot_root.display().to_string(),
                    staged_artifact_root: String::new(),
                    elapsed_ms: 0,
                    added_plugins: Vec::new(),
                    removed_plugins: Vec::new(),
                    changed_plugins: Vec::new(),
                    changed_plugin_reasons: BTreeMap::new(),
                },
                attempt,
            ));
        }

        let artifacts_dir = self.fixtures_root().join("artifacts");
        let index_path = artifacts_dir.join("index.json");
        let index = crate::plugin::artifact::load_artifact_index(&index_path).map_err(|e| {
            let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &e);
            (e, Box::new(attempt))
        })?;
        let index_map = crate::plugin::artifact::artifact_index_map(&index);

        // ── Phase 1: pre-load and validate all new dylibs ─────────────
        // No side effects — if anything fails, old plugins keep running.
        //
        // P1-20: `_dylib` is held as a struct-owned by-value handle for the
        // lifetime of `prepared` (drops at fn return, i.e. AFTER Phase 2
        // has updated the registry). The invoke path opens the artifact
        // fresh each time via `LoadedDylibApi::open`, so we don't hand any
        // function pointer from Phase 1 across to the registry — no
        // dangling reference to worry about. Keeping `_dylib` here still
        // matters because it validates the fingerprint before we mutate
        // the registry and gives us "old plugins keep running" on failure.
        struct Prepared {
            plugin_path: String,
            docs: PluginDocs,
            abi_fingerprint: AbiFingerprint,
            /// Artifact the reload just validated; repointed in the registry so
            /// the next invoke runs THIS build instead of the boot-staged one.
            artifact_path: PathBuf,
            _dylib: crate::plugin::dynamic::LoadedDylibApi,
        }
        let mut prepared: Vec<Prepared> = Vec::new();

        for plugin_path in &targets {
            let entry = index_map.get(plugin_path).ok_or_else(|| {
                let err = reload_artifact_missing_error(plugin_path);
                self.fail_reload(&previous_snapshot, started_at, err)
            })?;

            let resolved =
                crate::plugin::artifact::resolve_artifact_path(&index_path, &entry.artifact_path);
            let dylib = crate::plugin::dynamic::LoadedDylibApi::open(&resolved).map_err(|e| {
                let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &e);
                (e, Box::new(attempt))
            })?;
            let api = dylib.api();

            // Strict docs comparison.
            let new_docs: PluginDocs = serde_json::from_str(&(api.docs)().payload)
                .map_err(|e| reload_docs_parse_error(plugin_path, &e))
                .map_err(|err| self.fail_reload(&previous_snapshot, started_at, err))?;
            // The artifact is the source of truth for its own docs: a rebuild
            // that only rewords summaries/schemas is adopted (and the registry
            // entry is refreshed below), while a changed node set is a real
            // contract change and fails.
            if docs_contract_changed(&entry.docs, &new_docs) {
                let err = reload_docs_mismatch_error(
                    plugin_path,
                    &entry.abi_fingerprint,
                    entry.docs.nodes.len(),
                    new_docs.nodes.len(),
                );
                let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &err);
                return Err((err, Box::new(attempt)));
            }

            // Strict ABI fingerprint comparison.
            let actual_fingerprint: AbiFingerprint =
                serde_json::from_str(&(api.abi_fingerprint)().payload)
                    .map_err(|e| reload_abi_fingerprint_parse_error(plugin_path, &e))
                    .map_err(|err| self.fail_reload(&previous_snapshot, started_at, err))?;
            if actual_fingerprint.crate_hash != entry.abi_fingerprint.crate_hash
                || actual_fingerprint.api_hash != entry.abi_fingerprint.api_hash
            {
                let err = reload_abi_fingerprint_mismatch_error(
                    plugin_path,
                    &entry.abi_fingerprint,
                    actual_fingerprint,
                );
                let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &err);
                return Err((err, Box::new(attempt)));
            }

            prepared.push(Prepared {
                plugin_path: plugin_path.clone(),
                docs: new_docs,
                abi_fingerprint: actual_fingerprint,
                artifact_path: resolved.clone(),
                _dylib: dylib,
            });
        }

        // ── Phase 2: stop old services → update registry ───────────
        let registry = previous_snapshot.plugin_registry();
        let mut changed_plugins: Vec<String> = Vec::new();
        let mut changed_reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();

        // Stop background services + Task node threads before the old
        // dylib is replaced. This runs in Phase 2, AFTER validation: a
        // validation failure must leave the running plugins untouched.
        // (Previously it ran before Phase 1, so a failed reload had already
        // killed e.g. qq's 8099/8002 listeners — the bot went deaf.)
        for p in prepared.iter().rev() {
            let plugin_path = &p.plugin_path;
            eprintln!("reload_subtree: stopping services for {plugin_path}");
            self.service_registry
                .stop_plugin_services_timed(plugin_path);
            // Also invoke stop action for Task nodes (plugins that don't
            // implement the Service trait call stop via node invocation).
            //
            // P1-19: a plugin's stop handler runs inside the plugin's own
            // dylib; a panic there would unwind across the FFI boundary
            // (UB in the general case). Wrap each invocation in
            // catch_unwind so a broken stop handler cannot crash the whole
            // reload path.
            let snapshot = self.current_snapshot();
            let node_prefix = format!("{}::", plugin_path);
            let task_fqns = snapshot.node_registry().task_node_fqns();
            // The prefix match and the `plugin_id::node_id` split are both
            // expressed as iterator adapters rather than nested `if`s: every
            // fqn that reaches the body is a Task node of this plugin and is
            // guaranteed to carry a `::`, so there is no gate whose untaken
            // side is structurally unreachable.
            for (plugin_id, node_id) in task_fqns
                .iter()
                .filter(|fqn| fqn.starts_with(&node_prefix))
                .filter_map(|fqn| fqn.split_once("::"))
            {
                let payload = serde_json::json!({"action": "stop"}).to_string();
                let this = self;
                let fqn = format!("{plugin_id}::{node_id}");
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if TEST_STOP_HANDLER_PANIC_INJECTION
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        panic!("test panic injection: stop handler");
                    }
                    // 检查 stop 的返回值：handler 报 ok=false 或 invoke 出错时
                    // 给出诊断，而不是静默丢弃。
                    let outcome = this.invoke(plugin_id, node_id, payload);
                    if let Some(diag) = stop_result_diagnostic(&fqn, &outcome) {
                        eprintln!("{diag}");
                    }
                }));
                if let Err(err) = result {
                    let msg = reload_stop_handler_panic_message(err.as_ref());
                    eprintln!("[reload] stop handler for {plugin_id}::{node_id} panicked: {msg}");
                }
            }
            // Retire the keep-alive dylib handle for this plugin path. The
            // handle is moved to a retained list, NOT dropped: unloading a
            // Task-node dylib unmaps code that the plugin's own detached
            // threads may still be executing (observed as SIGSEGV right after
            // a `reload /qq`, whose message poller never stops). A `stop`
            // handler is not required to join every thread, so the mapping
            // stays until process exit; the cost is one mapping per reload of
            // a Task-node plugin.
            crate::plugin::invoke::unregister_task_library(plugin_path);
        }

        for p in &prepared {
            registry.reload_plugin_entry_with_artifact(
                &p.plugin_path,
                p.docs.clone(),
                p.abi_fingerprint.clone(),
                Some(p.artifact_path.clone()),
            );
            changed_plugins.push(p.plugin_path.clone());
            changed_reasons.insert(p.plugin_path.clone(), vec!["subtree reload".to_string()]);
            eprintln!("reload_subtree: reloaded {}", p.plugin_path);
        }

        // A subtree reload stops every target's services; bring them back up
        // with the freshly loaded dylibs.
        self.restart_services_for(&changed_plugins);

        let zombie_count = self.service_registry.zombie_count();
        if zombie_count > 0 {
            eprintln!(
                "reload_subtree: {} zombie service(s) remaining (use kill_zombie_services to clean up)",
                zombie_count
            );
        }

        // P1-21: give the subtree reload a distinct snapshot id so
        // invocation traces before and after the reload don't collide.
        // The previous behaviour reused `previous_snapshot.snapshot_id()`
        // for both `from` and `to`, making downstream analytics unable to
        // distinguish invocations against the old vs new plugin set.
        let new_snapshot_id = make_snapshot_dir_name();
        let report = ReloadReport {
            from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
            to_snapshot_id: new_snapshot_id,
            snapshot_root: self.snapshot_root.display().to_string(),
            staged_artifact_root: String::new(),
            elapsed_ms: started_at.elapsed().as_millis(),
            added_plugins: Vec::new(),
            removed_plugins: Vec::new(),
            changed_plugins: changed_plugins.clone(),
            changed_plugin_reasons: changed_reasons,
        };

        let attempt = ReloadAttemptReport {
            status: ReloadAttemptStatus::Reloaded,
            from_snapshot_id: report.from_snapshot_id.clone(),
            to_snapshot_id: Some(report.to_snapshot_id.clone()),
            snapshot_root: report.snapshot_root.clone(),
            staged_artifact_root: report.staged_artifact_root.clone(),
            elapsed_ms: report.elapsed_ms,
            plugin_count: Some(targets.len()),
            node_count: None,
            added_plugins: Vec::new(),
            removed_plugins: Vec::new(),
            changed_plugins,
            changed_plugin_reasons: BTreeMap::new(),
            failure_summary: None,
        };

        Ok((report, attempt))
    }

    pub fn reload_candidate(&self) -> Result<CandidateSnapshotStatus, RuntimeError> {
        match self.reload_candidate_internal() {
            Ok((status, attempt)) => {
                self.record_candidate_reload_attempt(attempt);
                if let Some(snapshot) = self.candidate_snapshot() {
                    let report = ReloadReport {
                        from_snapshot_id: status.from_snapshot_id.clone(),
                        to_snapshot_id: status.candidate_snapshot_id.clone(),
                        snapshot_root: status.snapshot_root.clone(),
                        staged_artifact_root: status.staged_artifact_root.clone(),
                        elapsed_ms: 0,
                        added_plugins: status.added_plugins.clone(),
                        removed_plugins: status.removed_plugins.clone(),
                        changed_plugins: status.changed_plugins.clone(),
                        changed_plugin_reasons: status.changed_plugin_reasons.clone(),
                    };
                    self.observe_snapshot_plugin_issues(
                        snapshot.as_ref(),
                        &report,
                        "candidate_reload",
                    );
                }
                // auto-iteration deferred to kernel timer.
                Ok(status)
            }
            Err((err, attempt)) => {
                self.record_candidate_reload_attempt(*attempt);
                self.observe_reload_error("candidate_reload", &err);
                // auto-iteration deferred to kernel timer.
                Err(err)
            }
        }
    }

    pub fn reload_candidate_with_diagnostics(&self) -> ReloadAttemptReport {
        match self.reload_candidate_internal() {
            Ok((status, attempt)) => {
                self.record_candidate_reload_attempt(attempt.clone());
                if let Some(snapshot) = self.candidate_snapshot() {
                    let report = ReloadReport {
                        from_snapshot_id: status.from_snapshot_id.clone(),
                        to_snapshot_id: status.candidate_snapshot_id.clone(),
                        snapshot_root: status.snapshot_root.clone(),
                        staged_artifact_root: status.staged_artifact_root.clone(),
                        elapsed_ms: 0,
                        added_plugins: status.added_plugins.clone(),
                        removed_plugins: status.removed_plugins.clone(),
                        changed_plugins: status.changed_plugins.clone(),
                        changed_plugin_reasons: status.changed_plugin_reasons.clone(),
                    };
                    self.observe_snapshot_plugin_issues(
                        snapshot.as_ref(),
                        &report,
                        "candidate_reload",
                    );
                }
                // auto-iteration deferred to kernel timer.
                attempt
            }
            Err((err, attempt)) => {
                let attempt = *attempt;
                self.record_candidate_reload_attempt(attempt.clone());
                self.observe_reload_error("candidate_reload", &err);
                // auto-iteration deferred to kernel timer.
                attempt
            }
        }
    }

    pub fn promote_candidate(&self) -> Result<ReloadReport, RuntimeError> {
        self.promote_candidate_matching(None)
    }

    pub(crate) fn promote_candidate_matching(
        &self,
        expected_candidate_snapshot_id: Option<&str>,
    ) -> Result<ReloadReport, RuntimeError> {
        if self.candidate_snapshot().is_none() {
            return Err(RuntimeError::CandidateSnapshotMissing);
        }
        clear_plugin_iteration_journal(&self.snapshot_root)?;
        let previous_snapshot = self.current_snapshot();
        let candidate = {
            let mut guard = self
                .candidate_snapshot
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let staged = guard
                .as_ref()
                .ok_or(RuntimeError::CandidateSnapshotMissing)?;
            if let Some(expected) = expected_candidate_snapshot_id {
                let actual = staged.snapshot.snapshot_id();
                if actual != expected {
                    return Err(candidate_replaced_error(expected, actual));
                }
            }
            guard.take().ok_or(RuntimeError::CandidateSnapshotMissing)?
        };
        let StagedCandidateSnapshot {
            snapshot: next_snapshot,
            config,
            ..
        } = candidate;
        self.install_config(config);
        {
            let mut guard = self
                .current_snapshot
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard = next_snapshot.clone();
        }

        let report = ReloadReport::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            0,
        );
        self.record_reload_attempt(ReloadAttemptReport::from_report(
            &report,
            next_snapshot.as_ref(),
        ));
        self.retire_snapshot(previous_snapshot);
        self.cleanup_retired_snapshots();
        Ok(report)
    }

    pub fn rollback_candidate(&self) -> Result<CandidateSnapshotStatus, RuntimeError> {
        self.rollback_candidate_matching(None)
    }

    pub(crate) fn rollback_candidate_matching(
        &self,
        expected_candidate_snapshot_id: Option<&str>,
    ) -> Result<CandidateSnapshotStatus, RuntimeError> {
        if self.candidate_snapshot().is_none() {
            return Err(RuntimeError::CandidateSnapshotMissing);
        }
        {
            let guard = self
                .candidate_snapshot
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let staged = guard
                .as_ref()
                .ok_or(RuntimeError::CandidateSnapshotMissing)?;
            if let Some(expected) = expected_candidate_snapshot_id {
                let actual = staged.snapshot.snapshot_id();
                if actual != expected {
                    return Err(candidate_replaced_error(expected, actual));
                }
            }
        }
        restore_plugin_iteration_workspace(&self.fixtures_root, &self.snapshot_root, None)?;
        let candidate = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .ok_or(RuntimeError::CandidateSnapshotMissing)?;
        let status = candidate.status.clone();
        self.retire_snapshot(candidate.snapshot);
        self.cleanup_retired_snapshots();
        Ok(status)
    }

    pub fn approve_blocked_iteration(
        &self,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let mut result = self.kernel.take_blocked_iteration(iteration_id)?;
        // R3: 校验 blocked 结果里记录的候选（暂存时的 snapshot_id）与当前
        // staged 候选一致。`reload_candidate` 是公开 API 且内核定时器自动
        // 迭代可触发，blocked 之后候选可能已被替换——approve 不得 promote
        // 一份从未验证的快照。不一致时保留 blocked 条目并报错（promote 的
        // Err 分支做插回），让操作者可以重试。
        let expected_candidate_snapshot_id = result
            .candidate
            .as_ref()
            .map(|candidate| candidate.candidate_snapshot_id.as_str());
        let report = match self.promote_candidate_matching(expected_candidate_snapshot_id) {
            Ok(report) => report,
            Err(err) => {
                self.kernel
                    .blocked_iterations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(iteration_id.to_string(), result);
                return Err(err);
            }
        };
        result.final_verdict = PluginIterationFinalVerdict::Promoted;
        result.blocked_reason = None;
        result.candidate = Some(CandidateSnapshotStatus {
            from_snapshot_id: report.from_snapshot_id.clone(),
            candidate_snapshot_id: report.to_snapshot_id.clone(),
            snapshot_root: report.snapshot_root.clone(),
            staged_artifact_root: report.staged_artifact_root.clone(),
            plugin_count: self.current_snapshot().plugin_registry().iter().count(),
            node_count: self.current_snapshot().node_registry().len(),
            added_plugins: report.added_plugins.clone(),
            removed_plugins: report.removed_plugins.clone(),
            changed_plugins: report.changed_plugins.clone(),
            changed_plugin_reasons: report.changed_plugin_reasons.clone(),
        });
        self.kernel.record_plugin_iteration_outcome(&result);
        Ok(result)
    }

    pub(crate) fn rollback_candidate_if_staged(
        &self,
        expected_candidate_snapshot_id: Option<&str>,
    ) -> Option<Result<(), RuntimeError>> {
        if self.candidate_snapshot().is_some() {
            Some(
                self.rollback_candidate_matching(expected_candidate_snapshot_id)
                    .map(|_| ()),
            )
        } else {
            None
        }
    }

    pub(crate) fn reload_internal(
        &self,
    ) -> Result<(ReloadReport, ReloadAttemptReport), (RuntimeError, Box<ReloadAttemptReport>)> {
        let previous_snapshot = self.current_snapshot();
        let staged_artifact_root = next_staged_artifact_root(&self.snapshot_root);
        let started_at = Instant::now();
        // Config is part of a reload: `settings`, `enabled` and the LLM profiles
        // all come from `config/`, so a reload that only rebuilt dylibs would
        // keep running on stale config. A broken config fails the reload and
        // leaves the previous snapshot serving.
        let config = match RuntimeConfig::load(&self.fixtures_root) {
            Ok(config) => self.adopt_reloaded_config(config),
            Err(err) => {
                let attempt = failed_reload_attempt(
                    previous_snapshot.as_ref(),
                    &self.snapshot_root,
                    &staged_artifact_root,
                    started_at.elapsed().as_millis(),
                    &err,
                );
                return Err((err, Box::new(attempt)));
            }
        };
        let loader = Loader::new(default_loader_config(&self.fixtures_root))
            .with_disabled(disabled_plugins(&config));

        let next_snapshot = match build_snapshot_with_staged_root(
            &loader,
            staged_artifact_root.clone(),
            &config.plugin_configs,
        ) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(err) => {
                let attempt = failed_reload_attempt(
                    previous_snapshot.as_ref(),
                    &self.snapshot_root,
                    &staged_artifact_root,
                    started_at.elapsed().as_millis(),
                    &err,
                );
                return Err((err, Box::new(attempt)));
            }
        };
        // Snapshot built → the reload is going to succeed, so the new config
        // goes live together with it (settings, `enabled`, LLM profiles).
        self.install_config(config);

        // Stop services for plugins that are being removed or changed in the
        // new snapshot before swapping it in.
        let previous_plugins: BTreeSet<String> = previous_snapshot
            .plugin_registry()
            .iter()
            .map(|(path, _)| path)
            .collect();
        let next_plugins: BTreeSet<String> = next_snapshot
            .plugin_registry()
            .iter()
            .map(|(path, _)| path)
            .collect();
        // Plugins the new snapshot dropped — stop their services.
        // 用 `_timed` 变体：stop 卡住时有上限，不会把 reload 拖死。
        let mut stopped_services: Vec<String> = Vec::new();
        for plugin_path in previous_plugins.difference(&next_plugins) {
            self.service_registry
                .stop_plugin_services_timed(plugin_path);
            stopped_services.push(plugin_path.clone());
        }
        // Also stop services for plugins whose docs changed (the new snapshot
        // may have different Task nodes).
        for plugin_path in next_plugins.intersection(&previous_plugins) {
            let prev_plugin = previous_snapshot.plugin_registry().get(plugin_path);
            let next_plugin = next_snapshot.plugin_registry().get(plugin_path);
            let prev_docs = prev_plugin.as_ref().and_then(|p| p.docs.as_ref());
            let next_docs = next_plugin.as_ref().and_then(|p| p.docs.as_ref());
            // Compare docs by JSON representation — if they differ, restart
            // services so the new plugin version's services are used.
            if prev_docs != next_docs {
                self.service_registry
                    .stop_plugin_services_timed(plugin_path);
                stopped_services.push(plugin_path.clone());
            }
        }

        {
            let mut guard = self
                .current_snapshot
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard = next_snapshot.clone();
        }
        let replaced_candidate = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        // Restart the Task services this reload stopped, against the new
        // snapshot (otherwise a reloaded qq/feishu stops listening for good).
        self.restart_services_for(&stopped_services);

        let report = ReloadReport::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            started_at.elapsed().as_millis(),
        );
        let retired_root = previous_snapshot.staged_artifact_root.clone();
        let retired_weak = Arc::downgrade(&previous_snapshot);
        drop(previous_snapshot);
        self.retired_snapshots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(RetiredSnapshot {
                snapshot: retired_weak,
                staged_artifact_root: retired_root,
            });
        if let Some(candidate) = replaced_candidate {
            self.retire_snapshot(candidate.snapshot);
        }
        self.cleanup_retired_snapshots();

        let attempt = ReloadAttemptReport::from_report(&report, next_snapshot.as_ref());
        Ok((report, attempt))
    }

    pub(crate) fn reload_candidate_internal(
        &self,
    ) -> Result<
        (CandidateSnapshotStatus, ReloadAttemptReport),
        (RuntimeError, Box<ReloadAttemptReport>),
    > {
        let previous_snapshot = self.current_snapshot();
        let staged_artifact_root = next_staged_artifact_root(&self.snapshot_root);
        let started_at = Instant::now();
        // Config is part of a reload: `settings`, `enabled` and the LLM profiles
        // all come from `config/`, so a reload that only rebuilt dylibs would
        // keep running on stale config. A broken config fails the reload and
        // leaves the previous snapshot serving.
        let config = match RuntimeConfig::load(&self.fixtures_root) {
            Ok(config) => self.adopt_reloaded_config(config),
            Err(err) => {
                let attempt = failed_reload_attempt(
                    previous_snapshot.as_ref(),
                    &self.snapshot_root,
                    &staged_artifact_root,
                    started_at.elapsed().as_millis(),
                    &err,
                );
                return Err((err, Box::new(attempt)));
            }
        };
        let loader = Loader::new(default_loader_config(&self.fixtures_root))
            .with_disabled(disabled_plugins(&config));

        let next_snapshot = match build_snapshot_with_staged_root(
            &loader,
            staged_artifact_root.clone(),
            &config.plugin_configs,
        ) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(err) => {
                let attempt = failed_reload_attempt(
                    previous_snapshot.as_ref(),
                    &self.snapshot_root,
                    &staged_artifact_root,
                    started_at.elapsed().as_millis(),
                    &err,
                );
                return Err((err, Box::new(attempt)));
            }
        };

        let report = ReloadReport::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            started_at.elapsed().as_millis(),
        );
        let status = CandidateSnapshotStatus::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            &report,
        );
        let attempt = ReloadAttemptReport::from_candidate_status(&status, report.elapsed_ms);
        let candidate_entry = StagedCandidateSnapshot {
            snapshot: next_snapshot,
            status: status.clone(),
            config,
        };

        let mut guard = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(previous_candidate) = guard.replace(candidate_entry) {
            self.retire_snapshot(previous_candidate.snapshot);
        }
        drop(guard);
        self.cleanup_retired_snapshots();
        Ok((status, attempt))
    }
}
