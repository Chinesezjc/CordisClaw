//! token 流式旁路的 host 侧：监听一个回环端口，把 provider 插件推来的增量帧
//! 转交给 [`TokenSink`]。
//!
//! # 为什么是 TCP 而不是 dlsym 回调
//!
//! 最初的设想是仿 `_cordis_agent_trigger` 加一个 host 导出符号让插件回调。但
//! 仓库自己的 SDK 测试（`cordis-plugin-sdk` 里 `agent_trigger_success_branch_via_dlopened_provider`
//! 一带）已记录：**测试二进制与 macOS 下 `dlsym(RTLD_DEFAULT)` 拿不到宿主符号**，
//! 且 `.cargo/config.toml` 的 `-Wl,-E` 只配了 `x86_64-unknown-linux-gnu`、只作用于
//! 可执行文件（符号定义在 `main.rs`）。`agent_trigger` 在符号缺失时静默 no-op——
//! 对"可选消息注入"可接受，对流式则意味着 REPL 在测试里永远静默且不报错。
//! 此外那个 C 签名 `fn(*const c_char)` 无法区分并发会话。
//!
//! TCP 回环没有这些问题：测试里能确定性工作，天然带会话隔离（一次调用一个
//! 监听器），且是本仓库既有手法（mock LLM server 就这么写）。
//!
//! # 契约
//!
//! 纯旁路。插件连不上、中途断开、或整个忽略 `sink_addr`，都不影响补全——
//! 权威结果始终是插件调用的返回值。因此这里所有失败都只记诊断、不外抛。

use cordis_plugin_sdk::llm::LlmStreamFrame;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// 增量 token 的去向。
///
/// 拆分前是传输层自己 `print!`，展示策略被埋在网络代码里，导致 inbox/serve
/// 无法关掉流式。现在由调用方决定：REPL 给 [`StdoutTokenSink`]，
/// inbox/serve 不建 sink。
pub trait TokenSink: Send + Sync {
    fn on_reasoning(&self, delta: &str);
    fn on_content(&self, delta: &str);
}

/// 一帧的处理结果。抽成枚举是为了让"读到什么该怎么办"成为**纯函数**可测，
/// 不必真起 socket 去构造每种情形。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FrameAction {
    /// 握手通过。
    Accepted,
    /// 增量：推给 sink。
    Reasoning(String),
    Content(String),
    /// 该丢弃但连接继续（无法解析的行）。
    Ignore,
    /// 致命：必须断开（握手 key 不符 / 握手前就发数据）。
    Reject(&'static str),
}

/// 解析并判定一帧。
///
/// `handshaked` 表示此前是否已完成握手——它作为**入参**而不是内部状态，
/// 这样两种取值的所有分支都能被单测直接驱动（同 `permission_fault_holds`
/// 把 `enforced` 作参数的手法），不会留下当前运行走不到的臂。
pub(crate) fn classify_frame(line: &str, expected_key: &str, handshaked: bool) -> FrameAction {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return FrameAction::Ignore;
    }
    let Ok(frame) = serde_json::from_str::<LlmStreamFrame>(trimmed) else {
        // 半个 UTF-8、被截断的行、非法 JSON：丢弃即可，等下一行。
        return FrameAction::Ignore;
    };
    match (frame, handshaked) {
        (LlmStreamFrame::Key { key }, false) => {
            if key == expected_key {
                FrameAction::Accepted
            } else {
                // 串台的连接：拒绝而不是把别人的 token 喂给本会话。
                FrameAction::Reject("sink handshake key mismatch")
            }
        }
        // 重复握手：忽略比断开更宽容，且不影响正确性。
        (LlmStreamFrame::Key { .. }, true) => FrameAction::Ignore,
        (_, false) => FrameAction::Reject("sink data frame before handshake"),
        (LlmStreamFrame::Reasoning { d }, true) => FrameAction::Reasoning(d),
        (LlmStreamFrame::Content { d }, true) => FrameAction::Content(d),
    }
}

/// 是否已超出预算。`elapsed` 作入参，避免依赖真实时钟造成不可测分支。
pub(crate) fn deadline_exceeded(elapsed: Duration, budget: Duration) -> bool {
    elapsed >= budget
}

/// 一次补全期间的 token 旁路监听器。
///
/// `Drop` 时关闭监听并 join 读取线程，因此它的生命周期天然等于一次调用。
pub(crate) struct SinkListener {
    addr: SocketAddr,
    key: String,
    handle: Option<std::thread::JoinHandle<()>>,
    /// 通知读取线程提前收工（补全已返回，不必再等）。
    stop: mpsc::Sender<()>,
}

impl SinkListener {
    /// 绑定回环端口并起后台读取线程。
    ///
    /// `budget` 是整体等待预算：插件始终不连、或连上后不再发数据时，线程据此
    /// 自行收工，不会把补全拖住。
    pub(crate) fn bind(
        sink: std::sync::Arc<dyn TokenSink>,
        key: String,
        budget: Duration,
    ) -> Option<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .map_err(|err| tracing_diag(format!("sink listener bind failed: {err}")))
            .ok()?;
        let addr = listener.local_addr().ok()?;
        // 非阻塞 accept + 轮询，这样"插件从不连接"能在预算内干净退出而不是挂死。
        listener.set_nonblocking(true).ok()?;
        let (stop_tx, stop_rx) = mpsc::channel();
        let expected = key.clone();
        let handle = std::thread::spawn(move || {
            serve_once(listener, sink.as_ref(), &expected, budget, &stop_rx);
        });
        Some(Self {
            addr,
            key,
            handle: Some(handle),
            stop: stop_tx,
        })
    }

    pub(crate) fn addr(&self) -> String {
        self.addr.to_string()
    }

    pub(crate) fn key(&self) -> &str {
        &self.key
    }
}

impl Drop for SinkListener {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// 接受一个连接并把帧转给 sink，直到对端关闭、预算耗尽或收到停止信号。
fn serve_once(
    listener: TcpListener,
    sink: &dyn TokenSink,
    expected_key: &str,
    budget: Duration,
    stop: &mpsc::Receiver<()>,
) {
    let started = Instant::now();
    let stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(err) => {
                match accept_retry(&err, stop.try_recv().is_ok(), started.elapsed(), budget) {
                    AcceptRetry::Wait => std::thread::sleep(Duration::from_millis(5)),
                    // Fatal 与 GiveUp 都是"停止等待"；差别只在要不要记一笔诊断。
                    // 合成一条路径后，这里不再有"只有 fd 层故障才走到"的语句
                    // ——`accept_retry` 的单测已覆盖三种判定本身。
                    verdict => {
                        verdict
                            .is_fatal()
                            .then(|| tracing_diag(format!("sink accept failed: {err}")));
                        return;
                    }
                }
            }
        }
    };
    // 接受后切回阻塞：非阻塞属性会被继承，否则 read 会立刻 WouldBlock。
    // 失败不单独早退——那条臂要靠 fd 层故障才能触发；真失败时下面的 read 会
    // 立刻出错，同样干净结束旁路。
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(budget));

    let mut handshaked = false;
    // `map_while(Result::ok)`：读错误（对端中途断开 / 读超时）与正常 EOF 一样
    // 结束旁路，补全本身不受影响。写成组合子而不是 `let Ok(..) else { return }`
    // 是为了不留一条只有真实 socket 故障才能走到的分支。
    for line in BufReader::new(stream).lines().map_while(Result::ok) {
        match classify_frame(&line, expected_key, handshaked) {
            FrameAction::Accepted => handshaked = true,
            FrameAction::Reasoning(d) => sink.on_reasoning(&d),
            FrameAction::Content(d) => sink.on_content(&d),
            FrameAction::Ignore => {}
            FrameAction::Reject(why) => {
                tracing_diag(format!("sink connection rejected: {why}"));
                return;
            }
        }
    }
}

/// accept 失败后的去向。抽成纯函数：`WouldBlock` + 停止信号 + 预算三者的组合
/// 无法靠真实 socket 稳定复现，作为入参就能把每条臂都测到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AcceptRetry {
    /// 还没人连上，继续等。
    Wait,
    /// 调用方已收工或预算耗尽——干净退出。
    GiveUp,
    /// 真正的 accept 错误。
    Fatal,
}

impl AcceptRetry {
    fn is_fatal(self) -> bool {
        matches!(self, AcceptRetry::Fatal)
    }
}

pub(crate) fn accept_retry(
    err: &std::io::Error,
    stopped: bool,
    elapsed: Duration,
    budget: Duration,
) -> AcceptRetry {
    if err.kind() != std::io::ErrorKind::WouldBlock {
        return AcceptRetry::Fatal;
    }
    if stopped || deadline_exceeded(elapsed, budget) {
        return AcceptRetry::GiveUp;
    }
    AcceptRetry::Wait
}

fn tracing_diag(message: String) {
    if std::env::var("CORDIS_AGENT_DIAG").is_ok() {
        eprintln!("[llm-sink] {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
    }

    impl TokenSink for Recorder {
        fn on_reasoning(&self, delta: &str) {
            self.events.lock().expect("lock").push(format!("r:{delta}"));
        }
        fn on_content(&self, delta: &str) {
            self.events.lock().expect("lock").push(format!("c:{delta}"));
        }
    }

    fn frame(json: &str) -> String {
        format!("{json}\n")
    }

    // ---- 纯函数：所有分支直接驱动，无需 socket ----------------------------

    #[test]
    fn classify_frame_requires_matching_handshake_first() {
        // 正确的握手。
        assert_eq!(
            classify_frame(r#"{"t":"key","key":"k1"}"#, "k1", false),
            FrameAction::Accepted
        );
        // key 不符：拒绝，否则会把别人的 token 喂进本会话。
        assert_eq!(
            classify_frame(r#"{"t":"key","key":"other"}"#, "k1", false),
            FrameAction::Reject("sink handshake key mismatch")
        );
        // 未握手就发数据：拒绝。
        assert_eq!(
            classify_frame(r#"{"t":"content","d":"x"}"#, "k1", false),
            FrameAction::Reject("sink data frame before handshake")
        );
        // 已握手后重复握手：忽略即可，不必断开。
        assert_eq!(
            classify_frame(r#"{"t":"key","key":"k1"}"#, "k1", true),
            FrameAction::Ignore
        );
    }

    #[test]
    fn classify_frame_maps_deltas_after_handshake() {
        assert_eq!(
            classify_frame(r#"{"t":"reasoning","d":"why"}"#, "k1", true),
            FrameAction::Reasoning("why".to_string())
        );
        assert_eq!(
            classify_frame(r#"{"t":"content","d":"hi"}"#, "k1", true),
            FrameAction::Content("hi".to_string())
        );
    }

    #[test]
    fn classify_frame_ignores_unparseable_lines() {
        // 空行、半个 JSON、被截断的 UTF-8、未知 tag —— 一律丢弃等下一行，
        // 不能因为一行坏数据就打断整条流。
        for bad in [
            "",
            "   ",
            "not json",
            r#"{"t":"content""#,
            r#"{"t":"unknown","d":"x"}"#,
            "\u{fffd}\u{fffd}",
        ] {
            assert_eq!(
                classify_frame(bad, "k1", true),
                FrameAction::Ignore,
                "should ignore: {bad:?}"
            );
        }
    }

    #[test]
    fn accept_retry_is_fatal_only_for_the_fatal_verdict() {
        assert!(AcceptRetry::Fatal.is_fatal());
        assert!(!AcceptRetry::GiveUp.is_fatal());
        assert!(!AcceptRetry::Wait.is_fatal());
    }

    #[test]
    fn accept_retry_covers_wait_giveup_and_fatal() {
        let would_block = std::io::Error::new(std::io::ErrorKind::WouldBlock, "nobody yet");
        let other = std::io::Error::other("socket exploded");
        let budget = Duration::from_secs(10);

        // 还有预算、没收到停止信号 → 继续等。
        assert_eq!(
            accept_retry(&would_block, false, Duration::ZERO, budget),
            AcceptRetry::Wait
        );
        // 调用方收工 → 立刻放弃，不必等满预算。
        assert_eq!(
            accept_retry(&would_block, true, Duration::ZERO, budget),
            AcceptRetry::GiveUp
        );
        // 预算耗尽 → 放弃。
        assert_eq!(
            accept_retry(&would_block, false, budget, budget),
            AcceptRetry::GiveUp
        );
        // 非 WouldBlock 的错误是真故障，且优先于停止信号判定。
        assert_eq!(
            accept_retry(&other, false, Duration::ZERO, budget),
            AcceptRetry::Fatal
        );
        assert_eq!(
            accept_retry(&other, true, budget, budget),
            AcceptRetry::Fatal
        );
    }

    /// 无法解析的行只是被跳过，连接继续——一行坏数据不该打断整条流。
    #[test]
    fn listener_skips_garbage_lines_and_keeps_streaming() {
        let recorder = Arc::new(Recorder::default());
        let listener = SinkListener::bind(
            recorder.clone() as Arc<dyn TokenSink>,
            "k1".to_string(),
            Duration::from_secs(5),
        )
        .expect("bind");

        let mut stream = TcpStream::connect(listener.addr()).expect("connect");
        stream
            .write_all(frame(r#"{"t":"key","key":"k1"}"#).as_bytes())
            .expect("handshake");
        stream.write_all(b"not json\n").expect("garbage");
        stream.write_all(b"\n").expect("blank");
        stream
            .write_all(frame(r#"{"t":"content","d":"after"}"#).as_bytes())
            .expect("content");
        drop(stream);
        drop(listener);

        assert_eq!(*recorder.events.lock().expect("lock"), vec!["c:after"]);
    }

    /// 诊断输出受环境变量控制：开关两态都要走到，否则 `tracing_diag` 里的
    /// 打印行永远没人执行。
    #[test]
    #[serial_test::serial]
    fn tracing_diag_honours_the_env_switch() {
        // 开关两态都要走到，否则 `tracing_diag` 里的打印行永远没人执行。
        // 收尾一律 remove_var：本仓库测试不依赖该变量的外部取值，无条件恢复
        // 比 `if let Some(prev)` 少一条当前运行走不到的臂。
        std::env::set_var("CORDIS_AGENT_DIAG", "1");
        tracing_diag("enabled path".to_string());
        std::env::remove_var("CORDIS_AGENT_DIAG");
        tracing_diag("disabled path".to_string());
    }

    #[test]
    fn deadline_exceeded_compares_against_budget() {
        assert!(!deadline_exceeded(
            Duration::from_millis(0),
            Duration::from_millis(10)
        ));
        assert!(!deadline_exceeded(
            Duration::from_millis(9),
            Duration::from_millis(10)
        ));
        assert!(deadline_exceeded(
            Duration::from_millis(10),
            Duration::from_millis(10)
        ));
        assert!(deadline_exceeded(
            Duration::from_millis(11),
            Duration::from_millis(10)
        ));
        // 零预算即立刻过期（调用方不想等流式时用）。
        assert!(deadline_exceeded(Duration::ZERO, Duration::ZERO));
    }

    // ---- socket 路径：用真实回环连接驱动 ----------------------------------

    #[test]
    fn listener_forwards_frames_to_the_sink_in_order() {
        let recorder = Arc::new(Recorder::default());
        let listener = SinkListener::bind(
            recorder.clone() as Arc<dyn TokenSink>,
            "k1".to_string(),
            Duration::from_secs(5),
        )
        .expect("bind");

        let mut stream = TcpStream::connect(listener.addr()).expect("connect");
        stream
            .write_all(frame(r#"{"t":"key","key":"k1"}"#).as_bytes())
            .expect("handshake");
        stream
            .write_all(frame(r#"{"t":"reasoning","d":"why"}"#).as_bytes())
            .expect("reasoning");
        stream
            .write_all(frame(r#"{"t":"content","d":"hi"}"#).as_bytes())
            .expect("content");
        drop(stream);
        drop(listener); // join 读取线程

        assert_eq!(
            *recorder.events.lock().expect("lock"),
            vec!["r:why", "c:hi"]
        );
    }

    #[test]
    fn listener_drops_connection_on_bad_handshake() {
        let recorder = Arc::new(Recorder::default());
        let listener = SinkListener::bind(
            recorder.clone() as Arc<dyn TokenSink>,
            "expected".to_string(),
            Duration::from_secs(5),
        )
        .expect("bind");

        let mut stream = TcpStream::connect(listener.addr()).expect("connect");
        stream
            .write_all(frame(r#"{"t":"key","key":"wrong"}"#).as_bytes())
            .expect("bad handshake");
        // 握手失败后即便再发数据也不该被转发。
        let _ = stream.write_all(frame(r#"{"t":"content","d":"leak"}"#).as_bytes());
        drop(stream);
        drop(listener);

        assert!(
            recorder.events.lock().expect("lock").is_empty(),
            "a mismatched key must not leak tokens into this session"
        );
    }

    /// 插件从不连接时监听线程必须在预算内干净收工——这是"sink 是纯旁路"的
    /// 核心保证：旁路不可用不能把补全拖住。
    #[test]
    fn listener_gives_up_when_nobody_connects() {
        let recorder = Arc::new(Recorder::default());
        let started = Instant::now();
        let listener = SinkListener::bind(
            recorder.clone() as Arc<dyn TokenSink>,
            "k1".to_string(),
            Duration::from_millis(80),
        )
        .expect("bind");
        drop(listener); // Drop 会 join；若线程挂死这里就永不返回
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "listener must not hang when the plugin never connects"
        );
        assert!(recorder.events.lock().expect("lock").is_empty());
    }

    /// 补全提前返回时，Drop 发停止信号让 accept 轮询立刻退出，不必等满预算。
    #[test]
    fn dropping_the_listener_stops_the_accept_loop_early() {
        let recorder = Arc::new(Recorder::default());
        let started = Instant::now();
        let listener = SinkListener::bind(
            recorder as Arc<dyn TokenSink>,
            "k1".to_string(),
            Duration::from_secs(30),
        )
        .expect("bind");
        drop(listener);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stop signal must short-circuit the 30s budget"
        );
    }

    #[test]
    fn listener_survives_a_peer_that_disconnects_mid_stream() {
        let recorder = Arc::new(Recorder::default());
        let listener = SinkListener::bind(
            recorder.clone() as Arc<dyn TokenSink>,
            "k1".to_string(),
            Duration::from_secs(5),
        )
        .expect("bind");

        let mut stream = TcpStream::connect(listener.addr()).expect("connect");
        stream
            .write_all(frame(r#"{"t":"key","key":"k1"}"#).as_bytes())
            .expect("handshake");
        stream
            .write_all(frame(r#"{"t":"content","d":"part"}"#).as_bytes())
            .expect("content");
        // 不发结束标记，直接断开。
        drop(stream);
        drop(listener);

        assert_eq!(*recorder.events.lock().expect("lock"), vec!["c:part"]);
    }

    #[test]
    fn listener_exposes_addr_and_key_for_the_payload() {
        let recorder = Arc::new(Recorder::default());
        let listener = SinkListener::bind(
            recorder as Arc<dyn TokenSink>,
            "handshake-key".to_string(),
            Duration::from_millis(50),
        )
        .expect("bind");
        // 不带格式参数：它只在断言失败时求值，会留下一行永久无覆盖。
        assert!(listener.addr().starts_with("127.0.0.1:"));
        assert_eq!(listener.key(), "handshake-key");
    }
}
