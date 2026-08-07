//! Workspace sandbox: path confinement, shell filtering, subprocess management.
//!
//! All file paths are relative to the workspace root and confined to it.
//! [`Sandbox::safe_resolve`] rejects `..` traversal. [`Sandbox::run_shell`]
//! blocks a set of destructive patterns and uses a clean environment with only
//! explicitly allowed vars.
//!
//! # Shell safety model
//!
//! The shell filter uses two complementary mechanisms, neither of which is a
//! security boundary:
//!
//! 1. **Denylist** ([`dangerous_re`]) — a regex that blocks the most obviously
//!    destructive patterns (recursive root deletes, fork bombs, `curl | sh`,
//!    etc.). This is a **best-effort guard**, not a security boundary. A
//!    denylist is inherently incomplete — it can always be bypassed (e.g. a
//!    recursive delete of a home directory is not blocked even though a
//!    recursive delete of the root is).
//!
//! 2. **Allowlist** ([`safe_command_re`]) — a regex that matches known-safe
//!    development commands (build tools, version control, file inspection,
//!    linters, test runners). When `confirm_shell` is enabled (the default,
//!    non-`--yolo` path), commands matching the allowlist run without a
//!    confirmation prompt. Anything outside the allowlist requires explicit
//!    user approval.
//!
//! The `--yolo` flag disables confirmation entirely, but the denylist still
//! applies as a last-resort filter. Neither mechanism replaces OS-level
//! sandboxing (Landlock/seccomp), which is intentionally out of scope.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;

pub(crate) const MAX_TOOL_OUTPUT: usize = 12_000;
const MAX_LINE_LENGTH: usize = 2000;
const REPLACE_ALL_WARN_THRESHOLD: usize = 20;

/// Best-effort denylist for obviously destructive shell commands.
///
/// This is **not a security boundary** — a denylist is inherently incomplete
/// and can always be bypassed. It blocks the most common destructive patterns
/// (recursive root deletes, filesystem formatting, fork bombs, `dd` to block
/// devices, `chmod 777` on root, and `curl`/`wget` piped to a shell) as a
/// first-pass filter. The `--yolo` flag and `confirm_shell` setting provide
/// the real safety net.
pub(crate) fn dangerous_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)(rm\s+(-[a-z]*f[a-z]*\s+)?/|mkfs|: \(\)\s*\{\s*:\|:&\s*\};:|dd\s+if=/dev/(zero|random|urandom)|chmod\s+(-R\s+)?777\s+/|curl\s+.*\|\s*(ba)?sh|wget\s+.*\|\s*(ba)?sh)",
        )
        .expect("valid regex")
    })
}

/// Allowlist of known-safe development commands.
///
/// When `confirm_shell` is enabled (the default, non-`--yolo` path), commands
/// matching this regex run without a confirmation prompt. Commands outside
/// this set require explicit user approval. This is a convenience, not a
/// security boundary — the allowlist can be bypassed by chaining commands
/// (e.g. `cargo build && rm -rf ~`), which is why the denylist still applies.
pub fn safe_command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)^\s*(cargo|rustc|rustup|go|node|npm|npx|yarn|pnpm|python|python3|pip|pip3|poetry|pytest|ruff|mypy|black|isort|flake8|eslint|prettier|tsc|jest|vitest|mocha|make|cmake|ninja|meson|gcc|g\+\+|clang|clang\+\+|ld|lld|ar|strip|objcopy|objdump|nm|readelf|size|strings|file|which|type|command|hash|env|printenv|pwd|ls|cat|head|tail|wc|sort|uniq|cut|tr|sed|awk|grep|rg|find|xargs|tee|diff|cmp|comm|patch|tar|gzip|gunzip|bzip2|bunzip2|xz|unxz|zip|unzip|git|hg|svn|fossil|pijul|jj|echo|printf|true|false|test|\[|expr|sleep|date|stat|du|df|basename|dirname|realpath|readlink|mkdir|touch|cp|mv|chmod|chown|id|whoami|uname|hostname|uptime|ps|time|timeout|nice|renice|nohup|exec|source|\.)(\s|$)",
        )
        .expect("valid regex")
    })
}

/// Normalize a path by resolving `.` and `..` components lexically.
///
/// Does not touch the filesystem — purely lexical normalization. This lets
/// us detect path traversal (`..` escaping the workspace) without relying on
/// `canonicalize()` (which fails for non-existent paths).
pub(crate) fn normalize_path(p: &Path) -> PathBuf {
    let mut components = Vec::new();
    for comp in p.components() {
        match comp {
            std::path::Component::ParentDir => {
                if let Some(std::path::Component::Normal(_)) = components.last() {
                    components.pop();
                } else {
                    components.push(comp);
                }
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// A workspace sandbox that confines file operations and shell commands.
///
/// All tool methods resolve paths relative to `workspace` and reject any
/// target that escapes it. `run_shell` additionally applies a best-effort
/// denylist ([`dangerous_re`]) and strips secret environment variables.
///
/// The denylist is **not a security boundary** — it can always be bypassed.
/// The `confirm_shell` setting (off with `--yolo`) provides the real safety
/// net by requiring user approval for each command. Commands matching the
/// [`safe_command_re`] allowlist skip the confirmation prompt.
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
    pub(crate) fn safe_resolve(&self, path: &str) -> Result<PathBuf> {
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

        if !self.workspace.exists() {
            return Ok(normalized);
        }

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
    ///
    /// Non-text documents (`.docx`, `.pdf`, `.xlsx`, `.odt`, `.epub`, ...) are
    /// converted to Markdown via [`super::document`] so the model can read them.
    /// Known binary files (images, audio, video, archives) are rejected.
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

        // Structured-document extraction: try before the binary guard so
        // .docx/.xlsx/.pdf render as text. Malformed documents fall through
        // to the normal text/binary handling.
        if super::document::is_extractable_document(path) {
            match super::document::extract_document_text(&p.to_string_lossy()) {
                Ok(markdown) => {
                    let lines: Vec<&str> = markdown.lines().collect();
                    let start = start_line.saturating_sub(1);
                    let end = (start + max_lines).min(lines.len());
                    let mut out = format!(
                        "--- {} (extracted document, lines {}-{} of {}) ---\n",
                        path,
                        start + 1,
                        end,
                        lines.len()
                    );
                    for (i, line) in lines[start..end].iter().enumerate() {
                        let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
                        let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
                        if out.chars().count() + rendered.chars().count() > MAX_TOOL_OUTPUT {
                            out.push_str(&format!("…[truncated at {} chars]", MAX_TOOL_OUTPUT));
                            break;
                        }
                        out.push_str(&rendered);
                    }
                    return Ok(out);
                }
                Err(e) => {
                    // Fall through to the binary guard / text read below.
                    tracing::debug!("document extraction failed for {}: {e}", path);
                }
            }
        }

        // Binary file guard: block known binary extensions (no I/O).
        if super::document::has_binary_extension(path) {
            return Ok(format!(
                "Error: {} is a binary file. Use list_dir to see available files, or run_shell to inspect it.",
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
            let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
            let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
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
            if count > REPLACE_ALL_WARN_THRESHOLD {
                return Ok(format!(
                    "Warning: replace_all would match {} occurrences in {} (threshold: {}). \
                     Provide a more specific old_string to narrow the match, \
                     or use individual search_replace calls for targeted edits.",
                    count, path, REPLACE_ALL_WARN_THRESHOLD
                ));
            }
            let new_content = content.replace(old_string, new_string);
            std::fs::write(&p, &new_content)?;
            return Ok(format!("Replaced {} occurrence(s) in {}", count, path));
        }

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
    /// `cwd` is forced to the workspace; the environment is cleared and only
    /// explicitly allowed vars (`PATH`, `HOME`, `PWD`, `LANG`) are passed
    /// through. The best-effort denylist ([`dangerous_re`]) blocks obviously
    /// destructive patterns. Output is capped at 12 000 chars.
    ///
    /// The denylist is **not a security boundary** — it can always be
    /// bypassed. The `confirm_shell` setting (off with `--yolo`) provides
    /// the real safety net by requiring user approval for each command.
    /// Commands matching the [`safe_command_re`] allowlist skip the prompt.
    pub fn run_shell(&self, command: &str, timeout_secs: u64) -> Result<String> {
        if dangerous_re().is_match(command) {
            return Ok("Error: command blocked by sandbox filter".into());
        }
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg(command)
            .current_dir(&self.workspace)
            .env_clear()
            .env("PWD", &self.workspace);
        for key in &["PATH", "HOME", "LANG"] {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        let mut child = cmd.spawn().context("spawn shell")?;
        match wait_for_child(&mut child, timeout_secs) {
            Some((status, stdout, stderr)) => {
                let mut out = format!("exit={}\n", status.code().unwrap_or(-1));
                out.push_str(&String::from_utf8_lossy(&stdout));
                out.push_str(&String::from_utf8_lossy(&stderr));
                Ok(cap_output(out))
            }
            None => Ok("Error: command timed out".into()),
        }
    }

    /// Regex content search (Grok Build `grep` semantics, pure-Rust fallback).
    ///
    /// Walks the workspace collecting file paths, then searches them in
    /// parallel. Files larger than 1 MiB are skipped to avoid dominating the
    /// search. Returns early once `max_results` matches are found.
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

        const MAX_FILE_SIZE: u64 = 1_048_576;

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

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in WalkDir::new(&search_root)
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
            let p = entry.path().to_path_buf();
            if let Some(inc) = include {
                if !super::glob_matches(&p, inc) {
                    continue;
                }
            }
            if p.metadata()
                .map(|m| m.len() > MAX_FILE_SIZE)
                .unwrap_or(true)
            {
                continue;
            }
            files.push(p);
        }

        let searched = files.len() as u32;
        let results: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let done = AtomicBool::new(false);
        let next = AtomicU32::new(0);

        std::thread::scope(|s| {
            let num_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(files.len().max(1));
            for _ in 0..num_threads {
                s.spawn(|| loop {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed) as usize;
                    if idx >= files.len() {
                        return;
                    }
                    let p = &files[idx];
                    let Ok(text) = std::fs::read_to_string(p) else {
                        continue;
                    };
                    for (i, line) in text.lines().enumerate() {
                        if re.is_match(line) {
                            let rel = p.strip_prefix(&self.workspace).unwrap_or(p);
                            let snippet: String = line.trim().chars().take(220).collect();
                            let mut r = results.lock().unwrap();
                            r.push(format!("{}:{}: {}", rel.display(), i + 1, snippet));
                            if r.len() >= max_results {
                                done.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                });
            }
        });

        let results = results.into_inner().unwrap();
        Ok(if results.is_empty() {
            format!("No matches (searched {} files)", searched)
        } else {
            results.join("\n")
        })
    }

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

        let mut child = Command::new(cmd)
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawn test runner")?;
        match wait_for_child(&mut child, 600) {
            Some((status, stdout, stderr)) => {
                let mut out = format!(
                    "--- run_tests ({}) exit={} ---\n",
                    cmd,
                    status.code().unwrap_or(-1)
                );
                out.push_str(&String::from_utf8_lossy(&stdout));
                if !stderr.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&stderr));
                }
                Ok(cap_output(out))
            }
            None => Ok("Error: test runner timed out".into()),
        }
    }

    /// Whether the workspace has a detectable test runner (Cargo, npm, or
    /// pytest). Mirrors the detection in [`Self::run_tests`]. Used by the
    /// enforced-verify gate to skip when there is nothing to run.
    pub fn has_test_runner(&self) -> bool {
        self.workspace.join("Cargo.toml").exists()
            || self.workspace.join("package.json").exists()
            || self.workspace.join("pytest.ini").exists()
            || self.workspace.join("pyproject.toml").exists()
            || self.workspace.join("setup.py").exists()
    }

    /// Auto-detect and run the project's linter / type checker.
    ///
    /// Non-mutating: reports problems without fixing them. Prefers the fastest
    /// check per ecosystem: `cargo clippy` for Rust, `tsc --noEmit` for
    /// TypeScript, `eslint` for plain JS, `pytest --collect-only` is avoided
    /// (that's a test, not a lint) — plain Python uses `python -m py_compile`.
    pub fn run_lint(&self) -> Result<String> {
        let has = |name: &str| self.workspace.join(name).exists();
        let (cmd, args): (&str, Vec<&str>) = if has("Cargo.toml") {
            (
                "cargo",
                vec!["clippy", "--all-targets", "--", "-D", "warnings"],
            )
        } else if has("tsconfig.json") {
            ("npx", vec!["tsc", "--noEmit", "-p", "tsconfig.json"])
        } else if has("package.json") {
            ("npx", vec!["eslint", "."])
        } else if has("pyproject.toml") || has("pytest.ini") || has("setup.py") {
            ("python", vec!["-m", "compileall", "-q", "."])
        } else {
            return Ok("No linter detected (no Cargo.toml, tsconfig.json, package.json, or Python config found)".into());
        };

        let mut child = Command::new(cmd)
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawn linter")?;
        match wait_for_child(&mut child, 600) {
            Some((status, stdout, stderr)) => {
                let mut out = format!(
                    "--- run_lint ({}) exit={} ---\n",
                    cmd,
                    status.code().unwrap_or(-1)
                );
                out.push_str(&String::from_utf8_lossy(&stdout));
                if !stderr.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&stderr));
                }
                Ok(cap_output(out))
            }
            None => Ok("Error: linter timed out".into()),
        }
    }
}

/// Truncate output to max chars with a clear marker.
///
/// Char-safe: truncates on a character boundary so multi-byte UTF-8 (non-ASCII
/// text in diffs, test output, etc.) never panics on a byte-slice boundary.
pub(crate) fn truncate_output(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{}\n...[truncated at {} chars]", truncated, max)
    }
}

/// Cap tool output to [`MAX_TOOL_OUTPUT`] chars (char-safe) with a marker.
pub(crate) fn cap_output(s: String) -> String {
    if s.chars().count() <= MAX_TOOL_OUTPUT {
        s
    } else {
        let truncated: String = s.chars().take(MAX_TOOL_OUTPUT).collect();
        format!("{}\n...[truncated]", truncated)
    }
}

/// Run a spawned child to completion with a timeout, draining stdout/stderr
/// on background threads so a chatty child can't deadlock the pipe buffers.
///
/// Returns `Some((exit_status, stdout, stderr))` on completion, or `None` if
/// the child did not finish within `timeout_secs` (the child is killed).
pub(crate) fn wait_for_child(
    child: &mut std::process::Child,
    timeout_secs: u64,
) -> Option<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
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

    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(timeout_secs);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(_) => return None,
        }
    };

    let stdout = stdout_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    let stderr = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default();
    Some((status, stdout, stderr))
}

pub(crate) enum TestRunner {
    Cargo,
    Npm,
    Pytest,
}

/// Literal case-insensitive search (kept for compatibility).
pub(crate) fn sandbox_search_code(
    sandbox: &Sandbox,
    query: &str,
    max_results: usize,
) -> Result<String> {
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
