//! Unit tests for the repo symbol map: extraction, scoring, ranking, rendering,
//! caching, and walk caps.

use super::*;

fn write_rs(dir: &std::path::Path, name: &str, body: &str) {
    let path = dir.join(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

#[test]
fn small_workspace_skips_map() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!should_build(tmp.path()));
    assert!(build_map(tmp.path()).is_none());
}

#[test]
fn extracts_rust_symbols() {
    let tmp = tempfile::tempdir().unwrap();
    write_rs(
        tmp.path(),
        "a.rs",
        "pub fn foo() {}\nstruct Bar {}\nenum Baz {}\nimpl Bar {}\n",
    );
    assert!(!should_build(tmp.path()), "1 file is below MIN_FILES");
    // Force map even for small workspace by calling extract directly.
    let mut syms = Vec::new();
    extract_symbols(
        "pub fn foo() {}\nstruct Bar {}\nenum Baz {}\nimpl Bar {}\n",
        "rs",
        &tmp.path().join("a.rs"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"foo".to_string()));
    assert!(names.contains(&"Bar".to_string()));
    assert!(names.contains(&"Baz".to_string()));
}

#[test]
fn extracts_python_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "def hello():\n    pass\n\nclass World:\n    pass\n",
        "py",
        std::path::Path::new("m.py"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"hello".to_string()));
    assert!(names.contains(&"World".to_string()));
}

#[test]
fn extracts_ts_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "export function greet(name) {}\nexport const PI = 3.14;\ninterface Shape {}\n",
        "ts",
        std::path::Path::new("x.ts"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"greet".to_string()));
    assert!(names.contains(&"PI".to_string()));
    assert!(names.contains(&"Shape".to_string()));
}

#[test]
fn extracts_js_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "function greet(name) {}\nconst PI = 3.14;\nclass Shape {}\n",
        "js",
        std::path::Path::new("x.js"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"greet".to_string()));
    assert!(names.contains(&"PI".to_string()));
    assert!(names.contains(&"Shape".to_string()));
}

#[test]
fn js_export_scores_higher_than_non_export_on_same_path() {
    let mut syms = Vec::new();
    extract_symbols(
        "export function greet(name) {}\nfunction helper() {}\n",
        "ts",
        std::path::Path::new("src/module.ts"),
        &mut syms,
    );
    let rel = std::path::Path::new("src/module.ts");
    for s in &mut syms {
        s.score = score_symbol(&s.name, s.kind, s.public, rel);
    }
    let greet = syms.iter().find(|s| s.name == "greet").unwrap();
    let helper = syms.iter().find(|s| s.name == "helper").unwrap();
    assert!(greet.public, "exported function should be public");
    assert!(!helper.public, "non-exported function should not be public");
    assert!(
        greet.score > helper.score,
        "exported symbol (score {}) should outrank non-exported (score {}) on same path",
        greet.score,
        helper.score
    );
}

#[test]
fn js_export_not_fooled_by_midline_export() {
    let mut syms = Vec::new();
    extract_symbols(
        "const x = someExport(foo);\nfunction real() {}\n",
        "js",
        std::path::Path::new("x.js"),
        &mut syms,
    );
    let x_sym = syms.iter().find(|s| s.name == "x");
    assert!(x_sym.is_some(), "const x should be extracted");
    assert!(
        !x_sym.unwrap().public,
        "midline 'export' should not mark as public"
    );
}

#[test]
fn extracts_go_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "func Hello() {\n}\n\ntype Foo struct {\n}\n",
        "go",
        std::path::Path::new("m.go"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"Hello".to_string()));
    assert!(names.contains(&"Foo".to_string()));
}

#[test]
fn extracts_c_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
            "int main(int argc, char** argv) {\n}\n\nclass Foo {\n};\nstruct Bar {\n};\nenum Baz {\n};\n",
            "cpp",
            std::path::Path::new("m.cpp"),
            &mut syms,
        );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"main".to_string()));
    assert!(names.contains(&"Foo".to_string()));
    assert!(names.contains(&"Bar".to_string()));
    assert!(names.contains(&"Baz".to_string()));
}

#[test]
fn extracts_ruby_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "def hello\nend\n\nclass World\nend\n",
        "rb",
        std::path::Path::new("m.rb"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"hello".to_string()));
    assert!(names.contains(&"World".to_string()));
}

#[test]
fn extracts_php_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "function hello() {\n}\n\nclass World {\n}\n",
        "php",
        std::path::Path::new("m.php"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"hello".to_string()));
    assert!(names.contains(&"World".to_string()));
}

#[test]
fn extracts_swift_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "func greet() {\n}\n\nclass Person {\n}\n",
        "swift",
        std::path::Path::new("m.swift"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"greet".to_string()));
    assert!(names.contains(&"Person".to_string()));
}

#[test]
fn extracts_kotlin_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "fun greet() {\n}\n\nclass Person {\n}\n",
        "kt",
        std::path::Path::new("m.kt"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"greet".to_string()));
    assert!(names.contains(&"Person".to_string()));
}

#[test]
fn extracts_csharp_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "void Greet() {\n}\n\nclass Person {\n}\n",
        "cs",
        std::path::Path::new("m.cs"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"Greet".to_string()));
    assert!(names.contains(&"Person".to_string()));
}

#[test]
fn extracts_shell_symbols() {
    let mut syms = Vec::new();
    extract_symbols(
        "hello() {\n}\n\nfunction world {\n}\n",
        "sh",
        std::path::Path::new("m.sh"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"hello".to_string()));
    assert!(names.contains(&"world".to_string()));
}

#[test]
fn map_caps_at_max_chars() {
    let tmp = tempfile::tempdir().unwrap();
    // Create enough files to cross MIN_SOURCE_FILES so build_map runs.
    for f in 0..60 {
        write_rs(
            tmp.path(),
            &format!("f{f}.rs"),
            &format!("pub fn fn_{f}() {{}}\n"),
        );
    }
    assert!(should_build(tmp.path()), "workspace should be large enough");
    let map = build_map(tmp.path()).expect("map should build");
    assert!(map.starts_with("<repo_map>"));
    assert!(map.ends_with("</repo_map>"));
    assert!(map.chars().count() <= MAX_MAP_CHARS + 40);
    assert!(map.contains("fn_0"));
}

#[test]
fn rust_patterns_skip_comments_and_strings() {
    let mut syms = Vec::new();
    extract_symbols(
            "// pub fn commented_out() {}\n/* fn block_comment() {} */\nlet s = \"fn not_a_fn() {}\";\npub fn real_fn() {}\n",
            "rs",
            std::path::Path::new("test.rs"),
            &mut syms,
        );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"real_fn".to_string()));
    assert!(!names.contains(&"commented_out".to_string()));
    assert!(!names.contains(&"block_comment".to_string()));
    assert!(!names.contains(&"not_a_fn".to_string()));
}

#[test]
fn js_patterns_skip_comments_and_strings() {
    let mut syms = Vec::new();
    extract_symbols(
        "// function commented() {}\nconst s = 'function inString() {}';\nfunction real() {}\n",
        "js",
        std::path::Path::new("test.js"),
        &mut syms,
    );
    let names: Vec<String> = syms.iter().map(|s| s.name.clone()).collect();
    assert!(names.contains(&"real".to_string()));
    assert!(!names.contains(&"commented".to_string()));
    assert!(!names.contains(&"inString".to_string()));
}

#[test]
fn skips_vendor_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    let nm = tmp.path().join("node_modules");
    std::fs::create_dir_all(&nm).unwrap();
    std::fs::write(nm.join("x.js"), "function foo() {}").unwrap();
    // node_modules should not contribute.
    assert_eq!(count_source_files(tmp.path()), 0);
}

#[test]
fn skips_new_skip_dirs() {
    let tmp = tempfile::tempdir().unwrap();
    for d in [
        ".next",
        "coverage",
        "vendor",
        "Pods",
        ".turbo",
        "target-local",
        ".tmp",
    ] {
        let dir = tmp.path().join(d);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.rs"), "pub fn foo() {}\n").unwrap();
    }
    assert_eq!(count_source_files(tmp.path()), 0);
}

#[test]
fn respects_gitignore_without_git_repo() {
    // No `.git` → ignore-crate walk still applies `.gitignore`.
    let tmp = tempfile::tempdir().unwrap();
    write_rs(tmp.path(), "src/lib.rs", "pub fn keep_me() {}\n");
    std::fs::create_dir_all(tmp.path().join("build_out")).unwrap();
    std::fs::write(
        tmp.path().join("build_out/generated.rs"),
        "pub fn ignored_symbol_xyz() {}\n",
    )
    .unwrap();
    std::fs::write(tmp.path().join(".gitignore"), "build_out/\n").unwrap();
    for f in 0..15 {
        write_rs(tmp.path(), &format!("src/f{f}.rs"), "pub fn f() {}\n");
    }
    let files = collect_source_files(tmp.path());
    assert!(
        files
            .iter()
            .all(|p| !p.to_string_lossy().contains("build_out")),
        "gitignore'd build_out must not be listed: {files:?}"
    );
    let map = build_map(tmp.path()).expect("map");
    assert!(!map.contains("ignored_symbol_xyz"));
    assert!(map.contains("keep_me") || map.contains("src/"));
}

#[test]
fn respects_gitignore_via_git_ls_files() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(root)
        .output()
        .expect("git init");
    // Avoid depending on the user's global template / identity.
    let _ = std::process::Command::new("git")
        .args(["config", "user.email", "repomap@test"])
        .current_dir(root)
        .output();
    let _ = std::process::Command::new("git")
        .args(["config", "user.name", "repomap"])
        .current_dir(root)
        .output();
    write_rs(root, "main.rs", "pub fn entry() {}\n");
    std::fs::create_dir_all(root.join("secret_gen")).unwrap();
    std::fs::write(
        root.join("secret_gen/x.rs"),
        "pub fn must_not_appear_abc() {}\n",
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), "secret_gen/\n").unwrap();
    for f in 0..15 {
        write_rs(root, &format!("m{f}.rs"), "pub fn m() {}\n");
    }
    // Stage something so ls-files has an index; untracked non-ignored still listed.
    let _ = std::process::Command::new("git")
        .args(["add", "main.rs", ".gitignore"])
        .current_dir(root)
        .output();
    let files = collect_source_files(root);
    assert!(
        git_list_sources(root).is_some(),
        "expected git listing path"
    );
    assert!(
        files
            .iter()
            .all(|p| !p.to_string_lossy().contains("secret_gen")),
        "git exclude-standard must drop secret_gen: {files:?}"
    );
}

#[test]
fn collect_prioritizes_entrypoints_over_deep_tests() {
    let tmp = tempfile::tempdir().unwrap();
    write_rs(tmp.path(), "src/main.rs", "pub fn main() {}\n");
    write_rs(
        tmp.path(),
        "src/deep/tests/helpers.rs",
        "fn test_helper() {}\n",
    );
    for f in 0..5 {
        write_rs(tmp.path(), &format!("src/util{f}.rs"), "pub fn u() {}\n");
    }
    let files = collect_source_files(tmp.path());
    let first = files[0].strip_prefix(tmp.path()).unwrap();
    assert!(
        first.ends_with("main.rs"),
        "entrypoint should sort first, got {}",
        first.display()
    );
    assert!(
        score_path(Path::new("src/main.rs")) > score_path(Path::new("src/deep/tests/helpers.rs"))
    );
}

#[test]
fn skips_large_generated_files() {
    let tmp = tempfile::tempdir().unwrap();
    // One small file to cross MIN_SOURCE_FILES, plus one huge generated file.
    for f in 0..15 {
        write_rs(tmp.path(), &format!("f{f}.rs"), "pub fn f() {}\n");
    }
    let big = tmp.path().join("generated.rs");
    std::fs::write(&big, "pub fn big() {}\n".repeat(100_000)).unwrap();
    let map = build_map(tmp.path()).expect("map should build");
    assert!(!map.contains("big"), "oversized file should be skipped");
}

#[test]
fn all_oversized_files_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    for f in 0..20 {
        let path = tmp.path().join(format!("f{f}.rs"));
        let body = "pub fn f() {}\n".repeat(100_000);
        std::fs::write(&path, body).unwrap();
    }
    assert!(!should_build(tmp.path()));
    assert!(build_map(tmp.path()).is_none());
}

#[test]
fn ranking_puts_entrypoints_before_deep_test_helpers() {
    let tmp = tempfile::tempdir().unwrap();
    // Entrypoint: pub struct Agent in src/agent/mod.rs.
    write_rs(
        tmp.path(),
        "src/agent/mod.rs",
        "pub struct Agent {}\npub enum AgentEvent {}\npub fn run_parallel() {}\n",
    );
    // Deep private test helpers that must lose budget.
    write_rs(
        tmp.path(),
        "src/agent/tests/helpers.rs",
        "fn test_setup() {}\nfn test_teardown() {}\nfn helper_do_thing() {}\n",
    );
    // Enough files to cross MIN_SOURCE_FILES.
    for f in 0..15 {
        write_rs(
            tmp.path(),
            &format!("src/util/f{f}.rs"),
            "pub fn util() {}\n",
        );
    }

    let map = build_map(tmp.path()).expect("map should build");
    let agent_idx = map
        .find("src/agent/mod.rs")
        .expect("entrypoint file present");
    let tests_idx = map.find("src/agent/tests/helpers.rs");
    assert!(
        tests_idx.is_none() || agent_idx < tests_idx.unwrap(),
        "entrypoint file must rank before deep test helpers"
    );
    // The public Agent struct must appear.
    assert!(map.contains("Agent [struct]"));
}

#[test]
fn output_is_grouped_and_wrapped() {
    let tmp = tempfile::tempdir().unwrap();
    write_rs(
        tmp.path(),
        "src/main.rs",
        "fn main() {}\npub struct App {}\n",
    );
    for f in 0..15 {
        write_rs(tmp.path(), &format!("src/mod/f{f}.rs"), "pub fn f() {}\n");
    }
    let map = build_map(tmp.path()).expect("map should build");
    assert!(map.starts_with("<repo_map>\n"));
    assert!(map.ends_with("</repo_map>"));
    // Grouped: file header followed by indented symbols. Both symbols in
    // src/main.rs appear under its header (order is by score, so `App`
    // ranks above `main`).
    let header_idx = map.find("src/main.rs\n").expect("file header present");
    let main_idx = map.find("main [fn]").expect("main present");
    let app_idx = map.find("App [struct]").expect("App present");
    assert!(header_idx < main_idx && header_idx < app_idx);
    assert!(main_idx < map.find("src/mod/").unwrap_or(usize::MAX));
}

#[test]
fn relative_paths_not_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    write_rs(tmp.path(), "src/main.rs", "fn main() {}\n");
    for f in 0..15 {
        write_rs(tmp.path(), &format!("src/mod/f{f}.rs"), "pub fn f() {}\n");
    }
    let map = build_map(tmp.path()).expect("map should build");
    let abs = tmp.path().to_string_lossy().to_string();
    assert!(
        !map.contains(&abs),
        "map must not contain absolute workspace paths"
    );
    assert!(map.contains("src/main.rs"));
}

#[test]
fn medium_workspace_with_few_files_still_builds() {
    // 15+ files but few symbols each: crosses MIN_SOURCE_FILES.
    let tmp = tempfile::tempdir().unwrap();
    for f in 0..15 {
        write_rs(tmp.path(), &format!("f{f}.rs"), "pub fn f() {}\n");
    }
    assert!(should_build(tmp.path()));
    assert!(build_map(tmp.path()).is_some());
}

#[test]
fn each_file_header_appears_once() {
    let tmp = tempfile::tempdir().unwrap();
    write_rs(
        tmp.path(),
        "src/a.rs",
        "pub fn a1() {}\npub fn a2() {}\npub fn a3() {}\n",
    );
    write_rs(
        tmp.path(),
        "src/b.rs",
        "pub fn b1() {}\npub fn b2() {}\npub fn b3() {}\n",
    );
    for f in 0..15 {
        write_rs(tmp.path(), &format!("src/mod/f{f}.rs"), "pub fn f() {}\n");
    }
    let map = build_map(tmp.path()).expect("map should build");
    let mut header_counts: HashMap<&str, usize> = HashMap::new();
    for line in map.lines() {
        if line.starts_with("  ") || line == "<repo_map>" || line == "</repo_map>" {
            continue;
        }
        *header_counts.entry(line).or_insert(0) += 1;
    }
    for (path, count) in &header_counts {
        assert_eq!(
            *count, 1,
            "path header '{}' appears {} times, expected exactly once",
            path, count
        );
    }
}

#[test]
fn many_symbols_few_files_still_builds() {
    // Few files but >= MIN_SYMBOLS symbols: crosses the symbol threshold.
    let tmp = tempfile::tempdir().unwrap();
    let mut body = String::new();
    for i in 0..90 {
        body.push_str(&format!("pub fn fn_{i}() {{}}\n"));
    }
    write_rs(tmp.path(), "a.rs", &body);
    assert!(
        !should_build(tmp.path()),
        "1 file is below MIN_SOURCE_FILES"
    );
    assert!(
        build_map(tmp.path()).is_some(),
        "symbol count should trigger a map even with few files"
    );
}

#[test]
fn build_map_is_cached_until_invalidated() {
    let tmp = tempfile::tempdir().unwrap();
    for f in 0..15 {
        write_rs(tmp.path(), &format!("f{f}.rs"), "pub fn f() {}\n");
    }
    let first = build_map(tmp.path()).expect("map should build");
    write_rs(
        tmp.path(),
        "new_unique.rs",
        "pub fn unique_cached_symbol() {}\n",
    );
    let second = build_map(tmp.path()).expect("cached");
    assert_eq!(first, second, "second call must reuse the cached map");
    invalidate(tmp.path());
    let third = build_map(tmp.path()).expect("rebuilt");
    assert_ne!(first, third);
    assert!(third.contains("unique_cached_symbol"));
}

#[test]
fn walk_skips_files_deeper_than_max_depth() {
    let tmp = tempfile::tempdir().unwrap();
    for f in 0..15 {
        write_rs(tmp.path(), &format!("f{f}.rs"), "pub fn shallow() {}\n");
    }
    let mut deep = tmp.path().to_path_buf();
    for i in 0..(MAX_WALK_DEPTH + 2) {
        deep.push(format!("d{i}"));
    }
    write_rs(&deep, "buried.rs", "pub fn buried_unique_symbol() {}\n");
    let map = build_map(tmp.path()).expect("map should build");
    assert!(
        !map.contains("buried_unique_symbol"),
        "files deeper than MAX_WALK_DEPTH must not be scanned"
    );
}
