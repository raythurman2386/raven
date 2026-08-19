//! Tests for git commit/undo, worktree isolation, merges, and checkpoints.

use crate::tools::Sandbox;

/// Initialize a throwaway git repo and return a Sandbox for it.
fn git_sandbox() -> (tempfile::TempDir, Sandbox) {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    sb.run_shell(
        "git init -q && git config user.email test@test && git config user.name test && git config core.autocrlf false",
        20,
    )
    .unwrap();
    (tmp, sb)
}

#[test]
fn git_commit_stages_and_commits_changes() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("a.txt", "v1").unwrap();
    let out = sb.git_commit("add a.txt").unwrap();
    assert!(!out.contains("Error"), "commit should succeed: {out}");
    assert!(!out.contains("No changes"), "should have committed");
    let log = sb.git_log(5).unwrap();
    assert!(log.contains("add a.txt"), "commit in log: {log}");
}

#[test]
fn git_commit_no_changes_returns_message() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("a.txt", "v1").unwrap();
    sb.git_commit("first").unwrap();
    let out = sb.git_commit("again").unwrap();
    assert!(out.contains("No changes"), "clean tree: {out}");
}

#[test]
fn git_commit_empty_message_errors() {
    let (_tmp, sb) = git_sandbox();
    let out = sb.git_commit("   ").unwrap();
    assert!(out.contains("empty commit message"));
}

#[test]
fn git_commit_does_not_need_host_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    sb.run_shell(
        "git init -q && git config --unset-all user.name; git config --unset-all user.email; true",
        20,
    )
    .unwrap();
    sb.write_file("a.txt", "v1").unwrap();
    let out = sb.git_commit("add a.txt").unwrap();
    assert!(
        !out.contains("Error"),
        "commit should succeed without host identity: {out}"
    );
    assert!(!out.contains("Please tell me who you are"), "{out}");
    let log = sb.git_log(1).unwrap();
    assert!(log.contains("add a.txt"), "commit in log: {log}");
}

#[test]
fn git_commit_not_a_repo_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.git_commit("msg").unwrap();
    assert!(out.contains("not a git repository"), "{out}");
}

#[test]
fn git_undo_restores_changes_to_working_tree() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("base.txt", "base").unwrap();
    sb.git_commit("init").unwrap();
    sb.write_file("a.txt", "v1").unwrap();
    sb.git_commit("add a.txt").unwrap();
    assert!(sb.git_log(5).unwrap().contains("add a.txt"));
    let out = sb.git_undo().unwrap();
    assert!(!out.contains("Error"), "undo should succeed: {out}");
    assert!(!sb.git_log(5).unwrap().contains("add a.txt"));
    assert!(sb.git_status().unwrap().contains("a.txt"));
}

#[test]
fn git_undo_no_commits_returns_message() {
    let (_tmp, sb) = git_sandbox();
    let out = sb.git_undo().unwrap();
    assert!(out.contains("No commits to undo"), "{out}");
}

#[test]
fn git_commit_excludes_raven_sessions() {
    let (_tmp, sb) = git_sandbox();
    std::fs::create_dir_all(sb.workspace.join(".raven/sessions/abc123")).unwrap();
    std::fs::write(
        sb.workspace.join(".raven/sessions/abc123/messages.jsonl"),
        "session data",
    )
    .unwrap();
    sb.write_file("src/main.rs", "fn main() {}").unwrap();
    let out = sb.git_commit("add main.rs").unwrap();
    assert!(!out.contains("Error"), "commit should succeed: {out}");
    let log = sb.git_log(5).unwrap();
    assert!(log.contains("add main.rs"), "commit in log: {log}");
    let status = sb.git_status().unwrap();
    assert!(
        status.contains(".raven/"),
        ".raven/ should remain unstaged: {status}"
    );
}

#[test]
fn git_commit_excludes_data_dir() {
    let (_tmp, sb) = git_sandbox();
    std::fs::create_dir_all(sb.workspace.join("data")).unwrap();
    std::fs::write(sb.workspace.join("data/notes.json"), "runtime data").unwrap();
    sb.write_file("src/lib.rs", "pub fn add() {}").unwrap();
    let out = sb.git_commit("add lib.rs").unwrap();
    assert!(!out.contains("Error"), "commit should succeed: {out}");
    let log = sb.git_log(5).unwrap();
    assert!(log.contains("add lib.rs"), "commit in log: {log}");
    let status = sb.git_status().unwrap();
    assert!(
        status.contains("data/"),
        "data/ should remain unstaged: {status}"
    );
}

#[test]
fn git_commit_excludes_env_files() {
    let (_tmp, sb) = git_sandbox();
    // Both a root `.env` and an env-variant like `.env.production` must be
    // left unstaged so credentials never leak into a commit.
    std::fs::write(sb.workspace.join(".env"), "SECRET=topsecret").unwrap();
    std::fs::write(sb.workspace.join(".env.production"), "KEY=anothersecret").unwrap();
    sb.write_file("src/main.rs", "fn main() {}").unwrap();
    let out = sb.git_commit("add main.rs").unwrap();
    assert!(!out.contains("Error"), "commit should succeed: {out}");
    let log = sb.git_log(5).unwrap();
    assert!(log.contains("add main.rs"), "commit in log: {log}");
    let status = sb.git_status().unwrap();
    assert!(
        status.contains(".env"),
        ".env files should remain unstaged: {status}"
    );
    assert!(
        status.contains(".env.production"),
        ".env.* files should remain unstaged: {status}"
    );
}

#[test]
fn git_commit_checkpoint_preserves_additions_but_not_deletions() {
    let (_tmp, sb) = git_sandbox();
    // Seed a tracked file, then commit it so it's in HEAD.
    sb.write_file("package-lock.json", "lockfile v1").unwrap();
    sb.git_commit("seed lockfile").unwrap();
    // Simulate a sub-agent's collateral deletion of a tracked file plus
    // its intended code work: a new file and a modified file.
    std::fs::remove_file(sb.workspace.join("package-lock.json")).unwrap();
    sb.write_file("src/server.ts", "export const server = 1;")
        .unwrap();
    sb.write_file("src/extra.ts", "export const extra = 2;")
        .unwrap();
    let out = sb
        .git_commit_checkpoint("checkpoint: uncommitted work")
        .unwrap();
    assert!(!out.contains("Error"), "checkpoint should succeed: {out}");
    // The checkpoint commit must contain the code work...
    let log = sb.git_log(5).unwrap();
    assert!(log.contains("checkpoint: uncommitted work"), "log: {log}");
    // ...but NOT the collateral deletion of the tracked file.
    let head_has_lock = sb
        .run_git(&["cat-file", "-e", "HEAD:package-lock.json"])
        .unwrap();
    assert!(
        !head_has_lock.contains("fatal"),
        "package-lock.json should still exist in HEAD after checkpoint: {head_has_lock}"
    );
    // The deletion should remain in the working tree (unstaged), so the
    // model can still decide to commit it deliberately.
    let status = sb.git_status().unwrap();
    assert!(
        status.contains("package-lock.json"),
        "deletion should remain visible in status: {status}"
    );
}

#[test]
fn git_commit_checkpoint_excludes_stray_temp_files() {
    // Finding 35 / Finding 30 repeat: a checkpoint must not sweep stray
    // untracked temp/scratch files the model's tooling drops in the
    // workspace root into the commit.
    let (_tmp, sb) = git_sandbox();
    // Seed a tracked file, then commit it so it's in HEAD.
    sb.write_file("README.md", "# raven\n").unwrap();
    sb.git_commit("seed readme").unwrap();
    // Real code work plus a stray 0-byte scratch file in the workspace root.
    sb.write_file("src/main.rs", "fn main() {}").unwrap();
    std::fs::write(sb.workspace.join("err.txt"), "").unwrap();
    let out = sb
        .git_commit_checkpoint("checkpoint: uncommitted work")
        .unwrap();
    assert!(!out.contains("Error"), "checkpoint should succeed: {out}");
    let log = sb.git_log(5).unwrap();
    assert!(log.contains("checkpoint: uncommitted work"), "log: {log}");
    // The checkpoint commit must contain the source work...
    let head_has_src = sb.run_git(&["cat-file", "-e", "HEAD:src/main.rs"]).unwrap();
    assert!(
        !head_has_src.contains("fatal"),
        "src/main.rs should be in HEAD after checkpoint: {head_has_src}"
    );
    // ...but NOT the stray err.txt.
    let head_has_err = sb.run_git(&["cat-file", "-e", "HEAD:err.txt"]).unwrap();
    assert!(
        head_has_err.contains("fatal"),
        "stray err.txt must NOT be in HEAD: {head_has_err}"
    );
    // The stray file should remain untracked afterward.
    let status = sb.git_status().unwrap();
    assert!(
        status.contains("err.txt"),
        "stray err.txt should remain untracked: {status}"
    );
}

#[test]
fn worktree_isolates_commits_between_branches() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("shared.txt", "base").unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");

    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    // Sub-agents run from the worktree but must reach the shared main
    // repo's `.git` (a sibling under the temp dir), matching the parallel
    // orchestration which sets `settings.sandbox_extra_rw`.
    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.write_file("a.txt", "work from sub-a").unwrap();
    sb_a.git_commit("sub-a: add a.txt").unwrap();

    sb_b.write_file("b.txt", "work from sub-b").unwrap();
    sb_b.git_commit("sub-b: add b.txt").unwrap();

    let log_a = sb_a.git_log(5).unwrap();
    let log_b = sb_b.git_log(5).unwrap();
    assert!(log_a.contains("sub-a: add a.txt"), "log_a: {log_a}");
    assert!(
        !log_a.contains("sub-b: add b.txt"),
        "log_a should not have sub-b commit: {log_a}"
    );
    assert!(log_b.contains("sub-b: add b.txt"), "log_b: {log_b}");
    assert!(
        !log_b.contains("sub-a: add a.txt"),
        "log_b should not have sub-a commit: {log_b}"
    );

    sb.merge_branch("raven-sub-a").unwrap();
    sb.merge_branch("raven-sub-b").unwrap();

    let main_log = sb.git_log(5).unwrap();
    assert!(
        main_log.contains("sub-a: add a.txt"),
        "main log: {main_log}"
    );
    assert!(
        main_log.contains("sub-b: add b.txt"),
        "main log: {main_log}"
    );
    assert!(sb
        .read_file("a.txt", 1, 10)
        .unwrap()
        .contains("work from sub-a"));
    assert!(sb
        .read_file("b.txt", 1, 10)
        .unwrap()
        .contains("work from sub-b"));

    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn worktree_must_be_removed_before_branch_can_be_deleted() {
    // Regression: deleting a branch while its worktree still exists fails
    // with "cannot delete branch used by worktree". The parallel sub-agent
    // cleanup must remove the worktree first, then delete the branch.
    let (_tmp, sb) = git_sandbox();
    sb.write_file("f.txt", "base").unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-a");
    sb.create_worktree("raven-sub-a", &wt_path).unwrap();

    // Deleting the branch while the worktree exists must fail, leaving the
    // branch present.
    let _ = sb.delete_branch("raven-sub-a");
    let branches = sb.run_git(&["branch", "--list", "raven-sub-a"]).unwrap();
    assert!(
        branches.contains("raven-sub-a"),
        "branch should still exist after failed delete while worktree present, got: {branches}"
    );

    // Remove the worktree first, then the branch delete succeeds.
    sb.remove_worktree(&wt_path).unwrap();
    let _ = sb.delete_branch("raven-sub-a");
    let branches_after = sb.run_git(&["branch", "--list", "raven-sub-a"]).unwrap();
    assert!(
        !branches_after.contains("raven-sub-a"),
        "branch should be gone after worktree removal + delete, got: {branches_after}"
    );
}

#[test]
fn worktree_concurrent_edits_to_same_file_are_isolated() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    let content: String = (1..=20).map(|i| format!("line{}\n", i)).collect();
    sb.write_file("shared.txt", &content).unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");

    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.search_replace("shared.txt", "line2\n", "line2-modified-by-a\n", false)
        .unwrap();
    sb_a.git_commit("sub-a: modify shared.txt").unwrap();

    sb_b.search_replace("shared.txt", "line18\n", "line18-modified-by-b\n", false)
        .unwrap();
    sb_b.git_commit("sub-b: modify shared.txt").unwrap();

    let log_a = sb_a.git_log(5).unwrap();
    let log_b = sb_b.git_log(5).unwrap();
    assert!(log_a.contains("sub-a: modify shared.txt"));
    assert!(!log_a.contains("sub-b: modify shared.txt"));
    assert!(log_b.contains("sub-b: modify shared.txt"));
    assert!(!log_b.contains("sub-a: modify shared.txt"));

    sb.merge_branch("raven-sub-a").unwrap();
    sb.merge_branch("raven-sub-b").unwrap();

    let main_log = sb.git_log(5).unwrap();
    assert!(
        main_log.contains("sub-a: modify shared.txt"),
        "main log: {main_log}"
    );
    assert!(
        main_log.contains("sub-b: modify shared.txt"),
        "main log: {main_log}"
    );

    let content = sb.read_file("shared.txt", 1, 30).unwrap();
    assert!(content.contains("line2-modified-by-a"));
    assert!(content.contains("line18-modified-by-b"));

    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn worktree_cleanup_removes_worktrees() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("f.txt", "v1").unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-x");

    sb.create_worktree("raven-sub-x", &wt_path).unwrap();
    assert!(wt_path.exists());

    sb.remove_worktree(&wt_path).unwrap();
    assert!(!wt_path.exists());
}

#[test]
fn merge_conflict_detection_after_conflicting_edits() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("shared.txt", "line1\nline2\nline3\n")
        .unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");

    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.search_replace("shared.txt", "line2\n", "line2-a\n", false)
        .unwrap();
    sb_a.git_commit("sub-a: modify line2").unwrap();

    sb_b.search_replace("shared.txt", "line2\n", "line2-b\n", false)
        .unwrap();
    sb_b.git_commit("sub-b: modify line2").unwrap();

    sb.merge_branch("raven-sub-a").unwrap();
    assert!(!sb.has_merge_conflicts().unwrap());

    let _ = sb.merge_branch("raven-sub-b");
    assert!(sb.has_merge_conflicts().unwrap());

    sb.abort_merge().unwrap();
    assert!(!sb.has_merge_conflicts().unwrap());

    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn abort_merge_restores_clean_working_tree() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("shared.txt", "line1\nline2\nline3\n")
        .unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");

    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.search_replace("shared.txt", "line2\n", "line2-a\n", false)
        .unwrap();
    sb_a.git_commit("sub-a: modify line2").unwrap();

    sb_b.search_replace("shared.txt", "line2\n", "line2-b\n", false)
        .unwrap();
    sb_b.git_commit("sub-b: modify line2").unwrap();

    sb.merge_branch("raven-sub-a").unwrap();
    let _ = sb.merge_branch("raven-sub-b");
    assert!(sb.has_merge_conflicts().unwrap());

    sb.abort_merge().unwrap();
    assert!(!sb.has_merge_conflicts().unwrap());

    let content = sb.read_file("shared.txt", 1, 10).unwrap();
    assert!(content.contains("line2-a"));
    assert!(!content.contains("<<<<<<<"));
    assert!(!content.contains("======="));
    assert!(!content.contains(">>>>>>>"));

    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn branch_diff_captures_changes_since_head() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("f.txt", "base").unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-a");
    sb.create_worktree("raven-sub-a", &wt_path).unwrap();
    let sb_a = Sandbox::with_extra_rw(wt_path, vec![main_ws.clone()]);

    sb_a.write_file("f.txt", "modified by sub-a").unwrap();
    sb_a.git_commit("sub-a: modify f.txt").unwrap();

    let diff = sb.branch_diff("raven-sub-a").unwrap();
    assert!(
        diff.contains("modified by sub-a"),
        "diff should contain the added line: {diff}"
    );

    sb.delete_branch("raven-sub-a").unwrap();
}

#[test]
fn branch_diff_empty_when_no_changes() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("f.txt", "base").unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-a");
    sb.create_worktree("raven-sub-a", &wt_path).unwrap();

    let diff = sb.branch_diff("raven-sub-a").unwrap();
    assert!(
        diff.trim().is_empty() || diff.trim() == "exit=0",
        "diff should be empty, got: {diff:?}"
    );

    sb.delete_branch("raven-sub-a").unwrap();
}

#[test]
fn auto_commit_preserves_uncommitted_worktree_changes() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("f.txt", "base").unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-a");
    sb.create_worktree("raven-sub-a", &wt_path).unwrap();
    let sb_a = Sandbox::with_extra_rw(wt_path.clone(), vec![main_ws.clone()]);

    sb_a.write_file("f.txt", "uncommitted work").unwrap();
    assert!(!sb_a.is_working_tree_clean());

    sb_a.git_commit("checkpoint: uncommitted work from sub-agent 0")
        .unwrap();
    assert!(sb_a.is_working_tree_clean());

    sb.merge_branch("raven-sub-a").unwrap();
    let content = sb.read_file("f.txt", 1, 10).unwrap();
    assert!(content.contains("uncommitted work"), "content: {content}");

    sb.delete_branch("raven-sub-a").unwrap();
}

#[test]
fn recovery_patch_written_on_merge_conflict() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("shared.txt", "line1\nline2\nline3\n")
        .unwrap();
    sb.git_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");

    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.search_replace("shared.txt", "line2\n", "line2-a\n", false)
        .unwrap();
    sb_a.git_commit("sub-a: modify line2").unwrap();

    sb_b.search_replace("shared.txt", "line2\n", "line2-b\n", false)
        .unwrap();
    sb_b.git_commit("sub-b: modify line2").unwrap();

    sb.merge_branch("raven-sub-a").unwrap();
    let _ = sb.merge_branch("raven-sub-b");
    assert!(sb.has_merge_conflicts().unwrap());

    let diff = sb.branch_diff("raven-sub-b").unwrap();
    assert!(
        diff.contains("line2-b"),
        "branch_diff should capture sub-b changes: {diff}"
    );

    let patch_path = main_ws.join(".raven/recovery-sub-1.patch");
    let _ = std::fs::create_dir_all(patch_path.parent().unwrap());
    std::fs::write(&patch_path, &diff).unwrap();
    assert!(patch_path.exists());

    sb.abort_merge().unwrap();
    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}
