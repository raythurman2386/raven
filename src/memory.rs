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

    // Find the section header and append after it
    let section_header = format!("## {}", section);
    if let Some(pos) = file_content.find(&section_header) {
        // Find the end of the line containing the header
        let insert_pos = file_content[pos..]
            .find('\n')
            .map(|n| pos + n + 1)
            .unwrap_or(file_content.len());
        let entry = format!("- {}\n", content.trim());
        file_content.insert_str(insert_pos, &entry);
    } else {
        // Section not found — append it
        file_content.push_str(&format!("\n## {}\n- {}\n", section, content.trim()));
    }

    std::fs::write(&path, file_content)?;
    Ok(format!("Updated memory [{}]: {}", section, content.trim()))
}
