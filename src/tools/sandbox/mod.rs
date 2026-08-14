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
//! applies as a last-resort filter. These shell-level guards are complemented
//! by OS-level sandboxing (Landlock, seccomp, rlimits, openat2) applied to
//! every subprocess — see [`confinement`] and
//! [`docs/security.md`](../docs/security.md).

mod confinement;

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use walkdir::WalkDir;

use confinement::run_confined;

pub(crate) use confinement::{setup_shell_env, spawn_confined, wait_for_child};

/// Cross-platform open flags for [`Sandbox::open_beneath`].
///
/// Kept intentionally small — just the flags the sandbox's file tools need.
/// Linux maps these onto `rustix::fs::OFlags` for `openat2`; other platforms
/// map them onto `std::fs::OpenOptions`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct OpenFlags(u32);

impl std::ops::BitOr for OpenFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl OpenFlags {
    pub(crate) const RDONLY: Self = Self(1 << 0);
    pub(crate) const WRONLY: Self = Self(1 << 1);
    pub(crate) const CREATE: Self = Self(1 << 2);
    pub(crate) const TRUNC: Self = Self(1 << 3);
    pub(crate) const APPEND: Self = Self(1 << 4);
    pub(crate) const CLOEXEC: Self = Self(1 << 5);

    fn contains(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    #[cfg(target_os = "linux")]
    fn to_rustix(self) -> rustix::fs::OFlags {
        let mut f = rustix::fs::OFlags::empty();
        if self.contains(Self::RDONLY) {
            f |= rustix::fs::OFlags::RDONLY;
        }
        if self.contains(Self::WRONLY) {
            f |= rustix::fs::OFlags::WRONLY;
        }
        if self.contains(Self::CREATE) {
            f |= rustix::fs::OFlags::CREATE;
        }
        if self.contains(Self::TRUNC) {
            f |= rustix::fs::OFlags::TRUNC;
        }
        if self.contains(Self::APPEND) {
            f |= rustix::fs::OFlags::APPEND;
        }
        if self.contains(Self::CLOEXEC) {
            f |= rustix::fs::OFlags::CLOEXEC;
        }
        f
    }
}

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
            r"(?i)(rm\s+(-[a-z]*f[a-z]*\s+)?/|mkfs|: \(\)\s*\{\s*:\|:&\s*\};:|dd\s+if=/dev/(zero|random|urandom)|chmod\s+(-R\s+)?777\s+/|curl\s+.*\|\s*(ba)?sh|wget\s+.*\|\s*(ba)?sh|format\s+[A-Za-z]:|del\s+/[sfq]\s+[A-Za-z]:\\|rd\s+/[sq]\s+[A-Za-z]:\\|rmdir\s+/[sq]\s+[A-Za-z]:\\|powershell\s+-[Cc]ommand\s+.*Remove-Item.*-Recurse.*-Force|Remove-Item\s+-Recurse\s+-Force\s+[A-Za-z]:\\|diskpart)",
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
            r"(?i)^\s*(cargo|rustc|rustup|go|node|npm|npx|yarn|pnpm|python|python3|pip|pip3|poetry|pytest|ruff|mypy|black|isort|flake8|eslint|prettier|tsc|jest|vitest|mocha|make|cmake|ninja|meson|gcc|g\+\+|clang|clang\+\+|ld|lld|ar|strip|objcopy|objdump|nm|readelf|size|strings|file|where|which|type|command|hash|set|env|printenv|pwd|cd|ls|dir|cat|head|tail|wc|sort|uniq|cut|tr|sed|awk|grep|rg|find|findstr|xargs|tee|diff|cmp|comp|fc|comm|patch|tar|gzip|gunzip|bzip2|bunzip2|xz|unxz|zip|unzip|git|hg|svn|fossil|pijul|jj|echo|printf|true|false|test|\[|expr|sleep|date|stat|du|df|basename|dirname|realpath|readlink|mkdir|touch|copy|cp|move|mv|ren|rename|chmod|chown|icacls|attrib|id|whoami|uname|hostname|uptime|ps|tasklist|time|timeout|nice|renice|nohup|exec|source|\.|call|cmd)(\s|$)",
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
/// denylist (`dangerous_re`) and strips secret environment variables.
///
/// The denylist is **not a security boundary** — it can always be bypassed.
/// The `confirm_shell` setting (off with `--yolo`) provides the real safety
/// net by requiring user approval for each command. Commands matching the
/// [`safe_command_re`] allowlist skip the confirmation prompt.
#[derive(Clone)]
pub struct Sandbox {
    /// The workspace root; all paths are confined to this directory.
    pub workspace: PathBuf,
    /// Extra Landlock RW roots granted to every confined child (e.g. a git
    /// worktree's shared main repo). Defaults to empty. The process temp dir
    /// is never implied — see [`confinement::FsPolicy`]. Never granted on
    /// Windows (no Landlock).
    pub extra_rw: Vec<PathBuf>,
}

impl Sandbox {
    /// Create a sandbox rooted at `workspace`.
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            extra_rw: Vec::new(),
        }
    }

    /// Create a sandbox rooted at `workspace` with extra Landlock RW roots
    /// granted to every confined child. Used for git worktree sub-agents that
    /// must reach the shared main repo (a sibling under the temp dir).
    pub fn with_extra_rw(workspace: PathBuf, extra_rw: Vec<PathBuf>) -> Self {
        Self {
            workspace,
            extra_rw,
        }
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

    /// Open a file relative to the workspace root with kernel-enforced path
    /// confinement.
    ///
    /// On Linux, uses `openat2` with `RESOLVE_BENEATH | NO_MAGICLINKS`, which
    /// makes the kernel refuse to resolve any path that escapes the workspace
    /// — atomically, with no TOCTOU race (a symlink cannot be swapped in
    /// between the check and the open). On other platforms, falls back to
    /// [`Self::safe_resolve`] + `std::fs::File::open`.
    ///
    /// `path` must be relative to the workspace root (e.g. `src/main.rs`).
    pub(crate) fn open_beneath(
        &self,
        path: &str,
        flags: OpenFlags,
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] mode: u32,
    ) -> Result<std::fs::File> {
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{openat2, ResolveFlags};
            let ws_dir = std::fs::File::open(&self.workspace)
                .map_err(|e| anyhow::anyhow!("open workspace dir: {e}"))?;
            let fd = openat2(
                &ws_dir,
                path,
                flags.to_rustix(),
                rustix::fs::Mode::from_bits_truncate(mode),
                ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "Path outside workspace or unopenable: {path}. Use relative paths like 'src/main.rs', not absolute paths starting with / ({e})"
                )
            })?;
            Ok(std::fs::File::from(fd))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let p = self.safe_resolve(path)?;
            let file = std::fs::OpenOptions::new()
                .read(flags.contains(OpenFlags::RDONLY))
                .write(flags.contains(OpenFlags::WRONLY))
                .create(flags.contains(OpenFlags::CREATE))
                .append(flags.contains(OpenFlags::APPEND))
                .truncate(flags.contains(OpenFlags::TRUNC))
                .open(&p)?;
            Ok(file)
        }
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
    /// converted to Markdown via `super::document` so the model can read them.
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
                    let mut used = out.chars().count();
                    for (i, line) in lines[start..end].iter().enumerate() {
                        let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
                        let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
                        let rendered_len = rendered.chars().count();
                        if used + rendered_len > MAX_TOOL_OUTPUT {
                            out.push_str(&format!("…[truncated at {} chars]", MAX_TOOL_OUTPUT));
                            break;
                        }
                        out.push_str(&rendered);
                        used += rendered_len;
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

        // Open via openat2 (kernel-enforced confinement, no TOCTOU race).
        let file = self.open_beneath(path, OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0)?;
        let text = std::io::read_to_string(file).context("read file")?;
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
        let mut used = out.chars().count();
        for (i, line) in lines[start..end].iter().enumerate() {
            let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
            let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
            let rendered_len = rendered.chars().count();
            if used + rendered_len > MAX_TOOL_OUTPUT {
                out.push_str(&format!("…[truncated at {} chars]", MAX_TOOL_OUTPUT));
                break;
            }
            out.push_str(&rendered);
            used += rendered_len;
        }
        Ok(out)
    }

    /// Full file write (create/overwrite).
    pub fn write_file(&self, path: &str, content: &str) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rel = normalize_path(Path::new(path))
            .to_string_lossy()
            .into_owned();
        let mut file = self.open_beneath(
            &rel,
            OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
            0o644,
        )?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
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
            let rel = normalize_path(Path::new(path))
                .to_string_lossy()
                .into_owned();
            let mut file = self.open_beneath(
                &rel,
                OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
                0o644,
            )?;
            use std::io::Write;
            file.write_all(new_string.as_bytes())?;
            return Ok(format!("Created {} ({} bytes)", path, new_string.len()));
        }

        if !p.is_file() {
            return Ok(format!("Error: {} is not a file", path));
        }

        // Normalize the path lexically so `..` components that cancel out
        // (e.g. `newdir/../f.txt`) resolve to a bare relative path before we
        // hand it to `openat2` (RESOLVE_BENEATH). Without this, openat2 tries
        // to traverse `newdir` literally and fails with ENOENT when the
        // intermediate dir does not exist — even though `safe_resolve` already
        // validated the normalized path. (Issues #104, #108.)
        let rel = normalize_path(Path::new(path))
            .to_string_lossy()
            .into_owned();

        // Open via openat2 (kernel-enforced confinement, no TOCTOU race).
        let file = self.open_beneath(&rel, OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0)?;
        let content = std::io::read_to_string(file).context("read file before edit")?;

        if replace_all {
            let count = content.matches(old_string).count();
            if count == 0 {
                return Ok(format!("Error: old_string not found in {}", path));
            }
            let new_content = content.replace(old_string, new_string);
            let mut file = self.open_beneath(
                &rel,
                OpenFlags::WRONLY | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
                0,
            )?;
            use std::io::Write;
            file.write_all(new_content.as_bytes())?;
            if count > REPLACE_ALL_WARN_THRESHOLD {
                return Ok(format!(
                    "Replaced {} occurrence(s) in {} (warning: large count, \
                     verify this was intended)",
                    count, path
                ));
            }
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
                let mut file = self.open_beneath(
                    &rel,
                    OpenFlags::WRONLY | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
                    0,
                )?;
                use std::io::Write;
                file.write_all(new_content.as_bytes())?;
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
    /// through. The best-effort denylist (`dangerous_re`) blocks obviously
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

        // Direct-exec path: if the command is a known-safe single binary with
        // no shell metacharacters, run it without a shell. This removes the
        // shell-injection surface entirely for the common case.
        let mut cmd = if is_direct_exec_command(command) {
            match parse_argv(command).and_then(|argv| {
                let mut it = argv.into_iter();
                let bin = it.next()?;
                let mut c = Command::new(bin);
                c.args(it);
                Some(c)
            }) {
                Some(c) => c,
                None => shell_command(command),
            }
        } else {
            shell_command(command)
        };

        cmd.current_dir(&self.workspace);
        setup_shell_env(&mut cmd, &self.workspace);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        run_confined(
            &mut cmd,
            &self.workspace,
            timeout_secs,
            &self.extra_rw,
            false,
        )
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
                            let mut r = results.lock().unwrap_or_else(|e| e.into_inner());
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

        let results = results.into_inner().unwrap_or_else(|e| e.into_inner());
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

        let (cmd, args): (&str, Vec<&str>) = match runner {
            TestRunner::Cargo => ("cargo", vec!["test"]),
            TestRunner::Npm => {
                if self.uses_vitest() {
                    (
                        "npx",
                        vec![
                            "vitest",
                            "--run",
                            "--pool=threads",
                            "--poolOptions.threads.singleThread",
                        ],
                    )
                } else {
                    ("npm", vec!["test"])
                }
            }
            TestRunner::Pytest => ("pytest", vec![]),
        };

        let mut command = Command::new(resolve_command(cmd));
        command
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        setup_shell_env(&mut command, &self.workspace);
        command.env("CI", "true");
        // Sanctioned test runner: skip the seccomp network block for npm
        // projects. vitest/v8 opens an AF_INET socket for V8 coverage + worker
        // IPC, which the block SIGSYS-kills. This is a user-sanctioned command
        // (not arbitrary model output), so the exemption does not weaken the
        // exfiltration guarantee. The flag is threaded into the pre_exec
        // closure — setting it via `command.env` alone is dead code because
        // pre_exec reads the parent env, not the Command::env override.
        let skip_network_block = matches!(runner, TestRunner::Npm);
        let mut confined = spawn_confined(
            &mut command,
            &self.workspace,
            &self.extra_rw,
            skip_network_block,
        )
        .context("spawn test runner")?;
        match wait_for_child(&mut confined.child, 600) {
            Some((status, stdout, stderr)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(signal) = status.signal() {
                        let mut out =
                            format!("--- run_tests ({cmd}) killed by signal {signal} ---\n",);
                        out.push_str(&String::from_utf8_lossy(&stdout));
                        if !stderr.is_empty() {
                            out.push_str(&String::from_utf8_lossy(&stderr));
                        }
                        return Ok(cap_output(out));
                    }
                }
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
    ///
    /// For npm projects, also requires `node_modules` to exist so the gate
    /// doesn't loop on scaffolding tasks where deps aren't installed yet.
    /// For Python projects, checks that `pytest` is on PATH.
    pub fn has_test_runner(&self) -> bool {
        if self.workspace.join("Cargo.toml").exists() {
            return true;
        }
        if self.workspace.join("package.json").exists() {
            return self.workspace.join("node_modules").is_dir();
        }
        if self.workspace.join("pytest.ini").exists()
            || self.workspace.join("pyproject.toml").exists()
            || self.workspace.join("setup.py").exists()
        {
            return std::process::Command::new(resolve_command("pytest"))
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok();
        }
        false
    }

    fn uses_vitest(&self) -> bool {
        let pkg = self.workspace.join("package.json");
        if !pkg.exists() {
            return false;
        }
        std::fs::read_to_string(&pkg)
            .ok()
            .map(|s| s.contains("\"vitest\""))
            .unwrap_or(false)
    }

    /// Whether a shell command is a test, typecheck, or lint invocation.
    ///
    /// Used by the enforced-verify gate to credit `run_shell`-based
    /// verification (e.g. `npm test`, `cargo clippy`, `pytest`) the same
    /// way it credits the `run_tests` tool.
    pub fn is_verification_command(command: &str) -> bool {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(
                r"(?i)^\s*(cargo\s+(test|clippy|fmt\s+--\s*check)|npm\s+(test|run\s+(test|typecheck|lint))|npx\s+(jest|vitest|mocha|tsc)|yarn\s+(test|typecheck|lint)|pnpm\s+(test|typecheck|lint)|pytest|python3?\s+-m\s+pytest|tsc(\s|$)|eslint(\s|$)|prettier\s+--\s*check|ruff\s+check|mypy(\s|$)|flake8(\s|$)|go\s+test|make\s+test|dotnet\s+test|zig\s+build\s+test|deno\s+test|bun\s+test)"
            )
            .expect("valid regex")
        });
        re.is_match(command)
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

        let mut command = Command::new(resolve_command(cmd));
        command
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        setup_shell_env(&mut command, &self.workspace);
        let mut confined = spawn_confined(&mut command, &self.workspace, &self.extra_rw, false)
            .context("spawn linter")?;
        match wait_for_child(&mut confined.child, 600) {
            Some((status, stdout, stderr)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(signal) = status.signal() {
                        let mut out =
                            format!("--- run_lint ({cmd}) killed by signal {signal} ---\n",);
                        out.push_str(&String::from_utf8_lossy(&stdout));
                        if !stderr.is_empty() {
                            out.push_str(&String::from_utf8_lossy(&stderr));
                        }
                        return Ok(cap_output(out));
                    }
                }
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

/// Shell metacharacters that indicate a command needs a real shell.
///
/// When a command contains none of these and its first token is on the
/// allowlist, we can run it via direct exec (no shell, no injection surface).
fn has_shell_metachars(command: &str) -> bool {
    command.chars().any(|c| {
        matches!(
            c,
            ';' | '&' | '|' | '>' | '<' | '`' | '$' | '(' | ')' | '\n'
        )
    })
}

/// Parse a command into argv via `shlex`. Returns `None` if the command
/// contains shell metacharacters or fails to parse.
fn parse_argv(command: &str) -> Option<Vec<String>> {
    if has_shell_metachars(command) {
        return None;
    }
    shlex::split(command)
}

/// Whether a command can be run via direct exec (no shell).
///
/// The first token must be on the `safe_command_re` allowlist AND the command
/// must contain no shell metacharacters. This flips the model from "denylist
/// dangerous" toward "allowlist safe": known-safe commands run without a
/// shell (no injection surface), everything else falls back to the shell path
/// (still denylist-filtered + confirmation-gated).
pub(crate) fn is_direct_exec_command(command: &str) -> bool {
    if has_shell_metachars(command) {
        return false;
    }
    let Some(argv) = parse_argv(command) else {
        return false;
    };
    let Some(first) = argv.first() else {
        return false;
    };
    safe_command_re().is_match(first)
}

/// Build a platform-aware shell command.
///
/// On Unix: `sh -c <command>`. On Windows: `cmd /C <command>`, falling back
/// to the `COMSPEC` environment variable if set.
fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".into());
        let mut cmd = Command::new(&shell);
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// Resolve a command name to its platform-appropriate executable.
///
/// On Windows, `npm`, `cargo`, `npx`, `python`, and `pytest` are often
/// `.cmd` or `.exe` shims. This function appends `.cmd` when the bare name
/// is not found but `<name>.cmd` exists on `PATH`. On Unix the name is
/// returned unchanged.
fn resolve_command(name: &str) -> String {
    #[cfg(windows)]
    {
        let cmd_name = format!("{}.cmd", name);
        if std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|p| p.join(&cmd_name).exists())
        {
            return cmd_name;
        }
    }
    let _ = name;
    name.to_string()
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
