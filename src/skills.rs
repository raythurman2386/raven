//! Skills — user-defined SKILL.md files the agent can discover and load.
//!
//! Mirrors Grok Build's skills system (`xai-grok-tools/.../skills/`) in a
//! dependency-light form: discover `SKILL.md` files under `.raven/skills/`
//! (workspace) and `~/.raven/skills/` (global), parse their YAML frontmatter's
//! `name`/`description`, and let the agent find a skill by keyword
//! (`skill_search`) or load its body as a `<skill>` envelope (`skill_load`).
//!
//! Frontmatter is parsed with a tiny line-based extractor (no YAML dep): only
//! the scalar `name` and `description` fields are read; anything more complex
//! is out of scope for a mini harness.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::SystemTime;

/// Max chars of a skill body injected into context (keep it bounded).
const MAX_SKILL_BODY_CHARS: usize = 8000;
/// Max chars of a description surfaced in search results.
const MAX_DESC_CHARS: usize = 500;

/// A discovered skill.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// Parse `name` and `description` from YAML frontmatter (line-based, tolerant).
///
/// Returns the body (everything after the closing `---`) and the parsed
/// name/description. Handles quoted and unquoted scalar values.
fn parse_skill_file(content: &str) -> (String, String, String) {
    let content = content.trim_start();
    let (front, body) = if let Some(rest) = content.strip_prefix("---") {
        match rest.find("\n---") {
            Some(end) => (&rest[..end], rest[end + 4..].to_string()),
            None => ("", content.to_string()),
        }
    } else {
        ("", content.to_string())
    };

    let mut name = String::new();
    let mut description = String::new();
    for line in front.lines() {
        let line = line.trim();
        if let Some((key, value)) = line.split_once(':') {
            let v = unquote(value.trim());
            match key.trim() {
                "name" if name.is_empty() => name = v.to_string(),
                "description" if description.is_empty() => description = v.to_string(),
                _ => {}
            }
        }
    }

    (name, description, body)
}

/// Strip one pair of surrounding quotes and trim.
fn unquote(v: &str) -> &str {
    v.strip_prefix('"')
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
        .unwrap_or(v)
}

/// Discover all SKILL.md files under `.raven/skills/` in `dir`, recursively.
fn find_skill_md(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let skills_dir = dir.join(".raven").join("skills");
    if skills_dir.is_dir() {
        walk_skills(&skills_dir, &mut out, 0);
    }
    out
}

fn walk_skills(dir: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 5 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    for d in dirs {
        let skill_md = d.join("SKILL.md");
        if skill_md.is_file() {
            out.push(skill_md);
        }
        walk_skills(&d, out, depth + 1);
    }
}

/// Cached skill list keyed by workspace path, invalidated when skills
/// directories change (tracked via directory mtime).
#[derive(Debug, Clone)]
struct CacheEntry {
    skills: Vec<Skill>,
    mtime: Option<SystemTime>,
}

static DISCOVER_CACHE: LazyLock<Mutex<HashMap<PathBuf, CacheEntry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn skills_dir_mtime(workspace: &Path) -> Option<SystemTime> {
    let mut max_mtime: Option<SystemTime> = None;
    let ws_dir = workspace.join(".raven").join("skills");
    if let Ok(meta) = std::fs::metadata(&ws_dir) {
        if let Ok(m) = meta.modified() {
            max_mtime = Some(m);
        }
    }
    if let Some(home) = dirs::home_dir() {
        let home_dir = home.join(".raven").join("skills");
        if let Ok(meta) = std::fs::metadata(&home_dir) {
            if let Ok(m) = meta.modified() {
                match max_mtime {
                    Some(existing) if m > existing => max_mtime = Some(m),
                    None => max_mtime = Some(m),
                    _ => {}
                }
            }
        }
    }
    max_mtime
}

/// Discover and parse all skills visible to `workspace`.
///
/// Results are cached and invalidated only when the skills directories change.
pub fn discover(workspace: &Path) -> Vec<Skill> {
    let current_mtime = skills_dir_mtime(workspace);

    {
        let cache = DISCOVER_CACHE.lock().unwrap();
        if let Some(entry) = cache.get(workspace) {
            if entry.mtime == current_mtime {
                return entry.skills.clone();
            }
        }
    }

    let mut skills = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let mut paths = find_skill_md(workspace);
    if let Some(home) = dirs::home_dir() {
        paths.extend(find_skill_md(&home));
    }

    for path in paths {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let (name, description, _body) = parse_skill_file(&content);
        if name.is_empty() {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        let description = description.chars().take(MAX_DESC_CHARS).collect::<String>();
        skills.push(Skill {
            name,
            description,
            path,
        });
    }

    skills.sort_by(|a, b| a.name.cmp(&b.name));

    let mut cache = DISCOVER_CACHE.lock().unwrap();
    cache.insert(
        workspace.to_path_buf(),
        CacheEntry {
            skills: skills.clone(),
            mtime: current_mtime,
        },
    );

    skills
}

/// List skills whose name or description matches `query` (case-insensitive).
///
/// Returns a concise list of `name — description` lines. Empty query lists all.
pub fn search(workspace: &Path, query: &str) -> String {
    let q = query.trim().to_lowercase();
    let skills = discover(workspace);
    if skills.is_empty() {
        return "No skills found. Drop SKILL.md files in .raven/skills/ or ~/.raven/skills/."
            .into();
    }

    let matched: Vec<&Skill> = if q.is_empty() {
        skills.iter().collect()
    } else {
        skills
            .iter()
            .filter(|s| {
                s.name.to_lowercase().contains(&q) || s.description.to_lowercase().contains(&q)
            })
            .collect()
    };

    if matched.is_empty() {
        return format!("No skills match '{query}'.");
    }

    let mut out = String::from("Available skills:\n");
    for s in matched {
        let desc = if s.description.is_empty() {
            "(no description)".to_string()
        } else {
            s.description.clone()
        };
        out.push_str(&format!("- {} — {}\n", s.name, desc));
    }
    out.trim_end().to_string()
}

/// Load a skill's body (frontmatter stripped) as a `<skill>` envelope, or an
/// error string if not found.
pub fn load(workspace: &Path, name: &str) -> String {
    let name = name.trim();
    for skill in discover(workspace) {
        if skill.name == name {
            match std::fs::read_to_string(&skill.path) {
                Ok(content) => {
                    let (_n, _d, body) = parse_skill_file(&content);
                    let body: String = body.chars().take(MAX_SKILL_BODY_CHARS).collect();
                    return format!(
                        "<skill name=\"{}\" path=\"{}\">\n{}\n</skill>",
                        skill.name,
                        skill.path.display(),
                        body
                    );
                }
                Err(e) => return format!("Error loading skill '{name}': {e}"),
            }
        }
    }
    format!("Skill '{name}' not found. Use skill_search to list available skills.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(dir: &Path, sub: &str, name: &str, desc: &str, body: &str) {
        let d = dir.join(".raven").join("skills").join(sub);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: \"{desc}\"\n---\n\n{body}\n"),
        )
        .unwrap();
    }

    #[test]
    fn discover_finds_skills_and_parses_frontmatter() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "review",
            "code-review",
            "Do a code review",
            "Review the diff.\n",
        );
        let skills = discover(tmp.path());
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "code-review");
        assert_eq!(skills[0].description, "Do a code review");
    }

    #[test]
    fn discover_skips_skill_without_name() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join(".raven").join("skills").join("nope");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), "no frontmatter here").unwrap();
        assert!(discover(tmp.path()).is_empty());
    }

    #[test]
    fn search_matches_name_and_description() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a",
            "commit",
            "Write conventional commits",
            "body",
        );
        write_skill(tmp.path(), "b", "refactor", "Clean up code", "body");
        let out = search(tmp.path(), "commit");
        assert!(out.contains("commit"));
        assert!(!out.contains("refactor"));
        // Description match
        let out = search(tmp.path(), "clean up");
        assert!(out.contains("refactor"));
    }

    #[test]
    fn search_empty_query_lists_all() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a", "one", "first", "body");
        write_skill(tmp.path(), "b", "two", "second", "body");
        let out = search(tmp.path(), "");
        assert!(out.contains("one"));
        assert!(out.contains("two"));
    }

    #[test]
    fn search_no_match_returns_message() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a", "one", "first", "body");
        let out = search(tmp.path(), "zzz");
        assert!(out.contains("No skills match"));
    }

    #[test]
    fn load_returns_skill_envelope() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "a",
            "commit",
            "desc",
            "Write a good commit message.",
        );
        let out = load(tmp.path(), "commit");
        assert!(out.contains("<skill name=\"commit\""));
        assert!(out.contains("Write a good commit message."));
        assert!(out.ends_with("</skill>"));
    }

    #[test]
    fn load_unknown_skill_returns_message() {
        let tmp = tempfile::tempdir().unwrap();
        let out = load(tmp.path(), "missing");
        assert!(out.contains("not found"));
    }

    #[test]
    fn discover_cache_returns_same_result_on_repeat_call() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a", "cache-test", "Cache test skill", "body");
        let first = discover(tmp.path());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "cache-test");
        let second = discover(tmp.path());
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].name, "cache-test");
    }

    #[test]
    fn discover_cache_invalidates_on_new_skill() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(tmp.path(), "a", "first-skill", "First", "body");
        let first = discover(tmp.path());
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name, "first-skill");
        write_skill(tmp.path(), "b", "second-skill", "Second", "body");
        let second = discover(tmp.path());
        assert_eq!(second.len(), 2);
    }
}
