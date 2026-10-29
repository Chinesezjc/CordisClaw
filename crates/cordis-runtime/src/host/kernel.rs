use super::*;

impl RuntimeKernel {
    pub fn new(workspace_root: impl Into<PathBuf>, config: &RuntimeConfig) -> Self {
        let workspace_root = workspace_root.into();
        Self {
            config_dir: config.config_dir.clone(),
            llm_api: RwLock::new(config.llm_api.clone()),
            plugin_configs: RwLock::new(config.plugin_configs.clone()),
            plugin_iteration_policy: PluginIterationPolicy::default(),
            plugin_issues: Mutex::new(BTreeMap::new()),
            plugin_history: Mutex::new(VecDeque::new()),
            blocked_iterations: Mutex::new(BTreeMap::new()),
            last_plugin_iteration: Mutex::new(None),
            active_plugin_iteration: Mutex::new(None),
            iteration_metrics: Mutex::new(KernelIterationMetrics::default()),
            memory: Mutex::new(ChangeMemory::with_limit(config.kernel.change_history_limit)),
            updater: AutoUpdater::new(&workspace_root),
            workspace_root,
        }
    }

    /// Install a reloaded config: the LLM API and the per-plugin config block
    /// both feed `status()` and the plugin-iteration gate, so they have to move
    /// together with the host's copy.
    pub fn apply_config(&self, config: &RuntimeConfig) {
        *self
            .llm_api
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = config.llm_api.clone();
        *self
            .plugin_configs
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = config.plugin_configs.clone();
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn status(&self) -> KernelStatus {
        // Lock each field individually and drop the guard immediately
        // to prevent deadlocks with concurrent PluginIteration operations
        // that may also need these locks.
        let llm_api = self
            .llm_api
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let plugin_configs = self
            .plugin_configs
            .read()
            .unwrap_or_else(|poison| poison.into_inner());
        let plugin_config_count = plugin_configs.len();
        let plugin_disabled_count = plugin_configs.values().filter(|c| !c.enabled).count();
        let plugin_issue_count = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let blocked_iteration_count = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let plugin_iteration_total = self
            .plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let last_plugin_iteration = self
            .last_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let metrics = self
            .iteration_metrics
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (
            iteration_total,
            iteration_promote_total,
            iteration_rollback_total,
            iteration_infrastructure_failure_total,
        ) = (
            metrics.iteration_total,
            metrics.iteration_promote_total,
            metrics.iteration_rollback_total,
            metrics.iteration_infrastructure_failure_total,
        );
        drop(metrics);
        // Single memory lock for both fields — avoids double-futex.
        let (history_len, last_change) = {
            let memory = self
                .memory
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let len = memory.len();
            let last = memory.recent(1).into_iter().next();
            (len, last)
        };
        KernelStatus {
            workspace_root: self.workspace_root.display().to_string(),
            config_dir: self.config_dir.display().to_string(),
            llm_provider: llm_api.provider.clone(),
            llm_model: llm_api.model.clone(),
            plugin_config_count,
            plugin_disabled_count,
            iteration_total,
            iteration_promote_total,
            iteration_rollback_total,
            iteration_infrastructure_failure_total,
            history_len,
            last_change,
            plugin_issue_count,
            blocked_iteration_count,
            plugin_iteration_total,
            last_plugin_iteration: last_plugin_iteration
                .as_ref()
                .map(plugin_iteration_status_from_result),
        }
    }

    pub fn history(&self) -> Vec<ChangeRecord> {
        let memory = self
            .memory
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        memory.recent(memory.len())
    }

    pub fn plugin_issues(&self) -> Vec<KernelPluginIssue> {
        let mut issues = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        issues.sort_by(|left, right| {
            left.status
                .cmp(&right.status)
                .then_with(|| left.source.priority().cmp(&right.source.priority()))
                .then_with(|| left.first_observed_at_ms.cmp(&right.first_observed_at_ms))
        });
        issues
    }

    pub fn plugin_history(&self) -> Vec<PluginIterationHistoryEntry> {
        self.plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn blocked_iterations(&self) -> Vec<PluginIterationStatus> {
        self.blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .map(plugin_iteration_status_from_result)
            .collect()
    }

    pub fn plugin_iteration_status(
        &self,
        iteration_id: &str,
    ) -> Result<PluginIterationStatus, RuntimeError> {
        if let Some(result) = self
            .last_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .filter(|result| result.iteration_id == iteration_id)
        {
            return Ok(plugin_iteration_status_from_result(&result));
        }
        if let Some(result) = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(iteration_id)
            .cloned()
        {
            return Ok(plugin_iteration_status_from_result(&result));
        }
        if let Some(entry) = self
            .plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .find(|entry| entry.iteration_id == iteration_id)
            .cloned()
        {
            return Ok(plugin_iteration_status_from_history(&entry));
        }
        Err(RuntimeError::PluginIterationStatusNotFound {
            iteration_id: iteration_id.to_string(),
        })
    }

    pub fn take_blocked_iteration(
        &self,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let mut blocked = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let result = blocked.remove(iteration_id).ok_or_else(|| {
            RuntimeError::PluginIterationStatusNotFound {
                iteration_id: iteration_id.to_string(),
            }
        })?;
        // The blocked map's only writer (`record_plugin_iteration_outcome`)
        // inserts exclusively `Blocked` verdicts, so this holds by invariant.
        debug_assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Blocked);
        Ok(result)
    }

    pub fn can_auto_iterate_plugins(&self) -> bool {
        let llm_api = self
            .llm_api
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        llm_api
            .api_key
            .as_ref()
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false)
            || std::env::var(&llm_api.api_key_env)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
    }

    pub fn observe_plugin_issue(
        &self,
        source: KernelPluginIssueSource,
        root_plugin_path: impl Into<String>,
        summary: impl Into<String>,
    ) -> KernelPluginIssue {
        let root_plugin_path = root_plugin_path.into();
        let summary = summary.into();
        let now_ms = now_ms();
        let issue_id = format!(
            "plugin-issue-{}-{}",
            root_plugin_path.replace('/', "-"),
            source.priority()
        );
        let mut guard = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let issue = guard
            .entry(issue_id.clone())
            .or_insert_with(|| KernelPluginIssue {
                issue_id: issue_id.clone(),
                root_plugin_path: root_plugin_path.clone(),
                target_plugin_paths: vec![root_plugin_path.clone()],
                source,
                summary: summary.clone(),
                status: KernelPluginIssueStatus::Open,
                first_observed_at_ms: now_ms,
                last_observed_at_ms: now_ms,
                observe_count: 0,
            });
        issue.last_observed_at_ms = now_ms;
        issue.observe_count += 1;
        issue.summary = summary;
        if !matches!(issue.status, KernelPluginIssueStatus::Running) {
            issue.status = KernelPluginIssueStatus::Open;
        }
        issue.clone()
    }

    pub(crate) fn select_issue_for_request(
        &self,
        request: &KernelPluginIterationRequest,
    ) -> Result<Option<KernelPluginIssue>, RuntimeError> {
        let issues = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(issue_id) = &request.issue_id {
            return issues.get(issue_id).cloned().map(Some).ok_or_else(|| {
                RuntimeError::PluginIterationIssueNotFound {
                    issue_id: issue_id.clone(),
                }
            });
        }
        let mut candidates = issues
            .values()
            .filter(|issue| issue.status == KernelPluginIssueStatus::Open)
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.source
                .priority()
                .cmp(&right.source.priority())
                .then_with(|| left.first_observed_at_ms.cmp(&right.first_observed_at_ms))
        });
        Ok(candidates.into_iter().next())
    }

    pub(super) fn begin_plugin_iteration(
        &self,
        snapshot: &RuntimeSnapshot,
        request: &KernelPluginIterationRequest,
    ) -> Result<PreparedPluginIteration, RuntimeError> {
        let mut active = self
            .active_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(iteration_id) = active.clone() {
            return Err(RuntimeError::PluginIterationActive { iteration_id });
        }

        let selected_issue = self.select_issue_for_request(request)?;
        let iteration_id = normalize_request_id(None, "plugin-iteration");

        // Root workspace mode: empty target_plugin_paths means work across all
        // plugins. This allows creating new top-level plugins and editing any
        // existing one. The "" → "plugins" root in allowed_plugin_roots lets
        // validate_path accept any path under plugins/.
        let root_mode = request.target_plugin_paths.is_empty();
        // A target that exists on disk but is not (yet) loaded — a freshly
        // scaffolded plugin — is its own root, so it can still be iterated.
        let disk_only_root = |targets: &[String]| -> Option<String> {
            let [target] = targets else { return None };
            let target = target.trim().trim_start_matches('/').to_string();
            if target.is_empty() {
                return None;
            }
            let loaded = snapshot
                .plugin_registry()
                .iter()
                .any(|(path, _)| *path == target || path.starts_with(&format!("{target}/")));
            let on_disk = self.workspace_root.join("plugins").join(&target).is_dir();
            (!loaded && on_disk).then_some(target)
        };
        let root_plugin_path = if let Some(issue) = &selected_issue {
            issue.root_plugin_path.clone()
        } else if root_mode {
            String::new()
        } else if let Some(root) = disk_only_root(&request.target_plugin_paths) {
            root
        } else {
            determine_root_plugin_path(snapshot, &request.target_plugin_paths)?
        };
        let target_plugin_paths: Vec<String> = if root_mode {
            snapshot
                .plugin_registry()
                .iter()
                .map(|(plugin_path, _)| plugin_path.clone())
                .collect()
        } else {
            snapshot
                .plugin_registry()
                .iter()
                .map(|(plugin_path, _)| plugin_path)
                .filter(|plugin_path| {
                    plugin_path == &root_plugin_path
                        || plugin_path.starts_with(&format!("{root_plugin_path}/"))
                })
                .collect()
        };
        // A brand-new plugin has no artifact yet, so it is not in the registry:
        // iterating it must still work, otherwise the only way to write the
        // first line of code is a bypass (`fs_write`). Accept an on-disk
        // plugin directory as the target.
        let target_plugin_paths: Vec<String> = if target_plugin_paths.is_empty() && !root_mode {
            let on_disk = !root_plugin_path.trim().is_empty()
                && self
                    .workspace_root
                    .join("plugins")
                    .join(&root_plugin_path)
                    .is_dir();
            if on_disk {
                eprintln!(
                    "[iteration] {root_plugin_path} is not loaded yet; iterating the on-disk directory"
                );
                vec![root_plugin_path.clone()]
            } else {
                target_plugin_paths
            }
        } else {
            target_plugin_paths
        };
        if target_plugin_paths.is_empty() && !root_mode {
            let message = if root_plugin_path.trim().is_empty() {
                "plugin subtree not found: the selected issue has no plugin root; pass \"/<plugin>\" (e.g. \"/expr\") or \"/\" for the whole workspace".to_string()
            } else {
                format!("plugin subtree not found for {root_plugin_path}")
            };
            return Err(RuntimeError::InvalidArgument { message });
        }
        let issue_id = selected_issue
            .as_ref()
            .map(|issue| issue.issue_id.clone())
            .unwrap_or_else(|| format!("plugin-issue-{iteration_id}"));
        let summary = request
            .instruction
            .clone()
            .or_else(|| selected_issue.as_ref().map(|issue| issue.summary.clone()))
            .unwrap_or_else(|| format!("iterate plugin subtree {root_plugin_path}"));
        let mut allowed_plugin_roots: BTreeMap<String, String> = target_plugin_paths
            .iter()
            .map(|plugin_path: &String| (plugin_path.clone(), format!("plugins/{plugin_path}")))
            .collect();
        // In root mode, add a catch-all root so that paths under any plugin
        // directory (including not-yet-created ones) pass subtree validation.
        if root_mode {
            allowed_plugin_roots.insert(String::new(), "plugins".to_string());
        }

        if let Some(ref issue) = selected_issue {
            self.plugin_issues
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .entry(issue.issue_id.clone())
                .and_modify(|entry| entry.status = KernelPluginIssueStatus::Running);
        }
        *active = Some(iteration_id.clone());

        Ok(PreparedPluginIteration {
            iteration_id,
            issue_id,
            root_plugin_path,
            target_plugin_paths,
            source: selected_issue.as_ref().map(|issue| issue.source),
            summary,
            manual_approved: request.manual_approved,
            tests_command: request.tests_command.clone(),
            safety_command: request.safety_command.clone(),
            verify_profile: request
                .verify_profile
                .unwrap_or(VerificationProfile::RustWorkspace),
            quality_score: request.quality_score,
            edit_plan: request.edit_plan.clone(),
            instruction: request.instruction.clone(),
            allowed_plugin_roots,
        })
    }

    pub fn finish_plugin_iteration(&self, iteration_id: &str) {
        let mut active = self
            .active_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if active.as_deref() == Some(iteration_id) {
            *active = None;
        }
    }

    pub(crate) fn update_issue_status(&self, issue_id: &str, status: KernelPluginIssueStatus) {
        if let Some(issue) = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(issue_id)
        {
            issue.status = status;
        }
    }

    pub fn record_plugin_iteration_outcome(&self, result: &KernelPluginIterationResult) {
        // P1-9: fragile nested-lock chain. Every lock is now acquired,
        // mutated, and released in the shortest possible scope so that at
        // no point does this function hold two of the kernel Mutexes at
        // once. Callers that hold `plugin_issues` (e.g. `select_issue_for_
        // request`) can invert order all day without deadlocking us.
        //
        // Canonical lock-order document (post-fix): every mutating call in
        // this file acquires locks in this order (partial order); this
        // function honours it by never holding more than one at a time.
        //   iteration_metrics → plugin_history → last_plugin_iteration →
        //   blocked_iterations → plugin_issues
        let completed_at_ms = now_ms();
        let history_entry = PluginIterationHistoryEntry {
            iteration_id: result.iteration_id.clone(),
            issue_id: result.issue_id.clone(),
            root_plugin_path: result.root_plugin_path.clone(),
            target_plugin_paths: result.target_plugin_paths.clone(),
            source: result.source,
            summary: result.summary.clone(),
            changed_paths: result.changed_paths.clone(),
            verifier_verdict: result.verifier_verdict,
            canary_verdict: result.canary.as_ref().map(|report| report.verdict),
            final_verdict: result.final_verdict,
            blocked_reason: result.blocked_reason.clone(),
            observed_at_ms: completed_at_ms,
            completed_at_ms,
        };

        {
            let mut metrics = self
                .iteration_metrics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            metrics.iteration_total += 1;
            match result.final_verdict {
                PluginIterationFinalVerdict::Blocked => {}
                PluginIterationFinalVerdict::Promoted => {
                    metrics.iteration_promote_total += 1;
                }
                PluginIterationFinalVerdict::RolledBack => {
                    metrics.iteration_rollback_total += 1;
                }
                PluginIterationFinalVerdict::InfrastructureFailure => {
                    metrics.iteration_infrastructure_failure_total += 1;
                }
            }
        }

        {
            // P1-23: cap `plugin_history` at a hard upper bound; without
            // this the VecDeque grew forever on long-running hosts. Newest
            // entries stay at the front, so we drop from the back.
            const MAX_PLUGIN_HISTORY: usize = 1024;
            let mut history = self
                .plugin_history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(existing) = history
                .iter_mut()
                .find(|entry| entry.iteration_id == result.iteration_id)
            {
                *existing = history_entry;
            } else {
                history.push_front(history_entry);
                while history.len() > MAX_PLUGIN_HISTORY {
                    history.pop_back();
                }
            }
        }

        {
            *self
                .last_plugin_iteration
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(result.clone());
        }

        {
            let mut blocked = self
                .blocked_iterations
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match result.final_verdict {
                // 基础设施故障与 Blocked 一样保留：腾出磁盘后可经
                // approve_blocked_iteration 原样重试；RolledBack 摘除是因为
                // 那代表插件真的没通过验证，重试同一份代码没有意义。
                PluginIterationFinalVerdict::Blocked
                | PluginIterationFinalVerdict::InfrastructureFailure => {
                    blocked.insert(result.iteration_id.clone(), result.clone());
                    // R9: blocked 保留无上限会随每次唯一的 iteration_id
                    // 无界增长（只有 approve 或后续同 id 结局才摘除）。与
                    // plugin_history / invocation_samples 的上限一致，保留
                    // 最近 64 条：BTreeMap 按 key 有序（iteration_id 由
                    // normalize_request_id 生成、前缀带毫秒时间戳），首条
                    // 即最旧，插一条最多超限 1 → pop_first 淘汰之。
                    const MAX_BLOCKED_ITERATIONS: usize = 64;
                    if blocked.len() > MAX_BLOCKED_ITERATIONS {
                        let _ = blocked.pop_first();
                    }
                }
                PluginIterationFinalVerdict::Promoted | PluginIterationFinalVerdict::RolledBack => {
                    blocked.remove(&result.iteration_id);
                }
            }
        }

        let status = match result.final_verdict {
            PluginIterationFinalVerdict::Blocked => KernelPluginIssueStatus::Blocked,
            PluginIterationFinalVerdict::Promoted => KernelPluginIssueStatus::Resolved,
            PluginIterationFinalVerdict::RolledBack => KernelPluginIssueStatus::Open,
            // 不是 Open：Open 读作"插件仍然坏着"，而磁盘满与插件质量无关。
            // 复用 Blocked（等人处置基础设施后重试），不新增 issue 状态。
            PluginIterationFinalVerdict::InfrastructureFailure => KernelPluginIssueStatus::Blocked,
        };
        self.update_issue_status(&result.issue_id, status);
    }

    pub fn run_iteration(
        &self,
        plan: AutoUpdatePlan,
        verification: VerificationInput,
    ) -> Result<AutoUpdateResult, RuntimeError> {
        let issue_id = plan.issue_id.clone();
        let patch_id = plan.patch_id.clone();
        {
            let mut metrics = self
                .iteration_metrics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            metrics.iteration_total += 1;
        }
        let result = self
            .updater
            .execute(plan, |_| Ok(VerificationEnvelope::from(verification)))?;
        {
            let mut metrics = self
                .iteration_metrics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if result.rolled_back {
                metrics.iteration_rollback_total += 1;
            } else {
                metrics.iteration_promote_total += 1;
            }
        }
        self.memory
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .record(
                issue_id,
                patch_id,
                "auto_update".to_string(),
                None,
                if result.rolled_back {
                    crate::kernel::memory::ChangeVerdict::Rollback
                } else {
                    crate::kernel::memory::ChangeVerdict::Promote
                },
                result.quality_score,
                Vec::new(),
            );
        Ok(result)
    }
}
