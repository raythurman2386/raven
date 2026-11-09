//! Workspace-scoped tools + lightweight sandbox.
//!
//! Tool surface mirrors the useful core of Grok Build:
//!   `read_file`, `search_replace`, `list_dir`, `grep`, `run_shell`, `todo_write`
//! plus `write_file` (full writes) and `search_code` (literal search).
//!
//! Tools are dispatched by name via [`dispatch`]; the OpenAI function-calling
//! schemas are produced by [`tool_definitions`].

mod definitions;
mod dispatch;
mod document;
mod git;
mod patch;
mod validate;

pub mod mcp;
pub mod plugins;
pub mod repomap;
pub mod skills;
pub mod web;

pub mod sandbox {
    pub use raven_sandbox::*;
}

pub use git::GitOps;
pub use patch::PatchOps;

use std::path::Path;

pub use definitions::{
    chat_tool_definitions, offloaded_tool_catalog, offloaded_tool_definitions,
    plan_tool_definitions, tool_definitions, tool_schema_text,
};
pub use dispatch::dispatch;
pub use raven_proto::TodoItem;
pub use raven_sandbox::{
    glob_segment_match, safe_command_re, system_command_autonomous, system_safe_command_re, Sandbox,
};
pub use validate::{validate_tool_call, MAX_ARGUMENTS_BYTES};

// ── Todo state ────────────────────────────────────────────────────────

/// Full-replace todo list (Grok Build `todo_write` semantics), persisted to
/// the session's `state/todos.json` so it survives compaction and resumes
/// but not a fresh session. Unavailable without an active session.
pub fn todo_write(state_dir: Option<&Path>, todos: Vec<TodoItem>) -> anyhow::Result<String> {
    let Some(state_dir) = state_dir else {
        return Ok(
            "No active session: todo_write is unavailable for this agent. \
             Sub-agents and parallel tasks run without todo persistence."
                .into(),
        );
    };
    let goal = raven_persist::state::load_goal_from_dir(state_dir);
    let (todos, auto_completed) = raven_persist::state::gate_residue_todos(goal.as_ref(), todos);
    raven_persist::state::save_todos(state_dir, &todos)?;
    let mut out = raven_persist::state::format_todos(&todos);
    if !auto_completed.is_empty() {
        out.push_str(
            "\n\n[residue auto-completed: primary work already met; \
             ask the user before reopening]\n",
        );
        for content in &auto_completed {
            out.push_str(&format!("- {content}\n"));
        }
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests;
