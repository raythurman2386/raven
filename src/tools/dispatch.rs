//! Tool dispatch: match on tool name and route to the appropriate handler.

use crate::error::ToolError;

use super::sandbox::{sandbox_search_code, Sandbox};
use super::TodoItem;

/// Convert an `anyhow::Error` into a [`ToolError`], extracting IO context
/// from the error chain when possible.
fn anyhow_to_tool_error(e: anyhow::Error, path: &str, operation: &str) -> ToolError {
    for cause in e.chain() {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return ToolError::io(path, operation, io.kind().into());
        }
    }
    ToolError::Other(format!("{e:#}"))
}

/// Dispatch a tool call by name, returning the result or a structured error.
///
/// Unknown tool names return an error string in `Ok` so the model receives
/// actionable feedback. Filesystem errors are returned as [`ToolError::Io`]
/// with path and operation context.
pub fn dispatch(
    sandbox: &Sandbox,
    name: &str,
    args: &serde_json::Value,
) -> Result<String, ToolError> {
    let res: anyhow::Result<String> = match name {
        "list_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            sandbox.list_dir(path)
        }
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let start = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let max = args
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(400) as usize;
            sandbox.read_file(path, start, max)
        }
        "search_replace" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            sandbox.search_replace(path, old, new, all)
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            sandbox.write_file(path, content)
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let include = args.get("include").and_then(|v| v.as_str());
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(50) as usize;
            sandbox.grep(pattern, path, include, max)
        }
        "run_shell" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(60);
            sandbox.run_shell(command, timeout)
        }
        "search_code" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(25) as usize;
            sandbox_search_code(sandbox, query, max)
        }
        "todo_write" => {
            let todos = match args.get("todos").and_then(|v| v.as_array()) {
                Some(arr) => arr
                    .iter()
                    .map(|t| TodoItem {
                        content: t
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        status: t
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending")
                            .to_string(),
                        priority: t
                            .get("priority")
                            .and_then(|v| v.as_str())
                            .unwrap_or("medium")
                            .to_string(),
                    })
                    .collect(),
                None => Vec::new(),
            };
            super::todo_write(todos)
        }
        "memory_update" => {
            let section = args.get("section").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            crate::memory::update_memory(&sandbox.workspace, section, content)
        }
        "git_status" => sandbox.git_status(),
        "git_diff" => {
            let staged = args
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            sandbox.git_diff(staged)
        }
        "git_log" => {
            let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            sandbox.git_log(n)
        }
        "apply_patch" => {
            let patch = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");
            sandbox.apply_patch(patch)
        }
        "run_tests" => sandbox.run_tests(),
        "run_lint" => sandbox.run_lint(),
        "skill_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            Ok(crate::skills::search(&sandbox.workspace, query))
        }
        "skill_load" => {
            let name = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            Ok(crate::skills::load(&sandbox.workspace, name))
        }
        "memory_search" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            Ok(crate::memory::search_memory(&sandbox.workspace, query))
        }
        "git_commit" => {
            let message = args.get("message").and_then(|v| v.as_str()).unwrap_or("");
            sandbox.git_commit(message)
        }
        other => return Ok(format!("Unknown tool: {}", other)),
    };
    res.map_err(|e| anyhow_to_tool_error(e, name, name))
}
