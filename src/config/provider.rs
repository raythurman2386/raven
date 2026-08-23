//! Provider resolution: named providers, API-key resolution, and the built-in
//! presets.
//!
//! The provider model is bundled here so switching providers is a single unit
//! (`--provider`, `/provider`, `provider = "…"` in config.toml). API keys are
//! resolved per provider: config-file `api_key` (if set) → `RAVEN_API_KEY`
//! (universal override) → the provider's declared `api_key_env` (or built-in
//! mapping, e.g. `OPENROUTER_API_KEY` / `OLLAMA_API_KEY`).

use serde::Deserialize;
use std::borrow::Cow;

use super::ConfigFile;

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
                default_model: "qwen3.8:latest".into(),
            }),
            "openrouter" => Some(Provider {
                name: name.into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                api_key: None,
                api_key_env: Some("OPENROUTER_API_KEY".into()),
                default_model: "x-ai/grok-4.5".into(),
            }),
            "opencode-go" => Some(Provider {
                name: name.into(),
                // OpenAI-compatible chat completions root (Raven appends
                // /chat/completions). See https://opencode.ai/zen/go/v1/models
                base_url: "https://opencode.ai/zen/go/v1".into(),
                api_key: None,
                api_key_env: Some("OPENCODE_GO_API_KEY".into()),
                default_model: "deepseek-v4-flash".into(),
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
            // Explicit arm required: the conventional fallback would produce
            // OPENCODE-GO_API_KEY (invalid env var — hyphen in the name).
            "opencode-go" => Cow::Borrowed("OPENCODE_GO_API_KEY"),
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
    pub(crate) fn merge(self, overlay: ProviderConfig) -> ProviderConfig {
        ProviderConfig {
            base_url: overlay.base_url.or(self.base_url),
            api_key: overlay.api_key.or(self.api_key),
            api_key_env: overlay.api_key_env.or(self.api_key_env),
            default_model: overlay.default_model.or(self.default_model),
        }
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
                    .unwrap_or_else(|| "qwen3.8:latest".into())
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
    for builtin in ["ollama", "openrouter", "opencode-go"] {
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
    #[cfg(not(windows))]
    use crate::config::load_config_file;
    use crate::config::ConfigFile;

    #[test]
    fn builtin_providers_have_expected_defaults() {
        let ollama = Provider::builtin("ollama").expect("ollama builtin");
        assert_eq!(ollama.base_url, "http://localhost:11434/v1");
        assert_eq!(ollama.default_model, "qwen3.8:latest");
        assert!(ollama.api_key.is_none());

        let or = Provider::builtin("openrouter").expect("openrouter builtin");
        assert_eq!(or.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(or.default_model, "x-ai/grok-4.5");
        assert!(
            or.api_key.is_none(),
            "key comes from env/config, not the preset"
        );

        let go = Provider::builtin("opencode-go").expect("opencode-go builtin");
        assert_eq!(go.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(go.default_model, "deepseek-v4-flash");
        assert_eq!(go.api_key_env.as_deref(), Some("OPENCODE_GO_API_KEY"));
        assert!(
            go.api_key.is_none(),
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
        assert!(names.contains(&"opencode-go".into()));
        assert!(names.contains(&"acme".into()));
        assert!(is_known_provider(&cfg, "acme"));
        assert!(is_known_provider(&cfg, "ollama"));
        assert!(is_known_provider(&cfg, "opencode-go"));
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
        let go = Provider::builtin("opencode-go").expect("opencode-go builtin");
        assert_eq!(go.api_key_env.as_deref(), Some("OPENCODE_GO_API_KEY"));
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
    fn opencode_go_key_env_not_hyphenated() {
        // The provider name contains a hyphen, so the conventional fallback
        // would yield the invalid env var OPENCODE-GO_API_KEY. The explicit
        // builtin arm must win and produce OPENCODE_GO_API_KEY.
        let go = Provider::builtin("opencode-go").expect("opencode-go builtin");
        assert_eq!(go.key_env_var().as_ref(), "OPENCODE_GO_API_KEY");
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
