//! Keyless web tools: `web_fetch` (HTML → text) and `web_search`.
//!
//! Unlike Grok Build's `web_search` — which requires an x.ai API key — raven
//! stays privacy-first and local: both tools work with no credentials.
//! `web_search` queries DuckDuckGo's HTML endpoint and parses the result
//! titles/URLs; `web_fetch` retrieves a page and strips markup to plain text.
//!
//! Both are read-only, capped at `MAX_TOOL_OUTPUT`, and run inside the agent's
//! async loop (they need HTTP). They never execute downloaded content.

use anyhow::{Context, Result};
use std::sync::OnceLock;

/// Cap on returned text, matching the file-tool output cap.
const MAX_TOOL_OUTPUT: usize = 12_000;
/// Connect + overall request timeout for web calls (short, so a hung fetch
/// can't stall the agent loop).
const WEB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

static WEB_CLIENT: OnceLock<Result<reqwest::Client>> = OnceLock::new();

/// Return a shared short-timeout client for web calls, initializing it once.
///
/// Separate from the agent's chat client so a slow web endpoint never shares
/// (or stalls) the model request path. The client is constructed lazily on
/// first use and reused across all subsequent calls.
fn web_client() -> Result<&'static reqwest::Client> {
    WEB_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .timeout(WEB_TIMEOUT)
                .connect_timeout(std::time::Duration::from_secs(10))
                .user_agent("raven-mini-harness/0.1 (+privacy-first local agent)")
                .build()
                .context("build web client")
        })
        .as_ref()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// Validate that `url` is an absolute http(s) URL. Rejects `file://`, `data://`,
/// and other schemes to keep the agent from reading local files or abusing the
/// sandbox host.
fn validate_http_url(url: &str) -> Result<reqwest::Url> {
    let u = reqwest::Url::parse(url).context("invalid URL")?;
    match u.scheme() {
        "http" | "https" => Ok(u),
        other => anyhow::bail!("unsupported URL scheme '{other}' (only http/https allowed)"),
    }
}

/// Strip HTML tags to readable plain text (a cheap, tag-agnostic extractor).
///
/// Replaces `<script>…</script>` and `<style>…</style>` blocks with a space,
/// then removes all remaining `<…>` tags, and collapses whitespace runs.
/// Not a full DOM parser — good enough to make a fetched page readable to a
/// language model without adding an HTML dependency.
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0usize;
    let mut skip_until = 0usize; // byte index to skip past (script/style close)

    while i < bytes.len() {
        if i < skip_until {
            i += 1;
            continue;
        }
        if bytes[i] == b'<' {
            // Detect script/style open so we can skip their contents.
            let rest = &html[i..];
            let lower = rest.to_ascii_lowercase();
            if lower.starts_with("<script") {
                if let Some(close) = rest.find("</script>") {
                    skip_until = i + close + "</script>".len();
                    out.push(' ');
                    i += 1;
                    continue;
                }
            } else if lower.starts_with("<style") {
                if let Some(close) = rest.find("</style>") {
                    skip_until = i + close + "</style>".len();
                    out.push(' ');
                    i += 1;
                    continue;
                }
            }
            // Find the end of this tag and drop it.
            match rest.find('>') {
                Some(end) => {
                    out.push(' ');
                    i += end + 1;
                    continue;
                }
                None => {
                    // Unterminated '<' — keep the rest as-is.
                    out.push_str(rest);
                    break;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }

    // Collapse whitespace runs.
    let mut result = String::with_capacity(out.len());
    let mut prev_space = false;
    for c in out.chars() {
        if c.is_whitespace() {
            if !prev_space {
                result.push(' ');
            }
            prev_space = true;
        } else {
            result.push(c);
            prev_space = false;
        }
    }
    result.trim().to_string()
}

/// Fetch a URL and return its readable text (HTML stripped), capped.
pub async fn fetch_text(url: &str) -> String {
    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    let u = match validate_http_url(url) {
        Ok(u) => u,
        Err(e) => return format!("Error: {e}"),
    };
    match client.get(u.clone()).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => return format!("Error: failed to read response body: {e}"),
            };
            let text = html_to_text(&body);
            cap_text(&text)
        }
        Ok(resp) => format!("Error: HTTP {} fetching {}", resp.status(), u),
        Err(e) => format!("Error: request failed: {e}"),
    }
}

/// Search the web (keyless, via DuckDuckGo's HTML endpoint) and return a
/// ranked list of `title — url` lines, capped.
///
/// `page` is 1-indexed; `None` or `Some(1)` returns the first page.
/// Page 2+ adds the `s` (start offset) parameter: `s = (page - 1) * 10`.
pub async fn search(query: &str, page: Option<u32>) -> String {
    if query.trim().is_empty() {
        return "Error: empty search query".into();
    }
    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    let page = page.unwrap_or(1).max(1);
    let mut params: Vec<(&str, &str)> = vec![("q", query.trim())];
    let s_val;
    if page > 1 {
        s_val = ((page - 1) * 10).to_string();
        params.push(("s", &s_val));
    }
    let url = reqwest::Url::parse_with_params("https://html.duckduckgo.com/html/", &params)
        .unwrap_or_else(|_| reqwest::Url::parse("https://html.duckduckgo.com/html/").unwrap());
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => return format!("Error: failed to read search response: {e}"),
            };
            cap_text(&parse_ddg_results(&body))
        }
        Ok(resp) => format!("Error: HTTP {} from search engine", resp.status()),
        Err(e) => format!("Error: search request failed: {e}"),
    }
}

/// Parse DuckDuckGo's HTML search results into `title — url` lines.
///
/// Two-tier strategy:
/// 1. Primary: extract `<a class="result__a" href="...">Title</a>` links.
/// 2. Fallback: if the primary parser finds nothing, scan the raw HTML for
///    `uddg=` redirect URLs and extract their decoded targets.
///
/// Both tiers decode the DuckDuckGo redirect (`//duckduckgo.com/l/?uddg=<urlencoded>`).
fn parse_ddg_results(html: &str) -> String {
    let lines = parse_ddg_primary(html);
    if !lines.is_empty() {
        return lines.join("\n");
    }
    let fallback = parse_ddg_fallback(html);
    if fallback.is_empty() {
        "No results found.".to_string()
    } else {
        fallback.join("\n")
    }
}

fn parse_ddg_primary(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = html;
    while let Some(rel) = rest.find("result__a") {
        let after = &rest[rel..];
        let (href_start, quote_char) = if let Some(h) = after.find("href=\"") {
            (rel + h + 6, '"')
        } else if let Some(h) = after.find("href='") {
            (rel + h + 6, '\'')
        } else {
            break;
        };
        let href_end = match html[href_start..].find(quote_char) {
            Some(e) => href_start + e,
            None => break,
        };
        let href = &html[href_start..href_end];

        let title_start = match html[href_end..].find('>') {
            Some(g) => href_end + g + 1,
            None => break,
        };
        let title_end = match html[title_start..].find("</a>") {
            Some(e) => title_start + e,
            None => break,
        };
        let title = html_to_text(&html[title_start..title_end]);

        let url = decode_ddg_redirect(href);
        lines.push(format!("{title} — {url}"));
        rest = &html[title_end + 4..];
        if lines.len() >= 10 {
            break;
        }
    }
    lines
}

fn parse_ddg_fallback(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut rest = html;
    while let Some(pos) = rest.find("uddg=") {
        let after = &rest[pos..];
        let val_start = pos + 5;
        let val_end = match after[5..].find('&') {
            Some(e) => val_start + e,
            None => match after[5..].find('"') {
                Some(e) => val_start + e,
                None => match after[5..].find('\'') {
                    Some(e) => val_start + e,
                    None => after.len(),
                },
            },
        };
        let encoded = &rest[val_start..val_end];
        let decoded = match urlencoding_percent_decode(encoded) {
            Ok(s) => s,
            Err(_) => {
                rest = &rest[val_end..];
                continue;
            }
        };
        if !decoded.is_empty()
            && (decoded.starts_with("http://") || decoded.starts_with("https://"))
        {
            lines.push(decoded);
        }
        rest = &rest[val_end..];
        if lines.len() >= 10 {
            break;
        }
    }
    lines
}

fn urlencoding_percent_decode(s: &str) -> Result<String, ()> {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v as char);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(' ');
        } else {
            out.push(bytes[i] as char);
        }
        i += 1;
    }
    Ok(out)
}

/// Decode a DuckDuckGo redirect URL (`//duckduckgo.com/l/?uddg=<urlencoded>`)
/// back to the real target.
fn decode_ddg_redirect(href: &str) -> String {
    let url = if href.starts_with("//") {
        format!("https:{}", href)
    } else {
        href.to_string()
    };
    if let Ok(parsed) = reqwest::Url::parse(&url) {
        if let Some(uddg) = parsed
            .query_pairs()
            .find(|(k, _)| k == "uddg")
            .map(|(_, v)| v.into_owned())
        {
            return uddg;
        }
    }
    url
}

/// Truncate text char-safe to `MAX_TOOL_OUTPUT` with a marker.
fn cap_text(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= MAX_TOOL_OUTPUT {
        text.to_string()
    } else {
        let head: String = chars.iter().take(MAX_TOOL_OUTPUT).collect();
        format!("{head}…[truncated]")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_http_url_accepts_http() {
        assert!(validate_http_url("https://example.com").is_ok());
        assert!(validate_http_url("http://example.com/x?y=1").is_ok());
    }

    #[test]
    fn validate_http_url_rejects_unsafe_schemes() {
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("data:text/plain,hi").is_err());
        assert!(validate_http_url("not-a-url").is_err());
    }

    #[test]
    fn html_to_text_strips_tags_and_scripts() {
        let html = "<html><head><style>body{color:red}</style></head><body><script>alert(1)</script><h1>Hello</h1><p>World</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hello"));
        assert!(text.contains("World"));
        assert!(!text.contains("<h1>"));
        assert!(!text.contains("alert(1)"));
        assert!(!text.contains("color:red"));
    }

    #[test]
    fn html_to_text_collapses_whitespace() {
        let text = html_to_text("<p>a    b</p><p>c</p>");
        let normalized: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(normalized, "a b c");
    }

    #[test]
    fn decode_ddg_redirect_decodes_uddg() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&rut=abc";
        assert_eq!(decode_ddg_redirect(href), "https://example.com/page");
    }

    #[test]
    fn cap_text_truncates_char_safe() {
        let long = "x".repeat(20_000);
        let capped = cap_text(&long);
        assert!(capped.contains("[truncated]"));
        assert!(capped.chars().count() <= MAX_TOOL_OUTPUT + 13); // + "...[truncated]"
    }

    #[test]
    fn cap_text_passthrough_short() {
        assert_eq!(cap_text("short"), "short");
    }

    #[tokio::test]
    async fn search_empty_query_errors() {
        let out = search("   ", None).await;
        assert!(out.starts_with("Error:"));
    }

    #[test]
    fn parse_ddg_results_caps_at_10() {
        let mut html = String::new();
        for i in 0..15 {
            html.push_str(&format!(
                r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample{}.com">Result {}</a>"#,
                i, i
            ));
        }
        let out = parse_ddg_results(&html);
        assert_eq!(out.lines().count(), 10);
    }

    #[test]
    fn parse_ddg_results_handles_fewer_than_10() {
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">Only One</a>"#;
        let out = parse_ddg_results(html);
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("Only One"));
    }

    #[test]
    fn search_url_includes_s_param_for_page_2() {
        let url = reqwest::Url::parse_with_params(
            "https://html.duckduckgo.com/html/",
            &[("q", "test"), ("s", "10")],
        )
        .unwrap();
        assert!(url.as_str().contains("s=10"));
        assert!(url.as_str().contains("q=test"));
    }

    #[test]
    fn search_url_no_s_param_for_page_1() {
        let url =
            reqwest::Url::parse_with_params("https://html.duckduckgo.com/html/", &[("q", "test")])
                .unwrap();
        assert!(!url.as_str().contains("s="));
        assert!(url.as_str().contains("q=test"));
    }

    #[test]
    fn parse_ddg_primary_handles_single_quote_href() {
        let html = r#"<a class="result__a" href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com'>Test</a>"#;
        let out = parse_ddg_results(html);
        assert!(out.contains("Test"));
        assert!(out.contains("https://example.com"));
    }

    #[test]
    fn parse_ddg_fallback_extracts_uddg_urls() {
        let html = r#"<div>uddg=https%3A%2F%2Fexample.com&rut=abc</div><span>uddg=https%3A%2F%2Fother.org"</span>"#;
        let out = parse_ddg_results(html);
        assert!(out.contains("https://example.com"));
        assert!(out.contains("https://other.org"));
    }

    #[test]
    fn parse_ddg_fallback_skips_non_http_urls() {
        let html = r#"uddg=ftp%3A%2F%2Fbad.com"#;
        let out = parse_ddg_results(html);
        assert_eq!(out, "No results found.");
    }

    #[test]
    fn parse_ddg_fallback_caps_at_10() {
        let mut html = String::new();
        for i in 0..15 {
            html.push_str(&format!("uddg=https%3A%2F%2Fexample{}.com&", i));
        }
        let out = parse_ddg_results(&html);
        assert_eq!(out.lines().count(), 10);
    }

    #[test]
    fn urlencoding_percent_decode_handles_plus() {
        assert_eq!(
            urlencoding_percent_decode("hello+world").unwrap(),
            "hello world"
        );
    }

    #[test]
    fn urlencoding_percent_decode_handles_percent() {
        assert_eq!(urlencoding_percent_decode("a%20b%2Fc").unwrap(), "a b/c");
    }
}
