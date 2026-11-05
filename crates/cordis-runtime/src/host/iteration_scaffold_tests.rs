use super::*;

/// Minimal fixtures tree the host can boot on: an empty artifact index and
/// a plugins workspace manifest.
fn minimal_fixture_tree() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    fs::create_dir_all(fixtures.join("plugins")).expect("plugins dir");
    fs::create_dir_all(fixtures.join("artifacts")).expect("artifacts dir");
    fs::write(
        fixtures.join("artifacts/index.json"),
        r#"{"schema_version":2,"generated_at":"now","topo_order":[],"entries":[]}"#,
    )
    .expect("index");
    fs::write(
        fixtures.join("plugins/Cargo.toml"),
        "[workspace]\nmembers = []\n",
    )
    .expect("workspace manifest");
    (temp, fixtures)
}

/// `create_plugin` must produce a plugin that the build's hard scaffold
/// check accepts — otherwise the iteration that created it can never land
/// (the 2026-10-28 imggen attempt: 8 rollbacks, "no write channel").
#[test]
fn create_plugin_scaffolds_what_the_build_requires() {
    let (_temp, fixtures) = minimal_fixture_tree();
    let host = RuntimeHost::boot(&fixtures).expect("boot empty fixtures");
    host.create_plugin("scaffold_probe", Some("probe plugin"))
        .expect("create_plugin");

    let dir = fixtures.join("plugins/scaffold_probe");
    for required in [
        "Cargo.toml",
        "src/lib.rs",
        "tests/scaffold_probe.rs",
        "docs/human/overview.md",
    ] {
        assert!(
            dir.join(required).exists(),
            "create_plugin must scaffold {required}"
        );
    }
    let manifest = fs::read_to_string(dir.join("Cargo.toml")).expect("manifest");
    assert!(
        manifest.contains("allow_generated_docs = true"),
        "new plugin must opt in to generated agent docs:\n{manifest}"
    );
    assert!(
        manifest.contains(r#"crate-type = ["rlib", "dylib"]"#),
        "allow_generated_docs only counts with a dylib crate-type:\n{manifest}"
    );
    let workspace = fs::read_to_string(fixtures.join("plugins/Cargo.toml")).expect("workspace");
    assert!(
        workspace.contains("scaffold_probe"),
        "member must be registered:\n{workspace}"
    );
}

/// A freshly scaffolded plugin has no artifact yet, so it is absent from
/// the registry — iterating it must still work. Otherwise the only way to
/// write its first line of code would be a filesystem-plugin bypass, which
/// is what the bot reported after eight rejected attempts.
#[test]
fn iteration_accepts_a_plugin_that_exists_only_on_disk() {
    let (_temp, fixtures) = minimal_fixture_tree();
    fs::create_dir_all(fixtures.join("plugins/foo/src")).expect("mkdir");
    fs::write(
        fixtures.join("plugins/foo/Cargo.toml"),
        "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n",
    )
    .expect("manifest");
    fs::write(fixtures.join("plugins/foo/src/lib.rs"), "// foo scaffold\n").expect("src");

    let config = crate::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(&fixtures, &config);
    let snapshot = runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snap-disk-only".to_string(),
            plugin_registry: crate::plugin::registry::PluginRegistry::default(),
            node_registry: crate::plugin::registry::NodeRegistry::default(),
            doc_registry: crate::service::doc_registry::DocRegistry::default(),
            graph_registry: crate::service::graph_registry::GraphRegistry::default(),
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        fixtures.join("staged"),
        BTreeMap::new(),
    );
    let request = KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: vec!["foo".to_string()],
        instruction: Some("add a node".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    };
    let prepared = kernel
        .begin_plugin_iteration(&snapshot, &request)
        .expect("an on-disk plugin must be iterable even when it is not loaded");
    assert_eq!(prepared.root_plugin_path, "foo");
    assert_eq!(prepared.target_plugin_paths, vec!["foo".to_string()]);
}

/// A plugin built after boot is not in the running snapshot; `reload
/// /<new>` must load it (full-reload fallback) instead of reporting a
/// no-op success.
#[test]
fn reload_of_a_brand_new_plugin_falls_back_to_a_full_reload() {
    let (_temp, fixtures) = minimal_fixture_tree();
    let host = RuntimeHost::boot(&fixtures).expect("boot empty fixtures");
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("late")
        .is_none());

    // Publish a JSON artifact for "late": no compiler involved, and the
    // loader accepts JSON artifacts.
    let abi = AbiFingerprint::current_build("crate_late_v1", "api_v2");
    let docs = serde_json::json!({
        "plugin_id": "late",
        "plugin_path": "late",
        "plugin_version": "0.1.0",
        "abi_version": 2,
        "nodes": [],
    });
    let artifact = serde_json::json!({
        "plugin_path": "late",
        "abi_fingerprint": abi,
        "docs": docs,
        "exports": [],
        "execution": null,
    });
    fs::write(
        fixtures.join("artifacts/late.json"),
        serde_json::to_vec_pretty(&artifact).expect("artifact"),
    )
    .expect("write artifact");
    let sha256 =
        crate::plugin::artifact::sha256_file(&fixtures.join("artifacts/late.json")).expect("sha");
    let index = serde_json::json!({
        "schema_version": 2,
        "generated_at": "t",
        "topo_order": ["late"],
        "entries": [{
            "plugin_path": "late",
            "version": "0.1.0",
            "abi_fingerprint": abi,
            "artifact_path": "late.json",
            "sha256": sha256,
            "built_at": "0",
            "parent": null,
            "required": true,
            "grants_from_parent": [],
            "docs": docs,
            "exports": [],
            "execution": null,
            "artifact_kind": "json",
            "build_fingerprint": "bf",
            "input_probe": { "files": [] },
            "local_path_deps": []
        }]
    });
    fs::write(
        fixtures.join("artifacts/index.json"),
        serde_json::to_vec_pretty(&index).expect("index"),
    )
    .expect("write index");

    let report = host.reload_with_diagnostics("/late");
    assert_eq!(
        report.status,
        ReloadAttemptStatus::Reloaded,
        "reload report: {report:?}"
    );
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("late")
            .is_some(),
        "the new plugin must be in the snapshot after the reload"
    );
}

/// The iteration's readable set includes the workspace manifest: adding a
/// plugin edits `members` under a sha precondition the agent computes from
/// this file.
#[test]
fn iteration_context_includes_the_workspace_manifest() {
    let (_temp, fixtures) = minimal_fixture_tree();
    fs::create_dir_all(fixtures.join("plugins/demo/src")).expect("mkdir");
    fs::write(
        fixtures.join("plugins/demo/Cargo.toml"),
        "[package]\nname = \"demo\"\n",
    )
    .expect("manifest");
    fs::write(fixtures.join("plugins/demo/src/lib.rs"), "// demo\n").expect("src");

    let paths = collect_plugin_context_paths(&fixtures, "demo", &["demo".to_string()])
        .expect("collect context");
    assert!(
        paths.all_paths.iter().any(|p| p == "plugins/Cargo.toml"),
        "workspace manifest must be readable in an iteration: {:?}",
        paths.all_paths
    );
}
