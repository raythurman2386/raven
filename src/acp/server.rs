//! ACP v1 stdio agent: JSON-RPC over newline-delimited stdin/stdout.
//!
//! Editors spawn `raven --acp` and talk ACP. Raven owns its own sandbox and
//! tools; MCP servers on `session/new` are ignored. Client `fs/*` /
//! `terminal/*` are not used.
//!
//! Stdin stays live during a prompt so `session/cancel` and
//! `session/request_permission` replies can arrive mid-turn.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::agent::{Agent, AgentEvent, ChatMessage};
use crate::config::{Mode, Settings};
use crate::session::{Session, SessionStore};

use super::protocol::{
    agent_capabilities, agent_info, auth_methods, error_code, error_msg, extract_prompt_text,
    map_event, mode_config_option, model_config_option, permission_allowed, result_msg,
    session_modes, session_update, Incoming, StopReason, AUTH_METHOD_ID, PROTOCOL_VERSION,
};

/// Shared writer so request handlers and the event pump can emit frames.
pub trait FrameWrite: Send {
    /// Write one JSON-RPC frame (already a Value) as a single NDJSON line.
    fn write_frame(&mut self, msg: &Value) -> Result<()>;
}

/// Stdout NDJSON writer used by `raven --acp`.
pub struct StdoutWriter<W: Write + Send> {
    inner: W,
}

impl<W: Write + Send> StdoutWriter<W> {
    /// Wrap a stdout (or test) writer.
    pub fn new(inner: W) -> Self {
        Self { inner }
    }
}

impl<W: Write + Send> FrameWrite for StdoutWriter<W> {
    fn write_frame(&mut self, msg: &Value) -> Result<()> {
        serde_json::to_writer(&mut self.inner, msg)?;
        self.inner.write_all(b"\n")?;
        self.inner.flush()?;
        Ok(())
    }
}

struct LiveSession {
    settings: Settings,
    messages: Vec<ChatMessage>,
    store: SessionStore,
    persisted: Session,
    cancel: Option<tokio::task::AbortHandle>,
    /// Serialized transcript persistence. Checkpoint snapshots carry the
    /// FULL history, so writes must land in event order: a stale write
    /// landing after a newer one would rewind messages.jsonl. The worker
    /// also keeps the sync FS work off the runtime (an inline write stalls
    /// the event pump mid-stream for large transcripts).
    saver: Option<tokio::task::JoinHandle<()>>,
}

/// In-memory ACP agent state (sessions + pending client replies).
pub struct AcpServer {
    /// Fully-resolved settings for the active provider (used as the template
    /// for each new session).
    template: Settings,
    /// The full loaded config, so the server can enumerate every configured
    /// provider for the model picker and switch a session onto any of them.
    cfg: crate::config::ConfigFile,
    initialized: bool,
    sessions: HashMap<String, LiveSession>,
    next_rpc_id: AtomicU64,
    pending: HashMap<u64, oneshot::Sender<Value>>,
}

impl AcpServer {
    /// Create a server that clones `template` settings for each new session
    /// and can enumerate providers from `cfg`.
    pub fn new(template: Settings, cfg: crate::config::ConfigFile) -> Self {
        Self {
            template,
            cfg,
            initialized: false,
            sessions: HashMap::new(),
            next_rpc_id: AtomicU64::new(1),
            pending: HashMap::new(),
        }
    }

    fn alloc_rpc_id(&self) -> u64 {
        self.next_rpc_id.fetch_add(1, Ordering::Relaxed)
    }

    fn take_pending(&mut self, id: u64) -> Option<oneshot::Sender<Value>> {
        self.pending.remove(&id)
    }

    fn settings_for_cwd(&self, cwd: &str) -> Result<Settings> {
        let path = PathBuf::from(cwd);
        if !path.is_absolute() {
            anyhow::bail!("cwd must be an absolute path");
        }
        if !path.is_dir() {
            anyhow::bail!("cwd is not a directory: {cwd}");
        }
        let mut settings = self.template.clone();
        settings.workspace = path.canonicalize().unwrap_or(path);
        Ok(settings)
    }
}

/// Build the session `configOptions` array (mode + model selects).
///
/// Mode is advertised as a `category: "mode"` select so clients that ignore
/// the legacy `modes` field (when `configOptions` is present) still get a
/// mode picker. The live `/models` fetches use a blocking reqwest client,
/// which panics if dropped on the async runtime — so run the whole build
/// on a blocking thread.
async fn build_config_options(
    cfg: crate::config::ConfigFile,
    current_model: String,
    current_mode: String,
) -> Value {
    tokio::task::spawn_blocking(move || {
        json!([
            mode_config_option(&current_mode),
            model_config_option(&cfg, &current_model)
        ])
    })
    .await
    .unwrap_or_else(|_| json!([]))
}

fn current_model_id(settings: &Settings) -> String {
    format!("{}/{}", settings.provider.name, settings.model)
}

/// Dispatch one incoming frame. Prompt turns are spawned so stdin stays live.
pub async fn dispatch(
    server: Arc<Mutex<AcpServer>>,
    incoming: Incoming,
    writer: Arc<Mutex<dyn FrameWrite>>,
) -> Result<()> {
    if incoming.is_response() {
        if let Some(id) = incoming.id.as_ref().and_then(|v| v.as_u64()) {
            let tx = server.lock().await.take_pending(id);
            if let Some(tx) = tx {
                let _ = tx.send(incoming.result.unwrap_or(Value::Null));
            }
        }
        return Ok(());
    }

    let method = incoming.method.clone().unwrap_or_default();
    if incoming.is_notification() {
        if method == "session/cancel" {
            if let Some(sid) = incoming
                .params
                .as_ref()
                .and_then(|p| p.get("sessionId"))
                .and_then(|v| v.as_str())
            {
                let mut srv = server.lock().await;
                if let Some(sess) = srv.sessions.get_mut(sid) {
                    if let Some(h) = sess.cancel.take() {
                        h.abort();
                    }
                }
            }
        }
        return Ok(());
    }

    let id = match incoming.id.clone() {
        Some(i) => i,
        None => return Ok(()),
    };
    let params = incoming.params.unwrap_or(Value::Null);

    {
        let srv = server.lock().await;
        if method != "initialize" && !srv.initialized {
            let mut w = writer.lock().await;
            w.write_frame(&error_msg(
                Some(&id),
                error_code::INVALID_REQUEST,
                "initialize must be called first",
            ))?;
            return Ok(());
        }
    }

    if method == "session/prompt" {
        tokio::spawn(run_prompt(server, id, params, writer));
        return Ok(());
    }

    let reply = {
        let mut srv = server.lock().await;
        match method.as_str() {
            "initialize" => on_initialize(&mut srv, &id),
            "authenticate" => {
                let method_id = params
                    .get("methodId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if method_id != AUTH_METHOD_ID {
                    error_msg(
                        Some(&id),
                        error_code::INVALID_PARAMS,
                        &format!("unknown auth method: {method_id}"),
                    )
                } else {
                    // Provider credentials are already resolved in-process from
                    // env / config / `.env`; there is nothing to exchange over
                    // the wire. A successful `authenticate` just acknowledges.
                    result_msg(&id, json!({}))
                }
            }
            "session/new" => on_session_new(&mut srv, &id, &params).await,
            "session/load" => on_session_load(&mut srv, &id, &params, &writer).await,
            "session/resume" => on_session_resume(&mut srv, &id, &params).await,
            "session/list" => on_session_list(&srv, &id, &params),
            "session/close" => on_session_close(&mut srv, &id, &params),
            "session/set_mode" => on_set_mode(&mut srv, &id, &params),
            "session/set_model" => on_set_model(&mut srv, &id, &params).await,
            "session/set_config_option" => on_set_config_option(&mut srv, &id, &params).await,
            _ => error_msg(
                Some(&id),
                error_code::METHOD_NOT_FOUND,
                &format!("method not found: {method}"),
            ),
        }
    };
    writer.lock().await.write_frame(&reply)?;
    Ok(())
}

fn on_initialize(srv: &mut AcpServer, id: &Value) -> Value {
    srv.initialized = true;
    result_msg(
        id,
        json!({
            "protocolVersion": PROTOCOL_VERSION,
            "agentCapabilities": agent_capabilities(),
            "agentInfo": agent_info(),
            "authMethods": auth_methods()
        }),
    )
}

fn persist_new(settings: &Settings) -> Result<(SessionStore, Session)> {
    let store = SessionStore::for_workspace(&settings.workspace)?;
    let session = store.create(&settings.model)?;
    Ok((store, session))
}

async fn on_session_new(srv: &mut AcpServer, id: &Value, params: &Value) -> Value {
    let cwd = match params.get("cwd").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return error_msg(Some(id), error_code::INVALID_PARAMS, "cwd is required"),
    };
    let settings = match srv.settings_for_cwd(cwd) {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INVALID_PARAMS, &e.to_string()),
    };
    let (store, persisted) = match persist_new(&settings) {
        Ok(v) => v,
        Err(e) => return error_msg(Some(id), error_code::INTERNAL, &e.to_string()),
    };
    let sid = persisted.summary.id.clone();
    let mode = settings.mode.label().to_string();
    let config_options =
        build_config_options(srv.cfg.clone(), current_model_id(&settings), mode.clone()).await;
    srv.sessions.insert(
        sid.clone(),
        LiveSession {
            settings,
            messages: Vec::new(),
            store,
            persisted,
            cancel: None,
            saver: None,
        },
    );
    result_msg(
        id,
        json!({
            "sessionId": sid,
            "modes": session_modes(&mode),
            "configOptions": config_options
        }),
    )
}

async fn on_session_load(
    srv: &mut AcpServer,
    id: &Value,
    params: &Value,
    writer: &Arc<Mutex<dyn FrameWrite>>,
) -> Value {
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                "sessionId is required",
            );
        }
    };
    let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    let settings = if cwd.is_empty() {
        srv.template.clone()
    } else {
        match srv.settings_for_cwd(cwd) {
            Ok(s) => s,
            Err(e) => return error_msg(Some(id), error_code::INVALID_PARAMS, &e.to_string()),
        }
    };
    let store = match SessionStore::for_workspace(&settings.workspace) {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INTERNAL, &e.to_string()),
    };
    let persisted = match store.load(&sid) {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INVALID_PARAMS, &e.to_string()),
    };
    {
        let mut w = writer.lock().await;
        for msg in &persisted.messages {
            let (kind, text) = match msg.role.as_str() {
                "user" => (
                    "user_message_chunk",
                    msg.content.clone().unwrap_or_default(),
                ),
                "assistant" => (
                    "agent_message_chunk",
                    msg.content.clone().unwrap_or_default(),
                ),
                _ => continue,
            };
            if text.is_empty() {
                continue;
            }
            let _ = w.write_frame(&session_update(
                &sid,
                json!({
                    "sessionUpdate": kind,
                    "content": {"type": "text", "text": text}
                }),
            ));
        }
    }
    let mode = settings.mode.label().to_string();
    let config_options =
        build_config_options(srv.cfg.clone(), current_model_id(&settings), mode.clone()).await;
    srv.sessions.insert(
        sid,
        LiveSession {
            settings,
            messages: persisted.messages.clone(),
            store,
            persisted,
            cancel: None,
            saver: None,
        },
    );
    result_msg(
        id,
        json!({
            "modes": session_modes(&mode),
            "configOptions": config_options
        }),
    )
}

async fn on_session_resume(srv: &mut AcpServer, id: &Value, params: &Value) -> Value {
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                "sessionId is required",
            );
        }
    };
    if srv.sessions.contains_key(&sid) {
        let sess = &srv.sessions[&sid];
        let mode = sess.settings.mode.label().to_string();
        let config_options = build_config_options(
            srv.cfg.clone(),
            current_model_id(&sess.settings),
            mode.clone(),
        )
        .await;
        return result_msg(
            id,
            json!({
                "modes": session_modes(&mode),
                "configOptions": config_options
            }),
        );
    }
    let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
    let settings = if cwd.is_empty() {
        srv.template.clone()
    } else {
        match srv.settings_for_cwd(cwd) {
            Ok(s) => s,
            Err(e) => return error_msg(Some(id), error_code::INVALID_PARAMS, &e.to_string()),
        }
    };
    let store = match SessionStore::for_workspace(&settings.workspace) {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INTERNAL, &e.to_string()),
    };
    let persisted = match store.load(&sid) {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INVALID_PARAMS, &e.to_string()),
    };
    let mode = settings.mode.label().to_string();
    let config_options =
        build_config_options(srv.cfg.clone(), current_model_id(&settings), mode.clone()).await;
    srv.sessions.insert(
        sid,
        LiveSession {
            settings,
            messages: persisted.messages.clone(),
            store,
            persisted,
            cancel: None,
            saver: None,
        },
    );
    result_msg(
        id,
        json!({
            "modes": session_modes(&mode),
            "configOptions": config_options
        }),
    )
}

fn on_session_list(srv: &AcpServer, id: &Value, params: &Value) -> Value {
    let cwd = params
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| srv.template.workspace.clone());
    let store = match SessionStore::for_workspace(&cwd) {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INTERNAL, &e.to_string()),
    };
    let sessions = match store.list() {
        Ok(s) => s,
        Err(e) => return error_msg(Some(id), error_code::INTERNAL, &e.to_string()),
    };
    let listed: Vec<Value> = sessions
        .into_iter()
        .map(|m| {
            json!({
                "sessionId": m.id,
                "cwd": cwd.display().to_string(),
                "title": m.title,
                "updatedAt": m.updated_at
            })
        })
        .collect();
    result_msg(id, json!({"sessions": listed}))
}

fn on_session_close(srv: &mut AcpServer, id: &Value, params: &Value) -> Value {
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                "sessionId is required",
            );
        }
    };
    if let Some(mut sess) = srv.sessions.remove(sid) {
        if let Some(h) = sess.cancel.take() {
            h.abort();
        }
    }
    result_msg(id, json!({}))
}

fn apply_mode(srv: &mut AcpServer, sid: &str, mode_id: &str) -> Result<(), String> {
    let mode = Mode::from_id(mode_id).ok_or_else(|| format!("unknown mode: {mode_id}"))?;
    match srv.sessions.get_mut(sid) {
        Some(sess) => {
            sess.settings.mode = mode;
            Ok(())
        }
        None => Err("unknown session".into()),
    }
}

fn on_set_mode(srv: &mut AcpServer, id: &Value, params: &Value) -> Value {
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                "sessionId is required",
            );
        }
    };
    let mode_id = params.get("modeId").and_then(|v| v.as_str()).unwrap_or("");
    match apply_mode(srv, &sid, mode_id) {
        Ok(()) => result_msg(id, json!({})),
        Err(e) => error_msg(Some(id), error_code::INVALID_PARAMS, &e),
    }
}

/// Apply a `provider/model` (or plain `model`) selection to a live session's
/// settings. When the value starts with a known provider name followed by `/`,
/// the session switches onto that provider (re-resolving endpoint/key) and
/// sets the model; otherwise the value is treated as a model on the current
/// provider. Re-fetches the context window for the new model and persists the
/// model change. Returns `Ok(())` or an error message for the wire.
async fn apply_model_selection(srv: &mut AcpServer, sid: &str, value: &str) -> Result<(), String> {
    let sess = srv
        .sessions
        .get_mut(sid)
        .ok_or_else(|| "unknown session".to_string())?;

    // Split `<provider>/<model>`. Only treat the prefix as a provider when it
    // is a known provider name; otherwise the whole value is a model on the
    // current provider.
    if let Some((provider_name, model)) = value.split_once('/') {
        if crate::config::is_known_provider(&srv.cfg, provider_name) {
            let new_provider =
                crate::config::resolve_provider(&srv.cfg, Some(provider_name.to_string()));
            sess.settings.provider = new_provider;
            sess.settings.model = model.to_string();
        } else {
            sess.settings.model = value.to_string();
        }
    } else {
        sess.settings.model = value.to_string();
    }

    // Re-fetch the context window for the (possibly switched) provider/model.
    sess.settings.context_window =
        crate::context::fetch_context_window(&sess.settings.provider, &sess.settings.model).await;
    sess.settings.max_tokens = Settings::derived_max_tokens(sess.settings.context_window);
    let _ = sess
        .store
        .update_model(&mut sess.persisted, &sess.settings.model);
    Ok(())
}

async fn on_set_model(srv: &mut AcpServer, id: &Value, params: &Value) -> Value {
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                "sessionId is required",
            );
        }
    };
    let model = match params.get("model").and_then(|v| v.as_str()) {
        Some(m) if !m.trim().is_empty() => m.to_string(),
        _ => {
            return error_msg(Some(id), error_code::INVALID_PARAMS, "model is required");
        }
    };
    match apply_model_selection(srv, &sid, &model).await {
        Ok(()) => result_msg(id, json!({})),
        Err(e) => error_msg(Some(id), error_code::INVALID_PARAMS, &e),
    }
}

/// Handle `session/set_config_option`. Supports `mode` (`plan`/`agent`/`chat`)
/// and `model` (`provider/model` id); other option ids are rejected.
///
/// The response includes the full `configOptions` list with current values,
/// as required by the Session Config Options spec.
async fn on_set_config_option(srv: &mut AcpServer, id: &Value, params: &Value) -> Value {
    let sid = match params.get("sessionId").and_then(|v| v.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                "sessionId is required",
            );
        }
    };
    let config_id = params
        .get("configId")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let value = match params.get("value").and_then(|v| v.as_str()) {
        Some(v) if !v.trim().is_empty() => v.to_string(),
        _ => {
            return error_msg(
                Some(id),
                error_code::INVALID_PARAMS,
                &format!("value is required for config option '{config_id}'"),
            );
        }
    };
    let applied = match config_id {
        "mode" => apply_mode(srv, &sid, &value),
        "model" => apply_model_selection(srv, &sid, &value).await,
        _ => Err(format!("unknown config option: {config_id}")),
    };
    if let Err(e) = applied {
        return error_msg(Some(id), error_code::INVALID_PARAMS, &e);
    }
    let (model, mode) = match srv.sessions.get(&sid) {
        Some(sess) => (
            current_model_id(&sess.settings),
            sess.settings.mode.label().to_string(),
        ),
        None => {
            return error_msg(Some(id), error_code::INVALID_PARAMS, "unknown session");
        }
    };
    let config_options = build_config_options(srv.cfg.clone(), model, mode).await;
    result_msg(id, json!({"configOptions": config_options}))
}

/// Relay an ask_user / shell-permission gate to the ACP client as a
/// `session/request_permission` request, translating the chosen option back
/// into `y` (allowed) or empty (denied / dismissed).
async fn ask_permission(
    writer: &Arc<Mutex<dyn FrameWrite>>,
    server: &Arc<Mutex<AcpServer>>,
    sid: &str,
    title: String,
    reply: oneshot::Sender<String>,
) {
    let rpc_id = {
        let srv = server.lock().await;
        srv.alloc_rpc_id()
    };
    let (perm_tx, perm_rx) = oneshot::channel::<Value>();
    {
        let mut srv = server.lock().await;
        srv.pending.insert(rpc_id, perm_tx);
    }
    let req = json!({
        "jsonrpc": "2.0",
        "id": rpc_id,
        "method": "session/request_permission",
        "params": {
            "sessionId": sid,
            "toolCall": {
                "toolCallId": format!("ask_{rpc_id}"),
                "title": title
            },
            "options": [
                {"optionId": "allow-once", "name": "Yes", "kind": "allow_once"},
                {"optionId": "reject-once", "name": "No", "kind": "reject_once"}
            ]
        }
    });
    if writer.lock().await.write_frame(&req).is_err() {
        let _ = reply.send(String::new());
        return;
    }
    match perm_rx.await {
        Ok(result) if permission_allowed(&result) => {
            let _ = reply.send("y".into());
        }
        _ => {
            let _ = reply.send(String::new());
        }
    }
}

async fn run_prompt(
    server: Arc<Mutex<AcpServer>>,
    id: Value,
    params: Value,
    writer: Arc<Mutex<dyn FrameWrite>>,
) {
    let reply = match run_prompt_inner(server, &params, writer.clone()).await {
        Ok(stop) => result_msg(&id, json!({"stopReason": stop.as_str()})),
        Err(e) => error_msg(Some(&id), error_code::INVALID_PARAMS, &e.to_string()),
    };
    let _ = writer.lock().await.write_frame(&reply);
}

async fn run_prompt_inner(
    server: Arc<Mutex<AcpServer>>,
    params: &Value,
    writer: Arc<Mutex<dyn FrameWrite>>,
) -> Result<StopReason> {
    let sid = params
        .get("sessionId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("sessionId is required"))?
        .to_string();
    let blocks = params
        .get("prompt")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow::anyhow!("prompt is required"))?;
    let text = extract_prompt_text(blocks).map_err(|e| anyhow::anyhow!("{e}"))?;
    let title = text_preview(&text);

    let (settings, preload) = {
        let srv = server.lock().await;
        let sess = srv
            .sessions
            .get(&sid)
            .ok_or_else(|| anyhow::anyhow!("unknown session"))?;
        if sess.persisted.summary.title.is_empty() && !crate::agent::is_title_prompt(&text) {
            let settings = sess.settings.clone();
            let store = sess.store.clone();
            let id = sess.persisted.summary.id.clone();
            let prompt = text.clone();
            tokio::spawn(async move {
                if let Some(generated) =
                    crate::agent::generate_session_title(&settings, &prompt).await
                {
                    let _ = store.apply_title_if_empty(&id, &generated);
                }
            });
        }
        (sess.settings.clone(), sess.messages.clone())
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);
    let prompt_text = text.clone();
    let handle = tokio::spawn(async move {
        let constructed = tokio::task::spawn_blocking({
            let settings = settings.clone();
            let preload = preload.clone();
            move || {
                if preload.is_empty() {
                    Agent::new(settings)
                } else {
                    Agent::with_messages(settings, preload)
                }
            }
        })
        .await
        .unwrap_or_else(|e| Err(anyhow::anyhow!("agent construction cancelled: {e}")));
        let mut agent = constructed?;
        if settings.mode.read_only() {
            agent = agent.plan_only();
        }
        agent.run(&prompt_text, tx).await?;
        Ok::<Vec<ChatMessage>, anyhow::Error>(agent.messages)
    });
    {
        let mut srv = server.lock().await;
        if let Some(sess) = srv.sessions.get_mut(&sid) {
            sess.cancel = Some(handle.abort_handle());
        }
    }

    let mut tool_seq = 0u64;
    let mut stop = StopReason::EndTurn;
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::AskUser { question, reply } => {
                ask_permission(&writer, &server, &sid, question, reply).await;
            }
            AgentEvent::AskPermission { command, reply } => {
                let title = format!("run shell: {command}");
                ask_permission(&writer, &server, &sid, title, reply).await;
            }
            AgentEvent::Done => {
                stop = StopReason::EndTurn;
                break;
            }
            AgentEvent::Error(_) => {
                stop = StopReason::Refusal;
                break;
            }
            AgentEvent::Checkpoint(msgs) => {
                // Persist off the runtime, in order: each Checkpoint snapshot
                // carries the full history, so writes are chained through the
                // session's saver task — an inline write would stall the event
                // pump mid-stream, and un-ordered spawn_blocking writes could
                // land a stale snapshot after a newer one (rewinding the
                // transcript).
                let (store, session, prev_saver, snapshot) = {
                    let mut srv = server.lock().await;
                    match srv.sessions.get_mut(&sid) {
                        Some(sess) => {
                            sess.messages = msgs.clone();
                            (
                                sess.store.clone(),
                                sess.persisted.clone(),
                                sess.saver.take(),
                                msgs.clone(),
                            )
                        }
                        None => continue,
                    }
                };
                let saver = tokio::spawn(async move {
                    if let Some(prev) = prev_saver {
                        let _ = prev.await;
                    }
                    let _ = tokio::task::spawn_blocking(move || {
                        store.save_all_messages(&session, &snapshot)
                    })
                    .await;
                });
                let mut srv = server.lock().await;
                if let Some(sess) = srv.sessions.get_mut(&sid) {
                    sess.saver = Some(saver);
                }
            }
            AgentEvent::SessionTitle(generated) => {
                // Same off-runtime treatment as Checkpoint: the summary write
                // is sync FS under the server lock.
                let (store, mut session) = {
                    let mut srv = server.lock().await;
                    match srv.sessions.get_mut(&sid) {
                        Some(sess) if sess.persisted.summary.title.is_empty() => {
                            (sess.store.clone(), sess.persisted.clone())
                        }
                        _ => continue,
                    }
                };
                let _ = tokio::task::spawn_blocking(move || {
                    store.update_summary(&mut session, Some(generated))
                })
                .await;
            }
            other => {
                let updates = map_event(&other, &mut tool_seq);
                if !updates.is_empty() {
                    let mut w = writer.lock().await;
                    for update in updates {
                        let _ = w.write_frame(&session_update(&sid, update));
                    }
                }
            }
        }
    }

    let finished = handle.await;
    {
        let mut srv = server.lock().await;
        if let Some(sess) = srv.sessions.get_mut(&sid) {
            sess.cancel = None;
        }
    }
    match finished {
        Ok(Ok(messages)) => {
            let (store, session, prev_saver) = {
                let mut srv = server.lock().await;
                match srv.sessions.get_mut(&sid) {
                    Some(sess) => {
                        sess.messages = messages.clone();
                        (
                            sess.store.clone(),
                            sess.persisted.clone(),
                            sess.saver.take(),
                        )
                    }
                    None => return Ok(stop),
                }
            };
            // Chain after any in-flight checkpoint so the final full-history
            // write can never be overtaken by a stale one, and finish before
            // the prompt response is sent (the client may resume immediately).
            if let Some(prev) = prev_saver {
                let _ = prev.await;
            }
            let write = tokio::task::spawn_blocking(move || {
                let mut session = session;
                let _ = store.save_all_messages(&session, &messages);
                store.update_summary(&mut session, Some(title))
            });
            let _ = write.await;
            Ok(stop)
        }
        Ok(Err(_)) => Ok(if stop == StopReason::EndTurn {
            StopReason::Refusal
        } else {
            stop
        }),
        Err(e) if e.is_cancelled() => Ok(StopReason::Cancelled),
        Err(_) => Ok(StopReason::Refusal),
    }
}

fn text_preview(text: &str) -> String {
    text.lines().next().unwrap_or("").chars().take(80).collect()
}

/// Serve ACP on the given reader/writer until stdin EOF.
///
/// Logs a diagnostic to stderr before returning so a clean (exit-0) mid-turn
/// death is never silent — the harness can distinguish "stdin EOF" from a
/// dispatch/write error. `serve_io` only returns `Ok(())` when the stdin line
/// loop ends on EOF; any `Err` here is a stdout write failure that bubbles to
/// `main` (which prints it and exits 1).
pub async fn serve_io<R, W>(server: AcpServer, reader: R, writer: W) -> Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let server = Arc::new(Mutex::new(server));
    let writer: Arc<Mutex<dyn FrameWrite>> = Arc::new(Mutex::new(StdoutWriter::new(writer)));
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in reader.lines() {
            match line {
                Ok(l) => {
                    if line_tx.send(l).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let result = serve_io_loop(server, &mut line_rx, &writer).await;
    match &result {
        Ok(()) => {
            // Clean exit: the stdin line loop ended on EOF. This is the only
            // path that yields exit code 0. Log it so a mid-turn EOF (e.g. the
            // harness closing the pipe) is visible in stderr instead of being
            // silent.
            eprintln!("raven --acp: serve_io exiting cleanly (stdin EOF)");
        }
        Err(e) => {
            eprintln!("raven --acp: serve_io error: {e:#}");
        }
    }
    result
}

async fn serve_io_loop(
    server: Arc<Mutex<AcpServer>>,
    line_rx: &mut mpsc::UnboundedReceiver<String>,
    writer: &Arc<Mutex<dyn FrameWrite>>,
) -> Result<()> {
    while let Some(line) = line_rx.recv().await {
        if line.trim().is_empty() {
            continue;
        }
        match Incoming::parse_line(&line) {
            Ok(incoming) => {
                if let Err(e) = dispatch(server.clone(), incoming, writer.clone()).await {
                    // A stdout write failure (EPIPE after the client died)
                    // ends the process; without this the exit is a silent
                    // clean-return that no harness can diagnose.
                    tracing::error!("ACP dispatch failed; exiting: {e:#}");
                    return Err(e);
                }
            }
            Err(e) => {
                let result =
                    writer
                        .lock()
                        .await
                        .write_frame(&error_msg(None, error_code::PARSE, &e));
                if let Err(write_err) = result {
                    tracing::error!("ACP stdout write failed; exiting: {write_err:#}");
                    return Err(write_err);
                }
            }
        }
    }
    tracing::info!("ACP stdin closed; exiting");
    Ok(())
}

/// Run `raven --acp` on real stdin/stdout.
pub async fn run_stdio(settings: Settings, cfg: crate::config::ConfigFile) -> Result<()> {
    tracing::info!("ACP serving (raven {})", env!("CARGO_PKG_VERSION"));
    let server = AcpServer::new(settings, cfg);
    let result = serve_io(
        server,
        std::io::BufReader::new(std::io::stdin()),
        std::io::stdout(),
    )
    .await;
    match &result {
        Ok(()) => tracing::info!("ACP exited: stdin EOF"),
        Err(e) => tracing::error!("ACP exited with error: {e:#}"),
    }
    result
}
