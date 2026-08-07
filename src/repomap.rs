//! Lightweight repo symbol map for context-aware agents.
//!
//! Grok Build uses a full tree-sitter scope-graph (`xai-codebase-graph`) to
//! build a code graph — that's far too heavy for a mini harness. This module
//! provides a dependency-light substitute: walk the workspace, extract
//! top-level-ish symbol declarations (`fn`, `struct`, `enum`, `impl`, `trait`,
//! `const`, `type`, `function`, `class`, `def`) via per-language regex, and
//! emit a compact `<repo_map>` block of `symbol — path:line` lines.
//!
//! The map is injected into the system prompt **only for large workspaces**
//! (above `MIN_FILES_FOR_MAP`), capped at `MAX_MAP_CHARS`, so small projects
//! aren't weighed down and large ones get structure without burning turns.

use std::path::Path;
use walkdir::WalkDir;

/// Only build a repo map when the workspace has at least this many source files.
const MIN_FILES_FOR_MAP: usize = 50;
/// Cap the rendered map (char-safe), matching tool-output discipline.
const MAX_MAP_CHARS: usize = 2000;
/// Skip these directories entirely.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "__pycache__",
    ".raven",
    ".grok",
    ".hermes",
    ".cargo",
];
/// Extensions considered source.
const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "cpp", "c", "h", "hpp", "rb", "php",
    "swift", "kt", "cs", "sh",
];

/// A (symbol, kind, path, line) declaration.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub path: String,
    pub line: usize,
}

/// Per-extension regex patterns (unanchored; first matching line wins).
/// Keys are file extensions (without the dot).
fn patterns_for(ext: &str) -> &'static [&'static str] {
    match ext {
        // Rust
        "rs" => &[
            r"\bpub\s+(?:async\s+)?fn\s+([a-zA-Z_][a-zA-Z0-9_]*)",
            r"\b(?:async\s+)?fn\s+([a-zA-Z_][a-zA-Z0-9_]*)",
            r"\bpub\s+struct\s+([A-Z][a-zA-Z0-9_]*)",
            r"\bstruct\s+([A-Z][a-zA-Z0-9_]*)",
            r"\bpub\s+enum\s+([A-Z][a-zA-Z0-9_]*)",
            r"\benum\s+([A-Z][a-zA-Z0-9_]*)",
            r"\bpub\s+trait\s+([A-Z][a-zA-Z0-9_]*)",
            r"\btrait\s+([A-Z][a-zA-Z0-9_]*)",
            r"\bimpl\s+([A-Z][a-zA-Z0-9_]*)",
            r"\bpub\s+const\s+([A-Z_][A-Z0-9_]*)",
            r"\bconst\s+([A-Z_][A-Z0-9_]*)",
            r"\bpub\s+type\s+([A-Z][a-zA-Z0-9_]*)",
        ],
        // Python
        "py" => &[
            r"^\s*(?:async\s+)?def\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(",
            r"^\s*class\s+([A-Za-z_][a-zA-Z0-9_]*)\s*[:(]",
        ],
        // JS/TS
        "js" | "ts" | "jsx" | "tsx" => &[
            r"(?:export\s+)?(?:async\s+)?function\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*\(",
            r"(?:export\s+)?const\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*=\s*[\(\[\{a-zA-Z_$0-9]",
            r"(?:export\s+)?class\s+([A-Za-z_$][a-zA-Z0-9_$]*)",
            r"(?:export\s+)?interface\s+([A-Za-z_$][a-zA-Z0-9_$]*)",
            r"(?:export\s+)?type\s+([A-Za-z_$][a-zA-Z0-9_$]*)\s*=",
        ],
        // Go
        "go" => &[
            r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_][a-zA-Z0-9_]*)\s*\(",
            r"^type\s+([A-Za-z_][a-zA-Z0-9_]*)\s+(?:struct|interface)\b",
        ],
        // C/C++
        "c" | "h" | "cpp" | "hpp" => &[
            r"[A-Za-z_][a-zA-Z0-9_]*\s+([A-Za-z_][a-zA-Z0-9_]*)\s*\([^;]*\)\s*\{",
            r"^class\s+([A-Za-z_][a-zA-Z0-9_]*)",
            r"^struct\s+([A-Za-z_][a-zA-Z0-9_]*)",
            r"^enum\s+([A-Za-z_][a-zA-Z0-9_]*)",
        ],
        // Ruby
        "rb" => &[
            r"^\s*def\s+([a-zA-Z_][a-zA-Z0-9_!?]*)\b",
            r"^\s*class\s+([A-Za-z_][a-zA-Z0-9_]*)",
        ],
        // PHP
        "php" => &[
            r"^\s*function\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(",
            r"^\s*class\s+([A-Za-z_][a-zA-Z0-9_]*)",
        ],
        // Swift / Kotlin / C#
        "swift" => &[
            r"^\s*(?:public\s+|private\s+)?func\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(",
            r"^\s*(?:public\s+|private\s+)?class\s+([A-Za-z_][a-zA-Z0-9_]*)",
        ],
        "kt" => &[
            r"^\s*(?:public\s+|private\s+|internal\s+)?fun\s+([A-Za-z_][a-zA-Z0-9_]*)\s*\(",
            r"^\s*(?:public\s+|private\s+|internal\s+)?class\s+([A-Za-z_][a-zA-Z0-9_]*)",
        ],
        "cs" => &[
            r"^\s*(?:public\s+|private\s+|internal\s+)?(?:static\s+)?[A-Za-z_][a-zA-Z0-9_<>]*\s+([A-Za-z_][a-zA-Z0-9_]*)\s*\(",
            r"^\s*(?:public\s+|private\s+|internal\s+)?class\s+([A-Za-z_][a-zA-Z0-9_]*)",
        ],
        // Shell
        "sh" => &[
            r"^([a-zA-Z_][a-zA-Z0-9_]*)\s*\(\)\s*\{",
            r"^\s*function\s+([a-zA-Z_][a-zA-Z0-9_]*)",
        ],
        _ => &[],
    }
}

/// Decide whether to build a repo map for `workspace` (large enough, has source).
pub fn should_build(workspace: &Path) -> bool {
    count_source_files(workspace) >= MIN_FILES_FOR_MAP
}

fn count_source_files(workspace: &Path) -> usize {
    let mut n = 0usize;
    for entry in WalkDir::new(workspace)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|s| *s == name.as_ref())
        })
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file()
            && entry
                .path()
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| SOURCE_EXTS.contains(&e))
                .unwrap_or(false)
        {
            n += 1;
        }
    }
    n
}

/// Build a compact repo map string, or `None` if the workspace is too small.
pub fn build_map(workspace: &Path) -> Option<String> {
    if !should_build(workspace) {
        return None;
    }
    let mut symbols = Vec::new();
    for entry in WalkDir::new(workspace)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|s| *s == name.as_ref())
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        if !SOURCE_EXTS.contains(&ext) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        extract_symbols(&content, ext, path, &mut symbols);
    }

    if symbols.is_empty() {
        return None;
    }

    symbols.sort_by_key(|s| (s.path.clone(), s.line));
    // Cap to MAX_MAP_CHARS.
    let mut out = String::from("<repo_map>\n");
    let base = workspace.display().to_string();
    for s in &symbols {
        let rel = s
            .path
            .strip_prefix(&base)
            .map(|p| p.to_string())
            .unwrap_or_else(|| s.path.clone());
        let line = format!("{} — {}:{}\n", s.name, rel, s.line);
        if out.chars().count() + line.chars().count() > MAX_MAP_CHARS {
            break;
        }
        out.push_str(&line);
    }
    out.push_str("</repo_map>");
    Some(out)
}

/// Extract symbol declarations from `content` for `ext`, appending to `symbols`.
fn extract_symbols(content: &str, ext: &str, path: &std::path::Path, symbols: &mut Vec<Symbol>) {
    let patterns = patterns_for(ext);
    if patterns.is_empty() {
        return;
    }
    let re: Vec<regex::Regex> = patterns
        .iter()
        .map(|p| regex::Regex::new(p).unwrap())
        .collect();
    let rel = path.display().to_string();
    for (i, line) in content.lines().enumerate() {
        let line_no = i + 1;
        for r in &re {
            if let Some(caps) = r.captures(line) {
                if let Some(m) = caps.get(1) {
                    symbols.push(Symbol {
                        name: m.as_str().to_string(),
                        path: rel.clone(),
                        line: line_no,
                    });
                    break; // one symbol per line
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_rs(dir: &std::path::Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).unwrap();
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
        // Create enough files to cross MIN_FILES_FOR_MAP so build_map runs.
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
    fn skips_vendor_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let nm = tmp.path().join("node_modules");
        std::fs::create_dir_all(&nm).unwrap();
        std::fs::write(nm.join("x.js"), "function foo() {}").unwrap();
        // node_modules should not contribute.
        assert_eq!(count_source_files(tmp.path()), 0);
    }
}
