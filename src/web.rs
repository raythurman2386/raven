//! Keyless web tools: `web_fetch` (HTML → text) and `web_search`.
//!
//! Both tools work with no credentials: `web_search` queries DuckDuckGo's
//! HTML endpoint by default and parses the result titles/URLs;
//! `web_fetch` retrieves a page and strips markup to plain text.
//!
//! Optionally, `web_search` can use a self-hosted [SearXNG] instance via its
//! JSON API when a base URL is configured (`RAVEN_SEARXNG_URL` or the
//! `searxng_url` config key). SearXNG is tried first; on any HTTP error,
//! empty results, or JSON parse failure the call falls back to DuckDuckGo so
//! agents keep working when the local instance is down. No API key is needed
//! for a typical SearXNG install.
//!
//! Both are read-only, capped, and run inside the agent's async loop (they
//! need HTTP). They never execute downloaded content. DuckDuckGo rate-limits
//! aggressive clients with a bot-detection challenge; that page is detected
//! and surfaced as an actionable error instead of being parsed as results.
//!
//! [SearXNG]: https://docs.searxng.org/

use anyhow::{Context, Result};
use std::sync::OnceLock;

/// Cap on returned text, matching the file-tool output cap.
const MAX_TOOL_OUTPUT: usize = 12_000;
/// Cap on downloaded bytes for a single `web_fetch` (the body is streamed and
/// cut here, so a huge page can't be fully downloaded before stripping).
const MAX_FETCH_BYTES: usize = 512 * 1024;
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
                .user_agent(concat!(
                    "raven/",
                    env!("CARGO_PKG_VERSION"),
                    " (+https://github.com/raythurman2386/raven)"
                ))
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
/// Replaces `<script>…</script>`, `<style>…</style>`, and HTML comment blocks
/// with a space, then removes all remaining `<…>` tags, decodes the common
/// character entities, and collapses whitespace runs. Not a full DOM parser —
/// good enough to make a fetched page readable to a language model without
/// adding an HTML dependency. Byte-safe: iterates bytes but pushes `str`
/// slices (never re-interpreting partial UTF-8 sequences as chars).
fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0usize;
    let mut skip_until = 0usize; // byte index to skip past (block close)

    while i < bytes.len() {
        if i < skip_until {
            i += 1;
            continue;
        }
        if bytes[i] == b'<' {
            let rest = &html[i..];
            let lower = rest.to_ascii_lowercase();
            let block: Option<&str> = if lower.starts_with("<script") {
                Some("</script>")
            } else if lower.starts_with("<style") {
                Some("</style>")
            } else if lower.starts_with("<!--") {
                Some("-->")
            } else {
                None
            };
            if let Some(close) = block {
                if let Some(end) = rest.find(close) {
                    skip_until = i + end + close.len();
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
        // Byte-safe text pass-through: `<` is ASCII, so a multibyte UTF-8
        // sequence can never contain one; advancing byte-by-byte over
        // non-ASCII bytes keeps the underlying `str` slices intact.
        let start = i;
        while i < bytes.len() && bytes[i] != b'<' {
            i += 1;
        }
        out.push_str(&html[start..i]);
    }

    // Decode entities before collapsing whitespace, so `&nbsp;` becomes a
    // normal space rather than U+00A0 (which renders oddly for models).
    let decoded = decode_html_entities(&out);

    // Collapse whitespace runs.
    let mut result = String::with_capacity(decoded.len());
    let mut prev_space = false;
    for c in decoded.chars() {
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

/// Decode the common HTML character entities to their plain-text equivalents.
///
/// Numeric forms (`&#65;`, `&#x41;`) and the named entities that matter for
/// readability are handled; unknown entities are left as-is. Not exhaustive —
/// good enough for search-result titles and page text.
fn decode_html_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'&' {
                i += 1;
            }
            out.push_str(&s[start..i]);
            continue;
        }
        // Find the entity terminator: ';' within a reasonable distance.
        let max_scan = (i + 10).min(bytes.len());
        let end = match bytes[i..max_scan].iter().position(|&b| b == b';') {
            Some(off) => i + off,
            None => {
                out.push('&');
                i += 1;
                continue;
            }
        };
        let name = &s[i + 1..end];
        let replacement: Option<char> = match name.strip_prefix('#') {
            Some(num) => {
                let code =
                    if let Some(hex) = num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                        u32::from_str_radix(hex, 16).ok()
                    } else {
                        num.parse::<u32>().ok()
                    };
                code.and_then(char::from_u32)
            }
            None => match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "hellip" => Some('…'),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "rsquo" => Some('\u{2019}'),
                "lsquo" => Some('\u{2018}'),
                "ldquo" => Some('\u{201C}'),
                "rdquo" => Some('\u{201D}'),
                "middot" => Some('·'),
                "copy" => Some('©'),
                "reg" => Some('®'),
                "trade" => Some('™'),
                "eacute" => Some('é'),
                "egrave" => Some('è'),
                "agrave" => Some('à'),
                "ccedil" => Some('ç'),
                "uuml" => Some('ü'),
                "ouml" => Some('ö'),
                "auml" => Some('ä'),
                "szlig" => Some('ß'),
                _ => None,
            },
        };
        match replacement {
            Some(c) => {
                out.push(c);
                i = end + 1;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// Fetch a URL and return its readable text (HTML stripped), capped.
///
/// The body is capped at 512 KB before parsing (a huge page is not fully
/// downloaded into memory), and binary content types (images, archives, …)
/// are rejected outright instead of being mangled through the HTML stripper.
pub async fn fetch_text(url: &str) -> String {
    let client = match web_client() {
        Ok(c) => c,
        Err(e) => return format!("Error: {e}"),
    };
    let u = match validate_http_url(url) {
        Ok(u) => u,
        Err(e) => return format!("Error: {e}"),
    };
    let resp = match client.get(u.clone()).send().await {
        Ok(r) => r,
        Err(e) => return format!("Error: request failed: {e}"),
    };
    if !resp.status().is_success() {
        return format!("Error: HTTP {} fetching {}", resp.status(), u);
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_html =
        content_type.contains("html") || content_type.contains("xml") || content_type.is_empty();
    if !is_html {
        return format!(
            "Error: unsupported content type '{content_type}' (web_fetch reads HTML/text pages; download binaries yourself)"
        );
    }
    // Stream-read at most MAX_FETCH_BYTES; more is truncated before stripping.
    let body = match read_capped(resp, MAX_FETCH_BYTES).await {
        Ok(b) => b,
        Err(e) => return format!("Error: failed to read response body: {e}"),
    };
    let text = String::from_utf8_lossy(&body);
    cap_text(&html_to_text(&text))
}

/// Read a response body up to `max` bytes, discarding the rest.
///
/// Streams chunk-by-chunk (via `Response::chunk`) so a huge page is cut off
/// at `max` bytes instead of being fully downloaded into memory first.
async fn read_capped(resp: reqwest::Response, max: usize) -> Result<Vec<u8>, reqwest::Error> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut stream = resp;
    while buf.len() < max {
        let chunk = match stream.chunk().await? {
            Some(c) => c,
            None => break,
        };
        let take = (max - buf.len()).min(chunk.len());
        buf.extend_from_slice(&chunk[..take]);
    }
    Ok(buf)
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
/// `page` is 1-indexed; `None` or `Some(1)` returns the first page. The
/// SearXNG backend maps it to the JSON API's `pageno` parameter.
pub async fn search(query: &str, page: Option<u32>, searxng: Option<&SearxngConfig>) -> String {
    if query.trim().is_empty() {
        return "Error: empty search query".into();
    }

    // Prefer SearXNG when configured; fall back to DDG on any failure so a
    // down/broken local instance never bricks search. Empty results also fall
    // back — DDG may have hits SearXNG's engine set didn't.
    if let Some(cfg) = searxng {
        if let Some(base) = cfg.base_url.as_deref() {
            let out = searxng_search(base, &cfg.engines, query.trim(), page).await;
            let failed = out.starts_with("Error:") || out == "No results found.";
            if !failed {
                return out;
            }
        }
    }

    ddg_search(query, page).await
}

/// Whether a DuckDuckGo response body is a bot-detection challenge page
/// rather than results (the anomaly page mentions its challenge form and
/// carries no result anchors). Surfaced as an actionable error so the model
/// knows to wait or switch backends instead of parsing junk.
fn is_ddg_challenge(html: &str) -> bool {
    (html.contains("challenge-form") || html.contains("anomaly.js")) && !html.contains("result__a")
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
            if is_ddg_challenge(&body) {
                return "Error: DuckDuckGo returned a bot-detection challenge (rate-limited). Wait a minute before searching again, or configure a SearXNG instance (RAVEN_SEARXNG_URL) as the search backend.".into();
            }
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
/// non-empty list is configured and `pageno` for 2+). On any error — HTTP
/// failure, empty results, or JSON parse failure — this returns an `Error:`
/// string so the caller can fall back to DuckDuckGo.
async fn searxng_search(
    base_url: &str,
    engines: &[String],
    query: &str,
    page: Option<u32>,
) -> String {
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
    let page = page.unwrap_or(1).max(1);
    let page_val: String;
    if page > 1 {
        page_val = page.to_string();
        params.push(("pageno", &page_val));
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
/// 1. Primary: extract `<a class="result__a" href="…">Title</a>` anchors.
///    The match is anchored to a real `<a` tag opening — `result__a` alone
///    also appears inside `<style>`/`<script>` blocks (CSS selectors), which
///    must never be parsed as results.
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

/// Find the next `<a` tag whose class attribute contains `result__a`.
///
/// Scans forward from `from`, returning the byte index of the tag opening.
/// Skips matches of the bare string `result__a` that are not inside an
/// `<a …>` class attribute (e.g. CSS selectors in `<style>` blocks).
fn find_result_anchor(html: &str, from: usize) -> Option<usize> {
    let bytes = html.as_bytes();
    let mut i = from;
    while let Some(rel) = html[i..].find("result__a") {
        let pos = i + rel;
        // Walk back to the nearest '<a ' opening within this tag.
        let mut start = pos;
        let tag_open_found = loop {
            if bytes[start] == b'<' {
                break if start + 1 < bytes.len()
                    && (bytes[start + 1] == b'a' || bytes[start + 1] == b'A')
                {
                    Some(start)
                } else {
                    None
                };
            }
            if start == 0 || pos - start > 200 {
                break None;
            }
            start -= 1;
        };
        if tag_open_found.is_some() {
            return Some(start);
        }
        i = pos + "result__a".len();
        if i >= bytes.len() {
            return None;
        }
    }
    None
}

fn parse_ddg_primary(html: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut from = 0usize;
    while let Some(tag_start) = find_result_anchor(html, from) {
        let after = &html[tag_start..];
        let (href_start, quote_char) = if let Some(h) = after.find("href=\"") {
            (tag_start + h + 6, '"')
        } else if let Some(h) = after.find("href='") {
            (tag_start + h + 6, '\'')
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
        from = title_end + 4;
        if lines.len() >= MAX_RESULTS {
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
        if lines.len() >= MAX_RESULTS {
            break;
        }
    }
    lines
}

/// Percent-decode a URL component (`+` as space, `%XX` hex escapes).
///
/// Byte-correct: decoded bytes are collected and interpreted as UTF-8 in one
/// pass (multi-byte sequences like `%E4%B8%AD` → one CJK character, not three
/// replacement chars). Invalid UTF-8 decodes lossily via `from_utf8_lossy`.
fn urlencoding_percent_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let src = s.as_bytes();
    let mut i = 0;
    while i < src.len() {
        match src[i] {
            b'%' if i + 2 < src.len() => {
                // Parse hex from bytes, not `&s[i+1..i+3]` — that slice panics
                // when the two bytes after `%` split a multi-byte UTF-8 char
                // (e.g. `%aé`).
                let hex = [src[i + 1], src[i + 2]];
                match std::str::from_utf8(&hex)
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        bytes.push(v);
                        i += 3;
                    }
                    None => {
                        bytes.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                bytes.push(b' ');
                i += 1;
            }
            b => {
                bytes.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
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

    // ── Challenge detection + parser anchoring ─────────────────────────

    #[test]
    fn ddg_challenge_page_is_detected() {
        // Shape of the real anomaly page: challenge form, no result anchors.
        let html = r#"<html><body><form id="challenge-form" action="//duckduckgo.com/anomaly.js?sv=html&cc=botnet" method="POST"></form></body></html>"#;
        assert!(is_ddg_challenge(html));
    }

    #[test]
    fn ddg_results_page_is_not_a_challenge() {
        let html = r#"<a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com">Result</a>"#;
        assert!(!is_ddg_challenge(html));
    }

    #[test]
    fn parse_ddg_ignores_css_selector_hits() {
        // `result__a` inside a <style> block (CSS selector) must never be
        // parsed as a result — the anchor must be a real <a …> tag.
        let html = r#"<html><head><style>.result__a { color: blue; } ader__logo-wrap { x }</style></head><body><p>No results here.</p></body></html>"#;
        let out = parse_ddg_results(html);
        assert_eq!(out, "No results found.");
        assert!(!out.contains("ader__logo-wrap"));
    }

    #[test]
    fn parse_ddg_prefers_anchor_over_css_junk() {
        // A page with BOTH a CSS selector hit and a real anchor: only the
        // real anchor becomes a result.
        let html = r#"<style>.result__a:hover { color: red; }</style><a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Freal.com">Real Result</a>"#;
        let out = parse_ddg_results(html);
        assert!(out.contains("Real Result — https://real.com"), "got: {out}");
        assert_eq!(out.lines().count(), 1);
    }

    #[test]
    fn find_result_anchor_requires_a_tag() {
        let html =
            r#"<div class="result__a">not a link</div><a class="result__a" href="x">yes</a>"#;
        let anchor = find_result_anchor(html, 0);
        assert!(anchor.is_some(), "the real <a> tag must be found");
        let out = parse_ddg_results(html);
        assert!(out.contains("yes"), "got: {out}");
    }

    // ── Entity decoding ────────────────────────────────────────────────

    #[test]
    fn html_to_text_decodes_entities() {
        assert_eq!(decode_html_entities("a &amp; b"), "a & b");
        assert_eq!(decode_html_entities("&lt;tag&gt;"), "<tag>");
        assert_eq!(decode_html_entities("caf&eacute;"), "café");
        assert_eq!(decode_html_entities("&#65;&#x42;"), "AB");
        assert_eq!(decode_html_entities("&nbsp;"), " ");
        assert_eq!(decode_html_entities("m&#8212;d"), "m—d");
    }

    #[test]
    fn html_to_text_leaves_unknown_entities() {
        assert_eq!(decode_html_entities("&unknownentity;"), "&unknownentity;");
    }

    #[test]
    fn html_to_text_strips_comments() {
        let html = "before<!-- hidden comment -->after";
        let out = html_to_text(html);
        assert!(out.contains("before"));
        assert!(out.contains("after"));
        assert!(!out.contains("hidden"));
    }

    // ── UTF-8-safe percent decoding ────────────────────────────────────

    #[test]
    fn percent_decode_multibyte_utf8() {
        // 中 is U+4E2D → UTF-8 bytes E4 B8 AD. The old byte-as-char decoder
        // produced three U+FFFD replacement chars; it must decode to one char.
        assert_eq!(urlencoding_percent_decode("%E4%B8%AD"), "中");
    }

    #[test]
    fn percent_decode_mixed_query() {
        assert_eq!(
            urlencoding_percent_decode("r%C3%A9sum%C3%A9+rust"),
            "résumé rust"
        );
    }

    #[test]
    fn percent_decode_invalid_utf8_is_lossy_not_panicking() {
        // Lone continuation byte is invalid UTF-8 — lossy decode, not panic.
        let out = urlencoding_percent_decode("%80");
        assert!(!out.is_empty());
    }

    #[test]
    fn percent_decode_keeps_literal_percent_without_hex() {
        assert_eq!(urlencoding_percent_decode("100% done"), "100% done");
    }

    #[test]
    fn percent_decode_percent_before_multibyte_does_not_panic() {
        // `%` + `a` + `é` (2-byte UTF-8): slicing `&s[i+1..i+3]` splits é.
        let out = urlencoding_percent_decode("%aé");
        assert!(out.contains('é'), "got {out:?}");
        assert!(out.contains('%') || out.contains('a'));
    }
}
