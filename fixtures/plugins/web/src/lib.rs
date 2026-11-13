//! Web access plugin — search and fetch.
//!
//! Nodes:
//! - `web_search`  — search the web through DeepSeek's Search API
//!   (`POST /v1/tools/search`): title, URL, date and a summary per result
//! - `web_fetch`   — read one page. `raw: false` (the default) uses DeepSeek's
//!   Fetch API (`POST /v1/tools/fetch`) for clean Markdown and falls back to a
//!   local fetch that strips HTML; `raw: true` skips the vendor entirely and
//!   returns the untouched response body (the HTML source), which is how a
//!   caller reaches image URLs, `<meta>` tags and JSON-LD
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
use std::io::Read;
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

/// Readable-mode cut-off. Extraction output is prose, so this is generous
/// enough for an article; a longer page is marked `truncated`.
const MAX_FETCH_CHARS: usize = 8000;

/// Raw-mode memory valve: 16 MiB.
///
/// Raw mode never cuts the body at a character count. A character cut silently
/// drops information — a page sliced at 8000 or 40000 chars still *looks*
/// complete, while the `<img src>` the caller wanted may sit one byte past the
/// cut. The repository already has a lossless size mechanism for tool results
/// (spill: the full text is written to `data/spill/`, the context keeps head,
/// tail and the path), so raw mode hands the whole body back and leaves size
/// handling to that generic layer. Only the hard memory limit refuses, loudly,
/// rather than buffering an unbounded body and returning a quietly truncated
/// page. [`read_capped`] enforces the limit while reading.
const MAX_RAW_FETCH_BYTES: usize = 16 * 1024 * 1024;

/// Exact message for [`MAX_RAW_FETCH_BYTES`] refusals.
const RAW_TOO_LARGE: &str = "response exceeds 16 MiB; refuse rather than truncate";

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

    /// `web_fetch`: return the untouched local response body (HTML source)
    /// instead of extracted Markdown. See [`WebResponse::mode`].
    #[serde(default)]
    raw: bool,
}

#[derive(Debug, Serialize)]
struct WebResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,

    /// `web_fetch` only: `"readable"` (extracted Markdown) or `"raw"`
    /// (untouched response body). Tells the model what `text` actually is.
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,

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

/// Local HTTP GET.
///
/// `raw = true` returns the body exactly as received (HTML source untouched);
/// `raw = false` reduces an HTML body to text and cuts it at
/// [`MAX_FETCH_CHARS`]. Both modes run the same SSRF guard, including on every
/// redirect hop.
fn web_fetch(url_str: &str, raw: bool) -> Result<(String, bool), String> {
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
    let is_html = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(is_html_content_type);

    let full = if raw {
        read_capped(resp, MAX_RAW_FETCH_BYTES)?
    } else {
        resp.text().map_err(|e| format!("read body: {e}"))?
    };
    Ok(render_body(full, is_html, raw))
}

/// Read a response body, refusing as soon as `limit` bytes would be exceeded.
///
/// This is the raw-mode memory valve: the cap is enforced chunk by chunk, so an
/// oversized body is never buffered whole. Refusing beats truncating — a cut
/// page looks complete; spill is the lossless way to shrink a result.
fn read_capped(mut reader: impl Read, limit: usize) -> Result<String, String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|e| format!("read body: {e}"))?;
        if read == 0 {
            break;
        }
        if buf.len() + read > limit {
            return Err(RAW_TOO_LARGE.to_string());
        }
        buf.extend_from_slice(&chunk[..read]);
    }
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Whether a content-type denotes HTML. A missing or empty type is treated as
/// HTML, matching the old local-fetch behaviour.
fn is_html_content_type(content_type: &str) -> bool {
    content_type.contains("text/html") || content_type.is_empty()
}

/// Shape one local response body for the caller.
///
/// Raw mode returns `body` verbatim with `truncated = false`; its only limit is
/// the memory valve in [`read_capped`]. Readable mode strips HTML when
/// `is_html` and cuts at [`MAX_FETCH_CHARS`].
fn render_body(body: String, is_html: bool, raw: bool) -> (String, bool) {
    if raw {
        return (body, false);
    }
    let text = if is_html { strip_html(&body) } else { body };
    let truncated = text.chars().count() > MAX_FETCH_CHARS;
    (text.chars().take(MAX_FETCH_CHARS).collect(), truncated)
}

/// Label for the response `mode` field.
fn mode_label(raw: bool) -> &'static str {
    if raw {
        "raw"
    } else {
        "readable"
    }
}

/// Pick the transport. Readable mode prefers the vendor's clean Markdown and
/// falls back to a local GET; raw mode always goes local, because the vendor
/// returns extracted Markdown and strips exactly the markup (`<img src>`,
/// `<meta>`, JSON-LD) that raw mode exists to expose.
fn fetch_page(
    url: &str,
    raw: bool,
    via_api: impl FnOnce(&str) -> Result<(String, bool), String>,
    local: impl FnOnce(&str, bool) -> Result<(String, bool), String>,
) -> Result<(String, bool), String> {
    if raw {
        local(url, true)
    } else {
        via_api(url).or_else(|_| local(url, false))
    }
}

/// Build the `web_fetch` response so success and failure report the same mode.
fn fetch_response(result: Result<(String, bool), String>, raw: bool) -> WebResponse {
    let mode = Some(mode_label(raw).to_string());
    match result {
        Ok((text, truncated)) => WebResponse {
            ok: true,
            node_id: "web_fetch".to_string(),
            text: Some(text),
            truncated: Some(truncated),
            mode,
            error: None,
        },
        Err(e) => WebResponse {
            ok: false,
            node_id: "web_fetch".to_string(),
            text: None,
            truncated: None,
            mode,
            error: Some(e),
        },
    }
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
                mode: None,
                error: None,
            })
        }
        "web_fetch" => {
            let url = req.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err("url is required for web_fetch".to_string());
            }
            let result = fetch_page(url, req.raw, fetch_via_api, web_fetch);
            Ok(fetch_response(result, req.raw))
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
                "Read one web page. Default readable mode returns clean Markdown from the DeepSeek Fetch API (falling back to a local GET that strips HTML, max 8000 chars). Set raw: true to skip the vendor and get the untouched HTTP response body (the HTML source, no tag stripping) — use it for image URLs (<img src>), <meta> tags and JSON-LD that extraction drops. Raw mode has no character cut: it returns the whole body and lets result spilling handle size; only a 16 MiB memory cap refuses outright. Only http/https allowed; private addresses are refused.",
                json!({
                    "type": "object",
                    "required": ["node_id", "url"],
                    "properties": {
                        "node_id": { "type": "string", "const": "web_fetch" },
                        "url": { "type": "string", "description": "URL to fetch" },
                        "raw": {
                            "type": "boolean",
                            "description": "Return the untouched response body (HTML source) instead of extracted Markdown. Use it to read <img src> URLs, <meta> tags or JSON-LD that readable mode strips."
                        }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "text": { "type": ["string", "null"] },
                        "truncated": { "type": "boolean" },
                        "mode": {
                            "type": "string",
                            "enum": ["readable", "raw"],
                            "description": "Which mode produced text: extracted Markdown or the raw response body"
                        },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["makes a POST to the DeepSeek Fetch API in readable mode; raw mode makes only a local HTTP GET"],
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
            mode: None,
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
    use super::{
        api_handle, config_fetch_url, config_search_url, fetch_endpoint, fetch_page, fetch_response,
        handle, is_html_content_type, mode_label, origin_of, read_capped, render_body,
        search_endpoint, web_fetch, WebRequest, MAX_FETCH_CHARS, MAX_RAW_FETCH_BYTES, RAW_TOO_LARGE,
    };
    use cordis_plugin_sdk::PluginRequest;
    use std::cell::Cell;

    /// A `web_fetch` request; all fields spelled out so a new request field
    /// forces this helper to be revisited.
    fn fetch_request(url: &str, raw: bool) -> WebRequest {
        WebRequest {
            node_id: "web_fetch".to_string(),
            query: None,
            max_results: None,
            url: Some(url.to_string()),
            raw,
        }
    }

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

    // 是否剥 HTML 由 content-type 决定；缺失/空值按 HTML 处理。
    #[test]
    fn html_content_type_is_recognised() {
        assert!(is_html_content_type("text/html; charset=utf-8"));
        assert!(is_html_content_type(""));
        assert!(!is_html_content_type("application/json"));
        assert!(!is_html_content_type("text/plain"));
    }

    // raw 模式把响应体原样交出：`<img src>` 和图片地址都还在。
    // readable 模式仍走 strip_html，两者是同一个入口按模式分叉。
    #[test]
    fn raw_mode_returns_untouched_html() {
        let html = r#"<html><body><p>hi</p><img src="/pic.jpg"></body></html>"#;
        let (raw_body, raw_truncated) = render_body(html.to_string(), true, true);
        assert_eq!(raw_body, html);
        assert!(!raw_truncated);

        let (readable, readable_truncated) = render_body(html.to_string(), true, false);
        assert_eq!(readable, "hi");
        assert!(!readable.contains("<img"));
        assert!(!readable.contains("/pic.jpg"));
        assert!(!readable_truncated);
    }

    // 非 HTML 的 body 两种模式都不剥；readable 把空 content-type 当 HTML。
    #[test]
    fn non_html_bodies_are_kept_in_both_modes() {
        let json = r#"{"a":1}"#;
        assert_eq!(render_body(json.to_string(), false, false).0, json);
        assert_eq!(render_body(json.to_string(), false, true).0, json);
        assert_eq!(render_body("<b>x</b>".to_string(), true, false).0, "x");
    }

    // raw 不在 readable 的 8000 字符处切割；readable 仍按字符（不是字节）切。
    #[test]
    fn raw_mode_does_not_truncate_at_the_readable_limit() {
        let body = "a".repeat(MAX_FETCH_CHARS + 1);
        let (raw_body, raw_truncated) = render_body(body.clone(), false, true);
        assert_eq!(raw_body, body);
        assert!(!raw_truncated);

        let (readable, readable_truncated) = render_body(body, false, false);
        assert_eq!(readable.chars().count(), MAX_FETCH_CHARS);
        assert!(readable_truncated);

        let cjk = "汉".repeat(MAX_FETCH_CHARS + 1);
        let (readable_cjk, cjk_truncated) = render_body(cjk, false, false);
        assert_eq!(readable_cjk.chars().count(), MAX_FETCH_CHARS);
        assert!(cjk_truncated);
    }

    // raw 只受 16 MiB 内存安全阀约束：边读边判，超限报错，绝不静默切割。
    #[test]
    fn raw_mode_refuses_bodies_above_the_memory_valve() {
        assert_eq!(MAX_RAW_FETCH_BYTES, 16 * 1024 * 1024);
        let body = b"12345".to_vec();
        assert_eq!(
            read_capped(std::io::Cursor::new(body.clone()), 4).unwrap_err(),
            RAW_TOO_LARGE
        );
        assert_eq!(read_capped(std::io::Cursor::new(body), 5).unwrap(), "12345");
        // 恰好等于上限的 body 完整返回；多一个字节就被拒绝。
        let at_limit = vec![b'a'; MAX_RAW_FETCH_BYTES];
        assert_eq!(
            read_capped(std::io::Cursor::new(at_limit), MAX_RAW_FETCH_BYTES)
                .unwrap()
                .len(),
            MAX_RAW_FETCH_BYTES
        );
        let over_limit = vec![b'a'; MAX_RAW_FETCH_BYTES + 1];
        assert_eq!(
            read_capped(std::io::Cursor::new(over_limit), MAX_RAW_FETCH_BYTES).unwrap_err(),
            RAW_TOO_LARGE
        );
    }

    // 读取失败与非法 UTF-8 都走明确错误 / 有损解码，不 panic。
    #[test]
    fn read_capped_reports_read_errors_and_decodes_lossily() {
        struct BrokenReader;
        impl std::io::Read for BrokenReader {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("broken pipe"))
            }
        }
        let error = read_capped(BrokenReader, 16).unwrap_err();
        assert!(error.contains("read body: broken pipe"), "{error}");
        assert_eq!(
            read_capped(std::io::Cursor::new(vec![0xff, b'a']), 16).unwrap(),
            "\u{fffd}a"
        );
    }

    // `raw` 缺省 false；显式 true 能反序列化。
    #[test]
    fn raw_flag_defaults_to_false_and_parses_when_present() {
        let omitted: WebRequest =
            serde_json::from_str(r#"{"node_id":"web_fetch","url":"https://example.com"}"#).unwrap();
        assert!(!omitted.raw);

        let present: WebRequest = serde_json::from_str(
            r#"{"node_id":"web_fetch","url":"https://example.com","raw":true}"#,
        )
        .unwrap();
        assert!(present.raw);
        assert_eq!(present.url.as_deref(), Some("https://example.com"));
    }

    // raw 模式不碰 vendor：Fetch API 闭包一旦被调用就 panic。
    #[test]
    fn raw_mode_skips_the_vendor_api() {
        let calls = Cell::new(0);
        let result = fetch_page(
            "https://example.com",
            true,
            |_| panic!("raw mode must not call the Fetch API"),
            |url, raw| {
                calls.set(calls.get() + 1);
                assert_eq!(url, "https://example.com");
                assert!(raw);
                Ok(("html".to_string(), false))
            },
        );
        assert_eq!(result.unwrap(), ("html".to_string(), false));
        assert_eq!(calls.get(), 1);
    }

    // readable 模式 API 优先，失败才回落本地，且回落时 raw 必须是 false。
    #[test]
    fn readable_mode_prefers_the_api_and_falls_back_locally() {
        let via_api = fetch_page(
            "https://example.com",
            false,
            |_| Ok(("markdown".to_string(), true)),
            |_, _| panic!("the local fetch must not run when the API succeeds"),
        );
        assert_eq!(via_api.unwrap(), ("markdown".to_string(), true));

        let calls = Cell::new(0);
        let fallback = fetch_page(
            "https://example.com",
            false,
            |_| Err("api refused".to_string()),
            |url, raw| {
                calls.set(calls.get() + 1);
                assert_eq!(url, "https://example.com");
                assert!(!raw);
                Ok(("local".to_string(), false))
            },
        );
        assert_eq!(fallback.unwrap(), ("local".to_string(), false));
        assert_eq!(calls.get(), 1);
    }

    // 响应带 mode：成功和失败都告诉模型拿到的是哪一种。
    #[test]
    fn the_response_reports_which_mode_ran() {
        assert_eq!(mode_label(true), "raw");
        assert_eq!(mode_label(false), "readable");

        let ok = fetch_response(Ok(("html".to_string(), false)), true);
        assert!(ok.ok);
        assert_eq!(ok.mode.as_deref(), Some("raw"));
        assert_eq!(ok.truncated, Some(false));
        let serialized = serde_json::to_value(&ok).unwrap();
        assert_eq!(serialized["mode"], "raw");
        assert_eq!(serialized["text"], "html");

        let failed = fetch_response(Err("boom".to_string()), false);
        assert!(!failed.ok);
        assert_eq!(failed.mode.as_deref(), Some("readable"));
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert!(serde_json::to_value(&failed).unwrap().get("text").is_none());
    }

    // 两种模式都先过 SSRF；raw 只换传输，不降低安全检查。
    #[test]
    fn both_modes_keep_the_ssrf_guard() {
        for raw in [true, false] {
            let error = web_fetch("http://127.0.0.1/secret", raw).unwrap_err();
            assert!(error.contains("forbidden"), "{error}");
        }

        let raw_response = handle(&fetch_request(
            "http://169.254.169.254/latest/meta-data/",
            true,
        ))
        .unwrap();
        assert!(!raw_response.ok);
        assert_eq!(raw_response.mode.as_deref(), Some("raw"));
        assert!(raw_response.error.unwrap().contains("forbidden"));

        let readable_response = handle(&fetch_request("http://10.0.0.1/secret", false)).unwrap();
        assert!(!readable_response.ok);
        assert_eq!(readable_response.mode.as_deref(), Some("readable"));
    }

    #[test]
    fn web_fetch_requires_a_url() {
        assert_eq!(
            handle(&fetch_request("   ", true)).unwrap_err(),
            "url is required for web_fetch"
        );
    }

    // 解析失败时没有 mode 可言；顺带覆盖 api_handle 的错误响应。
    #[test]
    fn malformed_payloads_are_reported_without_a_mode() {
        let response = api_handle(PluginRequest {
            payload: "not json".to_string(),
        });
        let value: serde_json::Value = serde_json::from_str(&response.payload).unwrap();
        assert!(!value["ok"].as_bool().unwrap());
        assert!(value.get("mode").is_none());
    }

    // 文档必须让模型看得见 raw 开关和 mode 字段。
    #[test]
    fn the_docs_describe_both_modes() {
        let docs = serde_json::to_value(super::docs_value()).unwrap();
        let node = docs["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == "web_fetch")
            .unwrap();
        assert_eq!(node["input_schema"]["properties"]["raw"]["type"], "boolean");
        assert_eq!(
            node["output_schema"]["properties"]["mode"]["enum"],
            serde_json::json!(["readable", "raw"])
        );
        assert!(node["summary"].as_str().unwrap().contains("raw: true"));
    }
}
