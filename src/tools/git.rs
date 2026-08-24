//! Git tools: status, diff, log, plus worktree isolation for parallel sub-agents.
//!
//! The harness never creates commits. Parallel sub-agent work is captured as a
//! unified diff and applied to the parent working tree.

use anyhow::{Context, Result};
use std::process::Command;

use super::sandbox::{
    cap_output, setup_shell_env, spawn_confined, truncate_output, wait_for_child, Sandbox,
    MAX_TOOL_OUTPUT,
};

impl Sandbox {
    /// `git status --porcelain=v1` — structured, compact output.
    pub fn git_status(&self) -> Result<String> {
        let out = self.run_git(&["status", "--porcelain=v1"])?;
        Ok(if git_out_empty(&out) {
            "No changes (working tree clean)".into()
        } else {
            out
        })
    }

    /// `git diff` — unstaged or staged changes.
    pub fn git_diff(&self, staged: bool) -> Result<String> {
        let args = if staged {
            &["diff", "--staged"][..]
        } else {
            &["diff"][..]
        };
        let out = self.run_git(args)?;
        Ok(truncate_output(&out, MAX_TOOL_OUTPUT))
    }

    /// `git log --oneline -n 10` — recent commit history.
    pub fn git_log(&self, n: usize) -> Result<String> {
        let n_str = n.to_string();
        let out = self.run_git(&["log", "--oneline", "-n", &n_str])?;
        Ok(out)
    }

    /// Resolve `HEAD` to a SHA. Used as the base revision when capturing a
    /// worktree's changes for patch-apply (no commit).
    pub fn rev_parse_head(&self) -> Result<String> {
        let out = self.run_git(&["rev-parse", "HEAD"])?;
        Ok(out.lines().next().unwrap_or("").trim().to_string())
    }

    /// Capture all worktree changes versus `base_rev` as a unified diff.
    ///
    /// Untracked files are included except `.raven/`, `data/`, `.env`, and
    /// `.env.*`. Does **not** create a commit. The index is restored afterward.
    pub fn export_diff_from(&self, base_rev: &str) -> Result<String> {
        let base = base_rev.trim();
        if base.is_empty() || base.contains("fatal") {
            return Ok(String::new());
        }
        let _ = self.run_git(&[
            "add",
            "-A",
            "--",
            ":!.raven/",
            ":!data/",
            ":!.env",
            ":!.env.*",
        ])?;
        let out = self.run_git(&["diff", "--cached", "--binary", base])?;
        let _ = self.run_git(&["reset", "-q", "HEAD"]);
        if git_out_empty(&out) {
            Ok(String::new())
        } else {
            Ok(out)
        }
    }

    /// Apply a unified diff to this workspace's working tree without committing.
    pub fn apply_git_patch(&self, patch: &str) -> Result<String> {
        let trimmed = patch.trim();
        if trimmed.is_empty() || git_out_empty(trimmed) {
            return Ok("no changes".into());
        }
        let rel = ".raven/tmp/apply.patch";
        let full = self.workspace.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, patch)?;
        let check = self.run_git(&["apply", "--check", "--", rel])?;
        if git_cmd_failed(&check) {
            let _ = std::fs::remove_file(&full);
            anyhow::bail!("patch does not apply: {check}");
        }
        let out = self.run_git(&["apply", "--", rel])?;
        let _ = std::fs::remove_file(&full);
        if git_cmd_failed(&out) {
            anyhow::bail!("git apply failed: {out}");
        }
        Ok(if git_out_empty(&out) {
            "applied".into()
        } else {
            out
        })
    }

    /// Create a git worktree at `worktree_path` on a new branch `branch_name`
    /// based on the current HEAD. The worktree shares the same git repository
    /// but has its own working tree, so parallel sub-agents cannot clobber
    /// each other's uncommitted files.
    pub fn create_worktree(
        &self,
        branch_name: &str,
        worktree_path: &std::path::Path,
    ) -> Result<()> {
        let path_str = worktree_path.to_string_lossy();
        // Grant RW on the worktree's parent dir so `git worktree add` can
        // create the sibling — without opening up the whole temp dir (which
        // would reopen the `06_sandbox_escape` hole when the workspace lives
        // under the temp dir). If the parent doesn't exist yet (tempdir parent
        // does), git needs to be able to mkdir it.
        let parent = worktree_path
            .parent()
            .unwrap_or(worktree_path)
            .to_path_buf();
        let out = self.run_git_with_extra(
            &["worktree", "add", "-b", branch_name, &path_str, "HEAD"],
            &[parent],
        )?;
        if out.contains("fatal") || out.contains("error") {
            anyhow::bail!("failed to create worktree: {}", out);
        }
        Ok(())
    }

    /// Remove a git worktree, even if it has uncommitted changes.
    pub fn remove_worktree(&self, worktree_path: &std::path::Path) -> Result<()> {
        let path_str = worktree_path.to_string_lossy();
        // Like [`Self::create_worktree`], grant RW on the worktree's parent so
        // git can delete the sibling without opening up the whole temp dir.
        let parent = worktree_path
            .parent()
            .unwrap_or(worktree_path)
            .to_path_buf();
        let _ =
            self.run_git_with_extra(&["worktree", "remove", &path_str, "--force"], &[parent])?;
        Ok(())
    }

    /// Delete a branch (forcefully, even if not merged).
    pub fn delete_branch(&self, branch_name: &str) -> Result<()> {
        let _ = self.run_git(&["branch", "-D", branch_name])?;
        Ok(())
    }

    /// Check whether the working tree has no uncommitted changes.
    ///
    /// Returns `true` when the workspace is not a git repository or when
    /// `git status --porcelain` produces no output. Git failures default
    /// to `true` (fail-open).
    pub fn is_working_tree_clean(&self) -> bool {
        if !self.is_git_repo().unwrap_or(false) {
            return true;
        }
        self.run_git(&["status", "--porcelain=v1"])
            .map(|out| out.trim().is_empty() || out.trim() == "exit=0")
            .unwrap_or(true)
    }

    pub(crate) fn is_git_repo(&self) -> Result<bool> {
        let mut cmd = Command::new("git");
        cmd.args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        setup_shell_env(&mut cmd, &self.workspace);
        let mut confined = spawn_confined(&mut cmd, &self.workspace, &self.extra_rw, false)
            .context("spawn git")?;
        match wait_for_child(&mut confined.child, 30) {
            Some((status, _, _)) => Ok(status.success()),
            None => Ok(false),
        }
    }

    pub(crate) fn run_git(&self, args: &[&str]) -> Result<String> {
        self.run_git_with_extra(args, &self.extra_rw)
    }

    /// Seed a commit in tests without exposing a model-facing commit tool.
    #[cfg(test)]
    pub(crate) fn test_commit(&self, message: &str) -> Result<String> {
        let _ = self.run_git(&["add", "-A"])?;
        self.run_git(&["commit", "-m", message])
    }

    /// Run `git` with an extra set of Landlock RW roots for the confined child.
    ///
    /// Used by [`Self::create_worktree`] to let `git worktree add` create the
    /// worktree sibling under the temp dir without granting RW on the whole
    /// temp dir (which would reopen the `06_sandbox_escape` hole).
    fn run_git_with_extra(&self, args: &[&str], extra_rw: &[std::path::PathBuf]) -> Result<String> {
        let mut cmd = Command::new("git");
        // Isolate git from host identity/hooks so worktree and test seeding
        // do not depend on `~/.gitconfig` or `commit.gpgsign`.
        cmd.args([
            "-c",
            "user.name=raven",
            "-c",
            "user.email=raven@local",
            "-c",
            "commit.gpgsign=false",
            "-c",
            #[cfg(unix)]
            "core.hooksPath=/dev/null",
            #[cfg(not(unix))]
            "core.hooksPath=NUL",
        ]);
        cmd.args(args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        setup_shell_env(&mut cmd, &self.workspace);
        cmd.env("GIT_AUTHOR_NAME", "raven")
            .env("GIT_AUTHOR_EMAIL", "raven@local")
            .env("GIT_COMMITTER_NAME", "raven")
            .env("GIT_COMMITTER_EMAIL", "raven@local")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        let mut confined =
            spawn_confined(&mut cmd, &self.workspace, extra_rw, false).context("spawn git")?;
        match wait_for_child(&mut confined.child, 30) {
            Some((status, stdout, stderr)) => {
                let mut out = String::new();
                if !stdout.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&stdout));
                }
                if !stderr.is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&String::from_utf8_lossy(&stderr));
                }
                if out.is_empty() {
                    out = format!("exit={}", status.code().unwrap_or(-1));
                }
                Ok(cap_output(out))
            }
            None => Ok("Error: git command timed out".into()),
        }
    }
}

fn git_out_empty(out: &str) -> bool {
    let t = out.trim();
    t.is_empty() || t == "exit=0"
}

fn git_cmd_failed(out: &str) -> bool {
    if git_out_empty(out) {
        return false;
    }
    let t = out.to_ascii_lowercase();
    t.contains("fatal") || t.contains("error:") || t.contains("patch failed")
}
