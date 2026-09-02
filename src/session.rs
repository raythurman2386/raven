//! Session persistence — save/load conversation history as JSONL.
//!
//! Sessions are stored per-workspace under `.raven/sessions/` (repo scope) or
//! under `~/.raven/system/sessions/` (system scope). Each session is a
//! directory containing:
//!   - `summary.json`   — metadata (id, model, timestamps, title) — the marker file
//!   - `messages.jsonl`  — append-only conversation (one ChatMessage per line)
//!
//! All file writes are atomic (temp file + rename) for crash safety.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::agent::ChatMessage;
use crate::config::Settings;

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

/// The current session format version. Sessions with a higher version are
/// from a newer release and cannot be loaded; sessions with a lower version
/// are migrated through the version chain on load.
pub const CURRENT_SESSION_FORMAT_VERSION: u32 = SESSION_FORMAT_VERSION;

/// Manages session storage for a workspace.
#[derive(Clone)]
pub struct SessionStore {
    sessions_dir: PathBuf,
    workspace: PathBuf,
}

impl SessionStore {
    /// Create a session store for the given workspace.
    /// Sessions live in `{workspace}/.raven/sessions/`.
    pub fn for_workspace(workspace: &Path) -> Result<Self> {
        let sessions_dir = workspace.join(".raven").join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        Ok(Self {
            sessions_dir,
            workspace: workspace.to_path_buf(),
        })
    }

    /// Create a session store matching the settings' scope.
    ///
    /// Repo scope roots the store at `{workspace}/.raven/sessions/` as before.
    /// System scope (workspace `/`) roots it at `~/.raven/system/sessions/`,
    /// matching the system-memory convention, so the privileged scope gets an
    /// on-disk audit trail instead of writing under `/`.
    pub fn for_settings(settings: &Settings) -> Result<Self> {
        if settings.scope.is_system() {
            let home = dirs::home_dir().context("cannot determine home directory")?;
            let sessions_dir = home.join(".raven").join("system").join("sessions");
            std::fs::create_dir_all(&sessions_dir)?;
            Ok(Self {
                sessions_dir,
                workspace: home,
            })
        } else {
            Self::for_workspace(&settings.workspace)
        }
    }

    /// Create a new session with a collision-proof ID.
    pub fn create(&self, model: &str) -> Result<Session> {
        let now = now_iso();
        let id = generate_session_id(&now);
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
    ///
    /// Reads the existing file, appends the new line, and writes atomically
    /// (temp file + rename) so a crash mid-write never leaves a partial line.
    pub fn append_message(&self, session: &Session, msg: &ChatMessage) -> Result<()> {
        let path = self.session_dir(&session.summary.id).join(MESSAGES_FILE);
        let line = serde_json::to_string(msg)?;

        let mut content = if path.exists() {
            std::fs::read(&path).context("read messages.jsonl")?
        } else {
            Vec::new()
        };
        content.extend_from_slice(line.as_bytes());
        content.push(b'\n');

        write_atomic(&path, &content)
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
        let summary = self.migrate_and_persist(summary)?;

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
                    // Migrate (and persist) so a future-version session is
                    // rejected here rather than surfacing only on load.
                    if let Ok(summary) = self.migrate_and_persist(summary) {
                        metas.push(SessionMeta {
                            id: summary.id.clone(),
                            title: summary.title.clone(),
                            model: summary.model.clone(),
                            updated_at: summary.updated_at.clone(),
                        });
                    }
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
        let mut buf = String::new();
        for msg in messages {
            buf.push_str(&serde_json::to_string(msg)?);
            buf.push('\n');
        }
        write_atomic(&path, buf.as_bytes())?;
        self.log_event(
            session,
            "messages_saved",
            &format!("{} messages", messages.len()),
        )
    }

    /// Set `summary.title` for `id` only when it is still empty.
    ///
    /// Used by the background title job so a cheap completion cannot overwrite
    /// a title the user (or a later turn) already chose. Does not load
    /// `messages.jsonl`.
    pub fn apply_title_if_empty(&self, id: &str, title: &str) -> Result<bool> {
        let title = title.trim();
        if title.is_empty() {
            return Ok(false);
        }
        let path = self.session_dir(id).join(SUMMARY_FILE);
        if !path.exists() {
            return Ok(false);
        }
        let mut summary: SessionSummary = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        if !summary.title.is_empty() {
            return Ok(false);
        }
        summary.title = title.to_string();
        summary.updated_at = now_iso();
        self.write_summary(&summary)?;
        Ok(true)
    }

    /// Update the session's summary (title, updated_at).
    pub fn update_summary(&self, session: &mut Session, title: Option<String>) -> Result<()> {
        session.summary.updated_at = now_iso();
        if let Some(t) = title {
            session.summary.title = t;
        }
        self.write_summary(&session.summary)?;
        self.log_event(session, "summary_updated", &session.summary.title)
    }

    /// Update the session's model name and persist the summary, so a model
    /// switched mid-session via `/model` is reflected on resume.
    pub fn update_model(&self, session: &mut Session, model: &str) -> Result<()> {
        session.summary.model = model.to_string();
        session.summary.updated_at = now_iso();
        self.write_summary(&session.summary)?;
        self.log_event(session, "model_changed", model)
    }

    /// Persist a local-only debug event in the session directory.
    ///
    /// This is intentionally local-only and not networked: it is a reproducible
    /// record for debugging or later review, without violating the project's
    /// no-remote-telemetry policy.
    pub fn log_event(&self, session: &Session, kind: &str, message: &str) -> Result<()> {
        let dir = self.session_dir(&session.summary.id);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("debug-events.jsonl");
        let line = serde_json::to_string(&serde_json::json!({
            "ts": now_iso(),
            "kind": kind,
            "message": message,
        }))?;
        let mut bytes = std::fs::read(&path).unwrap_or_default();
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        write_atomic(&path, &bytes)
    }

    /// Export this session as a local Markdown + JSON bundle.
    ///
    /// Writes `session.md` and `session.json` under `dest_dir` (created if
    /// needed). Copies `last.patch` when the session directory has one.
    /// Nothing is sent off-machine.
    pub fn export_bundle(&self, session: &Session, dest_dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dest_dir)
            .with_context(|| format!("create export dir {}", dest_dir.display()))?;
        let md_path = dest_dir.join("session.md");
        let json_path = dest_dir.join("session.json");
        write_atomic(&md_path, render_session_markdown(session).as_bytes())?;
        let json = serde_json::json!({
            "summary": session.summary,
            "messages": session.messages,
        });
        write_atomic(&json_path, serde_json::to_string_pretty(&json)?.as_bytes())?;
        let src_patch = self.session_dir(&session.summary.id).join("last.patch");
        if src_patch.exists() {
            let _ = std::fs::copy(&src_patch, dest_dir.join("last.patch"));
        }
        Ok(dest_dir.to_path_buf())
    }

    /// Default export directory for a session: `{workspace}/.raven/exports/{id}/`
    /// (system scope: `~/.raven/system/exports/{id}/` — derived from the
    /// sessions dir's parent so it stays next to the store it exports from).
    pub fn default_export_dir(&self, session: &Session) -> PathBuf {
        self.sessions_dir
            .parent()
            .unwrap_or(self.workspace.as_path())
            .join("exports")
            .join(&session.summary.id)
    }

    /// Snapshot the current repo diff for this session as a patch file.
    ///
    /// This creates a reviewable artifact in `.raven/sessions/<id>/last.patch`
    /// without sending anything remotely; it is suitable for auditing or
    /// rollback decisions after a task completes.
    ///
    /// Returns `true` when a non-empty git diff was written (so callers can
    /// tell the user where to find it). Empty trees and non-git workspaces
    /// return `false` after still writing a marker/empty file.
    pub fn snapshot_patch(&self, session: &Session) -> Result<bool> {
        let path = self.session_dir(&session.summary.id).join("last.patch");
        if !self.workspace.join(".git").exists() {
            let marker = "# no git repository detected; patch snapshot unavailable\n";
            write_atomic(&path, marker.as_bytes())?;
            return Ok(false);
        }

        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&self.workspace)
            .arg("diff")
            .arg("--no-ext-diff")
            .arg("--binary")
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let nonempty = !out.stdout.is_empty();
                write_atomic(&path, &out.stdout)?;
                Ok(nonempty)
            }
            Ok(out) => {
                let mut msg = String::from("# git diff failed\n");
                if !out.stderr.is_empty() {
                    msg.push_str(&String::from_utf8_lossy(&out.stderr));
                }
                write_atomic(&path, msg.as_bytes())?;
                Ok(false)
            }
            Err(e) => {
                let msg = format!("# failed to snapshot patch: {e}\n");
                write_atomic(&path, msg.as_bytes())?;
                Ok(false)
            }
        }
    }

    /// Remove a session directory (summary + messages + debug-events).
    ///
    /// Used by `/cleanup` to prune old sessions. Irreversible — the caller
    /// must confirm before invoking. The id must be a bare session directory
    /// name (no path separators / `..`) so a crafted id cannot make
    /// `remove_dir_all` target outside the sessions directory.
    pub fn delete(&self, id: &str) -> Result<()> {
        if !is_bare_session_id(id) {
            bail!("Refusing to delete non-session id: {id:?}");
        }
        let dir = self.session_dir(id);
        if !dir.exists() {
            bail!("Session not found: {}", id);
        }
        std::fs::remove_dir_all(&dir)
            .with_context(|| format!("remove session dir {}", dir.display()))?;
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

    /// Migrate a summary to the current format version and persist the result
    /// back to disk so the migration runs only once.
    fn migrate_and_persist(&self, summary: SessionSummary) -> Result<SessionSummary> {
        let migrated = migrate_summary(summary.clone())?;
        if migrated.version != summary.version {
            self.write_summary(&migrated)?;
        }
        Ok(migrated)
    }
}

/// Validate and migrate a session summary to the current format version.
///
/// Returns an error if the session was written by a newer version of Raven
/// (version > CURRENT_SESSION_FORMAT_VERSION). Sessions with a lower version
/// are migrated through the version chain.
fn migrate_summary(mut summary: SessionSummary) -> Result<SessionSummary> {
    if summary.version > CURRENT_SESSION_FORMAT_VERSION {
        bail!(
            "Session format version {} is newer than this build ({}). \
             Upgrade Raven to load this session.",
            summary.version,
            CURRENT_SESSION_FORMAT_VERSION
        );
    }

    // Version chain: add migration steps here as the format evolves.
    // Example for a future v1 → v2 migration:
    // if summary.version < 2 {
    //     // transform fields, rename keys, etc.
    //     summary.version = 2;
    // }

    summary.version = CURRENT_SESSION_FORMAT_VERSION;
    Ok(summary)
}

/// Render a session as readable Markdown for a local export bundle.
fn render_session_markdown(session: &Session) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Raven session {}\n\n", session.summary.id));
    out.push_str(&format!("- Model: {}\n", session.summary.model));
    out.push_str(&format!("- Created: {}\n", session.summary.created_at));
    out.push_str(&format!("- Updated: {}\n", session.summary.updated_at));
    if !session.summary.title.is_empty() {
        out.push_str(&format!("- Title: {}\n", session.summary.title));
    }
    out.push_str("\n## Messages\n");
    for (i, msg) in session.messages.iter().enumerate() {
        let heading = match msg.role.as_str() {
            "system" => "system".to_string(),
            "user" => "user".to_string(),
            "assistant" => "assistant".to_string(),
            "tool" => format!(
                "tool{}",
                msg.tool_call_id
                    .as_deref()
                    .map(|id| format!(" (`{id}`)"))
                    .unwrap_or_default()
            ),
            other => other.to_string(),
        };
        out.push_str(&format!("\n### {}. {}\n\n", i + 1, heading));
        if let Some(tcs) = &msg.tool_calls {
            for tc in tcs {
                out.push_str(&format!(
                    "- `{}` {}\n",
                    tc.function.name, tc.function.arguments
                ));
            }
            out.push('\n');
        }
        if let Some(content) = &msg.content {
            if !content.is_empty() {
                out.push_str(content);
                if !content.ends_with('\n') {
                    out.push('\n');
                }
            }
        }
    }
    out
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

/// Generate a collision-proof session ID by appending the process ID and a
/// monotonic counter suffix to the ISO timestamp
/// (e.g. `2026-08-06T00-06-18-12345-0001`). Colons are replaced with hyphens
/// so the ID is safe to use as a filename on Windows.
fn generate_session_id(iso: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let pid = std::process::id();
    let safe_iso = iso.replace(':', "-");
    format!("{safe_iso}-{pid}-{n:04x}")
}

/// Whether `id` is a bare session directory name safe to join onto the
/// sessions dir. Rejects empty ids, path separators (`/`, `\`), `.` / `..`,
/// and absolute paths so a crafted id cannot escape the sessions directory
/// (e.g. in [`SessionStore::delete`]).
fn is_bare_session_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && !id.contains('/')
        && !id.contains('\\')
        && std::path::Path::new(id).is_relative()
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

    use crate::config::{Mode, Provider, Scope, Settings};

    /// Minimal settings for `for_settings` tests, mirroring the fixture in
    /// `tui/tests.rs`.
    fn settings_for_scope(scope: Scope, workspace: &Path) -> Settings {
        Settings {
            model: "test-model".into(),
            provider: Provider::builtin("ollama").expect("ollama builtin"),
            workspace: workspace.to_path_buf(),
            max_iterations: 5,
            mode: Mode::Agent,
            scope,
            yolo: false,
            temperature: 0.2,
            max_tokens: 1024,
            rules: None,
            context_window: 8192,
            compact_threshold: 0.75,
            no_stream: false,
            verify: true,
            confirm_shell: true,
            theme: "ravenwood".into(),
            searxng_url: None,
            searxng_engines: Vec::new(),
            sandbox_extra_rw: Vec::new(),
            allow_delegate: true,
        }
    }

    #[test]
    fn for_settings_repo_roots_store_under_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        let settings = settings_for_scope(Scope::Repo, tmp.path());
        let store = SessionStore::for_settings(&settings).unwrap();
        assert_eq!(
            store.sessions_dir,
            tmp.path().join(".raven").join("sessions")
        );
        assert_eq!(store.workspace, tmp.path());
    }

    // Windows: dirs::home_dir() reads USERPROFILE, not HOME, so faking HOME
    // does not redirect the store root there — these system-scope tests are
    // Unix-only (the config tests use the same isolation precedent).
    #[cfg(unix)]
    #[test]
    fn for_settings_system_roots_store_under_home() {
        let home = tempfile::tempdir().unwrap();
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let settings = settings_for_scope(Scope::System, Path::new("/"));
        let result = SessionStore::for_settings(&settings);
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        let store = result.unwrap();
        assert_eq!(
            store.sessions_dir,
            home.path().join(".raven").join("system").join("sessions")
        );
        assert_eq!(store.workspace, home.path());
    }

    #[cfg(unix)]
    #[test]
    fn for_settings_system_round_trips_sessions() {
        let home = tempfile::tempdir().unwrap();
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let settings = settings_for_scope(Scope::System, Path::new("/"));
        let created = SessionStore::for_settings(&settings)
            .unwrap()
            .create("test-model")
            .unwrap();
        let reopened = SessionStore::for_settings(&settings).unwrap();
        let listed = reopened.list().unwrap();
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, created.summary.id);
    }

    #[cfg(unix)]
    #[test]
    fn default_export_dir_follows_store_root() {
        // Repo scope: exports land next to the workspace store.
        let tmp = tempfile::tempdir().unwrap();
        let ws_store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = ws_store.create("m").unwrap();
        assert_eq!(
            ws_store.default_export_dir(&session),
            tmp.path()
                .join(".raven")
                .join("exports")
                .join(&session.summary.id)
        );

        // System scope: exports land under ~/.raven/system/exports, next to
        // the system sessions dir (not directly under ~/.raven).
        let home = tempfile::tempdir().unwrap();
        let original_home = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        let settings = settings_for_scope(Scope::System, Path::new("/"));
        let sys_store = SessionStore::for_settings(&settings).unwrap();
        let session = sys_store.create("m").unwrap();
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(
            sys_store.default_export_dir(&session),
            home.path()
                .join(".raven")
                .join("system")
                .join("exports")
                .join(&session.summary.id)
        );
    }

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
    fn append_message_is_atomic_and_preserves_existing_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();

        let msg1 = ChatMessage {
            role: "user".into(),
            content: Some("hello".into()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        };
        let msg2 = ChatMessage {
            role: "assistant".into(),
            content: Some("hi".into()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        };

        store.append_message(&session, &msg1).unwrap();
        store.append_message(&session, &msg2).unwrap();

        let loaded = store.load(&session.summary.id).unwrap();
        assert_eq!(loaded.messages.len(), 2);
        assert_eq!(loaded.messages[0].role, "user");
        assert_eq!(loaded.messages[0].content.as_deref(), Some("hello"));
        assert_eq!(loaded.messages[1].role, "assistant");
        assert_eq!(loaded.messages[1].content.as_deref(), Some("hi"));

        let path = store.session_dir(&session.summary.id).join(MESSAGES_FILE);
        let raw = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 2);

        let leftovers: Vec<_> = std::fs::read_dir(store.session_dir(&session.summary.id))
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
    fn save_all_messages_replaces_file_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();
        store
            .append_message(
                &session,
                &ChatMessage {
                    role: "user".into(),
                    content: Some("old".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                },
            )
            .unwrap();
        let replacement = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("new".into()),
            tool_calls: None,
            tool_call_id: None,
            usage: None,
        }];
        store.save_all_messages(&session, &replacement).unwrap();
        let loaded = store.load(&session.summary.id).unwrap();
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(loaded.messages[0].content.as_deref(), Some("new"));
        let leftovers: Vec<_> = std::fs::read_dir(store.session_dir(&session.summary.id))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp files should remain");
    }

    #[test]
    fn snapshot_patch_false_without_git() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();
        let wrote = store.snapshot_patch(&session).unwrap();
        assert!(!wrote);
        let patch = store.session_dir(&session.summary.id).join("last.patch");
        let body = std::fs::read_to_string(&patch).unwrap();
        assert!(body.contains("no git repository"));
    }

    #[test]
    fn snapshot_patch_true_with_dirty_git() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(ws)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(ws)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(ws)
            .output()
            .unwrap();
        std::fs::write(ws.join("a.txt"), "v1\n").unwrap();
        std::process::Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(ws)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "seed"])
            .current_dir(ws)
            .output()
            .unwrap();
        std::fs::write(ws.join("a.txt"), "v2 dirty\n").unwrap();
        let store = SessionStore::for_workspace(ws).unwrap();
        let session = store.create("test-model").unwrap();
        let wrote = store.snapshot_patch(&session).unwrap();
        assert!(wrote, "dirty tree should produce a real patch");
        let patch =
            std::fs::read_to_string(store.session_dir(&session.summary.id).join("last.patch"))
                .unwrap();
        assert!(patch.contains("v2 dirty"), "{patch}");
    }

    #[test]
    fn export_bundle_writes_markdown_and_json() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let mut session = store.create("test-model").unwrap();
        session.summary.title = "fix parser".into();
        let _ = store.update_summary(&mut session, Some("fix parser".into()));
        store
            .append_message(
                &session,
                &ChatMessage {
                    role: "user".into(),
                    content: Some("please fix it".into()),
                    tool_calls: None,
                    tool_call_id: None,
                    usage: None,
                },
            )
            .unwrap();
        let loaded = store.load(&session.summary.id).unwrap();
        let dest = tmp.path().join("out");
        let written = store.export_bundle(&loaded, &dest).unwrap();
        assert_eq!(written, dest);
        let md = std::fs::read_to_string(dest.join("session.md")).unwrap();
        assert!(md.contains(&session.summary.id));
        assert!(md.contains("please fix it"));
        assert!(md.contains("fix parser"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dest.join("session.json")).unwrap())
                .unwrap();
        assert_eq!(json["summary"]["model"], "test-model");
        assert_eq!(json["messages"][0]["content"], "please fix it");
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
    fn generate_session_id_is_unique() {
        let base = "2026-08-06T00:06:18";
        let safe_base = "2026-08-06T00-06-18";
        let ids: Vec<String> = (0..100).map(|_| generate_session_id(base)).collect();
        let mut dedup = ids.clone();
        dedup.sort();
        dedup.dedup();
        assert_eq!(ids.len(), dedup.len(), "all 100 IDs must be unique");
        for id in &ids {
            assert!(
                id.starts_with(safe_base),
                "ID must start with timestamp: {id}"
            );
            let suffix = id.strip_prefix(&format!("{safe_base}-")).unwrap();
            let (pid_str, counter_str) = suffix.split_once('-').unwrap();
            let pid: u32 = pid_str.parse().unwrap();
            assert!(pid > 0, "PID must be non-zero: {id}");
            assert_eq!(counter_str.len(), 4, "counter must be 4 hex digits: {id}");
            u64::from_str_radix(counter_str, 16).unwrap();
        }
    }

    #[test]
    fn generate_session_id_includes_pid() {
        let id = generate_session_id("2026-08-06T00:06:18");
        let suffix = id.strip_prefix("2026-08-06T00-06-18-").unwrap();
        let (pid_str, _counter_str) = suffix.split_once('-').unwrap();
        let pid: u32 = pid_str.parse().unwrap();
        assert!(pid > 0, "PID must be non-zero: {id}");
    }

    #[test]
    fn generate_session_id_different_pids_produce_different_ids() {
        let base = "2026-08-06T00:06:18";
        let safe_base = "2026-08-06T00-06-18";
        let id1 = generate_session_id(base);
        let suffix1 = id1.strip_prefix(&format!("{safe_base}-")).unwrap();
        let (_pid1, counter1) = suffix1.split_once('-').unwrap();
        let simulated = format!("{safe_base}-99999-{counter1}");
        assert_ne!(id1, simulated, "different PID must produce different ID");
    }

    // ── Version migration ──────────────────────────────────────────────

    #[test]
    fn migrate_summary_passes_current_version() {
        let summary = SessionSummary {
            version: CURRENT_SESSION_FORMAT_VERSION,
            id: "test".into(),
            created_at: "2026-01-01T00:00:00".into(),
            updated_at: "2026-01-01T00:00:00".into(),
            model: "test-model".into(),
            title: String::new(),
        };
        let migrated = migrate_summary(summary).unwrap();
        assert_eq!(migrated.version, CURRENT_SESSION_FORMAT_VERSION);
    }

    #[test]
    fn migrate_summary_upgrades_older_version() {
        let summary = SessionSummary {
            version: 0,
            id: "test".into(),
            created_at: "2026-01-01T00:00:00".into(),
            updated_at: "2026-01-01T00:00:00".into(),
            model: "test-model".into(),
            title: String::new(),
        };
        let migrated = migrate_summary(summary).unwrap();
        assert_eq!(migrated.version, CURRENT_SESSION_FORMAT_VERSION);
    }

    #[test]
    fn migrate_summary_rejects_newer_version() {
        let summary = SessionSummary {
            version: CURRENT_SESSION_FORMAT_VERSION + 1,
            id: "test".into(),
            created_at: "2026-01-01T00:00:00".into(),
            updated_at: "2026-01-01T00:00:00".into(),
            model: "test-model".into(),
            title: String::new(),
        };
        let err = migrate_summary(summary).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("newer than this build"),
            "expected 'newer than this build' in error: {msg}"
        );
    }

    #[test]
    fn load_rejects_newer_format_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();

        // Overwrite summary.json with a future version
        let mut future = session.summary.clone();
        future.version = CURRENT_SESSION_FORMAT_VERSION + 1;
        let path = store.session_dir(&session.summary.id).join(SUMMARY_FILE);
        let content = serde_json::to_string_pretty(&future).unwrap();
        std::fs::write(&path, content).unwrap();

        let err = store.load(&session.summary.id).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("newer than this build"),
            "expected 'newer than this build' in error: {msg}"
        );
    }

    #[test]
    fn load_migrates_older_format_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();

        // Overwrite summary.json with an older version
        let mut old = session.summary.clone();
        old.version = 0;
        let path = store.session_dir(&session.summary.id).join(SUMMARY_FILE);
        let content = serde_json::to_string_pretty(&old).unwrap();
        std::fs::write(&path, content).unwrap();

        let loaded = store.load(&session.summary.id).unwrap();
        assert_eq!(loaded.summary.version, CURRENT_SESSION_FORMAT_VERSION);

        // The migration must be persisted so it runs only once.
        let on_disk: SessionSummary =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.version, CURRENT_SESSION_FORMAT_VERSION);
    }

    #[test]
    fn list_migrates_older_format_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();

        let mut old = session.summary.clone();
        old.version = 0;
        let path = store.session_dir(&session.summary.id).join(SUMMARY_FILE);
        let content = serde_json::to_string_pretty(&old).unwrap();
        std::fs::write(&path, content).unwrap();

        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, session.summary.id);

        // Persisted on disk after listing.
        let on_disk: SessionSummary =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.version, CURRENT_SESSION_FORMAT_VERSION);
    }

    #[test]
    fn list_skips_future_format_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let session = store.create("test-model").unwrap();

        let mut future = session.summary.clone();
        future.version = CURRENT_SESSION_FORMAT_VERSION + 1;
        let path = store.session_dir(&session.summary.id).join(SUMMARY_FILE);
        let content = serde_json::to_string_pretty(&future).unwrap();
        std::fs::write(&path, content).unwrap();

        // A future-version session is skipped from the listing rather than
        // surfacing a hard error for the whole list.
        let metas = store.list().unwrap();
        assert!(metas.is_empty());
    }

    #[test]
    fn delete_removes_session_from_listing() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let a = store.create("test-model").unwrap();
        let b = store.create("test-model").unwrap();

        // Both sessions are listed before deletion.
        assert_eq!(store.list().unwrap().len(), 2);

        store.delete(&a.summary.id).unwrap();
        let metas = store.list().unwrap();
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].id, b.summary.id);

        // The session directory is actually gone from disk.
        assert!(!store.session_dir(&a.summary.id).exists());
    }

    #[test]
    fn delete_missing_session_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        assert!(store.delete("does-not-exist").is_err());
    }

    #[test]
    fn delete_rejects_traversal_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();

        // A crafted id must not escape the sessions directory.
        for bad in ["..", "../..", "a/../../evil", "a\\..\\..", "/abs/path", ""] {
            assert!(store.delete(bad).is_err(), "delete must reject id {bad:?}");
        }
    }

    #[test]
    fn is_bare_session_id_classification() {
        assert!(is_bare_session_id("2026-08-06T00-06-18-123-0001"));
        assert!(!is_bare_session_id(".."));
        assert!(!is_bare_session_id("."));
        assert!(!is_bare_session_id(""));
        assert!(!is_bare_session_id("a/b"));
        assert!(!is_bare_session_id("a\\b"));
        assert!(!is_bare_session_id("/abs"));
    }

    #[test]
    fn update_model_persists_new_model_and_timestamp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let mut session = store.create("test-model").unwrap();

        store
            .update_model(&mut session, "deepseek-v4-pro:cloud")
            .unwrap();

        // In-memory summary reflects the new model.
        assert_eq!(session.summary.model, "deepseek-v4-pro:cloud");

        // Reloading from disk reflects the new model too.
        let loaded = store.load(&session.summary.id).unwrap();
        assert_eq!(loaded.summary.model, "deepseek-v4-pro:cloud");
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
