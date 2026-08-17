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
//!
//! # Layout
//!
//! The file-tool methods ([`fs`]), shell execution ([`shell`]), and
//! verification/lint ([`verify`]) each live in their own submodule; this
//! module holds the shared [`Sandbox`] type, the open-flag/limit constants,
//! and the free helpers they all use.

mod confinement;
mod fs;
mod shell;
mod verify;

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

pub(crate) use confinement::{setup_shell_env, spawn_confined, wait_for_child};
pub(crate) use fs::sandbox_search_code;
#[cfg(test)]
pub(crate) use shell::is_direct_exec_command;

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
    /// is never implied (see `confinement::FsPolicy`). Never granted on
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
