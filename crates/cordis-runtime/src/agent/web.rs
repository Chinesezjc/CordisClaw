//! Agent web tools: search and fetch.
//!
//! These provide the agent with read-only internet access — it can search
//! DuckDuckGo for information and fetch a page's plain-text content.

use crate::core::error::RuntimeError;
use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;
use std::time::Duration;

const REQWEST_TIMEOUT_SECS: u64 = 15;
const MAX_FETCH_CHARS: usize = 8000;

/// Result of a single web search result item.
#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct DdgInstantAnswer {
    #[serde(default)]
    #[allow(dead_code)]
    AbstractText: String,
    #[serde(default)]
    #[allow(dead_code)]
    AbstractURL: String,
    #[serde(default)]
    #[allow(dead_code)]
    AbstractSource: String,
    #[serde(default)]
    #[allow(dead_code)]
    RelatedTopics: Vec<DdgRelatedTopic>,
}

#[derive(Debug, Deserialize)]
struct DdgRelatedTopic {
    #[serde(default)]
    Text: String,
    #[serde(default)]
    FirstURL: String,
}

/// Search DuckDuckGo's Instant Answer API (no API key required).
/// Returns up to `max_results` items as `[{title, url, snippet}]`.
pub fn web_search(query: &str, max_results: usize) -> Result<Value, RuntimeError> {
    let client = Client::builder()
        .timeout(Duration::from_secs(REQWEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| RuntimeError::Invariant {
            message: format!("web_search: failed to build HTTP client: {e}"),
        })?;

    let url = format!(
        "https://api.duckduckgo.com/?q={}&format=json&no_html=1&skip_disambig=1",
        urlencoding(query)
    );

    let resp = client.get(&url).send().map_err(|e| RuntimeError::Invariant {
        message: format!("web_search: HTTP request failed: {e}"),
    })?;

    let body: DdgInstantAnswer =
        resp.json().map_err(|e| RuntimeError::Invariant {
            message: format!("web_search: failed to parse response: {e}"),
        })?;

    let mut items = Vec::new();

    // If DDG returned a direct answer, include it as the first result.
    if !body.AbstractText.is_empty() {
        items.push(serde_json::json!({
            "title": body.AbstractSource,
            "url": body.AbstractURL,
            "snippet": body.AbstractText,
        }));
    }

    // Collect related topics.
    for topic in body.RelatedTopics.iter().take(max_results.saturating_sub(items.len())) {
        if topic.Text.is_empty() {
            continue;
        }
        // DDG returns "title — snippet" in Text, split on first " — ".
        let (title, snippet) = match topic.Text.split_once(" — ") {
            Some((t, s)) => (t.to_string(), s.to_string()),
            None => (topic.Text.clone(), String::new()),
        };
        items.push(serde_json::json!({
            "title": title,
            "url": topic.FirstURL,
            "snippet": snippet,
        }));
    }

    Ok(Value::Array(items))
}

/// Fetch a URL and return its text content (HTML tags stripped).
pub fn web_fetch(url_str: &str) -> Result<Value, RuntimeError> {
    // Safety: only allow http/https.
    let parsed = reqwest::Url::parse(url_str).map_err(|_| RuntimeError::InvalidArgument {
        message: format!("web_fetch: invalid URL: {url_str}"),
    })?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(RuntimeError::InvalidArgument {
            message: format!("web_fetch: only http/https URLs are allowed, got: {scheme}"),
        });
    }
    // Block private / loopback addresses.
    if let Some(host) = parsed.host_str() {
        if host == "localhost" || host == "127.0.0.1" || host == "::1" {
            return Err(RuntimeError::InvalidArgument {
                message: "web_fetch: localhost is not allowed".to_string(),
            });
        }
        if host.starts_with("10.")
            || host.starts_with("172.16.")
            || host.starts_with("192.168.")
        {
            return Err(RuntimeError::InvalidArgument {
                message: "web_fetch: private network addresses are not allowed".to_string(),
            });
        }
    }

    let client = Client::builder()
        .timeout(Duration::from_secs(REQWEST_TIMEOUT_SECS))
        .build()
        .map_err(|e| RuntimeError::Invariant {
            message: format!("web_fetch: failed to build HTTP client: {e}"),
        })?;

    let resp = client.get(url_str).send().map_err(|e| RuntimeError::Invariant {
        message: format!("web_fetch: HTTP request failed: {e}"),
    })?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let is_html = content_type.contains("text/html") || content_type.is_empty();

    let full = resp
        .text()
        .map_err(|e| RuntimeError::Invariant {
            message: format!("web_fetch: failed to read response body: {e}"),
        })?;

    let text = if is_html {
        strip_html(&full)
    } else {
        full
    };

    let truncated: String = text.chars().take(MAX_FETCH_CHARS).collect();
    Ok(serde_json::json!({
        "url": url_str,
        "text": truncated,
        "truncated": text.len() > MAX_FETCH_CHARS,
    }))
}

/// Crude HTML tag stripper — removes `<...>` tags and decodes common entities.
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
    // Collapse whitespace.
    let collapsed: String = out
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    // Decode common entities.
    collapsed
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

/// Simple URL encoding for the search query.
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push_str("%20"),
            _ => {
                out.push_str(&format!("%{:02X}", b));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_spaces_and_special_chars() {
        let encoded = urlencoding("hello world & rust");
        assert_eq!(encoded, "hello%20world%20%26%20rust");
    }

    #[test]
    fn strip_html_removes_tags() {
        let text = strip_html("<p>Hello <b>world</b></p>");
        assert!(text.contains("Hello"));
        assert!(text.contains("world"));
        assert!(!text.contains('<'));
        assert!(!text.contains("</p>"));
    }

    #[test]
    fn web_fetch_rejects_file_url() {
        let result = web_fetch("file:///etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn web_fetch_rejects_localhost() {
        let result = web_fetch("http://localhost:8080/test");
        assert!(result.is_err());
    }
}
