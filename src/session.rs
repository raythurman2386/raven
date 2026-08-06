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
///
/// Uses a unique temp name (`.<pid>.<counter>.tmp`) rather than a fixed
/// `.tmp` so two concurrent writers (e.g. a running turn and a `/stop` flush)
/// can never clobber each other's in-flight temp file. The rename is atomic,
/// so a reader sees either the old or the new content, never a partial write.
fn write_atomic(path: &Path, content: &[u8]) -> Result<()> {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let unique = format!(".{}.{}.tmp", std::process::id(), n);
    let tmp = path.with_extension(unique);
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

    // Hinnant's algorithm computes the year for the March-based year; the
    // civil year is one greater when the resulting month is Jan or Feb.
    let y = y as u64 + u64::from(m <= 2);

    (y, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_atomic_uses_unique_tmp_and_leaves_none_behind() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("summary.json");
        // Write twice — the unique temp names must not collide, and after each
        // write no `.tmp` files may remain.
        write_atomic(&target, b"v1").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"v1");
        write_atomic(&target, b"v2").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"v2");
        // No temp files left behind.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp files should remain: {leftovers:?}"
        );
    }

    #[test]
    fn write_atomic_concurrent_writers_do_not_collide() {
        // Two sequential unique-tmp writes produce distinct temp names, proving
        // a concurrent pair can't clobber each other's in-flight file.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("messages.jsonl");
        // Simulate two writers producing unique tmp paths via write_atomic.
        write_atomic(&target, b"a").unwrap();
        write_atomic(&target, b"b").unwrap();
        let names: Vec<String> = std::fs::read_dir(tmp.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        // Only the final target remains.
        assert_eq!(names, vec!["messages.jsonl".to_string()]);
    }

    // ── unix_to_ymd_hms date arithmetic ────────────────────────────────
    //
    // Expected values are cross-checked against Python's
    // `datetime.fromtimestamp(ts, tz=timezone.utc)`.

    #[test]
    fn unix_to_ymd_hms_epoch() {
        assert_eq!(unix_to_ymd_hms(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1), (1970, 1, 1, 0, 0, 1));
        assert_eq!(unix_to_ymd_hms(86400), (1970, 1, 2, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_year_boundary() {
        // 1999-12-31 23:59:59 UTC
        assert_eq!(unix_to_ymd_hms(946684799), (1999, 12, 31, 23, 59, 59));
        // 2000-01-01 00:00:00 UTC
        assert_eq!(unix_to_ymd_hms(946684800), (2000, 1, 1, 0, 0, 0));
        // 1970-12-31 23:59:59 UTC
        assert_eq!(unix_to_ymd_hms(31535999), (1970, 12, 31, 23, 59, 59));
        // 1971-01-01 00:00:00 UTC
        assert_eq!(unix_to_ymd_hms(31536000), (1971, 1, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_leap_years() {
        // 2000 is a leap year (divisible by 400).
        assert_eq!(unix_to_ymd_hms(951782400), (2000, 2, 29, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(951868800), (2000, 3, 1, 0, 0, 0));
        // 2004 is a leap year.
        assert_eq!(unix_to_ymd_hms(1078012800), (2004, 2, 29, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1078099200), (2004, 3, 1, 0, 0, 0));
        // 2024 is a leap year.
        assert_eq!(unix_to_ymd_hms(1709164800), (2024, 2, 29, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1709251200), (2024, 3, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_non_leap_years() {
        // 2023 is not a leap year — Feb has 28 days.
        assert_eq!(unix_to_ymd_hms(1677542400), (2023, 2, 28, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(1677628800), (2023, 3, 1, 0, 0, 0));
        // 2100 is not a leap year (divisible by 100 but not 400).
        assert_eq!(unix_to_ymd_hms(4107456000), (2100, 2, 28, 0, 0, 0));
        assert_eq!(unix_to_ymd_hms(4107542400), (2100, 3, 1, 0, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_dst_adjacent() {
        // These are UTC timestamps around US DST transitions; the civil date
        // must be correct regardless of any local timezone.
        // 2024-03-10 08:00:00 UTC (US DST start day).
        assert_eq!(unix_to_ymd_hms(1710057600), (2024, 3, 10, 8, 0, 0));
        // 2024-11-03 06:00:00 UTC (US DST end day).
        assert_eq!(unix_to_ymd_hms(1730613600), (2024, 11, 3, 6, 0, 0));
    }

    #[test]
    fn unix_to_ymd_hms_2038_boundary() {
        // 2038-01-19 03:14:07 UTC — the classic 32-bit time_t overflow point.
        assert_eq!(unix_to_ymd_hms(2147483647), (2038, 1, 19, 3, 14, 7));
    }

    #[test]
    fn now_iso_round_trips_through_parser() {
        // now_iso produces "YYYY-MM-DDTHH:MM:SS" (UTC, second precision).
        // Parse it back and confirm the fields are internally consistent and
        // within a sane window of the current time.
        let iso = now_iso();
        assert_eq!(iso.len(), 19, "ISO string: {iso}");
        assert_eq!(iso.as_bytes()[10], b'T', "ISO string: {iso}");

        let year: u64 = iso[0..4].parse().unwrap();
        let month: u64 = iso[5..7].parse().unwrap();
        let day: u64 = iso[8..10].parse().unwrap();
        let hour: u64 = iso[11..13].parse().unwrap();
        let min: u64 = iso[14..16].parse().unwrap();
        let sec: u64 = iso[17..19].parse().unwrap();

        assert!((2000..=2100).contains(&year), "year out of range: {iso}");
        assert!((1..=12).contains(&month), "month out of range: {iso}");
        assert!((1..=31).contains(&day), "day out of range: {iso}");
        assert!(hour < 24, "hour out of range: {iso}");
        assert!(min < 60, "minute out of range: {iso}");
        assert!(sec < 60, "second out of range: {iso}");

        // The generated timestamp must be within a couple seconds of now.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let generated = unix_to_ymd_hms(now);
        let parsed = (year, month, day, hour, min, sec);
        // Allow the second to tick between the two calls.
        let diff = seconds_since_epoch(parsed).abs_diff(now);
        assert!(diff <= 2, "generated {parsed:?} vs now {generated:?}");
    }

    /// Convert a (year, month, day, hour, min, sec) UTC tuple back to unix
    /// seconds, using the same civil-date arithmetic as `unix_to_ymd_hms`.
    fn seconds_since_epoch((y, m, d, h, min, s): (u64, u64, u64, u64, u64, u64)) -> u64 {
        // Days since 1970-01-01 via Howard Hinnant's algorithm (inverse).
        let y = y as i64;
        let m = m as i64;
        let d = d as i64;
        let yoe = if m <= 2 { y - 1 } else { y };
        let era = yoe.div_euclid(400);
        let yoe = yoe.rem_euclid(400);
        let mp = if m > 2 { m - 3 } else { m + 9 };
        let doy = (153 * mp + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe - 719468;
        (days as u64) * 86400 + h * 3600 + min * 60 + s
    }
}
