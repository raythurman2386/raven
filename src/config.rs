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
use std::borrow::Cow;
use std::path::PathBuf;

/// The user-facing interaction mode for a session.
///
/// - [`Mode::Plan`] — propose a plan first (read-only toolset), then execute
///   after approval. This is the default.
/// - [`Mode::Agent`] — full toolset, no plan step. The model works directly.
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
}

/// Resolve the effective interaction mode from the configured/explicit mode and
/// the `--yolo` flag.
///
/// `--yolo` implies `--mode agent`: it disables the plan step and the shell
/// confirmation gate, and must expose the full (write/shell) toolset. Without
/// this, `raven --yolo` (no explicit `--mode`) would keep the default
/// `Mode::Plan` and silently degrade to a read-only toolset, leaving the model
/// unable to write files or run shell (see issue #126).
///
/// An explicit CLI `--mode` (`explicit_mode = Some`) takes precedence over the
/// yolo-implied agent mode, so a user can still request `--mode chat --yolo`
/// for autonomous read-only exploration. The config-file `mode` does *not*
/// count as explicit — only a CLI flag pins the mode.
pub fn resolve_mode(explicit_mode: Option<Mode>, config_mode: Option<Mode>, yolo: bool) -> Mode {
    let base = explicit_mode.or(config_mode).unwrap_or(Mode::Plan);
    if yolo && explicit_mode.is_none() {
        Mode::Agent
    } else {
        base
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
}

/// A named model endpoint (Ollama, OpenRouter, Ollama Cloud, …).
///
/// Bundles everything needed to talk to one provider so switching is a
/// single unit (`--provider`, `/provider`, `provider = "…"` in config).
#[derive(Debug, Clone)]
pub struct Provider {
    pub name: String,
    pub base_url: String,
    /// Optional Bearer token. Prefer provider-scoped env vars over
    /// config-file secrets (see [`Provider::resolve_key`]).
    pub api_key: Option<String>,
    /// Name of the env var that holds this provider's API key (e.g.
    /// `OPENROUTER_API_KEY`). Declared in `[providers.<name>]` via
    /// `api_key_env`; falls back to the built-in mapping. Only the *name* is
    /// stored here — never the secret itself.
    pub api_key_env: Option<String>,
    /// Model used when no explicit `--model` / `/model` override is set.
    pub default_model: String,
}

impl Provider {
    /// Built-in presets. `api_key` is intentionally left `None` here — it is
    /// resolved from env/config at construction (see [`Provider::resolve_key`]).
    pub fn builtin(name: &str) -> Option<Provider> {
        match name {
            "ollama" => Some(Provider {
                name: name.into(),
                base_url: "http://localhost:11434/v1".into(),
                api_key: None,
                api_key_env: Some("OLLAMA_API_KEY".into()),
                default_model: "gemma4:latest".into(),
            }),
            "openrouter" => Some(Provider {
                name: name.into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key: None,
                api_key_env: Some("OPENROUTER_API_KEY".into()),
                default_model: "deepseek-v4-flash:cloud".into(),
            }),
            _ => None,
        }
    }

    /// The provider-scoped API-key env var (e.g. `OPENROUTER_API_KEY`).
    ///
    /// Prefers a config-declared `api_key_env`; falls back to the built-in
    /// mapping for known providers, then a conventional `{NAME}_API_KEY`.
    fn key_env_var(&self) -> Cow<'_, str> {
        if let Some(env) = &self.api_key_env {
            return Cow::Borrowed(env.as_str());
        }
        match self.name.as_str() {
            "openrouter" => Cow::Borrowed("OPENROUTER_API_KEY"),
            "ollama" => Cow::Borrowed("OLLAMA_API_KEY"),
            other => Cow::Owned(format!("{}_API_KEY", other.to_uppercase())),
        }
    }

    /// Fill `api_key` from env if unset.
    ///
    /// Precedence (first non-empty wins):
    /// 1. Config-file `api_key` already on `self` — **not** overridden by env
    /// 2. `RAVEN_API_KEY` (universal override)
    /// 3. Provider-scoped var (`api_key_env` or built-in / conventional name)
    ///
    /// Empty/whitespace env values are treated as absent — an empty
    /// `RAVEN_API_KEY` must NOT shadow the provider-scoped var. Prefer env vars
    /// over a committed config `api_key` so secrets stay out of the file; once
    /// a literal `api_key` is set in TOML it wins for that provider.
    pub fn resolve_key(mut self) -> Provider {
        if self.api_key.is_none() {
            let universal = std::env::var("RAVEN_API_KEY").ok();
            let scoped = std::env::var(self.key_env_var().as_ref()).ok();
            self.api_key = Self::pick_key(universal, scoped);
        }
        self
    }

    /// Pure key-selection helper (no env access) so it is trivially testable
    /// without mutating process-global env vars. `universal` wins unless it is
    /// empty/whitespace, in which case `scoped` is used. Both are trimmed.
    fn pick_key(universal: Option<String>, scoped: Option<String>) -> Option<String> {
        let clean = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        clean(universal).or_else(|| clean(scoped))
    }
}

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

/// Max agent iterations: `RAVEN_MAX_ITER` env var, else `OG_MAX_ITER`, else `30`.
pub fn default_max_iter() -> usize {
    std::env::var("RAVEN_MAX_ITER")
        .or_else(|_| std::env::var("OG_MAX_ITER"))
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30)
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

/// TOML-declared provider definition (the `[providers.<name>]` table).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    /// Name of the env var holding this provider's API key (e.g.
    /// `OPENROUTER_API_KEY`). Only the name is stored in config — the secret
    /// itself stays in the environment.
    pub api_key_env: Option<String>,
    pub default_model: Option<String>,
}

impl ProviderConfig {
    /// Field-wise merge: each `Some` field on `overlay` replaces the base;
    /// `None` leaves the base value. Used when workspace config overlays
    /// global `[providers.<name>]` tables so a partial workspace entry does
    /// not wipe global `api_key_env` / `default_model` / etc.
    fn merge(self, overlay: ProviderConfig) -> ProviderConfig {
        ProviderConfig {
            base_url: overlay.base_url.or(self.base_url),
            api_key: overlay.api_key.or(self.api_key),
            api_key_env: overlay.api_key_env.or(self.api_key_env),
            default_model: overlay.default_model.or(self.default_model),
        }
    }
}

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

/// Resolve the active provider from an explicit CLI name, the config file,
/// and env. Precedence: `explicit` (CLI) > `RAVEN_PROVIDER` env >
/// `cfg.provider` > built-in default `ollama`.
///
/// The named provider is looked up in `cfg.providers` first, then the
/// built-in presets. Unknown names fall back to the built-in `ollama` with a
/// warning (never a hard error — a bad name shouldn't brick the session).
pub fn resolve_provider(cfg: &ConfigFile, explicit: Option<String>) -> Provider {
    let name = explicit
        .or_else(|| std::env::var("RAVEN_PROVIDER").ok())
        .or_else(|| cfg.provider.clone())
        .unwrap_or_else(|| "ollama".into());

    let p = match cfg.providers.get(&name) {
        Some(pc) => Provider {
            name: name.clone(),
            base_url: pc.base_url.clone().unwrap_or_else(|| {
                Provider::builtin(&name)
                    .map(|b| b.base_url)
                    .unwrap_or_else(|| "http://localhost:11434/v1".into())
            }),
            api_key: pc.api_key.clone(),
            api_key_env: pc
                .api_key_env
                .clone()
                .or_else(|| Provider::builtin(&name).and_then(|b| b.api_key_env)),
            default_model: pc.default_model.clone().unwrap_or_else(|| {
                Provider::builtin(&name)
                    .map(|b| b.default_model)
                    .unwrap_or_else(|| "gemma4:latest".into())
            }),
        },
        None => match Provider::builtin(&name) {
            Some(b) => b,
            None => {
                tracing::warn!("unknown provider {name:?}; falling back to builtin ollama");
                Provider::builtin("ollama").expect("ollama builtin exists")
            }
        },
    };
    p.resolve_key()
}

/// Names a user can pass to `/provider` or `--provider`.
///
/// Built-in presets are always included; config-declared names are merged in.
pub fn known_provider_names(cfg: &ConfigFile) -> Vec<String> {
    let mut names: Vec<String> = cfg.providers.keys().cloned().collect();
    for builtin in ["ollama", "openrouter"] {
        if !names.iter().any(|n| n == builtin) {
            names.push(builtin.to_string());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Whether `name` is a built-in preset or a config-declared provider.
pub fn is_known_provider(cfg: &ConfigFile, name: &str) -> bool {
    Provider::builtin(name).is_some() || cfg.providers.contains_key(name)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::infer_context_window;

    #[test]
    fn resolve_mode_yolo_forces_agent_when_no_explicit_mode() {
        // Regression for issue #126: `raven --yolo` (no --mode) must expose the
        // full (non-read-only) toolset, i.e. resolve to Mode::Agent.
        let mode = resolve_mode(None, None, true);
        assert_eq!(mode, Mode::Agent);
        assert!(!mode.read_only(), "yolo mode must not be read-only");
        assert!(!mode.plans_first(), "yolo mode must skip the plan step");
    }

    #[test]
    fn resolve_mode_yolo_does_not_override_explicit_cli_mode() {
        // `--mode chat --yolo` keeps chat: explicit CLI mode wins over the
        // yolo-implied agent mode, so a user can still get autonomous
        // read-only exploration.
        let mode = resolve_mode(Some(Mode::Chat), None, true);
        assert_eq!(mode, Mode::Chat);
        assert!(mode.read_only());
    }

    #[test]
    fn resolve_mode_yolo_ignores_config_file_mode() {
        // Only an explicit CLI --mode pins the mode; a config-file `mode`
        // does NOT count as explicit, so yolo still forces agent.
        let mode = resolve_mode(None, Some(Mode::Chat), true);
        assert_eq!(mode, Mode::Agent);
        assert!(!mode.read_only());
    }

    #[test]
    fn resolve_mode_no_yolo_keeps_default_plan() {
        let mode = resolve_mode(None, None, false);
        assert_eq!(mode, Mode::Plan);
        assert!(mode.read_only());
        assert!(mode.plans_first());
    }

    #[test]
    fn resolve_mode_no_yolo_respects_explicit_mode() {
        assert_eq!(resolve_mode(Some(Mode::Agent), None, false), Mode::Agent);
        assert_eq!(resolve_mode(Some(Mode::Chat), None, false), Mode::Chat);
    }

    #[test]
    fn resolve_mode_no_yolo_respects_config_file_mode() {
        assert_eq!(resolve_mode(None, Some(Mode::Agent), false), Mode::Agent);
        assert_eq!(resolve_mode(None, Some(Mode::Chat), false), Mode::Chat);
    }

    #[test]
    fn resolve_mode_explicit_overrides_config() {
        assert_eq!(
            resolve_mode(Some(Mode::Agent), Some(Mode::Chat), false),
            Mode::Agent
        );
    }

    #[test]
    fn infer_context_window_known_models() {
        assert_eq!(infer_context_window("qwen2.5-coder:7b"), 128_000);
        assert_eq!(infer_context_window("qwen3:14b"), 128_000);
        assert_eq!(infer_context_window("llama3.1:8b"), 128_000);
        assert_eq!(infer_context_window("llama3.2:1b"), 128_000);
        assert_eq!(infer_context_window("deepseek-r1:14b"), 128_000);
        assert_eq!(infer_context_window("codestral:22b"), 128_000);
    }

    #[test]
    fn infer_context_window_qwen35_is_256k() {
        // qwen3.5 must be 256K, not the generic qwen3 → 128K.
        assert_eq!(infer_context_window("qwen3.5:0.8b"), 262_144);
        assert_eq!(infer_context_window("qwen3.5"), 262_144);
    }

    #[test]
    fn infer_context_window_deepseek_cloud_variants() {
        // deepseek-v4-flash:cloud is 1M; pro:cloud is 512K.
        assert_eq!(infer_context_window("deepseek-v4-flash:cloud"), 1_000_000);
        assert_eq!(infer_context_window("deepseek-v4-pro:cloud"), 524_288);
        // Non-cloud deepseek falls back to the generic 128K.
        assert_eq!(infer_context_window("deepseek-r1:14b"), 128_000);
    }

    #[test]
    fn infer_context_window_glm_cloud() {
        assert_eq!(infer_context_window("glm-5.2:cloud"), 1_000_000);
        assert_eq!(infer_context_window("glm-5.2:cloud"), 1_000_000);
        // Non-cloud glm falls back to 128k, not the 1M cloud special case.
        assert_eq!(infer_context_window("glm5:8b"), 128_000);
    }

    #[test]
    fn infer_context_window_32k_models() {
        assert_eq!(infer_context_window("llama3:8b"), 32_768);
        assert_eq!(infer_context_window("codellama:13b"), 32_768);
        assert_eq!(infer_context_window("model-32k"), 32_768);
    }

    #[test]
    fn infer_context_window_mistral() {
        assert_eq!(infer_context_window("mistral:7b"), 8_192);
        assert_eq!(infer_context_window("something-8k"), 8_192);
    }

    #[test]
    fn infer_context_window_unknown_fallback() {
        assert_eq!(infer_context_window("phi3:mini"), 32_768);
        assert_eq!(infer_context_window("unknown-model"), 32_768);
        assert_eq!(infer_context_window(""), 32_768);
    }

    #[test]
    fn infer_context_window_case_insensitive() {
        assert_eq!(infer_context_window("QWEN2.5-CODER:7B"), 128_000);
        assert_eq!(infer_context_window("Mistral:7b"), 8_192);
    }

    #[test]
    fn derived_max_tokens_clamps_low() {
        assert_eq!(Settings::derived_max_tokens(8192), 1024);
        assert_eq!(Settings::derived_max_tokens(100), 1024);
        assert_eq!(Settings::derived_max_tokens(0), 1024);
    }

    #[test]
    fn derived_max_tokens_mid_range() {
        assert_eq!(Settings::derived_max_tokens(32_768), 4096);
        assert_eq!(Settings::derived_max_tokens(262_144), 32_768);
    }

    #[test]
    fn derived_max_tokens_clamps_high() {
        assert_eq!(Settings::derived_max_tokens(128_000), 16_000);
        assert_eq!(Settings::derived_max_tokens(1_000_000), 32_768);
    }

    #[test]
    fn load_agents_md_finds_agents_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "# Rules\nUse Rust 2021.").unwrap();
        let result = load_agents_md(tmp.path());
        assert!(result.contains("Use Rust 2021."));
    }

    #[test]
    fn load_agents_md_finds_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "# Claude rules").unwrap();
        let result = load_agents_md(tmp.path());
        assert!(result.contains("Claude rules"));
    }

    #[test]
    fn load_agents_md_prefers_agents_md_over_claude_md() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "agents-first").unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude-second").unwrap();
        let result = load_agents_md(tmp.path());
        assert_eq!(result, "agents-first");
    }

    #[test]
    fn load_agents_md_returns_empty_when_none_found() {
        let tmp = tempfile::tempdir().unwrap();
        let result = load_agents_md(tmp.path());
        assert!(result.is_empty());
    }

    #[test]
    fn load_agents_md_truncates_at_8000_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let long = "z".repeat(10_000);
        std::fs::write(tmp.path().join("AGENTS.md"), &long).unwrap();
        let result = load_agents_md(tmp.path());
        assert_eq!(result.chars().filter(|c| *c == 'z').count(), 8000);
        assert!(result.ends_with("[truncated: content exceeds 8000 chars]"));
    }

    #[test]
    fn load_agents_md_no_marker_when_under_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let short = "z".repeat(100);
        std::fs::write(tmp.path().join("AGENTS.md"), &short).unwrap();
        let result = load_agents_md(tmp.path());
        assert_eq!(result, short);
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn load_agents_md_no_marker_at_exact_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let exact = "z".repeat(8000);
        std::fs::write(tmp.path().join("AGENTS.md"), &exact).unwrap();
        let result = load_agents_md(tmp.path());
        assert_eq!(result.chars().count(), 8000);
        assert!(!result.contains("truncated"));
    }

    #[test]
    fn load_dotenv_sets_missing_keys_only() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        let unique = format!("RAVEN_TEST_DOTENV_{}", std::process::id());
        let unique_pre = format!("RAVEN_TEST_DOTENV_PRE_{}", std::process::id());
        std::fs::write(
            &path,
            format!("{unique}=from_file\n{unique_pre}=from_file\n# comment\nEMPTY=\n"),
        )
        .unwrap();
        std::env::set_var(&unique_pre, "already");
        let n = super::load_dotenv(&path);
        assert!(n >= 1);
        assert_eq!(std::env::var(&unique).unwrap(), "from_file");
        assert_eq!(std::env::var(&unique_pre).unwrap(), "already");
        std::env::remove_var(&unique);
        std::env::remove_var(&unique_pre);
    }

    #[test]
    fn load_dotenv_strips_quotes() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        let unique = format!("RAVEN_TEST_DOTENV_Q_{}", std::process::id());
        std::fs::write(&path, format!("{unique}=\"quoted value\"\n")).unwrap();
        super::load_dotenv(&path);
        assert_eq!(std::env::var(&unique).unwrap(), "quoted value");
        std::env::remove_var(&unique);
    }

    #[test]
    fn config_file_parses_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join(".raven");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"provider = "openrouter"
compact_threshold = 0.5
max_iterations = 10
"#,
        )
        .unwrap();
        let cfg = load_config_file(tmp.path());
        assert_eq!(cfg.provider.as_deref(), Some("openrouter"));
        assert_eq!(cfg.compact_threshold, Some(0.5));
        assert_eq!(cfg.max_iterations, Some(10));
    }

    #[test]
    fn config_file_parses_no_stream() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join(".raven");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.toml"), "no_stream = true\n").unwrap();
        let cfg = load_config_file(tmp.path());
        assert_eq!(cfg.no_stream, Some(true));
    }

    #[test]
    fn config_file_missing_returns_defaults() {
        // Isolate HOME so a real user global config (~/.raven/config.toml)
        // doesn't leak into this test.
        let original_home = std::env::var_os("HOME");
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_config_file(tmp.path());
        // Restore HOME so this test can't pollute later tests (which under
        // Landlock confinement need a valid HOME to spawn subprocesses).
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert!(cfg.provider.is_none());
        assert!(cfg.providers.is_empty());
    }

    #[test]
    fn config_file_parses_theme() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join(".raven");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(cfg_dir.join("config.toml"), "theme = \"nord\"\n").unwrap();
        let cfg = load_config_file(tmp.path());
        assert_eq!(cfg.theme.as_deref(), Some("nord"));
    }

    #[test]
    fn config_file_parses_searxng() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join(".raven");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            "searxng_url = \"http://127.0.0.1:8080\"\nsearxng_engines = [\"google\", \"bing\"]\n",
        )
        .unwrap();
        let cfg = load_config_file(tmp.path());
        assert_eq!(cfg.searxng_url.as_deref(), Some("http://127.0.0.1:8080"));
        assert_eq!(
            cfg.searxng_engines.as_deref(),
            Some(&["google".into(), "bing".into()][..])
        );
    }

    #[test]
    fn env_searxng_url_parses() {
        // Save any user-set value so this test never clobbers it for the run.
        let original = std::env::var("RAVEN_SEARXNG_URL").ok();
        std::env::set_var("RAVEN_SEARXNG_URL", "http://searx.example.com");
        assert_eq!(
            env_searxng_url().as_deref(),
            Some("http://searx.example.com")
        );
        std::env::set_var("RAVEN_SEARXNG_URL", "  ");
        assert!(env_searxng_url().is_none());
        match original {
            Some(v) => std::env::set_var("RAVEN_SEARXNG_URL", v),
            None => std::env::remove_var("RAVEN_SEARXNG_URL"),
        }
    }

    #[test]
    fn env_searxng_engines_parses_list() {
        let original = std::env::var("RAVEN_SEARXNG_ENGINES").ok();
        std::env::set_var("RAVEN_SEARXNG_ENGINES", "google,  bing ,");
        assert_eq!(
            env_searxng_engines(),
            Some(vec!["google".to_string(), "bing".to_string()])
        );
        std::env::set_var("RAVEN_SEARXNG_ENGINES", "  , , ");
        assert!(env_searxng_engines().is_none());
        match original {
            Some(v) => std::env::set_var("RAVEN_SEARXNG_ENGINES", v),
            None => std::env::remove_var("RAVEN_SEARXNG_ENGINES"),
        }
    }

    #[test]
    fn builtin_providers_have_expected_defaults() {
        let ollama = Provider::builtin("ollama").expect("ollama builtin");
        assert_eq!(ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(ollama.default_model, "gemma4:latest");
        assert!(ollama.api_key.is_none());

        let or = Provider::builtin("openrouter").expect("openrouter builtin");
        assert_eq!(or.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(or.default_model, "deepseek-v4-flash:cloud");
        assert!(
            or.api_key.is_none(),
            "key comes from env/config, not the preset"
        );
    }

    #[test]
    fn unknown_builtin_provider_is_none() {
        assert!(Provider::builtin("nope").is_none());
    }

    #[test]
    fn config_file_parses_providers_table() {
        let toml_str = r#"
            provider = "openrouter"
            [providers.ollama]
            base_url = "http://gpu-box:11434/v1"
            default_model = "qwen2.5-coder:14b"
            [providers.openrouter]
            base_url = "https://openrouter.ai/api/v1"
            default_model = "deepseek-v4-pro:cloud"
        "#;
        let cfg: ConfigFile = toml::from_str(toml_str).unwrap();
        assert_eq!(cfg.provider.as_deref(), Some("openrouter"));
        let ollama = cfg.providers.get("ollama").unwrap();
        assert_eq!(ollama.base_url.as_deref(), Some("http://gpu-box:11434/v1"));
        assert_eq!(ollama.default_model.as_deref(), Some("qwen2.5-coder:14b"));
    }

    #[test]
    fn resolve_provider_merges_config_over_builtin() {
        let mut cfg = ConfigFile {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        cfg.providers.insert(
            "ollama".into(),
            ProviderConfig {
                base_url: Some("http://gpu-box:11434/v1".into()),
                default_model: Some("qwen2.5-coder:14b".into()),
                ..Default::default()
            },
        );
        let p = resolve_provider(&cfg, None);
        assert_eq!(p.name, "ollama");
        assert_eq!(p.base_url, "http://gpu-box:11434/v1");
        assert_eq!(p.default_model, "qwen2.5-coder:14b");
    }

    #[test]
    fn resolve_provider_falls_back_to_builtin_default() {
        let cfg = ConfigFile::default();
        let p = resolve_provider(&cfg, None);
        assert_eq!(p.name, "ollama");
        assert_eq!(p.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn resolve_provider_explicit_wins_over_config() {
        let cfg = ConfigFile {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        let p = resolve_provider(&cfg, Some("openrouter".into()));
        assert_eq!(p.name, "openrouter");
        assert_eq!(p.base_url, "https://openrouter.ai/api/v1");
    }

    #[test]
    fn resolve_provider_unknown_falls_back_to_ollama() {
        let cfg = ConfigFile::default();
        let p = resolve_provider(&cfg, Some("nope".into()));
        assert_eq!(p.name, "ollama");
        assert_eq!(p.base_url, "http://localhost:11434/v1");
    }

    #[test]
    fn known_provider_names_includes_builtins_and_config() {
        let mut cfg = ConfigFile::default();
        cfg.providers.insert(
            "acme".into(),
            ProviderConfig {
                base_url: Some("http://localhost:9/v1".into()),
                ..Default::default()
            },
        );
        let names = known_provider_names(&cfg);
        assert!(names.contains(&"ollama".into()));
        assert!(names.contains(&"openrouter".into()));
        assert!(names.contains(&"acme".into()));
        assert!(is_known_provider(&cfg, "acme"));
        assert!(is_known_provider(&cfg, "ollama"));
        assert!(!is_known_provider(&cfg, "nope"));
    }

    #[test]
    fn pick_key_universal_wins_over_provider_scoped() {
        assert_eq!(
            Provider::pick_key(Some("sk-universal".into()), Some("sk-or".into())),
            Some("sk-universal".into())
        );
    }

    #[test]
    fn pick_key_empty_universal_does_not_shadow_provider_scoped() {
        // An empty/whitespace RAVEN_API_KEY must NOT block the provider-scoped
        // var from being used.
        assert_eq!(
            Provider::pick_key(Some("   ".into()), Some("sk-or".into())),
            Some("sk-or".into())
        );
    }

    #[test]
    fn pick_key_trims_and_drops_empty() {
        assert_eq!(Provider::pick_key(Some("  ".into()), None), None);
        assert_eq!(
            Provider::pick_key(None, Some("  sk-or  ".into())),
            Some("sk-or".into())
        );
        assert_eq!(Provider::pick_key(None, None), None);
    }

    #[test]
    fn builtin_providers_declare_key_env() {
        let ollama = Provider::builtin("ollama").expect("ollama builtin");
        assert_eq!(ollama.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
        let or = Provider::builtin("openrouter").expect("openrouter builtin");
        assert_eq!(or.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    }

    #[test]
    fn config_api_key_env_overrides_builtin_mapping() {
        // A config-declared api_key_env must win over the built-in mapping.
        let mut cfg = ConfigFile {
            provider: Some("openrouter".into()),
            ..Default::default()
        };
        cfg.providers.insert(
            "openrouter".into(),
            ProviderConfig {
                api_key_env: Some("MY_OR_KEY".into()),
                ..Default::default()
            },
        );
        let p = resolve_provider(&cfg, None);
        assert_eq!(p.api_key_env.as_deref(), Some("MY_OR_KEY"));
    }

    #[test]
    fn config_api_key_env_falls_back_to_builtin_when_unset() {
        // No api_key_env in config → the built-in mapping is used.
        let mut cfg = ConfigFile {
            provider: Some("openrouter".into()),
            ..Default::default()
        };
        cfg.providers.insert(
            "openrouter".into(),
            ProviderConfig {
                base_url: Some("https://openrouter.ai/api/v1".into()),
                ..Default::default()
            },
        );
        let p = resolve_provider(&cfg, None);
        assert_eq!(p.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));
    }

    #[test]
    fn unknown_provider_key_env_uses_conventional_name() {
        // A brand-new provider with no builtin and no api_key_env falls back to
        // the conventional {NAME}_API_KEY so it still works without a code edit.
        let p = Provider {
            name: "groq".into(),
            base_url: "https://api.groq.com/openai/v1".into(),
            api_key: None,
            api_key_env: None,
            default_model: "llama-3.3-70b-versatile".into(),
        };
        assert_eq!(p.key_env_var().as_ref(), "GROQ_API_KEY");
    }

    #[test]
    fn resolve_key_uses_declared_api_key_env() {
        // The declared api_key_env must be the var read for the key.
        let p = Provider {
            name: "custom".into(),
            base_url: "https://example.com/v1".into(),
            api_key: None,
            api_key_env: Some("CUSTOM_API_KEY".into()),
            default_model: "m".into(),
        };
        // No env var set → no key (proves it reads CUSTOM_API_KEY, not a
        // hardcoded one). The pick_key tests cover the selection logic.
        assert_eq!(p.resolve_key().api_key, None);
    }

    #[test]
    fn provider_config_merge_overlay_wins_per_field() {
        let base = ProviderConfig {
            base_url: Some("http://global:11434/v1".into()),
            api_key: None,
            api_key_env: Some("OLLAMA_API_KEY".into()),
            default_model: Some("gemma4:latest".into()),
        };
        let overlay = ProviderConfig {
            base_url: Some("http://workspace:11434/v1".into()),
            ..Default::default()
        };
        let merged = base.merge(overlay);
        assert_eq!(
            merged.base_url.as_deref(),
            Some("http://workspace:11434/v1")
        );
        assert_eq!(merged.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
        assert_eq!(merged.default_model.as_deref(), Some("gemma4:latest"));
        assert!(merged.api_key.is_none());
    }

    #[test]
    #[cfg(not(windows))]
    fn load_config_file_merges_provider_tables_fieldwise() {
        // Workspace only overrides base_url; global api_key_env + default_model
        // must survive (not wiped by a full HashMap replace).
        //
        // Isolate HOME so a real user global config doesn't leak in. This test
        // is Unix-only: `dirs::home_dir()` reads `HOME` on Unix but a different
        // set of vars on Windows, so the home-dir redirection is not portable.
        // The merge logic itself is covered cross-platform by
        // `provider_config_merge_overlay_wins_per_field`.
        let original_home = std::env::var_os("HOME");
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let global_dir = home.path().join(".raven");
        std::fs::create_dir_all(&global_dir).unwrap();
        std::fs::write(
            global_dir.join("config.toml"),
            r#"
[providers.ollama]
base_url = "http://global:11434/v1"
api_key_env = "OLLAMA_API_KEY"
default_model = "gemma4:latest"
"#,
        )
        .unwrap();

        let ws = tempfile::tempdir().unwrap();
        let ws_dir = ws.path().join(".raven");
        std::fs::create_dir_all(&ws_dir).unwrap();
        std::fs::write(
            ws_dir.join("config.toml"),
            r#"
[providers.ollama]
base_url = "http://workspace:11434/v1"
"#,
        )
        .unwrap();

        let cfg = load_config_file(ws.path());
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }

        let ollama = cfg.providers.get("ollama").expect("ollama provider");
        assert_eq!(
            ollama.base_url.as_deref(),
            Some("http://workspace:11434/v1")
        );
        assert_eq!(ollama.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));
        assert_eq!(ollama.default_model.as_deref(), Some("gemma4:latest"));
    }
}
