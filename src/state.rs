//! Persistent agent state — todos and the current goal.
//!
//! Unlike the in-memory todo store this replaces, state is written to
//! `.raven/state/` so it survives context compaction, session resume, and
//! process restarts. The current goal and pending todos are injected into the
//! system prompt each turn so the model always sees its objective and
//! remaining work (Claude Code's todo system / Grok Build's `goal/state.json`).

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A single todo item (content + status + priority).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// The agent's current goal, persisted across turns and sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub description: String,
    pub status: String,
    pub updated_at: String,
}

const STATE_DIR: &str = ".raven/state";
const TODOS_FILE: &str = "todos.json";
const GOAL_FILE: &str = "goal.json";

fn state_dir(workspace: &Path) -> PathBuf {
    workspace.join(STATE_DIR)
}

/// Load the persisted todo list, or an empty list if none exists.
pub fn load_todos(workspace: &Path) -> Vec<TodoItem> {
    let path = state_dir(workspace).join(TODOS_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
        Err(_) => Vec::new(),
    }
}

/// Persist the todo list atomically.
pub fn save_todos(workspace: &Path, todos: &[TodoItem]) -> Result<()> {
    let dir = state_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    let content = serde_json::to_string_pretty(todos)?;
    write_atomic(&dir.join(TODOS_FILE), content.as_bytes())
}

/// Load the persisted goal, or `None` if none has been set.
pub fn load_goal(workspace: &Path) -> Option<Goal> {
    let path = state_dir(workspace).join(GOAL_FILE);
    match std::fs::read_to_string(&path) {
        Ok(content) => serde_json::from_str(&content).ok(),
        Err(_) => None,
    }
}

/// Persist the goal atomically.
pub fn save_goal(workspace: &Path, goal: &Goal) -> Result<()> {
    let dir = state_dir(workspace);
    std::fs::create_dir_all(&dir)?;
    let content = serde_json::to_string_pretty(goal)?;
    write_atomic(&dir.join(GOAL_FILE), content.as_bytes())
}

const MAX_INJECTED_TODOS: usize = 20;

/// Normalize a free-form status into the supported set.
pub fn normalize_status(status: &str) -> &'static str {
    match status {
        "completed" | "complete" | "done" => "completed",
        "pending" | "todo" => "pending",
        _ => "in_progress",
    }
}

/// Render the todo list as a compact block for system-prompt injection.
pub fn format_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "No tasks".into();
    }
    let shown = todos.len().min(MAX_INJECTED_TODOS);
    let mut out = String::new();
    for (i, t) in todos.iter().take(shown).enumerate() {
        let mark = match normalize_status(&t.status) {
            "completed" => "[completed]",
            "in_progress" => "[in_progress]",
            _ => "[pending]",
        };
        out.push_str(&format!("{} {}: {}\n", mark, i + 1, t.content));
    }
    if todos.len() > shown {
        out.push_str(&format!("… {} more", todos.len() - shown));
    }
    out.trim_end().to_string()
}

/// Render the goal as a compact block for system-prompt injection.
pub fn format_goal(goal: &Goal) -> String {
    format!("[{}] {}", goal.status, goal.description)
}

/// Atomic write via a unique temp name + rename, so a reader sees either the
/// old or the new content, never a partial write.
fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = format!(".{}.{}.tmp", std::process::id(), n);
    let tmp = path.with_extension(unique);
    let write_res = std::fs::write(&tmp, content).context("write state temp");
    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
        return write_res;
    }
    let rename_res = std::fs::rename(&tmp, path).context("rename state temp");
    if rename_res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    rename_res
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("raven_state_test_{}_{n}", std::process::id()))
    }

    #[test]
    fn load_todos_empty_when_none() {
        let dir = ws();
        assert!(load_todos(&dir).is_empty());
    }

    #[test]
    fn save_and_load_todos_roundtrip() {
        let dir = ws();
        let todos = vec![
            TodoItem {
                content: "Do X".into(),
                status: "in_progress".into(),
                priority: "high".into(),
            },
            TodoItem {
                content: "Do Y".into(),
                status: "pending".into(),
                priority: "low".into(),
            },
        ];
        save_todos(&dir, &todos).unwrap();
        let loaded = load_todos(&dir);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "Do X");
        assert_eq!(loaded[0].status, "in_progress");
        assert_eq!(loaded[1].priority, "low");
    }

    #[test]
    fn save_todos_overwrites() {
        let dir = ws();
        save_todos(
            &dir,
            &[TodoItem {
                content: "A".into(),
                status: "pending".into(),
                priority: "medium".into(),
            }],
        )
        .unwrap();
        save_todos(
            &dir,
            &[TodoItem {
                content: "B".into(),
                status: "completed".into(),
                priority: "medium".into(),
            }],
        )
        .unwrap();
        let loaded = load_todos(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "B");
    }

    #[test]
    fn load_goal_none_when_missing() {
        let dir = ws();
        assert!(load_goal(&dir).is_none());
    }

    #[test]
    fn save_and_load_goal_roundtrip() {
        let dir = ws();
        let goal = Goal {
            description: "Ship the feature".into(),
            status: "in_progress".into(),
            updated_at: "2026-01-01".into(),
        };
        save_goal(&dir, &goal).unwrap();
        let loaded = load_goal(&dir).unwrap();
        assert_eq!(loaded.description, "Ship the feature");
        assert_eq!(loaded.status, "in_progress");
    }

    #[test]
    fn format_todos_empty() {
        assert_eq!(format_todos(&[]), "No tasks");
    }

    #[test]
    fn format_todos_marks_statuses() {
        let todos = vec![
            TodoItem {
                content: "A".into(),
                status: "completed".into(),
                priority: "high".into(),
            },
            TodoItem {
                content: "B".into(),
                status: "in_progress".into(),
                priority: "medium".into(),
            },
            TodoItem {
                content: "C".into(),
                status: "pending".into(),
                priority: "low".into(),
            },
        ];
        let out = format_todos(&todos);
        assert!(out.contains("[completed] 1: A"));
        assert!(out.contains("[in_progress] 2: B"));
        assert!(out.contains("[pending] 3: C"));
    }

    #[test]
    fn format_goal_renders_status() {
        let goal = Goal {
            description: "Do it".into(),
            status: "in_progress".into(),
            updated_at: "".into(),
        };
        assert_eq!(format_goal(&goal), "[in_progress] Do it");
    }

    #[test]
    fn format_todos_caps_injection() {
        let todos: Vec<TodoItem> = (0..25)
            .map(|i| TodoItem {
                content: format!("T{i}"),
                status: "pending".into(),
                priority: "low".into(),
            })
            .collect();
        let out = format_todos(&todos);
        assert!(out.contains("T19"));
        assert!(!out.contains("T20"));
        assert!(out.contains("… 5 more"));
    }

    #[test]
    fn normalize_status_aliases() {
        assert_eq!(normalize_status("done"), "completed");
        assert_eq!(normalize_status("todo"), "pending");
        assert_eq!(normalize_status("weird"), "in_progress");
    }
}
