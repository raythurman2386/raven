//! Git tools: status, diff, log, commit, undo.

use anyhow::{Context, Result};
use std::process::Command;

use super::sandbox::{cap_output, truncate_output, wait_for_child, Sandbox, MAX_TOOL_OUTPUT};

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
        let _ = self.run_git(&["add", "-A"])?;
        let commit_out = self.run_git(&["commit", "-m", msg])?;
        if commit_out.contains("fatal") || commit_out.contains("Error") {
            return Ok(commit_out);
        }
        self.git_log(1)
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

    pub(crate) fn is_git_repo(&self) -> Result<bool> {
        let mut child = Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("spawn git")?;
        match wait_for_child(&mut child, 30) {
            Some((status, _, _)) => Ok(status.success()),
            None => Ok(false),
        }
    }

    pub(crate) fn run_git(&self, args: &[&str]) -> Result<String> {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .context("spawn git")?;
        match wait_for_child(&mut child, 30) {
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
