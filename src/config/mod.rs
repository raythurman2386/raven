//! Configuration: `Settings`, environment defaults, context-window inference,
//! `AGENTS.md` / `CLAUDE.md` loading, and config file (`config.toml`).
//!
//! Precedence: CLI flag > env var > workspace config.toml > global config.toml > built-in default.
//!
//! # Environment variable precedence
//!
//! `RAVEN_*` vars take priority over legacy `OG_*` fallbacks:
//! - `RAVEN_MAX_ITER` > `OG_MAX_ITER`
//! - `RAVEN_CONTEXT_WINDOW` > `OG_CONTEXT_WINDOW`
//! - `RAVEN_COMPACT_THRESHOLD` > `OG_COMPACT_THRESHOLD`
//!
//! Provider API keys are resolved per provider: config-file `api_key` (if set)
//! → `RAVEN_API_KEY` (universal override) → the provider's declared
//! `api_key_env` (or built-in mapping, e.g. `OPENROUTER_API_KEY` /
//! `OLLAMA_API_KEY`).
//!
//! A repo-root or CWD `.env` file is loaded early by the binary (see
//! [`load_dotenv`]) without overriding already-exported shell variables.

use anyhow::Result;
use serde::Deserialize;
use std::path::PathBuf;

pub use onboarding::{config_paths, fallback_models, needs_onboarding, run_onboarding};
pub use provider::{
    is_known_provider, known_provider_names, resolve_provider, Provider, ProviderConfig,
};

mod onboarding;
mod provider;

/// The user-facing interaction mode for a session.
///
/// - [`Mode::Agent`] — full toolset, no plan step. The model works directly.
///   This is the default.
/// - [`Mode::Plan`] — propose a plan first (read-only toolset), then execute
///   after approval.
/// - [`Mode::Chat`] — read-only toolset, no plan step. For Q&A / exploration
///   without modifying the workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum Mode {
    Plan,
    Agent,
    Chat,
}

impl Mode {
    /// The next mode when cycling forward (Shift+Tab in the TUI).
    pub fn next(self) -> Mode {
        match self {
            Mode::Plan => Mode::Agent,
            Mode::Agent => Mode::Chat,
            Mode::Chat => Mode::Plan,
        }
    }

    /// Whether this mode proposes a plan before executing.
    pub fn plans_first(self) -> bool {
        matches!(self, Mode::Plan)
    }

    /// Whether this mode restricts the toolset to read-only.
    pub fn read_only(self) -> bool {
        matches!(self, Mode::Plan | Mode::Chat)
    }

    /// Human-readable label for the mode.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Plan => "plan",
            Mode::Agent => "agent",
            Mode::Chat => "chat",
        }
    }

    /// Parse a mode id as advertised over ACP (`plan` / `agent` / `chat`).
    pub fn from_id(id: &str) -> Option<Self> {
        match id {
            "plan" => Some(Mode::Plan),
            "agent" => Some(Mode::Agent),
            "chat" => Some(Mode::Chat),
            _ => None,
        }
    }
}

/// Resolve the effective interaction mode from the configured/explicit mode and
/// the `--yolo` flag.
///
/// `--yolo` implies `--mode agent`: it disables the plan step and the shell
/// confirmation gate, and must expose the full (write/shell) toolset. Without
/// this, `raven --yolo` (no explicit `--mode`) would keep a configured
/// `Mode::Plan` / `Mode::Chat` and silently degrade to a read-only toolset,
/// leaving the model unable to write files or run shell (see issue #126).
///
/// An explicit CLI `--mode` (`explicit_mode = Some`) takes precedence over the
/// yolo-implied agent mode, so a user can still request `--mode chat --yolo`
/// for autonomous read-only exploration. The config-file `mode` does *not*
/// count as explicit — only a CLI flag pins the mode.
pub fn resolve_mode(explicit_mode: Option<Mode>, config_mode: Option<Mode>, yolo: bool) -> Mode {
    let base = explicit_mode.or(config_mode).unwrap_or(Mode::Agent);
    if yolo && explicit_mode.is_none() {
        Mode::Agent
    } else {
        base
    }
}

/// The operational scope of a session.
///
/// - [`Scope::Repo`] — the default. The agent operates inside a single repo
///   workspace; the sandbox is rooted at that workspace (Landlock RW only there,
///   seccomp network-block, `confirm_shell` gate). This is the reviewed harness.
/// - [`Scope::System`] — opt-in via `--system`. The agent administers the whole
///   OS: the sandbox is rooted at `/` (write-everywhere at the Landlock layer),
///   system scope uses its own system prompt and persistence, and
///   `confirm_shell` is forced on so no destructive command runs unconfirmed.
///   Intended for trusted single-user machines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Repo,
    System,
}

impl Scope {
    /// Whether this is the system-administration scope.
    pub fn is_system(self) -> bool {
        matches!(self, Scope::System)
    }

    /// Human-readable label for the scope.
    pub fn label(self) -> &'static str {
        match self {
            Scope::Repo => "repo",
            Scope::System => "system",
        }
    }
}

/// Runtime configuration for an [`crate::agent::Agent`].
///
/// Constructed from CLI flags + environment variables in `crate::main`.
/// All fields are public so callers (TUI, headless runner, sub-agents) can
/// clone and inspect them.
///
/// # Invariants
///
/// - `workspace` must be an existing directory (verified at construction).
/// - `context_window` > 0 and `compact_threshold` is in `(0.0, 1.0]`.
/// - `max_tokens` is derived from the context window and clamped per
///   iteration inside the agent loop.
#[derive(Debug, Clone)]
pub struct Settings {
    pub model: String,
    /// The resolved provider (endpoint + auth + default model).
    pub provider: Provider,
    pub workspace: PathBuf,
    pub max_iterations: usize,
    pub mode: Mode,
    /// The operational scope: default `Repo` (single-repo workspace) or opt-in
    /// `System` (whole-OS administration). Determines the sandbox root, the
    /// system prompt, and which persistence/toolset gating applies.
    pub scope: Scope,
    pub yolo: bool,
    pub temperature: f32,
    pub max_tokens: u32,
    /// Extra rules appended to the system prompt (from `--rules`).
    pub rules: Option<String>,
    /// Context window size in tokens for the active model.
    pub context_window: usize,
    /// Fraction of (context_window - output_reserve) at which compaction triggers (0.0–1.0).
    pub compact_threshold: f32,
    /// Disable streaming and use a single non-streaming request instead.
    pub no_stream: bool,
    /// When true, the agent must call `run_tests` after editing files before
    /// it can finish a turn (enforced verification gate). Default on.
    pub verify: bool,
    /// When true, every `run_shell` command is confirmed with the user first
    /// (via the same ask_user channel). Off with `--yolo`.
    pub confirm_shell: bool,
    /// The active color theme name (e.g. `ravenwood`, `nord`). Resolved to a
    /// [`crate::tui::Theme`] at TUI startup; unknown names fall back to the
    /// default.
    pub theme: String,
    /// Optional self-hosted SearXNG base URL (e.g. `http://127.0.0.1:8080`).
    /// When set, `web_search` queries its JSON API; otherwise it falls back to
    /// DuckDuckGo's HTML endpoint. No API key required.
    pub searxng_url: Option<String>,
    /// Optional SearXNG engine list override (e.g. `["google", "bing"]`).
    /// When empty, the server's default engines are used.
    pub searxng_engines: Vec<String>,
    /// Extra Landlock RW roots granted to every confined subprocess (e.g. a
    /// git worktree sub-agent's shared main repo, which lives as a sibling
    /// under the temp dir). Defaults to empty; only set by parallel sub-agent
    /// orchestration. Linux-only (no effect on Windows).
    pub sandbox_extra_rw: Vec<PathBuf>,
    /// When false, `delegate_task` / `goal_set` / `todo_write` are rejected.
    /// Cleared on spawned sub-agents so they cannot nest or overwrite the
    /// parent's persisted goal and task list.
    pub allow_delegate: bool,
}

impl Settings {
    /// The provider's base URL (OpenAI-compatible `/v1/chat/completions`).
    pub fn base_url(&self) -> &str {
        &self.provider.base_url
    }

    /// The provider's API key, if any.
    pub fn api_key(&self) -> Option<&str> {
        self.provider.api_key.as_deref()
    }

    /// Verify the workspace directory exists; bail with a path message if not.
    pub fn ensure_workspace(&self) -> Result<()> {
        if !self.workspace.is_dir() {
            anyhow::bail!("Workspace does not exist: {}", self.workspace.display());
        }
        Ok(())
    }

    /// Output token budget derived from the context window: `window / 8`,
    /// clamped to `[1024, 32768]`.
    ///
    /// This is the *initial* `max_tokens`; the agent loop further clamps it
    /// per iteration so `prompt_tokens + max_tokens + 64 <= context_window`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // derived_max_tokens is an associated fn on Settings:
    /// // 8192  / 8 = 1024 (clamped up to 1024)
    /// // 32768 / 8 = 4096
    /// // 128_000 / 8 = 16000 (within range, no clamping)
    /// // 1_000_000 / 8 = 125000 (clamped down to 32768)
    /// assert_eq!(Settings::derived_max_tokens(8192), 1024);
    /// assert_eq!(Settings::derived_max_tokens(32768), 4096);
    /// assert_eq!(Settings::derived_max_tokens(128_000), 16000);
    /// assert_eq!(Settings::derived_max_tokens(1_000_000), 32768);
    /// ```
    pub fn derived_max_tokens(context_window: usize) -> u32 {
        // Output budget: window / 8, clamped to a generous ceiling so long,
        // detailed responses aren't cut short. 32k covers even 1M-token
        // cloud contexts comfortably while staying a sane per-call cap.
        let raw = context_window / 8;
        raw.clamp(1024, 32_768) as u32
    }

    /// Temperature rounded to 4 decimal places for JSON serialization.
    ///
    /// f32 → f64 conversion produces artifacts like `0.20000000298023224`,
    /// which some OpenRouter providers (e.g. Stealth) reject with HTTP 400.
    pub fn temperature_json(&self) -> f64 {
        round_temperature(self.temperature)
    }
}

/// Round an f32 temperature to 4 decimal places, returning a clean f64.
pub(crate) fn round_temperature(t: f32) -> f64 {
    let v = (f64::from(t) * 10_000.0).round() / 10_000.0;
    // Normalize -0.0 to 0.0
    if v == 0.0 {
        0.0
    } else {
        v
    }
}

/// A named model endpoint (Ollama, OpenRouter, Ollama Cloud, …).
///
/// Maximum number of characters read from an AGENTS.md-style file.
const MAX_AGENTS_MD_CHARS: usize = 8000;

/// Load optional project instructions from the workspace.
///
/// Checks, in order: `AGENTS.md`, `CLAUDE.md`, `.grok/AGENTS.md`, `AGENT.md`.
/// Returns the contents of the first match (truncated to 8000 chars with a
/// trailing truncation marker when trimmed), or an empty string if none are
/// found.
pub fn load_agents_md(workspace: &std::path::Path) -> String {
    const CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".grok/AGENTS.md", "AGENT.md"];
    for name in CANDIDATES {
        let p = workspace.join(name);
        if p.is_file() {
            if let Ok(text) = std::fs::read_to_string(&p) {
                return truncate_agents_md(&text);
            }
        }
    }
    String::new()
}

/// Truncate AGENTS.md content to 8000 chars, appending a marker when content
/// was cut so the model knows it is incomplete.
fn truncate_agents_md(content: &str) -> String {
    let char_count = content.chars().count();
    if char_count > MAX_AGENTS_MD_CHARS {
        let truncated: String = content.chars().take(MAX_AGENTS_MD_CHARS).collect();
        format!("{truncated}\n\n[truncated: content exceeds {MAX_AGENTS_MD_CHARS} chars]")
    } else {
        content.to_string()
    }
}

/// Load `KEY=VALUE` pairs from a `.env` file into the process environment.
///
/// Existing variables are **not** overwritten. Lines may be blank, comments
/// (`#…`), or `KEY=VALUE` with optional single/double quotes around the value.
/// Returns the number of keys newly set.
pub fn load_dotenv(path: &std::path::Path) -> usize {
    let Ok(text) = std::fs::read_to_string(path) else {
        return 0;
    };
    let mut set = 0usize;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, val)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || std::env::var_os(key).is_some() {
            continue;
        }
        let mut val = val.trim().to_string();
        if val.len() >= 2 {
            let bytes = val.as_bytes();
            if (bytes[0] == b'"' && bytes[val.len() - 1] == b'"')
                || (bytes[0] == b'\'' && bytes[val.len() - 1] == b'\'')
            {
                val = val[1..val.len() - 1].to_string();
            }
        }
        // SAFETY: single-threaded at startup before other threads spawn; we only
        // insert keys that are not already present.
        std::env::set_var(key, val);
        set += 1;
    }
    set
}

/// Load `.env` from `dir/.env` then `cwd/.env` (first wins per-key via no-overwrite).
pub fn load_dotenv_from(dir: &std::path::Path) {
    let _ = load_dotenv(&dir.join(".env"));
    if let Ok(cwd) = std::env::current_dir() {
        if cwd != dir {
            let _ = load_dotenv(&cwd.join(".env"));
        }
    }
}

/// Load `~/.raven/.env` if it exists, for provider API keys written by the
/// onboarding wizard. Read-only, no-overwrite semantics (same as
/// [`load_dotenv`]). Returns the number of keys newly set.
pub fn load_global_dotenv() -> usize {
    match dirs::home_dir() {
        Some(home) => load_dotenv(&home.join(".raven").join(".env")),
        None => 0,
    }
}

/// Max agent iterations: `RAVEN_MAX_ITER` env var, else `OG_MAX_ITER`, else `60`.
pub fn default_max_iter() -> usize {
    std::env::var("RAVEN_MAX_ITER")
        .or_else(|_| std::env::var("OG_MAX_ITER"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

/// Context window override from `RAVEN_CONTEXT_WINDOW` or `OG_CONTEXT_WINDOW` (tokens).
pub fn env_context_window() -> Option<usize> {
    std::env::var("RAVEN_CONTEXT_WINDOW")
        .or_else(|_| std::env::var("OG_CONTEXT_WINDOW"))
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Compact threshold override from `RAVEN_COMPACT_THRESHOLD` or `OG_COMPACT_THRESHOLD` (0.0–1.0).
pub fn env_compact_threshold() -> Option<f32> {
    std::env::var("RAVEN_COMPACT_THRESHOLD")
        .or_else(|_| std::env::var("OG_COMPACT_THRESHOLD"))
        .ok()
        .and_then(|s| s.parse().ok())
}

/// Optional SearXNG base URL from `RAVEN_SEARXNG_URL` (e.g.
/// `http://127.0.0.1:8080` or `https://searx.example.com`). Empty/whitespace
/// values are treated as absent, so a private install does not require it.
pub fn env_searxng_url() -> Option<String> {
    std::env::var("RAVEN_SEARXNG_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Optional comma-separated SearXNG engine list from `RAVEN_SEARXNG_ENGINES`
/// (e.g. `google,duckduckgo,bing`). Empty values are treated as absent, which
/// leaves engine selection to the SearXNG server defaults.
pub fn env_searxng_engines() -> Option<Vec<String>> {
    let raw = std::env::var("RAVEN_SEARXNG_ENGINES").ok()?;
    let engines: Vec<String> = raw
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if engines.is_empty() {
        None
    } else {
        Some(engines)
    }
}

// ── Config file ────────────────────────────────────────────────────────

/// Config file loaded from `~/.raven/config.toml` or `.raven/config.toml`.
///
/// All fields optional — only overrides built-in defaults when present.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfigFile {
    /// Active provider name (e.g. `ollama`, `openrouter`). Resolved against
    /// `providers` or the built-in presets.
    pub provider: Option<String>,
    /// Named provider definitions. Keys are provider names.
    #[serde(default)]
    pub providers: std::collections::HashMap<String, ProviderConfig>,
    pub context_window: Option<usize>,
    pub compact_threshold: Option<f32>,
    pub max_iterations: Option<usize>,
    pub mode: Option<Mode>,
    pub temperature: Option<f32>,
    /// Disable streaming and use a single non-streaming request instead.
    pub no_stream: Option<bool>,
    /// Enforce the agent runs tests after editing files before finishing.
    pub verify: Option<bool>,
    /// Color theme name (e.g. `ravenwood`, `nord`).
    pub theme: Option<String>,
    /// Optional self-hosted SearXNG base URL (e.g. `http://127.0.0.1:8080`).
    pub searxng_url: Option<String>,
    /// Optional comma-separated SearXNG engine list (e.g. `"google,bing"`).
    pub searxng_engines: Option<Vec<String>>,
}

/// Load config from workspace `.raven/config.toml` then `~/.raven/config.toml`.
///
/// Workspace config takes priority over global. Returns default (all None) if
/// neither file exists.
pub fn load_config_file(workspace: &std::path::Path) -> ConfigFile {
    // Try workspace config first (higher priority)
    let ws_config = workspace.join(".raven").join("config.toml");
    let ws = load_toml_file(&ws_config);

    // Then global config (lower priority)
    let global = dirs::home_dir()
        .map(|h| h.join(".raven").join("config.toml"))
        .map(|p| load_toml_file(&p))
        .unwrap_or_default();

    // Merge: workspace overrides global. Provider tables merge field-wise so a
    // workspace entry that only sets `base_url` keeps global `api_key_env` /
    // `default_model` rather than wiping them with `None`.
    ConfigFile {
        provider: ws.provider.or(global.provider),
        providers: {
            let mut m = global.providers;
            for (k, overlay) in ws.providers {
                match m.remove(&k) {
                    Some(base) => {
                        m.insert(k, base.merge(overlay));
                    }
                    None => {
                        m.insert(k, overlay);
                    }
                }
            }
            m
        },
        context_window: ws.context_window.or(global.context_window),
        compact_threshold: ws.compact_threshold.or(global.compact_threshold),
        max_iterations: ws.max_iterations.or(global.max_iterations),
        mode: ws.mode.or(global.mode),
        temperature: ws.temperature.or(global.temperature),
        no_stream: ws.no_stream.or(global.no_stream),
        verify: ws.verify.or(global.verify),
        theme: ws.theme.or(global.theme),
        searxng_url: ws.searxng_url.or(global.searxng_url),
        searxng_engines: ws.searxng_engines.or(global.searxng_engines),
    }
}

fn load_toml_file(path: &std::path::Path) -> ConfigFile {
    match std::fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!("failed to parse {}: {}", path.display(), e);
            ConfigFile::default()
        }),
        Err(_) => ConfigFile::default(),
    }
}

#[cfg(test)]
mod tests;
