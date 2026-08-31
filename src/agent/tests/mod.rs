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

/// Skip a test that binds a mock HTTP server (AF_INET socket) when running
/// under a restrictive outer sandbox that SIGSYS-kills sockets. Returns `true`
/// when the test should skip.
fn skip_if_outer_sandbox() -> bool {
    if crate::testutil::outer_sandbox_restrictive() {
        eprintln!("outer sandbox blocks AF_INET sockets; skipping mock-server agent test");
        true
    } else {
        false
    }
}

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
/// `ProviderUnreachable` path.
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
                400 => "Bad Request",
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
    let mut provider = crate::config::Provider::builtin("ollama").expect("ollama builtin");
    provider.base_url = base_url.into();
    crate::config::Settings {
        model: "mock-model".into(),
        provider,
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
        allow_delegate: true,
    }
}

fn plain(role: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: Some("x".into()),
        tool_calls: None,
        tool_call_id: None,
        usage: None,
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
        usage: None,
    }
}
#[test]
fn no_reminders_early_and_clean() {
    let msgs = vec![plain("system"), plain("user"), plain("assistant")];
    assert!(compute_reminders(&msgs, 0, None, &[]).is_empty());
    assert!(compute_reminders(&msgs, 3, None, &[]).is_empty());
}

#[test]
fn loop_breaker_fires_after_6_tool_only_turns() {
    let mut msgs = vec![plain("system")];
    for _ in 0..6 {
        msgs.push(tool_only());
    }
    let r = compute_reminders(&msgs, 6, None, &[]);
    assert!(
        r.iter().any(|t| t.contains("stuck in a loop")),
        "loop breaker should fire, got {r:?}"
    );
}

#[test]
fn loop_breaker_does_not_fire_after_3_tool_only_turns() {
    // Normal context-gathering (goal → list → grep → read) is 3-4 tool-only
    // turns; it must not be interrupted by the loop breaker.
    let mut msgs = vec![plain("system")];
    for _ in 0..3 {
        msgs.push(tool_only());
    }
    let r = compute_reminders(&msgs, 3, None, &[]);
    assert!(
        !r.iter().any(|t| t.contains("stuck in a loop")),
        "loop breaker should not fire at 3 turns, got {r:?}"
    );
}

#[test]
fn loop_breaker_ignores_recent_text_output() {
    // Only 5 tool-only turns; the sixth has content, so no loop breaker.
    let msgs = vec![
        plain("system"),
        tool_only(),
        tool_only(),
        tool_only(),
        tool_only(),
        tool_only(),
        plain("assistant"),
    ];
    let r = compute_reminders(&msgs, 6, None, &[]);
    assert!(
        !r.iter().any(|t| t.contains("stuck in a loop")),
        "should not fire with text output, got {r:?}"
    );
}

#[test]
fn iteration_5_nudge_removed() {
    let msgs = vec![plain("system")];
    for i in [4, 5, 6] {
        assert!(
            !compute_reminders(&msgs, i, None, &[])
                .iter()
                .any(|t| t.contains("Reflect")),
            "no reflect nudge at iter {i}"
        );
    }
}

#[test]
fn goal_aware_reminder_anchors_goal_and_next_task() {
    let msgs = vec![plain("system")];
    let goal = crate::state::Goal {
        description: "Ship the feature".into(),
        status: "in_progress".into(),
        updated_at: "".into(),
    };
    let todos = vec![
        crate::state::TodoItem {
            content: "Done task".into(),
            status: "completed".into(),
            priority: "high".into(),
        },
        crate::state::TodoItem {
            content: "Next task".into(),
            status: "pending".into(),
            priority: "medium".into(),
        },
    ];
    let r = compute_reminders(&msgs, 4, Some(&goal), &todos);
    assert!(
        r.iter().any(|t| t.contains("Ship the feature")),
        "goal should be anchored, got {r:?}"
    );
    assert!(
        r.iter().any(|t| t.contains("Next task")),
        "next pending task should be anchored, got {r:?}"
    );
}

#[test]
fn goal_aware_reminder_fires_once_then_every_8th() {
    let goal = crate::state::Goal {
        description: "Ship it".into(),
        status: "in_progress".into(),
        updated_at: "".into(),
    };
    let msgs = vec![plain("system")];
    let fired = |i: usize| {
        compute_reminders(&msgs, i, Some(&goal), &[])
            .iter()
            .any(|t| t.contains("Ship it"))
    };
    assert!(fired(4), "first anchor at iter 4");
    for i in 5..12 {
        assert!(!fired(i), "no anchor at iter {i}");
    }
    assert!(fired(12), "second anchor at iter 12");
    assert!(!fired(13), "no anchor at iter 13");
    assert!(fired(20), "third anchor at iter 20");
}

#[test]
fn goal_aware_reminder_skips_completed_goal() {
    let msgs = vec![plain("system")];
    let goal = crate::state::Goal {
        description: "Done goal".into(),
        status: "completed".into(),
        updated_at: "".into(),
    };
    let r = compute_reminders(&msgs, 4, Some(&goal), &[]);
    assert!(
        !r.iter().any(|t| t.contains("Done goal")),
        "completed goal should not be anchored, got {r:?}"
    );
}

#[tokio::test]
async fn repeated_identical_failing_tool_detected() {
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
async fn stream_interruption_preserves_partial_text() {
    if skip_if_outer_sandbox() {
        return;
    }
    // A4: a mid-stream failure after some content was produced must keep the
    // partial assistant text (with an interruption hint) and finish via `Done`
    // so the session persists it — not drop the turn with an `Error`.
    let tmp = tempfile::tempdir().unwrap();
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Partial answer\"}}]}\n\n",
        "data: {\"error\":{\"message\":\"stream interrupted\"}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![body]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("explain", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    // The partial text was streamed to the user.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Partial answer")));
    // The turn finished cleanly (Done), not as an Error.
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
    // The partial text + interruption hint was persisted as an assistant turn.
    let last = agent.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    let content = last.content.as_deref().unwrap_or("");
    assert!(content.contains("Partial answer"), "content: {content}");
    assert!(
        content.contains("stream interrupted"),
        "should carry the interruption hint: {content}"
    );
}

#[tokio::test]
async fn stream_error_with_no_content_still_aborts() {
    if skip_if_outer_sandbox() {
        return;
    }
    // A4: a stream error with NO partial content must still abort via `Error`
    // (nothing to preserve), not fabricate an empty assistant turn.
    let tmp = tempfile::tempdir().unwrap();
    let body = concat!(
        "data: {\"error\":{\"message\":\"connection reset\"}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![body]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("explain", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }

    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::Error(msg) if msg.contains("connection reset"))));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Done)));
    // No assistant turn was fabricated.
    assert!(!agent.messages.iter().any(|m| m.role == "assistant"
        && m.content
            .as_deref()
            .unwrap_or("")
            .contains("stream interrupted")));
}

#[tokio::test]
async fn stream_tool_call_then_answer() {
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
#[cfg(target_os = "linux")]
async fn verify_gates_when_run_tests_exits_nonzero() {
    if skip_if_outer_sandbox() {
        return;
    }
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
#[cfg(target_os = "linux")]
async fn verify_gates_when_run_shell_test_exits_nonzero() {
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
async fn compaction_thrashing_pauses_after_cap() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // A summarizer that returns a huge summary so compaction never actually
    // reduces the history (after >= before) — the thrashing case.
    let huge_summary = "x".repeat(200_000);
    let summarizer_body = format!(
        r#"{{"choices":[{{"message":{{"role":"assistant","content":"{huge_summary}"}}}}]}}"#
    );
    let agent_body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    // Enough summarizer responses to exceed the thrash cap (3) plus the final
    // agent response. The mock serves responses in order and reuses the
    // connection, so we need one summarizer body per compaction attempt.
    let mut responses: Vec<String> = Vec::new();
    for _ in 0..6 {
        responses.push(summarizer_body.clone());
    }
    responses.push(agent_body.to_string());
    let static_responses: Vec<&'static str> = responses
        .iter()
        .map(|s| Box::leak(s.clone().into_boxed_str()) as &'static str)
        .collect();
    let (base, _h) = spawn_mock(static_responses).await;
    let mut s = settings_for(tmp.path(), &base);
    s.context_window = 500;
    s.compact_threshold = 0.5;
    s.max_iterations = 8;
    let mut agent = Agent::new(s).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    // Compaction should have been attempted but then paused after the cap.
    let compacted = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::Compacted { .. }))
        .count();
    assert!(compacted >= 1, "compaction should have been attempted");
    assert!(
        compacted <= 3,
        "thrashing should pause after the cap, got {compacted}"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

#[tokio::test]
async fn multi_turn_conversation() {
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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
async fn budget_exhaustion_summary_keeps_meter_and_strips_wire_usage() {
    if skip_if_outer_sandbox() {
        return;
    }
    // The wrap-up request is a real metered provider call: its meter must be
    // persisted on the summary message, and its request body — which replays
    // history that now carries usage — must NOT contain any usage field.
    // Main iterations run offline via completion_source; the wrap-up request
    // itself goes over HTTP, so the mock serves exactly the summary round.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();

    let tool_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"total_tokens\":110}}\n\n",
        "data: [DONE]\n\n",
    );
    let summary_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"Summarized progress so far.\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":500,\"completion_tokens\":7,\"total_tokens\":507}}\n\n",
        "data: [DONE]\n\n",
    );
    let seen_body = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let capture = seen_body.clone();
    let (base, _h) = spawn_mock(vec![summary_round]).await;
    let mut s = settings_for(tmp.path(), &base);
    s.max_iterations = 2;
    let mut agent =
        Agent::new(s)
            .unwrap()
            .with_completion_source(Box::new(move |body: &serde_json::Value| {
                // Record every outgoing main-loop body for the wire assertions.
                capture.lock().unwrap().push_str(&body.to_string());
                capture.lock().unwrap().push('\n');
                tool_round.to_string()
            }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("do the thing", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}

    // Wire: no replayed message in ANY outgoing body carried a usage field.
    let sent = seen_body.lock().unwrap().clone();
    for (i, body) in sent.lines().enumerate() {
        assert!(
            !body.contains("\"usage\""),
            "outgoing request {i} leaked usage: {body}"
        );
    }

    // The summary assistant message carries the wrap-up request's own meter.
    let last = agent.messages.last().expect("summary message");
    assert_eq!(last.role, "assistant");
    let u = last.usage.expect("wrap-up meter persisted on summary");
    assert_eq!(u.prompt_tokens, 500);
    assert_eq!(u.completion_tokens, 7);
}

#[tokio::test]
async fn blank_response_stalls_then_recovers_not_done() {
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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

#[tokio::test]
async fn stream_options_400_falls_back_and_retries() {
    if skip_if_outer_sandbox() {
        return;
    }
    // A strict provider that rejects `stream_options.include_usage` with a
    // 400 must not fail the turn: the field is stripped, the request is
    // retried immediately without it, and usage calibration is disabled for
    // this provider (estimator runs uncalibrated — graceful fallback).
    let tmp = tempfile::tempdir().unwrap();
    let reject =
        r#"{"error":{"message":"Unknown field: stream_options","type":"invalid_request_error"}}"#;
    let ok_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"fallback ok\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock_status(vec![(400, reject), (200, ok_round)]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    assert!(agent.usage_supported, "starts optimistic");
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    // The retried (field-less) request succeeded and the turn finished clean.
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "fallback ok")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
    // The incompatibility was recorded, so later requests skip the field.
    assert!(!agent.usage_supported);
    // Survives Agent rebuild (TUI/ACP rebuild every turn).
    let agent2 = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    assert!(
        !agent2.usage_supported,
        "usage_supported=false must persist across Agent::new for the same base URL"
    );
}

#[tokio::test]
async fn stream_options_400_after_transient_does_not_exhaust_retries() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Two 503s then a stream_options 400 must still succeed: the 400 strip
    // path must not consume a transient-retry slot.
    let tmp = tempfile::tempdir().unwrap();
    let reject =
        r#"{"error":{"message":"Unknown field: stream_options","type":"invalid_request_error"}}"#;
    let unavailable = r#"{"error":{"message":"busy"}}"#;
    let ok_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock_status(vec![
        (503, unavailable),
        (503, unavailable),
        (400, reject),
        (200, ok_round),
    ])
    .await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "recovered")));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(!events.iter().any(|e| matches!(e, AgentEvent::Error(_))));
}

#[tokio::test]
async fn usage_chunk_feeds_calibration() {
    if skip_if_outer_sandbox() {
        return;
    }
    // A provider that reports usage on the final empty-choices chunk feeds
    // one calibration sample per request; the clamp path then uses the
    // corrected estimate.
    let tmp = tempfile::tempdir().unwrap();
    let round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"measured\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":321,\"completion_tokens\":5,\"total_tokens\":326}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    assert_eq!(agent.calibration.samples(), 0);
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("measure me", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}
    // Exactly one sample was observed from the streamed usage chunk.
    assert_eq!(agent.calibration.samples(), 1);
    assert!(agent.calibration.offset().is_some());
    // correct() rounds the raw f64 offset; offset() returns the pre-rounded
    // i64. At a clamp boundary (x.5) the two can differ by one — allow that.
    let off = agent.calibration.offset().unwrap();
    let corrected = agent.calibration.correct(1000) as i64;
    assert!(
        (corrected - (1000i64 + off)).abs() <= 1,
        "correct(1000)={} should match 1000+offset({off}) within rounding",
        agent.calibration.correct(1000)
    );
}

#[tokio::test]
async fn no_usage_reported_leaves_calibration_inert() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Providers that never send usage keep the calibration inert: zero
    // samples, passthrough estimates — the graceful fallback.
    let tmp = tempfile::tempdir().unwrap();
    let round = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"plain\"}}]}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![round]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}
    assert_eq!(agent.calibration.samples(), 0);
    assert_eq!(agent.calibration.offset(), None);
    assert_eq!(agent.calibration.correct(555), 555);
}

#[tokio::test]
async fn streaming_request_includes_usage_and_feeds_calibration_offline() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Offline (completion_source) end-to-end: the outgoing streaming body
    // must request `stream_options.include_usage`, and the usage chunk in the
    // scripted response must feed exactly one calibration sample — no real
    // sockets required.
    let tmp = tempfile::tempdir().unwrap();
    let seen_body = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let capture = seen_body.clone();
    let mut agent = Agent::new(settings_for(tmp.path(), "http://127.0.0.1:1"))
        .unwrap()
        .with_completion_source(Box::new(move |body: &serde_json::Value| {
            *capture.lock().unwrap() = body.to_string();
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"measured\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":321,\"completion_tokens\":5,\"total_tokens\":326}}\n\n",
                "data: [DONE]\n\n",
            )
            .to_string()
        }));
    assert_eq!(agent.calibration.samples(), 0);
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("measure me", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}

    // Request side: include_usage was requested on the streaming body.
    let sent = seen_body.lock().unwrap().clone();
    assert!(
        sent.contains("include_usage"),
        "streaming request should set include_usage: {sent}"
    );
    // Response side: the usage chunk fed exactly one calibration sample.
    assert_eq!(agent.calibration.samples(), 1);
    let off = agent.calibration.offset().expect("offset after one sample");
    // correct() rounds the raw f64 offset; offset() returns the pre-rounded
    // i64. At a clamp boundary (x.5) the two can differ by one — allow that.
    let corrected = agent.calibration.correct(1000) as i64;
    assert!(
        (corrected - (1000i64 + off)).abs() <= 1,
        "correct(1000)={} should match 1000+offset({off}) within rounding",
        agent.calibration.correct(1000)
    );
}

#[tokio::test]
async fn usage_chunk_is_persisted_on_assistant_message() {
    if skip_if_outer_sandbox() {
        return;
    }
    // The provider's meter must ride out of the loop on the assistant
    // message it belongs to, so session persistence records real usage.
    // The wire replay path (request_messages_json) strips it again.
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(settings_for(tmp.path(), "http://127.0.0.1:1"))
        .unwrap()
        .with_completion_source(Box::new(|_body: &serde_json::Value| {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"measured reply\"}}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":321,\"completion_tokens\":5,\"total_tokens\":326}}\n\n",
                "data: [DONE]\n\n",
            )
            .to_string()
        }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("measure me", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}

    let last = agent.messages.last().expect("assistant message persisted");
    assert_eq!(last.role, "assistant");
    let u = last
        .usage
        .expect("usage meter must be attached to the assistant message");
    assert_eq!(u.prompt_tokens, 321);
    assert_eq!(u.completion_tokens, 5);
    assert_eq!(u.total_tokens, 326);
}

#[tokio::test]
async fn no_usage_reported_leaves_assistant_usage_none() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Providers without meters persist exactly as before: usage stays None
    // and no usage key appears in the serialized transcript line.
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(settings_for(tmp.path(), "http://127.0.0.1:1"))
        .unwrap()
        .with_completion_source(Box::new(|_body: &serde_json::Value| {
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"plain reply\"}}]}\n\n",
                "data: [DONE]\n\n",
            )
            .to_string()
        }));
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    agent.run("hello", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}

    let last = agent.messages.last().expect("assistant message");
    assert!(last.usage.is_none(), "no meter → usage stays None");
    let line = serde_json::to_string(last).unwrap();
    assert!(
        !line.contains("usage"),
        "legacy transcripts unchanged: {line}"
    );
}

#[tokio::test]
async fn tool_call_iterations_persist_one_usage_per_iteration() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Each provider response is one metered request; a tool-calling turn
    // must record the meter on the assistant message that requested the
    // tools, not drop it. The mock serves its scripted list in order, so
    // iteration 1 gets the tool call (usage 100) and iteration 2 — after the
    // tool result — gets the plain reply (usage 200), proving meters stay
    // attached per iteration instead of folding into one.
    let tmp = tempfile::tempdir().unwrap();
    let round_tools = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\"completion_tokens\":10,\"total_tokens\":110}}\n\n",
        "data: [DONE]\n\n",
    );
    let round_plain = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"done reading\"}}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":200,\"completion_tokens\":20,\"total_tokens\":220}}\n\n",
        "data: [DONE]\n\n",
    );
    let (base, _h) = spawn_mock(vec![round_tools, round_plain]).await;
    let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    // read_file on a real file so the tool succeeds and the turn continues.
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    agent.run("read the file", tx).await.unwrap();
    while let Ok(_ev) = rx.try_recv() {}

    let metered: Vec<(String, u64)> = agent
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .filter_map(|m| m.usage.map(|u| (m.role.clone(), u.prompt_tokens)))
        .collect();
    assert_eq!(
        metered,
        vec![
            ("assistant".to_string(), 100),
            ("assistant".to_string(), 200)
        ],
        "each iteration's meter lands on its own assistant message"
    );
}
