//! Session persistence — save/load conversation history as JSONL.
//!
//! Sessions are stored per-workspace under `.raven/sessions/`.
//! Each session is a directory containing:
//!   - `summary.json`   — metadata (id, model, timestamps, title) — the marker file
//!   - `messages.jsonl`  — append-only conversation (one ChatMessage per line)
//!
//! All file writes are atomic (temp file + rename) for crash safety.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::agent::ChatMessage;

/// Metadata for a session, stored in `summary.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub version: u32,
    pub id: String,
    pub created_at: String,
    pub updated_at: String,
    pub model: String,
    pub title: String,
}

/// A loaded session with its messages.
#[derive(Debug, Clone)]
pub struct Session {
    pub summary: SessionSummary,
    pub messages: Vec<ChatMessage>,
}

/// Lightweight metadata for listing sessions without loading messages.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub model: String,
    pub updated_at: String,
}

const SUMMARY_FILE: &str = "summary.json";
const MESSAGES_FILE: &str = "messages.jsonl";
const SESSION_FORMAT_VERSION: u32 = 1;

/// Manages session storage for a workspace.
pub struct SessionStore {
    sessions_dir: PathBuf,
}

impl SessionStore {
    /// Create a session store for the given workspace.
    /// Sessions live in `{workspace}/.raven/sessions/`.
    pub fn for_workspace(workspace: &Path) -> Result<Self> {
        let sessions_dir = workspace.join(".raven").join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(Self { sessions_dir })
    }

    /// Create a new session with a timestamped ID.
    pub fn create(&self, model: &str) -> Result<Session> {
        let now = now_iso();
        let id = now.clone();
        let summary = SessionSummary {
            version: SESSION_FORMAT_VERSION,
            id: id.clone(),
            created_at: now.clone(),
            updated_at: now,
            model: model.to_string(),
            title: String::new(),
        };
        let dir = self.session_dir(&id);
        std::fs::create_dir_all(&dir)?;
        self.write_summary(&summary)?;
        Ok(Session {
            summary,
            messages: Vec::new(),
        })
    }

    /// Append a single message to a session's JSONL file.
    pub fn append_message(&self, session: &Session, msg: &ChatMessage) -> Result<()> {
        let path = self.session_dir(&session.summary.id).join(MESSAGES_FILE);
        let line = serde_json::to_string(msg)?;
        // Append (not atomic — JSONL is append-only by nature)
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .context("open messages.jsonl for append")?;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// Load a session by ID (reads summary + all messages).
    pub fn load(&self, id: &str) -> Result<Session> {
        let dir = self.session_dir(id);
        let summary_path = dir.join(SUMMARY_FILE);
        if !summary_path.exists() {
            bail!("Session not found: {}", id);
        }
        let summary_str = std::fs::read_to_string(&summary_path)?;
        let summary: SessionSummary = serde_json::from_str(&summary_str)?;

        let messages_path = dir.join(MESSAGES_FILE);
        let messages = if messages_path.exists() {
            load_messages(&messages_path)?
        } else {
            Vec::new()
        };

        Ok(Session { summary, messages })
    }

    /// List all sessions (metadata only, no messages loaded).
    pub fn list(&self) -> Result<Vec<SessionMeta>> {
        let mut metas = Vec::new();
        if !self.sessions_dir.exists() {
            return Ok(metas);
        }
        for entry in std::fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let summary_path = entry.path().join(SUMMARY_FILE);
            if !summary_path.exists() {
                continue;
            }
            if let Ok(summary_str) = std::fs::read_to_string(&summary_path) {
                if let Ok(summary) = serde_json::from_str::<SessionSummary>(&summary_str) {
                    metas.push(SessionMeta {
                        id: summary.id.clone(),
                        title: summary.title.clone(),
                        model: summary.model.clone(),
                        updated_at: summary.updated_at.clone(),
                    });
                }
            }
        }
        // Sort by updated_at descending (most recent first)
        metas.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        Ok(metas)
    }

    /// Get the most recent session (or None if no sessions exist).
    pub fn latest(&self) -> Result<Option<Session>> {
        let metas = self.list()?;
        if let Some(first) = metas.into_iter().next() {
            return Ok(Some(self.load(&first.id)?));
        }
        Ok(None)
    }

    /// Replace all messages in a session (used after a turn completes).
    pub fn save_all_messages(&self, session: &Session, messages: &[ChatMessage]) -> Result<()> {
        let path = self.session_dir(&session.summary.id).join(MESSAGES_FILE);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)?;
        for msg in messages {
            let line = serde_json::to_string(msg)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Update the session's summary (title, updated_at).
    pub fn update_summary(&self, session: &mut Session, title: Option<String>) -> Result<()> {
        session.summary.updated_at = now_iso();
        if let Some(t) = title {
            session.summary.title = t;
        }
        self.write_summary(&session.summary)?;
        Ok(())
    }

    // ── Internal helpers ──────────────────────────────────────────────

    fn session_dir(&self, id: &str) -> PathBuf {
        self.sessions_dir.join(id)
    }

    fn write_summary(&self, summary: &SessionSummary) -> Result<()> {
        let path = self.session_dir(&summary.id).join(SUMMARY_FILE);
        let content = serde_json::to_string_pretty(summary)?;
        write_atomic(&path, content.as_bytes())
    }
}

/// Load messages from a JSONL file.
fn load_messages(path: &Path) -> Result<Vec<ChatMessage>> {
    let mut messages = Vec::new();
    let content = std::fs::read_to_string(path)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<ChatMessage>(line) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                // Skip malformed lines but warn
                tracing::warn!("skipping malformed session line: {}", e);
            }
        }
    }
    Ok(messages)
}

/// Write bytes to a temp file then rename (atomic on most filesystems).
fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Generate an ISO 8601 timestamp string (UTC, second precision).
pub fn now_iso_public() -> String {
    now_iso()
}

/// Generate an ISO 8601 timestamp string (UTC, second precision).
fn now_iso() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    // Simple ISO format from Unix timestamp
    let (year, month, day, hour, min, sec) = unix_to_ymd_hms(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year, month, day, hour, min, sec
    )
}

/// Convert a Unix timestamp to (year, month, day, hour, min, sec) in UTC.
fn unix_to_ymd_hms(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86400;
    let rem = secs % 86400;
    let hour = rem / 3600;
    let min = (rem % 3600) / 60;
    let sec = rem % 60;

    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]

    (y as u64, m, d, hour, min, sec)
}
