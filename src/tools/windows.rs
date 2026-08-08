//! Windows-specific confinement via Job Objects.
//!
//! Job Objects are the Windows-native equivalent of the Unix-side
//! Landlock/seccomp/rlimits model. A Job Object can:
//!
//! - confine a whole process tree (a process in a job cannot escape it),
//! - set resource limits (CPU, memory, active process count, per-process time),
//! - kill all processes in the job when the last handle to the job is closed
//!   (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`).
//!
//! Raven assigns every subprocess it spawns to a fresh Job Object and keeps a
//! handle to it open for the lifetime of that subprocess. When the handle is
//! dropped, `KILL_ON_JOB_CLOSE` guarantees a runaway child (and its entire
//! process tree) cannot outlive the parent Raven process.
//!
//! # Lifetime contract
//!
//! A process assigned to a job does **not** hold a reference to the job object;
//! the job is destroyed when its last *handle* is closed. Therefore the parent
//! must retain the [`JobObject`] handle across the child's lifetime and drop it
//! only after the child has been waited on (or killed on timeout). Dropping the
//! guard is what triggers `KILL_ON_JOB_CLOSE`.

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
};

/// A Windows Job Object with Raven's confinement limits applied.
///
/// Dropping this guard closes the job handle. With `KILL_ON_JOB_CLOSE` set,
/// that terminates any processes still associated with the job — so this is
/// both the resource limit and the "cannot outlive the parent" guarantee.
pub(crate) struct JobObject(HANDLE);

impl JobObject {
    /// Create a new Job Object with Raven's default limits.
    ///
    /// Best-effort: on any failure we return `None`, letting the caller
    /// continue without job confinement. A security reviewer should treat a
    /// `None` here as "job confinement is NOT active".
    pub(crate) fn new() -> Option<Self> {
        // SAFETY: CreateJobObjectW with a null name/attributes returns a new
        // unnamed kernel object; null is always a valid argument here.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            tracing::warn!("CreateJobObjectW failed; job confinement disabled");
            return None;
        }

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
            | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
            | JOB_OBJECT_LIMIT_PROCESS_MEMORY
            | JOB_OBJECT_LIMIT_JOB_MEMORY;
        // Max 256 processes per job (kills runaway process trees).
        info.BasicLimitInformation.ActiveProcessLimit = 256;
        // 1 GiB per process, 1 GiB per job — matches the Unix RLIMIT_AS value.
        info.ProcessMemoryLimit = 1 << 30;
        info.JobMemoryLimit = 1 << 30;

        // SAFETY: `info` is a valid, fully-initialized JOBOBJECT_EXTENDED_LIMIT_
        // INFORMATION with the matching class enum; the length is exact.
        let ok = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            tracing::warn!("SetInformationJobObject failed; job confinement disabled");
            // SAFETY: dropping the only reference to the job handle.
            unsafe { CloseHandle(handle) };
            return None;
        }

        Some(Self(handle))
    }

    /// Assign the process identified by `pid` to this job.
    ///
    /// Returns `false` (after logging) if the process could not be opened or
    /// assigned. The caller should still let the child run — job confinement
    /// is best-effort, and other layers may still apply.
    pub(crate) fn assign_process(&self, pid: u32) -> bool {
        // Open the child with the minimal access we need to (a) assign it to
        // the job and (b) terminate it. We deliberately do NOT use
        // PROCESS_ALL_ACCESS.
        // SAFETY: opening by PID with a fixed rights mask; null is a valid
        // inherit-handle argument.
        let process = unsafe {
            OpenProcess(
                PROCESS_SET_QUOTA | PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if process.is_null() {
            tracing::warn!("OpenProcess({pid}) failed; child not job-confined");
            return false;
        }

        // SAFETY: both handles are valid and open.
        let ok = unsafe { AssignProcessToJobObject(self.0, process) };
        // SAFETY: close the process handle we opened (the job holds its own
        // reference to the process).
        unsafe { CloseHandle(process) };
        if ok == 0 {
            tracing::warn!("AssignProcessToJobObject({pid}) failed; child not job-confined");
            return false;
        }
        true
    }
}

impl Drop for JobObject {
    fn drop(&mut self) {
        // SAFETY: dropping the last (or only) handle to the job. With
        // KILL_ON_JOB_CLOSE set, any still-running processes in the job are
        // terminated here. The caller must drop this only after waiting on the
        // child (or killing it on timeout), otherwise the child is killed
        // immediately.
        unsafe { CloseHandle(self.0) };
    }
}
