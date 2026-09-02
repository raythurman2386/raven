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
            r"(?i)(rm\s+(-[a-z]*f[a-z]*\s+)?/|mkfs|: \(\)\s*\{\s*:\|:&\s*\};:|dd\s+if=/dev/(zero|random|urandom)|chmod\s+(-R\s+)?777\s+/|curl\s+.*\|\s*(ba)?sh|wget\s+.*\|\s*(ba)?sh|format\s+[A-Za-z]:|del\s+/[sfq]\s+[A-Za-z]:\\|rd\s+/[sq]\s+[A-Za-z]:\\|rmdir\s+/[sq]\s+[A-Za-z]:\\|powershell\s+-[Cc]ommand\s+.*Remove-Item.*-Recurse.*-Force|Remove-Item\s+-Recurse\s+-Force\s+[A-Za-z]:\\|diskpart|/dev/tcp|bash\s+-i|nc(at)?\s+[^\n]*-e|mkfifo|powershell[^\n]*-[Ee]nc(odedcommand)?|certutil[^\n]*-decode|Invoke-Expression|\biex\s*\(|base64\s+[^\n]*\|\s*(ba)?sh|curl\s+[^\n]*\|\s*(pwsh|powershell|cmd)|wget\s+[^\n]*\|\s*(pwsh|powershell|cmd))",
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
            r"(?i)^\s*(cargo|rustc|rustup|rustfmt|go|node|npm|npx|yarn|pnpm|bun|deno|uv|python|python3|pip|pip3|poetry|pytest|ruff|mypy|black|isort|flake8|eslint|prettier|tsc|jest|vitest|mocha|make|cmake|ninja|meson|just|gcc|g\+\+|clang|clang\+\+|ld|lld|ar|strip|objcopy|objdump|nm|readelf|size|strings|file|where|which|type|command|hash|set|env|printenv|pwd|cd|ls|dir|cat|head|tail|wc|sort|uniq|cut|tr|sed|awk|grep|rg|fd|find|findstr|xargs|tee|diff|cmp|comp|fc|comm|patch|tar|gzip|gunzip|bzip2|bunzip2|xz|unxz|zip|unzip|git|hg|svn|fossil|pijul|jj|echo|printf|true|false|test|\[|expr|sleep|date|stat|du|df|basename|dirname|realpath|readlink|mkdir|touch|copy|cp|move|mv|ren|rename|chmod|chown|icacls|attrib|id|whoami|uname|hostname|uptime|ps|tasklist|time|timeout|nice|renice|nohup|exec|source|\.|call|cmd|jq|yq|bat|delta|sccache|wasm-pack|hyperfine|tokei|buf|protoc)(\s|$)",
        )
        .expect("valid regex")
    })
}

/// System-scope allowlist: commands the system agent may run without a
/// confirmation prompt. Tiered on top of [`safe_command_re`] (which still
/// applies — dev commands stay safe in system scope too).
///
/// Autonomous in system scope:
/// - Read-only diagnostics: `systemctl`/`journalctl`/`coredumpctl`/`loginctl`
///   status+list+show, `pacman`/`pacman-conf` query+search+info,
///   `systemd-analyze` read-only subcommands, `ip`, `ss`, `df`, `free`,
///   `lscpu`, `lsblk`, `lspci`, `lsusb`, `uptime`, `vmstat`, `top`-style
///   readers, `bluetoothctl`/`nmcli`/`resolvectl` info, `hyprctl` reads,
///   `gum`-free text tools, `omarchy-*` informational helpers.
/// - The `omarchy <group> <action>` CLI where the action is informational
///   (`omarchy debug`, `omarchy commands`, `omarchy version`, `omarchy
///   <group> --help`, `omarchy bar list`, `omarchy plugin list`, …).
///
/// Still confirmed (never autonomous): package install/remove/upgrade,
/// `systemctl start/stop/restart/enable/disable/mask`, `omarchy install/refresh/
/// restart/theme set/pkg/remove`, anything matching `safe_command_re`'s
/// destructive denylist (`dangerous_re`), `sudo`/`su`, power operations
/// (`reboot`/`shutdown`/`poweroff`), process kills, and raw config writes
/// outside the sandbox's file tools.
///
/// The split mirrors the omarchy skill's own guidance: inspect first,
/// propose, confirm, then apply.
pub fn system_safe_command_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Verbose mode (?x): whitespace/comments in the pattern are ignored.
        // Read-only only: the `regex` crate has no lookahead, so pacman's
        // sync flags are enumerated explicitly (query/search/info only —
        // install (-S pkg), refresh (-Sy), upgrade (-Su) are NOT listed).
        Regex::new(
            r"(?xi)^\s*(?:
            pacman\s+(?:-[Qq](?:[SiIlodtu]*)|-Q[a-z]*|-Ss|-Si|-Sg|-Sl|-Sg|-Sp|-Sn|-F[a-z]*|--query|--sync|--search|--info)\b|
            pacman-conf|
            # systemd read-only
            systemctl\s+(?:status|list-|show|is-active|is-enabled|is-failed|cat|get-default|list-dependencies)|
            journalctl|
            coredumpctl\s+(?:list|info|dump)|
            loginctl\s+(?:list|show|status)|
            systemd-analyze\s+(?:blame|critical-chain|security|time|dump)|
            busctl\s+(?:list|status|tree)|
            # desktop / hardware state readers
            hyprctl\s+(?:monitors|workspaces|clients|activewindow|activeworkspace|version|getoption|decos|devices|binds|layers|splash|cursorinfo)|
            nmcli\s+(?:device|connection|general|networking)\s+(?:status|show|list)?|
            resolvectl\s+(?:status|query)|
            bluetoothctl\s+(?:info|devices|list)|
            upower\s+-[id]|
            # resource / hardware readers
            free|vmstat|iostat|mpstat|pidstat|sensors|lscpu|lsblk|lspci|lsusb|lsmem|lsscsi|
            # omarchy informational
            omarchy\s+(?:--help|-h|version|commands|debug)|
            omarchy\s+\S+\s+--help|
            omarchy\s+(?:theme\s+list|plugin\s+list|bar\s+list|menu\s+show)
            )",
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
    /// The operational scope. System scope (workspace `/`) redirects
    /// `.raven` scratch/state dirs to `~/.raven` via [`Sandbox::raven_dir`].
    pub scope: crate::config::Scope,
}

impl Sandbox {
    /// Create a sandbox rooted at `workspace` (repo scope).
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            extra_rw: Vec::new(),
            scope: crate::config::Scope::Repo,
        }
    }

    /// Create a sandbox rooted at `workspace` with extra Landlock RW roots
    /// granted to every confined child. Used for git worktree sub-agents that
    /// must reach the shared main repo (a sibling under the temp dir).
    pub fn with_extra_rw(workspace: PathBuf, extra_rw: Vec<PathBuf>) -> Self {
        Self {
            workspace,
            extra_rw,
            scope: crate::config::Scope::Repo,
        }
    }

    /// The `.raven` directory for scratch/state (pinned caches, temp dir,
    /// patch staging). Repo scope keeps it in the workspace; system scope
    /// moves it under `$HOME/.raven` because the workspace is `/` and
    /// `/.raven` is neither writable nor meaningful for a per-user agent.
    pub fn raven_dir(&self) -> PathBuf {
        if self.scope.is_system() {
            match std::env::var_os("HOME") {
                Some(home) => PathBuf::from(home).join(".raven"),
                None => self.workspace.join(".raven"),
            }
        } else {
            self.workspace.join(".raven")
        }
    }

    /// Create a sandbox rooted at `workspace` with an explicit scope.
    pub fn with_scope(workspace: PathBuf, scope: crate::config::Scope) -> Self {
        Self {
            workspace,
            extra_rw: Vec::new(),
            scope,
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
