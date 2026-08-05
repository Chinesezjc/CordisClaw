//! LLM provider 契约：kernel 与 provider 插件之间共享的请求/回复形状。
//!
//! 这些类型**只以 JSON 形式**穿过 `PluginRequest.payload` / `PluginResponse.payload`，
//! 从不作为裸结构体跨 FFI 边界传递——因此新增本模块不改 `RustPluginApiV2` 的
//! vtable 布局，`api_hash` 保持 `api_v2`，既有插件无需重建。
//!
//! ## 能力节点约定
//!
//! 声明 `llm_complete` 节点的已加载插件即成为 LLM provider（与
//! `soul_get`/`soul_set` 接管 soul 存取是同一套约定）。契约：
//!
//! - 请求 payload：`{node_id: "llm_complete", ...LlmCompletionRequest}`
//! - 回复 payload：`{"ok": true, ...LlmCompletion}` 或 `{"ok": false, "error": "..."}`
//!
//! ## 为什么 `body` 是不透明的 `Value`
//!
//! kernel 保留唯一一份请求体构造逻辑（消息拼装、工具规格、温度/长度上限），
//! provider 只负责把它送出去并把回复解析回 [`LlmMessage`]。这样 wire format
//! 的差异（OpenAI 的 `tool_calls` vs 其它家的形状）完全封在插件内部，而
//! kernel 侧的 agent 循环不需要知道对面是谁。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 一次补全请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LlmCompletionRequest {
    /// 供应商请求体，由 kernel 构造后原样透传。
    pub body: Value,
    /// 端点与鉴权等传输参数。
    pub transport: LlmTransportConfig,
    /// token 流式旁路的监听地址（`127.0.0.1:PORT`）。
    ///
    /// 缺省表示调用方不需要流式（例如 inbox/serve 路径），插件应直接累积完整
    /// 回复。**即便提供了地址，插件也可以忽略它**——权威结果始终是本次调用的
    /// 返回值，sink 只是旁路。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sink_addr: Option<String>,
    /// 流式旁路的关联键：插件连上 `sink_addr` 后必须首先发送
    /// `{"key":"<sink_key>"}` 一行完成握手。
    ///
    /// `handle` 可被多会话并发进入（invoke 不持锁、每次新 dlopen），此键让
    /// host 能拒绝走错的连接。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sink_key: Option<String>,
}

/// 传输层参数。模型名/温度/长度上限等属于请求体，不在此处。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct LlmTransportConfig {
    pub base_url: String,
    /// 读取 API key 的环境变量名。key 本身不经此结构体传递，由插件在请求时
    /// 从环境读取，避免密钥落进任何序列化产物。
    pub api_key_env: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    pub timeout_ms: u64,
    pub stream_timeout_secs: u64,
}

/// 一次补全的结果。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmCompletion {
    pub message: LlmMessage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

/// 模型返回的一条助手消息。
///
/// `tool_calls` 必须结构化返回：agent 循环在**本轮中途**就要据此分派工具，
/// 只回文本的 provider 无法驱动工具调用。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct LlmMessage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    /// 推理型模型的思维内容（若有）。展示上与 `content` 分开处理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<LlmToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmToolCall {
    pub id: String,
    /// 供应商的调用类型标签（OpenAI 为 `"function"`）。
    #[serde(rename = "type", default = "default_tool_call_type")]
    pub call_type: String,
    pub function: LlmToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LlmToolFunction {
    pub name: String,
    /// 未解析的 JSON 参数串——供应商按增量流式下发，拼接后可能仍不合法，
    /// 故保持字符串交由调用方解析并处理错误。
    pub arguments: String,
}

fn default_tool_call_type() -> String {
    "function".to_string()
}

/// 流式旁路的一帧。行分隔 JSON，每行一帧。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum LlmStreamFrame {
    /// 握手：必须是连接后的第一帧。
    Key { key: String },
    /// 一段推理增量。
    Reasoning { d: String },
    /// 一段正文增量。
    Content { d: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn completion_request_round_trips_with_and_without_sink() {
        let transport = LlmTransportConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            organization: Some("org".to_string()),
            project: None,
            timeout_ms: 60_000,
            stream_timeout_secs: 60,
        };
        let with_sink = LlmCompletionRequest {
            body: json!({"model": "gpt-4.1-mini", "messages": []}),
            transport: transport.clone(),
            sink_addr: Some("127.0.0.1:5300".to_string()),
            sink_key: Some("k1".to_string()),
        };
        let text = serde_json::to_string(&with_sink).expect("serialize");
        assert_eq!(
            serde_json::from_str::<LlmCompletionRequest>(&text).expect("deserialize"),
            with_sink
        );

        // 无 sink：两个字段整个消失，插件据此判定走非流式。
        let without = LlmCompletionRequest {
            body: json!({}),
            transport,
            sink_addr: None,
            sink_key: None,
        };
        let value = serde_json::to_value(&without).expect("to_value");
        assert!(value.get("sink_addr").is_none());
        assert!(value.get("sink_key").is_none());
        assert_eq!(
            serde_json::from_value::<LlmCompletionRequest>(value).expect("from_value"),
            without
        );
    }

    #[test]
    fn completion_round_trips_and_defaults_are_lenient() {
        let full = LlmCompletion {
            message: LlmMessage {
                content: Some("hi".to_string()),
                reasoning_content: Some("thinking".to_string()),
                tool_calls: vec![LlmToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: LlmToolFunction {
                        name: "read_file".to_string(),
                        arguments: r#"{"path":"a.rs"}"#.to_string(),
                    },
                }],
            },
            response_id: Some("resp_1".to_string()),
            finish_reason: Some("tool_calls".to_string()),
        };
        let text = serde_json::to_string(&full).expect("serialize");
        assert_eq!(
            serde_json::from_str::<LlmCompletion>(&text).expect("deserialize"),
            full
        );

        // 只给 message 的最小回复也要能解析：provider 未必回 id / finish_reason。
        let minimal: LlmCompletion =
            serde_json::from_str(r#"{"message":{"content":"ok"}}"#).expect("minimal");
        assert_eq!(minimal.message.content.as_deref(), Some("ok"));
        assert!(minimal.message.tool_calls.is_empty());
        assert!(minimal.response_id.is_none());
        assert!(minimal.finish_reason.is_none());

        // 空消息（纯 tool_calls 轮次里 content 可能整个缺席）。
        let empty: LlmCompletion = serde_json::from_str(r#"{"message":{}}"#).expect("empty");
        assert_eq!(empty, LlmCompletion::default());
    }

    #[test]
    fn tool_call_type_defaults_to_function() {
        // 省略 `type` 的 provider 回复按 OpenAI 惯例补 "function"。
        let call: LlmToolCall =
            serde_json::from_str(r#"{"id":"c1","function":{"name":"n","arguments":"{}"}}"#)
                .expect("deserialize");
        assert_eq!(call.call_type, "function");
        // 序列化用 `type` 而非字段名 `call_type`。
        let value = serde_json::to_value(&call).expect("to_value");
        assert_eq!(value.get("type").and_then(|v| v.as_str()), Some("function"));
        assert!(value.get("call_type").is_none());
    }

    #[test]
    fn stream_frames_round_trip_as_tagged_lines() {
        let frames = vec![
            LlmStreamFrame::Key {
                key: "k1".to_string(),
            },
            LlmStreamFrame::Reasoning {
                d: "why".to_string(),
            },
            LlmStreamFrame::Content {
                d: "hello".to_string(),
            },
        ];
        for frame in &frames {
            let line = serde_json::to_string(frame).expect("serialize");
            assert!(!line.contains('\n'), "frames must stay single-line: {line}");
            assert_eq!(
                &serde_json::from_str::<LlmStreamFrame>(&line).expect("deserialize"),
                frame
            );
        }
        assert_eq!(
            serde_json::to_string(&frames[2]).expect("serialize"),
            r#"{"t":"content","d":"hello"}"#
        );
    }
}
