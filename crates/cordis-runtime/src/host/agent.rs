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
        // A dropped session leaves the organisation tree and forgets what it
        // was told its teammates were.
        self.session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.team_snapshots
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
        let mut session = crate::agent_loop::agent_loop()
            .ok_or_else(crate::agent_loop::missing_loop_error)?
            .create(api, session_kind_label)?;
        session.set_soul_key(options.soul_key.clone());
        session.set_require_end_tool(options.require_end_tool);
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
        self.session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                handle.session_id.clone(),
                SessionNode {
                    kind: session_kind_label.to_string(),
                    address: None,
                    spawned_by: None,
                    live: true,
                },
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
                    address: None,
                    spawned_by: None,
                },
            );
        Ok(handle)
    }

    /// Force the end-of-turn contract on a live session.
    ///
    /// Sessions restored from a snapshot keep whatever flag they were saved
    /// with, so a channel must re-assert it before every send; otherwise a
    /// recovered session answers in plain text and the room never hears it.
    pub fn agent_set_require_end_tool(&self, session_id: &str, require: bool) {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(managed) = guard.get_mut(session_id) {
            managed.session.set_require_end_tool(require);
        }
    }

    /// Designate the process's main session (the one `cordis-cli` boots).
    ///
    /// The main session is the only true root: its `spawned_by` stays `None`.
    /// It is also the fallback parent for work whose initiator cannot be named
    /// — iteration sessions today; see the open item in
    /// `docs/architecture/status-and-open-items.md`.
    pub fn set_main_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        if self.main_session_id() == Some(session_id.to_string()) {
            return Ok(());
        }
        let live = self
            .session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .is_some_and(|node| node.live);
        if !live {
            return Err(RuntimeError::InvalidArgument {
                message: format!("agent session {session_id} is not live"),
            });
        }
        *self
            .main_session_id
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(session_id.to_string());
        Ok(())
    }

    /// The main session's id, when this process has one registered.
    pub fn main_session_id(&self) -> Option<String> {
        self.main_session_id
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    /// Record the one structural edge the tree has: `child` was created by
    /// `parent`. Both must be live in this process.
    pub fn set_session_parent(&self, child_id: &str, parent_id: &str) -> Result<(), RuntimeError> {
        if child_id == parent_id {
            return Err(RuntimeError::InvalidArgument {
                message: format!("agent session {child_id} cannot be its own parent"),
            });
        }
        let live_parent = self
            .session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(parent_id)
            .is_some_and(|node| node.live);
        if !live_parent {
            return Err(RuntimeError::InvalidArgument {
                message: format!("agent session {parent_id} is not live"),
            });
        }
        {
            let mut nodes = self
                .session_nodes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let node = nodes
                .get_mut(child_id)
                .filter(|node| node.live)
                .ok_or_else(|| RuntimeError::InvalidArgument {
                    message: format!("agent session {child_id} is not live"),
                })?;
            node.spawned_by = Some(parent_id.to_string());
        }
        // Mirror the edge into the managed record while it is in the map; the
        // node map stays authoritative because a session is briefly absent
        // from `agent_sessions` for the duration of its turn.
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(managed) = guard.get_mut(child_id) {
            managed.spawned_by = Some(parent_id.to_string());
        }
        Ok(())
    }

    /// Hang a just-created session off the main session, if there is one.
    ///
    /// Used by `cordis-cli` for channel rooms and for the background
    /// reflection/review sessions, whose initiator is not a session at all.
    /// Without a registered main (embedding, unit tests) the session stays a
    /// root instead of failing the caller.
    pub fn attach_session_to_main(&self, session_id: &str) -> Result<(), RuntimeError> {
        let Some(main) = self.main_session_id() else {
            return Ok(());
        };
        if main == session_id {
            return Ok(());
        }
        self.set_session_parent(session_id, &main)
    }

    /// The structural parent recorded for a session, if any.
    pub fn session_parent(&self, session_id: &str) -> Option<String> {
        self.session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .and_then(|node| node.spawned_by.clone())
    }

    /// The structural facts about the caller's neighbourhood.
    ///
    /// Pure edges, no labels: who created me, who I created, who shares my
    /// kind of room, and the deduplicated union of those (never including me).
    /// Whether someone is a peer, a helper or the person in charge is inferred
    /// by asking them — the code never declares it.
    ///
    /// Only sessions live in this process appear. Crash recovery reads hundreds
    /// of historical snapshots back into `agent_sessions`; none of them has an
    /// agent anyone can talk to, so none of them shows up here.
    pub fn agent_my_team(&self, session_id: &str) -> Result<Value, RuntimeError> {
        let nodes = self
            .session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let me = nodes
            .get(session_id)
            .filter(|node| node.live)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        let room_kind = me.room_kind();
        let mut created: Vec<String> = Vec::new();
        let mut same_room: Vec<String> = Vec::new();
        for (id, node) in nodes.iter() {
            if !node.live || id == session_id {
                continue;
            }
            if node.spawned_by.as_deref() == Some(session_id) {
                created.push(id.clone());
            }
            if node.room_kind() == room_kind {
                same_room.push(id.clone());
            }
        }
        // A parent that is gone is not a teammate: drop the edge rather than
        // leak an orphan's id into someone's `my_team`.
        let spawned_by = me.spawned_by.clone().filter(|parent| {
            nodes
                .get(parent)
                .is_some_and(|node| node.live && parent != session_id)
        });
        let mut reachable: BTreeSet<String> = BTreeSet::new();
        if let Some(parent) = &spawned_by {
            reachable.insert(parent.clone());
        }
        reachable.extend(created.iter().cloned());
        reachable.extend(same_room.iter().cloned());
        reachable.remove(session_id);
        drop(nodes);
        // Remember what this caller was told: `send_to_session` only accepts
        // ids that appeared here.
        self.team_snapshots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id.to_string(), reachable.clone());
        let reachable: Vec<String> = reachable.into_iter().collect();
        Ok(serde_json::json!({
            "me": session_id,
            "spawned_by": spawned_by,
            "created": created,
            "same_room": same_room,
            "reachable": reachable,
        }))
    }

    /// Deliver a message to a session the caller has actually been introduced
    /// to.
    ///
    /// The target must appear in the caller's most recent `my_team` reply.
    /// Peer-to-peer sends are allowed — the tree is structure, not a chain of
    /// command — but an id from nowhere is refused: the model has to ask who
    /// it knows before addressing anyone.
    pub fn agent_send_to_session(
        &self,
        caller_session_id: &str,
        target: &str,
        input: &str,
    ) -> Result<Value, RuntimeError> {
        let resolved = self
            .agent_resolve_session(target)
            .ok_or_else(|| unknown_team_member(target))?;
        let known = self
            .team_snapshots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(caller_session_id)
            .cloned();
        if !known.is_some_and(|set| set.contains(&resolved)) {
            return Err(unknown_team_member(target));
        }
        let reply = self.agent_send(&resolved, input)?;
        Ok(serde_json::json!({
            "ok": true,
            "session_id": resolved,
            "reply_chars": reply.content.chars().count(),
            "tool_calls": reply.tool_events.len(),
        }))
    }

    /// Mark a session's tree node live. Membership in the node map is what
    /// makes a session a teammate, so this is the one place liveness flips.
    fn mark_session_node_live(&self, session_id: &str) {
        let mut nodes = self
            .session_nodes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        for node in nodes.get_mut(session_id).into_iter() {
            node.live = true;
        }
    }

    /// Give a session a stable, externally meaningful address.
    ///
    /// `agent_start` mints an opaque id; a channel adapter knows the room, so it
    /// registers that here and other sessions can then reach it by address.
    /// Registering an address also makes the session live: a recovered session
    /// whose room speaks again is a teammate from that moment on.
    pub fn agent_set_session_address(
        &self,
        session_id: &str,
        address: &str,
    ) -> Result<(), RuntimeError> {
        let main = self.main_session_id();
        let parent = {
            let mut nodes = self
                .session_nodes
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let node = nodes
                .get_mut(session_id)
                .ok_or_else(|| RuntimeError::InvalidArgument {
                    message: format!("agent session {session_id} is not live"),
                })?;
            node.address = Some(address.to_string());
            node.live = true;
            // A channel-registered session is a room and hangs off the main
            // session unless it already has a parent.
            if node.spawned_by.is_none() && Some(session_id) != main.as_deref() {
                node.spawned_by = main;
            }
            node.spawned_by.clone()
        };
        // Mirror into the managed record while it is in the map; the node map
        // is what `my_team` reads, so a session that is mid-turn (and therefore
        // temporarily out of the map) keeps its address either way.
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(managed) = guard.get_mut(session_id) {
            managed.address = Some(address.to_string());
            managed.spawned_by = parent;
        }
        Ok(())
    }

    /// Resolve an id *or* an address to a live session id.
    ///
    /// Ids win: an address is only consulted when no live id matches, so a
    /// session cannot shadow another by picking a colliding address.
    pub fn agent_resolve_session(&self, id_or_address: &str) -> Option<String> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if guard.contains_key(id_or_address) {
            return Some(id_or_address.to_string());
        }
        guard
            .iter()
            .find(|(_, managed)| managed.address.as_deref() == Some(id_or_address))
            .map(|(id, _)| id.clone())
    }

    /// The address a session was registered under, if any.
    pub fn agent_session_address(&self, session_id: &str) -> Option<String> {
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(session_id)
            .and_then(|managed| managed.address.clone())
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
        // The node map outlives this removal: a tool call runs while the
        // session is out of `agent_sessions`, and its `my_team` must still
        // find its caller. Running a turn also proves the session is alive, so
        // a recovered one joins the tree here.
        self.mark_session_node_live(session_id);
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
                        // 队列里的压缩在 turn 结束后执行，此时 provider 插件
                        // 仍按会话同一条 llm_complete 接缝解析；没有 provider
                        // 就传 None，走截断拼贴兜底。
                        let provider = self.compact_provider();
                        let provider = provider
                            .as_ref()
                            .map(|provider| provider as &dyn crate::agent_api::LlmProvider);
                        let (old, new) = session.session.compact_history(provider);
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
                self.auto_save_session(session_id, session.session.as_ref());
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

    /// Append a user message to a session's history **without running a turn**.
    ///
    /// A read-only channel (a group whose reply policy is disabled) uses this:
    /// the text still lands in history and in the snapshot, but no provider is
    /// called and no assistant reply exists. When the policy is turned back on,
    /// the next real turn sees everything that accumulated.
    pub fn agent_ingest(&self, session_id: &str, input: &str) -> Result<(), RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let managed =
            guard
                .get_mut(session_id)
                .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                    session_id: session_id.to_string(),
                })?;
        managed.session.ingest_user_message(input);
        // Persist immediately for the same reason `agent_send` does: a read-only
        // group may never get another turn, and the whole point is that its
        // context survives a restart.
        if matches!(managed.state, ManagedAgentState::RuntimeShell) {
            self.auto_save_session(session_id, managed.session.as_ref());
        }
        // Ingesting proves the session is alive (a recovered read-only room is
        // a teammate again, even though it never runs a turn).
        self.mark_session_node_live(session_id);
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

/// The refusal for a `send_to_session` target the caller was never introduced
/// to.
///
/// It names the fix, not just the failure: the model has to call `my_team`
/// first, then address someone from that reply. Kept as a free function so the
/// message has exactly one home and can be asserted byte-for-byte.
fn unknown_team_member(target: &str) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: format!(
            "unknown session {target}: it is not in your reachable set. \
             Call my_team first to see who you know."
        ),
    }
}
