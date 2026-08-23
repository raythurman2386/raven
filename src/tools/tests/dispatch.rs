//! Tests for tool dispatch and the read-only / full / chat toolsets.

use crate::tools::{
    chat_tool_definitions, dispatch, plan_tool_definitions, tool_definitions, Sandbox,
};

use super::sandbox;

#[test]
fn dispatch_rejects_missing_required_field() {
    let sb = sandbox();
    let result = dispatch(&sb, "read_file", &serde_json::json!({}), false).unwrap();
    assert!(
        result.contains("path"),
        "missing path should be rejected: {result}"
    );
}

#[test]
fn dispatch_rejects_empty_shell_command() {
    let sb = sandbox();
    let result = dispatch(&sb, "run_shell", &serde_json::json!({"command": ""}), false).unwrap();
    assert!(
        result.contains("command"),
        "empty command should be rejected: {result}"
    );
}

#[test]
fn dispatch_unknown_tool_returns_error() {
    let sb = sandbox();
    let result = dispatch(&sb, "nonexistent_tool", &serde_json::json!({}), false).unwrap();
    assert!(result.contains("Unknown tool"));
}

#[test]
fn dispatch_read_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("test.txt"), "content").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let result = dispatch(
        &sb,
        "read_file",
        &serde_json::json!({"path": "test.txt"}),
        false,
    )
    .unwrap();
    assert!(result.contains("content"));
}

#[test]
fn dispatch_write_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let result = dispatch(
        &sb,
        "write_file",
        &serde_json::json!({"path": "out.txt", "content": "data"}),
        false,
    )
    .unwrap();
    assert!(result.contains("Wrote"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("out.txt")).unwrap(),
        "data"
    );
}

#[test]
fn plan_tool_definitions_are_read_only() {
    let defs = plan_tool_definitions();
    let arr = defs.as_array().expect("plan tools should be an array");
    assert!(!arr.is_empty(), "plan toolset should not be empty");

    let names: Vec<String> = arr
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();

    for expected in [
        "list_dir",
        "read_file",
        "grep",
        "search_code",
        "git_status",
        "web_search",
        "web_fetch",
        "skill_search",
        "skill_load",
        "memory_search",
        "think",
    ] {
        assert!(
            names.iter().any(|n| n == expected),
            "plan toolset should include {expected}, got {names:?}"
        );
    }

    let forbidden = [
        "write_file",
        "search_replace",
        "run_shell",
        "todo_write",
        "goal_set",
        "delegate_task",
        "memory_update",
        "apply_patch",
        "run_tests",
    ];
    for bad in forbidden {
        assert!(
            !names.iter().any(|n| n == bad),
            "plan toolset must not include {bad}, got {names:?}"
        );
    }
}

#[test]
fn ask_user_in_full_toolset_not_plan_toolset() {
    let full = tool_definitions();
    let full_names: Vec<String> = full
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        full_names.iter().any(|n| n == "ask_user"),
        "full toolset should include ask_user, got {full_names:?}"
    );

    let plan = plan_tool_definitions();
    let plan_names: Vec<String> = plan
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        !plan_names.iter().any(|n| n == "ask_user"),
        "ask_user is interactive and must not be advertised during planning"
    );
}

#[test]
fn chat_toolset_includes_ask_user() {
    let chat = chat_tool_definitions();
    let names: Vec<String> = chat
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        names.iter().any(|n| n == "ask_user"),
        "chat toolset should include ask_user, got {names:?}"
    );
}

#[test]
fn chat_toolset_excludes_write_tools() {
    let chat = chat_tool_definitions();
    let names: Vec<String> = chat
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    let forbidden = [
        "write_file",
        "search_replace",
        "run_shell",
        "todo_write",
        "goal_set",
        "delegate_task",
        "memory_update",
        "apply_patch",
        "run_tests",
        "git_commit",
    ];
    for bad in forbidden {
        assert!(
            !names.iter().any(|n| n == bad),
            "chat toolset must not include {bad}, got {names:?}"
        );
    }
}

#[test]
fn dispatch_read_only_rejects_write_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let result = dispatch(
        &sb,
        "write_file",
        &serde_json::json!({"path": "out.txt", "content": "data"}),
        true,
    )
    .unwrap();
    assert!(
        result.contains("not available in read-only mode"),
        "write_file should be rejected in read-only mode: {result}"
    );
    assert!(
        !tmp.path().join("out.txt").exists(),
        "file must not be created in read-only mode"
    );
}

#[test]
fn dispatch_read_only_rejects_run_shell() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let result = dispatch(
        &sb,
        "run_shell",
        &serde_json::json!({"command": "echo hi"}),
        true,
    )
    .unwrap();
    assert!(
        result.contains("not available in read-only mode"),
        "run_shell should be rejected in read-only mode: {result}"
    );
}

#[test]
fn dispatch_read_only_allows_read_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("test.txt"), "content").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let result = dispatch(
        &sb,
        "read_file",
        &serde_json::json!({"path": "test.txt"}),
        true,
    )
    .unwrap();
    assert!(
        result.contains("content"),
        "read_file should work in read-only mode: {result}"
    );
}

#[test]
fn git_commit_in_full_toolset_not_plan_toolset() {
    let full = tool_definitions();
    let full_names: Vec<String> = full
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        full_names.iter().any(|n| n == "git_commit"),
        "full toolset should include git_commit, got {full_names:?}"
    );
    let plan = plan_tool_definitions();
    let plan_names: Vec<String> = plan
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        !plan_names.iter().any(|n| n == "git_commit"),
        "git_commit is mutating and must not be advertised during planning"
    );
}

#[test]
fn run_lint_in_full_toolset() {
    let full = tool_definitions();
    let full_names: Vec<String> = full
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        full_names.iter().any(|n| n == "run_lint"),
        "full toolset should include run_lint, got {full_names:?}"
    );
    let plan = plan_tool_definitions();
    let plan_names: Vec<String> = plan
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| {
            t.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(
        !plan_names.iter().any(|n| n == "run_lint"),
        "run_lint runs commands and must not be advertised during planning"
    );
}

#[test]
fn dispatch_run_lint_on_cargo_project() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Cargo.toml"), "[package]\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = dispatch(&sb, "run_lint", &serde_json::json!({}), false)
        .unwrap_or_else(|e| format!("Tool error: {e}"));
    assert!(out.contains("--- run_lint (cargo)"), "{out}");
}
