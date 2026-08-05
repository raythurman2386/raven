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

/// Runtime configuration for an [`crate::agent::Agent`].
///
/// Constructed from CLI flags + environment variables in [`crate::main`].
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
    pub plan_first: bool,
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
    /// clamped to `[1024, 8192]`.
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
    /// // 128_000 / 8 = 16000 (clamped down to 8192)
    /// assert_eq!(Settings::derived_max_tokens(8192), 1024);
    /// assert_eq!(Settings::derived_max_tokens(32768), 4096);
    /// assert_eq!(Settings::derived_max_tokens(128_000), 8192);
    /// ```
    pub fn derived_max_tokens(context_window: usize) -> u32 {
        // Output budget: window / 8, clamped to a generous ceiling so long,
        // detailed responses aren't cut short. 32k covers even 1M-token
        // cloud contexts comfortably while staying a sane per-call cap.
        let raw = context_window / 8;
        raw.clamp(1024, 32_768) as u32
    }
}

/// Load optional project instructions from the workspace.
///
/// Checks, in order: `AGENTS.md`, `CLAUDE.md`, `.grok/AGENTS.md`, `AGENT.md`.
/// Returns the contents of the first match (truncated to 8000 chars), or an
/// empty string if none are found.
pub fn load_agents_md(workspace: &std::path::Path) -> String {
    const CANDIDATES: &[&str] = &["AGENTS.md", "CLAUDE.md", ".grok/AGENTS.md", "AGENT.md"];
    for name in CANDIDATES {
        let p = workspace.join(name);
        if p.is_file() {
            if let Ok(text) = std::fs::read_to_string(&p) {
                return text.chars().take(8000).collect();
            }
        }
    }
    String::new()
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
    pub plan_first: Option<bool>,
    pub temperature: Option<f32>,
    /// Disable streaming and use a single non-streaming request instead.
    pub no_stream: Option<bool>,
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
        plan_first: ws.plan_first.or(global.plan_first),
        temperature: ws.temperature.or(global.temperature),
        no_stream: ws.no_stream.or(global.no_stream),
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
    fn infer_context_window_known_models() {
        assert_eq!(infer_context_window("qwen2.5-coder:7b"), 128_000);
        assert_eq!(infer_context_window("qwen3:14b"), 128_000);
        assert_eq!(infer_context_window("llama3.1:8b"), 128_000);
        assert_eq!(infer_context_window("llama3.2:1b"), 128_000);
        assert_eq!(infer_context_window("deepseek-r1:14b"), 128_000);
        assert_eq!(infer_context_window("codestral:22b"), 128_000);
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
        let long = "x".repeat(10_000);
        std::fs::write(tmp.path().join("AGENTS.md"), &long).unwrap();
        let result = load_agents_md(tmp.path());
        assert_eq!(result.chars().count(), 8000);
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
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", home.path());
        let tmp = tempfile::tempdir().unwrap();
        let cfg = load_config_file(tmp.path());
        assert!(cfg.model.is_none());
        assert!(cfg.host.is_none());
    }
}
