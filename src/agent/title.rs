//! Cheap session-title generation and detection.
//!
//! Clients (and some wrappers) send a "name this session" prompt as a full
//! user turn. Running that through the normal tool-using loop spends thousands
//! of tokens on the system prompt, repo map, and tool schema for an 8-token
//! title, and `save_all_messages` then replaces the real conversation with
//! those three lines. Title work is a tiny toolless completion instead.

use serde_json::json;

use crate::config::Settings;

const TITLE_SYSTEM: &str = "You name coding sessions. Reply with ONLY a concise 3-5 word title \
     in Title Case. No quotes, no punctuation, no extra text.";

/// Detect a client-generated "name this session" prompt.
pub fn is_title_prompt(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() || t.len() > 8_000 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    let asks_for_title = lower.contains("title")
        && (lower.contains("3-5 word") || lower.contains("3–5 word") || lower.contains("concise"));
    let reply_only = lower.starts_with("reply with only")
        || lower.starts_with("reply with")
        || lower.contains("only a concise");
    asks_for_title && reply_only
}

/// The user request buried inside a title-generation wrapper, if any.
pub fn extract_title_source(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    for marker in [
        "begins with this request:",
        "starts with this request:",
        "session that begins with this request:",
    ] {
        if let Some(idx) = lower.find(marker) {
            let rest = text[idx + marker.len()..].trim();
            if !rest.is_empty() {
                return rest.chars().take(800).collect();
            }
        }
    }
    text.trim().chars().take(800).collect()
}

/// Collapse a model reply into a short session title.
pub fn sanitize_title(raw: &str) -> String {
    let line = raw
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line.trim_matches(|c| c == '"' || c == '\'' || c == '`');
    let words: Vec<&str> = line.split_whitespace().take(6).collect();
    let joined = words.join(" ");
    let trimmed: String = joined
        .trim_end_matches(|c: char| c.is_ascii_punctuation() && c != '+')
        .chars()
        .take(80)
        .collect();
    trimmed.trim().to_string()
}

/// Fallback when the title request fails or returns empty.
pub fn fallback_title(text: &str) -> String {
    let src = extract_title_source(text);
    let words: Vec<String> = src
        .split_whitespace()
        .filter(|w| w.chars().any(|c| c.is_alphanumeric()))
        .take(5)
        .map(title_case_word)
        .collect();
    if words.is_empty() {
        "New Session".into()
    } else {
        words.join(" ")
    }
}

fn title_case_word(w: &str) -> String {
    let mut chars = w.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
    }
}

/// JSON body for a toolless title completion (tiny system prompt, no tools).
pub fn title_request_body(model: &str, user_text: &str, stream: bool) -> serde_json::Value {
    let source = extract_title_source(user_text);
    json!({
        "model": model,
        "messages": [
            {"role": "system", "content": TITLE_SYSTEM},
            {"role": "user", "content": source},
        ],
        "max_tokens": 24,
        "temperature": 0.0,
        "stream": stream,
    })
}

/// Fire-and-forget title completion used by the TUI/ACP on the first real
/// user prompt. Never constructs an [`crate::agent::Agent`] (no repo map,
/// no tools, no session messages). Fail-open: `None` on any error.
pub async fn generate_session_title(settings: &Settings, user_text: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()?;
    let url = format!(
        "{}/chat/completions",
        settings.base_url().trim_end_matches('/')
    );
    let body = title_request_body(&settings.model, user_text, false);
    let mut req = client.post(&url).json(&body);
    if let Some(key) = settings.api_key() {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let text = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let title = sanitize_title(text);
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_grok_style_title_prompt() {
        let prompt = "Reply with ONLY a concise 3-5 word title in Title Case \
             (no quotes, no punctuation) for a coding session that begins \
             with this request:\n\ncan we inspect this project";
        assert!(is_title_prompt(prompt));
        assert_eq!(extract_title_source(prompt), "can we inspect this project");
    }

    #[test]
    fn real_tasks_are_not_title_prompts() {
        assert!(!is_title_prompt(
            "can we inspect this project and work on optimizing it for massive files?"
        ));
        assert!(!is_title_prompt(
            "add a session title field to summary.json"
        ));
        assert!(!is_title_prompt(""));
    }

    #[test]
    fn sanitize_strips_quotes_and_punctuation() {
        assert_eq!(
            sanitize_title("\"Optimizing Editor for Large Files.\""),
            "Optimizing Editor for Large Files"
        );
        assert_eq!(sanitize_title("  \nTitle Here\nignore me"), "Title Here");
    }

    #[test]
    fn fallback_title_takes_leading_words() {
        assert_eq!(
            fallback_title("optimize the editor for huge files please"),
            "Optimize The Editor For Huge"
        );
    }

    #[test]
    fn title_request_has_no_tools_and_tiny_system() {
        let body = title_request_body("m", "fix the parser", false);
        assert!(body.get("tools").is_none());
        assert_eq!(body["max_tokens"], 24);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0]["content"].as_str().unwrap().contains("3-5 word"));
        assert!(!msgs[0]["content"].as_str().unwrap().contains("repo_map"));
    }
}
