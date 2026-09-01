//! Offline ACP protocol + in-process client tests.

use super::protocol::{
    agent_capabilities, auth_methods, extract_prompt_text, map_event, permission_allowed,
    tool_kind, Incoming, StopReason, AUTH_METHOD_ID,
};
use super::server::{dispatch, AcpServer, FrameWrite};
use crate::agent::AgentEvent;
use crate::config::{ConfigFile, Mode, Provider, Settings};
use crate::plan::{Plan, PlanStep, PlanStepStatus};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::Mutex;

/// Skip a test that opens a network socket (provider `/models` fetch or
/// context-window probe) when running under a restrictive outer sandbox that
/// SIGSYS-kills AF_INET sockets. Returns `true` when the test should skip.
fn skip_if_outer_sandbox() -> bool {
    if crate::testutil::outer_sandbox_restrictive() {
        eprintln!("outer sandbox blocks AF_INET sockets; skipping network-dependent ACP test");
        true
    } else {
        false
    }
}

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
        allow_delegate: true,
    }
}

/// Build an ACP server for tests with a default (empty) config file.
fn test_server(ws: &std::path::Path) -> AcpServer {
    AcpServer::new(settings(ws), ConfigFile::default())
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
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
    // Registry gate: authMethods must carry a type "agent" (or "terminal").
    let auth = &frames[0]["result"]["authMethods"];
    assert_eq!(auth[0]["type"], "agent");
    assert_eq!(auth[0]["id"], "agent-auth");
    let sid = frames[1]["result"]["sessionId"].as_str().unwrap();
    assert!(!sid.is_empty());
    assert_eq!(frames[1]["result"]["modes"]["currentModeId"], "agent");
    assert_eq!(frames[2]["result"]["sessions"][0]["sessionId"], sid);
}

#[tokio::test]
async fn session_new_advertises_model_config_option() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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

    let frames = frames_from(&buf);
    let opts = &frames[1]["result"]["configOptions"];
    let opts = opts.as_array().unwrap();
    let mode = opts
        .iter()
        .find(|o| o["id"] == "mode")
        .expect("mode option");
    assert_eq!(mode["type"], "select");
    assert_eq!(mode["category"], "mode");
    assert_eq!(mode["currentValue"], "agent");
    let values: Vec<&str> = mode["options"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|o| o["value"].as_str())
        .collect();
    assert_eq!(values, vec!["plan", "agent", "chat"]);
    let model = opts
        .iter()
        .find(|o| o["id"] == "model")
        .expect("model option");
    assert_eq!(model["type"], "select");
    assert_eq!(model["category"], "model");
    // currentValue is the active provider-qualified model id.
    assert_eq!(model["currentValue"], "ollama/fake-model");
    // Every option is a non-empty provider-qualified id. Don't assert a
    // specific model: the live /models fetch makes the exact list
    // environment-dependent.
    let options = model["options"].as_array().unwrap();
    assert!(!options.is_empty(), "model options must be non-empty");
    for opt in options {
        let value = opt["value"].as_str().expect("option value");
        assert!(
            value.contains('/'),
            "option must be provider-qualified: {value}"
        );
    }
}

#[tokio::test]
async fn rejects_methods_before_initialize() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
async fn set_config_option_switches_mode() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "mode", "value": "chat"}),
        ),
    )
    .await;
    let frames = frames_from(&buf);
    let opts = frames[2]["result"]["configOptions"].as_array().unwrap();
    let mode = opts.iter().find(|o| o["id"] == "mode").unwrap();
    assert_eq!(mode["currentValue"], "chat");

    send(
        &server,
        &writer,
        req(4, "session/resume", json!({"sessionId": sid})),
    )
    .await;
    let frames = frames_from(&buf);
    assert_eq!(frames[3]["result"]["modes"]["currentModeId"], "chat");
    let opts = frames[3]["result"]["configOptions"].as_array().unwrap();
    let mode = opts.iter().find(|o| o["id"] == "mode").unwrap();
    assert_eq!(mode["currentValue"], "chat");
}

#[tokio::test]
async fn prompt_unknown_session_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
    if skip_if_outer_sandbox() {
        return;
    }
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
                usage: None,
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
                usage: None,
            },
        )
        .unwrap();
    store
        .update_summary(&mut sess, Some("hello history".into()))
        .unwrap();

    let server = Arc::new(Mutex::new(test_server(&ws)));
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

#[test]
fn auth_methods_advertise_single_agent_method() {
    let methods = auth_methods();
    let arr = methods.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["id"], "agent-auth");
    assert_eq!(arr[0]["type"], "agent");
    assert!(arr[0]["name"].is_string());
    assert!(arr[0]["description"].is_string());
}

#[tokio::test]
async fn authenticate_accepts_advertised_method_and_rejects_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
        req(2, "authenticate", json!({"methodId": AUTH_METHOD_ID})),
    )
    .await;
    send(
        &server,
        &writer,
        req(3, "authenticate", json!({"methodId": "bogus"})),
    )
    .await;
    let frames = frames_from(&buf);
    assert!(frames[1]["result"].is_object());
    assert_eq!(frames[2]["error"]["code"], -32602);
}

#[tokio::test]
async fn set_model_updates_session_and_rejects_unknown_session() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
            "session/set_model",
            json!({"sessionId": sid, "model": "glm-5.3-flash:cloud"}),
        ),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            4,
            "session/set_model",
            json!({"sessionId": "missing", "model": "glm-5.3-flash:cloud"}),
        ),
    )
    .await;
    let frames = frames_from(&buf);
    assert!(frames[2]["result"].is_object());
    assert_eq!(frames[3]["error"]["code"], -32602);
}

#[tokio::test]
async fn set_model_rejects_missing_or_empty_model() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
        req(3, "session/set_model", json!({"sessionId": sid})),
    )
    .await;
    let frames = frames_from(&buf);
    assert_eq!(frames[2]["error"]["code"], -32602);
}

#[tokio::test]
async fn capabilities_advertise_set_capability() {
    let caps = agent_capabilities();
    assert!(caps["sessionCapabilities"]["set"].is_object());
}

#[tokio::test]
async fn set_config_option_switches_provider_and_model() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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

    // Switch to an opencode-go model via session/set_config_option (model id).
    send(
        &server,
        &writer,
        req(
            3,
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "model", "value": "opencode-go/deepseek-v4-flash"}),
        ),
    )
    .await;
    assert!(frames_from(&buf)[2]["result"].is_object());

    // resume returns configOptions whose currentValue reflects the switch.
    send(
        &server,
        &writer,
        req(4, "session/resume", json!({"sessionId": sid})),
    )
    .await;
    let frames = frames_from(&buf);
    let opts = frames[3]["result"]["configOptions"].as_array().unwrap();
    let model = opts.iter().find(|o| o["id"] == "model").unwrap();
    assert_eq!(model["currentValue"], "opencode-go/deepseek-v4-flash");
}

#[tokio::test]
async fn set_config_option_rejects_unknown_option_and_missing_value() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "bogus", "value": "x"}),
        ),
    )
    .await;
    send(
        &server,
        &writer,
        req(
            4,
            "session/set_config_option",
            json!({"sessionId": sid, "configId": "model"}),
        ),
    )
    .await;
    let frames = frames_from(&buf);
    assert_eq!(frames[2]["error"]["code"], -32602);
    assert_eq!(frames[3]["error"]["code"], -32602);
}

#[tokio::test]
async fn set_model_with_provider_qualifier_switches_provider() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let server = Arc::new(Mutex::new(test_server(&ws)));
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

    // Legacy session/set_model with a provider-qualified id also switches.
    send(
        &server,
        &writer,
        req(
            3,
            "session/set_model",
            json!({"sessionId": sid, "model": "opencode-go/glm-5.3-flash"}),
        ),
    )
    .await;
    assert!(frames_from(&buf)[2]["result"].is_object());

    send(
        &server,
        &writer,
        req(4, "session/resume", json!({"sessionId": sid})),
    )
    .await;
    let frames = frames_from(&buf);
    let opts = frames[3]["result"]["configOptions"].as_array().unwrap();
    let model = opts.iter().find(|o| o["id"] == "model").unwrap();
    assert_eq!(model["currentValue"], "opencode-go/glm-5.3-flash");
}

// ── checkpoint persistence through the ACP prompt path ─────────────────────

/// A minimal streaming-completion mock: serves the scripted SSE bodies in
/// order, then a benign empty completion. Keep-alive aware (one connection,
/// many requests) to match the agent's shared reqwest client.
async fn spawn_completion_mock(bodies: Vec<&'static str>) -> (String, tokio::task::JoinHandle<()>) {
    use std::net::TcpListener as StdTcpListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock listener");
    let addr = std_listener.local_addr().expect("local addr");
    std_listener.set_nonblocking(true).expect("set_nonblocking");
    let listener =
        tokio::net::TcpListener::from_std(std_listener).expect("convert to tokio listener");

    let handle = tokio::spawn(async move {
        let mut next = 0usize;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            loop {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // Read until end of headers.
                let n = loop {
                    match stream.read(&mut tmp).await {
                        Ok(0) => break 0,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break buf.len();
                            }
                        }
                        Err(_) => break 0,
                    }
                };
                if n == 0 {
                    break;
                }
                // Drain the request body (Content-Length) so keep-alive
                // framing stays aligned for the next request.
                let headers_end = buf
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .expect("header terminator");
                let content_length: usize = String::from_utf8_lossy(&buf[..headers_end])
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                let body_start = headers_end + 4;
                while buf.len() < body_start + content_length {
                    match stream.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
                // GETs (provider model probes) get an empty JSON list; POSTs
                // that are not completions (Ollama's /api/show context probe)
                // get an empty result object. Only /chat/completions POSTs
                // consume scripted bodies.
                let is_completion = buf.starts_with(b"POST ")
                    && buf
                        .windows(22)
                        .take(600)
                        .any(|w| w == b"/chat/completions HTTP");
                let body = if is_completion {
                    bodies
                        .get(next)
                        .map(|b| (*b).to_string())
                        .unwrap_or_else(|| "data: [DONE]\n\n".to_string())
                } else {
                    "[]".to_string()
                };
                if is_completion {
                    next += 1;
                }
                let content_type = if is_completion {
                    "text/event-stream"
                } else {
                    "application/json"
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\n\r\n{}",
                    content_type,
                    body.len(),
                    body
                );
                if stream.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });

    (format!("http://{addr}"), handle)
}

fn sse_tool_round(call_id: &str, path: &str) -> String {
    let args = json!({"path": path}).to_string();
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"{call_id}\",\"type\":\"function\",\"function\":{{\"name\":\"read_file\",\"arguments\":{}}}}}]}}}}]}}\n\ndata: [DONE]\n\n",
        serde_json::json!(args)
    )
}

/// Non-streaming JSON body (used by the background title job's completion).
fn json_title_reply() -> String {
    json!({
        "choices": [{"message": {"content": "Test Session Title"}}]
    })
    .to_string()
}

fn sse_text_round(text: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\ndata: [DONE]\n\n",
        serde_json::json!(text)
    )
}

/// A multi-round turn must leave every tool round persisted in
/// messages.jsonl via the ACP Checkpoint arm — a crash mid-turn keeps
/// history. The persistence must also run off the runtime (spawn_blocking),
/// which this exercises implicitly: the mock completes only if the event
/// pump keeps draining while the write runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn checkpoint_persists_each_tool_round_through_acp() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    std::fs::write(ws.join("a.rs"), "fn main() {}\n").unwrap();

    // Request order: (1) the background title job's non-streaming completion,
    // (2..3) the two tool rounds, (4) the final text round.
    let tool_round_1 = sse_tool_round("call_1", "a.rs");
    let tool_round_2 = sse_tool_round("call_2", "a.rs");
    let final_round = sse_text_round("done reading");
    let title_reply = json_title_reply();
    let (base, _mock) = spawn_completion_mock(vec![
        Box::leak(title_reply.into_boxed_str()),
        Box::leak(tool_round_1.into_boxed_str()),
        Box::leak(tool_round_2.into_boxed_str()),
        Box::leak(final_round.into_boxed_str()),
    ])
    .await;

    let mut s = settings(&ws);
    s.provider.base_url = base;
    let server = Arc::new(Mutex::new(AcpServer::new(s, ConfigFile::default())));
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
    let frames = frames_from(&buf);
    let sid = frames[1]["result"]["sessionId"]
        .as_str()
        .unwrap()
        .to_string();

    send(
        &server,
        &writer,
        req(
            3,
            "session/prompt",
            json!({"sessionId": sid, "prompt": [{"type":"text","text":"read a.rs twice"}]}),
        ),
    )
    .await;

    // The prompt response arrives after both tool rounds + checkpoints.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let got = frames_from(&buf)
            .iter()
            .any(|f| f.get("id") == Some(&json!(3)) && f.get("result").is_some());
        if got || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let store = crate::session::SessionStore::for_workspace(&ws).unwrap();
    let persisted = store.load(&sid).unwrap();
    let tool_results = persisted
        .messages
        .iter()
        .filter(|m| m.role == "tool")
        .count();
    assert!(
        tool_results >= 2,
        "both tool rounds must be checkpointed mid-turn, got {tool_results}"
    );
    assert!(
        persisted
            .messages
            .last()
            .and_then(|m| m.content.as_deref())
            .is_some_and(|c| c.contains("done reading")),
        "final text must be persisted"
    );
}
