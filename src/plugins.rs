//! Agent Plugins v1.0.0 — skills-only client conformance.
//!
//! Implements the portable subset of the Agent Plugins specification that
//! Raven supports: loading `plugin.json` manifests, validating the closed
//! manifest schema and plugin name constraints, and discovering Agent Skills
//! from each plugin's fixed `skills/` location. MCP servers (`mcp.json`) and
//! client extensions are outside Raven's scope and are ignored per §11.3.
//!
//! Plugin roots are `~/.raven/plugins/` (global) and `.raven/plugins/`
//! (workspace). Each immediate child directory containing a `plugin.json` is a
//! candidate plugin.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Canonical `$schema` identifier for Agent Plugins 1.0.0.
pub const PLUGIN_SCHEMA: &str = "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";

/// A loaded plugin: its validated name, filesystem root, and discovered skills.
#[derive(Debug, Clone)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    pub skills: Vec<PathBuf>,
}

/// The plugin roots visible to `workspace`: workspace-local then global.
fn plugin_roots(workspace: &Path) -> Vec<PathBuf> {
    let mut roots = vec![workspace.join(".raven").join("plugins")];
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".raven").join("plugins"));
    }
    roots
}

/// Validate a plugin name against §5.5: 1-64 chars of `[a-z0-9.-]`, alphanumeric
/// ends, and no consecutive `--` or `..`.
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let bytes = name.as_bytes();
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return false;
    }
    !name.contains("--") && !name.contains("..")
}

/// Whether `path` resolves (through symlinks) to a location within `root`.
fn within(root: &Path, path: &Path) -> bool {
    match (root.canonicalize(), path.canonicalize()) {
        (Ok(r), Ok(p)) => p.starts_with(&r),
        _ => false,
    }
}

/// Validate a parsed `plugin.json` object and return the plugin name.
///
/// Applies the spec's failure boundaries: an unsupported `$schema`, a missing
/// or invalid `name`, or any metadata type violation is fatal; unknown
/// top-level fields and a non-object `extensions` field are reported and
/// ignored.
fn validate_manifest(value: &serde_json::Value) -> Result<String, String> {
    let obj = value
        .as_object()
        .ok_or("plugin.json must be a JSON object")?;

    match obj.get("$schema").and_then(|v| v.as_str()) {
        Some(PLUGIN_SCHEMA) => {}
        Some(other) => return Err(format!("unsupported Agent Plugins version: {other}")),
        None => return Err("missing required field $schema".into()),
    }

    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("missing required field name")?;
    if !valid_name(name) {
        return Err(format!("invalid plugin name: {name}"));
    }

    const KNOWN: &[&str] = &[
        "$schema",
        "name",
        "version",
        "description",
        "author",
        "homepage",
        "repository",
        "license",
        "keywords",
        "extensions",
    ];
    for key in obj.keys() {
        if !KNOWN.contains(&key.as_str()) {
            tracing::warn!("plugin.json: ignoring unknown field {key}");
        }
    }

    for field in [
        "version",
        "description",
        "homepage",
        "repository",
        "license",
    ] {
        if let Some(v) = obj.get(field) {
            if !v.is_string() {
                return Err(format!("{field} must be a string"));
            }
        }
    }

    if let Some(author) = obj.get("author") {
        let author = author.as_object().ok_or("author must be an object")?;
        for (k, v) in author {
            match k.as_str() {
                "name" | "email" | "url" => {
                    if !v.is_string() {
                        return Err(format!("author.{k} must be a string"));
                    }
                }
                other => return Err(format!("author has unknown field {other}")),
            }
        }
    }

    if let Some(keywords) = obj.get("keywords") {
        let keywords = keywords.as_array().ok_or("keywords must be an array")?;
        for item in keywords {
            if !item.is_string() {
                return Err("keywords must contain only strings".into());
            }
        }
    }

    if let Some(extensions) = obj.get("extensions") {
        if !extensions.is_object() {
            tracing::warn!("plugin.json: ignoring non-object extensions field");
        }
    }

    Ok(name.to_string())
}

/// Discover `SKILL.md` files from a plugin's fixed `skills/` location.
///
/// Only immediate child directories are scanned (§7.1); each must contain a
/// `SKILL.md` that resolves to a regular file within the plugin root.
fn discover_plugin_skills(root: &Path) -> Vec<PathBuf> {
    let skills_dir = root.join("skills");
    if !skills_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(&skills_dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    let mut out = Vec::new();
    for dir in dirs {
        let skill_md = dir.join("SKILL.md");
        if skill_md.is_file() && within(root, &skill_md) {
            out.push(skill_md);
        }
    }
    out
}

/// Load a single plugin from `root`, or `None` if it is not a valid plugin.
fn load_plugin(root: &Path) -> Option<Plugin> {
    let root = root.canonicalize().ok()?;
    let manifest = root.join("plugin.json");
    if !manifest.is_file() {
        return None;
    }
    if !within(&root, &manifest) {
        tracing::warn!(
            "rejecting plugin {}: plugin.json resolves outside the plugin root",
            root.display()
        );
        return None;
    }
    let content = std::fs::read_to_string(&manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&content).ok()?;
    let name = match validate_manifest(&value) {
        Ok(name) => name,
        Err(e) => {
            tracing::warn!("rejecting plugin {}: {e}", root.display());
            return None;
        }
    };
    let skills = discover_plugin_skills(&root);
    Some(Plugin { name, root, skills })
}

/// Discover and load all plugins visible to `workspace`.
///
/// Plugins are deduplicated by name, with workspace-local plugins taking
/// precedence over global ones.
pub fn discover_plugins(workspace: &Path) -> Vec<Plugin> {
    let mut plugins = Vec::new();
    let mut seen = HashSet::new();
    for root in plugin_roots(workspace) {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            if let Some(plugin) = load_plugin(&dir) {
                if seen.insert(plugin.name.clone()) {
                    plugins.push(plugin);
                }
            }
        }
    }
    plugins.sort_by(|a, b| a.name.cmp(&b.name));
    plugins
}

/// Sorted, mtime-stamped list of every plugin manifest and skill file.
///
/// Used as a cache key so skill discovery invalidates when a plugin is added,
/// removed, or its manifest or skills change.
pub fn fingerprint(workspace: &Path) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mut files = Vec::new();
    for root in plugin_roots(workspace) {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let manifest = dir.join("plugin.json");
            if manifest.is_file() {
                let m = std::fs::metadata(&manifest).and_then(|m| m.modified()).ok();
                files.push((manifest, m));
            }
            let skills_dir = dir.join("skills");
            if skills_dir.is_dir() {
                if let Ok(skills) = std::fs::read_dir(&skills_dir) {
                    for skill in skills.flatten() {
                        let skill_md = skill.path().join("SKILL.md");
                        if skill_md.is_file() {
                            let m = std::fs::metadata(&skill_md).and_then(|m| m.modified()).ok();
                            files.push((skill_md, m));
                        }
                    }
                }
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(root: &Path, name: &str, manifest: &str) -> PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.json"), manifest).unwrap();
        dir
    }

    fn write_skill(plugin: &Path, sub: &str, body: &str) {
        let dir = plugin.join("skills").join(sub);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), body).unwrap();
    }

    fn minimal_manifest(name: &str) -> String {
        format!("{{\"$schema\": \"{PLUGIN_SCHEMA}\", \"name\": \"{name}\"}}")
    }

    #[test]
    fn valid_name_accepts_spec_examples() {
        for name in ["my-plugin", "acme.tools", "lint3r", "a"] {
            assert!(valid_name(name), "{name} should be valid");
        }
    }

    #[test]
    fn valid_name_rejects_spec_examples() {
        for name in [
            "My-Plugin",
            "-start",
            "has--double",
            "too.many..dots",
            "",
            "ends-",
        ] {
            assert!(!valid_name(name), "{name} should be invalid");
        }
    }

    #[test]
    fn valid_name_rejects_overlong() {
        assert!(!valid_name(&"a".repeat(65)));
        assert!(valid_name(&"a".repeat(64)));
    }

    #[test]
    fn validate_manifest_accepts_minimal() {
        let v: serde_json::Value =
            serde_json::from_str(&minimal_manifest("minimal-plugin")).unwrap();
        assert_eq!(validate_manifest(&v).unwrap(), "minimal-plugin");
    }

    #[test]
    fn validate_manifest_rejects_unsupported_schema() {
        let v: serde_json::Value =
            serde_json::from_str(r#"{"$schema": "https://agent-plugins.org/schemas/9.9.9/plugin.schema.json", "name": "x"}"#)
                .unwrap();
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn validate_manifest_rejects_missing_name() {
        let v: serde_json::Value =
            serde_json::from_str(&format!(r#"{{"$schema": "{PLUGIN_SCHEMA}"}}"#)).unwrap();
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn validate_manifest_rejects_wrong_metadata_type() {
        let v: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"$schema": "{PLUGIN_SCHEMA}", "name": "x", "version": 3}}"#
        ))
        .unwrap();
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn validate_manifest_rejects_bad_author_field() {
        let v: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"$schema": "{PLUGIN_SCHEMA}", "name": "x", "author": {{"bogus": "y"}}}}"#
        ))
        .unwrap();
        assert!(validate_manifest(&v).is_err());
    }

    #[test]
    fn validate_manifest_ignores_unknown_top_level_field() {
        let v: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"$schema": "{PLUGIN_SCHEMA}", "name": "x", "mystery": true}}"#
        ))
        .unwrap();
        assert_eq!(validate_manifest(&v).unwrap(), "x");
    }

    #[test]
    fn validate_manifest_ignores_non_object_extensions() {
        let v: serde_json::Value = serde_json::from_str(&format!(
            r#"{{"$schema": "{PLUGIN_SCHEMA}", "name": "x", "extensions": []}}"#
        ))
        .unwrap();
        assert_eq!(validate_manifest(&v).unwrap(), "x");
    }

    #[test]
    fn discover_plugins_finds_skills() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(&plugins_dir, "reports", &minimal_manifest("reports"));
        write_skill(
            &plugin,
            "summarize",
            "---\nname: summarize\ndescription: Summarize reports\n---\n\nbody\n",
        );
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].name, "reports");
        assert_eq!(plugins[0].skills.len(), 1);
    }

    #[test]
    fn discover_plugins_skips_directory_without_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        std::fs::create_dir_all(plugins_dir.join("not-a-plugin")).unwrap();
        assert!(discover_plugins(tmp.path()).is_empty());
    }

    #[test]
    fn discover_plugins_rejects_invalid_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        write_plugin(&plugins_dir, "bad", r#"{"name": "no-schema"}"#);
        assert!(discover_plugins(tmp.path()).is_empty());
    }

    #[test]
    fn discover_plugins_does_not_recurse_into_skill_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let plugins_dir = tmp.path().join(".raven").join("plugins");
        std::fs::create_dir_all(&plugins_dir).unwrap();
        let plugin = write_plugin(&plugins_dir, "deep", &minimal_manifest("deep"));
        // A SKILL.md nested two levels under skills/ must not be discovered.
        let nested = plugin.join("skills").join("outer").join("inner");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("SKILL.md"), "---\nname: hidden\n---\n").unwrap();
        let plugins = discover_plugins(tmp.path());
        assert_eq!(plugins.len(), 1);
        assert!(plugins[0].skills.is_empty());
    }
}
