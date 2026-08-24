//! Tests for git inspect tools, worktree isolation, and patch-apply merge.

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
fn git_status_reports_untracked_and_clean() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("a.txt", "v1").unwrap();
    sb.test_commit("seed").unwrap();
    assert!(sb.git_status().unwrap().contains("No changes"));
    sb.write_file("b.txt", "v2").unwrap();
    let status = sb.git_status().unwrap();
    assert!(status.contains("b.txt"), "status: {status}");
}

#[test]
fn export_diff_from_includes_uncommitted_work_not_env() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("src/lib.rs", "pub fn id() {}\n").unwrap();
    sb.test_commit("seed").unwrap();
    let base = sb.rev_parse_head().unwrap();

    sb.write_file("src/lib.rs", "pub fn square() {}\n").unwrap();
    sb.write_file("src/new.rs", "pub fn new() {}\n").unwrap();
    std::fs::write(sb.workspace.join(".env"), "SECRET=nope\n").unwrap();
    std::fs::create_dir_all(sb.workspace.join(".raven")).unwrap();
    std::fs::write(sb.workspace.join(".raven/skip.txt"), "skip").unwrap();

    let diff = sb.export_diff_from(&base).unwrap();
    assert!(diff.contains("square"), "diff should include edit: {diff}");
    assert!(
        diff.contains("new.rs"),
        "diff should include new file: {diff}"
    );
    assert!(
        !diff.contains("SECRET=nope"),
        ".env must be excluded: {diff}"
    );
    assert!(
        !diff.contains("skip.txt"),
        ".raven/ must be excluded: {diff}"
    );
    assert!(!sb.is_working_tree_clean(), "export must not commit");
}

#[test]
fn apply_git_patch_does_not_create_a_commit() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("f.txt", "base\n").unwrap();
    sb.test_commit("initial").unwrap();
    let log_before = sb.git_log(5).unwrap();
    let base = sb.rev_parse_head().unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-a");
    sb.create_worktree("raven-sub-a", &wt_path).unwrap();
    let sb_a = Sandbox::with_extra_rw(wt_path, vec![main_ws]);

    sb_a.write_file("f.txt", "from sub-a\n").unwrap();
    sb_a.write_file("a.txt", "added\n").unwrap();
    let patch = sb_a.export_diff_from(&base).unwrap();
    assert!(!patch.trim().is_empty(), "expected a non-empty patch");

    let out = sb.apply_git_patch(&patch).unwrap();
    assert!(
        out.contains("applied") || !out.contains("fatal"),
        "apply: {out}"
    );

    let content = sb.read_file("f.txt", 1, 10).unwrap();
    assert!(content.contains("from sub-a"), "content: {content}");
    let added = sb.read_file("a.txt", 1, 10).unwrap();
    assert!(added.contains("added"), "added: {added}");

    let log_after = sb.git_log(5).unwrap();
    assert_eq!(log_before, log_after, "apply must not create a commit");
    assert!(!sb.is_working_tree_clean(), "parent tree should stay dirty");

    sb.remove_worktree(&wt_dir.path().join("sub-a")).unwrap();
    sb.delete_branch("raven-sub-a").unwrap();
}

#[test]
fn worktree_isolates_uncommitted_edits() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("shared.txt", "base\n").unwrap();
    sb.test_commit("initial").unwrap();
    let base = sb.rev_parse_head().unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");
    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.write_file("a.txt", "work from sub-a").unwrap();
    sb_b.write_file("b.txt", "work from sub-b").unwrap();

    assert!(
        !sb.workspace.join("a.txt").exists(),
        "parent must not see sub-a files before apply"
    );
    assert!(!sb_a.workspace.join("b.txt").exists());
    assert!(!sb_b.workspace.join("a.txt").exists());

    let patch_a = sb_a.export_diff_from(&base).unwrap();
    let patch_b = sb_b.export_diff_from(&base).unwrap();
    sb.apply_git_patch(&patch_a).unwrap();
    sb.apply_git_patch(&patch_b).unwrap();

    assert!(sb
        .read_file("a.txt", 1, 10)
        .unwrap()
        .contains("work from sub-a"));
    assert!(sb
        .read_file("b.txt", 1, 10)
        .unwrap()
        .contains("work from sub-b"));

    sb.remove_worktree(&wt_dir.path().join("sub-a")).unwrap();
    sb.remove_worktree(&wt_dir.path().join("sub-b")).unwrap();
    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn worktree_must_be_removed_before_branch_can_be_deleted() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("f.txt", "base").unwrap();
    sb.test_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-a");
    sb.create_worktree("raven-sub-a", &wt_path).unwrap();

    let _ = sb.delete_branch("raven-sub-a");
    let branches = sb.run_git(&["branch", "--list", "raven-sub-a"]).unwrap();
    assert!(
        branches.contains("raven-sub-a"),
        "branch should still exist after failed delete while worktree present, got: {branches}"
    );

    sb.remove_worktree(&wt_path).unwrap();
    let _ = sb.delete_branch("raven-sub-a");
    let branches_after = sb.run_git(&["branch", "--list", "raven-sub-a"]).unwrap();
    assert!(
        !branches_after.contains("raven-sub-a"),
        "branch should be gone after worktree removal + delete, got: {branches_after}"
    );
}

#[test]
fn disjoint_hunks_apply_without_commit() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    let content: String = (1..=20).map(|i| format!("line{}\n", i)).collect();
    sb.write_file("shared.txt", &content).unwrap();
    sb.test_commit("initial").unwrap();
    let log_before = sb.git_log(5).unwrap();
    let base = sb.rev_parse_head().unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");
    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.search_replace("shared.txt", "line2\n", "line2-modified-by-a\n", false)
        .unwrap();
    sb_b.search_replace("shared.txt", "line18\n", "line18-modified-by-b\n", false)
        .unwrap();

    let patch_a = sb_a.export_diff_from(&base).unwrap();
    let patch_b = sb_b.export_diff_from(&base).unwrap();
    sb.apply_git_patch(&patch_a).unwrap();
    sb.apply_git_patch(&patch_b).unwrap();

    let content = sb.read_file("shared.txt", 1, 30).unwrap();
    assert!(content.contains("line2-modified-by-a"));
    assert!(content.contains("line18-modified-by-b"));
    assert_eq!(log_before, sb.git_log(5).unwrap());

    sb.remove_worktree(&wt_dir.path().join("sub-a")).unwrap();
    sb.remove_worktree(&wt_dir.path().join("sub-b")).unwrap();
    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn worktree_cleanup_removes_worktrees() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("f.txt", "v1").unwrap();
    sb.test_commit("initial").unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path = wt_dir.path().join("sub-x");

    sb.create_worktree("raven-sub-x", &wt_path).unwrap();
    assert!(wt_path.exists());

    sb.remove_worktree(&wt_path).unwrap();
    assert!(!wt_path.exists());
}

#[test]
fn conflicting_patches_fail_apply_without_commit() {
    let (_tmp, sb) = git_sandbox();
    let main_ws = sb.workspace.clone();
    sb.write_file("shared.txt", "line1\nline2\nline3\n")
        .unwrap();
    sb.test_commit("initial").unwrap();
    let log_before = sb.git_log(5).unwrap();
    let base = sb.rev_parse_head().unwrap();

    let wt_dir = tempfile::tempdir().unwrap();
    let wt_path_a = wt_dir.path().join("sub-a");
    let wt_path_b = wt_dir.path().join("sub-b");
    sb.create_worktree("raven-sub-a", &wt_path_a).unwrap();
    sb.create_worktree("raven-sub-b", &wt_path_b).unwrap();

    let sb_a = Sandbox::with_extra_rw(wt_path_a, vec![main_ws.clone()]);
    let sb_b = Sandbox::with_extra_rw(wt_path_b, vec![main_ws.clone()]);

    sb_a.search_replace("shared.txt", "line2\n", "line2-a\n", false)
        .unwrap();
    sb_b.search_replace("shared.txt", "line2\n", "line2-b\n", false)
        .unwrap();

    let patch_a = sb_a.export_diff_from(&base).unwrap();
    let patch_b = sb_b.export_diff_from(&base).unwrap();
    sb.apply_git_patch(&patch_a).unwrap();
    let err = sb.apply_git_patch(&patch_b).unwrap_err().to_string();
    assert!(
        err.contains("does not apply") || err.contains("patch"),
        "second apply should fail: {err}"
    );
    assert_eq!(log_before, sb.git_log(5).unwrap());
    let content = sb.read_file("shared.txt", 1, 10).unwrap();
    assert!(content.contains("line2-a"));
    assert!(!content.contains("line2-b"));

    sb.remove_worktree(&wt_dir.path().join("sub-a")).unwrap();
    sb.remove_worktree(&wt_dir.path().join("sub-b")).unwrap();
    sb.delete_branch("raven-sub-a").unwrap();
    sb.delete_branch("raven-sub-b").unwrap();
}

#[test]
fn apply_empty_patch_is_noop() {
    let (_tmp, sb) = git_sandbox();
    sb.write_file("f.txt", "base").unwrap();
    sb.test_commit("initial").unwrap();
    let out = sb.apply_git_patch("").unwrap();
    assert!(out.contains("no changes"), "{out}");
    let out = sb.apply_git_patch("exit=0").unwrap();
    assert!(out.contains("no changes"), "{out}");
}
