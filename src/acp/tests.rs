//! Offline ACP protocol + in-process client tests.

use super::protocol::{
    agent_capabilities, extract_prompt_text, map_event, permission_allowed, tool_kind, Incoming,
    StopReason,
};
use super::server::{dispatch, AcpServer, FrameWrite};
use crate::agent::AgentEvent;
use crate::config::{Mode, Provider, Settings};
use crate::plan::{Plan, PlanStep, PlanStepStatus};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

fn settings(ws: &std::path::Path) -> Settings {
    Settings {
        model: "fake-model".into(),
        provider: Provider::builtin("ollama").expect("ollama builtin"),
        workspace: ws.to_path_buf(),
        max_iterations: 4,
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

struct BufWriter {
    buf: Arc<StdMutex<Vec<u8>>>,
}

impl FrameWrite for BufWriter {
    fn write_frame(&mut self, msg: &Value) -> anyhow::Result<()> {
        let mut b = self.buf.lock().unwrap();
        serde_json::to_writer(&mut *b, msg)?;
        b.write_all(b"\n")?;
        Ok(())
    }
}

fn frames_from(buf: &Arc<StdMutex<Vec<u8>>>) -> Vec<Value> {
    let raw = String::from_utf8_lossy(&buf.lock().unwrap()).into_owned();
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

async fn send(server: &Arc<Mutex<AcpServer>>, writer: &Arc<Mutex<dyn FrameWrite>>, msg: Value) {
    let incoming = Incoming::parse_line(&msg.to_string()).unwrap();
    dispatch(server.clone(), incoming, writer.clone())
        .await
        .unwrap();
}

fn req(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

#[test]
fn incoming_classifies_request_notification_response() {
    let req = Incoming::parse_line(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#)
        .unwrap();
    assert!(req.is_request());
    assert!(!req.is_notification());

    let note =
        Incoming::parse_line(r#"{"jsonrpc":"2.0","method":"session/cancel","params":{}}"#).unwrap();
    assert!(note.is_notification());

    let resp = Incoming::parse_line(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap();
    assert!(resp.is_response());
}

#[test]
fn extract_prompt_joins_text_and_resources() {
    let blocks = vec![
        json!({"type":"text","text":"Look at"}),
        json!({"type":"resource_link","name":"lib.rs","uri":"file:///ws/lib.rs"}),
        json!({"type":"resource","resource":{"uri":"file:///ws/a.rs","text":"fn a(){}"}}),
    ];
    let text = extract_prompt_text(&blocks).unwrap();
    assert!(text.contains("Look at"));
    assert!(text.contains("[lib.rs](file:///ws/lib.rs)"));
    assert!(text.contains("fn a(){}"));
}

#[test]
fn extract_prompt_rejects_image() {
    let err = extract_prompt_text(&[json!({"type":"image","data":"xx","mimeType":"image/png"})])
        .unwrap_err();
    assert!(err.contains("image"));
}

#[test]
fn capabilities_advertise_no_mcp_and_load_session() {
    let caps = agent_capabilities();
    assert_eq!(caps["loadSession"], true);
    assert_eq!(caps["mcpCapabilities"]["http"], false);
    assert_eq!(caps["mcpCapabilities"]["sse"], false);
    assert_eq!(caps["promptCapabilities"]["image"], false);
    assert!(caps["sessionCapabilities"]["list"].is_object());
}

#[test]
fn tool_kind_covers_core_tools() {
    assert_eq!(tool_kind("read_file"), "read");
    assert_eq!(tool_kind("write_file"), "edit");
    assert_eq!(tool_kind("run_shell"), "execute");
    assert_eq!(tool_kind("web_search"), "fetch");
    assert_eq!(tool_kind("grep"), "search");
}

#[test]
fn map_event_streams_text_and_tools() {
    let mut seq = 0;
    let text = map_event(&AgentEvent::TextDelta("hi".into()), &mut seq);
    assert_eq!(text[0]["sessionUpdate"], "agent_message_chunk");
    assert_eq!(text[0]["content"]["text"], "hi");

    let start = map_event(
        &AgentEvent::ToolStart {
            name: "read_file".into(),
            args: json!({"path":"a.rs"}),
        },
        &mut seq,
    );
    assert_eq!(start[0]["sessionUpdate"], "tool_call");
    assert_eq!(start[0]["kind"], "read");
    assert_eq!(start[0]["toolCallId"], "call_1");

    let end = map_event(
        &AgentEvent::ToolEnd {
            name: "read_file".into(),
            preview: "ok".into(),
        },
        &mut seq,
    );
    assert_eq!(end[0]["sessionUpdate"], "tool_call_update");
    assert_eq!(end[0]["status"], "completed");
}

#[test]
fn map_event_plan_progress() {
    let plan = Plan {
        title: "T".into(),
        steps: vec![PlanStep {
            description: "edit lib".into(),
            status: PlanStepStatus::InProgress,
        }],
        created_at: "now".into(),
    };
    let mut seq = 0;
    let out = map_event(&AgentEvent::PlanProgress(plan), &mut seq);
    assert_eq!(out[0]["sessionUpdate"], "plan");
    assert_eq!(out[0]["entries"][0]["status"], "in_progress");
}

#[test]
fn permission_allowed_reads_option_id() {
    assert!(permission_allowed(&json!({
        "outcome": {"outcome": "selected", "optionId": "allow-once"}
    })));
    assert!(!permission_allowed(&json!({
        "outcome": {"outcome": "cancelled"}
    })));
    assert!(!permission_allowed(&json!({
        "outcome": {"outcome": "selected", "optionId": "reject-once"}
    })));
}

#[test]
fn stop_reason_wire_values() {
    assert_eq!(StopReason::EndTurn.as_str(), "end_turn");
    assert_eq!(StopReason::Cancelled.as_str(), "cancelled");
}

#[tokio::test]
async fn initialize_then_session_new_and_list() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(
            1,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        ),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            2,
            "session/new",
            json!({"cwd": ws.display().to_string(), "mcpServers": []}),
        ),
    )
    .await;
    send(&server, &writer, req(3, "session/list", json!({}))).await;

    let frames = frames_from(&buf);
    assert_eq!(frames[0]["result"]["protocolVersion"], 1);
    assert_eq!(frames[0]["result"]["agentInfo"]["name"], "raven");
    assert_eq!(
        frames[0]["result"]["agentCapabilities"]["loadSession"],
        true
    );
    let sid = frames[1]["result"]["sessionId"].as_str().unwrap();
    assert!(!sid.is_empty());
    assert_eq!(frames[1]["result"]["modes"]["currentModeId"], "agent");
    assert_eq!(frames[2]["result"]["sessions"][0]["sessionId"], sid);
}

#[tokio::test]
async fn rejects_methods_before_initialize() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(
            1,
            "session/new",
            json!({"cwd": ws.display().to_string(), "mcpServers": []}),
        ),
    )
    .await;
    let frames = frames_from(&buf);
    assert_eq!(frames[0]["error"]["code"], -32600);
}

#[tokio::test]
async fn unknown_method_is_minus_32601() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(1, "initialize", json!({"protocolVersion": 1})),
    )
    .await;
    send(&server, &writer, req(2, "nope/nope", json!({}))).await;
    let frames = frames_from(&buf);
    assert_eq!(frames[1]["error"]["code"], -32601);
}

#[tokio::test]
async fn session_new_rejects_relative_cwd() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(1, "initialize", json!({"protocolVersion": 1})),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            2,
            "session/new",
            json!({"cwd": "relative", "mcpServers": []}),
        ),
    )
    .await;
    let frames = frames_from(&buf);
    assert_eq!(frames[1]["error"]["code"], -32602);
}

#[tokio::test]
async fn set_mode_and_close() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(1, "initialize", json!({"protocolVersion": 1})),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            2,
            "session/new",
            json!({"cwd": ws.display().to_string(), "mcpServers": []}),
        ),
    )
    .await;
    let sid = frames_from(&buf)[1]["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();
    send(
        &server,
        &writer,
        req(
            3,
            "session/set_mode",
            json!({"sessionId": sid, "modeId": "chat"}),
        ),
    )
    .await;
    send(
        &server,
        &writer,
        req(4, "session/close", json!({"sessionId": sid})),
    )
    .await;
    let frames = frames_from(&buf);
    assert!(frames[2]["result"].is_object());
    assert!(frames[3]["result"].is_object());
}

#[tokio::test]
async fn prompt_unknown_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(1, "initialize", json!({"protocolVersion": 1})),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            2,
            "session/prompt",
            json!({
                "sessionId": "missing",
                "prompt": [{"type":"text","text":"hi"}]
            }),
        ),
    )
    .await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let frames = frames_from(&buf);
    let last = frames.last().unwrap();
    assert_eq!(last["error"]["code"], -32602);
}

#[tokio::test]
async fn load_replays_history() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let store = crate::session::SessionStore::for_workspace(&ws).unwrap();
    let mut sess = store.create("fake-model").unwrap();
    store
        .append_message(
            &sess,
            &crate::agent::ChatMessage {
                role: "user".into(),
                content: Some("hello history".into()),
                tool_calls: None,
                tool_call_id: None,
            },
        )
        .unwrap();
    store
        .append_message(
            &sess,
            &crate::agent::ChatMessage {
                role: "assistant".into(),
                content: Some("hi back".into()),
                tool_calls: None,
                tool_call_id: None,
            },
        )
        .unwrap();
    store
        .update_summary(&mut sess, Some("hello history".into()))
        .unwrap();

    let server = Arc::new(Mutex::new(AcpServer::new(settings(&ws))));
    let buf = Arc::new(StdMutex::new(Vec::new()));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(BufWriter { buf: buf.clone() }));

    send(
        &server,
        &writer,
        req(1, "initialize", json!({"protocolVersion": 1})),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            2,
            "session/load",
            json!({
                "sessionId": sess.summary.id,
                "cwd": ws.display().to_string(),
                "mcpServers": []
            }),
        ),
    )
    .await;
    let frames = frames_from(&buf);
    let updates: Vec<_> = frames
        .iter()
        .filter(|f| f.get("method").and_then(|m| m.as_str()) == Some("session/update"))
        .collect();
    assert!(updates
        .iter()
        .any(|f| f["params"]["update"]["content"]["text"] == "hello history"));
    assert!(updates
        .iter()
        .any(|f| f["params"]["update"]["content"]["text"] == "hi back"));
    assert!(frames
        .iter()
        .any(|f| f.get("id") == Some(&json!(2)) && f.get("result").is_some()));
}
