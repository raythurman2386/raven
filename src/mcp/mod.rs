//! Thin MCP client: stdio JSON-RPC servers whose tools join the agent loop.
//!
//! ACP v1 requires agents to connect to `mcpServers` on `session/new` /
//! `session/load` / `session/resume` (stdio transport is mandatory; HTTP/SSE
//! are optional capabilities Raven does not advertise). The same client is
//! used for native `[mcp.servers]` entries in `config.toml`, so TUI and
//! headless sessions can attach servers such as `sysmetrics-mcp`.

mod client;

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

pub use client::McpHandle;

/// Native `[mcp]` table from `config.toml`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct McpConfig {
    /// Named stdio servers. Keys are the server ids used in tool names
    /// (`{id}__{tool}`).
    #[serde(default)]
    pub servers: HashMap<String, McpServerConfig>,
}

/// One stdio MCP server in `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct McpServerConfig {
    /// Executable to spawn (looked up on `PATH` when not absolute).
    pub command: String,
    /// Arguments passed to `command`.
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables for the child.
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// When `false`, the server is listed but not started. Default: enabled.
    #[serde(default)]
    pub enabled: Option<bool>,
}

impl McpConfig {
    /// Specs for servers that are enabled.
    pub fn specs(&self) -> Vec<McpServerSpec> {
        let mut specs: Vec<McpServerSpec> = self
            .servers
            .iter()
            .filter(|(_, cfg)| cfg.enabled.unwrap_or(true))
            .map(|(name, cfg)| {
                McpServerSpec::new(name.clone(), cfg.command.clone())
                    .with_args(cfg.args.clone())
                    .with_env(cfg.env.clone())
            })
            .collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Overlay `other` onto `self` by server name (`other` wins on conflict).
    pub fn merge(mut self, other: Self) -> Self {
        self.servers.extend(other.servers);
        self
    }
}

/// Launch spec for one stdio MCP server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    /// Human-readable id (ACP `name` / config table key / plugin-qualified id).
    pub name: String,
    /// Executable path or `PATH` name.
    pub command: String,
    /// Command-line arguments.
    pub args: Vec<String>,
    /// Extra env vars (merged onto the inherited environment).
    pub env: HashMap<String, String>,
    /// Working directory. Plugin stdio servers default to the plugin root.
    pub cwd: Option<std::path::PathBuf>,
    /// Absolute plugin root; when set, exported as `PLUGIN_ROOT`.
    pub plugin_root: Option<std::path::PathBuf>,
    /// Absolute plugin data dir; when set, exported as `PLUGIN_DATA`.
    pub plugin_data: Option<std::path::PathBuf>,
}

impl McpServerSpec {
    /// A stdio spec with no extra args/env/plugin context.
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            plugin_root: None,
            plugin_data: None,
        }
    }

    /// Set command-line arguments.
    pub fn with_args(mut self, args: Vec<String>) -> Self {
        self.args = args;
        self
    }

    /// Set extra environment variables.
    pub fn with_env(mut self, env: HashMap<String, String>) -> Self {
        self.env = env;
        self
    }
}

impl McpServerSpec {
    /// Parse ACP `mcpServers` from a session lifecycle request.
    ///
    /// Stdio entries (no `type`, or `type: "stdio"`) are returned. HTTP/SSE
    /// entries are skipped because Raven does not advertise those transports.
    pub fn from_acp_params(params: &Value) -> Vec<Self> {
        let Some(arr) = params.get("mcpServers").and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for entry in arr {
            let Some(obj) = entry.as_object() else {
                continue;
            };
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if name.is_empty() {
                continue;
            }
            let transport = obj.get("type").and_then(|v| v.as_str()).unwrap_or("stdio");
            if transport != "stdio" {
                tracing::warn!(
                    name,
                    transport,
                    "skipping MCP server: only stdio transport is supported"
                );
                continue;
            }
            let Some(command) = obj.get("command").and_then(|v| v.as_str()) else {
                tracing::warn!(name, "skipping MCP server: missing command");
                continue;
            };
            if command.trim().is_empty() {
                continue;
            }
            let args = obj
                .get("args")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let mut env = HashMap::new();
            if let Some(vars) = obj.get("env").and_then(|v| v.as_array()) {
                for var in vars {
                    let n = var.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let val = var.get("value").and_then(|v| v.as_str()).unwrap_or("");
                    if !n.is_empty() {
                        env.insert(n.to_string(), val.to_string());
                    }
                }
            }
            out.push(
                Self::new(name.to_string(), command.to_string())
                    .with_args(args)
                    .with_env(env),
            );
        }
        out
    }
}

/// Merge two spec lists by server name. Entries in `overlay` win on conflict.
pub fn merge_specs(base: Vec<McpServerSpec>, overlay: Vec<McpServerSpec>) -> Vec<McpServerSpec> {
    let mut map = std::collections::BTreeMap::new();
    for spec in base {
        map.insert(spec.name.clone(), spec);
    }
    for spec in overlay {
        map.insert(spec.name.clone(), spec);
    }
    map.into_values().collect()
}

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
    arr.extend(extra);
    Value::Array(arr)
}

#[cfg(test)]
mod tests;
