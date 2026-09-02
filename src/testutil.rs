//! Shared test utilities, available to every `#[cfg(test)]` module.
//!
//! The most important helper is [`outer_sandbox_restrictive`], which detects
//! when raven's own test process is running inside a *nested* sandbox (CI, a
//! harness, or this agent's own confinement) that applies its own seccomp
//! network block and a low RLIMIT_FSIZE. In that environment, tests that
//! exercise the inner sandbox's network/rlimit exemptions (or that open a
//! socket to a mock server) are killed by the *outer* layer before the inner
//! behavior can be observed. Such tests skip gracefully instead of failing.

/// Whether the test process is running under a restrictive outer sandbox that
/// pre-empts the inner sandbox's exemptions.
///
/// Detection: the outer sandbox caps RLIMIT_FSIZE below 70 MiB (the size the
/// rlimit-exemption tests write). This is a reliable, socket-free probe — the
/// outer seccomp network block would SIGSYS-kill a socket probe, so we must
/// not open one here. A low RLIMIT_FSIZE is a strong signal the outer sandbox
/// is present (it also applies its own seccomp network block).
pub fn outer_sandbox_restrictive() -> bool {
    #[cfg(unix)]
    {
        use std::sync::OnceLock;
        static CAP: OnceLock<bool> = OnceLock::new();
        *CAP.get_or_init(|| {
            let mut rlim = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            let ok = unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut rlim) } == 0;
            // 70 MiB is what the rlimit-exemption tests write. If the outer
            // soft limit is below that (and not raisable), the write is killed
            // before the inner exemption can be observed.
            ok && rlim.rlim_cur != libc::RLIM_INFINITY && rlim.rlim_cur < 70 * 1024 * 1024
        })
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Serialize tests that mutate the process-wide `HOME` env var.
///
/// Rust runs tests in parallel within one process; `dirs::home_dir()` reads
/// `HOME` (Unix) / `USERPROFILE` (Windows) at call time, so two tests that
/// both set or remove the variable race. Lock this for the whole
/// set→assert→restore window. Zero-dependency (no `serial_test` crate).
pub fn home_env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    match LOCK.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
