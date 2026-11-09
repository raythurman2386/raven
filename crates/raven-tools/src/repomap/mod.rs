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
//! structure without burning turns. File discovery prefers `git ls-files`
//! (tracked + untracked, exclude-standard — inherently `.gitignore`-aware and
//! index-fast); otherwise falls back to an [`ignore`]-crate walk. Candidates
//! are path-scored so the scan budget prefers entrypoints/shallow `src/` over
//! deep tests. Depth- and file-capped; cached per workspace until
//! [`invalidate`] is called.

use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

mod patterns;

use patterns::patterns_for;

/// Build a map only when the workspace has at least this many source files.
const MIN_SOURCE_FILES: usize = 15;
/// ...or at least this many extracted symbols (covers medium projects with
/// few files but many declarations).
const MIN_SYMBOLS: usize = 80;
/// Cap the rendered map (char-safe), matching tool-output discipline.
const MAX_MAP_CHARS: usize = 3500;
/// Skip source files larger than this (likely generated/minified).
const MAX_FILE_BYTES: u64 = 256 * 1024;
/// Stop reading source files after this many. A parent folder of many repos
/// (e.g. `~/Work`) is a valid workspace path but must not be fully scanned
/// on every turn.
const MAX_SOURCE_FILES_SCANNED: usize = 300;
/// Do not descend deeper than this under the workspace root.
const MAX_WALK_DEPTH: usize = 8;
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

/// Skip these directories entirely (belt-and-suspenders on top of `.gitignore`).
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "target-local",
    "dist",
    "build",
    "out",
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
    ".tmp",
    ".cache",
];
/// Extensions considered source.
const SOURCE_EXTS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "cpp", "c", "h", "hpp", "rb", "php",
    "swift", "kt", "cs", "sh",
];

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

fn map_cache() -> &'static Mutex<HashMap<PathBuf, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_lock() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Option<String>>> {
    map_cache().lock().unwrap_or_else(|e| e.into_inner())
}

/// Drop the cached map for `workspace` so the next [`build_map`] rescans.
pub fn invalidate(workspace: &Path) {
    cache_lock().remove(workspace);
}

/// Decide whether to build a repo map for `workspace`. This is a cheap
/// superset check (source-file count only, no extraction); `build_map` makes
/// the final call using both file and symbol counts.
pub fn should_build(workspace: &Path) -> bool {
    if let Some(cached) = cache_lock().get(workspace) {
        return cached.is_some();
    }
    count_source_files(workspace) >= MIN_SOURCE_FILES
}

fn count_source_files(workspace: &Path) -> usize {
    collect_source_files(workspace)
        .into_iter()
        .take(MIN_SOURCE_FILES)
        .count()
}

/// Build a compact, ranked, grouped repo map string, or `None` if the
/// workspace is too small to be worth it.
///
/// Results are cached per workspace path. Call [`invalidate`] after file
/// edits so the next turn sees a fresh map.
pub fn build_map(workspace: &Path) -> Option<String> {
    if let Some(cached) = cache_lock().get(workspace) {
        return cached.clone();
    }
    let map = build_map_uncached(workspace);
    cache_lock().insert(workspace.to_path_buf(), map.clone());
    map
}

fn build_map_uncached(workspace: &Path) -> Option<String> {
    let candidates = collect_source_files(workspace);
    let total_candidates = candidates.len();
    let mut symbols = Vec::new();
    // Scan highest-value paths first so the file budget lands on entrypoints
    // and shallow modules rather than deep test helpers (filesystem order).
    for path in candidates.into_iter().take(MAX_SOURCE_FILES_SCANNED) {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        extract_symbols(&content, ext, &path, &mut symbols);
    }

    if total_candidates < MIN_SOURCE_FILES && symbols.len() < MIN_SYMBOLS {
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

/// Collect source files under `workspace`, preferring git's index (fast +
/// exclude-standard) and falling back to an ignore-aware filesystem walk.
/// Results are sorted by [`score_path`] descending so callers spend extract
/// budget on the most useful paths first.
fn collect_source_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = git_list_sources(workspace).unwrap_or_else(|| ignore_walk_sources(workspace));
    // If git is present but the workspace is nested under a parent repo whose
    // `.gitignore` excludes it (e.g. a temp dir under a gitignored
    // `.raven/tmp/`), `git ls-files` returns an empty list with success. That
    // would silently drop every source file. Fall back to the filesystem walk
    // in that case so the map still builds.
    if files.is_empty() {
        files = ignore_walk_sources(workspace);
    }
    files.sort_by(|a, b| {
        let ra = a.strip_prefix(workspace).unwrap_or(a);
        let rb = b.strip_prefix(workspace).unwrap_or(b);
        score_path(rb).cmp(&score_path(ra)).then_with(|| ra.cmp(rb))
    });
    files
}

/// `git ls-files --cached --others --exclude-standard`: tracked + untracked
/// non-ignored paths. Returns `None` when git is missing or the workspace is
/// not a repository so the caller can fall back to a filesystem walk.
fn git_list_sources(workspace: &Path) -> Option<Vec<PathBuf>> {
    let output = Command::new("git")
        .args([
            "-C",
            workspace.to_str()?,
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut files = Vec::new();
    for raw in output.stdout.split(|&b| b == 0) {
        if raw.is_empty() {
            continue;
        }
        let rel = PathBuf::from(String::from_utf8_lossy(raw).as_ref());
        if !is_eligible_source(workspace, &rel) {
            continue;
        }
        let abs = workspace.join(&rel);
        if !abs.is_file() {
            continue;
        }
        if abs.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            continue;
        }
        files.push(abs);
    }
    Some(files)
}

/// Filesystem walk that honors `.gitignore` / `.ignore` / `.git/info/exclude`
/// plus [`SKIP_DIRS`]. Used when git is unavailable.
fn ignore_walk_sources(workspace: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let walker = WalkBuilder::new(workspace)
        .max_depth(Some(MAX_WALK_DEPTH))
        .git_ignore(true)
        .git_exclude(true)
        .git_global(false)
        .ignore(true)
        .hidden(false)
        .require_git(false)
        // Do not inherit `.gitignore` from parent directories. The workspace
        // is the mapping root: only gitignore files *within* it should apply.
        // Without this, a workspace nested under a parent repo (e.g. a temp
        // dir under a gitignored `.raven/tmp/`) has all its files skipped by
        // the parent's `.gitignore`.
        .parents(false)
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !SKIP_DIRS.iter().any(|s| *s == name.as_ref())
        })
        .build();
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let Ok(rel) = path.strip_prefix(workspace) else {
            continue;
        };
        if !is_eligible_source(workspace, rel) {
            continue;
        }
        if entry.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
            continue;
        }
        files.push(path.to_path_buf());
    }
    files
}

/// Extension, depth, and hard skip-dir checks shared by git and walk listing.
fn is_eligible_source(_workspace: &Path, rel: &Path) -> bool {
    if rel.components().count() > MAX_WALK_DEPTH {
        return false;
    }
    if rel.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        SKIP_DIRS.iter().any(|s| *s == name.as_ref())
    }) {
        return false;
    }
    rel.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SOURCE_EXTS.contains(&e))
}

/// Path-level priority before extraction (higher = scan sooner).
fn score_path(rel: &Path) -> i32 {
    let mut score = 0;
    if is_entrypoint_path(rel) {
        score += 50;
    }
    if rel
        .components()
        .next()
        .is_some_and(|c| c.as_os_str() == "src")
    {
        score += 15;
    }
    if rel.components().count() <= SHALLOW_DEPTH {
        score += 20;
    }
    if is_test_path(rel) {
        score -= 40;
    }
    score
}

/// Select symbols by score under budget, then group by path, then render
/// each file exactly once. Hard-stops cleanly: never cuts a line in half.
fn render(symbols: &[Symbol]) -> String {
    let mut selected: Vec<&Symbol> = Vec::new();
    let mut seen: HashMap<&str, ()> = HashMap::new();
    let mut chars = "<repo_map>\n".chars().count();

    for s in symbols {
        let header_chars = if seen.contains_key(s.path.as_str()) {
            0
        } else {
            s.path.chars().count() + 1
        };
        let line = format!("  {} [{}]\n", s.name, s.kind);
        if chars + header_chars + line.chars().count() > MAX_MAP_CHARS {
            break;
        }
        chars += header_chars + line.chars().count();
        seen.insert(s.path.as_str(), ());
        selected.push(s);
    }

    let mut groups: Vec<(&str, Vec<&&Symbol>)> = Vec::new();
    let mut group_idx: HashMap<&str, usize> = HashMap::new();
    for s in &selected {
        if let Some(&idx) = group_idx.get(s.path.as_str()) {
            groups[idx].1.push(s);
        } else {
            group_idx.insert(s.path.as_str(), groups.len());
            groups.push((s.path.as_str(), vec![s]));
        }
    }

    let mut out = String::from("<repo_map>\n");
    for (path, syms) in &groups {
        out.push_str(&format!("{}\n", path));
        for s in syms {
            out.push_str(&format!("  {} [{}]\n", s.name, s.kind));
        }
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
                    let public = pat.public;
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
mod tests;
