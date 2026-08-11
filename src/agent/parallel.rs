//! Parallel sub-agents: run several focused agents in isolated git worktrees
//! and merge their branches back.

use anyhow::{Context, Result};
use tokio::sync::mpsc;

use crate::config::Settings;
use crate::tools::Sandbox;

use super::core::Agent;
use super::types::AgentEvent;

/// Report from a single parallel sub-agent.
#[derive(Debug, Clone)]
pub struct SubAgentReport {
    pub index: usize,
    pub text: String,
    pub elapsed: std::time::Duration,
    /// "merged", "conflict", "no changes", or "error: ..."
    pub merge_status: String,
}

/// Run several focused sub-agents in parallel and return their final reports.
///
/// Each sub-agent gets its own isolated git worktree on a unique branch so
/// that `git add -A` and `git commit` only stage and commit that sub-agent's
/// own work. After all sub-agents finish, each branch is merged back into the
/// original branch and the worktrees are cleaned up.
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
        });
        if let Some((branch_name, wt_path, sandbox)) = cleanup {
            branches_to_merge.push((i, branch_name, wt_path, sandbox));
        }
    }
    results.sort_by_key(|r| r.index);

    if is_git {
        let mut conflicted: Vec<(usize, String)> = Vec::new();
        for (i, branch_name, _wt_path, sandbox) in &branches_to_merge {
            let merge_result = sandbox.merge_branch(branch_name);
            let status = match merge_result {
                Ok(out) if out.contains("Already up to date") => "no changes".to_string(),
                Ok(out)
                    if out.contains("CONFLICT")
                        || sandbox.has_merge_conflicts().unwrap_or(false) =>
                {
                    let _ = sandbox.abort_merge();
                    conflicted.push((*i, branch_name.clone()));
                    tracing::warn!(
                        "merge conflict for sub-agent {} (branch {}), merge aborted",
                        i,
                        branch_name
                    );
                    "conflict".to_string()
                }
                Ok(_) => "merged".to_string(),
                Err(e) => {
                    tracing::warn!(
                        "merge error for sub-agent {} (branch {}): {}",
                        i,
                        branch_name,
                        e
                    );
                    format!("error: {e}")
                }
            };
            // Find the result for this sub-agent and set its merge_status.
            if let Some(r) = results.iter_mut().find(|r| r.index == *i) {
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
                "merge conflicts detected for {} sub-agent(s): {}. \
                 The working tree has been restored to its pre-merge state. \
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
