//! OS-level confinement for agent subprocesses.
//!
//! Every tool that execs (`run_shell`, `run_tests`, `run_lint`, git) goes
//! through [`spawn_confined`]. On Linux that applies Landlock + seccomp +
//! rlimits in `pre_exec`. On macOS, rlimits only.
//!
//! # Filesystem policy
//!
//! Write surface is **the workspace plus explicit extra roots**. The process
//! temp dir is never granted: children get `TMPDIR` pinned under
//! `workspace/.raven/tmp`, so cargo/rustc never need `/tmp`. Granting the
//! global temp dir was the `06_sandbox_escape` hole — a confined `echo pwned
//! > /tmp/probe` succeeded for any workspace not nested under `/tmp`
//! (including a normal `~/src/...` checkout).

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Filesystem access granted to a confined child.
///
/// Constructed once per spawn so the Landlock ruleset and the tests share
/// the same policy. Extra RW roots are for git worktrees (the sibling main
/// repo, or the worktree parent while creating/removing it) — never the
/// whole temp dir.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
pub(crate) struct FsPolicy {
    workspace: PathBuf,
    extra_rw: Vec<PathBuf>,
}

#[cfg(target_os = "linux")]
impl FsPolicy {
    pub(crate) fn new(workspace: impl Into<PathBuf>, extra_rw: Vec<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            extra_rw,
        }
    }

    fn canon(p: &Path) -> PathBuf {
        p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
    }

    /// Read-write roots: workspace, explicit extras, `/dev`.
    ///
    /// `/dev` is RW so git can open `/dev/null`. The process temp dir is
    /// intentionally absent — see the module docs.
    pub(crate) fn rw_roots(&self) -> Vec<PathBuf> {
        let ws = Self::canon(&self.workspace);
        let mut roots = vec![ws];
        for root in &self.extra_rw {
            let c = Self::canon(root);
            if !roots.iter().any(|p| p == &c) {
                roots.push(c);
            }
        }
        roots.push(PathBuf::from("/dev"));
        roots
    }

    /// Read-only roots: system prefixes.
    ///
    /// These are granted read+exec so system tooling (`/usr/bin/*`, shared
    /// libs, `/etc`, `/proc`) works. `$HOME` is deliberately NOT here — it is
    /// granted only as a traversal root (Execute) so a confined child can
    /// reach toolchain dirs under `$HOME` without being able to read arbitrary
    /// home files (`~/.ssh`, `~/.env`, `~/.aws`, sibling workspaces, docs).
    pub(crate) fn ro_roots(&self) -> Vec<PathBuf> {
        ["/usr", "/bin", "/lib", "/lib64", "/etc", "/proc"]
            .into_iter()
            .map(PathBuf::from)
            .collect()
    }

    /// Traversal roots: `$HOME` and `~/.cargo` with Execute only.
    ///
    /// Landlock needs Execute on every path component to reach a binary under
    /// `$HOME` (e.g. `~/.cargo/bin/cargo`), but granting read on all of
    /// `$HOME` would let a confined child read secrets (`~/.ssh`, `~/.env`,
    /// `~/.aws`, sibling workspaces, documents). Execute alone permits
    /// traversal into `$HOME` without ReadFile/ReadDir, so the child can reach
    /// the toolchain dirs (granted separately) but cannot list or read the
    /// rest of the home directory.
    ///
    /// `~/.cargo` is also Execute-only (not read): the read+exec grants are
    /// scoped to `~/.cargo/bin` (proxies) and `~/.cargo/registry` (host
    /// registry), so `~/.cargo/credentials` — a direct child of `~/.cargo` —
    /// stays unreadable. Cargo never reads it anyway because `CARGO_HOME` is
    /// pinned to `workspace/.raven/cargo-home`.
    pub(crate) fn traversal_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        if let Ok(home) = std::env::var("HOME").map(PathBuf::from) {
            if let Ok(home_canon) = home.canonicalize() {
                roots.push(home_canon);
            }
            let cargo = home.join(".cargo");
            if let Ok(c) = cargo.canonicalize() {
                if !roots.iter().any(|r| r == &c) {
                    roots.push(c);
                }
            }
        }
        roots
    }

    /// Toolchain roots: read+exec on the dirs where toolchain binaries live.
    ///
    /// These are the `$HOME` subtrees a confined child needs to read+exec to
    /// run cargo/rustc/node/etc:
    /// - every `PATH` directory under `$HOME` (so any tool on PATH can exec —
    ///   `~/.cargo/bin`, `~/.local/bin`, `~/.local/share/mise/shims`,
    ///   `~/.config/nvm/.../bin`, `~/.opencode/bin`, …). PATH dirs are bin
    ///   dirs, not secret dirs (`~/.ssh`, `~/.env`, `~/.aws` are never on
    ///   PATH), so this does not widen the read surface to home secrets.
    /// - `~/.cargo/registry` (host registry, reachable through the pinned
    ///   `CARGO_HOME` symlink)
    /// - `~/.rustup` (real toolchain binaries that rustup proxies exec)
    /// - `~/.local/share/mise/installs` (mise-installed tool binaries)
    /// - `~/.config/mise` (mise's config, which its shims read to resolve
    ///   installed tools)
    /// - `~/.config/nvm` (nvm-installed tool binaries; mise shims resolve
    ///   node/npm into this tree, which needs read to exec the npm-cli.js
    ///   symlink target under `lib/node_modules`)
    ///
    /// Everything else under `$HOME` is traversal-only (see
    /// [`Self::traversal_roots`]).
    pub(crate) fn toolchain_roots(&self) -> Vec<PathBuf> {
        let mut roots = Vec::new();
        let home = std::env::var("HOME").map(PathBuf::from).ok();
        let mut push = |p: PathBuf| {
            if let Ok(c) = p.canonicalize() {
                if !roots.iter().any(|r| r == &c) {
                    roots.push(c);
                }
            }
        };
        if let Some(home) = &home {
            // PATH dirs under $HOME (bin dirs only — never secret dirs).
            if let Some(path) = std::env::var_os("PATH") {
                for dir in std::env::split_paths(&path) {
                    if dir.starts_with(home) {
                        push(dir);
                    }
                }
            }
            // Real toolchain dirs that proxies/shims resolve into.
            for sub in [
                ".cargo/registry",
                ".rustup",
                ".local/share/mise/installs",
                ".config/mise",
                ".config/nvm",
            ] {
                push(home.join(sub));
            }
        }
        roots
    }

    /// Whether `path` is under any RW root. Used by tests; Landlock is the
    /// real enforcer at runtime.
    #[cfg(test)]
    pub(crate) fn allows_write(&self, path: &Path) -> bool {
        let target = Self::canon(path);
        self.rw_roots().iter().any(|root| {
            let r = Self::canon(root);
            target == r || target.starts_with(&r)
        })
    }
}

/// RLIMIT_FSIZE applied to confined children, in bytes (248 MiB).
///
/// Bounds a runaway write while staying above real toolchain outputs: a
/// debug test binary with full debuginfo can exceed 60 MiB (280 MiB for
/// raven's own), and release/linker temporaries land in the same range.
/// Sanctioned verification commands skip rlimits entirely (see
/// [`apply_rlimits`]).
pub(crate) const RLIMIT_FSIZE_BYTES: u64 = 248 << 20;

/// Apply resource limits (RLIMIT_*) to the calling process.
///
/// Linux + macOS. Caps oversized writes (RLIMIT_FSIZE), runaway CPU
/// (RLIMIT_CPU), and fd exhaustion (RLIMIT_NOFILE). Best-effort: failures are
/// ignored so a kernel that doesn't support a limit doesn't break the child.
///
/// `skip_rlimits` is set for sanctioned verification commands (`run_tests`,
/// `run_lint`, and `run_shell` test/lint/format invocations). Those commands
/// legitimately need to write large linker outputs (a debug test binary can
/// exceed the RLIMIT_FSIZE cap, which would SIGXFSZ-kill it) and to burn more
/// than 30s of CPU on a clean build. The exemption mirrors the seccomp
/// network-block exemption already granted to the same sanctioned commands.
///
/// Deliberately omitted:
/// - `RLIMIT_AS` (virtual address space): V8/Node reserve large regions up
///   front, so a cap aborts them at startup. It bounds virtual, not resident,
///   memory — the wrong tool.
/// - `RLIMIT_NPROC` (processes/threads per user): it is a *user-global*
///   ceiling, not a per-child one. Imposing it on a child can't isolate that
///   child; a fork bomb would instead kill the whole user session, and on a
///   busy machine it silently breaks any high-thread runtime (Node, etc.)
///   because the ambient thread count is already near the cap.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn apply_rlimits(skip_rlimits: bool) {
    if skip_rlimits {
        tracing::info!("rlimits skipped for sanctioned verification command");
        return;
    }
    use rustix::process::{setrlimit, Resource, Rlimit};
    let limits = [
        (
            Resource::Cpu,
            Rlimit {
                current: Some(30),
                maximum: Some(30),
            },
        ),
        (
            Resource::Fsize,
            Rlimit {
                current: Some(RLIMIT_FSIZE_BYTES),
                maximum: Some(RLIMIT_FSIZE_BYTES),
            },
        ),
        (
            Resource::Nofile,
            Rlimit {
                current: Some(1024),
                maximum: Some(1024),
            },
        ),
    ];
    for (res, lim) in limits {
        let _ = setrlimit(res, lim);
    }
}

/// Apply Landlock filesystem confinement to the calling process.
///
/// Linux-only. Grants the [`FsPolicy`] roots and denies everything else.
/// Best-effort: if the kernel doesn't support Landlock, we log and continue.
#[cfg(target_os = "linux")]
fn apply_landlock(workspace: &Path, extra_rw: &[PathBuf]) {
    if std::env::var("RAVEN_SANDBOX_LANDLOCK").as_deref() == Ok("0") {
        tracing::info!("landlock disabled via RAVEN_SANDBOX_LANDLOCK=0");
        return;
    }
    use landlock::{
        path_beneath_rules, Access, AccessFs, CompatLevel, Compatible, Ruleset, RulesetAttr,
        RulesetCreatedAttr, ABI,
    };
    // ABI V2+ is required so `AccessFs::from_all` includes `REFER`. Without
    // REFER, `rename`/`link` across directories (even under the same
    // path_beneath rule) fails with EXDEV — which is exactly how rustc
    // stages `.rmeta` from `target/.../incremental` into `target/.../deps`.
    let abi = ABI::V3;
    let access_all = AccessFs::from_all(abi);
    let access_read = AccessFs::from_read(abi);

    let policy = FsPolicy::new(workspace, extra_rw.to_vec());
    let rw_paths = policy.rw_roots();
    let ro_paths = policy.ro_roots();
    let traversal_paths = policy.traversal_roots();
    let toolchain_paths = policy.toolchain_roots();

    let result = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_all)
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&rw_paths, access_all)))
        .and_then(|r| r.add_rules(path_beneath_rules(&ro_paths, access_read)))
        // `$HOME` is Execute-only (traversal) so a confined child can reach
        // the toolchain dirs without being able to read arbitrary home files.
        .and_then(|r| r.add_rules(path_beneath_rules(&traversal_paths, AccessFs::Execute)))
        // The toolchain dirs under `$HOME` get read+exec so cargo/rustc/node
        // can run. Everything else under `$HOME` stays traversal-only.
        .and_then(|r| r.add_rules(path_beneath_rules(&toolchain_paths, access_read)))
        .and_then(|r| r.restrict_self());

    match result {
        Ok(status) => {
            if status.ruleset == landlock::RulesetStatus::NotEnforced {
                tracing::warn!(
                    "Landlock not enforced (kernel too old?); filesystem confinement is best-effort"
                );
            }
        }
        Err(e) => {
            tracing::warn!("Landlock failed to apply: {e}; filesystem confinement is best-effort");
        }
    }
}

/// Apply a seccomp filter that blocks network exfiltration.
///
/// Linux-only. Blocks `socket()` only when the domain is AF_INET or AF_INET6.
/// AF_UNIX sockets (esbuild, git ssh helpers) and `socketpair()` stay allowed.
/// Denied syscalls are killed immediately (`KillProcess`).
///
/// The `skip_network_block` flag is for sanctioned test runners (vitest/v8
/// opens an AF_INET socket for coverage IPC). It is captured in the
/// `pre_exec` closure — it cannot be read from `std::env::var` after fork,
/// because `Command::env` overrides are only applied at execve.
#[cfg(target_os = "linux")]
fn apply_seccomp_network_block(skip_network_block: bool) {
    use seccompiler::{
        BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter,
        SeccompRule,
    };
    use std::convert::TryInto;

    if skip_network_block {
        tracing::info!("seccomp: network block skipped for sanctioned test runner");
        return;
    }

    if std::env::var("RAVEN_SANDBOX_NETWORK_BLOCK").as_deref() == Ok("0") {
        tracing::info!("seccomp: network block disabled via RAVEN_SANDBOX_NETWORK_BLOCK=0");
        return;
    }

    let target_arch = match std::env::consts::ARCH.try_into() {
        Ok(arch) => arch,
        Err(_) => {
            tracing::warn!("seccomp: unsupported arch, network block skipped");
            return;
        }
    };

    let inet = match SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::AF_INET as u64,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("seccomp: invalid AF_INET condition: {e}");
            return;
        }
    };
    let inet6 = match SeccompCondition::new(
        0,
        SeccompCmpArgLen::Dword,
        SeccompCmpOp::Eq,
        libc::AF_INET6 as u64,
    ) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("seccomp: invalid AF_INET6 condition: {e}");
            return;
        }
    };
    let rule_inet = match SeccompRule::new(vec![inet]) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seccomp: invalid AF_INET rule: {e}");
            return;
        }
    };
    let rule_inet6 = match SeccompRule::new(vec![inet6]) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("seccomp: invalid AF_INET6 rule: {e}");
            return;
        }
    };
    let rules: Vec<(i64, Vec<SeccompRule>)> = vec![(libc::SYS_socket, vec![rule_inet, rule_inet6])];

    let filter: BpfProgram = match SeccompFilter::new(
        rules.into_iter().collect(),
        SeccompAction::Allow,
        SeccompAction::KillProcess,
        target_arch,
    ) {
        Ok(f) => match f.try_into() {
            Ok(bpf) => bpf,
            Err(e) => {
                tracing::warn!("seccomp: failed to compile filter: {e}");
                return;
            }
        },
        Err(e) => {
            tracing::warn!("seccomp: failed to build filter: {e}");
            return;
        }
    };

    if let Err(e) = seccompiler::apply_filter(&filter) {
        tracing::warn!("seccomp: failed to apply filter: {e}");
    }
}

/// Apply all OS-level confinement to the calling process (the child).
///
/// Called from `pre_exec` before `exec` (Unix only). Best-effort: each layer
/// logs and continues on failure so a kernel that doesn't support a feature
/// doesn't break the child.
#[cfg(unix)]
fn apply_os_confinement(
    workspace: &Path,
    extra_rw: &[PathBuf],
    skip_network_block: bool,
    skip_rlimits: bool,
) {
    unsafe { libc::setpgid(0, 0) };
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    apply_rlimits(skip_rlimits);
    #[cfg(target_os = "linux")]
    apply_landlock(workspace, extra_rw);
    #[cfg(target_os = "linux")]
    apply_seccomp_network_block(skip_network_block);
}

/// Set up a clean-but-usable environment for a shell subprocess.
///
/// Clears the environment and passes through `PATH`, `HOME`, `PWD`, and
/// `LANG`.
///
/// Also pins build-tool caches (`CARGO_HOME`, `TMPDIR`, npm cache, …) under
/// `raven_dir` — see [`pin_build_tool_dirs`]. System scope passes
/// `~/.raven` there; repo scope the workspace's own `.raven`.
pub(crate) fn setup_shell_env(cmd: &mut Command, workspace: &Path, raven_dir: &Path) {
    cmd.env_clear();
    cmd.env("PWD", workspace);
    for key in &["PATH", "HOME", "LANG", "TERM", "USER", "LOGNAME"] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    for key in &["RUSTUP_HOME", "RUSTUP_TOOLCHAIN"] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    // Pin git's global config to a raven-local file so `git config`
    // (without `--local`) doesn't try to write `~/.gitconfig`, which the
    // narrowed Landlock grant no longer makes writable. Mirrors the
    // `GIT_CONFIG_NOSYSTEM=1` isolation in the git tools.
    let git_config = raven_dir.join("gitconfig");
    cmd.env("GIT_CONFIG_GLOBAL", &git_config);
    pin_build_tool_dirs(cmd, workspace, raven_dir);
}

/// Pin package-manager caches and temp dirs under `raven_dir` (repo scope:
/// `{workspace}/.raven`; system scope: `~/.raven`).
///
/// Landlock no longer grants the process temp dir, so rustc/cargo/npm must
/// not write there. Keeping `CARGO_HOME`, `CARGO_TARGET_DIR`, `TMPDIR`, and
/// the npm cache under the Landlock-covered rule also avoids `EXDEV` hardlink
/// failures across Landlock hierarchies.
pub(crate) fn pin_build_tool_dirs(cmd: &mut Command, workspace: &Path, raven_dir: &Path) {
    let cargo_home = raven_dir.join("cargo-home");
    let tmp_dir = raven_dir.join("tmp");
    let npm_cache = raven_dir.join("npm-cache");
    let target_dir = workspace.join("target");
    for dir in [&cargo_home, &tmp_dir, &npm_cache, &target_dir] {
        let _ = std::fs::create_dir_all(dir);
    }
    seed_cargo_home(&cargo_home);

    cmd.env("CARGO_HOME", &cargo_home);
    cmd.env("CARGO_TARGET_DIR", &target_dir);
    cmd.env("npm_config_cache", &npm_cache);
    // Pin mise's config discovery + cache/state dirs to the workspace so its
    // shims (node/npm/go/...) don't walk up the directory tree reading
    // `.mise.toml` files under `$HOME` (e.g. `~/Work/.mise.toml`) or write to
    // `~/.cache/mise` / `~/.local/state/mise` — none of which the narrowed
    // Landlock grant makes readable/writable. `MISE_CEILING_PATHS` stops the
    // config walk-up at the workspace; mise still reads
    // `~/.config/mise/config.toml` (granted read via the toolchain roots).
    // Mirrors the CARGO_HOME/TMPDIR pinning above.
    cmd.env("MISE_CEILING_PATHS", workspace);
    let mise_cache = raven_dir.join("mise-cache");
    let mise_state = raven_dir.join("mise-state");
    for dir in [&mise_cache, &mise_state] {
        let _ = std::fs::create_dir_all(dir);
    }
    cmd.env("MISE_CACHE_DIR", &mise_cache);
    cmd.env("MISE_STATE_DIR", &mise_state);
    // Pin the temp dir so rustc/cargo/npm write under the workspace rule.
    cmd.env("TMPDIR", &tmp_dir);
    cmd.env("TEMP", &tmp_dir);
    cmd.env("TMP", &tmp_dir);
    cmd.env_remove("RUSTC_WRAPPER");
    cmd.env_remove("CARGO_INCREMENTAL");
}

/// Make the host cargo registry readable from the pinned `CARGO_HOME` via
/// symlink, so sandboxed cargo can resolve dependencies without re-downloading
/// the index/cache (the pinned home starts empty and most sandboxed commands
/// cannot reach the network).
///
/// `registry/index` and `registry/cache` are symlinked to the host's copies
/// (read access is granted through the read-only HOME root). `registry/src` —
/// where cargo *extracts* sources — stays a real directory inside the pinned
/// home so builds write only inside the Landlock write roots. Idempotent:
/// existing entries are left alone.
fn seed_cargo_home(cargo_home: &Path) {
    let Some(host_home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
    else {
        return;
    };
    if host_home == *cargo_home {
        return;
    }
    for sub in ["registry/index", "registry/cache"] {
        let dst = cargo_home.join(sub);
        let src = host_home.join(sub);
        if dst.exists() || dst.is_symlink() || !src.exists() {
            continue;
        }
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::os::unix::fs::symlink(&src, &dst);
    }
}

/// A running subprocess plus its OS-level confinement guard.
pub(crate) struct ConfinedChild {
    pub(crate) child: std::process::Child,
}

/// Spawn a configured `Command` under OS-level confinement.
///
/// Shared by `run_shell`, `run_tests`, `run_lint`, and the git tools.
/// The `Command` must already have its env, cwd, and stdio configured.
pub(crate) fn spawn_confined(
    cmd: &mut Command,
    workspace: &Path,
    extra_rw: &[PathBuf],
    skip_network_block: bool,
    skip_rlimits: bool,
) -> Result<ConfinedChild> {
    let ws = workspace.to_path_buf();
    let extra = extra_rw.to_vec();
    unsafe {
        cmd.pre_exec(move || {
            apply_os_confinement(&ws, &extra, skip_network_block, skip_rlimits);
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn command")?;
    Ok(ConfinedChild { child })
}

/// Run a configured `Command` under OS-level confinement and wait for it.
pub(crate) fn run_confined(
    cmd: &mut Command,
    workspace: &Path,
    timeout_secs: u64,
    extra_rw: &[PathBuf],
    skip_network_block: bool,
    skip_rlimits: bool,
) -> Result<String> {
    let mut confined = spawn_confined(cmd, workspace, extra_rw, skip_network_block, skip_rlimits)?;
    match wait_for_child(&mut confined.child, timeout_secs) {
        Some((status, stdout, stderr)) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = status.signal() {
                    let mut out = format!("Error: command killed by signal {signal}\n");
                    // The seccomp network block kills the first outbound
                    // socket (AF_INET/6) with SIGSYS. Without this note the
                    // model sees an opaque exit 159 / signal 31 and burns
                    // iterations re-diagnosing a deterministic policy kill
                    // (proxy env vars, IPv4 forcing, curl replacing pnpm…).
                    if signal == libc::SIGSYS {
                        out.push_str(
                            "This sandbox blocks network access (seccomp): the first \
                             outbound TCP connection is killed with SIGSYS (shell code \
                             159). The command will keep failing this way — do not \
                             retry or re-diagnose. Work offline, or ask the user to \
                             run network-dependent steps (package installs, downloads) \
                             themselves.\n",
                        );
                    }
                    out.push_str(&String::from_utf8_lossy(&stdout));
                    out.push_str(&String::from_utf8_lossy(&stderr));
                    return Ok(super::cap_output(out));
                }
            }
            let mut out = format!("exit={}\n", status.code().unwrap_or(-1));
            // Relayed SIGSYS: when the seccomp kill hits a grandchild (e.g.
            // python3 under `sh -c`), the shell survives and exits 159 with
            // "Bad system call" instead of the child being signal-reported.
            // Give the model the same explanation as the direct kill, or it
            // re-diagnoses the exit code as an environment bug.
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            );
            if status.code() == Some(159)
                && (combined.contains("Bad system call") || combined.contains("bad system call"))
            {
                out.push_str(
                    "This sandbox blocks network access (seccomp): the first \
                     outbound TCP connection is killed with SIGSYS (shell code \
                     159). The command will keep failing this way — do not \
                     retry or re-diagnose. Work offline, or ask the user to \
                     run network-dependent steps (package installs, downloads) \
                     themselves.\n",
                );
            }
            out.push_str(&combined);
            Ok(super::cap_output(out))
        }
        None => Ok("Error: command timed out".into()),
    }
}

/// Run a spawned child to completion with a timeout, draining stdout/stderr
/// on background threads so a chatty child can't deadlock the pipe buffers.
///
/// Returns `Some((exit_status, stdout, stderr))` on completion, or `None` if
/// the child did not finish within `timeout_secs` (the child is killed).
///
/// After the direct child exits, the child's entire process group is killed
/// to close inherited pipe fds held by grandchildren.
pub(crate) fn wait_for_child(
    child: &mut std::process::Child,
    timeout_secs: u64,
) -> Option<(std::process::ExitStatus, Vec<u8>, Vec<u8>)> {
    let (stdout_tx, stdout_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let (stderr_tx, stderr_rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let stdout_handle = child.stdout.take().map(|mut out| {
        let tx = stdout_tx;
        std::thread::spawn(move || {
            let buf = read_pipe_nonblocking(&mut out);
            let _ = tx.send(buf);
        })
    });
    let stderr_handle = child.stderr.take().map(|mut err| {
        let tx = stderr_tx;
        std::thread::spawn(move || {
            let buf = read_pipe_nonblocking(&mut err);
            let _ = tx.send(buf);
        })
    });

    let start = std::time::Instant::now();
    let deadline = std::time::Duration::from_secs(timeout_secs);
    let pid = child.id();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    kill_process_group(pid);
                    let _ = drain_pipes(stdout_rx, stderr_rx);
                    drop(stdout_handle);
                    drop(stderr_handle);
                    return None;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }
            Err(_) => {
                kill_process_group(pid);
                let _ = drain_pipes(stdout_rx, stderr_rx);
                drop(stdout_handle);
                drop(stderr_handle);
                return None;
            }
        }
    };

    kill_process_group(pid);

    let (stdout, stderr) = drain_pipes(stdout_rx, stderr_rx);
    drop(stdout_handle);
    drop(stderr_handle);
    Some((status, stdout, stderr))
}

#[cfg(unix)]
fn read_pipe_nonblocking(
    reader: &mut (impl std::io::Read + std::os::unix::io::AsRawFd),
) -> Vec<u8> {
    let fd = reader.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL, 0);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(_) => break,
        }
    }
    buf
}

#[cfg(not(unix))]
fn read_pipe_nonblocking(reader: &mut impl std::io::Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = std::io::Read::read_to_end(reader, &mut buf);
    buf
}

fn drain_pipes(
    stdout_rx: std::sync::mpsc::Receiver<Vec<u8>>,
    stderr_rx: std::sync::mpsc::Receiver<Vec<u8>>,
) -> (Vec<u8>, Vec<u8>) {
    let drain_timeout = std::time::Duration::from_secs(2);
    let start = std::time::Instant::now();
    let stdout = stdout_rx.recv_timeout(drain_timeout).unwrap_or_default();
    let remaining = drain_timeout.saturating_sub(start.elapsed());
    let stderr = stderr_rx.recv_timeout(remaining).unwrap_or_default();
    (stdout, stderr)
}

fn kill_process_group(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn policy_never_grants_process_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(&ws).unwrap();
        let policy = FsPolicy::new(&ws, Vec::new());
        let tmp_canon = std::env::temp_dir().canonicalize().unwrap();
        assert!(
            !policy.allows_write(&tmp_canon),
            "process temp dir must not be an RW root"
        );
        let probe = tmp_canon.join("raven_eval_escape_probe.txt");
        assert!(
            !policy.allows_write(&probe),
            "sibling under /tmp must not be writable: {}",
            probe.display()
        );
    }

    #[test]
    fn policy_grants_workspace_and_extra_rw() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        let extra = tmp.path().join("extra");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&extra).unwrap();
        let policy = FsPolicy::new(&ws, vec![extra.clone()]);
        assert!(policy.allows_write(&ws.join("src/lib.rs")));
        assert!(policy.allows_write(&extra.join("file")));
    }

    #[test]
    fn policy_does_not_grant_tmp_for_home_like_workspace() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/sandbox-policy-home");
        std::fs::create_dir_all(&root).unwrap();
        let ws = root.canonicalize().unwrap();
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        assert!(
            !ws.starts_with(&tmp),
            "test setup requires workspace outside process temp dir"
        );
        let policy = FsPolicy::new(&ws, Vec::new());
        assert!(policy.allows_write(&ws));
        assert!(!policy.allows_write(&tmp));
        assert!(!policy.allows_write(&tmp.join("raven_eval_escape_probe.txt")));
    }
}

#[cfg(unix)]
#[test]
fn wait_for_child_grandchild_pipe_does_not_hang() {
    use std::process::Command;
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg("(sleep 60 &); echo passed")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    let mut child = cmd.spawn().unwrap();
    let start = std::time::Instant::now();
    let result = wait_for_child(&mut child, 5)
        .expect("child with grandchild holding pipe should return promptly");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(4),
        "should not hang: took {:?}",
        start.elapsed()
    );
    assert_eq!(result.0.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&result.1).contains("passed"),
        "should drain stdout"
    );
}
