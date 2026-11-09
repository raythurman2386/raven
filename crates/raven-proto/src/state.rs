use serde::{Deserialize, Serialize};

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
