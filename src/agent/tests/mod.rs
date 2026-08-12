//! Unit + integration tests for the agent module.
//!
//! These exercise the streaming loop, tool dispatch, stall/verify recovery,
//! compaction, and parallel sub-agents against a mock HTTP server.

mod eval_suite;
mod fake_model;

use super::core::Agent;
use super::loop_control::compute_reminders;
use super::stream::args_to_string;
use super::types::{AgentEvent, ChatMessage, FunctionCall, ToolCall};
use crate::config::Mode;
use serde_json::{json, Value};
use std::net::TcpListener as StdTcpListener;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Read the raw HTTP request headers (everything up to and including the
/// `\r\n\r\n` header terminator), waiting until the full body (per
/// Content-Length) has arrived. Returns the header block as a String
/// (headers only, NOT including body).
#[allow(dead_code)]
async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await.expect("read from stream");
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        // Check for header terminator.
        if let Some(header_end) = find_subslice(b"\r\n\r\n", &buf) {
            let headers = String::from_utf8_lossy(&buf[..header_end + 4]).to_string();
            // Wait for full body per Content-Length so the client can send it.
            let content_length = extract_content_length(&headers).unwrap_or(0);
            let body_start = header_end + 4;
            while buf.len() < body_start + content_length {
                let n = stream.read(&mut tmp).await.expect("read body from stream");
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            return headers;
        }
    }
    String::from_utf8_lossy(&buf).to_string()
}

fn find_subslice(needle: &[u8], haystack: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn extract_content_length(headers: &str) -> Option<usize> {
    for line in headers.split("\r\n") {
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("content-length") {
                return value.trim().parse::<usize>().ok();
            }
        }
    }
    None
}

/// Serve the given responses (status, reason, body) over the listener,
/// keep-alive aware.
///
/// The agent uses a shared `reqwest::Client` that reuses one TCP
/// connection across requests. A naive one-connection-per-response mock
/// races: after the mock writes a response and drops the stream, the
/// client may already have sent its next request on that same connection,
/// where it sits unread while the mock blocks on `accept()` for a new
/// connection that never comes — hanging the agent until it times out and
/// ends the run with `Error` instead of `Done`. To avoid that, read
/// multiple requests per connection, serving the next scripted response
/// each time, until the connection closes. Once the scripted responses are
/// exhausted, serve a benign empty fallback so an extra request (a verify
/// retry, a timing shift) never hits a connection-refused → retry →
/// `OllamaUnreachable` path.
async fn serve_mock(
    listener: &tokio::net::TcpListener,
    responses: Vec<(u16, &'static str, &'static str)>,
) {
    let mut next = 0usize;
    loop {
        let (mut stream, _) = listener.accept().await.expect("accept");
        loop {
            // Read one request. If the connection closed (client done),
            // break to accept the next connection.
            if read_http_request(&mut stream).await.is_empty() {
                break;
            }
            let (status, reason, body) = match responses.get(next) {
                Some(&(s, r, b)) => {
                    next += 1;
                    (s, r, b)
                }
                None => (200, "OK", ""),
            };
            let response = format!(
                "HTTP/1.1 {} {}\r\n\
                 Content-Type: text/event-stream\r\n\
                 Content-Length: {}\r\n\
                 \r\n\
                 {}",
                status,
                reason,
                body.len(),
                body,
            );
            if stream.write_all(response.as_bytes()).await.is_err() {
                break;
            }
            if stream.flush().await.is_err() {
                break;
            }
        }
    }
}

/// Spawn a mock HTTP server that serves the given SSE bodies in order.
/// Returns `(base_url_string, handle)`.
#[allow(dead_code)]
async fn spawn_mock(responses: Vec<&'static str>) -> (String, tokio::task::JoinHandle<()>) {
    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let addr = std_listener.local_addr().expect("local addr");
    std_listener.set_nonblocking(true).expect("set_nonblocking");
    let listener =
        tokio::net::TcpListener::from_std(std_listener).expect("convert to tokio listener");

    let scripted: Vec<(u16, &'static str, &'static str)> =
        responses.into_iter().map(|b| (200, "OK", b)).collect();
    let handle = tokio::spawn(async move {
        serve_mock(&listener, scripted).await;
    });

    (format!("http://{addr}"), handle)
}

/// Same as `spawn_mock` but each entry carries a status code. For status
/// 503 use reason `Service Unavailable`, else `OK`. Body string is still
/// served after headers.
#[allow(dead_code)]
async fn spawn_mock_status(
    responses: Vec<(u16, &'static str)>,
) -> (String, tokio::task::JoinHandle<()>) {
    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let addr = std_listener.local_addr().expect("local addr");
    std_listener.set_nonblocking(true).expect("set_nonblocking");
    let listener =
        tokio::net::TcpListener::from_std(std_listener).expect("convert to tokio listener");

    let scripted: Vec<(u16, &'static str, &'static str)> = responses
        .into_iter()
        .map(|(status, body)| {
            let reason = match status {
                503 => "Service Unavailable",
                429 => "Too Many Requests",
                _ => "OK",
            };
            (status, reason, body)
        })
        .collect();
    let handle = tokio::spawn(async move {
        serve_mock(&listener, scripted).await;
    });

    (format!("http://{addr}"), handle)
}

/// Returns a `Settings` configured for tests against a mock server.
#[allow(dead_code)]
fn settings_for(workspace: &std::path::Path, base_url: &str) -> crate::config::Settings {
    crate::config::Settings {
        model: "mock-model".into(),
        base_url: base_url.into(),
        api_key: None,
        workspace: workspace.to_path_buf(),
        max_iterations: 5,
        mode: Mode::Agent,
        yolo: true,
        temperature: 0.0,
        max_tokens: 4096,
        rules: None,
        context_window: 128_000,
        compact_threshold: 0.75,
        no_stream: false,
        verify: false,
        confirm_shell: false,
        theme: "ravenwood".into(),
        searxng_url: None,
        searxng_engines: Vec::new(),
        sandbox_extra_rw: Vec::new(),
    }
}

fn plain(role: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: Some("x".into()),
        tool_calls: None,
        tool_call_id: None,
    }
}

fn tool_only() -> ChatMessage {
    ChatMessage {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(vec![ToolCall {
            id: "call_1".into(),
            type_: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
        }]),
        tool_call_id: None,
    }
}

#[test]
fn no_reminders_early_and_clean() {
    let msgs = vec![plain("system"), plain("user"), plain("assistant")];
    assert!(compute_reminders(&msgs, 0).is_empty());
    assert!(compute_reminders(&msgs, 2).is_empty());
}

#[test]
fn loop_breaker_fires_after_3_tool_only_turns() {
    let mut msgs = vec![plain("system")];
    for _ in 0..3 {
        msgs.push(tool_only());
    }
    let r = compute_reminders(&msgs, 3);
    assert!(
        r.iter().any(|t| t.contains("Stop calling tools")),
        "loop breaker should fire, got {r:?}"
    );
}

#[test]
fn loop_breaker_ignores_recent_text_output() {
    // Only 2 tool-only turns; the third has content, so no loop breaker.
    let msgs = vec![
        plain("system"),
        tool_only(),
        tool_only(),
        plain("assistant"),
    ];
    let r = compute_reminders(&msgs, 3);
    assert!(
        !r.iter().any(|t| t.contains("Stop calling tools")),
        "should not fire with text output, got {r:?}"
    );
}

#[test]
fn iteration_5_adds_reflect_nudge() {
    let msgs = vec![plain("system")];
    let r = compute_reminders(&msgs, 5);
    assert!(
        r.iter().any(|t| t.contains("Reflect")),
        "iteration-5 nudge should fire, got {r:?}"
    );
}

#[test]
fn iteration_5_does_not_fire_elsewhere() {
    let msgs = vec![plain("system")];
    assert!(!compute_reminders(&msgs, 4)
        .iter()
        .any(|t| t.contains("Reflect")));
    assert!(!compute_reminders(&msgs, 6)
        .iter()
        .any(|t| t.contains("Reflect")));
}

#[tokio::test]
async fn repeated_identical_failing_tool_detected() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/server.ts"), "existing content\n").unwrap();
    let fail_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"search_replace\",\"arguments\":\"{\\\"path\\\":\\\"src/server.ts\\\",\\\"old_string\\\":\\\"\\\",\\\"new_string\\\":\\\"new content\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        fail_round, fail_round, fail_round, fail_round, text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.max_iterations = 5;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("add route", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(
        agent.consecutive_failure_count >= 3,
        "should track 3+ consecutive identical failures, got {}",
        agent.consecutive_failure_count
    );
    assert!(
        agent.consecutive_failure_key.is_some(),
        "should have a failure key set"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[test]
fn args_string_passes_through() {
    assert_eq!(
        args_to_string(&json!("{\"path\":\"a.rs\"}")),
        "{\"path\":\"a.rs\"}"
    );
}

#[test]
fn args_object_serialized() {
    let s = args_to_string(&json!({"path": "a.rs", "n": 3}));
    assert!(
        s.contains("\"path\":\"a.rs\"") || s.contains("\"path\": \"a.rs\""),
        "args object should serialize path: {s}"
    );
    assert!(s.contains("\"n\":3"), "args object should serialize n: {s}");
}

#[test]
fn args_null_is_empty() {
    assert_eq!(args_to_string(&Value::Null), "");
}

#[test]
fn args_number_serialized() {
    assert_eq!(args_to_string(&json!(42)), "42");
}

#[tokio::test]
async fn stream_text_only_reaches_done() {
    let tmp = tempfile::tempdir().unwrap();
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![body]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Hel")));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "lo")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    // assistant message appended with the full concatenated content
    let last = agent.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content.as_deref(), Some("Hello"));
}

#[tokio::test]
async fn stream_tool_call_then_answer() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    // Round 1: SSE with a read_file tool call (no content delta).
    let tool_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    // Round 2: SSE with the final text answer.
    let final_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Done reading.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![tool_round, final_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("read a.rs", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read_file")));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolEnd { name, preview } if name == "read_file" && preview.contains("fn main"))));
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Done reading.")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    // The tool result was injected into conversation history.
    assert!(agent
        .messages
        .iter()
        .any(|m| m.role == "tool" && m.content.as_deref().unwrap_or("").contains("fn main")));
}

#[tokio::test]
async fn retries_on_5xx_then_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let ok_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock_status(vec![(503, "oops"), (200, ok_body)]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("ping", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Retry { attempt: 1, .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn non_stream_text_message() {
    let tmp = tempfile::tempdir().unwrap();
    let body = r#"{"choices":[{"message":{"role":"assistant","content":"plain json answer"}}]}"#;
    let (base, _h) = spawn_mock(vec![body]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.no_stream = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("go", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "plain json answer")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn model_not_found_no_retry() {
    let tmp = tempfile::tempdir().unwrap();
    let err_body = r#"{"error":"model 'nope' not found"}"#;
    let (base, _h) = spawn_mock_status(vec![(404, err_body)]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("go", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Error(msg) if msg.contains("not found"))));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Retry { .. })));
}

#[tokio::test]
async fn verify_requires_run_tests() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        edit_round, text_round, text_round, text_round, text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.rs", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn verify_passes_when_run_tests_called() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "#[test]\nfn it_works() { assert_eq!(2 + 2, 4); }\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let test_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"run_tests\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, test_round, text_round]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit and verify", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_off_does_not_gate() {
    let tmp = tempfile::tempdir().unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, text_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.rs", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_caps_at_max_attempts() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        edit_round, text_round, text_round, text_round, text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.rs", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    let verify_count = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::VerifyRequired))
        .count();
    assert_eq!(verify_count, 3);
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_skips_when_no_test_runner() {
    let tmp = tempfile::tempdir().unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    // No Cargo.toml in the tempdir → no test runner → gate skipped.
    let (base, _h) = spawn_mock(vec![edit_round, text_round]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.rs", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_gates_when_package_json_and_node_modules_exist() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        "{\"name\":\"x\",\"scripts\":{\"test\":\"vitest\"}}",
    )
    .unwrap();
    std::fs::create_dir(tmp.path().join("node_modules")).unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.ts\\\",\\\"content\\\":\\\"export const x = 1;\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        edit_round, text_round, text_round, text_round, text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.ts", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_skips_when_package_json_but_no_node_modules() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        "{\"name\":\"x\",\"scripts\":{\"test\":\"vitest\"}}",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.ts\\\",\\\"content\\\":\\\"export const x = 1;\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, text_round]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.ts", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
#[cfg(target_os = "linux")]
async fn verify_passes_when_run_shell_runs_test_command() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "#[test]\nfn it_works() { assert_eq!(2 + 2, 4); }\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let shell_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"run_shell\",\"arguments\":\"{\\\"command\\\":\\\"cargo test\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, shell_round, text_round]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent
        .run("edit and verify via run_shell", tx)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(!events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_still_gates_when_run_shell_is_not_test_command() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let shell_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"run_shell\",\"arguments\":\"{\\\"command\\\":\\\"cargo build\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        edit_round,
        shell_round,
        text_round,
        text_round,
        text_round,
        text_round,
        text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    s.max_iterations = 6;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit and build", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_gates_when_run_tests_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "#[test]\nfn failing() { panic!(\"boom\"); }\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let test_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"run_tests\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"tests passed\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        edit_round, test_round, text_round, text_round, text_round, text_round, text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    s.max_iterations = 6;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit and run failing tests", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn verify_gates_when_run_shell_test_exits_nonzero() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "#[test]\nfn failing() { panic!(\"boom\"); }\n",
    )
    .unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let shell_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"run_shell\",\"arguments\":\"{\\\"command\\\":\\\"cargo test\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"tests passed\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![
        edit_round,
        shell_round,
        text_round,
        text_round,
        text_round,
        text_round,
        text_round,
    ])
    .await;
    let mut s = settings_for(tmp.path(), &base);
    s.verify = true;
    s.max_iterations = 6;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent
        .run("edit and run failing tests via run_shell", tx)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired)));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn retries_on_429_then_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let ok_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock_status(vec![(429, "rate limited"), (200, ok_body)]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("ping", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Retry { attempt: 1, .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn compaction_triggers_when_context_exceeds_threshold() {
    let tmp = tempfile::tempdir().unwrap();
    let summarizer_body = r#"{"choices":[{"message":{"role":"assistant","content":"summary"}}]}"#;
    let agent_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![summarizer_body, agent_body]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.context_window = 500;
    s.compact_threshold = 0.5;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Compacted { .. })));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn multi_turn_conversation() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    let turn1 = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let turn2 = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"File looks good.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let turn3 = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"All done.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![turn1, turn2, turn3]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("read a.rs", tx).await.unwrap();
    let mut events1 = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events1.push(ev);
    }
    assert!(events1
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read_file")));
    assert!(events1.iter().any(|e| matches!(e, AgentEvent::Done)));

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("what do you think?", tx).await.unwrap();
    let mut events2 = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events2.push(ev);
    }
    assert!(events2
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "All done.")));
    assert!(events2.iter().any(|e| matches!(e, AgentEvent::Done)));

    let user_msgs: Vec<_> = agent.messages.iter().filter(|m| m.role == "user").collect();
    assert_eq!(user_msgs.len(), 2);
}

#[tokio::test]
async fn repo_map_stale_after_file_edit() {
    let tmp = tempfile::tempdir().unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, text_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    assert!(!agent.repo_map_stale);

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.rs", tx).await.unwrap();
    while rx.try_recv().is_ok() {}

    assert!(agent.repo_map_stale);
}

#[tokio::test]
async fn repo_map_rebuilt_on_next_turn_when_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, text_round, text_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.rs", tx).await.unwrap();
    while rx.try_recv().is_ok() {}
    assert!(agent.repo_map_stale);

    let sys_before = agent.messages[0].content.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("next turn", tx).await.unwrap();
    while rx.try_recv().is_ok() {}

    assert!(!agent.repo_map_stale);
    let sys_after = agent.messages[0].content.clone();
    assert_eq!(
        sys_before, sys_after,
        "system message should be rebuilt (same content for small workspace)"
    );
}

#[tokio::test]
async fn repo_map_not_rebuilt_when_not_stale() {
    let tmp = tempfile::tempdir().unwrap();
    let text_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![text_round, text_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    assert!(!agent.repo_map_stale);

    let sys_before = agent.messages[0].content.clone();

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    while rx.try_recv().is_ok() {}
    assert!(!agent.repo_map_stale);

    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello again", tx).await.unwrap();
    while rx.try_recv().is_ok() {}

    assert_eq!(
        agent.messages[0].content, sys_before,
        "system message unchanged when not stale"
    );
}

#[tokio::test]
async fn budget_exhaustion_emits_summary_and_done_not_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();

    // Every iteration the model calls a tool (never finishing on its own),
    // so the loop exhausts `max_iterations`. The wrap-up then makes ONE
    // toolless request whose text becomes the persisted summary.
    let tool_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let summary_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Summarized progress so far.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![tool_round, tool_round, summary_round]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.max_iterations = 2;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("do the thing", tx).await.unwrap();

    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    // Must end cleanly with Done, NOT an Error (the old MaxIterations path).
    let saw_error = events.iter().any(|e| matches!(e, AgentEvent::Error(_)));
    assert!(
        !saw_error,
        "budget exhaustion should not emit an Error event"
    );
    let saw_done = events.iter().any(|e| matches!(e, AgentEvent::Done));
    assert!(saw_done, "budget exhaustion should emit Done");

    // The summary must be persisted as a real assistant turn so a later
    // "continue" resumes from a coherent conversation.
    let last = agent.messages.last().expect("a final message");
    assert_eq!(last.role, "assistant", "last message should be the summary");
    assert!(
        last.content
            .as_deref()
            .unwrap_or("")
            .contains("Summarized progress"),
        "last assistant message should be the summary, got {:?}",
        last.content
    );

    // The summary user nudge is persisted too, keeping the alternation valid.
    let second_to_last = &agent.messages[agent.messages.len() - 2];
    assert_eq!(second_to_last.role, "user");
    assert!(
        second_to_last
            .content
            .as_deref()
            .unwrap_or("")
            .contains("maximum number of tool-calling iterations"),
        "summary prompt should be injected as a user message"
    );
}

#[tokio::test]
async fn blank_response_stalls_then_recovers_not_done() {
    let tmp = tempfile::tempdir().unwrap();
    // Round 1-2: blank responses (only [DONE], no content delta).
    let blank = "data: [DONE]\n\n";
    // Round 3: a real text answer after the nudges.
    let final_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Here is the report.\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![blank, blank, final_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("write the report", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    // The blank turns did NOT immediately finish with an empty Done.
    // Instead the model was nudged and re-ran, eventually producing text.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Here is the report.")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    // The persisted final assistant message carries the real content, not empty.
    let last = agent.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content.as_deref(), Some("Here is the report."));
    // blank_attempts was incremented past zero, proving the stall was handled.
    assert!(agent.blank_attempts > 0);
}

#[tokio::test]
async fn same_file_edits_in_one_turn_are_not_lost() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "foo\n").unwrap();
    // Round 1: two search_replace calls in ONE turn against a.txt.
    let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"search_replace\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"old_string\\\":\\\"foo\\\",\\\"new_string\\\":\\\"bar\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"type\":\"function\",\"function\":{\"name\":\"search_replace\",\"arguments\":\"{\\\"path\\\":\\\"a.txt\\\",\\\"old_string\\\":\\\"bar\\\",\\\"new_string\\\":\\\"baz\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
    // Round 2: the model's final text answer.
    let final_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![edit_round, final_round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("edit a.txt", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}
    // Both edits must have applied in order: foo -> bar -> baz.
    let content = std::fs::read_to_string(tmp.path().join("a.txt")).unwrap();
    assert!(
        content.contains("baz"),
        "final content should contain 'baz': {content}"
    );
    assert!(!content.contains("foo"), "foo should be gone: {content}");
    // The turn ended with the final text answer.
    let last = agent.messages.last().unwrap();
    assert_eq!(last.content.as_deref(), Some("done"));
}
