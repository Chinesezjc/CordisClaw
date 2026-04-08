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
    write_config_with_timeout(root, provider, base_url, api_key_env, model, 30000);
}

fn write_config_with_timeout(
    root: &std::path::Path,
    provider: &str,
    base_url: &str,
    api_key_env: &str,
    model: &str,
    timeout_ms: u64,
) {
    let config_dir = root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: {provider}\nbase_url: {base_url}\napi_key_env: {api_key_env}\nmodel: {model}\ntemperature: 0.0\nmax_tokens: 1024\ntimeout_ms: {timeout_ms}\n"
        ),
    )
    .expect("write llm config");
}

fn spawn_mock_llm_server_sequence(
    response_bodies: Vec<String>,
) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
    let responses = response_bodies
        .into_iter()
        .map(|body| (200_u16, body))
        .collect();
    spawn_mock_llm_server_sequence_with_statuses(responses)
}

fn spawn_mock_llm_server_sequence_with_statuses(
    responses: Vec<(u16, String)>,
) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let address = listener.local_addr().expect("listener addr");
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (status_code, response_body) in responses {
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
            requests.push(request);

            write!(
                stream,
                "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                status_code,
                mock_reason_phrase(status_code),
                response_body.len(),
                response_body
            )
            .expect("write response");
        }
        sender.send(requests).expect("send captured requests");
    });

    (format!("http://{}/v1", address), receiver, handle)
}

fn spawn_delayed_mock_llm_server_sequence(
    responses: Vec<(u64, String)>,
) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let address = listener.local_addr().expect("listener addr");
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for (delay_ms, response_body) in responses {
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
            requests.push(request);

            thread::sleep(std::time::Duration::from_millis(delay_ms));
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            )
            .expect("write response");
        }
        sender.send(requests).expect("send captured requests");
    });

    (format!("http://{}/v1", address), receiver, handle)
}

fn spawn_slow_mock_llm_server_sequence(
    delays_ms: Vec<u64>,
    response_body: String,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let address = listener.local_addr().expect("listener addr");

    let handle = thread::spawn(move || {
        for delay_ms in delays_ms {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));

            let mut first_line = String::new();
            reader
                .read_line(&mut first_line)
                .expect("read request line");

            let mut content_length = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read header line");
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

            thread::sleep(std::time::Duration::from_millis(delay_ms));
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
        }
    });

    (format!("http://{}/v1", address), handle)
}

fn spawn_chunked_mock_llm_server_sequence(
    responses: Vec<Vec<(u64, String)>>,
) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let address = listener.local_addr().expect("listener addr");
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for chunks in responses {
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
            requests.push(request);

            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
            )
            .expect("write response headers");
            stream.flush().expect("flush response headers");

            for (delay_ms, chunk) in chunks {
                thread::sleep(std::time::Duration::from_millis(delay_ms));
                write!(stream, "{:X}\r\n{}\r\n", chunk.len(), chunk).expect("write chunk");
                stream.flush().expect("flush chunk");
            }
            write!(stream, "0\r\n\r\n").expect("finish chunked response");
            stream.flush().expect("flush chunked end");
        }
        sender.send(requests).expect("send captured requests");
    });

    (format!("http://{}/v1", address), receiver, handle)
}

fn mock_reason_phrase(status_code: u16) -> &'static str {
    match status_code {
        200 => "OK",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Mock Response",
    }
}

fn spawn_mock_llm_server(
    response_body: String,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let (base_url, requests_rx, handle) = spawn_mock_llm_server_sequence(vec![response_body]);
    let (sender, receiver) = mpsc::channel();
    let unwrap_handle = thread::spawn(move || {
        let requests = requests_rx.recv().expect("receive captured requests");
        sender
            .send(
                requests
                    .into_iter()
                    .next()
                    .expect("single request should be captured"),
            )
            .expect("send single request");
        handle.join().expect("join mock llm server");
    });
    (base_url, receiver, unwrap_handle)
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

fn sse_body(events: Vec<serde_json::Value>) -> String {
    let mut body = String::new();
    for event in events {
        body.push_str(&sse_chunk(event));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn sse_chunk(event: serde_json::Value) -> String {
    format!("data: {}\n\n", event)
}

fn spawn_mock_deepseek_server(
    output_text: &str,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let response_body = sse_body(vec![json!({
        "id": "chatcmpl_test",
        "choices": [
            {
                "delta": {
                    "content": output_text
                }
            }
        ]
    })]);
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
    assert!(captured_request.contains("\"stream\":true"));
    assert!(captured_request.contains("Replace old with new in demo.txt"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_supports_deepseek_chat_streaming_long_responses() {
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
    let split_one = output_text.len() / 3;
    let split_two = (output_text.len() * 2) / 3;
    let first = output_text[..split_one].to_string();
    let second = output_text[split_one..split_two].to_string();
    let third = output_text[split_two..].to_string();
    let streamed_response = vec![
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_stream_long_1",
                "choices": [
                    {
                        "delta": {
                            "content": first
                        }
                    }
                ]
            })),
        ),
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_stream_long_1",
                "choices": [
                    {
                        "delta": {
                            "content": second
                        }
                    }
                ]
            })),
        ),
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_stream_long_1",
                "choices": [
                    {
                        "delta": {
                            "content": third
                        }
                    }
                ]
            })),
        ),
        (0, "data: [DONE]\n\n".to_string()),
    ];
    let (base_url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![streamed_response]);
    write_config_with_timeout(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-chat",
        100,
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .arg("--tests-command=grep -q new demo.txt")
        .arg("--safety-command=grep -q alpha-new-omega demo.txt")
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("stream_event attempt=1 event=1"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("stream_done attempt=1"), "stderr: {stderr}");
    assert!(!stderr.contains("phase=read_body"), "stderr: {stderr}");
    assert!(!stderr.contains("phase=read_stream"), "stderr: {stderr}");

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        1,
        "requests: {captured_requests:?}"
    );
    assert!(captured_requests[0].contains("\"stream\":true"));
    assert!(captured_requests[0].contains("\"model\":\"deepseek-chat\""));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_retries_transient_llm_http_failures() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let error_response = json!({
        "error": {
            "message": "temporary upstream overload"
        }
    })
    .to_string();
    let success_response = sse_body(vec![json!({
        "id": "chatcmpl_test_retry",
        "choices": [
            {
                "delta": {
                    "content": json!({
                        "summary": "Replace old with new in the demo file.",
                        "patches": [
                            {
                                "path": "demo.txt",
                                "find": "old",
                                "replace": "new"
                            }
                        ]
                    })
                    .to_string()
                }
            }
        ]
    })]);
    let (base_url, requests_rx, handle) = spawn_mock_llm_server_sequence_with_statuses(vec![
        (500, error_response),
        (200, success_response),
    ]);
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
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stdout.contains("\"verdict\": \"Promote\""),
        "stdout: {stdout}"
    );
    assert!(
        stderr.contains("[cordis-runtime][llm] request_start"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("phase=http_status status=500"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("request_success attempt=2/3"),
        "stderr: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        2,
        "requests: {captured_requests:?}"
    );
    assert!(captured_requests[1].contains("\"stream\":true"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_reports_timeout_diagnostics() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let response_body = json!({
        "id": "resp_slow",
        "output_text": json!({
            "summary": "Replace old with new in the demo file.",
            "patches": [
                {
                    "path": "demo.txt",
                    "find": "old",
                    "replace": "new"
                }
            ]
        })
        .to_string(),
    })
    .to_string();
    let (base_url, handle) =
        spawn_slow_mock_llm_server_sequence(vec![300, 300, 300], response_body);
    write_config_with_timeout(
        temp.path(),
        "openai",
        &base_url,
        "OPENAI_API_KEY",
        "gpt-4.1-mini",
        100,
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
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join slow mock server");
    assert!(
        !output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("request timed out after timeout_ms=100"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("attempt=3/3"), "stderr: {stderr}");
    assert!(stderr.contains("total_elapsed_ms="), "stderr: {stderr}");
    assert!(
        stderr.contains("endpoint=") && stderr.contains("/v1/responses"),
        "stderr: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-old-omega\n"
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_supports_deepseek_reasoner_tool_calls() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let first_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_1",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "Need to inspect the target file before planning.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_read_demo_batch",
                            "type": "function",
                            "function": {
                                "name": "read_context_files",
                                "arguments": "{\"paths\":[\"demo.txt\"]}"
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let second_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_2",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "The file contains old and should be updated.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_submit_plan",
                            "type": "function",
                            "function": {
                                "name": "submit_patch_plan",
                                "arguments": json!({
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
                                .to_string()
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let (base_url, requests_rx, handle) =
        spawn_mock_llm_server_sequence(vec![first_response, second_response]);
    write_config(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
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

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        2,
        "requests: {captured_requests:?}"
    );
    assert!(captured_requests[0].starts_with("POST /v1/chat/completions HTTP/1.1"));
    assert_authorization_bearer(&captured_requests[0], "test-key");
    assert!(captured_requests[0].contains("\"model\":\"deepseek-reasoner\""));
    assert!(captured_requests[0].contains("\"stream\":true"));
    assert!(captured_requests[0].contains("\"tools\""));
    assert!(captured_requests[0].contains("submit_patch_plan"));
    assert!(captured_requests[0].contains("read_context_files"));
    assert!(captured_requests[1]
        .contains("\"reasoning_content\":\"Need to inspect the target file before planning.\""));
    assert!(captured_requests[1].contains("\"tool_call_id\":\"call_read_demo_batch\""));
    assert!(captured_requests[1].contains("alpha-old-omega"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_deepseek_reasoner_surfaces_repeated_read_feedback() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let first_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_repeat_1",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "Inspect the target file first.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_read_repeat_1",
                            "type": "function",
                            "function": {
                                "name": "read_context_files",
                                "arguments": "{\"paths\":[\"demo.txt\"]}"
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let second_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_repeat_2",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "Double-check the same file.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_read_repeat_2",
                            "type": "function",
                            "function": {
                                "name": "read_context_file",
                                "arguments": "{\"path\":\"demo.txt\"}"
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let third_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_repeat_3",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "One more check before deciding.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_read_repeat_3",
                            "type": "function",
                            "function": {
                                "name": "read_context_files",
                                "arguments": "{\"paths\":[\"demo.txt\"]}"
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let fourth_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_repeat_submit",
        "choices": [
            {
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_submit_after_repeat_feedback",
                            "type": "function",
                            "function": {
                                "name": "submit_patch_plan",
                                "arguments": json!({
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
                                .to_string()
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let (base_url, requests_rx, handle) = spawn_mock_llm_server_sequence(vec![
        first_response,
        second_response,
        third_response,
        fourth_response,
    ]);
    write_config(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("reasoner_repeat_read planner_mode=patch"),
        "stderr: {stderr}"
    );

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        4,
        "requests: {captured_requests:?}"
    );
    assert!(
        captured_requests[2].contains("already_seen_paths")
            && captured_requests[2].contains("demo.txt"),
        "third request: {}",
        captured_requests[2]
    );
    assert!(
        captured_requests[3].contains("requires_change_strategy")
            && captured_requests[3].contains("consecutive_repeated_reads")
            && captured_requests[3].contains("submit_patch_plan"),
        "fourth request: {}",
        captured_requests[3]
    );
    assert!(
        captured_requests[3].contains("change strategy"),
        "fourth request: {}",
        captured_requests[3]
    );
    assert!(
        captured_requests[3].contains("submit_patch_plan"),
        "fourth request: {}",
        captured_requests[3]
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_supports_deepseek_reasoner_streaming_long_turns() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let first_stream = vec![
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_reasoner_stream_1",
                "choices": [
                    {
                        "delta": {
                            "reasoning_content": "Need to inspect the target file."
                        }
                    }
                ]
            })),
        ),
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_reasoner_stream_1",
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_read_demo_stream",
                                    "type": "function",
                                    "function": {
                                        "name": "read_context_file",
                                        "arguments": "{\"path\":\"demo.txt\"}"
                                    }
                                }
                            ]
                        }
                    }
                ]
            })),
        ),
        (0, "data: [DONE]\n\n".to_string()),
    ];
    let second_stream = vec![
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_reasoner_stream_2",
                "choices": [
                    {
                        "delta": {
                            "reasoning_content": "I can now submit a patch plan."
                        }
                    }
                ]
            })),
        ),
        (
            60,
            sse_chunk(json!({
                "id": "chatcmpl_reasoner_stream_2",
                "choices": [
                    {
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_submit_stream",
                                    "type": "function",
                                    "function": {
                                        "name": "submit_patch_plan",
                                        "arguments": json!({
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
                                        .to_string()
                                    }
                                }
                            ]
                        }
                    }
                ]
            })),
        ),
        (0, "data: [DONE]\n\n".to_string()),
    ];
    let (base_url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![first_stream, second_stream]);
    write_config_with_timeout(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
        100,
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("stream_event attempt=1 event=1"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("stream_done attempt=1"), "stderr: {stderr}");
    assert!(!stderr.contains("phase=read_stream"), "stderr: {stderr}");

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        2,
        "requests: {captured_requests:?}"
    );
    assert!(captured_requests[0].contains("\"stream\":true"));
    assert!(captured_requests[1].contains("\"stream\":true"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_deepseek_reasoner_continues_after_length_limited_reasoning() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let first_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_continue_1",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "I need a bit more room to finish planning."
                },
                "finish_reason": "length"
            }
        ]
    })]);
    let second_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_continue_2",
        "choices": [
            {
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_submit_after_continue",
                            "type": "function",
                            "function": {
                                "name": "submit_patch_plan",
                                "arguments": json!({
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
                                .to_string()
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let (base_url, requests_rx, handle) =
        spawn_mock_llm_server_sequence(vec![first_response, second_response]);
    write_config(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("reasoner_turn_continue turn=1 "),
        "stderr: {stderr}"
    );

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        2,
        "requests: {captured_requests:?}"
    );
    assert!(
        captured_requests[1]
            .contains("\"reasoning_content\":\"I need a bit more room to finish planning.\""),
        "second request: {}",
        captured_requests[1]
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_deepseek_reasoner_allows_many_turns_within_budget() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let mut responses = Vec::new();
    for idx in 0..17 {
        responses.push(sse_body(vec![json!({
            "id": format!("chatcmpl_reasoner_budget_{idx}"),
            "choices": [
                {
                    "delta": {
                        "reasoning_content": format!("budget turn {idx}")
                    },
                    "finish_reason": "length"
                }
            ]
        })]));
    }
    responses.push(sse_body(vec![json!({
        "id": "chatcmpl_reasoner_budget_submit",
        "choices": [
            {
                "delta": {
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_submit_after_budget_turns",
                            "type": "function",
                            "function": {
                                "name": "submit_patch_plan",
                                "arguments": json!({
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
                                .to_string()
                            }
                        }
                    ]
                }
            }
        ]
    })]));
    let (base_url, requests_rx, handle) = spawn_mock_llm_server_sequence(responses);
    write_config_with_timeout(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
        2_000,
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
        .env("DEEPSEEK_API_KEY", "test-key")
        .env("CORDIS_LLM_DEBUG", "1")
        .output()
        .expect("run llm-auto-update");

    handle.join().expect("join mock server");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&demo_path).expect("read demo file"),
        "alpha-new-omega\n"
    );

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("reasoner_turn_start turn=17"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("reasoner_turn_submit turn=18"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("exceeded 16 turns"), "stderr: {stderr}");

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        18,
        "requests: {captured_requests:?}"
    );
    assert!(
        captured_requests[17].contains("\"reasoning_content\":\"budget turn 16\""),
        "last request: {}",
        captured_requests[17]
    );
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_deepseek_reasoner_falls_back_after_tool_timeout() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let responses = vec![
        (
            40,
            sse_body(vec![json!({
                "id": "chatcmpl_reasoner_timeout_1",
                "choices": [
                    {
                        "delta": {
                            "reasoning_content": "I am still inspecting context.",
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_list_1",
                                    "type": "function",
                                    "function": {
                                        "name": "list_context_files",
                                        "arguments": "{}"
                                    }
                                }
                            ]
                        }
                    }
                ]
            })]),
        ),
        (
            40,
            sse_body(vec![json!({
                "id": "chatcmpl_reasoner_timeout_2",
                "choices": [
                    {
                        "delta": {
                            "reasoning_content": "I am still inspecting context.",
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_list_2",
                                    "type": "function",
                                    "function": {
                                        "name": "list_context_files",
                                        "arguments": "{}"
                                    }
                                }
                            ]
                        }
                    }
                ]
            })]),
        ),
        (
            0,
            sse_body(vec![json!({
                "id": "chatcmpl_reasoner_timeout_fallback",
                "choices": [
                    {
                        "delta": {
                            "content": json!({
                                "summary": "Replace old with new in the demo file.",
                                "patches": [
                                    {
                                        "path": "demo.txt",
                                        "find": "old",
                                        "replace": "new"
                                    }
                                ]
                            })
                            .to_string()
                        }
                    }
                ]
            })]),
        ),
    ];
    let (base_url, requests_rx, handle) = spawn_delayed_mock_llm_server_sequence(responses);
    write_config_with_timeout(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
        70,
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

    let stderr = String::from_utf8(output.stderr).expect("stderr utf-8");
    assert!(
        stderr.contains("reasoner_fallback operation=patch_planning"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("exceeded total planning budget"),
        "stderr: {stderr}"
    );

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        3,
        "requests: {captured_requests:?}"
    );
    assert!(captured_requests[0].contains("\"model\":\"deepseek-reasoner\""));
    assert!(captured_requests[1].contains("\"model\":\"deepseek-reasoner\""));
    assert!(
        captured_requests[2].contains("\"model\":\"deepseek-chat\""),
        "last request: {}",
        captured_requests[2]
    );
    assert!(captured_requests[2].contains("\"stream\":true"));
    assert!(captured_requests[2].contains("alpha-old-omega"));
}

#[cfg(not(windows))]
#[test]
fn llm_auto_update_deepseek_reasoner_retries_after_tool_feedback() {
    let temp = TempDir::new().expect("tempdir");
    let demo_path = temp.path().join("demo.txt");
    fs::write(&demo_path, "alpha-old-omega\n").expect("write demo file");

    let first_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_retry_1",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "I should inspect the file first.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_read_demo",
                            "type": "function",
                            "function": {
                                "name": "read_context_file",
                                "arguments": "{\"path\":\"demo.txt\"}"
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let second_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_retry_2",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "I'll try submitting a patch plan.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_submit_invalid",
                            "type": "function",
                            "function": {
                                "name": "submit_patch_plan",
                                "arguments": json!({
                                    "summary": "Broken first try.",
                                    "patches": [
                                        {
                                            "path": "demo.txt",
                                            "replace": "new"
                                        }
                                    ]
                                })
                                .to_string()
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let third_response = sse_body(vec![json!({
        "id": "chatcmpl_reasoner_retry_3",
        "choices": [
            {
                "delta": {
                    "reasoning_content": "The tool rejected the shape, so I will fix and resubmit.",
                    "tool_calls": [
                        {
                            "index": 0,
                            "id": "call_submit_valid",
                            "type": "function",
                            "function": {
                                "name": "submit_patch_plan",
                                "arguments": json!({
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
                                .to_string()
                            }
                        }
                    ]
                }
            }
        ]
    })]);
    let (base_url, requests_rx, handle) =
        spawn_mock_llm_server_sequence(vec![first_response, second_response, third_response]);
    write_config(
        temp.path(),
        "deepseek",
        &base_url,
        "DEEPSEEK_API_KEY",
        "deepseek-reasoner",
    );

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let output = Command::new(bin)
        .arg("llm-auto-update")
        .arg(temp.path())
        .arg("--instruction=Replace old with new in demo.txt")
        .arg("--path=demo.txt")
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

    let captured_requests = requests_rx.recv().expect("capture requests");
    assert_eq!(
        captured_requests.len(),
        3,
        "requests: {captured_requests:?}"
    );
    assert!(
        captured_requests[2].contains("\\\"ok\\\":false"),
        "request: {}",
        captured_requests[2]
    );
    assert!(
        captured_requests[2].contains("text patch for demo.txt is missing `find`"),
        "request: {}",
        captured_requests[2]
    );
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
