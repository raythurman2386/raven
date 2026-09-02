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
mod sandbox;
mod validate;
#[cfg(windows)]
mod windows;

use std::path::Path;

pub use crate::state::TodoItem;
pub use definitions::{chat_tool_definitions, plan_tool_definitions, tool_definitions};
pub use dispatch::dispatch;
pub use sandbox::{safe_command_re, system_safe_command_re, Sandbox};
pub use validate::{validate_tool_call, MAX_ARGUMENTS_BYTES};

/// Minimal glob matcher: supports `*` and `?` against the file name.
pub(crate) fn glob_matches(path: &Path, pattern: &str) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    glob_segment_match(name, pattern)
}

pub fn glob_segment_match(text: &str, pat: &str) -> bool {
    let t = text.as_bytes();
    let p = pat.as_bytes();
    let (mut ti, mut pi) = (0, 0);
    let (mut star_t, mut star_p): (Option<usize>, Option<usize>) = (None, None);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = Some(ti);
            pi += 1;
        } else if let (Some(sp), Some(st)) = (star_p, star_t) {
            pi = sp + 1;
            ti = st + 1;
            star_t = Some(ti);
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ── Todo state ────────────────────────────────────────────────────────

/// Full-replace todo list (Grok Build `todo_write` semantics), persisted to
/// `.raven/state/todos.json` so it survives compaction and sessions.
pub fn todo_write(workspace: &Path, todos: Vec<TodoItem>) -> anyhow::Result<String> {
    crate::state::save_todos(workspace, &todos)?;
    Ok(crate::state::format_todos(&todos))
}

#[cfg(test)]
mod tests;
