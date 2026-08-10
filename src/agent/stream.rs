//! Streaming and non-streaming response processing.
//!
//! Both paths accumulate assistant text deltas and tool calls from an
//! OpenAI-compatible `/chat/completions` response, normalizing the tool-call
//! `arguments` field to a string via [`args_to_string`].
//!
//! The core parsing lives in [`process_stream_text`] and
//! [`process_non_stream_json`], which operate on a raw SSE body string and a
//! parsed JSON value respectively. The HTTP-facing [`Agent::process_stream`]
//! and [`Agent::process_non_stream`] read a `reqwest::Response` and delegate
//! to those, so the same parsing is exercised by the offline fake-model tests.

use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use super::core::Agent;
use super::types::AgentEvent;

/// Parsed completion payload shared by stream and non-stream paths.
#[derive(Debug, Default)]
pub(crate) struct ParsedCompletion {
    pub content: String,
    pub tool_acc: BTreeMap<u32, (String, String, String)>,
    /// Last observed `finish_reason` (`stop`, `tool_calls`, `length`, …).
    pub finish_reason: Option<String>,
    /// Provider-level error message extracted from the body (if any).
    pub error: Option<String>,
}

impl Agent {
    /// Process a streaming SSE response, accumulating content and tool calls.
    ///
    /// Bytes are buffered first so multi-byte UTF-8 sequences split across
    /// TCP chunks are never lossy-decoded mid-character.
    pub(crate) async fn process_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> ParsedCompletion {
        let mut stream = resp.bytes_stream();
        let mut raw = Vec::new();
        let mut stream_err: Option<String> = None;
        while let Some(item) = stream.next().await {
            match item {
                Ok(c) => raw.extend_from_slice(&c),
                Err(e) => {
                    let msg = format!("Stream error: {e}");
                    let _ = tx.send(AgentEvent::Error(msg.clone())).await;
                    stream_err = Some(msg);
                    break;
                }
            }
        }
        let body = String::from_utf8_lossy(&raw);
        let mut parsed = process_stream_text(&body, tx).await;
        if parsed.error.is_none() {
            if let Some(e) = stream_err {
                parsed.error = Some(e);
            }
        }
        parsed
    }

    /// Process a non-streaming JSON response (fallback for weak SSE hosts).
    pub(crate) async fn process_non_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> ParsedCompletion {
        let Ok(v) = resp.json::<Value>().await else {
            return ParsedCompletion {
                error: Some("Failed to parse non-streaming JSON response".into()),
                ..Default::default()
            };
        };
        process_non_stream_json(&v, tx).await
    }
}

/// Strip an SSE `data:` line prefix (optional space after the colon).
fn sse_data_payload(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("data:")?;
    Some(rest.strip_prefix(' ').unwrap_or(rest))
}

fn extract_api_error(v: &Value) -> Option<String> {
    let err = v.get("error")?;
    if let Some(s) = err.as_str() {
        let t = s.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    if let Some(msg) = err.get("message").and_then(|m| m.as_str()) {
        let t = msg.trim();
        if !t.is_empty() {
            return Some(t.to_string());
        }
    }
    Some(err.to_string())
}

/// Parse a full SSE body string, accumulating content and tool calls.
pub(crate) async fn process_stream_text(
    body: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> ParsedCompletion {
    let mut content_buf = String::new();
    let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut finish_reason: Option<String> = None;
    let mut error: Option<String> = None;

    for line in body.lines() {
        let line = line.trim();
        let Some(data) = sse_data_payload(line) else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        if let Some(err) = extract_api_error(&v) {
            error = Some(err);
            continue;
        }
        let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        let Some(choice) = choices.first() else {
            continue;
        };
        // Streaming path: await TextDelta so slow consumers still get every chunk.
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if fr != "null" && !fr.is_empty() {
                finish_reason = Some(fr.to_string());
            }
        }
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            content_buf.push_str(c);
            let _ = tx.send(AgentEvent::TextDelta(c.to_string())).await;
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                let entry = tool_acc
                    .entry(idx)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                    if !id.is_empty() {
                        entry.0 = id.to_string();
                    }
                }
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                        if !name.is_empty() {
                            entry.1 = name.to_string();
                        }
                    }
                    if let Some(args) = func.get("arguments") {
                        entry.2.push_str(&args_to_string(args));
                    }
                }
            }
        }
    }

    if let Some(ref fr) = finish_reason {
        if fr == "content_filter" && error.is_none() {
            error = Some("Completion blocked by content filter".into());
        }
    }

    ParsedCompletion {
        content: content_buf,
        tool_acc,
        finish_reason,
        error,
    }
}

/// Parse a non-streaming JSON response value, accumulating content and tool
/// calls.
pub(crate) async fn process_non_stream_json(
    v: &Value,
    tx: &mpsc::Sender<AgentEvent>,
) -> ParsedCompletion {
    let mut content_buf = String::new();
    let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();
    let mut finish_reason: Option<String> = None;

    if let Some(err) = extract_api_error(v) {
        return ParsedCompletion {
            error: Some(err),
            ..Default::default()
        };
    }

    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return ParsedCompletion::default();
    };
    let Some(choice) = choices.first() else {
        return ParsedCompletion::default();
    };

    if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        if fr != "null" && !fr.is_empty() {
            finish_reason = Some(fr.to_string());
        }
    }

    // Non-streaming uses "message" instead of "delta"
    let msg = choice.get("message").cloned().unwrap_or(json!({}));

    if let Some(c) = msg.get("content").and_then(|c| c.as_str()) {
        content_buf.push_str(c);
        let _ = tx.send(AgentEvent::TextDelta(c.to_string())).await;
    }

    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for (i, tc) in tcs.iter().enumerate() {
            let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(i as u64) as u32;
            let id = tc
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            let func = tc.get("function").cloned().unwrap_or(json!({}));
            let name = func
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = func
                .get("arguments")
                .map(args_to_string)
                .unwrap_or_default();
            tool_acc.insert(idx, (id, name, args));
        }
    }

    let mut error = None;
    if finish_reason.as_deref() == Some("content_filter") {
        error = Some("Completion blocked by content filter".into());
    }

    ParsedCompletion {
        content: content_buf,
        tool_acc,
        finish_reason,
        error,
    }
}

/// Convert a tool-call `arguments` JSON value into the string form the
/// dispatch layer expects.
///
/// OpenAI-compatible endpoints vary: most stream `arguments` as a JSON *string*
/// fragment (accumulated across chunks), but some return a fully-formed JSON
/// *object*. Normalize both to a string so tool arguments are never silently
/// dropped (which would produce a malformed call with empty args). A JSON
/// `null`/missing value becomes an empty string.
pub(crate) fn args_to_string(args: &Value) -> String {
    match args {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn sse_data_without_space_is_accepted() {
        let (tx, mut rx) = mpsc::channel(8);
        let body = "data:{\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata:[DONE]\n\n";
        let parsed = process_stream_text(body, &tx).await;
        assert_eq!(parsed.content, "hi");
        assert!(parsed.error.is_none());
        drop(tx);
        let _ = rx.recv().await;
    }

    #[tokio::test]
    async fn sse_error_event_is_captured() {
        let (tx, _rx) = mpsc::channel(8);
        let body = r#"data: {"error":{"message":"Rate limit exceeded","code":429}}

data: [DONE]

"#;
        let parsed = process_stream_text(body, &tx).await;
        assert!(parsed.error.as_deref().unwrap_or("").contains("Rate limit"));
        assert!(parsed.content.is_empty());
        assert!(parsed.tool_acc.is_empty());
    }

    #[tokio::test]
    async fn finish_reason_content_filter_becomes_error() {
        let (tx, _rx) = mpsc::channel(8);
        let body = r#"data: {"choices":[{"delta":{},"finish_reason":"content_filter"}]}

data: [DONE]

"#;
        let parsed = process_stream_text(body, &tx).await;
        assert_eq!(parsed.finish_reason.as_deref(), Some("content_filter"));
        assert!(parsed.error.is_some());
    }

    #[tokio::test]
    async fn non_stream_error_object() {
        let (tx, _rx) = mpsc::channel(8);
        let v = json!({"error": {"message": "Invalid API key"}});
        let parsed = process_non_stream_json(&v, &tx).await;
        assert!(parsed.error.unwrap().contains("Invalid API key"));
    }

    #[test]
    fn args_to_string_object_and_null() {
        assert_eq!(args_to_string(&json!({"a": 1})), r#"{"a":1}"#);
        assert_eq!(args_to_string(&Value::Null), "");
        assert_eq!(args_to_string(&json!("{\"a\":1}")), r#"{"a":1}"#);
    }
}
