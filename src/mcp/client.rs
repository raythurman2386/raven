//! Stdio JSON-RPC MCP session: spawn, initialize, tools/list, tools/call.

use super::{advertised_name, McpServerSpec, McpTool};
use crate::error::ToolError;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const PROTOCOL_VERSION: &str = "2025-03-26";

/// Connected MCP servers whose tools can be advertised and invoked.
#[derive(Clone, Default)]
pub struct McpHandle {
    tools: Arc<Vec<McpTool>>,
}

impl McpHandle {
    /// Spawn and handshake every spec. Failed servers are logged and skipped.
    pub fn connect(specs: &[McpServerSpec]) -> Self {
        let mut tools = Vec::new();
        let mut seen = HashSet::new();
        for spec in specs {
            match McpSession::start(spec) {
                Ok((session, listed)) => {
                    let session = Arc::new(Mutex::new(session));
                    tracing::info!(
                        server = %spec.name,
                        tools = listed.len(),
                        "MCP server connected"
                    );
                    for t in listed {
                        let name = advertised_name(&spec.name, &t.name);
                        if !seen.insert(name.clone()) {
                            tracing::warn!(
                                tool = %name,
                                "skipping duplicate MCP tool name after sanitizing"
                            );
                            continue;
                        }
                        tools.push(McpTool {
                            advertised_name: name,
                            tool_name: t.name,
                            description: format!("[{}] {}", spec.name, t.description),
                            input_schema: t.input_schema,
                            read_only: t.read_only,
                            session: Arc::clone(&session),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        server = %spec.name,
                        command = %spec.command,
                        "MCP server failed to start: {e}"
                    );
                }
            }
        }
        Self {
            tools: Arc::new(tools),
        }
    }

    /// Whether any tools were discovered.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Number of advertised tools.
    pub fn tool_count(&self) -> usize {
        self.tools.len()
    }

    /// Distinct server prefixes currently connected (sorted).
    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .tools
            .iter()
            .filter_map(|t| t.advertised_name.split("__").next().map(str::to_string))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Whether `name` is an advertised MCP tool.
    pub fn has_tool(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.advertised_name == name)
    }

    /// Whether the named MCP tool is treated as read-only.
    pub fn is_read_only(&self, name: &str) -> bool {
        self.tools
            .iter()
            .find(|t| t.advertised_name == name)
            .map(|t| t.read_only)
            .unwrap_or(false)
    }

    /// ACP `ToolKind` for an advertised MCP tool (`read` or `execute`).
    pub fn acp_kind(&self, name: &str) -> Option<&'static str> {
        let t = self.tools.iter().find(|t| t.advertised_name == name)?;
        Some(if t.read_only { "read" } else { "execute" })
    }

    /// OpenAI tool schemas, filtered when the session is read-only.
    pub fn openai_tools(&self, read_only_session: bool) -> Vec<Value> {
        self.tools
            .iter()
            .filter(|t| !read_only_session || t.read_only)
            .map(McpTool::openai_schema)
            .collect()
    }

    /// Invoke an advertised MCP tool. Unknown names return an error string.
    pub fn call(&self, advertised: &str, args: &Value) -> Result<String, ToolError> {
        let tool = self
            .tools
            .iter()
            .find(|t| t.advertised_name == advertised)
            .ok_or_else(|| ToolError::Other(format!("unknown MCP tool: {advertised}")))?;
        let mut session = tool
            .session
            .lock()
            .map_err(|e| ToolError::Other(format!("MCP session lock: {e}")))?;
        let result = session.call(&tool.tool_name, args)?;
        Ok(flatten_call_result(&result))
    }
}

struct ListedTool {
    name: String,
    description: String,
    input_schema: Value,
    read_only: bool,
}

pub(super) struct McpSession {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Value>,
    next_id: u64,
}

impl McpSession {
    fn start(spec: &McpServerSpec) -> anyhow::Result<(Self, Vec<ListedTool>)> {
        let mut cmd = Command::new(&spec.command);
        cmd.args(&spec.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = spec.cwd.as_ref().or(spec.plugin_root.as_ref()) {
            cmd.current_dir(cwd);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        if let Some(root) = &spec.plugin_root {
            cmd.env("PLUGIN_ROOT", root);
        }
        if let Some(data) = &spec.plugin_data {
            if let Err(e) = std::fs::create_dir_all(data) {
                anyhow::bail!("create PLUGIN_DATA {}: {e}", data.display());
            }
            cmd.env("PLUGIN_DATA", data);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {}: {e}", spec.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("MCP stdin missing"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("MCP stdout missing"))?;
        let stderr = child.stderr.take();
        if let Some(stderr) = stderr {
            let name = spec.name.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    match line {
                        Ok(l) if !l.is_empty() => {
                            tracing::debug!(server = %name, "mcp stderr: {l}");
                        }
                        Err(_) => break,
                        _ => {}
                    }
                }
            });
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        let trimmed = l.trim();
                        if trimmed.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(trimmed) {
                            Ok(v) => {
                                if tx.send(v).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                tracing::debug!("MCP stdout not JSON: {e}: {trimmed}");
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });
        let mut session = Self {
            child,
            stdin,
            rx,
            next_id: 1,
        };
        let init = session.request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "raven",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }),
            INIT_TIMEOUT,
        )?;
        if init.get("error").is_some() {
            anyhow::bail!(
                "initialize error: {}",
                init.get("error").map(|e| e.to_string()).unwrap_or_default()
            );
        }
        session.notify("notifications/initialized", json!({}))?;
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = json!({});
            if let Some(c) = &cursor {
                params["cursor"] = json!(c);
            }
            let listed = session.request("tools/list", params, INIT_TIMEOUT)?;
            let result = listed.get("result").cloned().unwrap_or(listed);
            if let Some(arr) = result.get("tools").and_then(|v| v.as_array()) {
                for t in arr {
                    let name = t
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if name.is_empty() {
                        continue;
                    }
                    let description = t
                        .get("description")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&name)
                        .to_string();
                    let input_schema = t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    let read_only = tool_is_read_only(t.get("annotations"));
                    tools.push(ListedTool {
                        name,
                        description,
                        input_schema,
                        read_only,
                    });
                }
            }
            cursor = result
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            if cursor.is_none() {
                break;
            }
        }
        Ok((session, tools))
    }

    fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
        writeln!(self.stdin, "{}", serde_json::to_string(&msg)?)?;
        self.stdin.flush()?;
        Ok(())
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&msg)?)?;
        self.stdin.flush()?;
        self.read_id(id, timeout)
    }

    fn call(&mut self, name: &str, arguments: &Value) -> Result<Value, ToolError> {
        let args = if arguments.is_null() {
            json!({})
        } else {
            arguments.clone()
        };
        self.request(
            "tools/call",
            json!({"name": name, "arguments": args}),
            CALL_TIMEOUT,
        )
        .map_err(|e| ToolError::Other(format!("MCP {name}: {e}")))
    }

    fn read_id(&mut self, id: u64, timeout: Duration) -> anyhow::Result<Value> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                anyhow::bail!("MCP request timed out waiting for id {id}");
            }
            match self.rx.recv_timeout(remaining) {
                Ok(msg) => {
                    if let Some(remote_id) = json_rpc_id_u64(msg.get("id")) {
                        if remote_id == id {
                            if let Some(err) = msg.get("error") {
                                let message = err
                                    .get("message")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("MCP error");
                                anyhow::bail!("{message}");
                            }
                            return Ok(msg);
                        }
                    }
                    if msg.get("method").is_some() && msg.get("id").is_some() {
                        let remote_id = msg.get("id").cloned().unwrap_or(Value::Null);
                        let reply = json!({
                            "jsonrpc": "2.0",
                            "id": remote_id,
                            "error": {"code": -32601, "message": "Method not found"}
                        });
                        let _ = writeln!(self.stdin, "{}", serde_json::to_string(&reply)?);
                        let _ = self.stdin.flush();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    anyhow::bail!("MCP request timed out waiting for id {id}");
                }
                Err(RecvTimeoutError::Disconnected) => {
                    anyhow::bail!("MCP server closed stdout");
                }
            }
        }
    }
}

impl Drop for McpSession {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn json_rpc_id_u64(id: Option<&Value>) -> Option<u64> {
    match id {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// MCP tools are read-only only when the server sets `readOnlyHint: true`.
pub(super) fn tool_is_read_only(annotations: Option<&Value>) -> bool {
    annotations
        .and_then(|a| a.get("readOnlyHint"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

fn flatten_call_result(msg: &Value) -> String {
    let result = msg.get("result").unwrap_or(msg);
    if result.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        let body = content_text(result);
        return if body.is_empty() {
            "MCP tool error".into()
        } else {
            format!("MCP tool error: {body}")
        };
    }
    let text = content_text(result);
    if text.is_empty() {
        result.to_string()
    } else {
        text
    }
}

fn content_text(result: &Value) -> String {
    let Some(arr) = result.get("content").and_then(|v| v.as_array()) else {
        return result
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    };
    let mut parts = Vec::new();
    for block in arr {
        let kind = block.get("type").and_then(|v| v.as_str()).unwrap_or("text");
        match kind {
            "text" => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
            "resource" | "resource_link" => {
                parts.push(block.to_string());
            }
            _ => {
                if let Some(t) = block.get("text").and_then(|v| v.as_str()) {
                    parts.push(t.to_string());
                }
            }
        }
    }
    parts.join("\n")
}
