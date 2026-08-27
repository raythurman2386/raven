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

/// Custom provider entry "name:base_url". The model is prompted separately.
/// Returns None if the name or URL is invalid (empty name, empty URL, or the
/// URL does not start with http:// or https://).
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

/// Curated fallback model list for a provider, used when the live endpoint
/// probe returns nothing. Guarantees the wizard always has a starting point.
pub fn fallback_models(provider_name: &str) -> Vec<String> {
    match provider_name {
        "ollama" => vec![
            "qwen3.8:latest".into(),
            "qwen3:14b".into(),
            "deepseek-v4-flash:cloud".into(),
            "deepseek-v4-pro:cloud".into(),
            "glm-5.3-flash:cloud".into(),
        ],
        "openrouter" => vec!["x-ai/grok-4.5".into(), "x-ai/grok-4.6".into()],
        // OpenCode Go models, from the OpenAI-style /zen/go/v1/models list.
        "opencode-go" => vec![
            "deepseek-v4-flash".into(),
            "deepseek-v4-pro".into(),
            "glm-5.3-flash".into(),
            "kimi-k3".into(),
            "mimo-v2.5".into(),
            "qwen3.8-max".into(),
            "minimax-m3".into(),
        ],
        // Custom OpenAI-compatible endpoints: suggest a sensible OpenAI-model
        // default so the user always has a starting point, then let them type
        // the exact model id their endpoint exposes.
        _ => vec!["gpt-4o".into(), "gpt-4o-mini".into()],
    }
}

/// Serialize-only shape of the config we write. NO api_key field — the key
/// lives in a separate ~/.raven/.env so config.toml stays secret-free.
#[derive(serde::Serialize)]
struct ProvidersTable {
    provider: String,
    providers: std::collections::BTreeMap<String, ProviderEntry>,
}

#[derive(serde::Serialize)]
struct ProviderEntry {
    base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
}

/// Build a secret-free config.toml for the chosen provider/model/base_url.
pub fn build_config_toml(name: &str, base_url: &str, model: &str) -> String {
    let entry = ProviderEntry {
        base_url: base_url.to_string(),
        default_model: Some(model.to_string()),
    };
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(name.to_string(), entry);
    toml::to_string_pretty(&ProvidersTable {
        provider: name.to_string(),
        providers,
    })
    .expect("providers table is serializable")
}

/// Build the ~/.raven/.env line(s) for an API key, or empty string if none.
/// Uses a provider-scoped var when known, else the universal RAVEN_API_KEY.
pub fn build_env_file(api_key: Option<String>, provider_name: &str) -> String {
    let Some(key) = api_key else {
        return String::new();
    };
    let var = match provider_name {
        "openrouter" => "OPENROUTER_API_KEY",
        "ollama" => "OLLAMA_API_KEY",
        "opencode-go" => "OPENCODE_GO_API_KEY",
        _ => "RAVEN_API_KEY",
    };
    format!("{var}={key}\n")
}

/// Write the config.toml into `dir` (creating it), Unix mode 0600.
pub fn write_global_config(dir: &std::path::Path, contents: &str) -> std::io::Result<()> {
    write_private(dir, "config.toml", contents)
}

/// Write the key .env into `dir` (creating), Unix 0600.
pub fn write_global_env(dir: &std::path::Path, contents: &str) -> std::io::Result<()> {
    write_private(dir, ".env", contents)
}

/// Write `file` into `dir`, creating it with restrictive permissions so the
/// file is never exposed at default (umask) perms. On Unix the 0600 mode is
/// set at open/creation time, not via a post-write chmod — that avoids the
/// TOCTOU window where a freshly-written secrets file would be world-readable
/// before its permissions are tightened. On Windows we rely on the user
/// profile's default ACLs (mirroring the existing convention).
fn write_private(dir: &std::path::Path, file: &str, contents: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(file);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        // Restrict the config dir itself to the owner so other local users
        // can't stat/list ~/.raven even though the file contents are 0600.
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        f.write_all(contents.as_bytes())?;
        f.flush()?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, contents)?;
    }
    Ok(())
}

/// Run the interactive first-run wizard: pick a provider, model, and optional
/// API key; persist a secret-free `~/.raven/config.toml` and the key (if any)
/// to `~/.raven/.env`; return the resulting [`ConfigFile`].
pub async fn run_onboarding() -> anyhow::Result<ConfigFile> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let global_dir = home.join(".raven");

    println!("Welcome to Raven — first-run setup\n");

    // 1. Provider menu: known providers + a trailing "custom" entry.
    let mut known = crate::config::known_provider_names(&super::ConfigFile::default());
    known.sort();
    let mut options: Vec<String> = known.clone();
    options.push("custom (any OpenAI-compatible base URL)".to_string());
    println!("Pick a provider:");
    for (i, o) in options.iter().enumerate() {
        println!("  {}. {o}", i + 1);
    }

    let provider_choice = read_choice(options.len(), false)?;
    let (provider_name, base_url) = if let Some(idx) = provider_choice {
        if idx < known.len() {
            let name = known[idx].clone();
            let base = crate::config::Provider::builtin(&name)
                .map(|p| p.base_url)
                .unwrap_or_else(|| "http://localhost:11434/v1".to_string());
            (name, base)
        } else {
            // "custom" entry selected (last option).
            println!("\nEnter custom endpoint as `name:base_url` (e.g. acme:http://gpu:8080/v1):");
            let entry = read_line()?;
            match parse_custom_provider(&entry) {
                Some((name, url)) => (name, url),
                None => anyhow::bail!(
                    "invalid custom endpoint: expected `name:http(s)://host[:port]/v1`"
                ),
            }
        }
    } else {
        anyhow::bail!("no provider selected");
    };

    // 2. Optional API key (blank for none; local Ollama needs none).
    println!("API key for {provider_name} (blank for none):");
    let api_key = read_line_trimmed().filter(|s| !s.is_empty());

    // 3. Model selection: live list when reachable, else curated fallback.
    let mut provider = crate::config::Provider {
        name: provider_name.clone(),
        base_url: base_url.clone(),
        api_key: api_key.clone(),
        api_key_env: crate::config::Provider::builtin(&provider_name).and_then(|p| p.api_key_env),
        default_model: String::new(),
    };
    let live = crate::tui::fetch_live_provider_models(&provider);
    let candidates = if live.is_empty() {
        fallback_models(&provider_name)
    } else {
        live
    };

    let model = if candidates.is_empty() {
        println!("No models discovered. Type a model id:");
        read_line_trimmed().ok_or_else(|| anyhow::anyhow!("no model entered"))?
    } else {
        println!("Model:");
        for (i, m) in candidates.iter().enumerate() {
            println!("  {}. {m}", i + 1);
        }
        println!("  0. type a custom model id");
        match read_choice(candidates.len(), true)? {
            Some(idx) => candidates[idx].clone(),
            None => {
                println!("Model id:");
                let m = read_line_trimmed().unwrap_or_default();
                if m.is_empty() {
                    anyhow::bail!("no model entered");
                }
                m
            }
        }
    };
    provider.default_model = model.clone();

    // 4. Persist secret-free config + optional key env.
    let config_toml = build_config_toml(&provider_name, &base_url, &model);
    write_global_config(&global_dir, &config_toml)?;
    let env = build_env_file(api_key, &provider_name);
    if !env.is_empty() {
        write_global_env(&global_dir, &env)?;
    }

    // 5. Return the equivalent in-memory ConfigFile.
    let mut cfg = ConfigFile {
        provider: Some(provider_name.clone()),
        ..Default::default()
    };
    cfg.providers.insert(
        provider_name.clone(),
        super::ProviderConfig {
            base_url: Some(base_url),
            default_model: Some(model),
            ..Default::default()
        },
    );
    println!(
        "\nSaved to {}. Next run won't re-prompt.",
        global_dir.display()
    );
    Ok(cfg)
}

/// Read one trimmed line from stdin, erroring if stdin is not interactive.
fn read_line() -> anyhow::Result<String> {
    crate::runner::read_line_if_tty()
        .map(|l| l.trim().to_string())
        .ok_or_else(|| anyhow::anyhow!("stdin is not interactive"))
}

/// Read one trimmed line, or None if stdin is not interactive.
fn read_line_trimmed() -> Option<String> {
    crate::runner::read_line_if_tty().map(|l| l.trim().to_string())
}

/// Read a 1-based menu choice into a 0-based index. When `allow_zero` is true,
/// a literal `0` returns None (caller treats it as "custom/manual entry").
/// Invalid or out-of-range input is an error.
fn read_choice(count: usize, allow_zero: bool) -> anyhow::Result<Option<usize>> {
    let line = read_line()?;
    if allow_zero && line == "0" {
        return Ok(None);
    }
    let n: usize = line
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid choice: expected a number 1..={count}"))?;
    if n == 0 || n > count {
        anyhow::bail!("invalid choice: expected a number 1..={count}");
    }
    Ok(Some(n - 1))
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
    fn fallback_models_opencode_go() {
        let m = fallback_models("opencode-go");
        assert!(m.contains(&"deepseek-v4-flash".to_string()));
        assert!(m.contains(&"glm-5.3-flash".to_string()));
        assert!(m.contains(&"qwen3.8-max".to_string()));
        assert!(m.contains(&"minimax-m3".to_string()));
    }

    #[test]
    fn fallback_models_custom() {
        let m = fallback_models("acme");
        assert!(m.contains(&"gpt-4o".to_string()));
    }
}

#[cfg(test)]
mod tests4 {
    use super::*;

    #[test]
    fn build_config_toml_secret_free() {
        let toml = build_config_toml("ollama", "http://localhost:11434/v1", "qwen3.8:latest");
        assert!(toml.contains("provider = \"ollama\""));
        assert!(toml.contains("default_model = \"qwen3.8:latest\""));
        assert!(toml.contains("base_url = \"http://localhost:11434/v1\""));
        assert!(!toml.contains("api_key"), "config.toml must be secret-free");
    }

    #[test]
    fn build_config_toml_custom_provider() {
        let toml = build_config_toml("acme", "http://gpu:8080/v1", "gpt-4o");
        assert!(toml.contains("provider = \"acme\""));
        assert!(toml.contains("base_url = \"http://gpu:8080/v1\""));
        assert!(toml.contains("default_model = \"gpt-4o\""));
    }

    #[test]
    fn build_env_file_lines() {
        assert!(build_env_file(Some("sk-or-abc".into()), "openrouter")
            .contains("OPENROUTER_API_KEY=sk-or-abc"));
        assert!(build_env_file(Some("sk-x".into()), "ollama").contains("OLLAMA_API_KEY=sk-x"));
        assert!(build_env_file(Some("sk-ocg".into()), "opencode-go")
            .contains("OPENCODE_GO_API_KEY=sk-ocg"));
        assert!(build_env_file(Some("sk-c".into()), "acme").contains("RAVEN_API_KEY=sk-c"));
        assert!(build_env_file(None, "ollama").is_empty());
    }

    #[test]
    fn write_helpers_persist_and_are_private() {
        let dir = tempfile::tempdir().unwrap();
        write_global_config(dir.path(), "provider = \"ollama\"\n").unwrap();
        write_global_env(dir.path(), "OLLAMA_API_KEY=sk-x\n").unwrap();
        let cfg = std::fs::read_to_string(dir.path().join("config.toml")).unwrap();
        let env = std::fs::read_to_string(dir.path().join(".env")).unwrap();
        assert!(cfg.contains("provider = \"ollama\""));
        assert!(env.contains("OLLAMA_API_KEY=sk-x"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let m = std::fs::metadata(dir.path().join("config.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(m & 0o777, 0o600);
            let me = std::fs::metadata(dir.path().join(".env"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(me & 0o777, 0o600);
        }
    }
}
