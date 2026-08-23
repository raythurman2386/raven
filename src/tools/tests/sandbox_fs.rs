//! Tests for sandbox path resolution and workspace-scoped file operations
//! (`read_file`, `write_file`, `search_replace`, `grep`, globs, `list_dir`).

use crate::tools::sandbox::{truncate_output, OpenFlags};
use crate::tools::{glob_segment_match, Sandbox};
use std::io::Write;

use super::sandbox;

#[test]
fn safe_resolve_rejects_traversal() {
    let sb = sandbox();
    let _ = sb.write_file("../../escaped.txt", "data");
    assert!(
        !std::path::Path::new("/tmp/escaped.txt").exists(),
        "file should not be created outside workspace"
    );
    let safe_result = sb.safe_resolve("subdir/../../escaped.txt");
    assert!(
        safe_result.is_err(),
        "traversal should be rejected: {:?}",
        safe_result
    );
}

#[test]
fn safe_resolve_allows_within_workspace() {
    let sb = sandbox();
    let result = sb.safe_resolve("src/main.rs");
    assert!(result.is_ok());
}

#[test]
fn safe_resolve_rejects_absolute_outside() {
    let sb = sandbox();
    let result = sb.safe_resolve("/etc/passwd");
    assert!(result.is_err());
}

#[test]
#[cfg(windows)]
fn write_file_rejects_windows_absolute_outside() {
    let sb = sandbox();
    let probe = r"C:\Windows\Temp\raven_eval_win_escape.txt";
    let _ = std::fs::remove_file(probe);
    let res = sb.write_file(probe, "pwned");
    let msg = match res {
        Ok(s) => s,
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("outside") || msg.contains("Error") || msg.contains("relative"),
        "absolute Windows path must be rejected: {msg}"
    );
    assert!(
        !std::path::Path::new(probe).exists(),
        "file tool must not create {probe}"
    );
}

#[test]
fn safe_resolve_blocks_symlink_escape_write() {
    let tmp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let ws = tmp.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), ws.join("evil")).unwrap();
        let sb = Sandbox::new(ws.clone());
        let res = sb.write_file("evil/escaped.txt", "pwned");
        assert!(
            res.is_err() || !res.unwrap().contains("Wrote"),
            "write through symlink should be rejected"
        );
        assert!(
            !outside.path().join("escaped.txt").exists(),
            "file must not be written outside the workspace"
        );
    }
    let _ = tmp;
}

#[test]
fn safe_resolve_blocks_symlink_escape_read() {
    let tmp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
    #[cfg(unix)]
    let ws = tmp.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), ws.join("evil")).unwrap();
        let sb = Sandbox::new(ws.clone());
        let res = sb.read_file("evil/secret.txt", 1, 100);
        assert!(
            res.is_err() || !res.unwrap().contains("top secret"),
            "read through symlink should be rejected"
        );
    }
    let _ = tmp;
}

#[test]
fn open_beneath_rejects_traversal() {
    let sb = sandbox();
    let res = sb.open_beneath(
        "../../escaped.txt",
        OpenFlags::RDONLY | OpenFlags::CLOEXEC,
        0,
    );
    assert!(
        res.is_err(),
        "open_beneath should reject traversal: {:?}",
        res
    );
}

#[test]
fn open_beneath_rejects_absolute_outside() {
    let sb = sandbox();
    let res = sb.open_beneath("/etc/passwd", OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0);
    assert!(
        res.is_err(),
        "open_beneath should reject absolute paths outside workspace: {:?}",
        res
    );
}

#[test]
fn open_beneath_blocks_symlink_escape_read() {
    let tmp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    std::fs::write(outside.path().join("secret.txt"), "top secret").unwrap();
    #[cfg(unix)]
    let ws = tmp.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), ws.join("evil")).unwrap();
        let sb = Sandbox::new(ws.clone());
        let res = sb.open_beneath("evil/secret.txt", OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0);
        assert!(
            res.is_err(),
            "open_beneath should reject symlink escape: {:?}",
            res
        );
    }
    let _ = tmp;
}

#[test]
fn open_beneath_blocks_symlink_escape_write() {
    let tmp = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let outside = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let ws = tmp.path().canonicalize().unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), ws.join("evil")).unwrap();
        let sb = Sandbox::new(ws.clone());
        let res = sb.open_beneath(
            "evil/escaped.txt",
            OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
            0o644,
        );
        assert!(
            res.is_err(),
            "open_beneath should reject symlink escape on write: {:?}",
            res
        );
        assert!(
            !outside.path().join("escaped.txt").exists(),
            "file must not be written outside the workspace"
        );
    }
    let _ = tmp;
}

#[test]
fn open_beneath_reads_within_workspace() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), "hello").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let file = sb
        .open_beneath("a.txt", OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0)
        .unwrap();
    let content = std::io::read_to_string(file).unwrap();
    assert_eq!(content, "hello");
}

#[test]
fn list_dir_shows_contents() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn main() {}").unwrap();
    std::fs::create_dir(tmp.path().join("src")).unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.list_dir(".").unwrap();
    assert!(out.contains("a.rs"));
    assert!(out.contains("src"));
}

#[test]
fn list_dir_nonexistent_returns_error() {
    let sb = sandbox();
    let out = sb.list_dir("does_not_exist").unwrap();
    assert!(out.contains("Error"));
}

#[test]
fn list_dir_on_file_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.txt"), "hello").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.list_dir("file.txt").unwrap();
    assert!(out.contains("Error"));
}

#[test]
fn list_dir_empty_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.list_dir(".").unwrap();
    assert_eq!(out, "(empty)");
}

#[test]
fn read_file_returns_content() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.read_file("test.txt", 1, 100).unwrap();
    assert!(out.contains("line1"));
    assert!(out.contains("lines 1-3 of 3"));
}

#[test]
fn read_file_line_range() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\nd\ne\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.read_file("test.txt", 2, 2).unwrap();
    assert!(out.contains("lines 2-3 of 5"));
}

#[test]
fn read_file_nonexistent_returns_error() {
    let sb = sandbox();
    let out = sb.read_file("nonexistent.txt", 1, 100).unwrap();
    assert!(out.contains("Error"));
}

#[test]
fn write_file_creates_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.write_file("new.txt", "content here").unwrap();
    assert!(out.contains("Wrote"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
        "content here"
    );
}

#[test]
fn write_file_creates_parent_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    sb.write_file("src/deep/nested.rs", "fn main() {}").unwrap();
    assert!(tmp.path().join("src/deep/nested.rs").exists());
}

#[test]
fn search_replace_unique_match() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "old line\nother\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb
        .search_replace("file.rs", "old line", "new line", false)
        .unwrap();
    assert!(out.contains("Edited"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
        "new line\nother\n"
    );
}

#[test]
fn search_replace_not_unique_returns_error() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "dup\ndup\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb
        .search_replace("file.rs", "dup", "unique", false)
        .unwrap();
    assert!(out.contains("not unique"));
}

#[test]
fn search_replace_all() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "dup\ndup\ndup\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.search_replace("file.rs", "dup", "x", true).unwrap();
    assert!(out.contains("Replaced 3"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
        "x\nx\nx\n"
    );
}

#[test]
fn search_replace_all_exceeds_threshold_warns() {
    let tmp = tempfile::tempdir().unwrap();
    let content = "dup\n".repeat(25);
    std::fs::write(tmp.path().join("file.rs"), &content).unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.search_replace("file.rs", "dup", "x", true).unwrap();
    // The write must happen even when the count exceeds the threshold —
    // previously this returned a warning without writing, causing the
    // agent to think the edit succeeded when nothing changed.
    assert!(out.contains("Replaced"), "should report replacement: {out}");
    assert!(out.contains("warning"), "should include warning: {out}");
    assert!(out.contains("25"), "should mention count: {out}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("file.rs")).unwrap(),
        "x\n".repeat(25),
        "file must be modified"
    );
}

#[test]
fn search_replace_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("file.rs"), "content\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.search_replace("file.rs", "missing", "x", false).unwrap();
    assert!(out.contains("not found"));
}

#[test]
fn search_replace_empty_old_creates_file() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb
        .search_replace("new.txt", "", "new content", false)
        .unwrap();
    assert!(out.contains("Created"));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("new.txt")).unwrap(),
        "new content"
    );
}

#[test]
fn grep_finds_matches() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn hello() {}\nfn world() {}\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.grep("hello", "", None, 10).unwrap();
    assert!(out.contains("a.rs"), "output: {}", out);
}

#[test]
fn grep_no_matches() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.rs"), "fn foo() {}\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.grep("nonexistent_pattern", "", None, 10).unwrap();
    assert!(out.contains("No matches"));
}

#[test]
fn grep_respects_max_results() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("a.rs"),
        "match\nmatch\nmatch\nmatch\nmatch\n",
    )
    .unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.grep("match", "", None, 2).unwrap();
    assert_eq!(out.lines().count(), 2, "output: {}", out);
}

#[test]
fn grep_skips_hidden_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".git")).unwrap();
    std::fs::write(tmp.path().join(".git/config"), "match_in_git\n").unwrap();
    std::fs::write(tmp.path().join("visible.rs"), "match_visible\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.grep("match", "", None, 10).unwrap();
    assert!(out.contains("visible.rs"), "output: {}", out);
    assert!(!out.contains(".git/config"), "output: {}", out);
}

#[test]
fn read_file_extracts_docx() {
    // End-to-end: read_file on a .docx must return extracted text, not
    // "Tool error: read file". Regression for the relative-vs-absolute
    // path bug where extraction read against the process CWD.
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().canonicalize().unwrap();
    let docx = ws.join("report.docx");

    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    zip.start_file("[Content_Types].xml", opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"#,
    )
    .unwrap();
    zip.start_file("_rels/.rels", opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"#,
    )
    .unwrap();
    zip.start_file("word/document.xml", opts).unwrap();
    zip.write_all(
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Hello from read_file</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
    )
    .unwrap();
    let cursor = zip.finish().unwrap();
    std::fs::write(&docx, cursor.into_inner()).unwrap();

    let sb = Sandbox::new(ws);
    let out = sb.read_file("report.docx", 1, 100).unwrap();
    assert!(out.contains("Hello from read_file"), "output: {out}");
    assert!(out.contains("extracted document"), "output: {out}");
}

#[test]
fn read_file_rejects_binary() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("img.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.read_file("img.png", 1, 100).unwrap();
    assert!(out.contains("binary file"), "output: {out}");
}

#[test]
fn glob_matches_star() {
    assert!(glob_segment_match("main.rs", "*.rs"));
    assert!(glob_segment_match("main.go", "*.go"));
    assert!(!glob_segment_match("main.rs", "*.go"));
}

#[test]
fn glob_matches_question() {
    assert!(glob_segment_match("a.rs", "?.rs"));
    assert!(!glob_segment_match("ab.rs", "?.rs"));
}

#[test]
fn truncate_output_is_char_safe() {
    let s = "héllo wörld — café — naïve";
    let out = truncate_output(s, 10);
    assert!(out.contains("[truncated at 10 chars]"));
    assert!(out.chars().count() > 10);
    assert!(out.starts_with(&s.chars().take(10).collect::<String>()));
}

#[test]
fn truncate_output_short_unmodified() {
    let s = "short";
    assert_eq!(truncate_output(s, 100), s);
}

#[test]
fn read_file_caps_output_at_max_tool_output() {
    let tmp = tempfile::tempdir().unwrap();
    let big: String = (0..20_000)
        .map(|i| format!("line {i} {}\n", "x".repeat(60)))
        .collect();
    std::fs::write(tmp.path().join("big.txt"), &big).unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.read_file("big.txt", 1, 100_000).unwrap();
    assert!(
        out.chars().count() <= 12064,
        "read_file output should be capped, got {} chars",
        out.chars().count()
    );
    assert!(out.contains("truncated at"), "missing truncation marker");
}

#[test]
fn write_file_dotdot_cancels_missing_intermediate_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.write_file("newdir/../f.txt", "hello").unwrap();
    assert!(out.contains("Wrote"), "write should succeed: {out}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
        "hello"
    );
    assert!(
        !tmp.path().join("newdir").exists(),
        "intermediate dir should not be created"
    );
}

#[test]
fn search_replace_empty_old_dotdot_cancels_missing_intermediate_dir() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb
        .search_replace("newdir/../f.txt", "", "hello", false)
        .unwrap();
    assert!(out.contains("Created"), "create should succeed: {out}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
        "hello"
    );
    assert!(
        !tmp.path().join("newdir").exists(),
        "intermediate dir should not be created"
    );
}

#[test]
fn search_replace_dotdot_on_existing_file() {
    // Regression for #104/#108: the non-empty old_string write path in
    // search_replace passed the raw path to openat2 (RESOLVE_BENEATH),
    // which fails on `..`-paths when the intermediate dir doesn't exist.
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    sb.write_file("f.txt", "hello world").unwrap();
    let out = sb
        .search_replace("newdir/../f.txt", "hello", "goodbye", false)
        .unwrap();
    assert!(out.contains("Edited"), "edit should succeed: {out}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
        "goodbye world"
    );
}

#[test]
fn search_replace_dotdot_replace_all() {
    // Same regression, replace_all path.
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    sb.write_file("f.txt", "a a a").unwrap();
    let out = sb
        .search_replace("newdir/../f.txt", "a", "b", true)
        .unwrap();
    assert!(
        out.contains("Replaced"),
        "replace_all should succeed: {out}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("f.txt")).unwrap(),
        "b b b"
    );
}
