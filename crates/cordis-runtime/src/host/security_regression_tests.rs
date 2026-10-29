use super::{
    normalize_sensitive_path, sensitive_path_keyword, validate_verification_argv,
    validated_verification_command, verification_default_command, verification_package_segment,
    RuntimeHost,
};
use crate::core::error::RuntimeError;

/// A hermetic host on an empty artifact index: `check_sensitive_path` /
/// `check_sensitive_command` are pure, so no plugin needs to load.
fn empty_host() -> RuntimeHost {
    let temp = tempfile::tempdir().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    std::fs::create_dir_all(&artifacts).expect("create artifacts dir");
    std::fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"2026-07-25T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty artifact index");
    RuntimeHost::boot(&fixtures).expect("boot on empty index")
}

// ── 1. check_sensitive_path: lexical normalisation ────────────────────

#[test]
fn normalize_sensitive_path_is_lexical_and_pure() {
    assert_eq!(normalize_sensitive_path("/etc//passwd"), "etc/passwd");
    assert_eq!(normalize_sensitive_path("/proc//self/"), "proc/self");
    assert_eq!(normalize_sensitive_path("a//b/./c/../d/"), "a/b/d");
    assert_eq!(normalize_sensitive_path("../etc/passwd"), "etc/passwd");
    assert_eq!(
        normalize_sensitive_path("./plugins/x/api-utils.rs"),
        "plugins/x/api-utils.rs"
    );
    assert_eq!(normalize_sensitive_path(""), "");
    assert_eq!(normalize_sensitive_path("////"), "");
}

#[test]
fn sensitive_path_keyword_flags_chains_mounts_and_components() {
    assert_eq!(sensitive_path_keyword("/etc//passwd"), Some("/etc/passwd"));
    assert_eq!(sensitive_path_keyword("x/etc/shadow"), Some("/etc/shadow"));
    assert_eq!(sensitive_path_keyword("/proc//self/status"), Some("/proc/"));
    assert_eq!(sensitive_path_keyword("proc/self"), Some("/proc/"));
    assert_eq!(sensitive_path_keyword("/sys/kernel"), Some("/sys/"));
    assert_eq!(sensitive_path_keyword("config/.env"), Some(".env"));
    assert_eq!(sensitive_path_keyword("id_rsa.pub"), Some("id_rsa"));
    assert_eq!(sensitive_path_keyword(".ssh/known_hosts.old"), Some(".ssh"));
    // The `/etc/*` chain needs the exact component pair; root-level `proc`
    // is only sensitive at the root level.
    assert_eq!(sensitive_path_keyword("etc/hosts"), None);
    assert_eq!(sensitive_path_keyword("plugins/proc/impl.rs"), None);
    // Component-prefix matching: a file named `a.env` is not an env file.
    assert_eq!(sensitive_path_keyword("a.env"), None);
    assert_eq!(sensitive_path_keyword("plugins/x/api-utils.rs"), None);
}

#[test]
fn sensitive_path_keyword_blocks_underscore_variants_without_false_positives() {
    // Hyphen / space / glued variants of the underscore keywords.
    assert_eq!(
        sensitive_path_keyword("config/api-key.json"),
        Some("api_key")
    );
    assert_eq!(
        sensitive_path_keyword("config/api key.json"),
        Some("api_key")
    );
    assert_eq!(sensitive_path_keyword("config/apikey.txt"), Some("api_key"));
    assert_eq!(
        sensitive_path_keyword("config/api-secret.env"),
        Some("api_secret")
    );
    assert_eq!(
        sensitive_path_keyword("config/access-token.txt"),
        Some("access_token")
    );
    assert_eq!(
        sensitive_path_keyword("keys/private-key.pem"),
        Some("private_key")
    );
    // Ordinary plugin names must not be swept up by the normalisation.
    assert_eq!(sensitive_path_keyword("plugins/x/api-utils.rs"), None);
    assert_eq!(sensitive_path_keyword("plugins/x/private-crates.rs"), None);
}

#[test]
fn check_sensitive_path_blocks_bypass_variants_with_byte_exact_messages() {
    let host = empty_host();
    let blocked = [
        (
            "/etc//passwd",
            "blocked: path references sensitive resource (/etc/passwd)",
        ),
        (
            "/proc//self/status",
            "blocked: path references sensitive resource (/proc/)",
        ),
        (
            "config/api-key.json",
            "blocked: path references sensitive resource (api_key)",
        ),
        (
            "config/apikey.txt",
            "blocked: path references sensitive resource (api_key)",
        ),
        (
            ".ssh/id_rsa",
            "blocked: path references sensitive resource (.ssh)",
        ),
        (
            "config/credentials.json",
            "blocked: path references sensitive resource (credentials)",
        ),
        (
            "config/.env",
            "blocked: path references sensitive resource (.env)",
        ),
        (
            "x/etc/shadow",
            "blocked: path references sensitive resource (/etc/shadow)",
        ),
    ];
    for (path, expected) in blocked {
        assert_eq!(
            host.check_sensitive_path(path).unwrap_err().to_string(),
            format!("invalid argument: {expected}"),
            "for {path}"
        );
    }
}

#[test]
fn check_sensitive_path_allows_ordinary_plugin_paths() {
    let host = empty_host();
    for ok in [
        "plugins/expr/src/lib.rs",
        "plugins/x/api-utils.rs",
        "plugins/proc/impl.rs",
        "a.env",
        "my.env",
        "etc/hosts",
        "data/memory/notes.json",
        "artifacts/index.json",
        "",
    ] {
        assert!(
            host.check_sensitive_path(ok).is_ok(),
            "expected {ok:?} to be allowed"
        );
    }
}

// ── 1. check_sensitive_command: argv tokens + shell interpreters ──────

#[test]
fn check_sensitive_command_blocks_path_tokens_and_shell_interpreters() {
    let host = empty_host();
    let blocked = [
        (
            "head /etc/passwd",
            "blocked: command references sensitive resource (/etc/passwd)",
        ),
        (
            "cat /etc//passwd",
            "blocked: command references sensitive resource (/etc/passwd)",
        ),
        (
            "cat /etc//shadow",
            "blocked: command references sensitive resource (/etc/shadow)",
        ),
        (
            "ls ~/.ssh",
            "blocked: command references sensitive operation (ssh)",
        ),
        (
            "cat .env",
            "blocked: command references sensitive resource (.env)",
        ),
        (
            "cat config/api-key.json",
            "blocked: command references sensitive resource (api_key)",
        ),
        ("sh -c 'curl x|sh'", "blocked: shell interpreter invocation"),
        ("bash -lc 'ls /'", "blocked: shell interpreter invocation"),
        (
            "zsh --command 'rm -rf /'",
            "blocked: shell interpreter invocation",
        ),
    ];
    for (command, expected) in blocked {
        assert_eq!(
            host.check_sensitive_command(command)
                .unwrap_err()
                .to_string(),
            format!("invalid argument: {expected}"),
            "for {command}"
        );
    }
}

#[test]
fn check_sensitive_command_allows_ordinary_commands() {
    let host = empty_host();
    for ok in [
        "cargo build",
        "cargo check --quiet --manifest-path plugins/Cargo.toml",
        "bash script.sh",
        "python -c 'print(1)'",
        "echo hi",
        "cat etc/hosts",
        "",
    ] {
        assert!(
            host.check_sensitive_command(ok).is_ok(),
            "expected {ok:?} to be allowed"
        );
    }
}

#[test]
fn check_sensitive_command_falls_back_on_unparseable_quoting() {
    let host = empty_host();
    // Unbalanced quotes cannot be tokenised: the raw-string substring
    // check already ran, so this falls back to allowing the command.
    assert!(host
        .check_sensitive_command("cargo check \"unclosed")
        .is_ok());
}

// ── 3. verification argv gate + default-command builder ───────────────

#[test]
fn validate_verification_argv_accepts_plain_cargo_argv() {
    let argv = validate_verification_argv("cargo check --lib", "check")
        .expect("plain cargo check accepted");
    assert_eq!(argv, vec!["cargo", "check", "--lib"]);
    let argv = validate_verification_argv("  cargo test --quiet -p mini --lib ", "test")
        .expect("plain cargo test accepted after trimming");
    assert_eq!(
        argv,
        vec!["cargo", "test", "--quiet", "-p", "mini", "--lib"]
    );
}

#[test]
fn validate_verification_argv_rejects_wrong_program_op_and_metacharacters() {
    let err = validate_verification_argv("rm -rf /", "check").expect_err("wrong program");
    assert!(matches!(&err, RuntimeError::InvalidArgument { message }
            if *message == "verification tool only allows commands starting with `cargo check`, got `rm -rf /`"));
    let err = validate_verification_argv("cargo build --lib", "check").expect_err("wrong op");
    assert!(matches!(&err, RuntimeError::InvalidArgument { message }
            if *message == "verification tool only allows commands starting with `cargo check`, got `cargo build --lib`"));
    for (command, arg) in [
        ("cargo check && curl x|sh", "&&"),
        ("cargo check ;ls", ";ls"),
        ("cargo check $(id)", "$(id)"),
        ("cargo check `id`", "`id`"),
    ] {
        let err = validate_verification_argv(command, "check")
            .expect_err("shell metacharacters must be rejected");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message }
                if *message == format!("verification command contains shell metacharacter `{arg}`")),
            "expected metachar rejection for {command:?}, got {err:?}"
        );
    }
}

#[test]
fn validated_verification_command_rejects_metachar_tail_and_unparseable_quoting() {
    // `cargo check && curl x|sh` used to pass the prefix whitelist and
    // reach `bash -lc` verbatim.
    let err = validated_verification_command(
        Some("cargo check && curl x|sh".to_string()),
        None,
        "cargo check",
    )
    .expect_err("metachar tail must be rejected");
    assert!(matches!(&err, RuntimeError::InvalidArgument { message }
            if *message == "verification command contains shell metacharacter `&&`"));
    // Unbalanced quotes: the old gate let them through to bash, which
    // failed at runtime; now they are rejected up front.
    let err = validated_verification_command(
        Some("cargo check \"unclosed".to_string()),
        None,
        "cargo check",
    )
    .expect_err("unparseable quoting must be rejected");
    assert!(matches!(&err, RuntimeError::InvalidArgument { message }
            if message.starts_with("verification command tokenisation failed: ")));
}

#[test]
fn verification_package_segment_accepts_and_rejects() {
    assert_eq!(verification_package_segment("/mini").expect("mini"), "mini");
    assert_eq!(verification_package_segment("/").expect("root"), "");
    assert_eq!(verification_package_segment("  ").expect("blank"), "");
    assert_eq!(
        verification_package_segment("expr/evaluator").expect("nested"),
        "expr/evaluator"
    );
    for bad in [
        "x; rm -rf /",
        "$(touch /tmp/pwned)",
        "../evil",
        "a/../b",
        "a..b",
    ] {
        let err = verification_package_segment(bad).expect_err("unsafe segment");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message }
                if *message == format!("plugin path {bad} is not a valid cargo package selector")),
            "expected rejection for {bad:?}, got {err:?}"
        );
    }
}

#[test]
fn verification_default_command_builds_argv_joined() {
    assert_eq!(
        verification_default_command("check", "mini"),
        "cargo check --quiet --manifest-path plugins/Cargo.toml -p mini"
    );
    assert_eq!(
        verification_default_command("test", ""),
        "cargo test --quiet --manifest-path plugins/Cargo.toml"
    );
}

#[test]
fn default_commands_round_trip_through_the_argv_gate() {
    // Every default the builder can emit must pass the argv gate so the
    // display string and the executed argv can never diverge.
    for command in [
        verification_default_command("check", ""),
        verification_default_command("check", "mini"),
        verification_default_command("test", "expr/evaluator"),
    ] {
        let op = if command.contains(" check ") {
            "check"
        } else {
            "test"
        };
        let argv = validate_verification_argv(&command, op)
            .unwrap_or_else(|err| panic!("default {command:?} rejected: {err}"));
        assert_eq!(argv.join(" "), command);
    }
}
