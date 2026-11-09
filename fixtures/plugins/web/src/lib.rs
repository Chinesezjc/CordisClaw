//! Web access plugin — search and fetch.
//!
//! Nodes:
//! - `web_search`  — search the web through DeepSeek's Search API
//!   (`POST /v1/tools/search`): title, URL, date and a summary per result
//! - `web_fetch`   — read one page through DeepSeek's Fetch API
//!   (`POST /v1/tools/fetch`), which returns Markdown; falls back to a local
//!   fetch when the vendor cannot serve the page
//!
//! Safety: only http/https URLs are allowed; localhost, loopback, and private
//! network addresses are blocked before a URL is sent to the vendor.
//!
//! Backend: plain JSON endpoints, not a model-mediated tool — searching does not
//! cost a chat completion, and the result carries no model-written prose that
//! could be mistaken for a fact.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 60;

/// Environment override (highest priority).
const SEARCH_URL_ENV: &str = "CORDIS_WEB_SEARCH_URL";
/// Environment override for the Fetch API endpoint.
const FETCH_URL_ENV: &str = "CORDIS_WEB_FETCH_URL";

/// Read `web_search_url:` from the same flat config file the rest of the
/// plugin parses (kept dependency-free on purpose).
fn config_search_url(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("web_search_url:")
            .map(|value| value.trim().trim_matches('"'))
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

/// Read `web_fetch_url:` from the same flat config file.
fn config_fetch_url(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        line.trim()
            .strip_prefix("web_fetch_url:")
            .map(|value| value.trim().trim_matches('"'))
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
    })
}

const MAX_FETCH_CHARS: usize = 8000;

// ---------------------------------------------------------------------------
// SSRF guard (P0-22)
// ---------------------------------------------------------------------------
//
// P2-2: `ip_is_forbidden` and `check_url_safety` moved to the shared
// `cordis-net` crate. Both `web` and `vision` re-export the same
// implementation via `cordis_net::*`; when adding a new outbound-HTTP
// plugin, depend on `cordis-net` rather than copy-pasting the checks.
use cordis_net::check_url_safety;

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct WebRequest {
    /// "web_search" | "web_fetch"
    node_id: String,

    #[serde(default)]
    query: Option<String>,

    /// How many results to ask for; the Search API caps this at 50.
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
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Web search — DeepSeek Search API
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

/// Search endpoint derived from the configured base, unless overridden.
///
/// DeepSeek exposes the Search API as a plain JSON endpoint
/// (`POST /v1/tools/search`), not as a model-mediated tool: the model does not
/// have to run a turn to find something out.
fn search_endpoint(base_url: &str, override_url: Option<&str>) -> String {
    match override_url.map(str::trim).filter(|value| !value.is_empty()) {
        Some(url) => url.to_string(),
        None => format!("{}/v1/tools/search", origin_of(base_url)),
    }
}

/// Scheme and host only.
///
/// The chat base URL is `https://api.deepseek.com/beta`; appending the tools
/// path to it produced `/beta/v1/tools/search` and a 404. The tools live at the
/// origin, not under the chat path.
fn origin_of(base_url: &str) -> String {
    let trimmed = base_url.trim().trim_end_matches('/');
    let Some(scheme_end) = trimmed.find("://") else {
        return trimmed.to_string();
    };
    let after_scheme = &trimmed[scheme_end + 3..];
    let host_end = after_scheme.find('/').unwrap_or(after_scheme.len());
    format!("{}{}", &trimmed[..scheme_end + 3], &after_scheme[..host_end])
}

/// Fetch endpoint derived from the configured base.
fn fetch_endpoint(base_url: &str, override_url: Option<&str>) -> String {
    match override_url.map(str::trim).filter(|value| !value.is_empty()) {
        Some(url) => url.to_string(),
        None => format!("{}/v1/tools/fetch", origin_of(base_url)),
    }
}

/// Search the web through DeepSeek's Search API.
///
/// Results carry a `summary` written for a model to read; the snippet is the
/// fallback when the summary is missing.
fn web_search(query: &str, count: usize, ori_question: Option<&str>) -> Result<String, String> {
    let (api_key, _model, base_url) =
        read_llm_config().ok_or("no api_key found in config/llm_api.yaml".to_string())?;
    let config_text = std::fs::read_to_string("config/llm_api.yaml").ok();
    let config_url = config_text.as_deref().and_then(config_search_url);
    let url = search_endpoint(
        &base_url,
        std::env::var(SEARCH_URL_ENV).ok().as_deref().or(config_url.as_deref()),
    );

    let mut body = json!({"query": query, "count": count});
    if let Some(question) = ori_question.filter(|value| !value.trim().is_empty()) {
        body["ori_question"] = json!(question);
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .map_err(|e| format!("search request: {e}"))?;

    let status = resp.status();
    let text = resp.text().map_err(|e| format!("search read body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "search API error ({}): {}",
            status.as_u16(),
            text.chars().take(300).collect::<String>()
        ));
    }
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("search parse JSON: {e}"))?;
    Ok(format_search_results(&json))
}

/// Render `{"results": [...]}` for a model to read.
fn format_search_results(json: &Value) -> String {
    let Some(results) = json["results"].as_array() else {
        return "No results found.".to_string();
    };
    if results.is_empty() {
        return "No results found.".to_string();
    }
    let mut out = format!("## Search results ({})\n\n", results.len());
    for (index, item) in results.iter().enumerate() {
        let title = item["title"].as_str().unwrap_or("(no title)");
        let url = item["url"].as_str().unwrap_or("");
        out.push_str(&format!("{}. **{}**\n   {}\n", index + 1, title, url));
        if let Some(date) = item["date"].as_str().filter(|value| !value.is_empty()) {
            out.push_str(&format!("   {date}\n"));
        }
        if let Some(summary) = item["summary"]
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| item["snippet"].as_str())
        {
            out.push_str(&format!("   {summary}\n"));
        }
        out.push('\n');
    }
    out
}

/// Fetch a page through DeepSeek's Fetch API: clean Markdown, no HTML stripping.
///
/// Falls back to the local fetcher when the API cannot serve the page (403 for
/// a blocked target, 404 for an unextractable page), so a refusal by the vendor
/// does not turn into a missing tool.
fn fetch_via_api(url_str: &str) -> Result<(String, bool), String> {
    // Still refuse internal targets: the URL travels to the vendor.
    check_url_safety(url_str)?;
    let (api_key, _model, base_url) =
        read_llm_config().ok_or("no api_key found in config/llm_api.yaml".to_string())?;
    let config_text = std::fs::read_to_string("config/llm_api.yaml").ok();
    let config_url = config_text.as_deref().and_then(config_fetch_url);
    let url = fetch_endpoint(
        &base_url,
        std::env::var(FETCH_URL_ENV).ok().as_deref().or(config_url.as_deref()),
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("build HTTP client: {e}"))?;
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&json!({ "url": url_str }))
        .send()
        .map_err(|e| format!("fetch request: {e}"))?;
    let status = resp.status();
    let text = resp.text().map_err(|e| format!("fetch read body: {e}"))?;
    if !status.is_success() {
        return Err(format!(
            "fetch API error ({}): {}",
            status.as_u16(),
            text.chars().take(300).collect::<String>()
        ));
    }
    let json: Value = serde_json::from_str(&text).map_err(|e| format!("fetch parse JSON: {e}"))?;
    let title = json["title"].as_str().unwrap_or("");
    let body = json["content"].as_str().unwrap_or("");
    if body.trim().is_empty() {
        return Err("fetch API returned no content".to_string());
    }
    let mut rendered = String::new();
    if !title.is_empty() {
        rendered.push_str(&format!("# {title}\n\n"));
    }
    rendered.push_str(body);
    let truncated = rendered.chars().count() > MAX_FETCH_CHARS;
    Ok((
        rendered.chars().take(MAX_FETCH_CHARS).collect(),
        truncated,
    ))
}

// ---------------------------------------------------------------------------
// Web fetch
// ---------------------------------------------------------------------------

fn web_fetch(url_str: &str) -> Result<(String, bool), String> {
    // P0-22: comprehensive SSRF guard. Applied at URL time and again after
    // DNS resolution, and re-applied on every redirect hop via a custom
    // redirect policy. Blocks:
    //   - loopback / private RFC1918 / CGNAT (100.64/10)
    //   - link-local + cloud metadata endpoint (169.254.169.254 lives here)
    //   - "unspecified" (0.0.0.0/8)
    //   - IPv6 ULA (fc00::/7), link-local (fe80::/10), loopback (::1),
    //     IPv4-mapped IPv6 (::ffff:127.0.0.1 style)
    //   - hosts whose DNS resolves to any of the above (DNS rebinding)
    // The previous string-prefix filter missed 172.17.0.0/12 (Docker
    // bridge), 169.254.169.254 (metadata), IPv6 ULA/link-local, and had no
    // DNS-based check at all.
    check_url_safety(url_str)?;

    let client = Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                return attempt.error("too many redirects");
            }
            if let Err(msg) = check_url_safety(attempt.url().as_str()) {
                return attempt.error(msg);
            }
            attempt.follow()
        }))
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
            let count = req.max_results.unwrap_or(8).clamp(1, 50);
            let text = web_search(query, count, None)?;
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
            match fetch_via_api(url).or_else(|_| web_fetch(url)) {
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
                "Search the web. Returns ranked results with title, URL, date and a summary written for a model to read. Use it to check a fact instead of answering from memory; follow up with web_fetch for a full page.",
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
                &["makes a POST to the DeepSeek Search API"],
                &["api key not configured", "network unavailable", "rate limited"],
            ).with_agent_accessible(),
            node_doc(
                "web_fetch",
                "Read one web page. Uses the DeepSeek Fetch API (clean Markdown, max 8000 chars) and falls back to a local fetch. Only http/https allowed; private addresses are refused.",
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
                &["makes a POST to the DeepSeek Fetch API, then an HTTP GET if it refuses"],
                &["invalid URL", "network timeout", "localhost/private IP blocked"],
            ).with_agent_accessible(),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("web_search_api", "api_v2")
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

// P2-2: SSRF tests live in `cordis-net` (see fixtures/plugins/_net/src/lib.rs)
// now that the guard implementation is shared.

#[cfg(test)]
mod tests {
    use super::{config_fetch_url, config_search_url, fetch_endpoint, origin_of, search_endpoint};

    #[test]
    fn endpoints_derive_from_the_configured_base() {
        assert_eq!(
            search_endpoint("https://api.deepseek.com", None),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(
            fetch_endpoint("https://api.deepseek.com/", None),
            "https://api.deepseek.com/v1/tools/fetch"
        );
        // The chat base carries a path (`/beta`); the tools live at the origin.
        assert_eq!(
            search_endpoint("https://api.deepseek.com/beta", None),
            "https://api.deepseek.com/v1/tools/search"
        );
        assert_eq!(
            fetch_endpoint("https://api.deepseek.com/beta/", None),
            "https://api.deepseek.com/v1/tools/fetch"
        );
        assert_eq!(origin_of("not a url"), "not a url");
        // An override wins, because a self-hosted base may not follow the path.
        assert_eq!(
            search_endpoint("https://api.deepseek.com", Some("https://proxy.example/s")),
            "https://proxy.example/s"
        );
        assert_eq!(
            fetch_endpoint("https://api.deepseek.com", Some("  ")),
            "https://api.deepseek.com/v1/tools/fetch"
        );
    }

    #[test]
    fn the_config_can_override_both_endpoints() {
        let cfg = "api_key: sk-x\nweb_search_url: https://cfg.example/s\nweb_fetch_url: https://cfg.example/f\n";
        assert_eq!(config_search_url(cfg), Some("https://cfg.example/s".to_string()));
        assert_eq!(config_fetch_url(cfg), Some("https://cfg.example/f".to_string()));
        assert_eq!(config_search_url("api_key: sk-x\n"), None);
        assert_eq!(config_fetch_url("api_key: sk-x\n"), None);
    }

    // config 里的空值 / 缺键不覆盖默认，引号会被剥掉。
    #[test]
    fn config_search_url_ignores_blank_and_missing() {
        assert!(config_search_url("api_key: sk-x\n").is_none());
        assert!(config_search_url("web_search_url:   \n").is_none());
        assert_eq!(
            config_search_url("web_search_url: \"https://x/y\"  \n").as_deref(),
            Some("https://x/y")
        );
    }
}
