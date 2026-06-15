//! Web access plugin — search and fetch.
//!
//! Nodes:
//! - `web_search`  — search the web using DeepSeek Anthropic-compatible API
//!                    (returns structured results: title + URL per result)
//! - `web_fetch`   — fetch a URL and return plain-text content
//!
//! Safety: only http/https URLs are allowed; localhost, loopback, and private
//! network addresses are blocked.
//!
//! Backend:
//! DeepSeek Anthropic-compatible endpoint with native web_search server tool.
//! Returns structured search results the agent can verify with web_fetch.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 60;
const MAX_FETCH_CHARS: usize = 8000;

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebRequest {
    /// "web_search" | "web_fetch"
    node_id: String,

    #[serde(default)]
    query: Option<String>,

    #[serde(default)]
    #[allow(dead_code)]
    max_results: Option<usize>,

    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct WebResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Web search — DeepSeek Anthropic-compatible API (native web_search tool)
// ---------------------------------------------------------------------------

fn read_llm_config() -> Option<(String, String, String)> {
    let path = "config/llm_api.yaml";
    let text = std::fs::read_to_string(path).ok()?;
    let mut api_key = None;
    let mut model = None;
    let mut base_url = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(v) = trimmed.strip_prefix("api_key: ") {
            api_key = Some(v.trim().to_string());
        } else if let Some(v) = trimmed.strip_prefix("model: ") {
            model = Some(v.trim().to_string());
        } else if let Some(v) = trimmed.strip_prefix("base_url: ") {
            base_url = Some(v.trim().to_string());
        }
    }
    Some((
        api_key?,
        model.unwrap_or_else(|| "deepseek-chat".to_string()),
        base_url.unwrap_or_else(|| "https://api.deepseek.com".to_string()),
    ))
}

fn web_search_anthropic(query: &str) -> Result<String, String> {
    let (api_key, model, _base_url) = read_llm_config()
        .ok_or("no api_key found in config/llm_api.yaml".to_string())?;

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    // Use DeepSeek's Anthropic-compatible endpoint with web_search server tool.
    let body = json!({
        "model": model,
        "max_tokens": 2048,
        "messages": [{"role": "user", "content": query}],
        "tools": [{"type": "web_search_20250305", "name": "web_search"}]
    });

    let url = "https://api.deepseek.com/anthropic/messages";
    let resp = client
        .post(url)
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("Anthropic HTTP request: {e}"))?;

    let status = resp.status();
    let resp_body = resp
        .text()
        .map_err(|e| format!("Anthropic read body: {e}"))?;

    if !status.is_success() {
        return Err(format!(
            "Anthropic API error ({}): {}",
            status.as_u16(),
            &resp_body.chars().take(500).collect::<String>()
        ));
    }

    let json: Value =
        serde_json::from_str(&resp_body).map_err(|e| format!("Anthropic parse JSON: {e}"))?;

    // Extract structured search results + model text from content blocks.
    let content_blocks = json["content"]
        .as_array()
        .ok_or_else(|| format!("unexpected response format: {}", &resp_body.chars().take(500).collect::<String>()))?;

    let mut results: Vec<String> = Vec::new();
    let mut model_text = String::new();

    for block in content_blocks {
        let block_type = block["type"].as_str().unwrap_or("");
        match block_type {
            "web_search_tool_result" => {
                if let Some(items) = block["content"].as_array() {
                    for (i, item) in items.iter().enumerate() {
                        let title = item["title"].as_str().unwrap_or("(no title)");
                        let item_url = item["url"].as_str().unwrap_or("");
                        results.push(format!("{}. **{}**\n   {}", i + 1, title, item_url));
                    }
                }
            }
            "text" => {
                if let Some(t) = block["text"].as_str() {
                    model_text.push_str(t);
                }
            }
            _ => {}
        }
    }

    let mut out = String::new();
    if !results.is_empty() {
        out.push_str(&format!("## Search results ({})\n\n", results.len()));
        out.push_str(&results.join("\n\n"));
        out.push_str("\n\n---\n\n");
    }
    if !model_text.is_empty() {
        out.push_str("**Summary:** ");
        out.push_str(&model_text);
    } else {
        out.push_str("No results found.");
    }

    Ok(out)
}

// ---------------------------------------------------------------------------
// Web fetch
// ---------------------------------------------------------------------------

fn web_fetch(url_str: &str) -> Result<(String, bool), String> {
    let parsed =
        reqwest::Url::parse(url_str).map_err(|_| format!("invalid URL: {url_str}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("only http/https allowed, got: {scheme}"));
    }
    if let Some(host) = parsed.host_str() {
        if host == "localhost" || host == "127.0.0.1" || host == "::1" {
            return Err("localhost is not allowed".to_string());
        }
        if host.starts_with("10.")
            || host.starts_with("172.16.")
            || host.starts_with("192.168.")
        {
            return Err("private network addresses are not allowed".to_string());
        }
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let resp = client
        .get(url_str)
        .send()
        .map_err(|e| format!("HTTP request: {e}"))?;
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_html = content_type.contains("text/html") || content_type.is_empty();

    let full = resp.text().map_err(|e| format!("read body: {e}"))?;
    let text = if is_html { strip_html(&full) } else { full };
    let truncated = text.len() > MAX_FETCH_CHARS;
    let truncated_text: String = text.chars().take(MAX_FETCH_CHARS).collect();

    Ok((truncated_text, truncated))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        if ch == '<' {
            in_tag = true;
            continue;
        }
        if ch == '>' {
            in_tag = false;
            if !out.ends_with(' ') {
                out.push(' ');
            }
            continue;
        }
        if !in_tag {
            out.push(ch);
        }
    }
    let collapsed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn handle(req: &WebRequest) -> Result<WebResponse, String> {
    match req.node_id.as_str() {
        "web_search" => {
            let query = req.query.as_deref().unwrap_or("").trim();
            if query.is_empty() {
                return Err("query is required for web_search".to_string());
            }
            let text = web_search_anthropic(query)?;
            Ok(WebResponse {
                ok: true,
                node_id: "web_search".to_string(),
                text: Some(text),
                truncated: None,
                error: None,
            })
        }
        "web_fetch" => {
            let url = req.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err("url is required for web_fetch".to_string());
            }
            match web_fetch(url) {
                Ok((text, truncated)) => Ok(WebResponse {
                    ok: true,
                    node_id: "web_fetch".to_string(),
                    text: Some(text),
                    truncated: Some(truncated),
                    error: None,
                }),
                Err(e) => Ok(WebResponse {
                    ok: false,
                    node_id: "web_fetch".to_string(),
                    text: None,
                    truncated: None,
                    error: Some(e),
                }),
            }
        }
        other => Err(format!("unknown node_id: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "web",
        "web",
        "0.1.0",
        None,
        vec![
            node_doc(
                "web_search",
                "Search the web using DeepSeek Anthropic-compatible API. Returns structured results (title + URL per result) plus an AI summary. Use web_fetch to verify specific pages.",
                json!({
                    "type": "object",
                    "required": ["node_id", "query"],
                    "properties": {
                        "node_id": { "type": "string", "const": "web_search" },
                        "query": { "type": "string", "description": "Search query" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "text": { "type": ["string", "null"], "description": "Structured search results (numbered list with titles + URLs) + AI summary" },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["makes HTTP request to DeepSeek Anthropic-compatible endpoint with native web_search tool"],
                &["api key not configured", "network unavailable", "rate limited"],
            ).with_agent_accessible(),
            node_doc(
                "web_fetch",
                "Fetch a web page and return plain-text content (HTML tags stripped). Max 8000 chars. Only http/https allowed.",
                json!({
                    "type": "object",
                    "required": ["node_id", "url"],
                    "properties": {
                        "node_id": { "type": "string", "const": "web_fetch" },
                        "url": { "type": "string", "description": "URL to fetch" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "text": { "type": ["string", "null"] },
                        "truncated": { "type": "boolean" },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["makes HTTP GET request to the target URL"],
                &["invalid URL", "network timeout", "localhost/private IP blocked"],
            ).with_agent_accessible(),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "web_anthropic_search".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<WebRequest>(&req.payload)
        .map_err(|e| format!("web plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&WebResponse {
            ok: false,
            node_id: "error".to_string(),
            text: None,
            truncated: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
