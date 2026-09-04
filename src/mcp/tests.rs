use super::*;
use serde_json::json;
use std::path::PathBuf;

fn fake_mcp_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_mcp.py")
}

fn python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|bin| {
        std::process::Command::new(bin)
            .arg("-c")
            .arg("import sys")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[test]
fn sanitize_id_replaces_odd_chars() {
    assert_eq!(sanitize_id("sysmetrics"), "sysmetrics");
    assert_eq!(sanitize_id("sys metrics!"), "sys_metrics_");
    assert_eq!(sanitize_id(""), "mcp");
}

#[test]
fn advertised_name_joins_server_and_tool() {
    assert_eq!(
        advertised_name("sysmetrics", "get_cpu_metrics"),
        "sysmetrics__get_cpu_metrics"
    );
}

#[test]
fn from_acp_params_parses_stdio_and_skips_http() {
    let params = json!({
        "mcpServers": [
            {
                "name": "sysmetrics",
                "command": "sysmetrics-mcp",
                "args": ["--temp-unit", "celsius"],
                "env": [{"name": "FOO", "value": "bar"}]
            },
            {
                "type": "http",
                "name": "remote",
                "url": "https://example.com/mcp",
                "headers": []
            },
            {
                "name": "broken"
            }
        ]
    });
    let specs = McpServerSpec::from_acp_params(&params);
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "sysmetrics");
    assert_eq!(specs[0].command, "sysmetrics-mcp");
    assert_eq!(specs[0].args, vec!["--temp-unit", "celsius"]);
    assert_eq!(specs[0].env.get("FOO").map(String::as_str), Some("bar"));
}

#[test]
fn merge_specs_acp_overrides_native_same_name() {
    let native = vec![McpServerSpec::new("sysmetrics", "old")];
    let acp = vec![McpServerSpec::new("sysmetrics", "new").with_args(vec!["--stdio".into()])];
    let merged = merge_specs(native, acp);
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].command, "new");
    assert_eq!(merged[0].args, vec!["--stdio"]);
}

#[test]
fn mcp_config_specs_skip_disabled() {
    let mut servers = HashMap::new();
    servers.insert(
        "on".into(),
        McpServerConfig {
            command: "a".into(),
            args: vec![],
            env: HashMap::new(),
            enabled: None,
        },
    );
    servers.insert(
        "off".into(),
        McpServerConfig {
            command: "b".into(),
            args: vec![],
            env: HashMap::new(),
            enabled: Some(false),
        },
    );
    let cfg = McpConfig { servers };
    let specs = cfg.specs();
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "on");
}

#[test]
fn fake_mcp_roundtrip_echo_and_read_only_filter() {
    let Some(py) = python() else {
        eprintln!("python not available; skipping fake MCP roundtrip");
        return;
    };
    let script = fake_mcp_script();
    if !script.is_file() {
        panic!("missing {}", script.display());
    }
    let handle = McpHandle::connect(&[
        McpServerSpec::new("fake", py).with_args(vec![script.display().to_string()])
    ]);
    assert!(!handle.is_empty(), "fake MCP should list tools");
    assert!(handle.has_tool("fake__echo_text"));
    assert!(handle.has_tool("fake__boom"));
    assert!(handle.has_tool("fake__plain"));
    assert!(handle.is_read_only("fake__echo_text"));
    assert!(!handle.is_read_only("fake__boom"));
    assert!(!handle.is_read_only("fake__plain"));

    let ro = handle.openai_tools(true);
    let names: Vec<&str> = ro
        .iter()
        .filter_map(|t| t.pointer("/function/name")?.as_str())
        .collect();
    assert!(names.contains(&"fake__echo_text"));
    assert!(!names.contains(&"fake__boom"));
    assert!(!names.contains(&"fake__plain"));

    let out = handle
        .call("fake__echo_text", &json!({"text": "hello-mcp"}))
        .unwrap();
    assert_eq!(out, "hello-mcp");
}

#[test]
fn tool_is_read_only_requires_explicit_hint() {
    use super::client::tool_is_read_only;
    assert!(!tool_is_read_only(None));
    assert!(!tool_is_read_only(Some(&json!({"destructiveHint": false}))));
    assert!(tool_is_read_only(Some(&json!({"readOnlyHint": true}))));
    assert!(!tool_is_read_only(Some(&json!({"readOnlyHint": false}))));
}

#[test]
fn missing_command_yields_empty_handle() {
    let handle =
        McpHandle::connect(&[McpServerSpec::new("gone", "/no/such/mcp-server-raven-test")]);
    assert!(handle.is_empty());
    assert!(
        connect_specs(&[McpServerSpec::new("gone", "/no/such/mcp-server-raven-test",)]).is_none()
    );
}
