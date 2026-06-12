//! Web scraper sub-plugin — search the web and optionally fetch page content.
//!
//! Node:
//! - `web_scraper` — search using Brave or Bing, then optionally fetch each result's page text.
//!
//! Safety: only http/https URLs are allowed; localhost, loopback, and private
//! network addresses are blocked.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 15;
const MAX_FETCH_CHARS: usize = 4000;

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ScraperRequest {
    #[serde(default)]
    query: Option<String>,

    #[serde(default)]
    max_results: Option<usize>,

    /// Whether to fetch each result's page content (default true).
    #[serde(default = "default_fetch_content")]
    fetch_content: bool,
}

fn default_fetch_content() -> bool {
    true
}

#[derive(Debug, Serialize)]
struct ScraperResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    results: Option<Vec<ScrapedResult>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct ScrapedResult {
    title: String,
    url: String,
    snippet: String,

    /// Fetched page text (only present when fetch_content is true).
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    text_truncated: Option<bool>,
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
// Web search — router (auto-select backend by available API key)
// ---------------------------------------------------------------------------

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

fn web_search(query: &str, max_results: usize) -> Result<(Vec<SearchResult>, &'static str), String> {
    if std::env::var("BRAVE_API_KEY").is_ok() {
        return web_search_brave(query, max_results).map(|r| (r, "brave"));
    }
    if std::env::var("BING_API_KEY").is_ok() {
        return web_search_bing(query, max_results).map(|r| (r, "bing"));
    }
    Err("no search backend available: set BRAVE_API_KEY or BING_API_KEY".to_string())
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

fn handle(req: &ScraperRequest) -> Result<ScraperResponse, String> {
    let query = req.query.as_deref().unwrap_or("").trim();
    if query.is_empty() {
        return Err("query is required for web_scraper".to_string());
    }

    let max = req.max_results.unwrap_or(3).min(5);
    let (search_results, _backend) = web_search(query, max)?;

    let results: Vec<ScrapedResult> = if req.fetch_content {
        search_results
            .into_iter()
            .map(|sr| {
                let (text, text_truncated) = match web_fetch(&sr.url) {
                    Ok((t, tr)) => (Some(t), Some(tr)),
                    Err(e) => (Some(format!("[fetch error: {e}]")), Some(false)),
                };
                ScrapedResult {
                    title: sr.title,
                    url: sr.url,
                    snippet: sr.snippet,
                    text,
                    text_truncated,
                }
            })
            .collect()
    } else {
        search_results
            .into_iter()
            .map(|sr| ScrapedResult {
                title: sr.title,
                url: sr.url,
                snippet: sr.snippet,
                text: None,
                text_truncated: None,
            })
            .collect()
    };

    Ok(ScraperResponse {
        ok: true,
        node_id: "web_scraper".to_string(),
        results: Some(results),
        error: None,
    })
}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "web_scraper",
        "web/scraper",
        "0.1.0",
        None,
        vec![node_doc(
            "web_scraper",
            "Search the web and optionally fetch each result's page content. Auto-selects backend by available API key: BRAVE_API_KEY preferred, falls back to BING_API_KEY. Returns up to max_results (default 3, max 5) with title, url, snippet, and fetched text.",
            json!({
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Search query" },
                    "max_results": { "type": "integer", "description": "Max results (default 3, max 5)" },
                    "fetch_content": { "type": "boolean", "description": "Whether to fetch page content (default true)" }
                }
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "results": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": { "type": "string" },
                                "url": { "type": "string" },
                                "snippet": { "type": "string" },
                                "text": { "type": ["string", "null"] },
                                "text_truncated": { "type": "boolean" }
                            }
                        }
                    },
                    "error": { "type": ["string", "null"] }
                }
            }),
            &["makes HTTP requests to Brave/Bing search API and target URLs"],
            &["BRAVE_API_KEY and BING_API_KEY not set", "network unavailable", "rate limited", "no results found", "fetch timeout"],
        ).with_agent_accessible()],
    None)
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_web_scraper_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<ScraperRequest>(&req.payload)
        .map_err(|e| format!("web_scraper: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&ScraperResponse {
            ok: false,
            node_id: "web_scraper".to_string(),
            results: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
