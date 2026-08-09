//! Lightweight, model-oriented repo symbol map for context-aware agents.
//!
//! Grok Build uses a full tree-sitter scope-graph (`xai-codebase-graph`) to
//! build a code graph — that's far too heavy for a mini harness. This module
//! provides a dependency-light substitute: walk the workspace once, extract
//! top-level-ish symbol declarations (`fn`, `struct`, `enum`, `impl`, `trait`,
//! `const`, `type`, `function`, `class`, `def`) via per-language regex, score
//! each symbol for how useful it is to a model, and emit a compact grouped
//! `<repo_map>` block.
//!
//! # Model-oriented ranking
//!
//! The map is injected into the system prompt, so budget should go to the
//! symbols a model is most likely to need: entrypoints (`main`, `lib.rs`,
//! `index.ts`), public/exported types, and shallow files — not to whichever
//! files happen to sort first lexically. Each symbol gets an integer score
//! (see the `*_BONUS` / `*_PENALTY` constants below); symbols are sorted by
//! score descending and rendered grouped by relative path until the char
//! budget is exhausted. Private helpers buried in deep test files lose budget
//! to `pub struct Agent` / `main`.
//!
//! The map is built only for non-trivial workspaces (at least
//! `MIN_SOURCE_FILES` source files *or* `MIN_SYMBOLS` symbols), capped at
//! `MAX_MAP_CHARS`, so small projects aren't weighed down and large ones get
//! structure without burning turns.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use walkdir::WalkDir;

/// Build a map only when the workspace has at least this many source files.
const MIN_SOURCE_FILES: usize = 15;
/// ...or at least this many extracted symbols (covers medium projects with
/// few files but many declarations).
const MIN_SYMBOLS: usize = 80;
/// Cap the rendered map (char-safe), matching tool-output discipline.
const MAX_MAP_CHARS: usize = 3500;
/// Skip source files larger than this (likely generated/minified).
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Paths at or shallower than this depth under the workspace get a bonus.
const SHALLOW_DEPTH: usize = 3;

// Scoring constants (tunable). Higher = more likely to appear in the map.
const ENTRYPOINT_BONUS: i32 = 40; // name is a known entrypoint
const ENTRYPOINT_PATH_BONUS: i32 = 25; // path is a known entrypoint file
const PUBLIC_BONUS: i32 = 20; // declaration is pub/exported
const TYPE_KIND_BONUS: i32 = 15; // struct/enum/trait/class/interface/type
const SHALLOW_DEPTH_BONUS: i32 = 10; // shallow path under workspace
const TEST_PATH_PENALTY: i32 = 20; // path looks like a test
const TEST_NAME_PENALTY: i32 = 15; // name looks like a test helper

/// Names treated as entrypoints.
const ENTRYPOINT_NAMES: &[&str] = &["main", "Main", "run", "start", "App", "Router", "index"];
/// File names treated as entrypoint modules.
const ENTRYPOINT_PATHS: &[&str] = &[
    "main.rs",
    "lib.rs",
    "mod.rs",
    "index.ts",
    "index.js",
    "__init__.py",
];
/// Kinds that represent type-like declarations.
const TYPE_KINDS: &[&str] = &["struct", "enum", "trait", "class", "interface", "type"];

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
    ".next",
    "coverage",
    "vendor",
    "Pods",
    ".turbo",
];
/// Extensions considered source.
const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "cpp", "c", "h", "hpp", "rb", "php",
    "swift", "kt", "cs", "sh",
];

/// A compiled per-extension pattern: a regex plus the symbol kind it produces
/// and whether a match implies a public/exported declaration.
struct Pattern {
    re: regex::Regex,
    kind: &'static str,
    public: bool,
}

impl Pattern {
    fn new(re: &str, kind: &'static str, public: bool) -> Self {
        Self {
            re: regex::Regex::new(re).expect("repomap pattern must compile"),
            kind,
            public,
        }
    }
}

/// A (symbol, kind, path, line, score) declaration.
#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    /// Workspace-relative path with `/` separators.
    pub path: String,
    pub line: usize,
    pub kind: &'static str,
    pub public: bool,
    pub score: i32,
}

/// Per-extension compiled patterns, built once (line-anchored; first matching
/// line wins). Keys are canonical file extensions (without the dot); aliases
/// like `cpp`/`h`/`hpp` and `ts`/`jsx`/`tsx` are normalized to their canonical
/// form before lookup.
fn patterns_for(ext: &str) -> &'static [Pattern] {
    static PATTERNS: OnceLock<HashMap<&'static str, Vec<Pattern>>> = OnceLock::new();
    let canonical = match ext {
        "h" | "cpp" | "hpp" => "c",
        "ts" | "jsx" | "tsx" => "js",
        other => other,
    };
    PATTERNS
        .get_or_init(|| {
            let mut m: HashMap<&'static str, Vec<Pattern>> = HashMap::new();
            // Rust
            m.insert(
                "rs",
                vec![
                    Pattern::new(r"^\s*\bpub\s+(?:async\s+)?fn\s+([a-zA-Z_][a-zA-Z0-9_]*)", "fn", true),
                    Pattern::new(r"^\s*\b(?:async\s+)?fn\s+([a-zA-Z_][a-zA-Z0-9_]*)", "fn", false),
                    Pattern::new(r"^\s*\bpub\s+struct\s+([A-Z][a-zA-Z0-9_]*)", "struct", true),
                    Pattern::new(r"^\s*\bstruct\s+([A-Z][a-zA-Z0-9_]*)", "struct", false),
                    Pattern::new(r"^\s*\bpub\s+enum\s+([A-Z][a-zA-Z0-9_]*)", "enum", true),
                    Pattern::new(r"^\s*\benum\s+([A-Z][a-zA-Z0-9_]*)", "enum", false),
                    Pattern::new(r"^\s*\bpub\s+trait\s+([A-Z][a-zA-Z0-9_]*)", "trait", true),
                    Pattern::new(r"^\s*\btrait\s+([A-Z][a-zA-Z0-9_]*)", "trait", false),
                    Pattern::new(r"^\s*\bimpl\s+([A-Z][a-zA-Z0-9_]*)", "impl", false),
                    Pattern::new(r"^\s*\bpub\s+const\s+([A-Z_][A-Z0-9_]*)", "const", true),
                    Pattern::new(r"^\s*\bconst\s+([A-Z_][A-Z0-9_]*)", "const", false),
                    Pattern::new(r"^\s*\bpub\s+type\s+([A-Z][a-zA-Z0-9_]*)", "type", true),
                ],
            );
            // Python
            m.insert(
                "py",
                vec![
                    Pattern::new(r"^\s*(?:async\s+)?def\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*class\s+([A-Za-z_][a-zA-Z0-9_]*)\s*[:(]", "class", false),
                ],
            );
            // JS/TS
            m.insert(
                "js",
                vec![
                    Pattern::new(r"^\s*(?:export\s+)?(?:async\s+)?function\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*(?:export\s+)?const\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*=\s*[\(\[a-zA-Z_$0-9]", "const", false),
                    Pattern::new(r"^\s*(?:export\s+)?class\s+([A-Za-z_$][a-zA-Z0-9_$]*)", "class", false),
                    Pattern::new(r"^\s*(?:export\s+)?interface\s+([A-Za-z_$][a-zA-Z0-9_$]*)", "interface", false),
                    Pattern::new(r"^\s*(?:export\s+)?type\s+([A-Za-z_$][a-zA-Z0-9_$]*)\s*=", "type", false),
                ],
            );
            // Go
            m.insert(
                "go",
                vec![
                    Pattern::new(r"^func\s+(?:\([^)]*\)\s*)?([A-Za-z_][a-zA-Z0-9_]*)\s*\(", "fn", false),
                    Pattern::new(r"^type\s+([A-Za-z_][a-zA-Z0-9_]*)\s+(?:struct|interface)\b", "type", false),
                ],
            );
            // C/C++
            m.insert(
                "c",
                vec![
                    Pattern::new(r"[A-Za-z_][a-zA-Z0-9_]*\s+([A-Za-z_][a-zA-Z0-9_]*)\s*\([^;]*\)\s*\{", "fn", false),
                    Pattern::new(r"^class\s+([A-Za-z_][a-zA-Z0-9_]*)", "class", false),
                    Pattern::new(r"^struct\s+([A-Za-z_][a-zA-Z0-9_]*)", "struct", false),
                    Pattern::new(r"^enum\s+([A-Za-z_][a-zA-Z0-9_]*)", "enum", false),
                ],
            );
            // Ruby
            m.insert(
                "rb",
                vec![
                    Pattern::new(r"^\s*def\s+([a-zA-Z_][a-zA-Z0-9_!?]*)\b", "fn", false),
                    Pattern::new(r"^\s*class\s+([A-Za-z_][a-zA-Z0-9_]*)", "class", false),
                ],
            );
            // PHP
            m.insert(
                "php",
                vec![
                    Pattern::new(r"^\s*function\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*class\s+([A-Za-z_][a-zA-Z0-9_]*)", "class", false),
                ],
            );
            // Swift / Kotlin / C#
            m.insert(
                "swift",
                vec![
                    Pattern::new(r"^\s*(?:public\s+|private\s+)?func\s+([a-zA-Z_][a-zA-Z0-9_]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*(?:public\s+|private\s+)?class\s+([A-Za-z_][a-zA-Z0-9_]*)", "class", false),
                ],
            );
            m.insert(
                "kt",
                vec![
                    Pattern::new(r"^\s*(?:public\s+|private\s+|internal\s+)?fun\s+([A-Za-z_][a-zA-Z0-9_]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*(?:public\s+|private\s+|internal\s+)?class\s+([A-Za-z_][a-zA-Z0-9_]*)", "class", false),
                ],
            );
            m.insert(
                "cs",
                vec![
                    Pattern::new(r"^\s*(?:public\s+|private\s+|internal\s+)?(?:static\s+)?[A-Za-z_][a-zA-Z0-9_<>]*\s+([A-Za-z_][a-zA-Z0-9_]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*(?:public\s+|private\s+|internal\s+)?class\s+([A-Za-z_][a-zA-Z0-9_]*)", "class", false),
                ],
            );
            // Shell
            m.insert(
                "sh",
                vec![
                    Pattern::new(r"^([a-zA-Z_][a-zA-Z0-9_]*)\s*\(\)\s*\{", "fn", false),
                    Pattern::new(r"^\s*function\s+([a-zA-Z_][a-zA-Z0-9_]*)", "fn", false),
                ],
            );
            m
        })
        .get(canonical)
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

/// Decide whether to build a repo map for `workspace`. This is a cheap
/// superset check (source-file count only, no extraction); `build_map` makes
/// the final call using both file and symbol counts.
pub fn should_build(workspace: &Path) -> bool {
    count_source_files(workspace) >= MIN_SOURCE_FILES
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

/// Build a compact, ranked, grouped repo map string, or `None` if the
/// workspace is too small to be worth it.
pub fn build_map(workspace: &Path) -> Option<String> {
    let mut symbols = Vec::new();
    let mut source_files = 0usize;
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
        source_files += 1;
        // Skip likely-generated files (minified bundles, lockfiles, etc.).
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            continue;
        };
        extract_symbols(&content, ext, path, &mut symbols);
    }

    if source_files < MIN_SOURCE_FILES && symbols.len() < MIN_SYMBOLS {
        return None;
    }
    if symbols.is_empty() {
        return None;
    }

    // Score each symbol using its workspace-relative path, and rewrite the
    // stored path to be relative so the renderer emits `/`-separated relative
    // paths (never absolute).
    for s in &mut symbols {
        let rel = Path::new(&s.path)
            .strip_prefix(workspace)
            .unwrap_or(Path::new(&s.path));
        s.score = score_symbol(&s.name, s.kind, s.public, rel);
        s.path = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
    }
    // Stable ordering: higher score first, then path, then name. Within a
    // file this yields higher-score-then-name, which the grouped renderer
    // preserves.
    symbols.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.name.cmp(&b.name))
    });

    Some(render(&symbols))
}

/// Render symbols grouped by relative path, capped at `MAX_MAP_CHARS`.
/// Hard-stops cleanly: never cuts a line in half.
fn render(symbols: &[Symbol]) -> String {
    let mut out = String::from("<repo_map>\n");
    let mut current_file: Option<&str> = None;
    for s in symbols {
        if current_file != Some(s.path.as_str()) {
            let header = format!("{}\n", s.path);
            if out.chars().count() + header.chars().count() > MAX_MAP_CHARS {
                break;
            }
            out.push_str(&header);
            current_file = Some(s.path.as_str());
        }
        let line = format!("  {} [{}]\n", s.name, s.kind);
        if out.chars().count() + line.chars().count() > MAX_MAP_CHARS {
            break;
        }
        out.push_str(&line);
    }
    out.push_str("</repo_map>");
    out
}

/// Score a symbol for model usefulness. Higher is better.
fn score_symbol(name: &str, kind: &str, public: bool, rel: &Path) -> i32 {
    let mut score = 0;
    if ENTRYPOINT_NAMES.contains(&name) {
        score += ENTRYPOINT_BONUS;
    }
    if is_entrypoint_path(rel) {
        score += ENTRYPOINT_PATH_BONUS;
    }
    if public {
        score += PUBLIC_BONUS;
    }
    if TYPE_KINDS.contains(&kind) {
        score += TYPE_KIND_BONUS;
    }
    if rel.components().count() <= SHALLOW_DEPTH {
        score += SHALLOW_DEPTH_BONUS;
    }
    if is_test_path(rel) {
        score -= TEST_PATH_PENALTY;
    }
    if name.starts_with("test_") || name.ends_with("Test") {
        score -= TEST_NAME_PENALTY;
    }
    score
}

fn is_entrypoint_path(rel: &Path) -> bool {
    let name = rel.file_name().and_then(|n| n.to_str()).unwrap_or("");
    ENTRYPOINT_PATHS.iter().any(|s| name.ends_with(s))
}

fn is_test_path(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    s.contains("/tests/") || s.contains("__tests__") || s.contains("_test.") || s.contains(".test.")
}

/// Extract symbol declarations from `content` for `ext`, appending to
/// `symbols`. `path` is stored as-is (workspace-relative when called from
/// `build_map`); scoring happens later against the relative path.
fn extract_symbols(content: &str, ext: &str, path: &Path, symbols: &mut Vec<Symbol>) {
    let patterns = patterns_for(ext);
    if patterns.is_empty() {
        return;
    }
    let rel = path.display().to_string();
    for (i, line) in content.lines().enumerate() {
        let line_no = i + 1;
        for pat in patterns {
            if let Some(caps) = pat.re.captures(line) {
                if let Some(m) = caps.get(1) {
                    let public = pat.public || line.contains("export ");
                    symbols.push(Symbol {
                        name: m.as_str().to_string(),
                        path: rel.clone(),
                        line: line_no,
                        kind: pat.kind,
                        public,
                        score: 0,
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
        for d in [".next", "coverage", "vendor", "Pods", ".turbo"] {
            let dir = tmp.path().join(d);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("x.rs"), "pub fn foo() {}\n").unwrap();
        }
        assert_eq!(count_source_files(tmp.path()), 0);
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
}
