use serde::{Deserialize, Serialize};

/// Closed status vocabulary for a goal or a todo.
///
/// On disk the value stays a string so older sessions still load. Every
/// reader — the status strip, compaction, and the reminder the model sees —
/// must parse through [`WorkStatus::parse`] so an alias cannot be "done" in
/// one place and still open in another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkStatus {
    /// Not started.
    Pending,
    /// Currently being worked.
    InProgress,
    /// Finished.
    Completed,
    /// Dropped. Closed, and not counted as finished work.
    Cancelled,
}

impl WorkStatus {
    /// Map free-form model text onto the closed set.
    ///
    /// Matching is case-insensitive. `done` / `complete` are completed,
    /// `todo` is pending, and `canceled` / `skipped` are cancelled. Anything
    /// else, including typos, stays [`WorkStatus::InProgress`] so a misspelled
    /// status cannot hide a task.
    pub fn parse(status: &str) -> Self {
        match status.trim().to_ascii_lowercase().as_str() {
            "completed" | "complete" | "done" => Self::Completed,
            "pending" | "todo" => Self::Pending,
            "cancelled" | "canceled" | "skipped" => Self::Cancelled,
            _ => Self::InProgress,
        }
    }

    /// Canonical status string written back into prompts.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Still needs work. Completed and cancelled are closed.
    pub fn is_open(self) -> bool {
        matches!(self, Self::Pending | Self::InProgress)
    }

    /// Finished successfully. Cancelled is closed but not completed.
    pub fn is_completed(self) -> bool {
        matches!(self, Self::Completed)
    }
}

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
