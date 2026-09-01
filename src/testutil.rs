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
