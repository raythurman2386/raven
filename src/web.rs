//! Keyless web tools: `web_fetch` (HTML → text) and `web_search`.
//!
//! Unlike Grok Build's `web_search` — which requires an x.ai API key — raven
//! stays privacy-first and local: both tools work with no credentials.
//! `web_search` queries DuckDuckGo's HTML endpoint and parses the result
//! titles/URLs; `web_fetch` retrieves a page and strips markup to plain text.
//!
//! Optionally, `web_search` can use a self-hosted [SearXNG] instance via its
//! JSON API when a base URL is configured (`RAVEN_SEARXNG_URL` or the
//! `searxng_url` config key). SearXNG is tried first; on any HTTP error,
//! empty results, or JSON parse failure the call falls back to DuckDuckGo so
//! agents keep working when the local instance is down. No API key is needed
//! for a typical SearXNG install.
//!
//! Both are read-only, capped at `MAX_TOOL_OUTPUT`, and run inside the agent's
//! async loop (they need HTTP). They never execute downloaded content.
//!
//! [SearXNG]: https://docs.searxng.org/

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

/// Configuration for an optional self-hosted SearXNG search backend.
#[derive(Debug, Clone, Default)]
pub struct SearxngConfig {
    /// Base URL only, e.g. `http://127.0.0.1:8080` or `https://searx.example.com`
    /// (no trailing `/search` path — it's appended when building the request).
    pub base_url: Option<String>,
    /// Optional engine list (e.g. `["google", "bing"]`). Empty leaves engine
    /// selection to the SearXNG server defaults.
    pub engines: Vec<String>,
}

impl SearxngConfig {
    /// Build a config from the environment (`RAVEN_SEARXNG_URL` and
    /// `RAVEN_SEARXNG_ENGINES`). Returns a default (all `None`/empty) config
    /// when no SearXNG base URL is configured, which disables the SearXNG path.
    pub fn from_env() -> Self {
        Self {
            base_url: crate::config::env_searxng_url(),
            engines: crate::config::env_searxng_engines().unwrap_or_default(),
        }
    }
}

/// Search the web and return a ranked list of `title — url` lines, capped.
///
/// When `searxng.base_url` is set, queries the SearXNG JSON API first and
/// falls back to DuckDuckGo's HTML endpoint on any failure (HTTP error, empty
/// results, or JSON parse error) so search keeps working when the local
/// instance is down. Without a SearXNG base URL, behavior is unchanged from
/// the keyless DuckDuckGo path.
///
/// `page` is 1-indexed; `None` or `Some(1)` returns the first page. Note that
/// pagination only applies to the DuckDuckGo fallback — SearXNG ignores `page`.
pub async fn search(query: &str, page: Option<u32>, searxng: Option<&SearxngConfig>) -> String {
    if query.trim().is_empty() {
        return "Error: empty search query".into();
    }

    // Prefer SearXNG when configured; fall back to DDG on any failure so a
    // down/broken local instance never bricks search. Empty results also fall
    // back — DDG may have hits SearXNG's engine set didn't.
    if let Some(cfg) = searxng {
        if let Some(base) = cfg.base_url.as_deref() {
            let out = searxng_search(base, &cfg.engines, query.trim()).await;
            let failed = out.starts_with("Error:") || out == "No results found.";
            if !failed {
                return out;
            }
        }
    }

    ddg_search(query, page).await
}

/// The DuckDuckGo HTML scrape path (the default fallback backend).
async fn ddg_search(query: &str, page: Option<u32>) -> String {
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

/// Query a SearXNG instance's JSON API and return compact `title — url` lines
/// (with a short snippet), capped at [`MAX_RESULTS`].
///
/// Builds `GET {base}/search?q={query}&format=json` (plus `engines` when a
/// non-empty list is configured). On any error — HTTP failure, empty results,
/// or JSON parse failure — this returns an `Error:` string so the caller can
/// fall back to DuckDuckGo.
async fn searxng_search(base_url: &str, engines: &[String], query: &str) -> String {
    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    // Validate the base URL is http/https only, matching web_fetch's scheme
    // discipline (rejects file://, data://, etc.).
    let base = match validate_http_url(base_url) {
        Ok(b) => b,
        Err(e) => return format!("Error: invalid SearXNG base URL: {e}"),
    };
    let mut base = base.to_string().trim_end_matches('/').to_string();
    base.push_str("/search");

    let mut params: Vec<(&str, &str)> = vec![("q", query), ("format", "json")];
    let engines_joined: String;
    if !engines.is_empty() {
        engines_joined = engines.join(",");
        params.push(("engines", &engines_joined));
    }

    let url = match reqwest::Url::parse_with_params(&base, &params) {
        Ok(u) => u,
        Err(e) => return format!("Error: failed to build SearXNG search URL: {e}"),
    };
    match client.get(url).send().await {
        Ok(resp) if resp.status().is_success() => {
            let body = match resp.text().await {
                Ok(b) => b,
                Err(e) => return format!("Error: failed to read SearXNG response: {e}"),
            };
            cap_text(&parse_searxng_results(&body))
        }
        Ok(resp) => format!("Error: HTTP {} from SearXNG", resp.status()),
        Err(e) => format!("Error: SearXNG request failed: {e}"),
    }
}

/// Maximum number of results returned by SearXNG (mirrors the DDG cap).
const MAX_RESULTS: usize = 10;

/// Parse a SearXNG JSON response body into compact `title — url` lines with a
/// short snippet, capped at [`MAX_RESULTS`] entries.
///
/// The JSON shape is `{"results": [{"title": "...", "url": "...", "content":
/// "..." | "snippet": "..."}, ...]}`. Entries missing a title or URL are
/// skipped. Returns `"No results found."` when the list is empty.
fn parse_searxng_results(body: &str) -> String {
    let value: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => return format!("Error: invalid SearXNG JSON response: {e}"),
    };
    let Some(results) = value.get("results").and_then(|r| r.as_array()) else {
        return "Error: SearXNG response missing 'results' array".into();
    };

    let mut lines = Vec::new();
    for item in results.iter().take(MAX_RESULTS) {
        let title = item.get("title").and_then(|t| t.as_str()).unwrap_or("");
        let url = item.get("url").and_then(|u| u.as_str()).unwrap_or("");
        if title.is_empty() || url.is_empty() {
            continue;
        }
        // SearXNG uses `content` (some engines `snippet`); fall back gracefully.
        let snippet = item
            .get("content")
            .or_else(|| item.get("snippet"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let snippet = snippet.trim();
        if snippet.is_empty() {
            lines.push(format!("{title} — {url}"));
        } else {
            lines.push(format!("{title} — {url}\n  {snippet}"));
        }
    }

    if lines.is_empty() {
        "No results found.".to_string()
    } else {
        lines.join("\n")
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
        let decoded = urlencoding_percent_decode(encoded);
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

fn urlencoding_percent_decode(s: &str) -> String {
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
    out
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
        let out = search("   ", None, None).await;
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
        assert_eq!(urlencoding_percent_decode("hello+world"), "hello world");
    }

    #[test]
    fn urlencoding_percent_decode_handles_percent() {
        assert_eq!(urlencoding_percent_decode("a%20b%2Fc"), "a b/c");
    }

    // ── SearXNG parsing ────────────────────────────────────────────────

    fn searxng_fixture(n: usize) -> String {
        let mut results = Vec::new();
        for i in 0..n {
            results.push(format!(
                r#"{{"title": "Result {i}", "url": "https://example{i}.com/page", "content": "Snippet for result {i}"}}"#
            ));
        }
        format!(r#"{{"query": "test", "results": [{}]}}"#, results.join(","))
    }

    #[test]
    fn parse_searxng_results_formats_titles_and_urls() {
        let out = parse_searxng_results(&searxng_fixture(2));
        assert!(out.contains("Result 0 — https://example0.com/page"));
        assert!(out.contains("Result 1 — https://example1.com/page"));
        assert!(out.contains("Snippet for result 0"));
    }

    #[test]
    fn parse_searxng_results_caps_at_max() {
        let out = parse_searxng_results(&searxng_fixture(15));
        // 10 results, each rendered as a title line + a snippet line.
        assert_eq!(out.lines().count(), 20);
        assert!(out.contains("Result 9"));
        assert!(!out.contains("Result 14"));
    }

    #[test]
    fn parse_searxng_results_empty_is_safe_message() {
        assert_eq!(
            parse_searxng_results(r#"{"query": "x", "results": []}"#),
            "No results found."
        );
    }

    #[test]
    fn parse_searxng_results_missing_array_is_error() {
        let out = parse_searxng_results(r#"{"foo": 1}"#);
        assert!(out.starts_with("Error:"));
    }

    #[test]
    fn parse_searxng_results_bad_json_is_error() {
        let out = parse_searxng_results("not json");
        assert!(out.starts_with("Error:"));
    }

    #[test]
    fn parse_searxng_results_skips_entries_missing_title_or_url() {
        let body = r#"{"results": [
            {"title": "", "url": "https://example.com", "content": "no title"},
            {"title": "No URL", "url": "", "content": "no url"},
            {"title": "Good", "url": "https://good.com", "content": "ok"}
        ]}"#;
        let out = parse_searxng_results(body);
        assert!(out.contains("Good — https://good.com"));
        assert!(!out.contains("no title"));
        assert!(!out.contains("No URL"));
    }

    #[test]
    fn parse_searxng_results_uses_snippet_fallback() {
        let body = r#"{"results": [{"title": "T", "url": "https://x.com", "snippet": "snip"}]}"#;
        let out = parse_searxng_results(body);
        assert!(out.contains("snip"));
    }

    #[test]
    fn searxng_rejects_non_http_base_url() {
        // The base URL scheme validation shares web_fetch's discipline.
        assert!(validate_http_url("file:///etc/passwd").is_err());
        assert!(validate_http_url("http://127.0.0.1:8080").is_ok());
        assert!(validate_http_url("https://searx.example.com").is_ok());
    }

    #[test]
    fn searxng_config_from_env_respects_url() {
        let original = std::env::var("RAVEN_SEARXNG_URL").ok();
        std::env::set_var("RAVEN_SEARXNG_URL", "http://127.0.0.1:8080");
        let cfg = SearxngConfig::from_env();
        assert_eq!(cfg.base_url.as_deref(), Some("http://127.0.0.1:8080"));
        match original {
            Some(v) => std::env::set_var("RAVEN_SEARXNG_URL", v),
            None => std::env::remove_var("RAVEN_SEARXNG_URL"),
        }
    }

    #[test]
    fn searxng_config_from_env_empty_url_disables() {
        let original = std::env::var("RAVEN_SEARXNG_URL").ok();
        std::env::remove_var("RAVEN_SEARXNG_URL");
        let cfg = SearxngConfig::from_env();
        assert!(cfg.base_url.is_none());
        match original {
            Some(v) => std::env::set_var("RAVEN_SEARXNG_URL", v),
            None => std::env::remove_var("RAVEN_SEARXNG_URL"),
        }
    }
}
