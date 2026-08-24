//! ACP v1 wire helpers: JSON-RPC envelopes, capabilities, and event mapping.
//!
//! Kept small on purpose — Raven speaks a documented subset of ACP v1
//! (no MCP, no client FS/terminal, text prompts only). See
//! <https://agentclientprotocol.com/protocol/overview>.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::AgentEvent;
use crate::plan::{Plan, PlanStepStatus};

/// ACP protocol major version Raven implements.
pub const PROTOCOL_VERSION: u16 = 1;

/// JSON-RPC error codes used by this adapter.
pub mod error_code {
    pub const PARSE: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL: i32 = -32603;
}

/// Why a `session/prompt` turn stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTurnRequests,
    Cancelled,
    Refusal,
}

impl StopReason {
    /// Wire value for `PromptResponse.stopReason`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTurnRequests => "max_turn_requests",
            Self::Cancelled => "cancelled",
            Self::Refusal => "refusal",
        }
    }
}

/// Incoming JSON-RPC frame (request, notification, or response).
#[derive(Debug, Deserialize)]
pub struct Incoming {
    #[serde(default)]
    pub jsonrpc: Option<String>,
    #[serde(default)]
    pub id: Option<Value>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub params: Option<Value>,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub error: Option<Value>,
}

impl Incoming {
    /// Parse one NDJSON line into an [`Incoming`] frame.
    pub fn parse_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| e.to_string())
    }

    /// Whether this frame is a JSON-RPC request (has method + id).
    pub fn is_request(&self) -> bool {
        self.method.is_some() && self.id.is_some()
    }

    /// Whether this frame is a notification (has method, no id).
    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }

    /// Whether this frame is a response to a request we sent.
    pub fn is_response(&self) -> bool {
        self.method.is_none() && self.id.is_some()
    }
}

/// Agent capabilities advertised in `initialize`.
pub fn agent_capabilities() -> Value {
    json!({
        "loadSession": true,
        "promptCapabilities": {
            "image": false,
            "audio": false,
            "embeddedContext": true
        },
        "mcpCapabilities": {
            "http": false,
            "sse": false
        },
        "sessionCapabilities": {
            "list": {},
            "close": {},
            "resume": {},
            "set": {}
        }
    })
}

/// Auth methods advertised in `initialize`.
///
/// Raven authenticates to its model provider using credentials already
/// resolved in-process (env vars / config / `.env`) before the ACP loop
/// starts — it has no interactive OAuth flow to run over the wire. So it
/// advertises a single `agent`-type method: the agent handles authentication
/// itself. The `agent` type (ACP default when `type` is omitted) is what the
/// ACP registry's `--auth-check` requires, and mirrors how grok-build
/// advertises its `xai.api_key` method.
pub const AUTH_METHOD_ID: &str = "agent-auth";

/// The single advertised auth method.
pub fn auth_methods() -> Value {
    json!([{
        "id": AUTH_METHOD_ID,
        "name": "Agent auth",
        "description": "Authenticates with the provider credentials configured in-process (OLLAMA_API_KEY / RAVEN_API_KEY)",
        "type": "agent"
    }])
}

/// Implementation info sent in `initialize`.
pub fn agent_info() -> Value {
    json!({
        "name": "raven",
        "title": "Raven",
        "version": env!("CARGO_PKG_VERSION")
    })
}

/// Session modes Raven can switch between via `session/set_mode`.
///
/// Kept for clients that only speak the older modes API. Clients that
/// support [`configOptions`](https://agentclientprotocol.com/protocol/session-config-options)
/// SHOULD ignore this field and use the `mode` select option instead.
pub fn session_modes(current: &str) -> Value {
    json!({
        "currentModeId": current,
        "availableModes": [
            {"id": "plan", "name": "Plan", "description": "Propose a plan, then execute after approval"},
            {"id": "agent", "name": "Agent", "description": "Full tools, no plan step"},
            {"id": "chat", "name": "Chat", "description": "Read-only Q&A"}
        ]
    })
}

/// ACP `mode` select config option (category `mode`).
///
/// Session Config Options supersede the older `modes` field. When both are
/// present, modern clients (Zed, etc.) use `configOptions` exclusively — so
/// without this option the editor has no mode picker and the session stays
/// on whatever `Settings.mode` defaulted to (plan).
pub fn mode_config_option(current: &str) -> Value {
    json!({
        "id": "mode",
        "name": "Session Mode",
        "description": "Plan (propose then execute), Agent (full tools), or Chat (read-only)",
        "category": "mode",
        "type": "select",
        "currentValue": current,
        "options": [
            {"value": "plan", "name": "Plan", "description": "Propose a plan, then execute after approval"},
            {"value": "agent", "name": "Agent", "description": "Full tools, no plan step"},
            {"value": "chat", "name": "Chat", "description": "Read-only Q&A"}
        ]
    })
}

/// Cap on the number of models advertised per provider in the ACP model
/// picker, so a huge catalog (e.g. OpenRouter-scale) can't flood the editor
/// dropdown. Mirrors Hermes's ACP model-inventory cap.
pub const ACP_MAX_MODELS_PER_PROVIDER: usize = 200;

/// Build the ACP `model` select config option, enumerating every configured
/// provider's models as provider-qualified ids (`<provider>/<model>`).
///
/// `current_value` is the active `provider/model` id (or a plain model on the
/// active provider). Each provider's list comes from its live `/models`
/// endpoint when reachable, else its curated `fallback_models`. Options are
/// sorted by provider then model and deduped, and per-provider bounded by
/// [`ACP_MAX_MODELS_PER_PROVIDER`].
pub fn model_config_option(cfg: &crate::config::ConfigFile, current_value: &str) -> Value {
    let mut options: Vec<Value> = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for name in crate::config::known_provider_names(cfg) {
        // Resolve the provider (config table or built-in) to reach its
        // endpoint + key, then fetch its live model list.
        let provider = crate::config::resolve_provider(cfg, Some(name.clone()));
        let models = crate::tui::fetch_live_provider_models(&provider);
        let models = if models.is_empty() {
            crate::config::fallback_models(&name)
        } else {
            models
        };
        for model in models.into_iter().take(ACP_MAX_MODELS_PER_PROVIDER) {
            let value = format!("{name}/{model}");
            if seen.insert(value.clone()) {
                options.push(json!({"value": value, "name": format!("{name} · {model}")}));
            }
        }
    }

    options.sort_by_key(|o| o["name"].as_str().unwrap_or_default().to_string());

    json!({
        "id": "model",
        "name": "Model",
        "category": "model",
        "type": "select",
        "currentValue": current_value,
        "options": options
    })
}

/// Successful JSON-RPC result.
pub fn result_msg(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// JSON-RPC error response.
pub fn error_msg(id: Option<&Value>, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id.cloned().unwrap_or(Value::Null),
        "error": {"code": code, "message": message}
    })
}

/// `session/update` notification.
pub fn session_update(session_id: &str, update: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/update",
        "params": {
            "sessionId": session_id,
            "update": update
        }
    })
}

/// Flatten ACP prompt content blocks into a single user string.
///
/// Baseline: `text` and `resource_link`. `resource` (embeddedContext) is
/// accepted. `image` / `audio` return an error so the caller can reject the
/// prompt.
pub fn extract_prompt_text(prompt: &[Value]) -> Result<String, String> {
    let mut parts = Vec::new();
    for block in prompt {
        let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "text" => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
            "resource_link" => {
                let name = block
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("resource");
                let uri = block.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                parts.push(format!("[{name}]({uri})"));
            }
            "resource" => {
                if let Some(text) = block.pointer("/resource/text").and_then(|v| v.as_str()) {
                    let uri = block
                        .pointer("/resource/uri")
                        .and_then(|v| v.as_str())
                        .unwrap_or("embedded");
                    parts.push(format!("<{uri}>\n{text}\n</{uri}>"));
                }
            }
            "image" => return Err("image prompts are not supported".into()),
            "audio" => return Err("audio prompts are not supported".into()),
            other => return Err(format!("unsupported content block type: {other}")),
        }
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        Err("prompt contained no usable text".into())
    } else {
        Ok(text)
    }
}

/// Map a Raven tool name to an ACP `ToolKind`.
pub fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" => "read",
        "write_file" | "search_replace" | "apply_patch" => "edit",
        "grep" | "search_code" | "memory_search" | "skill_search" => "search",
        "run_shell" | "run_tests" | "run_lint" => "execute",
        "git_status" | "git_diff" | "git_log" => "read",
        "web_search" | "web_fetch" => "fetch",
        "todo_write" | "memory_update" | "skill_load" | "goal_set" | "think" => "think",
        "delegate_task" => "other",
        "ask_user" => "other",
        _ => "other",
    }
}

/// Map a Raven [`Plan`] to an ACP `plan` update.
pub fn plan_update(plan: &Plan) -> Value {
    let entries: Vec<Value> = plan
        .steps
        .iter()
        .map(|s| {
            let status = match s.status {
                PlanStepStatus::Pending => "pending",
                PlanStepStatus::InProgress => "in_progress",
                PlanStepStatus::Completed | PlanStepStatus::Skipped => "completed",
            };
            json!({
                "content": s.description,
                "priority": "medium",
                "status": status
            })
        })
        .collect();
    json!({
        "sessionUpdate": "plan",
        "entries": entries
    })
}

/// Map one [`AgentEvent`] to zero or more `session/update` payloads.
///
/// `AskUser` is not mapped here — the server turns it into
/// `session/request_permission`. `Done` / `Error` end the turn.
pub fn map_event(event: &AgentEvent, tool_seq: &mut u64) -> Vec<Value> {
    match event {
        AgentEvent::TextDelta(t) if !t.is_empty() => vec![json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": t}
        })],
        AgentEvent::ToolStart { name, args } => {
            *tool_seq += 1;
            let id = format!("call_{tool_seq}");
            vec![json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": name,
                "kind": tool_kind(name),
                "status": "in_progress",
                "rawInput": args
            })]
        }
        AgentEvent::ToolEnd { name, preview } => {
            let id = format!("call_{tool_seq}");
            vec![json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": id,
                "title": name,
                "status": "completed",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": preview}
                }]
            })]
        }
        AgentEvent::PlanProgress(plan) => vec![plan_update(plan)],
        AgentEvent::Compacted {
            before_tokens,
            after_tokens,
            note: _,
        } => vec![json!({
            "sessionUpdate": "usage_update",
            "used": after_tokens,
            "size": before_tokens
        })],
        AgentEvent::RecoveryPatch { path, reason } => vec![json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {
                "type": "text",
                "text": format!("\n[recovery patch: {path} ({reason})]\napply: git apply {path}\n")
            }
        })],
        AgentEvent::TextDelta(_)
        | AgentEvent::Iteration(_)
        | AgentEvent::Retry { .. }
        | AgentEvent::VerifyRequired
        | AgentEvent::AskUser { .. }
        | AgentEvent::Done
        | AgentEvent::Error(_) => Vec::new(),
    }
}

/// Parse a `session/request_permission` client response into a yes/no answer.
pub fn permission_allowed(result: &Value) -> bool {
    let outcome = result.get("outcome");
    let kind = outcome
        .and_then(|o| o.get("outcome"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if kind == "cancelled" {
        return false;
    }
    let option = outcome
        .and_then(|o| o.get("optionId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    option == "allow-once" || option == "allow-always" || option == "yes"
}
