use serde_json::json;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use tempfile::TempDir;

fn write_config(
    root: &std::path::Path,
    provider: &str,
    base_url: &str,
    api_key_env: &str,
    model: &str,
) {
    let config_dir = root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: {provider}\nbase_url: {base_url}\napi_key_env: {api_key_env}\nmodel: {model}\ntemperature: 0.0\nmax_tokens: 1024\ntimeout_ms: 30000\n"
        ),
    )
    .expect("write llm config");
}

fn spawn_mock_llm_server(
    response_body: String,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let address = listener.local_addr().expect("listener addr");
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
        let mut request = String::new();

        let mut first_line = String::new();
        reader
            .read_line(&mut first_line)
            .expect("read request line");
        request.push_str(&first_line);

        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).expect("read header line");
            request.push_str(&line);
            if line == "\r\n" {
                break;
            }

            let lowercase = line.to_ascii_lowercase();
            if let Some(value) = lowercase.strip_prefix("content-length:") {
                content_length = value.trim().parse::<usize>().expect("parse content length");
            }
        }

        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body).expect("read request body");
        request.push_str(&String::from_utf8_lossy(&body));
        sender.send(request).expect("send captured request");

        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        )
        .expect("write response");
    });

    (format!("http://{}/v1", address), receiver, handle)
}

fn spawn_mock_openai_server(
    output_text: &str,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let response_body = json!({
        "id": "resp_test",
        "output_text": output_text,
    })
    .to_string();
    spawn_mock_llm_server(response_body)
}

fn spawn_mock_deepseek_server(
    output_text: &str,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let response_body = json!({
        "id": "chatcmpl_test",
        "choices": [
            {
                "message": {
                    "content": output_text
                }
            }
        ]
    })
    .to_string();
    spawn_mock_llm_server(response_body)
}

fn assert_authorization_bearer(request: &str, expected_token: &str) {
    let expected_value = format!("Bearer {expected_token}");
    let mut matched = false;
    for line in request.lines() {
        if line.is_empty() {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("authorization") {
            assert_eq!(
                value.trim(),
                expected_value,
                "authorization header value mismatch"
            );
            matched = true;
            break;
        }
    }

    assert!(
        matched,
        "authorization header missing in request:\n{request}"
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_promotes_and_keeps_changes() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let output_text = json!({
        "summary": "Replace old with new in the demo file.",
        "patches": [
            {
                "path": "demo.txt",
                "find": "old",
                "replace": "new"
            }
        ]
    })
    .to_string();
    let (base_url, request_rx, handle) = spawn_mock_openai_server(&output_text);
    write_config(
        temp.path(),
        "openai",
        &base_url,
        "OPENAI_API_KEY",
        "gpt-4.1-mini",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .arg("--tests-command=grep -q new demo.txt")
        .arg("--safety-command=grep -q new demo.txt")
        .env("OPENAI_API_KEY", "test-key")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("\"verdict\": \"Promote\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"rolled_back\": false"),
        "stdout: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let captured_request = request_rx.recv().expect("capture request");
    assert!(captured_request.starts_with("POST /v1/responses HTTP/1.1"));
    assert_authorization_bearer(&captured_request, "test-key");
    assert!(captured_request.contains("Replace old with new in demo.txt"));
    assert!(captured_request.contains("alpha-old-omega"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_rolls_back_when_tests_fail() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let output_text = json!({
        "summary": "Replace old with new in the demo file.",
        "patches": [
            {
                "path": "demo.txt",
                "find": "old",
                "replace": "new"
            }
        ]
    })
    .to_string();
    let (base_url, _request_rx, handle) = spawn_mock_openai_server(&output_text);
    write_config(
        temp.path(),
        "openai",
        &base_url,
        "OPENAI_API_KEY",
        "gpt-4.1-mini",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .arg("--tests-command=cargo --badflag")
        .arg("--safety-command=cargo --version")
        .env("OPENAI_API_KEY", "test-key")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("\"verdict\": \"Rollback\""),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("\"rolled_back\": true"), "stdout: {stdout}");
    assert!(
        stdout.contains("\"tests_passed\": false"),
        "stdout: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-old-omega\n"
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_supports_deepseek_chat_completions() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let output_text = json!({
        "summary": "Replace old with new in the demo file.",
        "patches": [
            {
                "path": "demo.txt",
                "find": "old",
                "replace": "new"
            }
        ]
    })
    .to_string();
    let (base_url, request_rx, handle) = spawn_mock_deepseek_server(&output_text);
    write_config(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-chat",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .arg("--tests-command=grep -q new demo.txt")
        .arg("--safety-command=grep -q new demo.txt")
        .env("DEEPSEEK_API_KEY", "test-key")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("\"verdict\": \"Promote\""),
        "stdout: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let captured_request = request_rx.recv().expect("capture request");
    assert!(captured_request.starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert_authorization_bearer(&captured_request, "test-key");
    assert!(captured_request.contains("\"model\":\"deepseek-chat\""));
    assert!(captured_request.contains("\"response_format\":{\"type\":\"json_object\"}"));
    assert!(captured_request.contains("Replace old with new in demo.txt"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_uses_model_suggested_verification_commands() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let output_text = json!({
        "summary": "Replace old with new in the demo file.",
        "tests_command": "grep -q new demo.txt",
        "safety_command": "grep -q alpha-new-omega demo.txt",
        "patches": [
            {
                "path": "demo.txt",
                "find": "old",
                "replace": "new"
            }
        ]
    })
    .to_string();
    let (base_url, request_rx, handle) = spawn_mock_openai_server(&output_text);
    write_config(
        temp.path(),
        "openai",
        &base_url,
        "OPENAI_API_KEY",
        "gpt-4.1-mini",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .env("OPENAI_API_KEY", "test-key")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("\"verdict\": \"Promote\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"command\": \"grep -q new demo.txt\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"command\": \"grep -q alpha-new-omega demo.txt\""),
        "stdout: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let captured_request = request_rx.recv().expect("capture request");
    assert!(captured_request.contains("Replace old with new in demo.txt"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_supports_plugin_verifier_commands() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let fixtures_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("fixtures")
        .canonicalize()
        .expect("fixtures root");
    let output_text = json!({
        "summary": "Replace old with new in the demo file.",
        "tests_command": format!(
            "plugin:{}",
            json!({
                "fixtures_root": fixtures_root,
                "plugin_path": "expr",
                "node_id": "expr_entry",
                "payload_json": {
                    "expression": "1 + 2 * 3"
                },
                "expect_substring": "\"value\":7.0"
            })
        ),
        "safety_command": "grep -q alpha-new-omega demo.txt",
        "patches": [
            {
                "path": "demo.txt",
                "find": "old",
                "replace": "new"
            }
        ]
    })
    .to_string();
    let (base_url, _request_rx, handle) = spawn_mock_openai_server(&output_text);
    write_config(
        temp.path(),
        "openai",
        &base_url,
        "OPENAI_API_KEY",
        "gpt-4.1-mini",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .env("OPENAI_API_KEY", "test-key")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("\"runner\": \"plugin\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"verdict\": \"Promote\""),
        "stdout: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_rust_workspace_profile_runs_static_check_stage() {
    let temp = TempDir::new().expect("tempdir");
    fs::write(
        temp.path().join("Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write manifest");
    fs::create_dir_all(temp.path().join("src")).expect("src dir");
    let source_path = temp.path().join("src/lib.rs");
    fs::write(&source_path, "pub fn demo() -> &'static str { \"old\" }\n").expect("write source");

    let output_text = json!({
        "summary": "Replace old with new in the source file.",
        "patches": [
            {
                "path": "src/lib.rs",
                "find": "\"old\"",
                "replace": "\"new\""
            }
        ]
    })
    .to_string();
    let (base_url, _request_rx, handle) = spawn_mock_openai_server(&output_text);
    write_config(
        temp.path(),
        "openai",
        &base_url,
        "OPENAI_API_KEY",
        "gpt-4.1-mini",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in src/lib.rs")
        .arg("--path=src/lib.rs")
        .arg("--verify-profile=rust-workspace")
        .env("OPENAI_API_KEY", "test-key")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(
        stdout.contains("\"profile\": \"rust_workspace\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"kind\": \"static_check\""),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("\"status\": \"passed\""),
        "stdout: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&source_path).expect("read source"),
        "pub fn demo() -> &'static str { \"new\" }\n"
    );
}
