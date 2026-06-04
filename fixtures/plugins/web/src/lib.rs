//! Web access plugin — search and fetch.
//!
//! Nodes:
//! - `web_search`  — search DuckDuckGo and return results
//! - `web_fetch`   — fetch a URL and return plain-text content
//!
//! Safety: only http/https URLs are allowed; localhost, loopback, and private
//! network addresses are blocked.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse, NodeType,
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
// DDG response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct DdgInstantAnswer {
    #[serde(default)]
    AbstractText: String,
    #[serde(default)]
    AbstractURL: String,
    #[serde(default)]
    AbstractSource: String,
    #[serde(default)]
    RelatedTopics: Vec<DdgRelatedTopic>,
}

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct DdgRelatedTopic {
    #[serde(default)]
    Text: String,
    #[serde(default)]
    FirstURL: String,
}

// ---------------------------------------------------------------------------
// Web fetch types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct FetchPayload {
    url: String,
}

// ---------------------------------------------------------------------------
// Web search
// ---------------------------------------------------------------------------

fn web_search(query: &str, max_results: usize) -> Result<Vec<SearchResult>, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;

    let url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencode(query)
    );

    let resp = client.get(&url).send().map_err(|e| format!("HTTP request: {e}"))?;
    let body: DdgInstantAnswer =
        resp.json().map_err(|e| format!("parse response: {e}"))?;

    let mut items = Vec::new();

    if !body.AbstractText.is_empty() {
        items.push(SearchResult {
            title: body.AbstractSource,
            url: body.AbstractURL,
            snippet: body.AbstractText,
        });
    }

    for topic in body.RelatedTopics.iter().take(max_results.saturating_sub(items.len())) {
        if topic.Text.is_empty() {
            continue;
        }
        let (title, snippet) = match topic.Text.split_once(" — ") {
            Some((t, s)) => (t.to_string(), s.to_string()),
            None => (topic.Text.clone(), String::new()),
        };
        items.push(SearchResult {
            title,
            url: topic.FirstURL.clone(),
            snippet,
        });
    }

    Ok(items)
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

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
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
            let results = web_search(query, max)?;
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
                "Search the web using DuckDuckGo. Returns up to max_results items with title, url, and snippet.",
                json!({
                    "type": "object",
                    "required": ["node_id", "query"],
                    "properties": {
                        "node_id": { "type": "string", "const": "web_search" },
                        "query": { "type": "string", "description": "Search query" },
                        "max_results": { "type": "integer", "description": "Max results (default 5, max 20)" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "results": { "type": "array", "items": { "type": "object" } },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["makes HTTP request to DuckDuckGo API"],
                &["network unavailable", "rate limited", "no results found"],
            ),
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
            ),
        ],
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_web_v1".to_string(),
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
