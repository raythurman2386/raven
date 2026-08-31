//! Offline fake-model agent-loop tests.
//!
//! These drive the full `Agent::run` loop without any HTTP by installing a
//! scripted [`CompletionSource`] (see [`super::core::Agent::with_completion_source`]).
//! The closure receives the outgoing request body and returns a raw SSE body
//! string, so the loop's compaction, tool dispatch, stall recovery, and
//! finish logic all run against deterministic scripted completions.

use super::super::core::{clamp_max_tokens, Agent, CompletionSource};
use super::super::types::AgentEvent;
use crate::config::{Mode, Provider, Settings};
use serde_json::{json, Value};
use tokio::sync::mpsc;

/// Skip a test that invokes a real tool (write_file/run_tests/run_shell) which
/// spawns a confined child, when running under a restrictive outer sandbox
/// that kills the child. Returns `true` when the test should skip.
fn skip_if_outer_sandbox() -> bool {
    if crate::testutil::outer_sandbox_restrictive() {
        eprintln!("outer sandbox restrictive; skipping fake-model test that invokes a real tool");
        true
    } else {
        false
    }
}

/// Build a `Settings` for offline tests. `base_url` is unused (no HTTP) but
/// kept so the settings are realistic.
fn settings_for(workspace: &std::path::Path) -> Settings {
    Settings {
        model: "fake-model".into(),
        provider: Provider::builtin("ollama").expect("ollama builtin"),
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
async fn steer_channel_injects_direction_at_iteration_boundary() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Round 1: a read_file tool call. The completion closure queues a
    // direction into the steering channel while that round runs (the way
    // the TUI does), so the loop drains it at the next boundary — after the
    // tool result — and the model sees it in round 2.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
    let mut bodies = vec![
        sse_tool_call("call_1", "read_file", r#"{"path":"a.rs"}"#),
        sse_text("Applied the redirect."),
    ]
    .into_iter();
    let mut first = true;
    let steer = steer_tx.clone();
    let source: CompletionSource = Box::new(move |_req| {
        if first {
            first = false;
            let _ = steer.send("focus on the main function".into());
        }
        bodies
            .next()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_string())
    });
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(source)
        .with_steer_channel(steer_rx);
    let (tx, mut rx) = mpsc::channel(64);
    drop(steer_tx);
    agent.run("read a.rs", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events.iter().any(
            |e| matches!(e, AgentEvent::Steered(t) if t.contains("focus on the main function"))
        ),
        "Steered event must fire when the direction lands"
    );
    let steer_idx = agent
        .messages
        .iter()
        .position(|m| {
            m.role == "user" && m.content.as_deref().is_some_and(|c| c.contains("[steer]"))
        })
        .expect("[steer] user message must be persisted");
    assert!(
        steer_idx > 0 && agent.messages[steer_idx - 1].role == "tool",
        "[steer] must land after the tool result, got {:?}",
        &agent.messages[..=steer_idx]
    );
    let last = agent.messages.last().unwrap();
    assert_eq!(
        last.content.as_deref(),
        Some("Applied the redirect."),
        "turn must continue after the steer and use the redirected round"
    );
}

#[tokio::test]
async fn steer_at_finish_boundary_extends_turn_instead_of_ending() {
    if skip_if_outer_sandbox() {
        return;
    }
    // Round 1: the model tries to finish ("All done."). The direction is
    // queued while that round streams, so at the finish boundary the turn
    // must NOT end — it runs another round that honors the redirect.
    let tmp = tempfile::tempdir().unwrap();
    let (steer_tx, steer_rx) = mpsc::unbounded_channel::<String>();
    let mut bodies = vec![
        sse_text("All done."),
        sse_text("Redirected: here are the follow-up results."),
    ]
    .into_iter();
    let mut first = true;
    let steer = steer_tx.clone();
    let source: CompletionSource = Box::new(move |_req| {
        if first {
            first = false;
            let _ = steer.send("also check the config".into());
        }
        bodies
            .next()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_string())
    });
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(source)
        .with_steer_channel(steer_rx);
    let (tx, mut rx) = mpsc::channel(64);
    drop(steer_tx);
    agent.run("do the main task", tx).await.unwrap();
    let events = drain(&mut rx).await;

    // The turn continued: the final assistant message is the round-2
    // content, not the first wrap-up.
    let last = agent.messages.last().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(
        last.content.as_deref(),
        Some("Redirected: here are the follow-up results.")
    );
    // History contains the first wrap-up, then the [steer] user message.
    let roles: Vec<(&str, Option<&str>)> = agent
        .messages
        .iter()
        .map(|m| (m.role.as_str(), m.content.as_deref()))
        .collect();
    assert!(
        roles.windows(2).any(|w| w[0].0 == "assistant"
            && w[1].0 == "user"
            && w[1].1.unwrap_or("").contains("[steer]")),
        "assistant wrap-up must be followed by the [steer] message: {roles:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::Steered(t) if t.contains("also check the config"))),
        "Steered event must fire"
    );
}

#[tokio::test]
async fn executes_tool_then_finishes() {
    if skip_if_outer_sandbox() {
        return;
    }
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
    if skip_if_outer_sandbox() {
        return;
    }
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

#[tokio::test]
async fn budget_exhaustion_does_not_auto_commit() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.test"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::fs::write(tmp.path().join("seed.txt"), "seed\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "seed.txt"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "seed"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    let log_before = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(tmp.path())
        .output()
        .unwrap();

    // Every iteration the model writes a file (dirtying the tree), never
    // finishing on its own. After max_iterations the budget is exhausted —
    // the harness must wrap up without creating a commit.
    let write_round = sse_tool_call(
        "call_w",
        "write_file",
        r#"{"path":"out.txt","content":"done"}"#,
    );
    let summary_round = sse_text("Budget exhausted, work left uncommitted.");

    let mut s = settings_for(tmp.path());
    s.max_iterations = 2;
    let mut agent = Agent::new(s).unwrap().with_completion_source(scripted(vec![
        write_round.clone(),
        write_round.clone(),
        summary_round,
    ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("do the thing", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events.iter().any(|e| matches!(e, AgentEvent::Done)),
        "turn must end with Done"
    );
    assert!(
        !events.iter().any(|e| matches!(e, AgentEvent::Error(_))),
        "turn must not emit Error"
    );
    assert!(
        !agent.sandbox.is_working_tree_clean(),
        "working tree must stay dirty — harness must not auto-commit"
    );
    let log_after = std::process::Command::new("git")
        .args(["log", "--oneline"])
        .current_dir(tmp.path())
        .output()
        .unwrap();
    assert_eq!(
        log_before.stdout, log_after.stdout,
        "HEAD must not move on budget exhaustion"
    );
    assert!(
        tmp.path().join("out.txt").exists(),
        "edited file must still be on disk"
    );
}

/// Build an SSE body with multiple tool calls in one assistant turn.
fn sse_multi_tools(calls: &[(&str, &str, &str)]) -> String {
    let mut parts = Vec::new();
    for (i, (id, name, args)) in calls.iter().enumerate() {
        parts.push(format!(
            "{{\"index\":{i},\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":{}}}}}",
            json!(id),
            json!(name),
            json!(args),
        ));
    }
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{}]}}}}]}}\n\ndata: [DONE]\n\n",
        parts.join(",")
    )
}

#[tokio::test]
async fn mixed_read_write_tool_results_preserve_call_order() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "A\n").unwrap();
    std::fs::write(tmp.path().join("b.txt"), "B\n").unwrap();

    // Order: read a, write c, read b — mutator in the middle used to reorder results.
    let multi = sse_multi_tools(&[
        ("call_r1", "read_file", r#"{"path":"a.txt"}"#),
        (
            "call_w1",
            "write_file",
            r#"{"path":"c.txt","content":"C\n"}"#,
        ),
        ("call_r2", "read_file", r#"{"path":"b.txt"}"#),
    ]);
    let done = sse_text("done");
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![multi, done]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("mixed tools", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    // Find the assistant tool_calls message, then the next three tool results.
    let mut ids = Vec::new();
    let mut saw_assistant_tools = false;
    for m in &agent.messages {
        if m.role == "assistant" && m.tool_calls.is_some() {
            saw_assistant_tools = true;
            continue;
        }
        if saw_assistant_tools && m.role == "tool" {
            ids.push(m.tool_call_id.clone().unwrap_or_default());
            if ids.len() == 3 {
                break;
            }
        }
    }
    assert_eq!(
        ids,
        vec![
            "call_r1".to_string(),
            "call_w1".to_string(),
            "call_r2".to_string()
        ],
        "tool results must follow tool_calls[] order, got {ids:?}"
    );
    assert!(
        tmp.path().join("c.txt").is_file(),
        "write should have applied"
    );
}

#[tokio::test]
async fn title_prompt_is_toolless_and_does_not_pollute_history() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let seen_src = seen.clone();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(Box::new(move |req: &Value| {
            seen_src.lock().unwrap().push(req.clone());
            sse_text("Optimizing Editor for Large Files")
        }));
    let before = agent.messages.len();
    let (tx, mut rx) = mpsc::channel(64);
    let prompt = "Reply with ONLY a concise 3-5 word title in Title Case \
         (no quotes, no punctuation) for a coding session that begins \
         with this request:\n\ncan we inspect this project";
    agent.run(prompt, tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert_eq!(
        agent.messages.len(),
        before,
        "title turn must not append user/assistant messages"
    );
    assert!(events.iter().any(|e| matches!(
        e,
        AgentEvent::SessionTitle(t) if t == "Optimizing Editor for Large Files"
    )));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::Checkpoint(_))),
        "title turn must not checkpoint a fake conversation"
    );

    let reqs = seen.lock().unwrap();
    assert_eq!(reqs.len(), 1, "exactly one title completion");
    assert!(
        reqs[0].get("tools").is_none(),
        "title request must not advertise tools"
    );
    assert_eq!(reqs[0]["max_tokens"], 24);
    let msgs = reqs[0]["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[1]["content"], "can we inspect this project");
}

#[tokio::test]
async fn tool_round_emits_checkpoint_with_tool_result() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("call_1", "read_file", r#"{"path":"a.rs"}"#),
            sse_text("Done reading."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("read a.rs", tx).await.unwrap();
    let events = drain(&mut rx).await;

    let checkpoint = events.iter().find_map(|e| match e {
        AgentEvent::Checkpoint(msgs) => Some(msgs),
        _ => None,
    });
    let msgs = checkpoint.expect("tool round must emit Checkpoint");
    assert!(
        msgs.iter()
            .any(|m| m.role == "tool" && m.content.as_deref().unwrap_or("").contains("fn main")),
        "checkpoint must include the tool result so a crash mid-turn keeps history"
    );
}
