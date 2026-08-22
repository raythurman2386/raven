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
