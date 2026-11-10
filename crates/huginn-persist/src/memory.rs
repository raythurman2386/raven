//! Project memory — cross-session Markdown memory files.
//!
//! Workspace memory at `.raven/MEMORY.md` is injected into the setup
//! prompt after AGENTS.md (default ~8KB / 100 lines; leaner under
//! `lean_prompt` or docs-oriented tasks). The agent can update memory via
//! the `memory_update` tool.

use anyhow::{Context, Result};
use std::path::Path;

/// Default MEMORY injection budget (reduced from 25k — dumping the full
/// file on every task burned ~25k of setup before any real work).
pub const MAX_MEMORY_CHARS: usize = 8_000;
pub const MAX_MEMORY_LINES: usize = 100;

/// Leaner budget when `lean_prompt` is on or the task is docs/verify-oriented.
pub const LEAN_MEMORY_CHARS: usize = 3_500;
pub const LEAN_MEMORY_LINES: usize = 50;

/// Char/line caps for MEMORY injection into the setup prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryBudget {
    pub max_chars: usize,
    pub max_lines: usize,
}

impl MemoryBudget {
    pub fn standard() -> Self {
        Self {
            max_chars: MAX_MEMORY_CHARS,
            max_lines: MAX_MEMORY_LINES,
        }
    }

    pub fn lean() -> Self {
        Self {
            max_chars: LEAN_MEMORY_CHARS,
            max_lines: LEAN_MEMORY_LINES,
        }
    }
}

/// Whether a pinned constraint / goal text looks docs- or verify-oriented
/// (prefer the lean MEMORY budget for those tasks).
pub fn looks_docs_oriented(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const NEEDLES: &[&str] = &[
        "doc_drift",
        "doc drift",
        "docs-drift",
        "documentation",
        "docs only",
        "docs-oriented",
        "fix docs",
        "readme",
        "verify drift",
        "live drift",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Relevance query for MEMORY injection.
///
/// Only under `lean_prompt` or a docs/verify-oriented pinned constraint.
/// Ordinary sessions keep full budgeted MEMORY with no token filter — a
/// non-docs pinned constraint must not trigger relevance slicing.
pub fn memory_relevance_for(lean_prompt: bool, constraint: Option<&str>) -> Option<&str> {
    let trimmed = constraint.map(str::trim).filter(|s| !s.is_empty());
    let docs = trimmed.is_some_and(looks_docs_oriented);
    if lean_prompt || docs {
        trimmed
    } else {
        None
    }
}

const MEMORY_TEMPLATE: &str = r#"# Project Memory

## Conventions
<!-- Coding conventions, style, tooling preferences -->

## Decisions
<!-- Architectural decisions with dates: [YYYY-MM-DD] Description -->

## Context
<!-- Project structure, constraints, environment notes -->
"#;

/// Load workspace memory from `.raven/MEMORY.md` with the standard budget.
///
/// Returns an empty string if the file doesn't exist.
pub fn load_memory(workspace: &Path) -> String {
    load_memory_budgeted(workspace, MemoryBudget::standard(), None)
}

/// Load workspace memory with an explicit budget and optional relevance query.
///
/// When `relevance` is set, prefer lines that match its tokens (preserving
/// file order) before falling back to a plain head truncate, so a smaller
/// budget still keeps task-related lessons.
pub fn load_memory_budgeted(
    workspace: &Path,
    budget: MemoryBudget,
    relevance: Option<&str>,
) -> String {
    let path = workspace.join(".raven").join("MEMORY.md");
    match std::fs::read_to_string(&path) {
        Ok(content) => select_and_truncate_memory(&content, budget, relevance),
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
        Some(c) => select_and_truncate_memory(&c, MemoryBudget::standard(), None),
        None => String::new(),
    }
}

/// Load system-scope memory from `~/.raven/system/MEMORY.md`.
///
/// Reads the system file via the home-rooted loader and falls back to the
/// global `~/.raven/MEMORY.md` when the system file is absent.
pub fn load_system_memory() -> String {
    match dirs::home_dir() {
        Some(home) => load_system_memory_from(&home),
        None => String::new(),
    }
}

/// Select (optional relevance) then truncate memory to the given budget.
fn select_and_truncate_memory(
    content: &str,
    budget: MemoryBudget,
    relevance: Option<&str>,
) -> String {
    let selected = match relevance.map(str::trim).filter(|q| !q.is_empty()) {
        Some(query) => relevance_slice(content, query),
        None => content.to_string(),
    };
    truncate_memory(&selected, budget)
}

/// Keep lines that match any relevance token, preserving file order.
///
/// Always retains Markdown headings so section structure survives. When no
/// line matches, returns the original content (caller still truncates).
fn relevance_slice(content: &str, query: &str) -> String {
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() >= 3)
        .collect();
    if tokens.is_empty() {
        return content.to_string();
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut any_match = false;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            kept.push(line);
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if tokens.iter().any(|t| lower.contains(t)) {
            kept.push(line);
            any_match = true;
        }
    }
    if !any_match {
        return content.to_string();
    }
    kept.join("\n")
}

/// Truncate memory to fit within both char and line limits.
fn truncate_memory(content: &str, budget: MemoryBudget) -> String {
    let truncated: String = content.chars().take(budget.max_chars).collect();
    let lines: Vec<&str> = truncated.lines().take(budget.max_lines).collect();
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
    update_memory_at(
        &workspace.join(".raven").join("MEMORY.md"),
        section,
        content,
    )
}

/// Append content to a section of the system-scope memory file
/// (`~/.raven/system/MEMORY.md`), mirroring where `load_system_memory` reads.
pub fn update_system_memory(section: &str, content: &str) -> Result<String> {
    let home = dirs::home_dir().context("cannot determine home directory")?;
    update_system_memory_from(&home, section, content)
}

/// Explicit-home variant of [`update_system_memory`] so tests can be
/// deterministic without mutating the process-wide `HOME` (mirrors
/// [`load_system_memory_from`]).
fn update_system_memory_from(home: &Path, section: &str, content: &str) -> Result<String> {
    let path = home.join(".raven").join("system").join("MEMORY.md");
    update_memory_at(&path, section, content)
}

/// Shared section-append implementation behind [`update_memory`] and
/// [`update_system_memory`].
fn update_memory_at(path: &Path, section: &str, content: &str) -> Result<String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file_content = if path.exists() {
        std::fs::read_to_string(path).context("read MEMORY.md")?
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

    std::fs::write(path, file_content)?;
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

    #[test]
    fn update_system_memory_writes_under_home_system() {
        let home = tempfile::tempdir().unwrap();
        update_system_memory_from(home.path(), "Conventions", "Prefer omarchy CLI").unwrap();
        let path = home.path().join(".raven").join("system").join("MEMORY.md");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("Prefer omarchy CLI"), "got: {content}");
        assert!(content.contains("## Conventions"), "template created");
    }

    #[test]
    fn update_system_memory_skips_duplicate() {
        let home = tempfile::tempdir().unwrap();
        update_system_memory_from(home.path(), "Conventions", "Prefer omarchy CLI").unwrap();
        let out =
            update_system_memory_from(home.path(), "Conventions", "Prefer omarchy CLI").unwrap();
        assert!(out.contains("already contains"), "got: {out}");
    }

    #[test]
    fn standard_budget_truncates_large_memory_with_marker() {
        let mut body = String::from("# Project Memory\n\n## Context\n");
        for i in 0..400 {
            body.push_str(&format!(
                "- engine-review lesson {i}: avoid dumping full MEMORY on every task\n"
            ));
        }
        let ws = workspace_with_memory(&body);
        let out = load_memory(&ws);
        assert!(
            out.contains("...[memory truncated]"),
            "expected truncation marker, got len={}",
            out.len()
        );
        assert!(
            out.chars().count() <= MAX_MEMORY_CHARS + 32,
            "standard budget overrun: {}",
            out.chars().count()
        );
        assert!(
            out.lines().count() <= MAX_MEMORY_LINES + 1,
            "too many lines: {}",
            out.lines().count()
        );
        assert!(out.contains("engine-review lesson 0"));
    }

    #[test]
    fn lean_budget_is_stricter_than_standard() {
        let mut body = String::from("# Project Memory\n\n## Context\n");
        for i in 0..400 {
            body.push_str(&format!(
                "- padding line {i} with enough characters to fill\n"
            ));
        }
        let ws = workspace_with_memory(&body);
        let standard = load_memory_budgeted(&ws, MemoryBudget::standard(), None);
        let lean = load_memory_budgeted(&ws, MemoryBudget::lean(), None);
        assert!(
            lean.len() < standard.len(),
            "lean={} standard={}",
            lean.len(),
            standard.len()
        );
        assert!(lean.contains("...[memory truncated]"));
        assert!(lean.chars().count() <= LEAN_MEMORY_CHARS + 32);
    }

    #[test]
    fn relevance_slice_prefers_matching_lessons() {
        let body = concat!(
            "# Project Memory\n",
            "## Decisions\n",
            "- Use Rust for services\n",
            "- Deploy via Docker\n",
            "## Context\n",
            "- doc_drift verify must stay at drift=0 before ship\n",
            "- unrelated database sharding note\n",
            "- circling: do not re-run identical doc_drift after success\n",
        );
        let ws = workspace_with_memory(body);
        let out =
            load_memory_budgeted(&ws, MemoryBudget::lean(), Some("Fix live doc_drift verify"));
        assert!(out.contains("doc_drift verify must stay"), "got: {out}");
        assert!(out.contains("identical doc_drift"), "got: {out}");
        assert!(
            !out.contains("database sharding"),
            "irrelevant line should be dropped, got: {out}"
        );
        assert!(out.contains("## Context"), "headings retained");
    }

    #[test]
    fn looks_docs_oriented_matches_doc_drift_asks() {
        assert!(looks_docs_oriented("Fix live doc_drift"));
        assert!(looks_docs_oriented("Update README only"));
        assert!(!looks_docs_oriented("Implement memory allocator"));
        assert!(!looks_docs_oriented("Ship circling guards"));
    }

    #[test]
    fn relevance_is_noop_for_non_docs_pinned_constraint() {
        // Ordinary session: non-docs constraint must not become a relevance query.
        assert_eq!(
            memory_relevance_for(false, Some("Ship circling guards")),
            None
        );
        assert_eq!(
            memory_relevance_for(false, Some("Implement memory allocator")),
            None
        );
        // Docs-oriented / lean_prompt still enable relevance.
        assert_eq!(
            memory_relevance_for(false, Some("Fix live doc_drift")),
            Some("Fix live doc_drift")
        );
        assert_eq!(
            memory_relevance_for(true, Some("Ship circling guards")),
            Some("Ship circling guards")
        );
        assert_eq!(memory_relevance_for(false, None), None);
        assert_eq!(memory_relevance_for(false, Some("   ")), None);

        // With relevance=None, load keeps the full (budgeted) file — including
        // lines a docs slice would drop.
        let body = concat!(
            "# Project Memory\n",
            "## Decisions\n",
            "- Use Rust for services\n",
            "- Deploy via Docker\n",
            "## Context\n",
            "- doc_drift verify must stay at drift=0 before ship\n",
            "- unrelated database sharding note\n",
        );
        let ws = workspace_with_memory(body);
        let out = load_memory_budgeted(&ws, MemoryBudget::standard(), None);
        assert!(out.contains("Use Rust for services"), "got: {out}");
        assert!(out.contains("database sharding"), "got: {out}");
        assert!(out.contains("doc_drift verify"), "got: {out}");
    }
}
