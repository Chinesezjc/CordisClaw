use cordis_runtime::plugin::tooling::ensure_fixture_artifacts;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::OnceLock;
use std::thread;
use std::time::Duration;

static FIXTURES_ROOT: OnceLock<PathBuf> = OnceLock::new();

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

#[allow(dead_code)]
pub fn spawn_chunked_mock_llm_server_sequence(
    responses: Vec<Vec<(u64, String)>>,
) -> (String, mpsc::Receiver<Vec<String>>, thread::JoinHandle<()>) {
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
                        if accept_started.elapsed() >= Duration::from_secs(30) {
                            sender.send(requests).expect("send captured requests");
                            return;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept request: {err}"),
                }
            };
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
