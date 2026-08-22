//! First-run onboarding wizard: provider + model selection.
//!
//! Runs only on a fresh interactive install (no config, no overrides, TTY).
//! Pure gate + path helpers here; the interactive wizard and writes are
//! added in later tasks.

use std::path::PathBuf;

use super::ConfigFile;

/// (global config path, workspace config path)
pub fn config_paths(workspace: &std::path::Path) -> (PathBuf, PathBuf) {
    let global = dirs::home_dir()
        .map(|h| h.join(".raven").join("config.toml"))
        .unwrap_or_default();
    (global, workspace.join(".raven").join("config.toml"))
}

/// True only when the run is interactive, unconfigured, and un-overridden.
/// Pure so it is unit-testable offline.
pub fn needs_onboarding(
    cfg: &ConfigFile,
    cli_model: Option<String>,
    cli_provider: Option<String>,
    env_provider: Option<String>,
    interactive: bool,
) -> bool {
    interactive
        && cfg.provider.is_none()
        && cfg.providers.is_empty()
        && cli_model.is_none()
        && cli_provider.is_none()
        && env_provider.is_none()
}

/// Parse a 1-based provider menu choice into a 0-based index, or None.
#[allow(dead_code)] // wired into the wizard in Task 4/6
pub fn parse_provider_choice(input: &str, count: usize) -> Option<usize> {
    let n: usize = input.trim().parse().ok()?;
    // `.then` (lazy) not `.then_some` (eager): `n - 1` must not run when n == 0.
    (n >= 1 && n <= count).then(|| n - 1)
}

/// Custom provider entry "name:base_url". The model is prompted separately.
/// Returns None if the name or URL is invalid (empty name, empty URL, or the
/// URL does not start with http:// or https://).
#[allow(dead_code)] // wired into the wizard in Task 4/6
pub fn parse_custom_provider(input: &str) -> Option<(String, String)> {
    let (name, url) = input.trim().split_once(':')?;
    let name = name.trim().to_string();
    let url = url.trim().to_string();
    if name.is_empty()
        || url.is_empty()
        || !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return None;
    }
    Some((name, url))
}

/// Returns true if the endpoint answers GET {base}/api/tags (Ollama).
/// Bounded 2s timeout; never panics.
#[allow(dead_code)] // wired into the wizard in Task 4/6
pub async fn ollama_reachable(base_url: &str) -> bool {
    let trimmed = base_url.trim_end_matches('/');
    let endpoint = format!("{}/api/tags", trimmed.trim_end_matches("/v1"));
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    else {
        return false;
    };
    client
        .get(&endpoint)
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

/// Curated fallback model list for a provider, used when the live endpoint
/// probe returns nothing. Guarantees the wizard always has a starting point.
#[allow(dead_code)] // wired into the wizard in Task 4/6
pub fn fallback_models(provider_name: &str) -> Vec<String> {
    match provider_name {
        "ollama" => vec![
            "qwen3.8:latest".into(),
            "qwen3:14b".into(),
            "deepseek-v4-flash:cloud".into(),
            "deepseek-v4-pro:cloud".into(),
            "glm-5.2:cloud".into(),
        ],
        "openrouter" => vec!["x-ai/grok-4.5".into(), "x-ai/grok-4.6".into()],
        // Custom OpenAI-compatible endpoints: suggest a sensible OpenAI-model
        // default so the user always has a starting point, then let them type
        // the exact model id their endpoint exposes.
        _ => vec!["gpt-4o".into(), "gpt-4o-mini".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_install_with_tty_needs_onboarding() {
        let cfg = ConfigFile::default();
        assert!(needs_onboarding(&cfg, None, None, None, true));
    }

    #[test]
    fn tty_with_any_config_provider_skips() {
        let cfg = ConfigFile {
            provider: Some("ollama".into()),
            ..Default::default()
        };
        assert!(!needs_onboarding(&cfg, None, None, None, true));
    }

    #[test]
    fn tty_with_config_providers_table_skips() {
        let mut cfg = ConfigFile::default();
        cfg.providers.insert(
            "acme".into(),
            super::super::provider::ProviderConfig {
                base_url: Some("http://x/v1".into()),
                ..Default::default()
            },
        );
        assert!(!needs_onboarding(&cfg, None, None, None, true));
    }

    #[test]
    fn explicit_model_skips() {
        assert!(!needs_onboarding(
            &ConfigFile::default(),
            Some("qwen3.8".into()),
            None,
            None,
            true
        ));
    }

    #[test]
    fn explicit_provider_skips() {
        assert!(!needs_onboarding(
            &ConfigFile::default(),
            None,
            Some("openrouter".into()),
            None,
            true
        ));
    }

    #[test]
    fn env_provider_skips() {
        assert!(!needs_onboarding(
            &ConfigFile::default(),
            None,
            None,
            Some("openrouter".into()),
            true
        ));
    }

    #[test]
    fn non_tty_never_onboards() {
        assert!(!needs_onboarding(
            &ConfigFile::default(),
            None,
            None,
            None,
            false
        ));
    }

    #[test]
    fn config_paths_global_and_ws() {
        let p = config_paths(std::path::Path::new("/tmp/ws"));
        assert!(p.0.ends_with(".raven/config.toml"));
        assert!(p.1.ends_with("/tmp/ws/.raven/config.toml"));
    }
}

#[cfg(test)]
mod tests2 {
    use super::*;

    #[test]
    fn parse_provider_choice_known() {
        assert_eq!(parse_provider_choice("1", 3), Some(0));
        assert_eq!(parse_provider_choice("2", 3), Some(1));
        assert_eq!(parse_provider_choice("0", 3), None);
        assert_eq!(parse_provider_choice("x", 3), None);
        assert_eq!(parse_provider_choice("99", 3), None);
    }

    #[test]
    fn parse_custom_provider_entry() {
        let (name, url) = parse_custom_provider("acme:http://gpu:8080/v1").unwrap();
        assert_eq!(name, "acme");
        assert_eq!(url, "http://gpu:8080/v1");
        assert!(parse_custom_provider(":http://gpu:8080/v1").is_none());
        assert!(parse_custom_provider("acme").is_none());
        assert!(parse_custom_provider("acme:localhost:8080").is_none());
        assert!(parse_custom_provider("!!!").is_none());
    }
}

#[cfg(test)]
mod tests3 {
    use super::*;

    #[test]
    fn fallback_models_ollama() {
        let m = fallback_models("ollama");
        assert!(m.contains(&"qwen3.8:latest".to_string()));
        assert!(m.contains(&"deepseek-v4-pro:cloud".to_string()));
    }

    #[test]
    fn fallback_models_openrouter() {
        let m = fallback_models("openrouter");
        assert!(m.contains(&"x-ai/grok-4.5".to_string()));
    }

    #[test]
    fn fallback_models_custom() {
        let m = fallback_models("acme");
        assert!(m.contains(&"gpt-4o".to_string()));
    }
}
