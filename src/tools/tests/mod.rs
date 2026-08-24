//! Unit tests for the workspace-scoped tools and sandbox, split by subsystem.
//!
//! The tests were originally one ~2,170-line block in `tools/mod.rs`; they are
//! now grouped by the subsystem they exercise:
//!
//! - [`sandbox_fs`] — path resolution, file I/O, `search_replace`, `grep`, globs
//! - [`sandbox_shell`] — `run_shell`, OS confinement, command allow/deny lists
//! - [`git`] — status/diff/log, worktrees, patch-apply merge
//! - [`dispatch`] — tool dispatch and the plan/chat/full toolsets
//! - [`patch`] — unified-diff parsing and `apply_patch`
//! - [`verify`] — `run_tests`/`run_lint` and the verification-gate matcher

use crate::tools::Sandbox;

mod dispatch;
mod git;
mod patch;
mod sandbox_fs;
mod sandbox_shell;
mod verify;

/// A fresh `Sandbox` rooted at a throwaway temp dir.
fn sandbox() -> Sandbox {
    let tmp = tempfile::tempdir().unwrap();
    Sandbox::new(tmp.path().canonicalize().unwrap())
}
