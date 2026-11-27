//! Node-level tests for `edit_image` — `POST {base_url}/images/edits`.
//!
//! Kept in their own file so `tests/contract.rs` stays a pure doc/JSON check.
//! Every test drives the real node body through the `image_gen::testing` shim:
//! the same path guard, the same hand-built multipart encoder and the same
//! content-addressed cache write the agent hits in production, only pointed at a
//! loopback mock server and a temp directory.

use base64::Engine;
use image_gen::testing;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// 1x1 transparent PNG — the smallest legal `/images/edits` reply.
const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
/// SHA-256 of the decoded fixture (cross-checked with `sha256sum`).
const TINY_PNG_SHA256: &str = "c414cd0e204de974f73753c7e28d7638e7b3691bb8b1a2bab6b25bb7fed7ce77";

fn decode_png() -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(TINY_PNG_B64)
        .expect("decode fixture")
}

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!(
        "image-gen-edit-{tag}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Serve exactly one HTTP request with `body`, then close.
///
/// The handle yields the whole request (request line, headers and body) as lossy
/// text, so a test can assert both the endpoint and the multipart payload that
/// actually left the process.
fn mock_server(status: &str, body: String) -> (String, thread::JoinHandle<String>) {
    let status = status.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    let addr = listener.local_addr().expect("mock addr");
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(head_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    {
                        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                if name.trim().eq_ignore_ascii_case("content-length") {
                                    value.trim().parse::<usize>().ok()
                                } else {
                                    None
                                }
                            })
                            .unwrap_or(0);
                        if buffer.len() >= head_end + 4 + content_length {
                            break;
                        }
                    }
                }
                Err(_) => break,
            }
        }
        let response = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        String::from_utf8_lossy(&buffer).into_owned()
    });
    (format!("http://{addr}"), handle)
}

/// 底图落在 data/media/ 与图片输出目录之外时直接拒绝，一次 HTTP 都不发。
#[test]
fn base_image_outside_the_allowed_directories_is_rejected() {
    let dir = temp_dir("outside");
    let key_env = "IMAGE_GEN_TEST_TOKEN_EDIT_OUTSIDE_FILE";
    std::env::set_var(key_env, "test-token");
    let config = testing::config(
        // Nothing listens here: reaching the network at all would fail the test
        // with a connection error instead of the "outside" guard we expect.
        "http://127.0.0.1:1/v1",
        dir.join("images"),
        dir.join("media"),
        key_env,
    );

    let output = testing::edit_image(
        &config,
        &testing::edit_request("Cargo.toml", "add a red frame"),
    );

    assert_eq!(output["ok"], json!(false), "output: {output}");
    let error = output["error"].as_str().expect("error");
    assert!(error.contains("outside"), "error must contain outside: {error}");

    std::env::remove_var(key_env);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 底图走 multipart 上传到 /images/edits，返回与 generate_image 同形，并按
/// 同样规则把结果写进 data/media/<sha256>.png。
#[test]
fn cached_base_image_is_edited_and_the_result_is_cached() {
    let body = format!(
        r#"{{"created":1,"data":[{{"b64_json":"{TINY_PNG_B64}","revised_prompt":"BIG SALE on the poster"}}]}}"#
    );
    let (base_url, handle) = mock_server("200 OK", body);
    let dir = temp_dir("ok");
    let media_dir = dir.join("media");
    let output_dir = dir.join("images");
    let key_env = "IMAGE_GEN_TEST_TOKEN_EDIT_OK_FILE";
    std::env::set_var(key_env, "test-token");
    let config = testing::config(&base_url, output_dir.clone(), media_dir.clone(), key_env);

    std::fs::create_dir_all(&media_dir).expect("media dir");
    let base_image = media_dir.join("base.png");
    std::fs::write(&base_image, decode_png()).expect("write base image");

    let output = testing::edit_image(
        &config,
        &testing::edit_request(
            &base_image.display().to_string(),
            "add the words BIG SALE",
        ),
    );

    assert_eq!(output["ok"], json!(true), "output: {output}");
    assert_eq!(output["node_id"], json!("edit_image"));
    assert_eq!(output["sha256"], json!(TINY_PNG_SHA256));
    assert_eq!(output["format"], json!("png"));
    assert_eq!(output["bytes"], json!(decode_png().len()));
    assert_eq!(output["revised_prompt"], json!("BIG SALE on the poster"));

    let path = PathBuf::from(output["path"].as_str().expect("path"));
    assert!(path.is_absolute(), "path should be absolute: {path:?}");
    assert!(
        path.starts_with(&output_dir),
        "the edited file must land in the output directory: {path:?}"
    );
    assert_eq!(
        std::fs::read(&path).expect("read edited image"),
        decode_png()
    );
    assert_eq!(
        output["cq_image"],
        json!(format!("[CQ:image,file={}]", path.display()))
    );

    let media = media_dir.join(format!("{TINY_PNG_SHA256}.png"));
    assert_eq!(
        output["media_path"],
        json!(media.display().to_string()),
        "media_path must be the content-addressed cache"
    );
    assert_eq!(output["media_cached"], json!(true));
    assert_eq!(
        std::fs::read(&media).expect("read media cache"),
        decode_png()
    );

    std::env::remove_var(key_env);
    let _ = std::fs::remove_dir_all(&dir);

    // The base image really went out on the wire as the `image` file part.
    let request = handle.join().expect("mock server thread");
    assert!(
        request.starts_with("POST /images/edits "),
        "request line: {request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("content-type: multipart/form-data; boundary="),
        "request: {request}"
    );
    assert!(
        request.contains("Content-Disposition: form-data; name=\"image\"; filename=\"base.png\""),
        "request: {request}"
    );
    assert!(
        request.contains("name=\"prompt\""),
        "the prompt must be a text field: {request}"
    );
}

/// 手工拼的 multipart 体：文本字段 + `name="image"` 文件段 + 自带 boundary 收尾。
#[test]
fn multipart_body_carries_the_image_as_a_named_file_part() {
    let boundary = "image-gen-test-boundary-42";
    let image = decode_png();
    let body = testing::multipart_body(
        boundary,
        &[("model", "gpt-image-2.5-flare"), ("prompt", "overlay BIG SALE")],
        "base.png",
        &image,
    );
    let text = String::from_utf8_lossy(&body);

    assert!(
        text.contains("name=\"image\"; filename="),
        "the file part must be named image: {text}"
    );
    assert!(text.contains("filename=\"base.png\""), "body: {text}");
    assert!(text.contains("Content-Type: image/png"), "body: {text}");
    assert!(
        body.ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()),
        "the body must close on its own boundary: {text}"
    );
    assert!(
        body.windows(image.len())
            .any(|window| window == image.as_slice()),
        "the raw base image bytes must be in the body"
    );

    // Same helper, same OpenAI-compatible prefix for both nodes.
    assert_eq!(
        testing::endpoint_for("http://127.0.0.1:8792/v1/", "edits"),
        "http://127.0.0.1:8792/v1/images/edits"
    );
    assert_eq!(
        testing::endpoint_for("http://127.0.0.1:8792/v1", "generations"),
        "http://127.0.0.1:8792/v1/images/generations"
    );
}

/// 上游返回错误时错误信息透传，节点结果仍是 ok=false 而不是 ABI 错误。
#[test]
fn upstream_error_is_reported_as_ok_false() {
    let body = r#"{"error":{"message":"base image could not be decoded"}}"#.to_string();
    let (base_url, handle) = mock_server("400 Bad Request", body);
    let dir = temp_dir("err");
    let media_dir = dir.join("media");
    let key_env = "IMAGE_GEN_TEST_TOKEN_EDIT_ERR_FILE";
    std::env::set_var(key_env, "test-token");
    let config = testing::config(&base_url, dir.join("images"), media_dir.clone(), key_env);

    std::fs::create_dir_all(&media_dir).expect("media dir");
    let base_image = media_dir.join("broken-input.png");
    std::fs::write(&base_image, decode_png()).expect("write base image");

    let output = testing::edit_image(
        &config,
        &testing::edit_request(&base_image.display().to_string(), "restyle"),
    );

    assert_eq!(output["ok"], json!(false), "output: {output}");
    let error: &Value = &output["error"];
    let error = error.as_str().expect("error");
    assert!(error.contains("400"), "error: {error}");
    assert!(error.contains("could not be decoded"), "error: {error}");

    std::env::remove_var(key_env);
    let _ = std::fs::remove_dir_all(&dir);
    handle.join().expect("mock server thread");
}
