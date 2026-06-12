//! Web access plugin — search and fetch.
//!
//! Nodes:
//! - `web_search`  — search the web using DeepSeek, Brave, or Bing (auto-selects by available API key)
//! - `web_fetch`   — fetch a URL and return plain-text content
//!
//! Safety: only http/https URLs are allowed; localhost, loopback, and private
//! network addresses are blocked.
//!
//! Backends (tried in order):
//! 1. DeepSeek  — DEEPSEEK_API_KEY env var, returns AI-summarised text via /v1/chat + enable_search
//! 2. Brave     — BRAVE_API_KEY env var, returns structured results (title/url/snippet)
//! 3. Bing      — BING_API_KEY env var, returns structured results (title/url/snippet)

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 15;
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
    max_results: Option<usize>,

    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Serialize)]
struct WebResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<SearchResult>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

// ---------------------------------------------------------------------------
// Web search — Bing API
// ---------------------------------------------------------------------------

fn web_search_bing(query: &str, max_results: usize) -> Result<Vec<SearchResult>, String> {
    let api_key = std::env::var("BING_API_KEY")
        .map_err(|_| "BING_API_KEY environment variable not set".to_string())?;

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let resp = client
        .get("https://api.bing.microsoft.com/v7.0/search")
        .header("Ocp-Apim-Subscription-Key", &api_key)
        .query(&[
            ("q", query),
            ("count", &max_results.min(20).to_string()),
            ("mkt", "zh-CN"),
        ])
        .send()
        .map_err(|e| format!("HTTP request: {e}"))?;

    let body = resp.text().map_err(|e| format!("read body: {e}"))?;
    let json: Value = serde_json::from_str(&body).map_err(|e| format!("parse JSON: {e}"))?;

    let web_pages = json["webPages"]["value"]
        .as_array()
        .ok_or_else(|| "no search results found (check BING_API_KEY)".to_string())?;

    let items: Vec<SearchResult> = web_pages
        .iter()
        .take(max_results)
        .map(|item| SearchResult {
            title: item["name"].as_str().unwrap_or("").to_string(),
            url: item["url"].as_str().unwrap_or("").to_string(),
            snippet: item["snippet"].as_str().unwrap_or("").to_string(),
        })
        .filter(|r| !r.title.is_empty() && !r.url.is_empty())
        .collect();

    if items.is_empty() {
        return Err("no search results found".to_string());
    }

    Ok(items)
}

// ---------------------------------------------------------------------------
// Web search — Brave API
// ---------------------------------------------------------------------------

fn web_search_brave(query: &str, max_results: usize) -> Result<Vec<SearchResult>, String> {
    let api_key = std::env::var("BRAVE_API_KEY")
        .map_err(|_| "BRAVE_API_KEY environment variable not set".to_string())?;

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let resp = client
        .get("https://api.search.brave.com/res/v1/web/search")
        .header("X-Subscription-Token", &api_key)
        .header("Accept", "application/json")
        .query(&[
            ("q", query),
            ("count", &max_results.min(20).to_string()),
        ])
        .send()
        .map_err(|e| format!("HTTP request: {e}"))?;

    let body = resp.text().map_err(|e| format!("read body: {e}"))?;
    let json: Value = serde_json::from_str(&body).map_err(|e| format!("parse JSON: {e}"))?;

    let web_results = json["web"]["results"]
        .as_array()
        .ok_or_else(|| "no search results found (check BRAVE_API_KEY)".to_string())?;

    let items: Vec<SearchResult> = web_results
        .iter()
        .take(max_results)
        .map(|item| SearchResult {
            title: item["title"].as_str().unwrap_or("").to_string(),
            url: item["url"].as_str().unwrap_or("").to_string(),
            snippet: item["description"].as_str().unwrap_or("").to_string(),
        })
        .filter(|r| !r.title.is_empty() && !r.url.is_empty())
        .collect();

    if items.is_empty() {
        return Err("no search results found".to_string());
    }

    Ok(items)
}

// ---------------------------------------------------------------------------
// Web search — DeepSeek API (native search, returns AI-summarised text)
// ---------------------------------------------------------------------------

fn web_search_deepseek(query: &str) -> Result<String, String> {
    let api_key = std::env::var("DEEPSEEK_API_KEY")
        .map_err(|_| "DEEPSEEK_API_KEY environment variable not set".to_string())?;

    let model = std::env::var("DEEPSEEK_MODEL").unwrap_or_else(|_| "deepseek-chat".to_string());

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": query}],
        "enable_search": true
    });

    let resp = client
        .post("https://api.deepseek.com/v1/chat")
        .header("Authorization", format!("Bearer {}", api_key))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("DeepSeek HTTP request: {e}"))?;

    let status = resp.status();
    let body = resp.text().map_err(|e| format!("DeepSeek read body: {e}"))?;

    if !status.is_success() {
        return Err(format!("DeepSeek API error ({}): {}", status.as_u16(), body));
    }

    let json: Value =
        serde_json::from_str(&body).map_err(|e| format!("DeepSeek parse JSON: {e}"))?;

    let content = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| {
            format!(
                "unexpected DeepSeek response format: {}",
                &body.chars().take(500).collect::<String>()
            )
        })?;

    Ok(content.to_string())
}

// ---------------------------------------------------------------------------
// Web search — router (auto-select backend by available API key)
// ---------------------------------------------------------------------------

fn web_search_structured(query: &str, max_results: usize) -> Result<(Vec<SearchResult>, &'static str), String> {
    if std::env::var("BRAVE_API_KEY").is_ok() {
        return web_search_brave(query, max_results).map(|r| (r, "brave"));
    }
    if std::env::var("BING_API_KEY").is_ok() {
        return web_search_bing(query, max_results).map(|r| (r, "bing"));
    }
    Err("no search backend available: set DEEPSEEK_API_KEY, BRAVE_API_KEY, or BING_API_KEY".to_string())
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

    let resp = client.get(url_str).send().map_err(|e| format!("HTTP request: {e}"))?;
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
        if ch == '<' { in_tag = true; continue; }
        if ch == '>' { in_tag = false; if !out.ends_with(' ') { out.push(' '); } continue; }
        if !in_tag { out.push(ch); }
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
            let max = req.max_results.unwrap_or(5).min(20);

            // 1) Try DeepSeek native search first (returns AI-summarised text)
            if std::env::var("DEEPSEEK_API_KEY").is_ok() {
                match web_search_deepseek(query) {
                    Ok(text) => {
                        return Ok(WebResponse {
                            ok: true,
                            node_id: "web_search".to_string(),
                            results: None,
                            text: Some(text),
                            truncated: None,
                            error: None,
                        });
                    }
                    Err(e) => {
                        // Log and fall through to structured backends
                        // (don't return the error unless everything fails)
                        let _deepseek_err = e;
                    }
                }
            }

            // 2) Fall back to structured search (Brave / Bing)
            let (results, _backend) = web_search_structured(query, max)?;
            Ok(WebResponse {
                ok: true,
                node_id: "web_search".to_string(),
                results: Some(results),
                text: None,
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
                    results: None,
                    text: Some(text),
                    truncated: Some(truncated),
                    error: None,
                }),
                Err(e) => Ok(WebResponse {
                    ok: false,
                    node_id: "web_fetch".to_string(),
                    results: None,
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
                "Search the web using DeepSeek, Brave, or Bing API. Auto-selects backend by available API key: DEEPSEEK_API_KEY preferred, then BRAVE_API_KEY, then BING_API_KEY. DeepSeek returns AI-summarised text; Brave/Bing return structured results (title/url/snippet).",
                json!({
                    "type": "object",
                    "required": ["node_id", "query"],
                    "properties": {
                        "node_id": { "type": "string", "const": "web_search" },
                        "query": { "type": "string", "description": "Search query" },
                        "max_results": { "type": "integer", "description": "Max results for structured backends (default 5, max 20); ignored by DeepSeek" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "results": { "type": "array", "items": { "type": "object" }, "description": "Structured results (Brave/Bing), null for DeepSeek" },
                        "text": { "type": ["string", "null"], "description": "AI-summarised text (DeepSeek), null for structured backends" },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["makes HTTP request to DeepSeek /v1/chat, Brave Search API, or Bing API"],
                &["no API key set (DEEPSEEK_API_KEY, BRAVE_API_KEY, BING_API_KEY)", "network unavailable", "rate limited", "no results found"],
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
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "web_deepseek_v1".to_string(),
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
            results: None,
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
