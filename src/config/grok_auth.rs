//! Grok Build subscription auth for the `grok` provider.
//!
//! Reads SpaceXAI OIDC credentials from `~/.grok/auth.json` (the same file
//! Grok Build writes via `grok login`), refreshes them when near expiry, and
//! returns the access token plus the CLI chat-proxy identity headers the
//! proxy requires.
//!
//! This module does **not** run an interactive login — users authenticate with
//! `grok login` (or device-code) first. Raven reuses that session so
//! subscription billing flows through `https://cli-chat-proxy.grok.com/v1`.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default OpenAI-compatible root for Grok Build's subscription proxy.
pub const DEFAULT_PROXY_BASE_URL: &str = "https://cli-chat-proxy.grok.com/v1";

/// Header value that marks the bearer token as a Grok CLI session credential.
const TOKEN_AUTH_VALUE: &str = "xai-grok-cli";

/// Refresh this many seconds before `expires_at`.
const EARLY_REFRESH_SECS: u64 = 300;

/// One credential entry inside `~/.grok/auth.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrokAuthEntry {
    /// Access token (Grok Build stores this as `key`).
    pub key: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub oidc_issuer: Option<String>,
    #[serde(default)]
    pub oidc_client_id: Option<String>,
    #[serde(default)]
    pub auth_mode: Option<String>,
    /// Preserve unknown fields so a rewrite does not drop Grok Build metadata.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

/// Resolved session ready to attach to inference requests.
#[derive(Debug, Clone)]
pub struct GrokSession {
    pub access_token: String,
    pub request_headers: Vec<(String, String)>,
}

/// Errors loading or refreshing Grok subscription credentials.
#[derive(Debug, thiserror::Error)]
pub enum GrokAuthError {
    #[error("Grok auth file not found at {0}; run `grok login` first")]
    MissingFile(PathBuf),
    #[error("failed to read Grok auth file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Grok auth file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("Grok auth file {0} has no usable SpaceXAI credential; run `grok login`")]
    Empty(PathBuf),
    #[error("Grok session token expired and no refresh_token is available; run `grok login`")]
    ExpiredNoRefresh,
    #[error("Grok token refresh failed: {0}")]
    Refresh(String),
    #[error("failed to write refreshed Grok auth to {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Path to `auth.json`: `RAVEN_GROK_AUTH` > `$GROK_HOME/auth.json` > `~/.grok/auth.json`.
pub fn auth_path() -> PathBuf {
    if let Ok(p) = std::env::var("RAVEN_GROK_AUTH") {
        let trimmed = p.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    if let Ok(home) = std::env::var("GROK_HOME") {
        let trimmed = home.trim();
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed).join("auth.json");
        }
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".grok")
        .join("auth.json")
}

/// Proxy base URL: `RAVEN_GROK_PROXY_BASE_URL` > `GROK_CLI_CHAT_PROXY_BASE_URL` > default.
pub fn proxy_base_url() -> String {
    std::env::var("RAVEN_GROK_PROXY_BASE_URL")
        .or_else(|_| std::env::var("GROK_CLI_CHAT_PROXY_BASE_URL"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_PROXY_BASE_URL.to_string())
}

/// Identity headers the CLI chat proxy requires (semver client version).
pub fn client_headers() -> Vec<(String, String)> {
    vec![
        ("X-XAI-Token-Auth".into(), TOKEN_AUTH_VALUE.into()),
        (
            "x-grok-client-version".into(),
            env!("CARGO_PKG_VERSION").into(),
        ),
        ("x-grok-client-identifier".into(), "raven".into()),
    ]
}

/// Load `~/.grok/auth.json`, refresh if needed, and return a bearer session.
pub fn resolve_session() -> Result<GrokSession, GrokAuthError> {
    resolve_session_at(&auth_path())
}

/// Always exchange the refresh token, persist it, and return the new access token.
///
/// Used after the chat proxy returns 401. A still-valid access token is not reused.
pub fn force_refresh() -> Result<String, GrokAuthError> {
    force_refresh_at(&auth_path())
}

/// Same as [`force_refresh`] with an explicit auth file path (tests).
pub fn force_refresh_at(path: &Path) -> Result<String, GrokAuthError> {
    let mut file = load_auth_file(path)?;
    let (entry_key, mut entry) = select_entry(&file, path)?;
    refresh_entry(&mut entry)?;
    file.insert(entry_key, entry.clone());
    write_auth_file(path, &file)?;
    if entry.key.trim().is_empty() {
        return Err(GrokAuthError::Empty(path.to_path_buf()));
    }
    Ok(entry.key)
}

/// Same as [`resolve_session`] but with an explicit auth file path (tests).
pub fn resolve_session_at(path: &Path) -> Result<GrokSession, GrokAuthError> {
    let mut file = load_auth_file(path)?;
    let (entry_key, mut entry) = select_entry(&file, path)?;

    if needs_refresh(&entry) {
        match refresh_entry(&mut entry) {
            Ok(()) => {
                file.insert(entry_key.clone(), entry.clone());
                if let Err(e) = write_auth_file(path, &file) {
                    tracing::warn!("grok auth: refreshed token but failed to persist: {e}");
                }
            }
            Err(e) => {
                // If the access token is still within a soft window, keep going;
                // otherwise surface the refresh failure.
                if entry_expired(&entry) {
                    return Err(e);
                }
                tracing::warn!("grok auth: refresh failed ({e}); using cached access token");
            }
        }
    }

    if entry.key.trim().is_empty() {
        return Err(GrokAuthError::Empty(path.to_path_buf()));
    }

    Ok(GrokSession {
        access_token: entry.key,
        request_headers: client_headers(),
    })
}

fn load_auth_file(path: &Path) -> Result<BTreeMap<String, GrokAuthEntry>, GrokAuthError> {
    if !path.is_file() {
        return Err(GrokAuthError::MissingFile(path.to_path_buf()));
    }
    let raw = fs::read_to_string(path).map_err(|source| GrokAuthError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_str(&raw).map_err(|source| GrokAuthError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn write_auth_file(
    path: &Path,
    file: &BTreeMap<String, GrokAuthEntry>,
) -> Result<(), GrokAuthError> {
    let json = serde_json::to_vec_pretty(file).map_err(|source| GrokAuthError::Parse {
        path: path.to_path_buf(),
        source,
    })?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| GrokAuthError::Write {
        path: path.to_path_buf(),
        source,
    })?;

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = parent.join(format!(".auth.{}.{}.tmp", std::process::id(), n));
    fs::write(&tmp, &json).map_err(|source| GrokAuthError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600));
    }
    fs::rename(&tmp, path).map_err(|source| GrokAuthError::Write {
        path: path.to_path_buf(),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Prefer a SpaceXAI (`auth.x.ai`) entry; otherwise the latest-expiring entry.
fn select_entry(
    file: &BTreeMap<String, GrokAuthEntry>,
    path: &Path,
) -> Result<(String, GrokAuthEntry), GrokAuthError> {
    if file.is_empty() {
        return Err(GrokAuthError::Empty(path.to_path_buf()));
    }

    let spacexai = file.iter().find(|(k, e)| {
        k.contains("auth.x.ai")
            || e.oidc_issuer
                .as_deref()
                .is_some_and(|i| i.contains("auth.x.ai"))
    });
    if let Some((k, e)) = spacexai {
        return Ok((k.clone(), e.clone()));
    }

    let mut best: Option<(String, GrokAuthEntry, Option<SystemTime>)> = None;
    for (k, e) in file {
        let exp = e.expires_at.as_deref().and_then(parse_rfc3339);
        match &best {
            None => best = Some((k.clone(), e.clone(), exp)),
            Some((_, _, best_exp)) => {
                let take = match (exp, *best_exp) {
                    (Some(a), Some(b)) => a > b,
                    (Some(_), None) => true,
                    _ => false,
                };
                if take {
                    best = Some((k.clone(), e.clone(), exp));
                }
            }
        }
    }
    best.map(|(k, e, _)| (k, e))
        .ok_or_else(|| GrokAuthError::Empty(path.to_path_buf()))
}

fn needs_refresh(entry: &GrokAuthEntry) -> bool {
    match entry.expires_at.as_deref().and_then(parse_rfc3339) {
        Some(expires) => {
            let threshold = SystemTime::now() + Duration::from_secs(EARLY_REFRESH_SECS);
            expires <= threshold
        }
        // No expiry metadata: treat as refreshable so a stale token can heal.
        None => entry.refresh_token.is_some(),
    }
}

fn entry_expired(entry: &GrokAuthEntry) -> bool {
    match entry.expires_at.as_deref().and_then(parse_rfc3339) {
        Some(expires) => expires <= SystemTime::now(),
        None => false,
    }
}

fn refresh_entry(entry: &mut GrokAuthEntry) -> Result<(), GrokAuthError> {
    let refresh = entry
        .refresh_token
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or(GrokAuthError::ExpiredNoRefresh)?;
    let issuer = entry
        .oidc_issuer
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("https://auth.x.ai");
    let client_id = entry
        .oidc_client_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| GrokAuthError::Refresh("credential is missing oidc_client_id".into()))?;

    let token_url = format!("{}/oauth2/token", issuer.trim_end_matches('/'));
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| GrokAuthError::Refresh(e.to_string()))?;

    let resp = client
        .post(&token_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("client_id", client_id),
        ])
        .send()
        .map_err(|e| GrokAuthError::Refresh(e.to_string()))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        return Err(GrokAuthError::Refresh(format!(
            "HTTP {status}: {}",
            body.chars().take(200).collect::<String>()
        )));
    }

    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: String,
        refresh_token: Option<String>,
        expires_in: Option<u64>,
    }

    let tok: TokenResponse = resp
        .json()
        .map_err(|e| GrokAuthError::Refresh(e.to_string()))?;
    if tok.access_token.trim().is_empty() {
        return Err(GrokAuthError::Refresh(
            "token endpoint returned an empty access_token".into(),
        ));
    }

    entry.key = tok.access_token;
    if let Some(rt) = tok.refresh_token.filter(|s| !s.trim().is_empty()) {
        entry.refresh_token = Some(rt);
    }
    let expires_in = tok.expires_in.unwrap_or(21_600);
    entry.expires_at = Some(format_rfc3339(
        SystemTime::now() + Duration::from_secs(expires_in),
    ));
    Ok(())
}

/// Parse a subset of RFC3339 used by Grok Build (`…Z`, optional fractional secs).
fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    if !s.ends_with('Z') {
        return None;
    }
    let body = &s[..s.len() - 1];
    let (date, time) = body.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;

    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, Some(frac)),
        None => (time, None),
    };
    let mut t = hms.split(':');
    let hour: u32 = t.next()?.parse().ok()?;
    let minute: u32 = t.next()?.parse().ok()?;
    let second: u32 = t.next()?.parse().ok()?;
    if t.next().is_some() {
        return None;
    }

    let nanos = match frac {
        Some(f) => {
            let digits: String = f.chars().filter(|c| c.is_ascii_digit()).take(9).collect();
            if digits.is_empty() {
                0
            } else {
                let padded = format!("{digits:0<9}");
                padded.parse::<u32>().ok()?
            }
        }
        None => 0,
    };

    let days = days_from_civil(year, month, day)?;
    let secs = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::new(secs as u64, nanos))
}

fn format_rfc3339(ts: SystemTime) -> String {
    let dur = ts.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let nanos = dur.subsec_nanos();
    let (year, month, day) = civil_from_days(secs.div_euclid(86_400));
    let tod = secs.rem_euclid(86_400) as u32;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{nanos:09}Z")
}

/// Howard Hinnant civil-from-days (UTC).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m as u32, d as u32)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp as u64 + 2) / 5 + day as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe as i64 - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_entry(expires_at: &str) -> GrokAuthEntry {
        GrokAuthEntry {
            key: "access-token".into(),
            refresh_token: Some("refresh-token".into()),
            expires_at: Some(expires_at.into()),
            oidc_issuer: Some("https://auth.x.ai".into()),
            oidc_client_id: Some("client-id".into()),
            auth_mode: Some("oidc".into()),
            extra: BTreeMap::from([("email".into(), Value::String("user@example.com".into()))]),
        }
    }

    #[test]
    fn parse_rfc3339_accepts_grok_timestamps() {
        let t = parse_rfc3339("2026-09-21T18:42:31.630253080Z").expect("parse");
        let formatted = format_rfc3339(t);
        assert!(formatted.starts_with("2026-09-21T18:42:31."));
        assert!(formatted.ends_with('Z'));
    }

    #[test]
    fn needs_refresh_when_within_early_window() {
        let soon = format_rfc3339(SystemTime::now() + Duration::from_secs(60));
        let entry = sample_entry(&soon);
        assert!(needs_refresh(&entry));

        let later = format_rfc3339(SystemTime::now() + Duration::from_secs(3600));
        let entry = sample_entry(&later);
        assert!(!needs_refresh(&entry));
    }

    #[test]
    fn select_entry_prefers_auth_x_ai() {
        let mut file: BTreeMap<String, GrokAuthEntry> = BTreeMap::new();
        file.insert(
            "https://other.example::1".into(),
            GrokAuthEntry {
                key: "other".into(),
                refresh_token: None,
                expires_at: Some(format_rfc3339(
                    SystemTime::now() + Duration::from_secs(9999),
                )),
                oidc_issuer: Some("https://other.example".into()),
                oidc_client_id: Some("1".into()),
                auth_mode: None,
                extra: BTreeMap::new(),
            },
        );
        file.insert(
            "https://auth.x.ai::abc".into(),
            sample_entry(&format_rfc3339(
                SystemTime::now() + Duration::from_secs(100),
            )),
        );
        let (k, e) = select_entry(&file, Path::new("/tmp/auth.json")).unwrap();
        assert_eq!(k, "https://auth.x.ai::abc");
        assert_eq!(e.key, "access-token");
    }

    #[test]
    fn resolve_session_at_reads_fresh_token_without_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut file: BTreeMap<String, GrokAuthEntry> = BTreeMap::new();
        let later = format_rfc3339(SystemTime::now() + Duration::from_secs(7200));
        file.insert("https://auth.x.ai::abc".into(), sample_entry(&later));
        let json = serde_json::to_vec_pretty(&file).unwrap();
        fs::write(&path, json).unwrap();

        let session = resolve_session_at(&path).unwrap();
        assert_eq!(session.access_token, "access-token");
        assert!(session
            .request_headers
            .iter()
            .any(|(k, v)| k == "X-XAI-Token-Auth" && v == TOKEN_AUTH_VALUE));
        assert!(session
            .request_headers
            .iter()
            .any(|(k, v)| k == "x-grok-client-identifier" && v == "raven"));
        assert!(session
            .request_headers
            .iter()
            .any(|(k, v)| k == "x-grok-client-version" && v == env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn expired_session_without_refresh_token_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut file: BTreeMap<String, GrokAuthEntry> = BTreeMap::new();
        let past = format_rfc3339(SystemTime::now() - Duration::from_secs(3600));
        let mut entry = sample_entry(&past);
        entry.refresh_token = None;
        file.insert("https://auth.x.ai::abc".into(), entry);
        fs::write(&path, serde_json::to_vec_pretty(&file).unwrap()).unwrap();

        let err = resolve_session_at(&path).unwrap_err();
        assert!(matches!(err, GrokAuthError::ExpiredNoRefresh));
        let err = force_refresh_at(&path).unwrap_err();
        assert!(matches!(err, GrokAuthError::ExpiredNoRefresh));
        assert!(err.to_string().contains("grok login"));
    }

    #[test]
    fn resolve_session_at_missing_file_is_clear_error() {
        let err = resolve_session_at(Path::new("/no/such/grok-auth.json")).unwrap_err();
        assert!(matches!(err, GrokAuthError::MissingFile(_)));
        assert!(err.to_string().contains("grok login"));
    }

    #[test]
    fn write_auth_file_preserves_extra_fields_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let mut file: BTreeMap<String, GrokAuthEntry> = BTreeMap::new();
        let later = format_rfc3339(SystemTime::now() + Duration::from_secs(7200));
        file.insert("https://auth.x.ai::abc".into(), sample_entry(&later));
        write_auth_file(&path, &file).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("user@example.com"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn client_headers_use_numeric_semver() {
        let version = client_headers()
            .into_iter()
            .find(|(k, _)| k == "x-grok-client-version")
            .unwrap()
            .1;
        // Proxy rejects non-semver values like "raven-0.6.5".
        assert!(version.chars().next().unwrap().is_ascii_digit());
        assert!(!version.contains("raven"));
    }

    #[test]
    fn proxy_base_url_default() {
        assert_eq!(DEFAULT_PROXY_BASE_URL, "https://cli-chat-proxy.grok.com/v1");
    }
}
