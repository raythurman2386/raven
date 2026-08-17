//! Tests for unified-diff parsing and `Sandbox::apply_patch`.

use crate::tools::patch::parse_unified_diff;
use crate::tools::Sandbox;

use super::sandbox;

#[test]
fn parse_unified_diff_basic() {
    let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,2 +1,2 @@\n line1\n-old\n+new\n line3\n";
    let hunks = parse_unified_diff(patch);
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].file_path, "file.rs");
    assert_eq!(hunks[0].lines.len(), 4);
}

#[test]
fn apply_patch_modifies_file() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "line1\nold\nline3\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
    let result = sb.apply_patch(patch).unwrap();
    assert!(result.contains("Patched"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
        "line1\nnew\nline3\n"
    );
}

#[test]
fn apply_patch_creates_backup() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "line1\nold\nline3\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
    let result = sb.apply_patch(patch).unwrap();
    assert!(result.contains("Patched"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("file.rs.bak")).unwrap(),
        "line1\nold\nline3\n"
    );
}

#[test]
fn apply_patch_creates_backup_no_extension() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("Makefile"), "line1\nold\nline3\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let patch = "--- a/Makefile\n+++ b/Makefile\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
    let result = sb.apply_patch(patch).unwrap();
    assert!(result.contains("Patched"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("Makefile.bak")).unwrap(),
        "line1\nold\nline3\n"
    );
}

#[test]
fn apply_patch_rejects_context_mismatch() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "line1\nWRONG\nline3\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let patch = "--- a/file.rs\n+++ b/file.rs\n@@ -1,3 +1,3 @@\n line1\n-old\n+new\n line3\n";
    let result = sb.apply_patch(patch).unwrap();
    assert!(result.contains("Error"));
    assert!(result.contains("mismatch"));
}

#[test]
fn apply_patch_empty_returns_error() {
    let sb = sandbox();
    let result = sb.apply_patch("").unwrap();
    assert!(result.contains("no valid hunks"));
}
