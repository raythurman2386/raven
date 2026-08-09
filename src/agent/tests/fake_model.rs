//! Offline fake-model agent-loop tests.
//!
//! These drive the full `Agent::run` loop without any HTTP by installing a
//! scripted [`CompletionSource`] (see [`super::core::Agent::with_completion_source`]).
//! The closure receives the outgoing request body and returns a raw SSE body
//! string, so the loop's compaction, tool dispatch, stall recovery, and
//! finish logic all run against deterministic scripted completions.

use super::super::core::{clamp_max_tokens, Agent, CompletionSource};
use super::super::types::AgentEvent;
use crate::config::{Mode, Settings};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Build a `Settings` for offline tests. `base_url` is unused (no HTTP) but
/// kept so the settings are realistic.
fn settings_for(workspace: &std::path::Path) -> Settings {
    Settings {
        model: "fake-model".into(),
        base_url: "http://127.0.0.1:1".into(), // never contacted
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
    }
}

/// A scripted completion source that serves the given SSE bodies in order.
/// Each call pops the next body; once exhausted it returns a benign empty
/// completion so an unexpected extra request never hangs the loop.
fn scripted(bodies: Vec<String>) -> CompletionSource {
    let mut queue = bodies.into_iter();
    Box::new(move |_req: &Value| {
        queue
            .next()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_string())
    })
}

/// Build an SSE body that streams a single text content delta.
fn sse_text(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json!(text)
    )
}

/// Build an SSE body that carries one tool call (no content delta).
fn sse_tool_call(id: &str, name: &str, args: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":{}}}}}]}}}}]}}\n\ndata: [DONE]\n\n",
        json!(id),
        json!(name),
        json!(args),
    )
}

/// Drain a receiver into a Vec of events.
async fn drain(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

#[tokio::test]
async fn finishes_when_assistant_returns_content_without_tools() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![sse_text("Hello, world.")]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("hi", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Hello, world.")),
        "should stream the assistant text"
    );
    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "should finish with Done"
    );
    // The assistant message is persisted with the full content.
    let last = agent.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content.as_deref(), Some("Hello, world."));
}

#[tokio::test]
async fn blank_content_without_tools_is_stall_then_recovers_or_caps() {
    let tmp = tempfile::tempdir().unwrap();
    // Two blank turns (no content, no tools), then a real answer.
    let blank = "data: [DONE]\n\n".to_string();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            blank.clone(),
            blank.clone(),
            sse_text("Here is the report."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("write the report", tx).await.unwrap();
    let events = drain(&mut rx).await;

    // The blank turns must NOT have produced a clean Done on the first blank.
    // Instead the loop stalled, injected a reminder, and re-ran until it got
    // real content.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Here is the report.")),
        "should eventually produce real content"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(
        agent.blank_attempts > 0,
        "blank_attempts should be incremented past zero"
    );
    let last = agent.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.content.as_deref(), Some("Here is the report."));
}

#[tokio::test]
async fn blank_content_caps_after_max_attempts() {
    let tmp = tempfile::tempdir().unwrap();
    // More blanks than the cap (3). After the cap, the turn must still end
    // visibly (emit_summary) rather than hang or emit a clean empty Done.
    let blank = "data: [DONE]\n\n".to_string();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("do the thing", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "turn must end with Done even after the blank cap"
    );
    assert_eq!(agent.blank_attempts, 3, "blank attempts should cap at 3");
}

#[tokio::test]
async fn executes_tool_then_finishes() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    // Round 1: a read_file tool call. Round 2: the final text answer.
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("call_1", "read_file", r#"{"path":"a.rs"}"#),
            sse_text("Done reading."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("read a.rs", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read_file")),
        "should emit ToolStart for read_file"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolEnd { name, .. } if name == "read_file")),
        "should emit ToolEnd for read_file"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Done reading.")),
        "should stream the final answer"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    // The tool result was injected into the conversation history.
    assert!(
        agent
            .messages
            .iter()
            .any(|m| m.role == "tool" && m.content.as_deref().unwrap_or("").contains("fn main")),
        "tool result should be in history"
    );
}

#[tokio::test]
async fn serializes_same_file_mutating_tools() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "foo\n").unwrap();
    // One turn with two search_replace calls against the same file. They must
    // apply serially in call order: foo -> bar -> baz.
    let edit_round = {
        let call_a = json!({
            "index": 0, "id": "call_a", "type": "function",
            "function": {"name": "search_replace", "arguments": r#"{"path":"a.txt","old_string":"foo","new_string":"bar"}"#},
        });
        let call_b = json!({
            "index": 1, "id": "call_b", "type": "function",
            "function": {"name": "search_replace", "arguments": r#"{"path":"a.txt","old_string":"bar","new_string":"baz"}"#},
        });
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices": [{"delta": {"tool_calls": [call_a]}}]}),
            json!({"choices": [{"delta": {"tool_calls": [call_b]}}]}),
        )
    };
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![edit_round, sse_text("done")]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("edit a.txt", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let content = std::fs::read_to_string(tmp.path().join("a.txt")).unwrap();
    assert!(
        content.contains("baz"),
        "final content should contain 'baz': {content}"
    );
    assert!(!content.contains("foo"), "foo should be gone: {content}");
}

#[test]
fn max_tokens_clamped_to_remaining_context() {
    // Plenty of room: max_tokens is the binding constraint.
    assert_eq!(clamp_max_tokens(4096, 100, 128_000, 64), 4096);
    // Context nearly full: clamp down to the remaining budget.
    assert_eq!(clamp_max_tokens(4096, 127_000, 128_000, 64), 936);
    // Context over budget: floor of 256 is still reserved.
    assert_eq!(clamp_max_tokens(4096, 200_000, 128_000, 64), 256);
    // Margin larger than remaining: floor applies.
    assert_eq!(clamp_max_tokens(4096, 128_000, 128_000, 64), 256);
}
