//! Agent Plugins `mcp.json` → stdio [`McpServerSpec`].
//!
//! Invalid documents disable MCP for that plugin only. HTTP/SSE entries are
//! skipped. Names are `{plugin}__{server}` so they cannot collide with a
//! single-segment native/ACP server id after sanitizing.

use super::{within, Plugin, MCP_SCHEMA, PLUGIN_SCHEMA};
use crate::mcp::McpServerSpec;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

/// Stdio MCP launch specs from every valid plugin visible to `workspace`.
pub fn mcp_specs(workspace: &Path) -> Vec<McpServerSpec> {
    let mut specs = Vec::new();
    for plugin in super::discover_plugins(workspace) {
        specs.extend(load_plugin_mcp(&plugin));
    }
    specs
}

fn schema_version(url: &str) -> Option<&str> {
    url.split("/schemas/").nth(1)?.split('/').next()
}

/// Single-pass expansion of `${PLUGIN_ROOT}` and `${PLUGIN_DATA}` only.
fn expand_placeholders(s: &str, plugin_root: &str, plugin_data: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while !rest.is_empty() {
        if let Some(tail) = rest.strip_prefix("${PLUGIN_ROOT}") {
            out.push_str(plugin_root);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("${PLUGIN_DATA}") {
            out.push_str(plugin_data);
            rest = tail;
        } else {
            let Some(ch) = rest.chars().next() else {
                break;
            };
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(c),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
        }
    }
    out
}

fn path_contained(root: &Path, path: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let resolved = if path.exists() {
        path.canonicalize().ok()
    } else {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        };
        Some(lexical_normalize(&abs))
    };
    resolved.is_some_and(|p| p.starts_with(&root))
}

fn plugin_data_dir(plugin_root: &Path, plugin_name: &str) -> PathBuf {
    plugin_root
        .parent()
        .and_then(|plugins| plugins.parent())
        .map(|raven| raven.join("plugin-data").join(plugin_name))
        .unwrap_or_else(|| plugin_root.join(".plugin-data"))
}

fn load_plugin_mcp(plugin: &Plugin) -> Vec<McpServerSpec> {
    let mcp_path = plugin.root.join("mcp.json");
    if !mcp_path.exists() {
        return Vec::new();
    }
    if !mcp_path.is_file() || !within(&plugin.root, &mcp_path) {
        tracing::warn!(
            plugin = %plugin.name,
            "mcp.json is present but is not a regular file inside the plugin root"
        );
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(&mcp_path) else {
        tracing::warn!(plugin = %plugin.name, "failed to read mcp.json");
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) else {
        tracing::warn!(plugin = %plugin.name, "mcp.json is not valid JSON");
        return Vec::new();
    };
    parse_mcp_document(&plugin.name, &plugin.root, &value)
}

fn parse_mcp_document(
    plugin_name: &str,
    plugin_root: &Path,
    value: &serde_json::Value,
) -> Vec<McpServerSpec> {
    let Some(obj) = value.as_object() else {
        tracing::warn!(plugin = plugin_name, "mcp.json must be a JSON object");
        return Vec::new();
    };
    let schema = obj.get("$schema").and_then(|v| v.as_str());
    match schema {
        Some(MCP_SCHEMA) => {}
        Some(other) => {
            let plugin_ver = schema_version(PLUGIN_SCHEMA);
            let mcp_ver = schema_version(other);
            if mcp_ver != plugin_ver {
                tracing::warn!(
                    plugin = plugin_name,
                    schema = other,
                    "mcp.json Agent Plugins version does not match plugin.json"
                );
            } else {
                tracing::warn!(
                    plugin = plugin_name,
                    schema = other,
                    "unsupported mcp.json $schema"
                );
            }
            return Vec::new();
        }
        None => {
            tracing::warn!(plugin = plugin_name, "mcp.json missing required $schema");
            return Vec::new();
        }
    }
    if !obj.contains_key("mcpServers") {
        tracing::warn!(plugin = plugin_name, "mcp.json missing mcpServers");
        return Vec::new();
    }
    for key in obj.keys() {
        if key != "$schema" && key != "mcpServers" {
            tracing::warn!(
                plugin = plugin_name,
                field = %key,
                "mcp.json has unknown top-level field; disabling MCP for this plugin"
            );
            return Vec::new();
        }
    }
    let Some(servers) = obj.get("mcpServers").and_then(|v| v.as_object()) else {
        tracing::warn!(plugin = plugin_name, "mcpServers must be an object");
        return Vec::new();
    };

    let data = plugin_data_dir(plugin_root, plugin_name);
    if let Err(e) = std::fs::create_dir_all(&data) {
        tracing::warn!(
            plugin = plugin_name,
            path = %data.display(),
            "failed to create PLUGIN_DATA: {e}"
        );
    }
    let root_s = plugin_root.to_string_lossy();
    let data_s = data.to_string_lossy();
    let mut out = Vec::new();
    for (key, entry) in servers {
        match stdio_spec_from_entry(
            plugin_name,
            plugin_root,
            &data,
            &root_s,
            &data_s,
            key,
            entry,
        ) {
            Ok(Some(spec)) => out.push(spec),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(plugin = plugin_name, server = %key, "{e}");
            }
        }
    }
    out
}

fn stdio_spec_from_entry(
    plugin_name: &str,
    plugin_root: &Path,
    plugin_data: &Path,
    root_s: &str,
    data_s: &str,
    key: &str,
    entry: &serde_json::Value,
) -> Result<Option<McpServerSpec>, String> {
    let obj = entry
        .as_object()
        .ok_or_else(|| "server entry must be an object".to_string())?;
    let transport = obj
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing type".to_string())?;
    match transport {
        "stdio" => {}
        "streamable-http" | "sse" => {
            tracing::warn!(
                plugin = plugin_name,
                server = key,
                transport,
                "skipping MCP server: only stdio transport is supported"
            );
            return Ok(None);
        }
        other => return Err(format!("unknown MCP transport: {other}")),
    }

    const STDIO_FIELDS: &[&str] = &["type", "command", "args", "env", "cwd"];
    for field in obj.keys() {
        if !STDIO_FIELDS.contains(&field.as_str()) {
            return Err(format!("unknown field {field} on stdio server"));
        }
    }

    let command = obj
        .get("command")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing command".to_string())?;
    if command.is_empty() || command.contains(char::is_whitespace) {
        return Err("command must be a single executable token".into());
    }
    let resolved_command = if let Some(rel) = command.strip_prefix("./") {
        let path = plugin_root.join(rel);
        if !path_contained(plugin_root, &path) {
            return Err("command resolves outside the plugin root".into());
        }
        path.to_string_lossy().into_owned()
    } else if command.contains('/') || command.contains('\\') {
        return Err("command must be a bare name or a plugin-relative ./ path".into());
    } else {
        command.to_string()
    };

    let args = match obj.get("args") {
        None => Vec::new(),
        Some(v) => {
            let arr = v.as_array().ok_or("args must be an array of strings")?;
            let mut out = Vec::new();
            for item in arr {
                let s = item.as_str().ok_or("args must be an array of strings")?;
                out.push(expand_placeholders(s, root_s, data_s));
            }
            out
        }
    };

    let mut env = HashMap::new();
    if let Some(v) = obj.get("env") {
        let map = v.as_object().ok_or("env must be an object of strings")?;
        for (k, val) in map {
            if k == "PLUGIN_ROOT" || k == "PLUGIN_DATA" {
                return Err("env must not contain PLUGIN_ROOT or PLUGIN_DATA".into());
            }
            let s = val.as_str().ok_or("env values must be strings")?;
            env.insert(k.clone(), expand_placeholders(s, root_s, data_s));
        }
    }

    let cwd = match obj.get("cwd") {
        None => Some(plugin_root.to_path_buf()),
        Some(v) => {
            let raw = v.as_str().ok_or("cwd must be a string")?;
            Some(resolve_cwd(raw, plugin_root, plugin_data, root_s, data_s)?)
        }
    };

    let name = format!("{plugin_name}__{key}");
    Ok(Some(McpServerSpec {
        name,
        command: resolved_command,
        args,
        env,
        cwd,
        plugin_root: Some(plugin_root.to_path_buf()),
        plugin_data: Some(plugin_data.to_path_buf()),
    }))
}

fn resolve_cwd(
    raw: &str,
    plugin_root: &Path,
    plugin_data: &Path,
    root_s: &str,
    data_s: &str,
) -> Result<PathBuf, String> {
    let plugin_form =
        raw == "${PLUGIN_ROOT}" || raw.starts_with("${PLUGIN_ROOT}/") || raw.starts_with("./");
    let data_form = raw == "${PLUGIN_DATA}" || raw.starts_with("${PLUGIN_DATA}/");
    if !plugin_form && !data_form {
        return Err("cwd must be ./…, ${PLUGIN_ROOT}[ /…], or ${PLUGIN_DATA}[ /…]".into());
    }
    let expanded = expand_placeholders(raw, root_s, data_s);
    let path = if let Some(rel) = expanded.strip_prefix("./") {
        plugin_root.join(rel)
    } else {
        PathBuf::from(expanded)
    };
    if plugin_form && !path_contained(plugin_root, &path) {
        return Err("cwd resolves outside the plugin root".into());
    }
    if data_form && !path_contained(plugin_data, &path) {
        return Err("cwd resolves outside PLUGIN_DATA".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::{discover_plugins, PLUGIN_SCHEMA};

    fn write_plugin(root: &Path, name: &str, manifest: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), manifest).unwrap();
        dir
    }

    fn minimal_manifest(name: &str) -> String {
        format!("{{\"$schema\": \"{PLUGIN_SCHEMA}\", \"name\": \"{name}\"}}")
    }

    fn named<'a>(specs: &'a [McpServerSpec], prefix: &str) -> Vec<&'a McpServerSpec> {
        specs
            .iter()
            .filter(|s| s.name == prefix || s.name.starts_with(&format!("{prefix}__")))
            .collect()
    }

    fn mcp_doc(servers: &str) -> String {
        format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": {servers}}}"#)
    }

    fn write_mcp(plugin: &Path, body: &str) {
        std::fs::write(plugin.join("mcp.json"), body).unwrap();
    }

    #[test]
    fn expand_placeholders_is_single_pass() {
        let out = expand_placeholders(
            "${PLUGIN_ROOT}/x ${PLUGIN_DATA}/y ${NOPE} ${PLUGIN_ROOT}",
            "/root",
            "/data",
        );
        assert_eq!(out, "/root/x /data/y ${NOPE} /root");
        let out = expand_placeholders("${PLUGIN_ROOT}", "${PLUGIN_DATA}/nested", "/data");
        assert_eq!(out, "${PLUGIN_DATA}/nested");
        assert_eq!(
            expand_placeholders("café ${PLUGIN_ROOT}", "/r", "/d"),
            "café /r"
        );
    }

    #[test]
    fn mcp_specs_loads_stdio_and_skips_http() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(
            &plugins_dir,
            "raven-test-metrics",
            &minimal_manifest("raven-test-metrics"),
        );
        write_mcp(
            &plugin,
            &mcp_doc(
                r#"{
                    "local": {
                        "type": "stdio",
                        "command": "sysmetrics-mcp",
                        "args": ["--data", "${PLUGIN_ROOT}/cfg"],
                        "env": {"CONFIG": "${PLUGIN_DATA}/c"},
                        "cwd": "${PLUGIN_ROOT}"
                    },
                    "remote": {
                        "type": "streamable-http",
                        "url": "https://example.com/mcp"
                    }
                }"#,
            ),
        );
        let specs = mcp_specs(tmp.path());
        let mine = named(&specs, "raven-test-metrics");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].name, "raven-test-metrics__local");
        assert_eq!(mine[0].command, "sysmetrics-mcp");
        assert_eq!(mine[0].args[0], "--data");
        assert!(mine[0].args[1].ends_with("/cfg"));
        assert!(mine[0].env["CONFIG"].contains("/plugin-data/raven-test-metrics/c"));
        assert_eq!(
            mine[0].cwd.as_ref().and_then(|p| p.canonicalize().ok()),
            plugin.canonicalize().ok()
        );
        assert!(mine[0].plugin_root.is_some());
        assert!(mine[0].plugin_data.is_some());
    }

    #[test]
    fn mcp_specs_unknown_top_level_disables_plugin_mcp() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(
            &plugins_dir,
            "raven-test-x",
            &minimal_manifest("raven-test-x"),
        );
        write_mcp(
            &plugin,
            &format!(r#"{{"$schema": "{MCP_SCHEMA}", "mcpServers": {{}}, "extra": true}}"#),
        );
        let specs = mcp_specs(tmp.path());
        assert!(named(&specs, "raven-test-x").is_empty());
        assert!(discover_plugins(tmp.path())
            .iter()
            .any(|p| p.name == "raven-test-x"));
    }

    #[test]
    fn mcp_specs_schema_mismatch_disables_plugin_mcp() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(
            &plugins_dir,
            "raven-test-y",
            &minimal_manifest("raven-test-y"),
        );
        write_mcp(
            &plugin,
            r#"{"$schema": "https://agent-plugins.org/schemas/9.9.9/mcp.schema.json", "mcpServers": {}}"#,
        );
        assert!(named(&mcp_specs(tmp.path()), "raven-test-y").is_empty());
    }

    #[test]
    fn mcp_specs_rejects_escaping_command_and_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(
            &plugins_dir,
            "raven-test-z",
            &minimal_manifest("raven-test-z"),
        );
        write_mcp(
            &plugin,
            &mcp_doc(
                r#"{
                    "escape-cmd": {"type": "stdio", "command": "../bin/server"},
                    "escape-cwd": {"type": "stdio", "command": "echo", "cwd": "./../.."},
                    "ok": {"type": "stdio", "command": "echo"}
                }"#,
            ),
        );
        let specs = mcp_specs(tmp.path());
        let mine = named(&specs, "raven-test-z");
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0].name, "raven-test-z__ok");
    }

    #[test]
    fn mcp_specs_rejects_reserved_env_override() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(
            &plugins_dir,
            "raven-test-e",
            &minimal_manifest("raven-test-e"),
        );
        write_mcp(
            &plugin,
            &mcp_doc(
                r#"{"bad": {"type": "stdio", "command": "echo", "env": {"PLUGIN_ROOT": "/tmp"}}}"#,
            ),
        );
        assert!(named(&mcp_specs(tmp.path()), "raven-test-e").is_empty());
    }

    #[test]
    fn mcp_plugin_stdio_roundtrip() {
        let Some(py) = ["python3", "python"].into_iter().find(|bin| {
            std::process::Command::new(bin)
                .arg("-c")
                .arg("import sys")
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }) else {
            eprintln!("python not available; skipping plugin MCP roundtrip");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(
            &plugins_dir,
            "raven-test-echo",
            &minimal_manifest("raven-test-echo"),
        );
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_mcp.py");
        std::fs::copy(&script, plugin.join("server.py")).unwrap();
        write_mcp(
            &plugin,
            &mcp_doc(&format!(
                r#"{{"echo": {{"type": "stdio", "command": "{py}", "args": ["${{PLUGIN_ROOT}}/server.py"]}}}}"#
            )),
        );
        let specs = mcp_specs(tmp.path());
        let mine: Vec<McpServerSpec> = named(&specs, "raven-test-echo")
            .into_iter()
            .cloned()
            .collect();
        assert_eq!(mine.len(), 1);
        let handle = crate::mcp::McpHandle::connect(&mine);
        assert!(handle.has_tool("raven-test-echo__echo__echo_text"));
        let out = handle
            .call(
                "raven-test-echo__echo__echo_text",
                &serde_json::json!({"text": "from-plugin"}),
            )
            .unwrap();
        assert_eq!(out, "from-plugin");
    }
}
