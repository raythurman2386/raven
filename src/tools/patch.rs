//! Unified diff patching: parse and apply patches.

use anyhow::{Context, Result};

use super::sandbox::Sandbox;

/// A parsed hunk from a unified diff.
pub(crate) struct DiffHunk {
    pub(crate) file_path: String,
    pub(crate) old_start: usize,
    pub(crate) lines: Vec<(DiffLineType, String)>,
}

#[derive(PartialEq)]
pub(crate) enum DiffLineType {
    Context,
    Remove,
    Add,
}

/// Parse a unified diff into hunks.
///
/// Supports the standard format:
///   --- a/file.rs
///   +++ b/file.rs
///   @@ -start,count +start,count @@
///    context line
///   -removed line
///   +added line
pub(crate) fn parse_unified_diff(text: &str) -> Vec<DiffHunk> {
    let mut hunks = Vec::new();
    let mut current_file: Option<String> = None;
    let mut current_hunk: Option<DiffHunk> = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("--- ") {
            let path = rest.trim().strip_prefix("a/").unwrap_or(rest.trim());
            current_file = Some(path.to_string());
        } else if let Some(rest) = line.strip_prefix("+++ ") {
            let path = rest.trim().strip_prefix("b/").unwrap_or(rest.trim());
            current_file = Some(path.to_string());
        } else if line.starts_with("@@ ") {
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }
            let file = current_file.clone().unwrap_or_default();
            let parts: Vec<&str> = line.split_whitespace().collect();
            let old_start = parts
                .get(1)
                .and_then(|s| s.strip_prefix('-'))
                .and_then(|s| s.split(',').next())
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            current_hunk = Some(DiffHunk {
                file_path: file,
                old_start,
                lines: Vec::new(),
            });
        } else if let Some(h) = current_hunk.as_mut() {
            if line.starts_with('-') && !line.starts_with("---") {
                h.lines.push((DiffLineType::Remove, line[1..].to_string()));
            } else if line.starts_with('+') && !line.starts_with("+++") {
                h.lines.push((DiffLineType::Add, line[1..].to_string()));
            } else if line.starts_with(' ') || line.is_empty() {
                h.lines.push((
                    DiffLineType::Context,
                    line.trim_start_matches(' ').to_string(),
                ));
            }
        }
    }

    if let Some(h) = current_hunk.take() {
        hunks.push(h);
    }
    hunks
}

/// Apply a single hunk to file content.
///
/// Finds the context lines in the file content starting at old_start,
/// replaces removed lines with added lines.
pub(crate) fn apply_hunk(content: &str, hunk: &DiffHunk) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start_idx = hunk.old_start.saturating_sub(1);

    let mut expected: Vec<&str> = Vec::new();
    let mut replacement: Vec<String> = Vec::new();

    for (line_type, text) in &hunk.lines {
        match line_type {
            DiffLineType::Context => {
                expected.push(text.as_str());
                replacement.push(text.clone());
            }
            DiffLineType::Remove => {
                expected.push(text.as_str());
            }
            DiffLineType::Add => {
                replacement.push(text.clone());
            }
        }
    }

    if start_idx + expected.len() > lines.len() {
        return Ok(format!(
            "Error: patch context exceeds file length for {}",
            hunk.file_path
        ));
    }

    for (i, expected_line) in expected.iter().enumerate() {
        let file_line = lines[start_idx + i].trim_end();
        let exp = expected_line.trim_end();
        if file_line != exp {
            return Ok(format!(
                "Error: patch context mismatch in {} at line {}: expected {:?}, got {:?}",
                hunk.file_path,
                start_idx + i + 1,
                exp,
                file_line
            ));
        }
    }

    let mut result = Vec::new();
    for &line in &lines[..start_idx] {
        result.push(line.to_string());
    }
    for line in &replacement {
        result.push(line.clone());
    }
    for &line in &lines[start_idx + expected.len()..] {
        result.push(line.to_string());
    }

    Ok(result.join("\n") + if content.ends_with('\n') { "\n" } else { "" })
}

impl Sandbox {
    /// Apply a unified diff patch to files in the workspace.
    ///
    /// Supports multiple hunks and files. Rejects if context lines don't match.
    pub fn apply_patch(&self, patch_text: &str) -> Result<String> {
        let hunks = parse_unified_diff(patch_text);
        if hunks.is_empty() {
            return Ok("Error: no valid hunks found in patch".into());
        }

        let mut changed_files = Vec::new();
        for hunk in &hunks {
            let path = self.safe_resolve(&hunk.file_path)?;
            if !path.is_file() {
                return Ok(format!(
                    "Error: {} is not a file (cannot patch)",
                    hunk.file_path
                ));
            }
            let content = std::fs::read_to_string(&path).context("read file for patch")?;
            let new_content = apply_hunk(&content, hunk)?;
            if new_content.starts_with("Error:") {
                return Ok(new_content);
            }
            std::fs::write(&path, new_content)?;
            changed_files.push(hunk.file_path.clone());
        }

        Ok(format!(
            "Patched {} file(s): {}",
            changed_files.len(),
            changed_files.join(", ")
        ))
    }
}
