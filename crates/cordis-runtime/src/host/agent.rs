use super::*;

impl RuntimeHost {
    pub fn drop_session(&self, session_id: &str) {
        // R1: 若该会话正有一个 in-flight turn（`agent_send` 已把它移出
        // `agent_sessions`），下面的 map remove 会落空、只删到快照，turn
        // 结束后会话仍会被收尾插回并 auto_save 重写快照。此时标记
        // `dropped_during_turn`：检查与标记在 inflight 锁下原子完成，而
        // turn 收尾也持同一把锁做消费，二者互斥，不会出现"标记晚了"的
        // 复活竞态。
        {
            let inflight = self
                .inflight_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if inflight.contains(session_id) {
                self.dropped_during_turn
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(session_id.to_string());
            }
        }
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.pending_session_actions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.delete_session_snapshot(session_id);
    }

    pub fn config_value(&self) -> RuntimeConfig {
        self.config
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) fn adopt_reloaded_config(&self, mut config: RuntimeConfig) -> RuntimeConfig {
        let boot_snapshot_root = self.config_value().runtime.snapshot_root;
        if config.runtime.snapshot_root != boot_snapshot_root {
            eprintln!(
                "[reload] runtime.snapshot_root changes require a restart; keeping the boot value"
            );
            config.runtime.snapshot_root = boot_snapshot_root;
        }
        config
    }

    pub fn install_config(&self, config: RuntimeConfig) {
        *self
            .config
            .write()
            .unwrap_or_else(|poison| poison.into_inner()) = config.clone();
        self.kernel.apply_config(&config);
    }

    pub fn agent_start(&self, kind: AgentSessionKind) -> Result<AgentSessionHandle, RuntimeError> {
        self.agent_start_with(kind, AgentStartOptions::default())
    }

    pub fn agent_start_with(
        &self,
        kind: AgentSessionKind,
        options: AgentStartOptions,
    ) -> Result<AgentSessionHandle, RuntimeError> {
        let handle = AgentSessionHandle {
            session_id: normalize_request_id(None, "agent-session"),
            kind,
        };
        let session_kind_label = match kind {
            AgentSessionKind::RuntimeShell => "runtime_shell",
            AgentSessionKind::PluginIteration => "plugin_iteration",
        };
        let state = match kind {
            AgentSessionKind::RuntimeShell => ManagedAgentState::RuntimeShell,
            AgentSessionKind::PluginIteration => {
                return Err(RuntimeError::InvalidArgument {
                    message: "plugin_iteration agent sessions must be started by iterate_plugins"
                        .to_string(),
                });
            }
        };
        let profile_name = options.profile.as_deref().unwrap_or("default");
        let api = self
            .config_value()
            .llm_profiles
            .resolve(profile_name)
            .api
            .clone();
        let mut session = AgentSession::new(api, session_kind_label)?;
        session.set_soul_key(options.soul_key.clone());
        self.profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                handle.session_id.clone(),
                ProfileFallbackEntry {
                    desired: profile_name.to_string(),
                    degraded: false,
                },
            );
        // R1: 防御性标记——若同 id 仍有一个 in-flight turn（当前 id 由
        // `normalize_request_id` 生成、不会撞车），旧 turn 收尾不得插回
        // 覆盖本新会话。
        mark_dropped_if_inflight(
            &self.inflight_sessions,
            &self.dropped_during_turn,
            &handle.session_id,
        );
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                handle.session_id.clone(),
                ManagedAgentSession {
                    handle: handle.clone(),
                    session,
                    state,
                },
            );
        Ok(handle)
    }

    pub fn agent_send(&self, session_id: &str, input: &str) -> Result<AgentReply, RuntimeError> {
        self.agent_send_with_images(session_id, input, &[])
    }

    pub fn agent_send_with_turn_meta(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
        notes: &str,
        turn_meta: &BTreeMap<String, String>,
    ) -> Result<AgentReply, RuntimeError> {
        if !turn_meta.is_empty() {
            self.turn_meta
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(session_id.to_string(), turn_meta.clone());
        }
        let result = self.agent_send_inner(session_id, input, image_urls, notes);
        self.turn_meta
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        result
    }

    pub(crate) fn agent_send_inner(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
        notes: &str,
    ) -> Result<AgentReply, RuntimeError> {
        // R1: 摘除与 inflight 标记在同一把锁下原子完成（规范锁序
        // inflight → agent_sessions，见 `drop_session` / 收尾段）。并发
        // 第二个 `agent_send` 要么看到 map 里的会话、要么看到 inflight
        // 标记，不再有"已 remove 但未标记"的窗口，因此 in-flight 会话
        // 不会再被误报成 `AgentSessionNotFound`，而是 `AgentSessionBusy`。
        let mut session = {
            let mut inflight = self
                .inflight_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let mut guard = self
                .agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let Some(session) = guard.remove(session_id) else {
                return if inflight.contains(session_id) {
                    Err(RuntimeError::AgentSessionBusy {
                        session_id: session_id.to_string(),
                    })
                } else {
                    Err(RuntimeError::AgentSessionNotFound {
                        session_id: session_id.to_string(),
                    })
                };
            };
            inflight.insert(session_id.to_string());
            session
        };
        let images = self.image_parts_for_batch(image_urls);
        // R1: respond 期间可能 panic（agent 循环内的 assert、越界等）。
        // 用 catch_unwind 包住：panic 也继续走收尾，会话数据不丢失，
        // 返回值换成 Invariant 错误。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            session.respond_with_images(self, input, &images, notes)
        }));
        // P1-25: drain any pending actions queued by tools that couldn't
        // reach the session during `respond` (compact_context et al) and
        // apply them here, before reinserting into the map. Errors are
        // logged, not propagated, because the underlying tool call has
        // already returned "deferred" to the LLM.
        {
            let pending = {
                let mut guard = self
                    .pending_session_actions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                guard.remove(session_id).unwrap_or_default()
            };
            for action in pending {
                match action {
                    PendingSessionAction::CompactHistory => {
                        let (old, new) = session.session.compact_history();
                        eprintln!(
                            "[pending-action] compact_history for session {session_id}: {old} -> {new}"
                        );
                    }
                }
            }
        }
        // R1: 收尾。检查 dropped → auto_save → 插回 → 摘除 inflight 标记
        // 全程持 inflight 锁，与 `drop_session` 的标记互斥：不会出现
        // "drop 已执行、会话仍被复活并重写快照"的竞态。
        let mut inflight = self
            .inflight_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let dropped = self
            .dropped_during_turn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        let result = match outcome {
            Ok(result) => result,
            Err(payload) => {
                let err = agent_respond_panicked_error(&payload);
                eprintln!("[agent-send] session {session_id} respond panicked: {err}");
                Err(err)
            }
        };
        if !dropped {
            // Auto-save on success for RuntimeShell sessions so that
            // session context survives crashes and restarts.
            if result.is_ok() && matches!(session.state, ManagedAgentState::RuntimeShell) {
                self.auto_save_session(session_id, &session.session);
            }
            self.agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(session_id.to_string(), session);
        }
        inflight.remove(session_id);
        result
    }

    pub fn agent_send_with_fallback_turn_context(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
        notes: &str,
    ) -> Result<AgentReply, RuntimeError> {
        self.agent_send_with_fallback_turn_meta(
            session_id,
            input,
            image_urls,
            notes,
            &BTreeMap::new(),
        )
    }

    pub fn agent_send_with_fallback_turn_meta(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
        notes: &str,
        turn_meta: &BTreeMap<String, String>,
    ) -> Result<AgentReply, RuntimeError> {
        let entry = {
            let guard = self
                .profile_fallback
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.get(session_id).cloned()
        };
        let Some(entry) = entry else {
            // Sessions started via plain agent_start (REPL, tests) have no
            // fallback entry under a non-default registry only if created
            // before this map existed; treat as plain send.
            return self.agent_send_with_turn_meta(session_id, input, image_urls, notes, turn_meta);
        };

        // Recovery probe: while degraded, put the desired profile back
        // before this attempt. If it is still down we fall through to the
        // normal failure path below and re-degrade.
        if entry.degraded {
            self.swap_session_profile(session_id, &entry.desired)?;
        }

        match self.agent_send_with_turn_meta(session_id, input, image_urls, notes, turn_meta) {
            Ok(reply) => {
                if entry.degraded {
                    self.set_degraded(session_id, false);
                    let msg = format!(
                        "LLM profile '{}' recovered; session {} switched back",
                        entry.desired, session_id
                    );
                    eprintln!("[llm-profile] {msg}");
                    crate::kernel::notify::send(self, &format!("⚙️ {msg}"));
                }
                Ok(reply)
            }
            Err(primary_err) => {
                let config = self.config_value();
                let Some(fallback) = config.llm_profiles.fallback_of(&entry.desired) else {
                    return Err(primary_err);
                };
                let fallback = fallback.to_string();
                self.swap_session_profile(session_id, &fallback)?;
                match self
                    .agent_send_with_turn_meta(session_id, input, image_urls, notes, turn_meta)
                {
                    Ok(reply) => {
                        self.set_degraded(session_id, true);
                        let msg = format!(
                            "LLM profile '{}' failing ({}); session {} degraded to fallback '{}'",
                            entry.desired, primary_err, session_id, fallback
                        );
                        eprintln!("[llm-profile] {msg}");
                        self.kernel.observe_plugin_issue(
                            KernelPluginIssueSource::InvokeFailure,
                            "/llm-profile",
                            msg.clone(),
                        );
                        crate::kernel::notify::send(self, &format!("⚙️ {msg}"));
                        Ok(reply)
                    }
                    Err(fallback_err) => {
                        // Both profiles down. Restore the desired profile so
                        // the next attempt probes it first, and surface the
                        // original error (more representative).
                        let _ = self.swap_session_profile(session_id, &entry.desired);
                        self.set_degraded(session_id, false);
                        eprintln!(
                            "[llm-profile] fallback '{fallback}' also failed: {fallback_err}"
                        );
                        Err(primary_err)
                    }
                }
            }
        }
    }

    pub fn refresh_session_soul(
        &self,
        session_id: &str,
        soul_key: &str,
    ) -> Result<(), RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let not_found = || RuntimeError::AgentSessionNotFound {
            session_id: session_id.to_string(),
        };
        let managed = guard.get_mut(session_id).ok_or_else(not_found)?;
        managed.session.set_soul_key(soul_key.to_string());
        Ok(())
    }

    pub fn agent_record_outbound_reply(
        &self,
        session_id: &str,
        reply: &str,
    ) -> Result<bool, RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let not_found = || RuntimeError::AgentSessionNotFound {
            session_id: session_id.to_string(),
        };
        let managed = guard.get_mut(session_id).ok_or_else(not_found)?;
        Ok(managed.session.replace_last_action_with_reply(reply))
    }

    pub fn refresh_session_caller(
        &self,
        session_id: &str,
        is_group: bool,
        is_admin: bool,
        can_approve_iteration: bool,
    ) -> Result<(), RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let not_found = || RuntimeError::AgentSessionNotFound {
            session_id: session_id.to_string(),
        };
        let managed = guard.get_mut(session_id).ok_or_else(not_found)?;
        managed.session.set_caller_policy(is_group, is_admin);
        managed
            .session
            .set_approval_capability(can_approve_iteration);
        Ok(())
    }

    pub fn session_caller_policy(&self, session_id: &str) -> Option<(bool, bool)> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let managed = guard.get(session_id)?;
        Some(managed.session.caller_policy())
    }

    pub fn session_approval_capability(&self, session_id: &str) -> Option<bool> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let managed = guard.get(session_id)?;
        Some(managed.session.can_approve_iteration())
    }

    pub fn set_degraded(&self, session_id: &str, degraded: bool) {
        let mut guard = self
            .profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(entry) = guard.get_mut(session_id) {
            entry.degraded = degraded;
        }
    }

    pub(crate) fn agent_sessions_mut(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<String, ManagedAgentSession>> {
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn agent_inject(
        &self,
        session_id: &str,
        user_input: &str,
        assistant_output: &str,
    ) -> Result<(), RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session =
            guard
                .get_mut(session_id)
                .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                    session_id: session_id.to_string(),
                })?;
        session
            .session
            .inject_exchange(user_input, assistant_output);
        Ok(())
    }

    pub fn agent_status(&self, session_id: &str) -> Result<AgentSessionStatus, RuntimeError> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session = guard
            .get(session_id)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        Ok(session.session.status())
    }

    pub fn agent_transcript(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentTranscriptEntry>, RuntimeError> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session = guard
            .get(session_id)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        Ok(session.session.transcript().to_vec())
    }
}
