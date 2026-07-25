use cordis_runtime::plugin::tooling::ensure_fixture_artifacts;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

#[allow(dead_code)]
static FIXTURES_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The pre-built `.so` artifacts under `fixtures/artifacts/` are all
/// `x86_64-unknown-linux-gnu` ELF binaries (their target triple is recorded
/// in `index.json`). On any other host the loader marks them
/// `Unavailable(AbiMismatch)` and `RuntimeHost::boot` fails, so tests that
/// need real dylib loading must skip declaratively:
///
/// ```ignore
/// if !support::linux_dylib_artifacts_available() {
///     eprintln!("[skip] fixture dylibs are x86_64-linux only; skipping on this host");
///     return;
/// }
/// ```
#[allow(dead_code)]
pub fn linux_dylib_artifacts_available() -> bool {
    cordis_plugin_sdk::CORDIS_TARGET == "x86_64-unknown-linux-gnu"
}

#[allow(dead_code)]
pub fn fixtures_root() -> PathBuf {
    FIXTURES_ROOT
        .get_or_init(|| {
            let root = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../fixtures")
                .canonicalize()
                .expect("fixtures must exist");
            ensure_fixture_artifacts(&root).expect("fixture artifacts should be ready");
            root
        })
        .clone()
}

/// 两次 LLM 请求之间 mock server 等待 accept 的上限。
///
/// 本地 30 秒：足够覆盖迭代测试中间的热缓存 cargo build，又能让真死锁
/// 的测试快速失败。CI（GitHub Actions 自动设 `CI=true`）放宽到 15 分钟：
/// 冷缓存 runner 上迭代流程中途的 fixture `cargo build` 远超 30 秒，
/// server 提前退出会丢掉后续请求，断言在"缺请求"上失败
/// （runtime_host_iterate_plugins_agent_retries_on_warning_and_promotes
/// 在 CI 第 9/11 轮以此方式复现）。
fn mock_llm_accept_timeout() -> Duration {
    if std::env::var_os("CI").is_some() {
        Duration::from_secs(900)
    } else {
        Duration::from_secs(30)
    }
}

#[allow(dead_code)]
pub fn spawn_chunked_mock_llm_server_sequence(
    responses: Vec<Vec<(u64, String)>>,
) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
    let accept_timeout = mock_llm_accept_timeout();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
    listener
        .set_nonblocking(true)
        .expect("set listener nonblocking");
    let address = listener.local_addr().expect("listener addr");
    let (sender, receiver) = mpsc::channel();

    let handle = thread::spawn(move || {
        let mut requests = Vec::new();
        for chunks in responses {
            let accept_started = std::time::Instant::now();
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(stream) => break stream,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        if accept_started.elapsed() >= accept_timeout {
                            sender.send(requests).expect("send captured requests");
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept request: {err}"),
                }
            };
            // The listener is non-blocking (accept-with-timeout loop above) and
            // the accepted stream inherits that mode on macOS: a read racing
            // ahead of the client's first bytes then fails with WouldBlock
            // instead of blocking. Switch the accepted stream back to blocking
            // before reading the request.
            stream.set_nonblocking(false).expect("set stream blocking");
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
                    content_length = value.trim().parse::<usize>().expect("content length");
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
                thread::sleep(Duration::from_millis(delay_ms));
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

#[allow(dead_code)]
pub fn sse_response(events: Vec<Value>) -> Vec<(u64, String)> {
    let mut chunks = events
        .into_iter()
        .map(|event| (0, format!("data: {}\n\n", event)))
        .collect::<Vec<_>>();
    chunks.push((0, "data: [DONE]\n\n".to_string()));
    chunks
}
