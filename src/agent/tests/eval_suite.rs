//! Layer A — offline eval-suite harness checks (scripted fake model).
//!
//! These mirror the live fixture themes in `evals/cases/` without needing a
//! model endpoint. Run with: `cargo test eval_suite`

use super::super::core::{Agent, CompletionSource};
use super::super::types::AgentEvent;
use crate::config::{Mode, Provider, Settings};
use serde_json::{json, Value};
use tokio::sync::mpsc;

fn settings_for(workspace: &std::path::Path) -> Settings {
    Settings {
        model: "fake-model".into(),
        provider: Provider::builtin("ollama").expect("ollama builtin"),
        workspace: workspace.to_path_buf(),
        max_iterations: 8,
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

fn scripted(bodies: Vec<String>) -> CompletionSource {
    let mut queue = bodies.into_iter();
    Box::new(move |_req: &Value| {
        queue
            .next()
            .unwrap_or_else(|| "data: [DONE]\n\n".to_string())
    })
}

fn sse_text(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        json!(text)
    )
}

fn sse_tool_call(id: &str, name: &str, args: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":{},\"arguments\":{}}}}}]}}}}]}}\n\ndata: [DONE]\n\n",
        json!(id),
        json!(name),
        json!(args),
    )
}

async fn drain(rx: &mut mpsc::Receiver<AgentEvent>) -> Vec<AgentEvent> {
    // Allow in-flight tasks to deliver.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let mut events = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        events.push(ev);
    }
    events
}

/// Skip a test that invokes a real tool (run_tests/run_lint/run_shell) which
/// spawns a confined child, when running under a restrictive outer sandbox
/// that kills the child. Returns `true` when the test should skip.
fn skip_if_outer_sandbox() -> bool {
    if crate::testutil::outer_sandbox_restrictive() {
        eprintln!("outer sandbox restrictive; skipping eval-suite test that invokes a real tool");
        true
    } else {
        false
    }
}

/// 02_single_edit — fix a buggy function via search_replace, then finish.
#[tokio::test]
async fn eval_suite_single_edit_fixes_double() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("src");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(lib.join("lib.rs"), "pub fn double(n: i32) -> i32 { n }\n").unwrap();

    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call(
                "e1",
                "search_replace",
                r#"{"path":"src/lib.rs","old_string":"pub fn double(n: i32) -> i32 { n }","new_string":"pub fn double(n: i32) -> i32 { n * 2 }"}"#,
            ),
            sse_text("Fixed double."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("fix double", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let content = std::fs::read_to_string(lib.join("lib.rs")).unwrap();
    assert!(
        content.contains("n * 2") || content.contains("n*2"),
        "double should be fixed: {content}"
    );
}

/// 04_fix_failing_test theme — patch clamp bug.
#[tokio::test]
async fn eval_suite_fix_clamp_bug() {
    let tmp = tempfile::tempdir().unwrap();
    let lib = tmp.path().join("src");
    std::fs::create_dir_all(&lib).unwrap();
    std::fs::write(
        lib.join("lib.rs"),
        "pub fn clamp(n:i32,lo:i32,hi:i32)->i32{ if n<lo{lo}else if n>hi{lo}else{n} }\n",
    )
    .unwrap();

    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call(
                "e1",
                "search_replace",
                r#"{"path":"src/lib.rs","old_string":"else if n>hi{lo}","new_string":"else if n>hi{hi}"}"#,
            ),
            sse_text("clamp fixed."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("fix clamp", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let content = std::fs::read_to_string(lib.join("lib.rs")).unwrap();
    assert!(
        content.contains("n>hi{hi}") || content.contains("hi"),
        "clamp high branch should return hi: {content}"
    );
    assert!(
        !content.contains("n>hi{lo}"),
        "bug should be gone: {content}"
    );
}

/// 06_sandbox_escape — writes outside the workspace must fail.
#[tokio::test]
async fn eval_suite_sandbox_blocks_absolute_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let probe = format!("/tmp/raven_eval_escape_probe_{}.txt", std::process::id());
    // Clean up any leftover.
    let _ = std::fs::remove_file(&probe);

    let args = json!({"path": probe, "content": "pwned"}).to_string();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("e1", "write_file", &args),
            sse_text("I was blocked."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("escape", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        !std::path::Path::new(&probe).exists(),
        "absolute escape probe must not be created"
    );
    // Tool should have reported an error end or the content stays absent.
    let tool_ended = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolEnd { name, .. } if name == "write_file"));
    assert!(
        tool_ended,
        "write_file should still complete as a tool call ({} events)",
        events.len()
    );
    // Primary assertion is the missing probe file (above). Optionally the
    // tool preview mentions the rejection.
    let _ = events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolEnd { name, preview }
                if name == "write_file"
                    && (preview.contains("error")
                        || preview.contains("Error")
                        || preview.contains("outside")
                        || preview.contains("Path"))
        )
    });
}

/// 06 — relative parent escape blocked.
#[tokio::test]
async fn eval_suite_sandbox_blocks_parent_escape() {
    let tmp = tempfile::tempdir().unwrap();
    let parent_probe = tmp
        .path()
        .parent()
        .unwrap()
        .join(format!("outside_escape_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&parent_probe);

    let rel = format!("../{}", parent_probe.file_name().unwrap().to_string_lossy());
    let args = json!({"path": rel, "content": "pwned"}).to_string();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("e1", "write_file", &args),
            sse_text("blocked"),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("escape parent", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    assert!(
        !parent_probe.exists(),
        "parent escape file must not be created"
    );
}

/// 08_skill_use — skill_load then write file per skill.
#[tokio::test]
async fn eval_suite_skill_load_and_write() {
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join(".raven/skills/hello-style");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "# hello-style\n\nCreate src/hello.txt with HELLO_FROM_SKILL_V1\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();

    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("s1", "skill_load", r#"{"name":"hello-style"}"#),
            sse_tool_call(
                "w1",
                "write_file",
                r#"{"path":"src/hello.txt","content":"HELLO_FROM_SKILL_V1\n"}"#,
            ),
            sse_text("Applied hello-style."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("apply hello-style", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let hello = std::fs::read_to_string(tmp.path().join("src/hello.txt")).unwrap();
    assert!(
        hello.contains("HELLO_FROM_SKILL_V1"),
        "skill output file missing marker: {hello}"
    );
}

/// Edits finish uncommitted — the harness has no git_commit tool and does
/// not auto-commit.
#[tokio::test]
async fn eval_suite_edits_finish_uncommitted() {
    let tmp = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(tmp.path())
            .output()
            .unwrap()
    };
    git(&["init"]);
    git(&["config", "user.email", "eval@test"]);
    git(&["config", "user.name", "Eval"]);
    std::fs::write(tmp.path().join("lib.rs"), "pub fn id(n:i32)->i32{n}\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-m", "seed"]);
    let log_before = git(&["log", "--oneline"]);

    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call(
                "w1",
                "write_file",
                r#"{"path":"lib.rs","content":"pub fn square(n:i32)->i32{n*n}\n"}"#,
            ),
            sse_text("Edited square."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("add square", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "git_commit")),
        "git_commit must not run"
    );
    assert!(
        !agent.sandbox.is_working_tree_clean(),
        "tree should stay dirty — no auto-commit"
    );
    let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
    assert!(content.contains("square"), "square should be present");
    let log_after = git(&["log", "--oneline"]);
    assert_eq!(log_before.stdout, log_after.stdout, "HEAD must not move");
}

/// Stall recovery — blank responses retry (eval control-plane theme).
#[tokio::test]
async fn eval_suite_blank_stall_recovers() {
    let tmp = tempfile::tempdir().unwrap();
    let blank = "data: [DONE]\n\n".to_string();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            blank.clone(),
            blank,
            sse_text("Recovered deliverable."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("deliver", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(agent.blank_attempts > 0);
    assert!(events
        .iter()
        .any(|e| { matches!(e, AgentEvent::TextDelta(s) if s.contains("Recovered deliverable")) }));
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

/// Enforced verify gate after edits when verify=true.
#[tokio::test]
async fn eval_suite_verify_gate_after_edit() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    // Minimal cargo project so has_test_runner is true.
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname=\"v\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn x()->i32{1}\n").unwrap();

    let mut s = settings_for(tmp.path());
    s.verify = true;
    s.max_iterations = 6;

    // Edit without tests → verify nudge → model runs run_tests → finishes.
    let mut agent = Agent::new(s).unwrap().with_completion_source(scripted(vec![
        sse_tool_call(
            "w1",
            "write_file",
            r#"{"path":"src/lib.rs","content":"pub fn x()->i32{2}\n"}"#,
        ),
        // Model tries to finish with text only — should be bounced by verify gate.
        sse_text("All done without tests."),
        // After verify reminder, call run_tests then finish.
        sse_tool_call("t1", "run_tests", r#"{}"#),
        sse_text("Verified."),
    ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("edit and stop", tx).await.unwrap();
    let events = drain(&mut rx).await;

    let saw_verify = events
        .iter()
        .any(|e| matches!(e, AgentEvent::VerifyRequired));
    let ran_tests = events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "run_tests"));
    assert!(
        saw_verify || ran_tests,
        "verify gate should fire or tests should run (verify={saw_verify}, tests={ran_tests})"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

/// 07_memory — MEMORY.md is injected into the system prompt.
#[tokio::test]
async fn eval_suite_memory_injected_in_system_prompt() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join(".raven")).unwrap();
    std::fs::write(
        tmp.path().join(".raven/MEMORY.md"),
        "## Conventions\n- Preferred test command: cargo test --workspace -- --nocapture\n",
    )
    .unwrap();

    let agent = Agent::new(settings_for(tmp.path())).unwrap();
    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(
        sys.contains("cargo test --workspace"),
        "system prompt should include memory: {sys}"
    );
}

/// 01_readonly — read_file then answer; no writes.
#[tokio::test]
async fn eval_suite_readonly_read_then_answer() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "pub fn add(a:i32,b:i32)->i32{a+b}\n",
    )
    .unwrap();

    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("r1", "read_file", r#"{"path":"src/lib.rs"}"#),
            sse_text("The function is add; add(2,3)=5."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("what is add(2,3)?", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(events.iter().any(|e| {
        matches!(e, AgentEvent::TextDelta(s) if s.contains('5') && s.to_lowercase().contains("add"))
    }));
    // No mutating tools.
    assert!(!events.iter().any(|e| {
        matches!(
            e,
            AgentEvent::ToolStart { name, .. }
                if name == "write_file" || name == "search_replace" || name == "run_shell"
        )
    }));
}

/// 13_goal_set — goal_set persists to `.raven/state/goal.json` and is
/// injected into the system prompt on the next turn.
#[tokio::test]
async fn eval_suite_goal_set_persists_and_injects() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call(
                "g1",
                "goal_set",
                r#"{"description":"Ship the feature","status":"in_progress"}"#,
            ),
            sse_text("Goal set."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("set a goal", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let goal_path = tmp.path().join(".raven/state/goal.json");
    assert!(goal_path.exists(), "goal.json should be written");
    let goal = crate::state::load_goal(tmp.path()).expect("goal should load");
    assert_eq!(goal.description, "Ship the feature");
    assert_eq!(goal.status, "in_progress");

    let sys = agent.messages[0].content.clone().unwrap_or_default();
    assert!(
        sys.contains("Ship the feature"),
        "same-turn system prompt should include the goal: {sys}"
    );

    // A fresh agent should inject the goal into its system prompt.
    let agent2 = Agent::new(settings_for(tmp.path())).unwrap();
    let sys = agent2.messages[0].content.clone().unwrap_or_default();
    assert!(
        sys.contains("Ship the feature"),
        "system prompt should include the goal: {sys}"
    );
}

/// 14_todo_write — todo_write persists to `.raven/state/todos.json` and is
/// injected into the system prompt on the next turn.
#[tokio::test]
async fn eval_suite_todo_write_persists_and_injects() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call(
                "t1",
                "todo_write",
                r#"{"todos":[{"content":"Do A","status":"in_progress","priority":"high"},{"content":"Do B","status":"pending","priority":"low"}]}"#,
            ),
            sse_text("Todos saved."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("track tasks", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let todos = crate::state::load_todos(tmp.path());
    assert_eq!(todos.len(), 2);
    assert_eq!(todos[0].content, "Do A");
    assert_eq!(todos[0].status, "in_progress");

    let agent2 = Agent::new(settings_for(tmp.path())).unwrap();
    let sys = agent2.messages[0].content.clone().unwrap_or_default();
    assert!(
        sys.contains("Do A") && sys.contains("Do B"),
        "system prompt should include todos: {sys}"
    );
}

/// 15_delegate_task — a delegate_task call returns a bounded summary and the
/// main agent continues. The child is stubbed so this stays offline.
#[tokio::test]
async fn eval_suite_delegate_task_returns_summary() {
    let tmp = tempfile::tempdir().unwrap();
    super::super::parallel::stub_delegate_task("x".repeat(4000));
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call(
                "d1",
                "delegate_task",
                r#"{"description":"explore the codebase"}"#,
            ),
            sse_text("Main agent done."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("delegate", tx).await.unwrap();
    let events = drain(&mut rx).await;

    let preview = events.iter().find_map(|e| match e {
        AgentEvent::ToolEnd { name, preview } if name == "delegate_task" => Some(preview.clone()),
        _ => None,
    });
    let preview = preview.expect("delegate_task should complete");
    assert!(
        preview.starts_with("Sub-agent result:"),
        "expected bounded summary prefix, got {preview}"
    );
    assert!(
        preview.chars().count() <= 2030,
        "summary should be capped, got {} chars",
        preview.chars().count()
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

/// 16_think — think is a read-only no-op that records a thought.
#[tokio::test]
async fn eval_suite_think_records_thought() {
    let tmp = tempfile::tempdir().unwrap();
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("th1", "think", r#"{"thought":"check the edge case"}"#),
            sse_text("Done."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("think", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "think")),
        "think should run"
    );
    assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
}

/// 12_verify_before_done — finishing an edit without tests arms the gate.
#[tokio::test]
async fn eval_suite_verify_before_done_emits_gate() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname=\"v\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "pub fn clamp(n:i32,lo:i32,hi:i32)->i32{ if n<lo{lo}else if n>hi{lo}else{n} }\n",
    )
    .unwrap();

    let mut s = settings_for(tmp.path());
    s.verify = true;
    s.max_iterations = 6;

    let mut agent = Agent::new(s).unwrap().with_completion_source(scripted(vec![
        sse_tool_call(
            "w1",
            "search_replace",
            r#"{"path":"src/lib.rs","old_string":"else if n>hi{lo}","new_string":"else if n>hi{hi}"}"#,
        ),
        sse_text("Fixed."),
        sse_tool_call("t1", "run_tests", r#"{}"#),
        sse_text("Verified."),
    ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("fix clamp", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::VerifyRequired)),
        "finishing an edit without run_tests must emit VerifyRequired"
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "run_tests")));
}

/// Claiming "tests passed" in assistant text without calling `run_tests`
/// must still arm the enforced-verify gate.
#[tokio::test]
async fn eval_suite_claims_tests_passed_without_run_tests_still_gates() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname=\"v\"\nversion=\"0.1.0\"\nedition=\"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(tmp.path().join("src/lib.rs"), "pub fn id(n:i32)->i32{n}\n").unwrap();

    let mut s = settings_for(tmp.path());
    s.verify = true;
    s.max_iterations = 6;

    let mut agent = Agent::new(s).unwrap().with_completion_source(scripted(vec![
        sse_tool_call(
            "w1",
            "write_file",
            r#"{"path":"src/lib.rs","content":"pub fn id(n:i32)->i32{n}\n"}"#,
        ),
        sse_text("All tests passed."),
        sse_tool_call("t1", "run_tests", r#"{}"#),
        sse_text("Verified after the gate."),
    ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("touch lib", tx).await.unwrap();
    let events = drain(&mut rx).await;

    assert!(
        events
            .iter()
            .any(|e| matches!(e, AgentEvent::VerifyRequired)),
        "claiming tests passed in text must not skip the verify gate"
    );
}

/// Large tool output is capped: default `read_file` reports the true line
/// count and does not return the tail of an 800-line file.
#[tokio::test]
async fn eval_suite_large_tool_output_is_capped() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut body = String::from("pub const HEAD: &str = \"MARKER_HEAD_alpha\";\n");
    for i in 0..800 {
        body.push_str(&format!("// filler line {i:04} padding padding padding\n"));
    }
    body.push_str("pub const TAIL: &str = \"MARKER_TAIL_omega\";\n");
    std::fs::write(src.join("big.rs"), &body).unwrap();

    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![
            sse_tool_call("r1", "read_file", r#"{"path":"src/big.rs"}"#),
            sse_text("Read the file."),
        ]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("read big.rs", tx).await.unwrap();
    let events = drain(&mut rx).await;

    let preview = events.iter().find_map(|e| match e {
        AgentEvent::ToolEnd { name, preview } if name == "read_file" => Some(preview.clone()),
        _ => None,
    });
    let preview = preview.expect("read_file should run");
    assert!(
        preview.contains("of 802") || preview.contains("of 801") || preview.contains("lines 1-400"),
        "default read should report the real size / a 400-line window: {preview}"
    );
    assert!(
        !preview.contains("MARKER_TAIL_omega"),
        "default 400-line window must not include the tail marker: {preview}"
    );
}

/// Same-file serial edits in one turn both land (no lost write).
#[tokio::test]
async fn eval_suite_same_file_two_edits_both_land() {
    let tmp = tempfile::tempdir().unwrap();
    let src = tmp.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn double(n: i32) -> i32 { n }\npub fn triple(n: i32) -> i32 { n }\n",
    )
    .unwrap();

    let two_edits = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":\"search_replace\",\"arguments\":{}}}}}]}}}}]}}\n\n\
         data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":1,\"id\":{},\"type\":\"function\",\"function\":{{\"name\":\"search_replace\",\"arguments\":{}}}}}]}}}}]}}\n\n\
         data: [DONE]\n\n",
        json!("e1"),
        json!(r#"{"path":"src/lib.rs","old_string":"pub fn double(n: i32) -> i32 { n }","new_string":"pub fn double(n: i32) -> i32 { n * 2 }"}"#),
        json!("e2"),
        json!(r#"{"path":"src/lib.rs","old_string":"pub fn triple(n: i32) -> i32 { n }","new_string":"pub fn triple(n: i32) -> i32 { n * 3 }"}"#),
    );
    let mut agent = Agent::new(settings_for(tmp.path()))
        .unwrap()
        .with_completion_source(scripted(vec![two_edits, sse_text("Edited both.")]));
    let (tx, mut rx) = mpsc::channel(64);
    agent.run("fix both", tx).await.unwrap();
    let _ = drain(&mut rx).await;

    let content = std::fs::read_to_string(src.join("lib.rs")).unwrap();
    assert!(
        content.contains("n * 2") || content.contains("n*2"),
        "double edit lost: {content}"
    );
    assert!(
        content.contains("n * 3") || content.contains("n*3"),
        "triple edit lost: {content}"
    );
}
