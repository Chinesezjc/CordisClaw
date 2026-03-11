use cordis_runtime::context::ContextRegistry;
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, AutoUpdater, FilePatch};
use cordis_runtime::kernel::evaluator::{EvalHarness, VerificationInput};
use cordis_runtime::kernel::memory::ChangeMemory;
use cordis_runtime::kernel::policy::IterationPolicy;
use cordis_runtime::kernel::r#loop::SelfIterationKernel;
use cordis_runtime::plugin::abi::{PluginRequest, RuntimePlugin};
use cordis_runtime::plugin::loader::{default_loader_config, Loader};
use cordis_runtime::plugin::shell::{ShellPlugin, ShellPluginResponsePayload};
use std::fs;
use std::path::PathBuf;

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(|x| x.as_str()) == Some("auto-update") {
        if let Err(err) = run_auto_update(&args[1..]) {
            eprintln!("auto-update failed: {err}");
            eprintln!("{}", auto_update_usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("shell-terminal") {
        if let Err(err) = run_shell_terminal(&args[1..]) {
            eprintln!("shell-terminal failed: {err}");
            eprintln!("{}", auto_update_usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("graph-html") {
        if let Err(err) = run_graph_html(&args[1..]) {
            eprintln!("graph-html failed: {err}");
            eprintln!("{}", auto_update_usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("dag-html") {
        if let Err(err) = run_dag_html(&args[1..]) {
            eprintln!("dag-html failed: {err}");
            eprintln!("{}", auto_update_usage());
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

    // CLI mode allows workspace-local paths by default, while sensitive paths
    // are still protected by SafetyGate/manual approval.
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
            patches: vec![FilePatch {
                path: patch_path,
                find,
                replace,
            }],
        },
        |_| {
            Ok(VerificationInput {
                tests_passed,
                safety_checks_passed,
                quality_score,
            })
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

fn parse_bool_flag(value: &str) -> Result<bool, Box<dyn std::error::Error>> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("invalid bool: {other} (expected true/false)").into()),
    }
}

fn run_shell_terminal(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut shell: Option<String> = None;
    let mut command: Option<String> = None;
    let mut cwd: Option<String> = None;
    let mut fixtures_root: Option<String> = None;

    for token in args {
        if let Some(value) = token.strip_prefix("--shell=") {
            shell = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--command=") {
            command = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--cwd=") {
            cwd = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--fixtures-root=") {
            fixtures_root = Some(value.to_string());
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let payload = serde_json::json!({
        "action": "start_terminal",
        "shell": shell,
        "command": command,
        "cwd": cwd,
        "fixtures_root": fixtures_root,
    })
    .to_string();
    let mut plugin = ShellPlugin::default();
    let response = plugin.handle(PluginRequest { payload });
    let parsed: ShellPluginResponsePayload = serde_json::from_str(&response.payload)?;
    if let Some(output) = &parsed.output {
        if !output.is_empty() {
            println!("{output}");
        }
    }
    println!(
        "shell_terminal ok={} exit_code={:?} message={}",
        parsed.ok, parsed.exit_code, parsed.message
    );
    if parsed.ok {
        Ok(())
    } else {
        Err(parsed.message.into())
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

fn auto_update_usage() -> String {
    "Usage:
  cargo run -p cordis-runtime -- <fixtures_root>
  cargo run -p cordis-runtime -- auto-update <workspace_root> <relative_path> <find> <replace> [--manual-approved] [--tests-passed=true|false] [--safety-checks-passed=true|false] [--quality-score=<u32>] [--diff-lines=<usize>]
  cargo run -p cordis-runtime -- shell-terminal [--shell=cordis] [--command=\"echo hi\"] [--cwd=/root] [--fixtures-root=fixtures]
  cargo run -p cordis-runtime -- graph-html [fixtures_root] [--output=registered-nodes.html]
  cargo run -p cordis-runtime -- dag-html [fixtures_root] [--output=registered-dag.html]"
        .to_string()
}
