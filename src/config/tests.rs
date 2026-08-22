//! Unit tests for `Settings`, mode resolution, env defaults, `AGENTS.md`
//! loading, and config-file parsing. Provider-resolution tests live in
//! `provider.rs` where they can reach the private helpers.

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
    // deepseek-v4-flash:cloud and pro:cloud are both 1M.
    assert_eq!(infer_context_window("deepseek-v4-flash:cloud"), 1_000_000);
    assert_eq!(infer_context_window("deepseek-v4-pro:cloud"), 1_000_000);
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
fn round_temperature_removes_f32_f64_artifacts() {
    // Regression for the OpenRouter 400 rejection: an f32 like 0.2 widens to
    // 0.20000000298023224 as an f64, which some providers reject. The rounded
    // value must serialize to a clean, provider-friendly JSON number.
    assert_eq!(
        serde_json::to_string(&round_temperature(0.2)).unwrap(),
        "0.2"
    );
    assert_eq!(
        serde_json::to_string(&round_temperature(0.7)).unwrap(),
        "0.7"
    );
    assert_eq!(
        serde_json::to_string(&round_temperature(0.33)).unwrap(),
        "0.33"
    );
    assert_eq!(
        serde_json::to_string(&round_temperature(0.05)).unwrap(),
        "0.05"
    );
    assert_eq!(
        serde_json::to_string(&round_temperature(0.4)).unwrap(),
        "0.4"
    );
    assert_eq!(
        serde_json::to_string(&round_temperature(1.0)).unwrap(),
        "1.0"
    );
    // Values beyond 4 decimals are rounded, not truncated.
    assert_eq!(
        serde_json::to_string(&round_temperature(0.12346)).unwrap(),
        "0.1235"
    );
}

#[test]
fn round_temperature_normalizes_negative_zero() {
    assert_eq!(round_temperature(0.0), 0.0);
    assert!(round_temperature(-0.0f32) == 0.0);
    assert_eq!(
        serde_json::to_string(&round_temperature(-0.0f32)).unwrap(),
        "0.0"
    );
}

#[test]
fn load_global_dotenv_reads_home_env() {
    // Isolate HOME so a real user ~/.raven/.env doesn't leak in.
    let original_home = std::env::var_os("HOME");
    let home = tempfile::tempdir().unwrap();
    std::env::set_var("HOME", home.path());
    let raven_dir = home.path().join(".raven");
    std::fs::create_dir_all(&raven_dir).unwrap();
    // Use a non-secret-shaped sentinel: a sk-* value would be rewritten by
    // the agent/harness as «redacted:…», so a plain token proves the loader
    // round-trips the actual value without ambiguity.
    std::fs::write(
        raven_dir.join(".env"),
        "OLLAMA_API_KEY=load-global-test-1\n",
    )
    .unwrap();

    let n = crate::config::load_global_dotenv();
    assert_eq!(n, 1);
    assert_eq!(
        std::env::var("OLLAMA_API_KEY").unwrap(),
        "load-global-test-1"
    );

    std::env::remove_var("OLLAMA_API_KEY");
    match original_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}
