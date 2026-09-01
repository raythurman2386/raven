//! Project memory — cross-session Markdown memory files.
//!
//! Workspace memory at `.raven/MEMORY.md` is injected into the system
//! prompt (first 25KB / 200 lines) after AGENTS.md. The agent can update
//! memory via the `memory_update` tool.

use anyhow::{Context, Result};
use std::path::Path;

const MAX_MEMORY_CHARS: usize = 25_000;
const MAX_MEMORY_LINES: usize = 200;

const MEMORY_TEMPLATE: &str = r#"# Project Memory

## Conventions
<!-- Coding conventions, style, tooling preferences -->

## Decisions
<!-- Architectural decisions with dates: [YYYY-MM-DD] Description -->

## Context
<!-- Project structure, constraints, environment notes -->
"#;

/// Load workspace memory from `.raven/MEMORY.md`.
///
/// Returns an empty string if the file doesn't exist.
/// Truncates to MAX_MEMORY_CHARS or MAX_MEMORY_LINES (whichever hits first).
pub fn load_memory(workspace: &Path) -> String {
    let path = workspace.join(".raven").join("MEMORY.md");
    match std::fs::read_to_string(&path) {
        Ok(content) => truncate_memory(&content),
        Err(_) => String::new(),
    }
}

/// Load system-scope memory from the given home root's
/// `.raven/system/MEMORY.md`.
///
/// Used by the `--system` scope (where the sandbox root is `/`, so the
/// workspace-relative `.raven/MEMORY.md` path is not meaningful). Falls back
/// to `<home>/.raven/MEMORY.md` when the system file is absent, so an existing
/// global memory file is still read. Returns an empty string if neither exists.
fn load_system_memory_from(home: &Path) -> String {
    let system = home.join(".raven").join("system").join("MEMORY.md");
    let content = match std::fs::read_to_string(&system) {
        Ok(c) => Some(c),
        Err(_) => std::fs::read_to_string(home.join(".raven").join("MEMORY.md")).ok(),
    };
    match content {
        Some(c) => truncate_memory(&c),
        None => String::new(),
    }
}

/// Load system-scope memory from `~/.raven/system/MEMORY.md`.
/// See [`load_system_memory_from`].
pub fn load_system_memory() -> String {
    match dirs::home_dir() {
        Some(home) => load_system_memory_from(&home),
        None => String::new(),
    }
}

/// Truncate memory to fit within both char and line limits.
fn truncate_memory(content: &str) -> String {
    let truncated: String = content.chars().take(MAX_MEMORY_CHARS).collect();
    let lines: Vec<&str> = truncated.lines().take(MAX_MEMORY_LINES).collect();
    if lines.len() < content.lines().count() || truncated.chars().count() < content.chars().count()
    {
        format!("{}\n...[memory truncated]", lines.join("\n"))
    } else {
        lines.join("\n")
    }
}

/// Append content to a specific section of the workspace memory file.
///
/// Creates the file with a template if it doesn't exist.
pub fn update_memory(workspace: &Path, section: &str, content: &str) -> Result<String> {
    let dir = workspace.join(".raven");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("MEMORY.md");

    let mut file_content = if path.exists() {
        std::fs::read_to_string(&path).context("read MEMORY.md")?
    } else {
        MEMORY_TEMPLATE.to_string()
    };

    let section_header = format!("## {}", section);
    let entry = format!("- {}", content.trim());
    if let Some(pos) = file_content.find(&section_header) {
        let insert_pos = file_content[pos..]
            .find('\n')
            .map(|n| pos + n + 1)
            .unwrap_or(file_content.len());

        let section_end = file_content[insert_pos..]
            .find("\n## ")
            .map(|n| insert_pos + n)
            .unwrap_or(file_content.len());
        let section_body = &file_content[insert_pos..section_end];

        if section_body.lines().any(|line| line.trim() == entry) {
            return Ok(format!(
                "Memory [{}] already contains: {}",
                section,
                content.trim()
            ));
        }

        file_content.insert_str(insert_pos, &format!("{entry}\n"));
    } else {
        file_content.push_str(&format!("\n## {}\n{entry}\n", section));
    }

    std::fs::write(&path, file_content)?;
    Ok(format!("Updated memory [{}]: {}", section, content.trim()))
}

/// Search workspace memory for snippets matching `query`.
///
/// Keyword-scans `.raven/MEMORY.md` (the same file injected into the system
/// prompt), scoring each line by how many of the query's tokens appear.
/// Returns the top-scoring lines as `path:line — content` snippets, capped.
///
/// Grok Build uses indexed keyword + vector search (`xai-grok-memory`); a
/// mini harness gets the high-value subset with a dependency-light keyword
/// scan of the single memory file.
const MAX_SEARCH_RESULTS: usize = 10;
const MAX_SNIPPET_CHARS: usize = 200;

pub fn search_memory(workspace: &Path, query: &str) -> String {
    let path = workspace.join(".raven").join("MEMORY.md");
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return "No memory file found (.raven/MEMORY.md).".into(),
    };

    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return "Empty search query.".into();
    }

    // Score each line by how many distinct query tokens it contains.
    struct Scored {
        score: usize,
        line_no: usize,
        text: String,
    }
    let mut scored: Vec<Scored> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let lower = line.to_lowercase();
        let score = tokens.iter().filter(|t| lower.contains(t.as_str())).count();
        if score > 0 {
            scored.push(Scored {
                score,
                line_no: i + 1,
                text: line.trim().to_string(),
            });
        }
    }

    scored.sort_by_key(|s| std::cmp::Reverse(s.score));
    scored.truncate(MAX_SEARCH_RESULTS);

    if scored.is_empty() {
        return format!("No memory matches '{query}'.");
    }

    let mut out = String::from("Memory matches (path:line — content):\n");
    for s in &scored {
        let snippet: String = s.text.chars().take(MAX_SNIPPET_CHARS).collect();
        out.push_str(&format!("MEMORY.md:{} — {}\n", s.line_no, snippet));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn workspace_with_memory(body: &str) -> PathBuf {
        // Unique dir under the OS temp dir so it survives the test (no TempDir
        // guard to drop it mid-test) and doesn't collide across parallel tests.
        static COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("raven_mem_test_{}_{n}", std::process::id()));
        let raven = dir.join(".raven");
        std::fs::create_dir_all(&raven).unwrap();
        std::fs::write(raven.join("MEMORY.md"), body).unwrap();
        dir
    }

    #[test]
    fn search_returns_matching_lines() {
        let ws =
            workspace_with_memory("## Decisions\n- Use Rust for services\n- Deploy via Docker\n");
        let out = search_memory(&ws, "rust");
        assert!(out.contains("Use Rust for services"));
        assert!(!out.contains("Deploy via Docker"));
    }

    #[test]
    fn search_ranks_lines_with_more_matches_higher() {
        let ws = workspace_with_memory("## Notes\n- Rust + Rust for services\n- Rust only\n");
        let out = search_memory(&ws, "rust services");
        let rust_rust_pos = out.find("Rust + Rust").unwrap();
        let rust_only_pos = out.find("Rust only").unwrap();
        assert!(rust_rust_pos < rust_only_pos, "higher-scoring line first");
    }

    #[test]
    fn search_no_match_returns_message() {
        let ws = workspace_with_memory("## Notes\n- Something unrelated\n");
        assert!(search_memory(&ws, "zzz").contains("No memory matches"));
    }

    #[test]
    fn search_no_memory_file_returns_message() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(search_memory(tmp.path(), "x").contains("No memory file"));
    }

    #[test]
    fn search_empty_query_returns_message() {
        let ws = workspace_with_memory("## Notes\n- x\n");
        assert!(search_memory(&ws, "   ").contains("Empty search"));
    }

    #[test]
    fn search_caps_results() {
        let mut body = String::from("## Notes\n");
        for i in 0..30 {
            body.push_str(&format!("- item {i} rust keyword\n"));
        }
        let ws = workspace_with_memory(&body);
        let out = search_memory(&ws, "rust");
        assert!(out.lines().filter(|l| l.contains("item")).count() <= MAX_SEARCH_RESULTS);
    }

    #[test]
    fn update_memory_skips_duplicate_entry() {
        let ws = workspace_with_memory("## Decisions\n- Use Rust\n");
        let result = update_memory(&ws, "Decisions", "Use Rust").unwrap();
        assert!(result.contains("already contains"));
        let content = std::fs::read_to_string(ws.join(".raven").join("MEMORY.md")).unwrap();
        assert_eq!(content.matches("- Use Rust").count(), 1);
    }

    #[test]
    fn update_memory_adds_new_entry() {
        let ws = workspace_with_memory("## Decisions\n- Use Rust\n");
        let result = update_memory(&ws, "Decisions", "Deploy via Docker").unwrap();
        assert!(result.contains("Updated memory"));
        let content = std::fs::read_to_string(ws.join(".raven").join("MEMORY.md")).unwrap();
        assert!(content.contains("- Deploy via Docker"));
    }

    #[test]
    fn update_memory_creates_file_with_template() {
        let tmp = tempfile::tempdir().unwrap();
        let result = update_memory(tmp.path(), "Decisions", "Use Rust").unwrap();
        assert!(result.contains("Updated memory"));
        let content = std::fs::read_to_string(tmp.path().join(".raven").join("MEMORY.md")).unwrap();
        assert!(content.contains("## Decisions"));
        assert!(content.contains("- Use Rust"));
    }

    #[test]
    fn update_memory_skips_duplicate_in_new_section() {
        let tmp = tempfile::tempdir().unwrap();
        update_memory(tmp.path(), "Decisions", "Use Rust").unwrap();
        let result = update_memory(tmp.path(), "Decisions", "Use Rust").unwrap();
        assert!(result.contains("already contains"));
        let content = std::fs::read_to_string(tmp.path().join(".raven").join("MEMORY.md")).unwrap();
        assert_eq!(content.matches("- Use Rust").count(), 1);
    }

    #[test]
    fn load_system_memory_reads_global_system_file_when_home_present() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(".raven").join("system");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("MEMORY.md"), "## System\n- Installed docker\n").unwrap();
        // Use the explicit home-root variant so the test is deterministic on
        // every OS (no reliance on env-var-based HOME resolution).
        let out = load_system_memory_from(tmp.path());
        assert!(out.contains("Installed docker"), "got: {out}");
    }
}
