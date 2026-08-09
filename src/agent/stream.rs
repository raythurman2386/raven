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

impl Agent {
    /// Process a streaming SSE response, accumulating content and tool calls.
    ///
    /// Returns (accumulated_content, accumulated_tool_calls).
    pub(crate) async fn process_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> (String, BTreeMap<u32, (String, String, String)>) {
        let mut stream = resp.bytes_stream();
        let mut body = String::new();
        while let Some(item) = stream.next().await {
            match item {
                Ok(c) => body.push_str(&String::from_utf8_lossy(&c)),
                Err(e) => {
                    let _ = tx
                        .send(AgentEvent::Error(format!("Stream error: {}", e)))
                        .await;
                    break;
                }
            }
        }
        process_stream_text(&body, tx).await
    }

    /// Process a non-streaming JSON response (fallback for weak SSE hosts).
    ///
    /// Returns (accumulated_content, accumulated_tool_calls).
    pub(crate) async fn process_non_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> (String, BTreeMap<u32, (String, String, String)>) {
        let Ok(v) = resp.json::<Value>().await else {
            return (String::new(), BTreeMap::new());
        };
        process_non_stream_json(&v, tx).await
    }
}

/// Parse a full SSE body string, accumulating content and tool calls.
///
/// Returns (accumulated_content, accumulated_tool_calls). Emits
/// [`AgentEvent::TextDelta`] for each content chunk.
pub(crate) async fn process_stream_text(
    body: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> (String, BTreeMap<u32, (String, String, String)>) {
    let mut content_buf = String::new();
    let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();

    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with("data: ") {
            continue;
        }
        let data = &line[6..];
        if data == "[DONE]" {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            continue;
        };
        let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
            continue;
        };
        let Some(choice) = choices.first() else {
            continue;
        };
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

    (content_buf, tool_acc)
}

/// Parse a non-streaming JSON response value, accumulating content and tool
/// calls.
///
/// Returns (accumulated_content, accumulated_tool_calls). Emits
/// [`AgentEvent::TextDelta`] for the content.
pub(crate) async fn process_non_stream_json(
    v: &Value,
    tx: &mpsc::Sender<AgentEvent>,
) -> (String, BTreeMap<u32, (String, String, String)>) {
    let mut content_buf = String::new();
    let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();

    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return (content_buf, tool_acc);
    };
    let Some(choice) = choices.first() else {
        return (content_buf, tool_acc);
    };

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

    (content_buf, tool_acc)
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
