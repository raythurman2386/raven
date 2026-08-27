//! Parallel sub-agents: run several focused agents in isolated git worktrees
//! and merge their branches back.

use anyhow::{Context, Result};
use std::path::Path;
use tokio::sync::mpsc;

use crate::config::Settings;
use crate::tools::Sandbox;

use super::core::Agent;
use super::types::AgentEvent;

#[cfg(test)]
thread_local! {
    static DELEGATE_STUB: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// Test-only: next `delegate_task` returns `out` without spawning a child.
#[cfg(test)]
pub fn stub_delegate_task(out: impl Into<String>) {
    DELEGATE_STUB.with(|c| *c.borrow_mut() = Some(out.into()));
}

/// Report from a single parallel sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentReport {
    pub index: usize,
    pub text: String,
    pub elapsed: std::time::Duration,
    /// "applied", "no changes", "uncommitted (preserved)", or "error: ..."
    pub merge_status: String,
    /// Path to a recovery patch file when the sub-agent's work could not be
    /// merged but was preserved as a diff.
    pub recovery_patch: Option<String>,
}

/// Run a single focused sub-agent on `task` and return its accumulated text
/// output (bounded). Used by the `delegate_task` tool so the model can offload
/// a sub-task to a fresh context window and get back a distilled summary,
/// keeping the main conversation clean (Claude Code's subagent pattern).
///
/// The sub-agent shares the workspace (no git-worktree isolation) and inherits
/// the same sandbox confinement. Nesting is disabled (`allow_delegate = false`)
/// so the child cannot spawn another delegate or overwrite parent goal/todos.
/// Tool events are consumed silently; only `TextDelta` output is accumulated.
pub async fn delegate_task(
    mut settings: Settings,
    task: String,
    parent_tx: mpsc::Sender<AgentEvent>,
) -> Result<String> {
    #[cfg(test)]
    if let Some(out) = DELEGATE_STUB.with(|c| c.borrow_mut().take()) {
        let _ = (settings, task);
        return Ok(out);
    }
    settings.allow_delegate = false;
    settings.max_iterations = settings.max_iterations.min(8);
    let mut agent = Agent::new(settings)?;
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    // Box the run future: delegate_task is reachable from agent.run (via the
    // delegate_task tool), so an unboxed recursive async fn would be infinitely
    // sized. Boxing breaks the cycle.
    let run = Box::pin(agent.run(&task, tx));
    let drain = async {
        let mut out = String::new();
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => out.push_str(&t),
                AgentEvent::Iteration(n) => {
                    // Bubble sub-agent iterations up so the parent TUI shows
                    // progress instead of silently waiting (the sub-agent's
                    // other events stay suppressed to keep the log clean).
                    let _ = parent_tx.send(AgentEvent::Subagent { iter: n }).await;
                }
                AgentEvent::Done | AgentEvent::Error(_) => break,
                _ => {}
            }
        }
        out
    };
    let (run_res, out) = tokio::join!(run, drain);
    if let Err(e) = run_res {
        tracing::warn!("delegate_task sub-agent failed: {e}");
    }
    Ok(out)
}

/// Run several focused sub-agents in parallel and return their final reports.
///
/// Each sub-agent gets its own isolated git worktree on a unique branch so
/// uncommitted edits cannot clobber each other. After all sub-agents finish,
/// each worktree's diff versus the original HEAD is applied to the parent
/// working tree (no commit) and the worktrees are cleaned up.
///
/// If the workspace is not a git repository, sub-agents share the workspace
/// directly (no isolation).
///
/// Live progress (tool calls, text deltas) is streamed to stderr with a
/// `[sub-agent N]` prefix so the user can observe concurrent execution.
pub async fn run_parallel(settings: &Settings, tasks: Vec<String>) -> Result<Vec<SubAgentReport>> {
    let sandbox = Sandbox::new(settings.workspace.clone());
    let is_git = sandbox.is_git_repo().unwrap_or(false);

    let worktree_dir = if is_git {
        let dir = tempfile::tempdir().context("create worktree temp dir")?;
        Some(dir)
    } else {
        None
    };

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut handles = Vec::new();
    for (i, task) in tasks.into_iter().enumerate() {
        let s = settings.clone();
        let branch_name = format!("raven-sub-{}-{}", i, timestamp);
        let wt_dir = worktree_dir.as_ref().map(|d| d.path().to_path_buf());
        eprintln!("[sub-agent {}] starting: {}", i, task);
        let handle = tokio::spawn(async move {
            let start = std::time::Instant::now();
            let (agent_settings, cleanup) = if let Some(ref wt_base) = wt_dir {
                let wt_path = wt_base.join(format!("sub-{}", i));
                let sandbox = Sandbox::new(s.workspace.clone());
                match sandbox.create_worktree(&branch_name, &wt_path) {
                    Ok(()) => {
                        let mut sub_settings = s.clone();
                        sub_settings.workspace = wt_path.clone();
                        // The worktree shares the main repo's `.git`, which
                        // lives outside the worktree (a sibling under the temp
                        // dir). Grant the sub-agent's sandbox RW access to the
                        // main workspace so git works across the shared repo,
                        // without opening up the whole temp dir.
                        sub_settings.sandbox_extra_rw = vec![s.workspace.clone()];
                        (sub_settings, Some((branch_name, wt_path, sandbox)))
                    }
                    Err(e) => {
                        tracing::warn!(
                            "sub-agent {}: failed to create worktree ({}), falling back to shared workspace",
                            i, e
                        );
                        (s.clone(), None)
                    }
                }
            } else {
                (s.clone(), None)
            };

            let mut agent = Agent::new(agent_settings)?;
            let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
            let runner = tokio::spawn(async move {
                if let Err(e) = agent.run(&task, tx).await {
                    tracing::warn!("sub-agent {} failed: {}", i, e);
                }
            });
            let mut out = String::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    AgentEvent::TextDelta(t) => {
                        out.push_str(&t);
                        eprint!("{}", t);
                        let _ = std::io::Write::flush(&mut std::io::stderr());
                    }
                    AgentEvent::ToolStart { name, args } => {
                        eprintln!("\n[sub-agent {}] → {}({})", i, name, args);
                    }
                    AgentEvent::ToolEnd { name, preview } => {
                        eprintln!(
                            "[sub-agent {}]   [{}] {}",
                            i,
                            name,
                            preview.chars().take(200).collect::<String>()
                        );
                    }
                    AgentEvent::Iteration(n) => {
                        eprintln!("[sub-agent {}] [iter {}]", i, n);
                    }
                    AgentEvent::Done | AgentEvent::Error(_) => break,
                    _ => {}
                }
            }
            let _ = runner.await;
            let elapsed = start.elapsed();
            Ok::<_, anyhow::Error>((out, cleanup, elapsed))
        });
        handles.push((i, handle));
    }

    let mut results: Vec<SubAgentReport> = Vec::with_capacity(handles.len());
    let mut branches_to_merge: Vec<(usize, String, std::path::PathBuf, Sandbox)> = Vec::new();
    for (i, h) in handles {
        let (out, cleanup, elapsed) = h.await??;
        results.push(SubAgentReport {
            index: i,
            text: out,
            elapsed,
            merge_status: String::new(), // set during merge below
            recovery_patch: None,
        });
        if let Some((branch_name, wt_path, sandbox)) = cleanup {
            branches_to_merge.push((i, branch_name, wt_path, sandbox));
        }
    }
    results.sort_by_key(|r| r.index);

    let base_rev = if is_git {
        sandbox.rev_parse_head().unwrap_or_default()
    } else {
        String::new()
    };

    if is_git {
        let mut conflicted: Vec<(usize, String)> = Vec::new();
        for (i, branch_name, wt_path, sandbox) in &branches_to_merge {
            let main_ws = sandbox.workspace.clone();
            let wt_sandbox = Sandbox::with_extra_rw(wt_path.clone(), vec![main_ws.clone()]);
            let patch = wt_sandbox.export_diff_from(&base_rev).unwrap_or_default();
            let status = if patch.trim().is_empty() {
                "no changes".to_string()
            } else {
                match sandbox.apply_git_patch(&patch) {
                    Ok(_) => "applied".to_string(),
                    Err(e) => {
                        let reason = format!("apply failed: {e}");
                        let patch_rel = persist_recovery_patch(&main_ws, *i, &patch, &reason);
                        conflicted.push((*i, branch_name.clone()));
                        tracing::warn!(
                            "patch apply failed for sub-agent {} (branch {}): {}; \
                             recovery patch written to {}",
                            i,
                            branch_name,
                            e,
                            patch_rel,
                        );
                        format!("uncommitted (preserved) → {}", patch_rel)
                    }
                }
            };
            if let Some(r) = results.iter_mut().find(|r| r.index == *i) {
                let patch = if status.starts_with("uncommitted (preserved)") {
                    status
                        .strip_prefix("uncommitted (preserved) → ")
                        .map(|s| s.to_string())
                } else {
                    None
                };
                r.recovery_patch = patch;
                r.merge_status = status;
            }
        }
        // Remove each worktree first so the branch is no longer referenced by
        // a live worktree; only then can the branch be deleted. Doing this in
        // the wrong order leaves a stale `prunable` worktree entry and an
        // orphaned branch that `git branch -D` refuses to delete.
        for (_i, _branch_name, wt_path, sandbox) in &branches_to_merge {
            let _ = sandbox.remove_worktree(wt_path);
        }
        for (_i, branch_name, _wt_path, sandbox) in &branches_to_merge {
            let _ = sandbox.delete_branch(branch_name);
        }
        if !conflicted.is_empty() {
            let names: Vec<String> = conflicted
                .iter()
                .map(|(i, b)| format!("sub-agent {} (branch {})", i, b))
                .collect();
            anyhow::bail!(
                "patch apply failed for {} sub-agent(s): {}. \
                 Successful applies remain uncommitted in the working tree. \
                 Recovery patches are under `.raven/recovery-sub-*.patch` \
                 (see `.raven/RECOVERY.md`). Apply with `git apply <patch>`. \
                 To avoid conflicts, assign disjoint files to each sub-agent.",
                conflicted.len(),
                names.join(", ")
            );
        }
    }

    if let Some(dir) = worktree_dir {
        let _ = dir.close();
    }

    Ok(results)
}

/// Write a sub-agent's unapplied diff to `.raven/recovery-sub-N.patch` and
/// append a line to `.raven/RECOVERY.md` so the artifact is findable after
/// the CLI output has scrolled away.
fn persist_recovery_patch(workspace: &Path, index: usize, patch: &str, reason: &str) -> String {
    let rel = format!(".raven/recovery-sub-{index}.patch");
    if !patch.trim().is_empty() {
        let full = workspace.join(&rel);
        if let Some(parent) = full.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&full, patch) {
            tracing::warn!("failed to write recovery patch for sub-agent {index}: {e}");
        } else {
            append_recovery_index(workspace, index, &rel, reason);
        }
    }
    rel
}

fn append_recovery_index(workspace: &Path, index: usize, rel: &str, reason: &str) {
    let path = workspace.join(".raven").join("RECOVERY.md");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let mut body = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        "# Raven recovery\n\nSub-agent work that could not be merged is preserved here.\n\n".into()
    });
    body.push_str(&format!(
        "- sub-agent {index}: {reason}\n  - patch: `{rel}`\n  - apply: `git apply {rel}`\n"
    ));
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!("failed to update .raven/RECOVERY.md: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_recovery_index_creates_markdown() {
        let tmp = tempfile::tempdir().unwrap();
        append_recovery_index(
            tmp.path(),
            0,
            ".raven/recovery-sub-0.patch",
            "merge conflict",
        );
        let md = std::fs::read_to_string(tmp.path().join(".raven/RECOVERY.md")).unwrap();
        assert!(md.contains("sub-agent 0"));
        assert!(md.contains("git apply .raven/recovery-sub-0.patch"));
        assert!(md.contains("merge conflict"));
    }
}
