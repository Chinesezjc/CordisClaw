use super::*;

impl<'a> PluginIterationAgentBackend<'a> {
    pub(crate) fn phase(&self) -> &'static str {
        if self.state.recorded_summary.is_some() {
            "finalized"
        } else if self.state.operations.is_empty() {
            "exploration"
        } else if self.state.verification_attempts == 0 {
            "editing"
        } else if self.state.verification_successes == 0 {
            "verification_retry"
        } else {
            "verification"
        }
    }

    pub(crate) fn apply_operations(
        &mut self,
        summary: &str,
        operations: Vec<PluginEditOperation>,
    ) -> Result<Value, RuntimeError> {
        let mut combined_operations = self.state.operations.clone();
        combined_operations.extend(operations.clone());
        let writable_roots = self
            .state
            .prepared
            .allowed_plugin_roots
            .values()
            .cloned()
            .collect::<BTreeSet<_>>();
        validate_reserved_child_keyword_identifiers(&combined_operations, &writable_roots)?;

        let executor = PluginEditExecutor::new(&self.host.fixtures_root);
        let mut local_rollback = PluginEditRollback::empty(&self.host.fixtures_root);
        for (idx, operation) in operations.iter().enumerate() {
            let single = PluginEditPlan {
                issue_id: self.state.prepared.issue_id.clone(),
                patch_id: format!("{}-tool-{}", self.state.prepared.iteration_id, idx),
                summary: summary.to_string(),
                operations: vec![operation.clone()],
            };
            let execute = executor.execute(
                &self.host.kernel.plugin_iteration_policy,
                &self.state.prepared.allowed_plugin_roots,
                &single,
            );
            match execute {
                Ok((_apply_result, rollback)) => {
                    local_rollback.absorb(rollback)?;
                }
                Err(err) => {
                    let rollback_err = local_rollback.rollback().err();
                    let enriched = enrich_plugin_iteration_edit_error(
                        operation,
                        &self.host.fixtures_root,
                        err,
                    );
                    return Err(with_partial_batch_rollback_failure(enriched, rollback_err));
                }
            }
        }
        self.state.rollback.absorb(local_rollback)?;

        // Persist the rollback journal to disk after every tool execution so
        // that a crash mid-agent-loop still leaves a recoverable journal on
        // restart — no window where files are modified but no backup exists.
        // Bound out of the `?` so the fallible expression fits one line.
        let persisted = self.state.rollback.persist_journal(
            &plugin_iteration_journal_path(&self.host.snapshot_root),
            &self.state.prepared.iteration_id,
        );
        persisted?;

        self.state.operations.extend(operations.clone());
        for path in operations.into_iter().map(|operation| operation.path) {
            if should_track_context_file(&path) {
                self.state.focus_context_paths.push(path.clone());
                self.state.all_context_paths.push(path);
            }
        }
        sort_and_dedup_context_paths(&mut self.state.focus_context_paths);
        sort_and_dedup_context_paths(&mut self.state.all_context_paths);
        let derived = self.state.derived_edit_plan();
        Ok(json!({
            "changed_paths": derived.changed_paths(),
            "operation_count": derived.operations.len(),
        }))
    }

    pub(crate) fn replace_file_exact_operation(args: ReplaceFileExactArgs) -> PluginEditOperation {
        PluginEditOperation {
            path: args.path,
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some(args.expected_old_string),
            expected_sha256: None,
            new_content: Some(args.new_content),
            pointer: None,
            dotted_key: None,
            value: None,
        }
    }

    pub(crate) fn visible_context_paths(&self) -> &[String] {
        if self.state.context_scope_expanded {
            &self.state.all_context_paths
        } else {
            &self.state.focus_context_paths
        }
    }

    pub(crate) fn list_context_files(&mut self, scope: ContextFilesScope) -> Value {
        if scope == ContextFilesScope::All {
            self.state.context_scope_expanded = true;
        }

        let mut focus_paths = self.state.focus_context_paths.clone();
        let mut paths = match scope {
            ContextFilesScope::Focus => focus_paths.clone(),
            ContextFilesScope::All => self.state.all_context_paths.clone(),
        };
        sort_plugin_context_paths(&mut focus_paths);
        sort_plugin_context_paths(&mut paths);
        json!({
            "root_plugin_path": self.state.prepared.root_plugin_path,
            "phase": self.phase(),
            "scope": match scope {
                ContextFilesScope::Focus => "focus",
                ContextFilesScope::All => "all",
            },
            "scope_expanded": self.state.context_scope_expanded,
            "hidden_count": self.state.all_context_paths.len().saturating_sub(paths.len()),
            "focus_paths": focus_paths,
            "paths": paths,
        })
    }

    pub(crate) fn read_context_path(&self, path: &str) -> Result<Value, RuntimeError> {
        let normalized = normalize_rel_path(path)?;
        if !self
            .state
            .all_context_paths
            .iter()
            .any(|item| item == &normalized)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "context file is not available in this plugin iteration session: {normalized}"
                ),
            });
        }
        if !self
            .visible_context_paths()
            .iter()
            .any(|item| item == &normalized)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "context file is currently hidden behind the structural focus shortlist: {normalized}. Call list_context_files with `{{\"scope\":\"all\"}}` before reading deeper subtree files."
                ),
            });
        }
        let abs_path = self.host.fixtures_root.join(&normalized);
        let content =
            fs::read_to_string(&abs_path).map_err(|err| region_io_error(&abs_path, &err))?;
        Ok(json!({
            "path": normalized,
            "sha256": sha256_text(&content),
            "content": content,
        }))
    }

    pub(crate) fn read_context_files(&self, paths: &[String]) -> Result<Value, RuntimeError> {
        let files = paths
            .iter()
            .map(|path| self.read_context_path(path))
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        Ok(json!({ "files": files }))
    }

    pub(crate) fn inspect_plugin_catalog(&self) -> Value {
        let snapshot = self.host.current_snapshot();
        let plugins = snapshot
            .plugin_registry()
            .iter()
            .filter(|(plugin_path, _)| {
                plugin_path == &self.state.prepared.root_plugin_path
                    || plugin_path.starts_with(&format!("{}/", self.state.prepared.root_plugin_path))
            })
            .map(|(plugin_path, plugin)| {
                json!({
                    "plugin_path": plugin_path,
                    "parent": plugin.parent,
                    "required": plugin.required,
                    "node_ids": plugin
                        .docs
                        .as_ref()
                        .map(|docs| docs.nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>())
                        .unwrap_or_default(),
                    "node_summaries": plugin
                        .docs
                        .as_ref()
                        .map(|docs| docs.nodes.iter().map(|node| {
                            json!({
                                "node_id": node.id,
                                "summary": node.summary,
                            })
                        }).collect::<Vec<_>>())
                        .unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "root_plugin_path": self.state.prepared.root_plugin_path,
            "plugins": plugins,
        })
    }

    pub(crate) fn scaffold_child_plugin(
        &mut self,
        args: ScaffoldChildPluginArgs,
    ) -> Result<Value, RuntimeError> {
        if !self
            .state
            .prepared
            .target_plugin_paths
            .iter()
            .any(|path| path == &args.parent_plugin_path)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "parent plugin path {} is outside the selected subtree",
                    args.parent_plugin_path
                ),
            });
        }

        let child_segment = sanitize_child_plugin_segment(&args.child_name);
        let child_plugin_path = format!("{}/{}", args.parent_plugin_path, child_segment);
        let child_root = format!("plugins/{child_plugin_path}");
        let node_id = args
            .node_id
            .clone()
            .unwrap_or_else(|| format!("{}_entry", child_segment.replace('-', "_")));
        let crate_name = child_plugin_path.replace(['/', '-'], "_");
        let summary = args
            .summary
            .clone()
            .unwrap_or_else(|| format!("Child plugin scaffold for {child_plugin_path}"));

        let parent_manifest_rel = format!("plugins/{}/Cargo.toml", args.parent_plugin_path);
        let parent_manifest_abs = self.host.fixtures_root.join(&parent_manifest_rel);
        let parent_manifest_text = fs::read_to_string(&parent_manifest_abs)
            .map_err(|err| region_io_error(&parent_manifest_abs, &err))?;
        let parent_manifest_sha = file_sha256(&parent_manifest_abs)?;
        let parent_toml: TomlValue = toml::from_str(&parent_manifest_text)
            .map_err(|err| parent_manifest_parse_error(&parent_manifest_abs, &err))?;
        let mut children = parent_toml
            .get("package")
            .and_then(TomlValue::as_table)
            .and_then(|value| value.get("metadata"))
            .and_then(TomlValue::as_table)
            .and_then(|value| value.get("cordis"))
            .and_then(TomlValue::as_table)
            .and_then(|value| value.get("children"))
            .and_then(TomlValue::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        let child_source = format!("./{child_segment}");
        if !children.iter().any(|entry| {
            entry
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|value| value == child_source)
        }) {
            children.push(json!({
                "source": child_source,
                "required": true,
                "grants": [],
            }));
        }

        let manifest_path = format!("{child_root}/Cargo.toml");
        let lib_path = format!("{child_root}/src/lib.rs");
        let core_path = format!("{child_root}/src/core.rs");
        let test_path = format!(
            "{child_root}/tests/{}_scaffold.rs",
            child_segment.replace('-', "_")
        );
        let human_path = format!("{child_root}/docs/human/overview.md");

        let operations = vec![
            PluginEditOperation {
                path: parent_manifest_rel.clone(),
                kind: PluginEditOpKind::TomlSet,
                expected_old_string: None,
                expected_sha256: Some(parent_manifest_sha),
                new_content: None,
                pointer: None,
                dotted_key: Some("package.metadata.cordis.children".to_string()),
                value: Some(Value::Array(children)),
            },
            PluginEditOperation {
                path: manifest_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_manifest(
                    &crate_name,
                    &child_plugin_path,
                    &node_id,
                )),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: lib_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_lib(
                    &crate_name,
                    &child_plugin_path,
                    &node_id,
                    &summary,
                )),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: core_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_core(&child_segment)),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: test_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_test(&crate_name)),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: human_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_overview(&child_plugin_path)),
                pointer: None,
                dotted_key: None,
                value: None,
            },
        ];
        let applied = self.apply_operations("scaffold_child_plugin", operations)?;
        self.state
            .scaffolded_children
            .push(ScaffoldedChildRegistration {
                parent_manifest_path: parent_manifest_rel.clone(),
                child_root_path: child_root.clone(),
            });
        self.state
            .scaffolded_children
            .sort_by(scaffolded_child_order);
        self.state.scaffolded_children.dedup();
        Ok(json!({
            "child_plugin_path": child_plugin_path,
            "template_plugin_path": args.template_plugin_path,
            "normalized_child_name": child_segment,
            "node_id": node_id,
            "parent_manifest_path": parent_manifest_rel,
            "created_paths": [manifest_path, lib_path, core_path, test_path, human_path],
            "result": applied,
        }))
    }

    pub(crate) fn run_checked_command(
        &mut self,
        stage: &str,
        command: String,
    ) -> Result<Value, RuntimeError> {
        self.state.verification_attempts += 1;
        // P0-17 alignment: the old `bash -lc <command>` dispatch let a `;` /
        // `&&` / `|` / backtick / `$()` tail after the prefix-validated cargo
        // invocation run arbitrary commands. Tokenise (POSIX quoting, no
        // shell expansion) and dispatch `cargo <op> …` directly with
        // `Command::new`; the argv gate is re-applied here as defence in
        // depth even though `validated_verification_command` already ran it.
        let required_op = if stage == "check" { "check" } else { "test" };
        let argv = validate_verification_argv(&command, required_op)?;
        let output = Command::new(&argv[0])
            .args(&argv[1..])
            .current_dir(&self.host.fixtures_root)
            .output()
            .map_err(|err| checked_command_spawn_error(&argv[0], &argv[1..], &err))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            let warning_diagnostics = warning_diagnostics_for_changed_paths(
                &stdout,
                &stderr,
                &self.state.operations,
                &self.host.fixtures_root,
            );
            if !warning_diagnostics.is_empty() {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: warning_cleanup_error_message(&command, &warning_diagnostics),
                });
            }
            self.state.verification_successes += 1;
        }
        Ok(json!({
            "stage": stage,
            "command": command,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
        }))
    }
}

impl<'a> AgentBackend for PluginIterationAgentBackend<'a> {
    type Host = RuntimeHost;
    fn host(&self) -> &RuntimeHost {
        self.host
    }
    fn system_prompt(&self) -> String {
        format!(
            "You are the Cordis plugin-iteration agent.\n\
Work directly through tools instead of proposing a large final JSON plan.\n\
You may only modify the selected plugin subtree rooted at {}.\n\
Start by calling list_context_files to inspect the current structural shortlist of readable files.\n\
The default context scope exposes structural source anchors for the root plugin, its direct children, and one nested child-plugin source layer. If you need deeper tests, docs, or additional subtree files, call list_context_files with `{{\"scope\":\"all\"}}` before reading them.\n\
Use read_context_files in batches instead of one file at a time, and avoid repeated list/read loops once you have enough structure to edit.\n\
Use replace_files_exact for source edits, including related updates across multiple files or multiple exact replacements you already understand. Group related edits into as few tool turns as possible.\n\
If an exact-replace tool reports invalid JSON or a stale exact-match pattern, reread the affected file and retry with a smaller replacement call instead of continuing from stale assumptions.\n\
Reserve inspect_plugin_catalog and any single-file follow-up cleanup only for cases where the structural file list and batched reads still leave ambiguity.\n\
Keep the new plugin architecture aligned with existing sibling plugins in the selected subtree instead of inventing a one-off layout.\n\
run_plugin_check and run_plugin_test both have safe defaults: call them with `{{}}` to run `cargo check --quiet --manifest-path plugins/Cargo.toml` or `cargo test --quiet --manifest-path plugins/Cargo.toml`.\n\
Use run_plugin_check and run_plugin_test until the files you changed are warning-free; if a verification command reports warnings in edited files, treat the work as incomplete and fix them before moving on.\n\
Once a warning-free check succeeds, stop exploring unless a later tool fails. Immediately run rebuild_plugin_workspace, then run_plugin_test, then record_iteration_summary.\n\
Use rebuild_plugin_workspace to refresh artifacts and generated docs after edits, but rebuild_plugin_workspace alone does not satisfy the final verification requirement.\n\
If a child plugin path uses a Rust keyword such as `mod`, keep that keyword in filesystem and `plugin_path` positions like `expr/evaluator/mod`. Type names such as `ModPlugin` and `ModError` are valid, but raw lower-case source identifiers such as a field, local, parameter, alias, or member named `mod` are invalid; prefer names like `modulo` or `mod_plugin` for those Rust identifiers.\n\
Replace placeholder scaffold implementations, tests, and docs together once the behavior is real, and do not stop after scaffolding a child plugin without wiring or testing it from the host subtree.\n\
When the iteration is ready, call record_iteration_summary with a concise summary and any recommended verification commands. record_iteration_summary must be your last tool call and it ends the session immediately.\n\
Do not attempt to modify runtime crates, repository root manifests, config, .git, target, or generated docs under docs/agent.",
            self.state.prepared.root_plugin_path
        )
    }

    fn tool_specs(&self) -> Vec<AgentToolSpec> {
        let mut tools = vec![
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES,
                description: "List readable context files for the selected plugin subtree. Defaults to the structural focus shortlist; use scope=all to expand the visible context set.",
                parameters: json!({"type":"object","properties":{"scope":{"type":"string","enum":["focus","all"]}},"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES,
                description: "Read multiple currently visible context files in one call. If a needed file is hidden behind the focus shortlist, expand first with list_context_files(scope=all).",
                parameters: json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"}}},"required":["paths"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG,
                description: "Inspect the currently loaded plugin subtree, including child plugins and node summaries.",
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN,
                description: "Create a sibling child plugin scaffold under the selected subtree and register it in the parent manifest.",
                parameters: json!({"type":"object","properties":{"parent_plugin_path":{"type":"string"},"child_name":{"type":"string"},"template_plugin_path":{"type":"string"},"node_id":{"type":"string"},"summary":{"type":"string"}},"required":["parent_plugin_path","child_name"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT,
                description: "Replace an exact string in one writable file. Prefer replace_files_exact unless this is truly a single-file follow-up.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_old_string":{"type":"string"},"new_content":{"type":"string"}},"required":["path","expected_old_string","new_content"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
                description: "Replace exact strings in one or more writable files in one call. Prefer this for nearly all source edits so related changes land in the same tool turn.",
                parameters: json!({"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"expected_old_string":{"type":"string"},"new_content":{"type":"string"}},"required":["path","expected_old_string","new_content"],"additionalProperties":false},"minItems":1}},"required":["edits"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_CREATE_FILE,
                description: "Create a new writable file inside the selected plugin subtree.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"new_content":{"type":"string"}},"required":["path","new_content"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_DELETE_FILE,
                description: "Delete a writable file when you know its expected sha256.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_sha256":{"type":"string"}},"required":["path","expected_sha256"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_TOML_SET,
                description: "Set one TOML dotted key inside a writable manifest using an expected sha256 guard.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_sha256":{"type":"string"},"dotted_key":{"type":"string"},"value":{}},"required":["path","expected_sha256","dotted_key","value"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_JSON_SET,
                description: "Set one JSON pointer inside a writable file using an expected sha256 guard.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_sha256":{"type":"string"},"pointer":{"type":"string"},"value":{}},"required":["path","expected_sha256","pointer","value"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK,
                description: "Run cargo check. plugin_path: \"/\" for whole workspace, \"/qq\" for a single plugin. Pass a custom command to override.",
                parameters: json!({"type":"object","properties":{"command":{"type":"string"},"plugin_path":{"type":"string","description":"\"/\" = all, \"/qq\" = single plugin"}},"required":["plugin_path"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST,
                description: "Run cargo test. plugin_path: \"/\" for whole workspace, \"/qq\" for a single plugin. Pass a custom command to override.",
                parameters: json!({"type":"object","properties":{"command":{"type":"string"},"plugin_path":{"type":"string","description":"\"/\" = all, \"/qq\" = single plugin"}},"required":["plugin_path"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE,
                description: "Rebuild plugin artifacts and sync generated docs. plugin_path: \"/\" for whole workspace, \"/qq\" for a single plugin.",
                parameters: json!({"type":"object","properties":{"plugin_path":{"type":"string","description":"\"/\" = all, \"/qq\" = single plugin"}},"required":["plugin_path"],"additionalProperties":false}),
            },
        ];
        if self.state.verification_successes > 0 && self.state.recorded_summary.is_none() {
            tools.push(AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY,
                description: "Record the final iteration summary and optional verification commands. This must be your last tool call and ends the iteration session immediately.",
                parameters: json!({"type":"object","properties":{"summary":{"type":"string"},"tests_command":{"type":"string"},"safety_command":{"type":"string"}},"required":["summary"],"additionalProperties":false}),
            });
        }
        tools
    }

    fn execute_tool(&mut self, name: &str, arguments: Value) -> Result<Value, RuntimeError> {
        match name {
            PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES => {
                let args = parse_agent_args::<ListContextFilesArgs>(arguments, name)?;
                let scope = parse_context_files_scope(args.scope.as_deref())?;
                Ok(self.list_context_files(scope))
            }
            PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES => {
                let args = parse_agent_args::<ReadContextFilesArgs>(arguments, name)?;
                self.read_context_files(&args.paths)
            }
            PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG => Ok(self.inspect_plugin_catalog()),
            PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN => {
                let args = parse_agent_args::<ScaffoldChildPluginArgs>(arguments, name)?;
                self.scaffold_child_plugin(args)
            }
            PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT => {
                let args = parse_agent_args::<ReplaceFileExactArgs>(arguments, name)?;
                self.apply_operations(
                    "replace_file_exact",
                    vec![Self::replace_file_exact_operation(args)],
                )
            }
            PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT => {
                let args = parse_agent_args::<ReplaceFilesExactArgs>(arguments, name)?;
                if args.edits.is_empty() {
                    return Err(RuntimeError::InvalidArgument {
                        message: "replace_files_exact requires at least one edit".to_string(),
                    });
                }
                self.apply_operations(
                    "replace_files_exact",
                    args.edits
                        .into_iter()
                        .map(Self::replace_file_exact_operation)
                        .collect::<Vec<_>>(),
                )
            }
            PLUGIN_AGENT_TOOL_CREATE_FILE => {
                let args = parse_agent_args::<CreateFileArgs>(arguments, name)?;
                self.apply_operations(
                    "create_file",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::CreateFile,
                        expected_old_string: Some(String::new()),
                        expected_sha256: None,
                        new_content: Some(args.new_content),
                        pointer: None,
                        dotted_key: None,
                        value: None,
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_DELETE_FILE => {
                let args = parse_agent_args::<DeleteFileArgs>(arguments, name)?;
                self.apply_operations(
                    "delete_file",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::DeleteFile,
                        expected_old_string: None,
                        expected_sha256: Some(args.expected_sha256),
                        new_content: None,
                        pointer: None,
                        dotted_key: None,
                        value: None,
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_TOML_SET => {
                let args = parse_agent_args::<TomlSetArgs>(arguments, name)?;
                self.apply_operations(
                    "toml_set",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::TomlSet,
                        expected_old_string: None,
                        expected_sha256: Some(args.expected_sha256),
                        new_content: None,
                        pointer: None,
                        dotted_key: Some(args.dotted_key),
                        value: Some(args.value),
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_JSON_SET => {
                let args = parse_agent_args::<JsonSetArgs>(arguments, name)?;
                self.apply_operations(
                    "json_set",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::JsonSet,
                        expected_old_string: None,
                        expected_sha256: Some(args.expected_sha256),
                        new_content: None,
                        pointer: Some(args.pointer),
                        dotted_key: None,
                        value: Some(args.value),
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK => {
                let args = parse_agent_args::<RunPluginCommandArgs>(arguments, name)?;
                let pp = args.plugin_path.as_deref().unwrap_or("/");
                // The LLM-controlled plugin path is validated up front (never
                // string-interpolated) and appended as its own argv element.
                let package_segment = verification_package_segment(pp)?;
                let default = verification_default_command("check", &package_segment);
                let explicit = normalize_optional_command(args.command);
                let command =
                    validated_verification_command(explicit, Some(default), "cargo check")?;
                self.run_checked_command("check", command)
            }
            PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST => {
                let args = parse_agent_args::<RunPluginCommandArgs>(arguments, name)?;
                let pp = args.plugin_path.as_deref().unwrap_or("/");
                let package_segment = verification_package_segment(pp)?;
                let default = verification_default_command("test", &package_segment);
                let prepared_tests = self.state.prepared.tests_command.clone();
                let explicit = normalize_optional_command(args.command)
                    .or_else(|| normalize_optional_command(prepared_tests));
                let command =
                    validated_verification_command(explicit, Some(default), "cargo test")?;
                self.run_checked_command("test", command)
            }
            PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE => {
                self.state.verification_attempts += 1;
                let args: serde_json::Value = arguments;
                let pp = args
                    .get("plugin_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/");
                let rebuilt = plugin_builder()
                    .ok_or_else(missing_builder_error)?
                    .rebuild_plugin(&self.host.fixtures_root, pp)?;
                Ok(json!({
                    "rebuilt_count": rebuilt.len(),
                    "rebuilt": rebuilt,
                    "counts_as_warning_free_verification": false,
                }))
            }
            PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY => {
                if self.state.operations.is_empty() || self.state.verification_successes == 0 {
                    return Err(RuntimeError::InvalidArgument {
                        message: "record_iteration_summary requires at least one edit and one successful verification step".to_string(),
                    });
                }
                // The promotion policy lives in `iteration::policy`: this call
                // site only reports its answer.
                if let Some(reason) = summary_rejection(&self.state.operations) {
                    return Err(RuntimeError::InvalidArgument {
                        message: reason.to_string(),
                    });
                }
                self.state.requires_approval =
                    !test_changes(&self.state.operations).modified.is_empty();
                let integration = ensure_scaffold_integration_edits(
                    &self.state.scaffolded_children,
                    &self.state.operations,
                );
                integration?;
                let args = parse_agent_args::<RecordIterationSummaryArgs>(arguments, name)?;
                self.state.recorded_summary = Some(args.summary.clone());
                self.state.tests_command = normalize_optional_command(args.tests_command);
                self.state.safety_command = normalize_optional_command(args.safety_command);
                Ok(json!({
                    "summary": args.summary,
                    "tests_command": self.state.tests_command,
                    "safety_command": self.state.safety_command,
                    "verification_attempts": self.state.verification_attempts,
                    "verification_successes": self.state.verification_successes,
                }))
            }
            other => Err(RuntimeError::InvalidArgument {
                message: format!("unsupported plugin iteration tool: {other}"),
            }),
        }
    }

    fn terminal_tool_reply(&self, name: &str, _output: &Value) -> Option<String> {
        (name == PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY)
            .then_some("Plugin iteration summary recorded.".to_string())
    }

    fn tool_scope_label(&self) -> String {
        format!("plugin_iteration:{}", self.phase())
    }
}
