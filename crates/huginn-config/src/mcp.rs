//! MCP server configuration and launch specs.
//!
//! The stdio client stays beside the tool dispatcher. These types are
//! configuration: `Settings` stores [`McpConfig`], and [`McpConfig::specs`]
//! builds the [`McpServerSpec`] values the client launches.

use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;

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
