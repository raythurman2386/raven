//! Per-language symbol-extraction patterns for the repo map.
//!
//! This is the static table of compiled per-extension regexes that
//! [`super::extract_symbols`] uses to pull top-level-ish declarations
//! (`fn`, `struct`, `enum`, `class`, `def`, …) out of each source file. Each
//! pattern carries the symbol kind it produces and whether a match implies a
//! public/exported declaration. Kept separate from the walk/cache/render logic
//! in `mod.rs` so the big data table doesn't bury the algorithm.

use std::collections::HashMap;
use std::sync::OnceLock;

/// A compiled per-extension pattern: a regex plus the symbol kind it produces
/// and whether a match implies a public/exported declaration.
pub(crate) struct Pattern {
    pub(crate) re: regex::Regex,
    pub(crate) kind: &'static str,
    pub(crate) public: bool,
}

impl Pattern {
    pub(crate) fn new(re: &str, kind: &'static str, public: bool) -> Self {
        Self {
            re: regex::Regex::new(re).expect("repomap pattern must compile"),
            kind,
            public,
        }
    }
}

/// Per-extension compiled patterns, built once (line-anchored; first matching
/// line wins). Keys are canonical file extensions (without the dot); aliases
/// like `cpp`/`h`/`hpp` and `ts`/`jsx`/`tsx` are normalized to their canonical
/// form before lookup.
pub(crate) fn patterns_for(ext: &str) -> &'static [Pattern] {
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
            // JS/TS — export patterns first so they match before non-export
            m.insert(
                "js",
                vec![
                    Pattern::new(r"^\s*export\s+(?:async\s+)?function\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*\(", "fn", true),
                    Pattern::new(r"^\s*(?:async\s+)?function\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*\(", "fn", false),
                    Pattern::new(r"^\s*export\s+const\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*=\s*[\(\[a-zA-Z_$0-9]", "const", true),
                    Pattern::new(r"^\s*const\s+([a-zA-Z_$][a-zA-Z0-9_$]*)\s*=\s*[\(\[a-zA-Z_$0-9]", "const", false),
                    Pattern::new(r"^\s*export\s+class\s+([A-Za-z_$][a-zA-Z0-9_$]*)", "class", true),
                    Pattern::new(r"^\s*class\s+([A-Za-z_$][a-zA-Z0-9_$]*)", "class", false),
                    Pattern::new(r"^\s*export\s+interface\s+([A-Za-z_$][a-zA-Z0-9_$]*)", "interface", true),
                    Pattern::new(r"^\s*interface\s+([A-Za-z_$][a-zA-Z0-9_$]*)", "interface", false),
                    Pattern::new(r"^\s*export\s+type\s+([A-Za-z_$][a-zA-Z0-9_$]*)\s*=", "type", true),
                    Pattern::new(r"^\s*type\s+([A-Za-z_$][a-zA-Z0-9_$]*)\s*=", "type", false),
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
