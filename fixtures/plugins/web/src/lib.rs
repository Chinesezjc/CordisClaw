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
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 60;
const MAX_FETCH_CHARS: usize = 8000;

// ---------------------------------------------------------------------------
// SSRF guard (P0-22)
// ---------------------------------------------------------------------------

/// Return an error if `ip` should never be reachable from a fetched URL.
/// This is the single source of truth used by both the pre-flight URL check
/// and the redirect-policy hook.
fn ip_is_forbidden(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback() {
                return Some("loopback address");
            }
            if v4.is_private() {
                return Some("RFC1918 private address");
            }
            if v4.is_link_local() {
                return Some("link-local address (cloud metadata surface)");
            }
            if v4.is_broadcast() || v4.is_unspecified() || v4.is_multicast() {
                return Some("special-purpose address");
            }
            // CGNAT 100.64.0.0/10 — not covered by is_private.
            if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
                return Some("CGNAT (100.64/10) address");
            }
            // 0.0.0.0/8 additional guard (is_unspecified is only 0.0.0.0).
            if octets[0] == 0 {
                return Some("0.0.0.0/8 address");
            }
            None
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some("loopback address");
            }
            if v6.is_unspecified() || v6.is_multicast() {
                return Some("special-purpose address");
            }
            let seg = v6.segments();
            // IPv4-mapped IPv6 (::ffff:0:0/96) — unwrap and re-check as v4.
            if seg[0] == 0
                && seg[1] == 0
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0xffff
            {
                let mapped = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return ip_is_forbidden(IpAddr::V4(mapped));
            }
            // ULA fc00::/7
            if (seg[0] & 0xfe00) == 0xfc00 {
                return Some("IPv6 ULA (fc00::/7)");
            }
            // Link-local fe80::/10
            if (seg[0] & 0xffc0) == 0xfe80 {
                return Some("IPv6 link-local (fe80::/10)");
            }
            None
        }
    }
}

/// Validate that a URL is safe to fetch: scheme is http(s), the parsed host
/// literal (if any) is not in a forbidden range, and — for hostnames — DNS
/// resolves to only allowed addresses. Called both before the initial
/// request and on every redirect hop.
fn check_url_safety(url_str: &str) -> Result<(), String> {
    let parsed = reqwest::Url::parse(url_str)
        .map_err(|_| format!("invalid URL: {url_str}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("only http/https allowed, got: {scheme}"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("URL missing host: {url_str}"))?;

    // If the host literal is an IP, judge it directly. Otherwise resolve
    // DNS and reject if any resolved address is forbidden. Uses `ToSocketAddrs`
    // so we don't need an extra dependency; the port doesn't matter for
    // resolution.
    if let Ok(ip) = host.parse::<IpAddr>() {
        if let Some(reason) = ip_is_forbidden(ip) {
            return Err(format!("host {host} is forbidden ({reason})"));
        }
        return Ok(());
    }
    let addrs = (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?;
    let mut saw_any = false;
    for sa in addrs {
        saw_any = true;
        if let Some(reason) = ip_is_forbidden(sa.ip()) {
            return Err(format!(
                "host {host} resolves to forbidden address {}: {reason}",
                sa.ip()
            ));
        }
    }
    if !saw_any {
        return Err(format!("host {host} did not resolve to any address"));
    }
    Ok(())
}

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

#[cfg(test)]
mod ssrf_tests {
    use super::{ip_is_forbidden, IpAddr};
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn rfc1918_and_metadata_and_docker_bridges_are_blocked() {
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3))).is_some());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))).is_some());
        // Docker default bridge — used to slip through 172.16 prefix filter.
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(172, 20, 0, 1))).is_some());
        // Cloud metadata endpoint (169.254/16).
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(169, 254, 169, 254))).is_some());
        // 0.0.0.0/8.
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 1))).is_some());
        // CGNAT 100.64/10.
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(100, 64, 1, 1))).is_some());
    }

    #[test]
    fn ipv6_loopback_ula_linklocal_are_blocked() {
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::LOCALHOST)).is_some());
        // ULA.
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1))).is_some());
        // Link-local.
        assert!(ip_is_forbidden(IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1))).is_some());
        // IPv4-mapped IPv6 pointing at loopback.
        assert!(
            ip_is_forbidden(IpAddr::V6("::ffff:127.0.0.1".parse().unwrap())).is_some(),
            "IPv4-mapped loopback must round-trip through ip_is_forbidden"
        );
    }

    #[test]
    fn public_ips_are_allowed() {
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).is_none());
        assert!(ip_is_forbidden(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))).is_none());
    }
}
