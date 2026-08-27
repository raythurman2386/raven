//! Streaming and non-streaming response processing.
//!
//! Both paths accumulate assistant text deltas and tool calls from an
//! OpenAI-compatible `/chat/completions` response, normalizing the tool-call
//! `arguments` field to a string via [`args_to_string`].
//!
//! Live HTTP streaming goes through [`StreamAccumulator`]: bytes are fed via
//! [`push_chunk`] (newline-delimited), each complete line is parsed, and
//! `TextDelta` events are emitted as tokens arrive. [`Agent::process_stream`]
//! is the HTTP entry point; [`process_stream_text`] is a test-only helper that
//! feeds a full SSE body line-by-line. Non-streaming responses use
//! [`process_non_stream_json`].

use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use super::core::Agent;
use super::types::AgentEvent;
use crate::tokenizer::TokenUsage;

/// Parsed completion payload shared by stream and non-stream paths.
#[derive(Debug, Default)]
pub(crate) struct ParsedCompletion {
    pub content: String,
    pub tool_acc: BTreeMap<u32, (String, String, String)>,
    /// Last observed `finish_reason` (`stop`, `tool_calls`, `length`, …).
    pub finish_reason: Option<String>,
    /// Provider-level error message extracted from the body (if any).
    pub error: Option<String>,
    /// Real token usage reported by the provider, when the response carries a
    /// `usage` object (non-streaming responses, or the final streaming chunk
    /// when `stream_options.include_usage` was requested). `None` when the
    /// provider omitted it — callers fall back to the estimator.
    pub usage: Option<TokenUsage>,
}

/// Feed raw bytes into an SSE line buffer, parsing complete lines as `\n`
/// boundaries are reached. Multi-byte UTF-8 is only decoded at line boundaries,
/// so a character split across TCP chunks is never lossy-decoded mid-sequence.
pub(crate) async fn push_chunk(
    line_buf: &mut Vec<u8>,
    chunk: &[u8],
    acc: &mut StreamAccumulator,
    tx: &mpsc::Sender<AgentEvent>,
) {
    for &b in chunk {
        if b == b'\n' {
            let line = String::from_utf8_lossy(line_buf);
            acc.feed_line(&line, tx).await;
            line_buf.clear();
        } else {
            line_buf.push(b);
        }
    }
}

/// Flush a trailing partial line (no final newline) into the accumulator.
pub(crate) async fn flush_line_buf(
    line_buf: &mut Vec<u8>,
    acc: &mut StreamAccumulator,
    tx: &mpsc::Sender<AgentEvent>,
) {
    if !line_buf.is_empty() {
        let line = String::from_utf8_lossy(line_buf);
        acc.feed_line(&line, tx).await;
        line_buf.clear();
    }
}

impl Agent {
    /// Process a streaming SSE response, accumulating content and tool calls.
    ///
    /// Lines are parsed and emitted **incrementally** as bytes arrive, so the
    /// consumer sees `TextDelta` events token-by-token rather than a single
    /// burst at the end. Bytes are buffered only up to the next newline so a
    /// multi-byte UTF-8 sequence split across TCP chunks is never lossy-decoded
    /// mid-character.
    pub(crate) async fn process_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> ParsedCompletion {
        let mut stream = resp.bytes_stream();
        let mut acc = StreamAccumulator::default();
        let mut line_buf: Vec<u8> = Vec::new();
        let mut stream_err: Option<String> = None;

        while let Some(item) = stream.next().await {
            match item {
                Ok(c) => push_chunk(&mut line_buf, &c, &mut acc, tx).await,
                Err(e) => {
                    let msg = format!("Stream error: {e}");
                    let _ = tx.send(AgentEvent::Error(msg.clone())).await;
                    stream_err = Some(msg);
                    break;
                }
            }
        }
        flush_line_buf(&mut line_buf, &mut acc, tx).await;

        let mut parsed = acc.finish();
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

/// Incremental SSE accumulator. Feed one line at a time via [`feed_line`];
/// call [`finish`] to get the final result. Emits `TextDelta` events as each
/// line is parsed, so streaming consumers see tokens as they arrive.
///
/// [`feed_line`]: StreamAccumulator::feed_line
/// [`finish`]: StreamAccumulator::finish
#[derive(Default)]
pub(crate) struct StreamAccumulator {
    content_buf: String,
    tool_acc: BTreeMap<u32, (String, String, String)>,
    finish_reason: Option<String>,
    error: Option<String>,
    usage: Option<TokenUsage>,
}

impl StreamAccumulator {
    /// Parse a single SSE line and accumulate its content/tool-call deltas.
    /// Emits a `TextDelta` event for any content chunk in this line.
    pub(crate) async fn feed_line(&mut self, line: &str, tx: &mpsc::Sender<AgentEvent>) {
        let line = line.trim();
        let Some(data) = sse_data_payload(line) else {
            return;
        };
        if data == "[DONE]" {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        if let Some(err) = extract_api_error(&v) {
            self.error = Some(err);
            return;
        }
        // Usage arrives on a final chunk with an EMPTY `choices` array (the
        // OpenAI `stream_options.include_usage` contract), so it must be
        // parsed before the `choices` early-return below — otherwise the
        // calibration sample is silently dropped.
        if let Some(u) = TokenUsage::from_json(&v) {
            self.usage = Some(u);
        }
        let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
            return;
        };
        let Some(choice) = choices.first() else {
            return;
        };
        if let Some(fr) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            if fr != "null" && !fr.is_empty() {
                self.finish_reason = Some(fr.to_string());
            }
        }
        let delta = choice.get("delta").cloned().unwrap_or(json!({}));
        if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
            self.content_buf.push_str(c);
            let _ = tx.send(AgentEvent::TextDelta(c.to_string())).await;
        }
        if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
            for tc in tcs {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                let entry = self
                    .tool_acc
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

    /// Finalize the accumulated stream into a [`ParsedCompletion`].
    pub(crate) fn finish(mut self) -> ParsedCompletion {
        if let Some(ref fr) = self.finish_reason {
            if fr == "content_filter" && self.error.is_none() {
                self.error = Some("Completion blocked by content filter".into());
            }
        }
        ParsedCompletion {
            content: self.content_buf,
            tool_acc: self.tool_acc,
            finish_reason: self.finish_reason,
            error: self.error,
            usage: self.usage,
        }
    }
}

/// Parse a full SSE body string, accumulating content and tool calls.
///
/// Only used by the offline fake-model test path and unit tests; the live
/// HTTP path streams line-by-line via [`StreamAccumulator`].
#[cfg(test)]
pub(crate) async fn process_stream_text(
    body: &str,
    tx: &mpsc::Sender<AgentEvent>,
) -> ParsedCompletion {
    let mut acc = StreamAccumulator::default();
    for line in body.lines() {
        acc.feed_line(line, tx).await;
    }
    acc.finish()
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

    // Usage is read before the `choices` early-returns so a response that
    // carries `usage` but an unusual/empty `choices` still yields its sample.
    let usage = TokenUsage::from_json(v);

    let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
        return ParsedCompletion {
            usage,
            ..Default::default()
        };
    };
    let Some(choice) = choices.first() else {
        return ParsedCompletion {
            usage,
            ..Default::default()
        };
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
        usage,
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

    #[tokio::test]
    async fn stream_accumulator_emits_deltas_incrementally() {
        // Regression: streaming must emit TextDelta events as each line is fed,
        // not buffer the whole body and burst at the end.
        let (tx, mut rx) = mpsc::channel(8);
        let mut acc = StreamAccumulator::default();

        acc.feed_line(r#"data: {"choices":[{"delta":{"content":"Hel"}}]}"#, &tx)
            .await;
        // First delta should be immediately available.
        let first = rx.try_recv().expect("first delta emitted immediately");
        match first {
            AgentEvent::TextDelta(t) => assert_eq!(t, "Hel"),
            _ => panic!("expected TextDelta, got a different event"),
        }

        acc.feed_line(r#"data: {"choices":[{"delta":{"content":"lo"}}]}"#, &tx)
            .await;
        let second = rx.try_recv().expect("second delta emitted immediately");
        match second {
            AgentEvent::TextDelta(t) => assert_eq!(t, "lo"),
            _ => panic!("expected TextDelta, got a different event"),
        }

        let parsed = acc.finish();
        assert_eq!(parsed.content, "Hello");
        assert!(rx.try_recv().is_err(), "no more events after finish");
    }

    #[tokio::test]
    async fn push_chunk_splits_one_sse_line_across_byte_chunks() {
        // Regression: one complete `data:` line arriving as multiple TCP
        // chunks must parse once the final `\n` arrives, and TextDelta must
        // fire then — not only after the whole body is buffered.
        let (tx, mut rx) = mpsc::channel(8);
        let mut acc = StreamAccumulator::default();
        let mut line_buf = Vec::new();

        let full = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hi\"}}]}\n";
        // Split mid-payload (after "data: {\"cho").
        let (a, rest) = full.split_at(12);
        let (b, c) = rest.split_at(10);
        push_chunk(&mut line_buf, a, &mut acc, &tx).await;
        assert!(rx.try_recv().is_err(), "no event before newline");
        push_chunk(&mut line_buf, b, &mut acc, &tx).await;
        assert!(rx.try_recv().is_err(), "still no event before newline");
        push_chunk(&mut line_buf, c, &mut acc, &tx).await;

        let ev = rx.try_recv().expect("delta after complete line");
        match ev {
            AgentEvent::TextDelta(t) => assert_eq!(t, "Hi"),
            _ => panic!("expected TextDelta"),
        }
        let parsed = acc.finish();
        assert_eq!(parsed.content, "Hi");
        assert!(line_buf.is_empty());
    }

    #[tokio::test]
    async fn push_chunk_preserves_utf8_split_across_chunks() {
        // "é" is 0xC3 0xA9. Split those two bytes across chunk boundaries
        // inside one SSE line; decoding must wait until the line completes.
        let (tx, mut rx) = mpsc::channel(8);
        let mut acc = StreamAccumulator::default();
        let mut line_buf = Vec::new();

        // Build: data: {"choices":[{"delta":{"content":"é"}}]}\n
        // with the two bytes of é in separate chunks.
        let prefix = br#"data: {"choices":[{"delta":{"content":""#;
        let suffix = br#""}}]}"#;
        push_chunk(&mut line_buf, prefix, &mut acc, &tx).await;
        push_chunk(&mut line_buf, &[0xC3], &mut acc, &tx).await; // first byte of é
        push_chunk(&mut line_buf, &[0xA9], &mut acc, &tx).await; // second byte
        push_chunk(&mut line_buf, suffix, &mut acc, &tx).await;
        assert!(rx.try_recv().is_err(), "no event before newline");
        push_chunk(&mut line_buf, b"\n", &mut acc, &tx).await;

        let ev = rx.try_recv().expect("delta after complete line");
        match ev {
            AgentEvent::TextDelta(t) => assert_eq!(t, "é"),
            _ => panic!("expected TextDelta"),
        }
        let parsed = acc.finish();
        assert_eq!(parsed.content, "é");
    }

    #[tokio::test]
    async fn flush_line_buf_emits_trailing_line_without_newline() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut acc = StreamAccumulator::default();
        let mut line_buf = Vec::new();
        push_chunk(
            &mut line_buf,
            br#"data: {"choices":[{"delta":{"content":"end"}}]}"#,
            &mut acc,
            &tx,
        )
        .await;
        assert!(rx.try_recv().is_err());
        flush_line_buf(&mut line_buf, &mut acc, &tx).await;
        let ev = rx.try_recv().expect("flushed trailing line");
        match ev {
            AgentEvent::TextDelta(t) => assert_eq!(t, "end"),
            _ => panic!("expected TextDelta"),
        }
        assert_eq!(acc.finish().content, "end");
    }

    #[test]
    fn args_to_string_object_and_null() {
        assert_eq!(args_to_string(&json!({"a": 1})), r#"{"a":1}"#);
        assert_eq!(args_to_string(&Value::Null), "");
        assert_eq!(args_to_string(&json!("{\"a\":1}")), r#"{"a":1}"#);
    }

    // ── usage parsing ───────────────────────────────────────────────────

    #[tokio::test]
    async fn sse_usage_chunk_with_empty_choices_is_parsed() {
        // The OpenAI `stream_options.include_usage` contract: the final chunk
        // carries `usage` and an EMPTY `choices` array. The accumulator must
        // capture the usage before its `choices` early-return.
        let (tx, _rx) = mpsc::channel(8);
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":321,\"completion_tokens\":9,\"total_tokens\":330}}\n\n",
            "data: [DONE]\n\n",
        );
        let parsed = process_stream_text(body, &tx).await;
        assert_eq!(parsed.content, "hi");
        let u = parsed.usage.expect("usage chunk must be captured");
        assert_eq!(u.prompt_tokens, 321);
        assert_eq!(u.completion_tokens, 9);
        assert_eq!(u.total_tokens, 330);
    }

    #[tokio::test]
    async fn sse_usage_absent_leaves_none() {
        let (tx, _rx) = mpsc::channel(8);
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let parsed = process_stream_text(body, &tx).await;
        assert!(parsed.usage.is_none(), "no usage → None (fallback path)");
    }

    #[tokio::test]
    async fn non_stream_usage_is_parsed() {
        let (tx, _rx) = mpsc::channel(8);
        let v = json!({
            "choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 500, "completion_tokens": 10, "total_tokens": 510}
        });
        let parsed = process_non_stream_json(&v, &tx).await;
        let u = parsed.usage.expect("non-streaming usage should parse");
        assert_eq!(u.prompt_tokens, 500);
        assert_eq!(u.total_tokens, 510);
    }

    #[tokio::test]
    async fn non_stream_without_choices_still_yields_usage() {
        // Degenerate response: usage present, choices missing entirely. The
        // usage must survive the early-return.
        let (tx, _rx) = mpsc::channel(8);
        let v = json!({
            "usage": {"prompt_tokens": 77, "completion_tokens": 3, "total_tokens": 80}
        });
        let parsed = process_non_stream_json(&v, &tx).await;
        assert!(parsed.content.is_empty());
        let u = parsed.usage.expect("usage must survive missing choices");
        assert_eq!(u.prompt_tokens, 77);
    }
}
