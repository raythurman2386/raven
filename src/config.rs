//! Configuration: `Settings`, environment defaults, context-window inference,
//! `AGENTS.md` / `CLAUDE.md` loading, and config file (`config.toml`).
//!
//! Precedence: CLI flag > env var > workspace config.toml > global config.toml > built-in default.
//!
//! # Environment variable precedence
//!
//! `RAVEN_*` vars take priority over legacy `OLLAMA_*` / `OG_*` fallbacks:
//! - `RAVEN_MODEL` > `OLLAMA_MODEL`
//! - `RAVEN_HOST` > `OLLAMA_HOST`
//! - `RAVEN_API_KEY` > `OLLAMA_API_KEY`
//! - `RAVEN_MAX_ITER` > `OG_MAX_ITER`
//! - `RAVEN_CONTEXT_WINDOW` > `OG_CONTEXT_WINDOW`
//! - `RAVEN_COMPACT_THRESHOLD` > `OG_COMPACT_THRESHOLD`

use anyhow::Result;
use serde::Deserialize;
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
    pub base_url: String,
    /// Optional API key for Ollama Cloud (or any OpenAI-compatible host that requires auth).
    /// Prefer setting via OLLAMA_API_KEY env var rather than CLI flags that end up in shell history.
    pub api_key: Option<String>,
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
}

impl Settings {
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

/// Default model name: `RAVEN_MODEL` env var, else `OLLAMA_MODEL`, else `gemma4:latest`.
pub fn default_model() -> String {
    std::env::var("RAVEN_MODEL")
        .or_else(|_| std::env::var("OLLAMA_MODEL"))
        .unwrap_or_else(|_| "gemma4:latest".into())
}

/// Default OpenAI-compatible base URL: `RAVEN_HOST` env var, else `OLLAMA_HOST`, else `http://localhost:11434/v1`.
pub fn default_base_url() -> String {
    std::env::var("RAVEN_HOST")
        .or_else(|_| std::env::var("OLLAMA_HOST"))
        .unwrap_or_else(|_| "http://localhost:11434/v1".into())
}

/// Read the API key from `RAVEN_API_KEY` or `OLLAMA_API_KEY`. Empty/whitespace strings are treated as absent.
pub fn default_api_key() -> Option<String> {
    std::env::var("RAVEN_API_KEY")
        .or_else(|_| std::env::var("OLLAMA_API_KEY"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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

// ── Config file ────────────────────────────────────────────────────────

/// Config file loaded from `~/.raven/config.toml` or `.raven/config.toml`.
///
/// All fields optional — only overrides built-in defaults when present.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ConfigFile {
    pub model: Option<String>,
    pub host: Option<String>,
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

    // Merge: workspace overrides global
    ConfigFile {
        model: ws.model.or(global.model),
        host: ws.host.or(global.host),
        context_window: ws.context_window.or(global.context_window),
        compact_threshold: ws.compact_threshold.or(global.compact_threshold),
        max_iterations: ws.max_iterations.or(global.max_iterations),
        mode: ws.mode.or(global.mode),
        temperature: ws.temperature.or(global.temperature),
        no_stream: ws.no_stream.or(global.no_stream),
        verify: ws.verify.or(global.verify),
        theme: ws.theme.or(global.theme),
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
    fn default_api_key_empty_is_none() {
        let key: Option<String> = Some("".to_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        assert!(key.is_none());
    }

    #[test]
    fn default_api_key_whitespace_is_none() {
        let key: Option<String> = Some("   \t ".to_string())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        assert!(key.is_none());
    }

    #[test]
    fn config_file_parses_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join(".raven");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.toml"),
            r#"model = "test-model"
compact_threshold = 0.5
max_iterations = 10
"#,
        )
        .unwrap();
        let cfg = load_config_file(tmp.path());
        assert_eq!(cfg.model.as_deref(), Some("test-model"));
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
        assert!(cfg.model.is_none());
        assert!(cfg.host.is_none());
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
}
