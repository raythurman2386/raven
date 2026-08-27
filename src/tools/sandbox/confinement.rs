//! OS-level confinement for agent subprocesses.
//!
//! Every tool that execs (`run_shell`, `run_tests`, `run_lint`, git) goes
//! through [`spawn_confined`]. On Linux that applies Landlock + seccomp +
//! rlimits in `pre_exec`. On macOS, rlimits only. On Windows, a Job Object.
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

    /// Read-only roots: system prefixes plus `$HOME`.
    ///
    /// HOME is always RO (never RW). Landlock needs Execute on every path
    /// component to exec a toolchain binary under `~/.local` / `~/.rustup`,
    /// so granting only leaf dirs left intermediates ungranted (EACCES).
    /// A workspace under HOME still gets its own RW rule; Landlock ORs
    /// matching rules, so writes inside the workspace stay allowed.
    pub(crate) fn ro_roots(&self) -> Vec<PathBuf> {
        let mut roots: Vec<PathBuf> = ["/usr", "/bin", "/lib", "/lib64", "/etc", "/proc"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        if let Ok(home) = std::env::var("HOME").map(PathBuf::from) {
            if let Ok(home_canon) = home.canonicalize() {
                if !roots.iter().any(|p| p == &home_canon) {
                    roots.push(home_canon);
                }
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

/// Apply resource limits (RLIMIT_*) to the calling process.
///
/// Linux + macOS. Caps oversized writes (RLIMIT_FSIZE), runaway CPU
/// (RLIMIT_CPU), and fd exhaustion (RLIMIT_NOFILE). Best-effort: failures are
/// ignored so a kernel that doesn't support a limit doesn't break the child.
///
/// `skip_rlimits` is set for sanctioned verification commands (`run_tests`,
/// `run_lint`, and `run_shell` test/lint/format invocations). Those commands
/// legitimately need to write large linker outputs (a debug test binary can
/// exceed 64 MiB, which `RLIMIT_FSIZE` would SIGXFSZ-kill) and to burn more
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
                current: Some(64 << 20),
                maximum: Some(64 << 20),
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

    let result = Ruleset::default()
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(access_all)
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&rw_paths, access_all)))
        .and_then(|r| r.add_rules(path_beneath_rules(&ro_paths, access_read)))
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

    let rules: Vec<(i64, Vec<SeccompRule>)> = vec![(
        libc::SYS_socket,
        vec![
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_INET as u64,
            )
            .expect("valid seccomp condition")])
            .expect("valid seccomp rule"),
            SeccompRule::new(vec![SeccompCondition::new(
                0,
                SeccompCmpArgLen::Dword,
                SeccompCmpOp::Eq,
                libc::AF_INET6 as u64,
            )
            .expect("valid seccomp condition")])
            .expect("valid seccomp rule"),
        ],
    )];

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
/// On Unix: clears the environment and passes through `PATH`, `HOME`, `PWD`,
/// and `LANG`. On Windows: clears the environment and passes through the
/// vars `cmd.exe` and common tools need to start.
///
/// Also pins build-tool caches (`CARGO_HOME`, `TMPDIR`, npm cache, …) under
/// the workspace — see [`pin_build_tool_dirs`].
pub(crate) fn setup_shell_env(cmd: &mut Command, workspace: &Path) {
    cmd.env_clear();
    #[cfg(windows)]
    {
        for key in &[
            "SystemRoot",
            "PATH",
            "USERPROFILE",
            "HOMEDRIVE",
            "HOMEPATH",
            "TEMP",
            "TMP",
            "COMSPEC",
            "PATHEXT",
        ] {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
    }
    #[cfg(not(windows))]
    {
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
    }
    pin_build_tool_dirs(cmd, workspace);
}

/// Pin package-manager caches and temp dirs inside the workspace.
///
/// Landlock no longer grants the process temp dir, so rustc/cargo/npm must
/// not write there. Keeping `CARGO_HOME`, `CARGO_TARGET_DIR`, `TMPDIR`, and
/// the npm cache under the workspace rule also avoids `EXDEV` hardlink
/// failures across Landlock hierarchies.
pub(crate) fn pin_build_tool_dirs(cmd: &mut Command, workspace: &Path) {
    let raven_dir = workspace.join(".raven");
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
    // Pin the temp dir only on Unix. Windows has no Landlock, and overriding
    // TEMP/TMP there breaks MSVC link.exe (UTF-16LE response-file parse).
    #[cfg(not(windows))]
    {
        cmd.env("TMPDIR", &tmp_dir);
        cmd.env("TEMP", &tmp_dir);
        cmd.env("TMP", &tmp_dir);
    }
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
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink(&src, &dst);
        }
        #[cfg(windows)]
        {
            let _ = create_dir_junction(&src, &dst);
        }
        #[cfg(not(unix))]
        {
            let _ = std::fs::copy(&src, &dst);
        }
    }
}

/// Create an NTFS directory junction (`dst` → `src`) on Windows.
///
/// Junctions need no elevated privileges unlike directory symlinks, and
/// cargo treats them like real directories for its registry lookups.
#[cfg(windows)]
fn create_dir_junction(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    // `mount point` tag: target is a volume-absolute path, no name surrogate
    // resolution against reparse points needed for our use.
    const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;

    // The junction target must be absolute with \\?\ prefix semantics; the
    // canonical absolute path works for both drive-letter and UNC sources.
    let target = src.canonicalize().unwrap_or_else(|_| src.to_path_buf());
    let mut target_w: Vec<u16> = target.as_os_str().encode_wide().collect();

    // A junction's substitute name must not end with a backslash.
    while target_w.last() == Some(&0x5C_u16) {
        target_w.pop();
    }

    // Reparse data buffer layout (WIN32_REPARSE_DATA_BUFFER for mount points):
    // tag(4) + data_len(2) + reserved(2) + substitute_offset(2) +
    // substitute_len(2) + print_offset(2) + print_len(2), then the strings.
    let header_len = 4 + 2 + 2 + 2 + 2 + 2 + 2;
    let substitute_bytes: Vec<u8> = target_w.iter().flat_map(|w| w.to_le_bytes()).collect();
    let mut buf = vec![0u8; header_len + substitute_bytes.len() + 2];
    buf[0..4].copy_from_slice(&IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    let data_len = (substitute_bytes.len() + 4 + 2) as u16;
    buf[4..6].copy_from_slice(&data_len.to_le_bytes());
    let substitute_offset = header_len as u16;
    buf[8..10].copy_from_slice(&substitute_offset.to_le_bytes());
    let substitute_len = substitute_bytes.len() as u16;
    buf[10..12].copy_from_slice(&substitute_len.to_le_bytes());
    // Print name: same as substitute (harmless; explorer shows it as a link).
    buf[12..14].copy_from_slice(&substitute_offset.to_le_bytes());
    buf[14..16].copy_from_slice(&substitute_len.to_le_bytes());
    buf[header_len..header_len + substitute_bytes.len()].copy_from_slice(&substitute_bytes);

    std::fs::create_dir(dst)?;
    let dst_w: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();

    // DEVICE_TYPE for FSCTL_SET_REPARSE_POINT; the constant lives behind a
    // feature flag we don't enable, so spell the value (0x9000048).
    const FSCTL_SET_REPARSE_POINT: u32 = 0x900A8;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const OPEN_EXISTING: u32 = 3;
    const INVALID_HANDLE_VALUE: isize = -1;

    let handle = unsafe {
        windows_sys::Win32::Storage::FileSystem::CreateFileW(
            dst_w.as_ptr(),
            (0x4000_0000u32 | 0x2000_0000u32 | 0x0080_0000u32) as u32, // GENERIC_WRITE | GENERIC_READ | WRITE_DAC
            0,
            std::ptr::null_mut(),
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if handle as isize == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut bytes_returned: u32 = 0;
    let set_ok = unsafe {
        windows_sys::Win32::System::IO::DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buf.as_ptr() as *const _,
            buf.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned,
            std::ptr::null_mut(),
        )
    };
    unsafe {
        windows_sys::Win32::Foundation::CloseHandle(handle);
    }
    if set_ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// A running subprocess plus its OS-level confinement guard.
///
/// The guard's scope is the whole subprocess lifetime: on Windows it owns the
/// Job Object handle, which must stay open until the child is reaped (with
/// `KILL_ON_JOB_CLOSE`, dropping it early would kill the child).
pub(crate) struct ConfinedChild {
    pub(crate) child: std::process::Child,
    #[cfg(windows)]
    _job: Option<crate::tools::windows::JobObject>,
}

/// Spawn a configured `Command` under OS-level confinement.
///
/// Shared by `run_shell`, `run_tests`, `run_lint`, and the git tools.
/// The `Command` must already have its env, cwd, and stdio configured.
pub(crate) fn spawn_confined(
    cmd: &mut Command,
    #[cfg_attr(not(unix), allow(unused_variables))] workspace: &Path,
    #[cfg_attr(not(unix), allow(unused_variables))] extra_rw: &[PathBuf],
    #[cfg_attr(not(unix), allow(unused_variables))] skip_network_block: bool,
    #[cfg_attr(not(unix), allow(unused_variables))] skip_rlimits: bool,
) -> Result<ConfinedChild> {
    #[cfg(unix)]
    {
        let ws = workspace.to_path_buf();
        let extra = extra_rw.to_vec();
        unsafe {
            cmd.pre_exec(move || {
                apply_os_confinement(&ws, &extra, skip_network_block, skip_rlimits);
                Ok(())
            });
        }
    }

    let child = cmd.spawn().context("spawn command")?;

    #[cfg(windows)]
    {
        match crate::tools::windows::JobObject::new() {
            Some(job) => {
                job.assign_process(child.id());
                Ok(ConfinedChild {
                    child,
                    _job: Some(job),
                })
            }
            None => Ok(ConfinedChild { child, _job: None }),
        }
    }

    #[cfg(not(windows))]
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
                    out.push_str(&String::from_utf8_lossy(&stdout));
                    out.push_str(&String::from_utf8_lossy(&stderr));
                    return Ok(super::cap_output(out));
                }
            }
            let mut out = format!("exit={}\n", status.code().unwrap_or(-1));
            out.push_str(&String::from_utf8_lossy(&stdout));
            out.push_str(&String::from_utf8_lossy(&stderr));
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
