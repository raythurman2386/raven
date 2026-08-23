//! Git tools: status, diff, log, commit, undo.

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
        Ok(if out.trim().is_empty() {
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

    /// Stage all changes and create a commit. Used by the agent to checkpoint
    /// its own work. Returns the new HEAD line.
    pub fn git_commit(&self, message: &str) -> Result<String> {
        self.git_commit_inner(message, true)
    }

    /// Checkpoint commit used by the harness itself (budget exhaustion in
    /// `core.rs`, uncommitted sub-agent work in `parallel.rs`). Unlike the
    /// model-facing [`Self::git_commit`], this preserves additions and
    /// modifications but does NOT stage deletions of tracked files — a
    /// collateral deletion (e.g. a sub-agent's failed `npm install` removing
    /// `package-lock.json`) must not be swept into a checkpoint commit. The
    /// checkpoint's job is to preserve *code* work, not to ratify accidental
    /// file removal.
    pub fn git_commit_checkpoint(&self, message: &str) -> Result<String> {
        self.git_commit_inner(message, false)
    }

    fn git_commit_inner(&self, message: &str, include_deletions: bool) -> Result<String> {
        let msg = message.trim();
        if msg.is_empty() {
            return Ok("Error: empty commit message".into());
        }
        if !self.is_git_repo()? {
            return Ok("Error: not a git repository (no .git found)".into());
        }
        let porcelain = self.run_git(&["status", "--porcelain=v1"])?;
        if porcelain == "exit=0" || porcelain.trim().is_empty() {
            return Ok("No changes to commit (working tree clean)".into());
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
        if !include_deletions {
            // Unstage deletions of tracked files and stray untracked temp
            // files so a checkpoint never commits collateral file removal or
            // accidental junk. `git diff --cached --name-status` lists what
            // `git add -A` just staged; `D` entries are tracked-file deletions
            // and `A` entries matching a stray-temp pattern are scratch files
            // the model's tooling dropped in the workspace. Restore them to
            // the index (keeping the working-tree change) so they don't enter
            // the checkpoint commit.
            let staged = self.run_git(&["diff", "--cached", "--name-status"])?;
            let mut unstaged: Vec<&str> = Vec::new();
            for line in staged.lines() {
                let mut it = line.splitn(2, '\t');
                let status = it.next().unwrap_or("").trim();
                let path = it.next().unwrap_or("").trim();
                if path.is_empty() {
                    continue;
                }
                let is_deletion = status == "D";
                let is_stray_addition = status == "A" && Self::is_stray_temp(path);
                if is_deletion || is_stray_addition {
                    unstaged.push(path);
                }
            }
            if !unstaged.is_empty() {
                let mut args = vec!["restore", "--staged", "--"];
                args.extend(unstaged.iter().copied());
                let _ = self.run_git(&args)?;
            }
        }
        if let Some(refusal) = self.refuse_if_staged_secrets()? {
            return Ok(refusal);
        }
        let commit_out = self.run_git(&["commit", "-m", msg])?;
        if commit_out.contains("fatal") || commit_out.contains("Error") {
            return Ok(commit_out);
        }
        self.git_log(1)
    }

    /// Scan staged files for well-known secret patterns and refuse the commit
    /// if any match. Complements the pathspec exclusions (`.env`, `.raven/`).
    ///
    /// Fail-closed: if the staged-name listing looks truncated or a staged
    /// path escapes the workspace, the commit is refused rather than skipped.
    fn refuse_if_staged_secrets(&self) -> Result<Option<String>> {
        let listing = self.run_git(&["diff", "--cached", "--name-only", "--diff-filter=ACMR"])?;
        if listing.contains("[truncated") {
            return Ok(Some(
                "Error: git_commit refused — staged change list is too large to scan for secrets"
                    .into(),
            ));
        }
        let mut findings = Vec::new();
        for line in listing.lines() {
            let path = line.trim();
            if path.is_empty() || path == "exit=0" || path.starts_with("warning:") {
                continue;
            }
            if path.contains("fatal") || path.contains("error:") {
                continue;
            }
            if path.contains("..") {
                return Ok(Some(format!(
                    "Error: git_commit refused — staged path looks unsafe: {path}"
                )));
            }
            let Ok(resolved) = self.safe_resolve(path) else {
                return Ok(Some(format!(
                    "Error: git_commit refused — staged path escapes workspace: {path}"
                )));
            };
            let Ok(bytes) = std::fs::read(&resolved) else {
                continue;
            };
            findings.extend(super::secrets::scan_bytes(path, &bytes));
            if findings.len() >= 12 {
                break;
            }
        }
        if findings.is_empty() {
            Ok(None)
        } else {
            Ok(Some(super::secrets::format_refusal(&findings)))
        }
    }

    /// Whether a path is a stray temp/scratch file the model's tooling may
    /// drop in the workspace root. These must never be swept into a checkpoint
    /// commit.
    fn is_stray_temp(path: &str) -> bool {
        let name = path.rsplit('/').next().unwrap_or(path);
        name == "err.txt"
            || name == "out.txt"
            || name == "testout.txt"
            || name.starts_with("_tmp_")
            || name.ends_with(".log")
    }

    /// Undo the last commit, keeping changes in the working tree
    /// (`git reset --soft HEAD~1`). Non-destructive: nothing is lost.
    pub fn git_undo(&self) -> Result<String> {
        if !self.is_git_repo()? {
            return Ok("Error: not a git repository (no .git found)".into());
        }
        let count = self.run_git(&["rev-list", "--count", "HEAD"])?;
        if count.contains("fatal") {
            return Ok("No commits to undo".into());
        }
        let out = self.run_git(&["reset", "--soft", "HEAD~1"])?;
        if out.contains("fatal") || out.starts_with("exit=") {
            return Ok(out);
        }
        Ok(format!(
            "Undid the last commit; changes are back in the working tree.\n{}",
            self.git_status()?
        ))
    }

    /// Create a git worktree at `worktree_path` on a new branch `branch_name`
    /// based on the current HEAD. The worktree shares the same git repository
    /// but has its own working tree, so `git add -A` and `git commit` only
    /// stage and commit changes made in that worktree.
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

    /// Merge a branch into the current branch with `--no-edit`.
    pub fn merge_branch(&self, branch_name: &str) -> Result<String> {
        self.run_git(&["merge", branch_name, "--no-edit"])
    }

    /// Check whether the working tree has unresolved merge conflicts.
    pub fn has_merge_conflicts(&self) -> Result<bool> {
        let status = self.run_git(&["status", "--porcelain=v1"])?;
        Ok(status.lines().any(|l| l.starts_with("UU ")))
    }

    /// Abort an in-progress merge and return to the pre-merge state.
    pub fn abort_merge(&self) -> Result<String> {
        self.run_git(&["merge", "--abort"])
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

    /// Get the diff between HEAD and a branch (three-dot: changes on the
    /// branch since it diverged from HEAD). Used to capture a recovery patch
    /// for a sub-agent branch that cannot be merged.
    pub fn branch_diff(&self, branch_name: &str) -> Result<String> {
        let spec = format!("HEAD...{}", branch_name);
        let out = self.run_git(&["diff", &spec])?;
        Ok(truncate_output(&out, MAX_TOOL_OUTPUT))
    }

    /// Check whether the working tree has no uncommitted changes.
    ///
    /// Returns `true` when the workspace is not a git repository or when
    /// `git status --porcelain` produces no output. Git failures default
    /// to `true` (fail-open) so the guard never blocks the agent loop.
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

    /// Run `git` with an extra set of Landlock RW roots for the confined child.
    ///
    /// Used by [`Self::create_worktree`] to let `git worktree add` create the
    /// worktree sibling under the temp dir without granting RW on the whole
    /// temp dir (which would reopen the `06_sandbox_escape` hole).
    fn run_git_with_extra(&self, args: &[&str], extra_rw: &[std::path::PathBuf]) -> Result<String> {
        let mut cmd = Command::new("git");
        // Agent commits must not depend on the host git identity, hooks, or
        // commit.gpgsign. Those are the usual reasons `05_git_commit_clean`
        // flakes: the model called git_commit, git refused, tree stayed dirty.
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
