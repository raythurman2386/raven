//! Persistent agent state — todos, the current goal, and the pinned user
//! constraint.
//!
//! State is scoped to the active session: the files live next to the
//! session's `messages.jsonl` (`{workspace}/.raven/sessions/{id}/state/`),
//! so a fresh session starts with no goal or todos and only `--resume`
//! carries them forward. Memory (`MEMORY.md`) is the cross-session store
//! and is unaffected. The current goal and pending todos are injected into
//! the system prompt each turn so the model always sees its objective and
//! remaining work (Claude Code's todo system / Grok Build's `goal/state.json`).
//!
//! The first user ask is also pinned as a `user_constraint` that `goal_set`
//! cannot overwrite, so re-anchoring cannot erase the original ask.

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

/// The agent's current goal, persisted across turns and — via session resume
/// — across process restarts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub description: String,
    pub status: String,
    pub updated_at: String,
}

const STATE_DIR_NAME: &str = "state";
const TODOS_FILE: &str = "todos.json";
const GOAL_FILE: &str = "goal.json";

/// Build the state directory for a session id under a session store root:
/// `{sessions_dir}/{session_id}/state/`.
pub fn session_state_dir(sessions_dir: &Path, session_id: &str) -> PathBuf {
    sessions_dir.join(session_id).join(STATE_DIR_NAME)
}

/// Read a goal/todo JSON file, falling back to the legacy workspace-global
/// location (`.raven/state/`) for state written before goals became
/// session-scoped. The fallback is read-only: writes always land in the
/// session directory so legacy files age out naturally.
fn read_json_with_legacy<T: serde::de::DeserializeOwned>(
    primary: &Path,
    legacy: Option<&Path>,
) -> Option<T> {
    if let Ok(content) = std::fs::read_to_string(primary) {
        return serde_json::from_str(&content).ok();
    }
    let legacy = legacy?;
    let content = std::fs::read_to_string(legacy).ok()?;
    serde_json::from_str(&content).ok()
}

/// Atomically write a state file, creating the parent directory as needed.
fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let content = serde_json::to_string_pretty(value)?;
    write_atomic(path, content.as_bytes())
}

/// Load the session's todo list, or an empty list if none exists.
///
/// Falls back to the legacy workspace-global file when the session has no
/// todos of its own (one-release migration; see module docs).
pub fn load_todos(sessions_dir: &Path, session_id: &str, workspace: &Path) -> Vec<TodoItem> {
    read_json_with_legacy(
        &session_state_dir(sessions_dir, session_id).join(TODOS_FILE),
        Some(&workspace.join(STATE_DIR_NAME).join(TODOS_FILE)),
    )
    .unwrap_or_default()
}

/// Persist the todo list atomically under a session state directory.
pub fn save_todos(state_dir: &Path, todos: &[TodoItem]) -> Result<()> {
    write_json(&state_dir.join(TODOS_FILE), &todos)
}

/// Load the session's persisted goal, or `None` if none has been set.
///
/// Falls back to the legacy workspace-global file when the session has no
/// goal of its own (one-release migration; see module docs).
pub fn load_goal(sessions_dir: &Path, session_id: &str, workspace: &Path) -> Option<Goal> {
    read_json_with_legacy(
        &session_state_dir(sessions_dir, session_id).join(GOAL_FILE),
        Some(&workspace.join(STATE_DIR_NAME).join(GOAL_FILE)),
    )
}

/// Persist the goal atomically under a session state directory.
pub fn save_goal(state_dir: &Path, goal: &Goal) -> Result<()> {
    write_json(&state_dir.join(GOAL_FILE), goal)
}

/// Load a goal from a state directory directly (no legacy fallback).
pub fn load_goal_from_dir(state_dir: &Path) -> Option<Goal> {
    let path = state_dir.join(GOAL_FILE);
    serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
}

/// Load todos from a state directory directly (no legacy fallback).
pub fn load_todos_from_dir(state_dir: &Path) -> Vec<TodoItem> {
    let path = state_dir.join(TODOS_FILE);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
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

const CONSTRAINT_FILE: &str = "user_constraint.json";
const MAX_CONSTRAINT_CHARS: usize = 800;

/// First-user ask pinned for the session. Survives `goal_set` / `todo_write`
/// rewrites so the agent cannot silently replace the user's constraint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UserConstraint {
    pub text: String,
    pub set_at: String,
}

/// Load the pinned user constraint from a state directory, if any.
pub fn load_user_constraint_from_dir(state_dir: &Path) -> Option<UserConstraint> {
    let path = state_dir.join(CONSTRAINT_FILE);
    serde_json::from_str(&std::fs::read_to_string(&path).ok()?).ok()
}

/// Persist the user constraint atomically under a session state directory.
pub fn save_user_constraint(state_dir: &Path, constraint: &UserConstraint) -> Result<()> {
    write_json(&state_dir.join(CONSTRAINT_FILE), constraint)
}

/// Pin `user_text` as the session constraint the first time only.
///
/// Returns `Ok(true)` when a new constraint was written, `Ok(false)` when one
/// already existed or the text was empty.
pub fn pin_user_constraint_once(state_dir: &Path, user_text: &str) -> Result<bool> {
    if load_user_constraint_from_dir(state_dir).is_some() {
        return Ok(false);
    }
    let text = truncate_constraint(user_text);
    if text.is_empty() {
        return Ok(false);
    }
    let constraint = UserConstraint {
        text,
        set_at: crate::session::now_iso_public(),
    };
    save_user_constraint(state_dir, &constraint)?;
    Ok(true)
}

fn truncate_constraint(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    let count = t.chars().count();
    if count <= MAX_CONSTRAINT_CHARS {
        t.to_string()
    } else {
        let truncated: String = t.chars().take(MAX_CONSTRAINT_CHARS).collect();
        format!("{truncated}…")
    }
}

/// Render the pinned constraint for setup-prompt injection.
pub fn format_user_constraint(constraint: &UserConstraint) -> String {
    constraint.text.clone()
}

/// Append the pinned user-constraint section to a setup body, if present.
pub fn append_user_constraint_section(body: &mut String, state_dir: &Path) {
    if let Some(c) = load_user_constraint_from_dir(state_dir) {
        body.push_str("\n--- User constraint (pinned; do not replace via goal_set) ---\n");
        body.push_str(&format_user_constraint(&c));
        body.push('\n');
    }
}

/// Non-blocking / dated-residue style todos that should not keep the session
/// open after primary live work is already satisfied.
pub fn looks_like_residue_todo(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("dated")
        || lower.contains("memory-plan")
        || lower.contains("memory plan")
        || lower.contains("non-blocking")
        || lower.contains("residue")
        || (lower.contains("memory")
            && (lower.contains("update")
                || lower.contains("record")
                || lower.contains("write")
                || lower.contains("dated")))
}

/// Whether primary live work is already done (goal completed, or every
/// non-residue todo is completed).
pub fn primary_work_satisfied(goal: Option<&Goal>, todos: &[TodoItem]) -> bool {
    if let Some(g) = goal {
        if normalize_status(&g.status) == "completed" {
            return true;
        }
    }
    let live: Vec<&TodoItem> = todos
        .iter()
        .filter(|t| !looks_like_residue_todo(&t.content))
        .collect();
    !live.is_empty()
        && live
            .iter()
            .all(|t| normalize_status(&t.status) == "completed")
}

/// When primary work is satisfied, auto-complete open residue todos so the
/// agent stops re-anchoring on dated housekeeping without user confirmation.
///
/// Returns the (possibly modified) todo list and the contents that were
/// auto-completed.
pub fn gate_residue_todos(
    goal: Option<&Goal>,
    mut todos: Vec<TodoItem>,
) -> (Vec<TodoItem>, Vec<String>) {
    if !primary_work_satisfied(goal, &todos) {
        return (todos, Vec::new());
    }
    let mut auto = Vec::new();
    for t in &mut todos {
        if normalize_status(&t.status) != "completed" && looks_like_residue_todo(&t.content) {
            t.status = "completed".into();
            auto.push(t.content.clone());
        }
    }
    (todos, auto)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws() -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("raven_state_test_{}_{n}", std::process::id()))
    }

    /// A session store root with two created session dirs, so state paths
    /// exist the way `SessionStore::create` leaves them.
    fn store_with_sessions(tmp: &Path) -> (PathBuf, String, String) {
        let sessions = tmp.join(".raven/sessions");
        let a = "20260910T090000Z-1-1".to_string();
        let b = "20260910T090000Z-1-2".to_string();
        std::fs::create_dir_all(sessions.join(&a)).unwrap();
        std::fs::create_dir_all(sessions.join(&b)).unwrap();
        (sessions, a, b)
    }

    #[test]
    fn session_state_dir_is_under_the_session() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        let dir = session_state_dir(&sessions, &a);
        assert!(dir.starts_with(sessions.join(&a)));
        assert!(dir.ends_with("state"));
    }

    #[test]
    fn load_todos_empty_when_none() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        assert!(load_todos(&sessions, &a, &tmp).is_empty());
    }

    #[test]
    fn save_and_load_todos_roundtrip() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
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
        save_todos(&session_state_dir(&sessions, &a), &todos).unwrap();
        let loaded = load_todos(&sessions, &a, &tmp);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].content, "Do X");
        assert_eq!(loaded[0].status, "in_progress");
        assert_eq!(loaded[1].priority, "low");
    }

    #[test]
    fn save_todos_overwrites() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        save_todos(
            &session_state_dir(&sessions, &a),
            &[TodoItem {
                content: "A".into(),
                status: "pending".into(),
                priority: "medium".into(),
            }],
        )
        .unwrap();
        save_todos(
            &session_state_dir(&sessions, &a),
            &[TodoItem {
                content: "B".into(),
                status: "completed".into(),
                priority: "medium".into(),
            }],
        )
        .unwrap();
        let loaded = load_todos(&sessions, &a, &tmp);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].content, "B");
    }

    #[test]
    fn state_is_isolated_between_sessions() {
        let tmp = ws();
        let (sessions, a, b) = store_with_sessions(&tmp);
        let goal = Goal {
            description: "Session A goal".into(),
            status: "in_progress".into(),
            updated_at: "2026-09-10".into(),
        };
        save_goal(&session_state_dir(&sessions, &a), &goal).unwrap();
        assert!(load_goal(&sessions, &b, &tmp).is_none());
        assert_eq!(
            load_goal(&sessions, &a, &tmp).unwrap().description,
            "Session A goal"
        );
    }

    #[test]
    fn legacy_goal_is_read_but_not_session_scoped() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        let legacy = Goal {
            description: "Legacy goal".into(),
            status: "in_progress".into(),
            updated_at: "2026-09-09".into(),
        };
        let legacy_dir = tmp.join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join(GOAL_FILE),
            serde_json::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        assert_eq!(
            load_goal(&sessions, &a, &tmp).unwrap().description,
            "Legacy goal"
        );

        // Saving a new goal writes to the session dir only; the session goal
        // then shadows the legacy one.
        let fresh = Goal {
            description: "Fresh goal".into(),
            status: "in_progress".into(),
            updated_at: "2026-09-10".into(),
        };
        save_goal(&session_state_dir(&sessions, &a), &fresh).unwrap();
        assert_eq!(
            load_goal(&sessions, &a, &tmp).unwrap().description,
            "Fresh goal"
        );
        assert!(legacy_dir.join(GOAL_FILE).exists());
    }

    #[test]
    fn load_goal_none_when_missing() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        assert!(load_goal(&sessions, &a, &tmp).is_none());
    }

    #[test]
    fn save_and_load_goal_roundtrip() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        let goal = Goal {
            description: "Ship the feature".into(),
            status: "in_progress".into(),
            updated_at: "2026-01-01".into(),
        };
        save_goal(&session_state_dir(&sessions, &a), &goal).unwrap();
        let loaded = load_goal(&sessions, &a, &tmp).unwrap();
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

    #[test]
    fn pin_user_constraint_once_is_sticky() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        let dir = session_state_dir(&sessions, &a);
        assert!(pin_user_constraint_once(&dir, "  Fix live doc_drift  ").unwrap());
        let c = load_user_constraint_from_dir(&dir).unwrap();
        assert_eq!(c.text, "Fix live doc_drift");
        assert!(!pin_user_constraint_once(&dir, "different ask").unwrap());
        assert_eq!(
            load_user_constraint_from_dir(&dir).unwrap().text,
            "Fix live doc_drift"
        );
    }

    #[test]
    fn append_user_constraint_section_injects_block() {
        let tmp = ws();
        let (sessions, a, _) = store_with_sessions(&tmp);
        let dir = session_state_dir(&sessions, &a);
        pin_user_constraint_once(&dir, "Ship circling guards").unwrap();
        let mut body = String::from(
            "Workspace root: /tmp
",
        );
        append_user_constraint_section(&mut body, &dir);
        assert!(body.contains("--- User constraint (pinned; do not replace via goal_set) ---"));
        assert!(body.contains("Ship circling guards"));
    }

    #[test]
    fn residue_todos_auto_complete_when_primary_met() {
        assert!(looks_like_residue_todo(
            "Update dated memory-plan with residual notes"
        ));
        assert!(!looks_like_residue_todo("Fix live doc_drift verify"));

        let goal = Goal {
            description: "Harness-only fixes".into(),
            status: "completed".into(),
            updated_at: "2026-09-25".into(),
        };
        let todos = vec![
            TodoItem {
                content: "Live doc_drift = 0".into(),
                status: "completed".into(),
                priority: "high".into(),
            },
            TodoItem {
                content: "Chase dated memory-plan residue".into(),
                status: "pending".into(),
                priority: "low".into(),
            },
        ];
        assert!(primary_work_satisfied(Some(&goal), &todos));
        let (gated, auto) = gate_residue_todos(Some(&goal), todos);
        assert_eq!(auto, vec!["Chase dated memory-plan residue".to_string()]);
        assert_eq!(normalize_status(&gated[1].status), "completed");
    }

    #[test]
    fn residue_gate_leaves_open_when_live_work_pending() {
        let todos = vec![
            TodoItem {
                content: "Fix live doc_drift".into(),
                status: "in_progress".into(),
                priority: "high".into(),
            },
            TodoItem {
                content: "Record dated memory-plan note".into(),
                status: "pending".into(),
                priority: "low".into(),
            },
        ];
        assert!(!primary_work_satisfied(None, &todos));
        let (gated, auto) = gate_residue_todos(None, todos.clone());
        assert!(auto.is_empty());
        assert_eq!(gated[1].status, "pending");
    }
}
