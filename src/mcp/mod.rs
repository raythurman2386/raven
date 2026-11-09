//! Thin MCP client: stdio JSON-RPC servers whose tools join the agent loop.
//!
//! ACP v1 requires agents to connect to `mcpServers` on `session/new` /
//! `session/load` / `session/resume` (stdio transport is mandatory; HTTP/SSE
//! are optional capabilities Raven does not advertise). The same client is
//! used for native `[mcp.servers]` entries in `config.toml`, so TUI and
//! headless sessions can attach servers such as `sysmetrics-mcp`.

mod client;

use serde_json::{json, Value};
use std::sync::Arc;

pub use client::McpHandle;

pub use raven_config::{merge_specs, McpConfig, McpServerConfig, McpServerSpec};

/// Connect every spec, skipping servers that fail to start or handshake.
pub fn connect_specs(specs: &[McpServerSpec]) -> Option<McpHandle> {
    if specs.is_empty() {
        return None;
    }
    let handle = McpHandle::connect(specs);
    if handle.is_empty() {
        None
    } else {
        Some(handle)
    }
}

/// Sanitize a server or tool id for OpenAI function names (`[A-Za-z0-9_-]`).
pub fn sanitize_id(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        "mcp".into()
    } else {
        s
    }
}

/// Advertised tool name: `{server}__{tool}`.
pub fn advertised_name(server: &str, tool: &str) -> String {
    format!("{}__{}", sanitize_id(server), sanitize_id(tool))
}

/// One discovered MCP tool, ready to advertise and call.
#[derive(Clone)]
pub struct McpTool {
    /// Name shown to the model (`server__tool`).
    pub advertised_name: String,
    /// MCP `tools/call` name on the wire.
    pub tool_name: String,
    /// Description forwarded to the model.
    pub description: String,
    /// JSON Schema `inputSchema` from the server.
    pub input_schema: Value,
    /// True when the server marked the tool read-only (or did not mark it
    /// destructive). Read-only tools stay available in plan/chat mode.
    pub read_only: bool,
    session: Arc<std::sync::Mutex<client::McpSession>>,
}

impl McpTool {
    /// OpenAI-style function-calling schema for this tool.
    pub fn openai_schema(&self) -> Value {
        let parameters = if self.input_schema.is_object() {
            self.input_schema.clone()
        } else {
            json!({"type": "object", "properties": {}})
        };
        json!({
            "type": "function",
            "function": {
                "name": self.advertised_name,
                "description": self.description,
                "parameters": parameters
            }
        })
    }
}

/// Append MCP tools onto an existing OpenAI tools array.
pub fn merge_tool_defs(base: Value, mcp: &McpHandle, read_only_session: bool) -> Value {
    let extra = mcp.openai_tools(read_only_session);
    if extra.is_empty() {
        return base;
    }
    let mut arr = base.as_array().cloned().unwrap_or_default();
    let mut extra = extra;
    extra.sort_by_key(tool_fn_name);
    arr.extend(extra);
    Value::Array(arr)
}

fn tool_fn_name(tool: &Value) -> String {
    tool.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests;
