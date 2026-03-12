use cordis_runtime::config::RuntimeConfig;
use cordis_runtime::context::ContextRegistry;
use cordis_runtime::host::{KernelApplyRequest, KernelPlanRequest, RuntimeHost, RuntimeKernel};
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, AutoUpdater, FilePatch, VerificationEnvelope};
use cordis_runtime::kernel::evaluator::{EvalHarness, VerificationInput};
use cordis_runtime::kernel::memory::ChangeMemory;
use cordis_runtime::kernel::policy::IterationPolicy;
use cordis_runtime::kernel::r#loop::SelfIterationKernel;
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::plugin::invoke::PluginInvoker;
use cordis_runtime::plugin::loader::{default_loader_config, Loader};
use cordis_runtime::plugin::tooling::{
    prepare_artifacts, rebuild_fixture_artifacts, refresh_artifact_index, sync_plugin_docs,
    PrepareMode,
};
use serde_json::Value;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(|x| x.as_str()) == Some("auto-update") {
        if let Err(err) = run_auto_update(&args[1..]) {
            eprintln!("auto-update failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("serve") {
        if let Err(err) = run_serve(&args[1..]) {
            eprintln!("serve failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("invoke") {
        if let Err(err) = run_invoke(&args[1..]) {
            eprintln!("invoke failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("execute") {
        if let Err(err) = run_execute(&args[1..]) {
            eprintln!("execute failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("llm-auto-update") {
        if let Err(err) = run_llm_auto_update(&args[1..]) {
            eprintln!("llm-auto-update failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("graph-html") {
        if let Err(err) = run_graph_html(&args[1..]) {
            eprintln!("graph-html failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("dag-html") {
        if let Err(err) = run_dag_html(&args[1..]) {
            eprintln!("dag-html failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("sync-plugin-docs") {
        if let Err(err) = run_sync_plugin_docs(&args[1..]) {
            eprintln!("sync-plugin-docs failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("refresh-artifact-index") {
        if let Err(err) = run_refresh_artifact_index(&args[1..]) {
            eprintln!("refresh-artifact-index failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("rebuild-fixture-artifacts") {
        if let Err(err) = run_rebuild_fixture_artifacts(&args[1..]) {
            eprintln!("rebuild-fixture-artifacts failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("prepare-artifacts") {
        if let Err(err) = run_prepare_artifacts(&args[1..]) {
            eprintln!("prepare-artifacts failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }

    if let Err(err) = run_loader(args.first().map(PathBuf::from)) {
        eprintln!("load failed: {err}");
        std::process::exit(1);
    }
}

fn run_loader(root: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(|| PathBuf::from("fixtures"));
    prepare_fixtures_root(&root, false)?;
    let config = default_loader_config(&root);
    let loader = Loader::new(config);
    let output = loader.load()?;

    println!("execution_id: {}", output.execution_id);
    println!("loaded plugins:");
    for (path, plugin) in output.plugin_registry.iter() {
        println!("- {path}: {:?}", plugin.load_result);
    }
    println!("registered nodes: {}", output.node_registry.len());
    println!(
        "metrics: abi_mismatch={}, no_fallback={}, unavailable={}",
        output.metrics.dylib_abi_mismatch_total,
        output.metrics.dylib_no_fallback_total,
        output.metrics.plugin_unavailable_total
    );

    if let Ok(service) = output.context.inject::<String>("root/child", "service.db") {
        println!("inject(root/child, service.db) -> {}", service.as_str());
    }

    Ok(())
}

fn run_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (root, runtime_only) = parse_root_and_runtime_only(args, "fixtures")?;
    prepare_fixtures_root(&root, runtime_only)?;
    let host = RuntimeHost::boot(&root).map_err(|err| runtime_mode_error(err, &root, runtime_only))?;
    println!(
        "serve ready snapshot_id={}",
        host.current_snapshot().snapshot_id()
    );
    io::stdout().flush()?;

    let stdin = io::stdin();
    let mut locked = stdin.lock();
    loop {
        let mut line = String::new();
        let read = locked.read_line(&mut line)?;
        if read == 0 {
            break;
        }

        match handle_serve_command(&host, line.trim()) {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) => {
                println!("serve error: {err}");
                io::stdout().flush()?;
            }
        }
    }

    Ok(())
}

fn handle_serve_command(
    host: &RuntimeHost,
    command: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Ok(true);
    }

    match command {
        "help" => {
            println!("{}", serve_usage());
        }
        "plugins" => {
            let snapshot = host.current_snapshot();
            println!("snapshot_id={}", snapshot.snapshot_id());
            for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
                println!("{plugin_path} {:?}", plugin.load_result);
            }
        }
        "status" => {
            println!("{}", serde_json::to_string(&host.status())?);
        }
        "reload" => {
            let report = host.reload_with_diagnostics();
            println!("{}", serde_json::to_string(&report)?);
        }
        "kernel status" => {
            println!("{}", serde_json::to_string(&host.kernel().status())?);
        }
        "kernel history" => {
            println!("{}", serde_json::to_string(&host.kernel().history())?);
        }
        "exit" | "quit" => return Ok(false),
        _ => {
            if let Some(rest) = command.strip_prefix("invoke ") {
                let (plugin_path, remainder) =
                    split_first_token(rest).ok_or("missing plugin_path for invoke")?;
                let (node_id, payload_json) =
                    split_first_token(remainder).ok_or("missing node_id/payload for invoke")?;
                let response = host.invoke(plugin_path, node_id, payload_json.to_string())?;
                emit_invoke_response(&response.payload)?;
            } else if let Some(rest) = command.strip_prefix("execute ") {
                let (target_node_fqn, payload_json) =
                    split_first_token(rest).ok_or("missing node_fqn/payload for execute")?;
                let payload = serde_json::from_str::<Value>(payload_json)?;
                let response = host.execute(target_node_fqn, payload)?;
                println!("{}", serde_json::to_string(&response)?);
            } else if let Some(json) = command.strip_prefix("kernel apply-plan ") {
                let request: KernelApplyRequest = serde_json::from_str(json)?;
                let result = host
                    .kernel()
                    .run_iteration(request.plan, request.verification)?;
                println!("{}", serde_json::to_string(&result)?);
            } else if let Some(json) = command.strip_prefix("kernel plan-apply ") {
                let request: KernelPlanRequest = serde_json::from_str(json)?;
                let result = host.kernel().plan_and_run_iteration(request)?;
                println!("{}", serde_json::to_string(&result)?);
            } else {
                println!("unknown serve command: {command}");
            }
        }
    }

    io::stdout().flush()?;
    Ok(true)
}

fn run_invoke(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() < 2 {
        return Err("missing required args: <plugin_path> <node_id>".into());
    }

    let plugin_path = args[0].clone();
    let node_id = args[1].clone();
    let mut fixtures_root: Option<PathBuf> = None;
    let mut payload_json: Option<String> = None;
    let mut runtime_only = false;

    for token in &args[2..] {
        if let Some(value) = token.strip_prefix("--fixtures-root=") {
            fixtures_root = Some(PathBuf::from(value));
            continue;
        }
        if let Some(value) = token.strip_prefix("--payload-json=") {
            payload_json = Some(value.to_string());
            continue;
        }
        if token == "--runtime-only" {
            runtime_only = true;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let payload = payload_json.ok_or("missing required flag: --payload-json=<json>")?;
    let fixtures_root = fixtures_root.unwrap_or_else(PluginInvoker::default_fixtures_root);
    prepare_fixtures_root(&fixtures_root, runtime_only)?;
    let invoker = PluginInvoker::load(&fixtures_root)
        .map_err(|err| runtime_mode_error(err, &fixtures_root, runtime_only))?;
    let response = invoker.invoke(&plugin_path, &node_id, payload)?;
    emit_invoke_response(&response.payload)
}

fn run_execute(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("missing required args: <node_fqn>".into());
    }

    let mut root = PathBuf::from("fixtures");
    let mut target_node_fqn: Option<String> = None;
    let mut payload_json: Option<String> = None;
    let mut runtime_only = false;

    for token in args {
        if let Some(value) = token.strip_prefix("--fixtures-root=") {
            root = PathBuf::from(value);
            continue;
        }
        if let Some(value) = token.strip_prefix("--payload-json=") {
            payload_json = Some(value.to_string());
            continue;
        }
        if token == "--runtime-only" {
            runtime_only = true;
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if target_node_fqn.is_none() {
            target_node_fqn = Some(token.clone());
            continue;
        }
        return Err(format!("unexpected extra arg: {token}").into());
    }

    let target_node_fqn = target_node_fqn.ok_or("missing required arg: <node_fqn>")?;
    let payload = payload_json.ok_or("missing required flag: --payload-json=<json>")?;
    prepare_fixtures_root(&root, runtime_only)?;
    let host = RuntimeHost::boot(&root).map_err(|err| runtime_mode_error(err, &root, runtime_only))?;
    let payload = serde_json::from_str::<Value>(&payload)?;
    let result = host.execute(&target_node_fqn, payload)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn emit_invoke_response(payload: &str) -> Result<(), Box<dyn std::error::Error>> {
    let value = match serde_json::from_str::<Value>(payload) {
        Ok(value) => value,
        Err(_) => {
            println!("{payload}");
            return Ok(());
        }
    };

    let Some(object) = value.as_object() else {
        println!("{payload}");
        return Ok(());
    };

    if let Some(output) = object.get("output").and_then(|v| v.as_str()) {
        if !output.is_empty() {
            println!("{output}");
        }
    }

    if let Some(ok) = object.get("ok").and_then(|v| v.as_bool()) {
        let exit_code = object.get("exit_code").cloned().unwrap_or(Value::Null);
        let message = object
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        println!(
            "invoke ok={} exit_code={} message={}",
            ok,
            format_scalar(&exit_code),
            message
        );
        if ok {
            return Ok(());
        }
        return Err(message.to_string().into());
    }

    println!("{payload}");
    Ok(())
}

fn format_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn run_auto_update(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() < 4 {
        return Err("missing required args".into());
    }

    let workspace_root = PathBuf::from(&args[0]);
    let patch_path = args[1].clone();
    let find = args[2].clone();
    let replace = args[3].clone();

    let mut manual_approved = false;
    let mut tests_passed = true;
    let mut safety_checks_passed = true;
    let mut quality_score = 90_u32;
    let mut diff_lines = 1_usize;

    for token in &args[4..] {
        if token == "--manual-approved" {
            manual_approved = true;
            continue;
        }
        if let Some(value) = token.strip_prefix("--tests-passed=") {
            tests_passed = parse_bool_flag(value)?;
            continue;
        }
        if let Some(value) = token.strip_prefix("--safety-checks-passed=") {
            safety_checks_passed = parse_bool_flag(value)?;
            continue;
        }
        if let Some(value) = token.strip_prefix("--quality-score=") {
            quality_score = value.parse::<u32>()?;
            continue;
        }
        if let Some(value) = token.strip_prefix("--diff-lines=") {
            diff_lines = value.parse::<usize>()?;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let mut policy = IterationPolicy::default();
    policy.path_allowlist = vec!["".to_string()];
    let mut kernel =
        SelfIterationKernel::new(policy, EvalHarness::default(), ChangeMemory::default());
    let updater = AutoUpdater::new(&workspace_root);
    let result = updater.execute(
        &mut kernel,
        AutoUpdatePlan {
            issue_id: "cli-issue".to_string(),
            patch_id: "cli-patch".to_string(),
            manual_approved,
            diff_lines,
            patches: vec![FilePatch::text(patch_path, find, replace)],
        },
        |_| {
            Ok(VerificationEnvelope::from(VerificationInput {
                tests_passed,
                safety_checks_passed,
                quality_score,
            }))
        },
    )?;

    println!("auto_update verdict: {:?}", result.report.verdict);
    println!("rolled_back: {}", result.rolled_back);
    println!("changed_paths: {:?}", result.changed_paths);
    println!("evaluation_reasons: {:?}", result.report.evaluation.reasons);
    println!(
        "kernel_metrics: total={}, promote={}, rollback={}",
        kernel.metrics().iteration_total,
        kernel.metrics().iteration_promote_total,
        kernel.metrics().iteration_rollback_total
    );
    Ok(())
}

fn run_llm_auto_update(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("missing required args: <workspace_root>".into());
    }

    let workspace_root = PathBuf::from(&args[0]);
    let mut instruction: Option<String> = None;
    let mut issue_id: Option<String> = None;
    let mut patch_id: Option<String> = None;
    let mut paths = Vec::new();
    let mut manual_approved = false;
    let mut tests_command: Option<String> = None;
    let mut safety_command: Option<String> = None;
    let mut verify_profile: Option<VerificationProfile> = None;
    let mut quality_score: Option<u32> = None;
    let mut dry_run = false;

    for token in &args[1..] {
        if let Some(value) = token.strip_prefix("--instruction=") {
            instruction = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--issue-id=") {
            issue_id = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--patch-id=") {
            patch_id = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--path=") {
            paths.push(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--tests-command=") {
            tests_command = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--safety-command=") {
            safety_command = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--verify-profile=") {
            verify_profile = Some(parse_verify_profile_flag(value)?);
            continue;
        }
        if let Some(value) = token.strip_prefix("--quality-score=") {
            quality_score = Some(value.parse::<u32>()?);
            continue;
        }
        if token == "--manual-approved" {
            manual_approved = true;
            continue;
        }
        if token == "--dry-run" {
            dry_run = true;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let instruction = instruction.ok_or("missing required flag: --instruction=<text>")?;
    if paths.is_empty() {
        return Err("missing required flag: --path=<relative_path>".into());
    }

    let config = RuntimeConfig::load(&workspace_root)?;
    let kernel = RuntimeKernel::new(&workspace_root, &config);
    let request = KernelPlanRequest {
        issue_id,
        patch_id,
        instruction,
        paths,
        manual_approved,
        tests_command,
        safety_command,
        verify_profile,
        quality_score,
    };

    if dry_run {
        let planned = kernel.plan_update(request)?;
        println!("{}", serde_json::to_string_pretty(&planned)?);
        return Ok(());
    }

    let result = kernel.plan_and_run_iteration(request)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn parse_bool_flag(value: &str) -> Result<bool, Box<dyn std::error::Error>> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("invalid bool: {other} (expected true/false)").into()),
    }
}

fn parse_verify_profile_flag(
    value: &str,
) -> Result<VerificationProfile, Box<dyn std::error::Error>> {
    match value {
        "default" => Ok(VerificationProfile::Default),
        "rust-workspace" | "rust_workspace" => Ok(VerificationProfile::RustWorkspace),
        other => Err(format!(
            "invalid verify profile: {other} (expected default|rust-workspace)"
        )
        .into()),
    }
}

fn run_graph_html(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root: Option<PathBuf> = None;
    let mut output_path = PathBuf::from("registered-nodes.html");

    for token in args {
        if let Some(value) = token.strip_prefix("--output=") {
            output_path = PathBuf::from(value);
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if root.is_none() {
            root = Some(PathBuf::from(token));
            continue;
        }
        return Err(format!("unexpected extra arg: {token}").into());
    }

    let root = root.unwrap_or_else(|| PathBuf::from("fixtures"));
    prepare_fixtures_root(&root, false)?;
    let loader = Loader::new(default_loader_config(&root));
    let output = loader.load()?;
    let html = output
        .graph_registry
        .handle_get_html("/graphs/registered-nodes.html")?;
    fs::write(&output_path, html)?;

    let absolute = if output_path.is_absolute() {
        output_path
    } else {
        std::env::current_dir()?.join(output_path)
    };
    println!("graph_html written to {}", absolute.display());
    println!(
        "plugins={} nodes={}",
        output.graph_registry.graph().plugins.len(),
        output.graph_registry.graph().nodes.len()
    );
    Ok(())
}

fn split_first_token(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let mut split_index = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_whitespace() {
            split_index = idx;
            break;
        }
    }

    let token = &trimmed[..split_index];
    let remainder = trimmed[split_index..].trim_start();
    Some((token, remainder))
}

fn run_dag_html(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root: Option<PathBuf> = None;
    let mut output_path = PathBuf::from("registered-dag.html");

    for token in args {
        if let Some(value) = token.strip_prefix("--output=") {
            output_path = PathBuf::from(value);
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if root.is_none() {
            root = Some(PathBuf::from(token));
            continue;
        }
        return Err(format!("unexpected extra arg: {token}").into());
    }

    let root = root.unwrap_or_else(|| PathBuf::from("fixtures"));
    prepare_fixtures_root(&root, false)?;
    let loader = Loader::new(default_loader_config(&root));
    let output = loader.load()?;
    let html = output
        .graph_registry
        .handle_get_html("/graphs/registered-dag.html")?;
    fs::write(&output_path, html)?;

    let absolute = if output_path.is_absolute() {
        output_path
    } else {
        std::env::current_dir()?.join(output_path)
    };
    println!("dag_html written to {}", absolute.display());
    println!(
        "nodes={} edges={} diagnostics={}",
        output.graph_registry.dag().nodes.len(),
        output.graph_registry.dag().edges.len(),
        output.graph_registry.dag().diagnostics.len()
    );
    Ok(())
}

fn run_sync_plugin_docs(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let root = parse_optional_root_arg(args, "fixtures")?;
    prepare_fixtures_root(&root, false)?;
    let written = sync_plugin_docs(&root)?;
    println!("synced_plugin_docs={}", written.len());
    for path in written {
        println!("{}", path.display());
    }
    Ok(())
}

fn run_refresh_artifact_index(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let root = parse_optional_root_arg(args, "fixtures")?;
    prepare_fixtures_root(&root, false)?;
    let refreshed = refresh_artifact_index(&root)?;
    println!("refreshed_artifact_entries={}", refreshed.len());
    for (plugin_path, hash) in refreshed {
        println!("{plugin_path} {hash}");
    }
    Ok(())
}

fn run_rebuild_fixture_artifacts(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let root = parse_optional_root_arg(args, "fixtures")?;
    let rebuilt = rebuild_fixture_artifacts(&root)?;
    println!("rebuilt_artifact_entries={}", rebuilt.len());
    for (plugin_path, hash) in rebuilt {
        println!("{plugin_path} {hash}");
    }
    Ok(())
}

fn run_prepare_artifacts(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root = PathBuf::from("fixtures");
    let mut mode = PrepareMode::Incremental;

    for token in args {
        if token == "--full" {
            mode = PrepareMode::Full;
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        root = PathBuf::from(token);
    }

    let report = prepare_artifacts(&root, mode)?;
    println!(
        "prepared_artifacts rebuilt={} reused={} full_rebuild={}",
        report.rebuilt.len(),
        report.reused.len(),
        report.full_rebuild
    );
    for (plugin_path, hash) in report.rebuilt {
        println!("{plugin_path} {hash}");
    }
    Ok(())
}

fn prepare_fixtures_root(root: &Path, runtime_only: bool) -> Result<(), Box<dyn std::error::Error>> {
    if runtime_only {
        return Ok(());
    }
    let report = prepare_artifacts(root, PrepareMode::Incremental)?;
    if !report.rebuilt.is_empty() {
        println!(
            "prepared fixture artifacts under {} rebuilt={} reused={} full_rebuild={}",
            root.display(),
            report.rebuilt.len(),
            report.reused.len(),
            report.full_rebuild
        );
    }
    Ok(())
}

fn parse_optional_root_arg(
    args: &[String],
    default_root: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    match args {
        [] => Ok(PathBuf::from(default_root)),
        [root] if !root.starts_with("--") => Ok(PathBuf::from(root)),
        [other] => Err(format!("unknown flag: {other}").into()),
        _ => Err("too many arguments".into()),
    }
}

fn parse_root_and_runtime_only(
    args: &[String],
    default_root: &str,
) -> Result<(PathBuf, bool), Box<dyn std::error::Error>> {
    let mut root = PathBuf::from(default_root);
    let mut runtime_only = false;
    let mut seen_root = false;

    for token in args {
        if token == "--runtime-only" {
            runtime_only = true;
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if seen_root {
            return Err(format!("unexpected extra arg: {token}").into());
        }
        root = PathBuf::from(token);
        seen_root = true;
    }

    Ok((root, runtime_only))
}

fn runtime_mode_error(
    err: cordis_runtime::core::error::RuntimeError,
    root: &Path,
    runtime_only: bool,
) -> Box<dyn std::error::Error> {
    if runtime_only {
        return format!(
            "{err}; bundle is runtime-only, run `cargo run -p cordis-runtime -- prepare-artifacts {}` to rebuild artifacts",
            root.display()
        )
        .into();
    }
    Box::new(err)
}

fn usage() -> String {
    "Usage:
  cargo run -p cordis-runtime -- <fixtures_root>
  cargo run -p cordis-runtime -- serve [fixtures_root] [--runtime-only]
  cargo run -p cordis-runtime -- invoke <plugin_path> <node_id> --payload-json=<json> [--fixtures-root=fixtures] [--runtime-only]
  cargo run -p cordis-runtime -- execute <node_fqn> --payload-json=<json> [--fixtures-root=fixtures] [--runtime-only]
  cargo run -p cordis-runtime -- llm-auto-update <workspace_root> --instruction=<text> --path=<relative_path> [--path=<relative_path> ...] [--issue-id=<id>] [--patch-id=<id>] [--manual-approved] [--tests-command=<shell>] [--safety-command=<shell>] [--verify-profile=<default|rust-workspace>] [--quality-score=<u32>] [--dry-run]
    tests/safety commands also accept plugin:{\"plugin_path\":\"<plugin_path>\",\"node_id\":\"<node_id>\",\"payload_json\":{},\"expect_substring\":\"<expected text>\",\"fixtures_root\":\"<optional fixtures root>\"}
  cargo run -p cordis-runtime -- auto-update <workspace_root> <relative_path> <find> <replace> [--manual-approved] [--tests-passed=true|false] [--safety-checks-passed=true|false] [--quality-score=<u32>] [--diff-lines=<usize>]
  cargo run -p cordis-runtime -- graph-html [fixtures_root] [--output=registered-nodes.html]
  cargo run -p cordis-runtime -- dag-html [fixtures_root] [--output=registered-dag.html]
  cargo run -p cordis-runtime -- prepare-artifacts [fixtures_root] [--full]
  cargo run -p cordis-runtime -- sync-plugin-docs [fixtures_root]
  cargo run -p cordis-runtime -- refresh-artifact-index [fixtures_root]
  cargo run -p cordis-runtime -- rebuild-fixture-artifacts [fixtures_root]"
        .to_string()
}

fn serve_usage() -> &'static str {
    "serve commands:
  help
  status
  plugins
  reload
  invoke <plugin_path> <node_id> <payload-json>
  execute <node_fqn> <payload-json>
  kernel status
  kernel history
  kernel apply-plan <json>
  kernel plan-apply <json>
  exit"
}
