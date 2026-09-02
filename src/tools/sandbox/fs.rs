//! Workspace-confined file tools: path resolution, file I/O, search/replace,
//! `grep`, and the literal `search_code` helper.

use anyhow::{bail, Context, Result};
use regex::Regex;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use walkdir::WalkDir;

use crate::tools::{document, glob_matches};

use super::{
    normalize_path, OpenFlags, Sandbox, MAX_LINE_LENGTH, MAX_TOOL_OUTPUT,
    REPLACE_ALL_WARN_THRESHOLD,
};

impl Sandbox {
    /// Resolve `path` relative to the workspace, rejecting traversal and
    /// symlink escapes.
    ///
    /// Two defenses:
    /// 1. Lexical `..` traversal is rejected via [`normalize_path`].
    /// 2. The nearest existing ancestor is canonicalized (resolving symlinks)
    ///    and must remain inside the canonicalized workspace. This blocks
    ///    `workspace/link -> /etc` from escaping on both read and write
    ///    (including writes whose parent directory is a symlink pointing out).
    pub(crate) fn safe_resolve(&self, path: &str) -> Result<PathBuf> {
        let joined = self.workspace.join(path);
        let normalized = normalize_path(&joined);
        if !normalized.starts_with(&self.workspace) {
            bail!(
                "Path outside workspace: {}. Use relative paths like 'src/main.rs', not absolute paths starting with /",
                path
            );
        }

        let ws_canon = self
            .workspace
            .canonicalize()
            .unwrap_or_else(|_| self.workspace.clone());

        if !self.workspace.exists() {
            return Ok(normalized);
        }

        let mut probe: &std::path::Path = normalized.as_path();
        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        while !probe.exists() {
            if let Some(name) = probe.file_name() {
                suffix.push(name.to_os_string());
            }
            match probe.parent() {
                Some(p) if !p.as_os_str().is_empty() => probe = p,
                _ => break,
            }
        }

        let anchor_canon = probe.canonicalize().unwrap_or_else(|_| probe.to_path_buf());
        if !anchor_canon.starts_with(&ws_canon) {
            bail!(
                "Path outside workspace via symlink: {}. All paths must stay within the workspace root.",
                path
            );
        }

        let mut target = anchor_canon;
        for seg in suffix.iter().rev() {
            target.push(seg);
        }
        Ok(target)
    }

    /// Open a file relative to the workspace root with kernel-enforced path
    /// confinement.
    ///
    /// On Linux, uses `openat2` with `RESOLVE_BENEATH | NO_MAGICLINKS`, which
    /// makes the kernel refuse to resolve any path that escapes the workspace
    /// — atomically, with no TOCTOU race (a symlink cannot be swapped in
    /// between the check and the open). On other platforms, falls back to
    /// [`Self::safe_resolve`] + `std::fs::File::open`.
    ///
    /// `path` must be relative to the workspace root (e.g. `src/main.rs`).
    pub(crate) fn open_beneath(
        &self,
        path: &str,
        flags: OpenFlags,
        #[cfg_attr(not(target_os = "linux"), allow(unused_variables))] mode: u32,
    ) -> Result<std::fs::File> {
        #[cfg(target_os = "linux")]
        {
            use rustix::fs::{openat2, ResolveFlags};
            let ws_dir = std::fs::File::open(&self.workspace)
                .map_err(|e| anyhow::anyhow!("open workspace dir: {e}"))?;
            let fd = openat2(
                &ws_dir,
                path,
                flags.to_rustix(),
                rustix::fs::Mode::from_bits_truncate(mode),
                ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
            )
            .map_err(|e| {
                anyhow::anyhow!(
                    "Path outside workspace or unopenable: {path}. Use relative paths like 'src/main.rs', not absolute paths starting with / ({e})"
                )
            })?;
            Ok(std::fs::File::from(fd))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let p = self.safe_resolve(path)?;
            let file = std::fs::OpenOptions::new()
                .read(flags.contains(OpenFlags::RDONLY))
                .write(flags.contains(OpenFlags::WRONLY))
                .create(flags.contains(OpenFlags::CREATE))
                .append(flags.contains(OpenFlags::APPEND))
                .truncate(flags.contains(OpenFlags::TRUNC))
                .open(&p)?;
            Ok(file)
        }
    }

    /// List the contents of a directory (dirs first, then files, alphabetical).
    pub fn list_dir(&self, path: &str) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if !p.exists() {
            return Ok(format!("Error: {} does not exist", path));
        }
        if !p.is_dir() {
            return Ok(format!("Error: {} is not a directory", path));
        }
        let mut items = Vec::new();
        let mut entries: Vec<_> = std::fs::read_dir(&p)?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| {
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            (!is_dir, e.file_name().to_string_lossy().to_lowercase())
        });
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let kind = if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                "dir "
            } else {
                "file"
            };
            let size = if e.path().is_file() {
                e.metadata()
                    .map(|m| format!("  ({} B)", m.len()))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            items.push(format!("{} {}{}", kind, name, size));
        }
        Ok(if items.is_empty() {
            "(empty)".into()
        } else {
            items.join("\n")
        })
    }

    /// Read a file, returning a numbered line range (1-based `start_line`, up to `max_lines`).
    /// Lines longer than 2000 chars are truncated.
    ///
    /// Non-text documents (`.docx`, `.pdf`, `.xlsx`, `.odt`, `.epub`, ...) are
    /// converted to Markdown via `document` so the model can read them.
    /// Known binary files (images, audio, video, archives) are rejected.
    pub fn read_file(&self, path: &str, start_line: usize, max_lines: usize) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if !p.exists() {
            return Ok(format!(
                "Error: {} does not exist. Use list_dir to see available files, then use a relative path like 'src/main.rs'.",
                path
            ));
        }
        if !p.is_file() {
            return Ok(format!(
                "Error: {} is not a file. Use list_dir to see available files. Paths are relative to the workspace root, e.g. 'README.md' not '.README.md'.",
                path
            ));
        }

        // Structured-document extraction: try before the binary guard so
        // .docx/.xlsx/.pdf render as text. Malformed documents fall through
        // to the normal text/binary handling.
        if document::is_extractable_document(path) {
            match document::extract_document_text(&p.to_string_lossy()) {
                Ok(markdown) => {
                    let lines: Vec<&str> = markdown.lines().collect();
                    let start = start_line.saturating_sub(1);
                    let end = (start + max_lines).min(lines.len());
                    let mut out = format!(
                        "--- {} (extracted document, lines {}-{} of {}) ---\n",
                        path,
                        start + 1,
                        end,
                        lines.len()
                    );
                    let mut used = out.chars().count();
                    for (i, line) in lines[start..end].iter().enumerate() {
                        let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
                        let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
                        let rendered_len = rendered.chars().count();
                        if used + rendered_len > MAX_TOOL_OUTPUT {
                            out.push_str(&format!("…[truncated at {} chars]", MAX_TOOL_OUTPUT));
                            break;
                        }
                        out.push_str(&rendered);
                        used += rendered_len;
                    }
                    return Ok(out);
                }
                Err(e) => {
                    // Fall through to the binary guard / text read below.
                    tracing::debug!("document extraction failed for {}: {e}", path);
                }
            }
        }

        // Binary file guard: block known binary extensions (no I/O).
        if document::has_binary_extension(path) {
            return Ok(format!(
                "Error: {} is a binary file. Use list_dir to see available files, or run_shell to inspect it.",
                path
            ));
        }

        // Open via openat2 (kernel-enforced confinement, no TOCTOU race).
        let file = self.open_beneath(path, OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0)?;
        let text = std::io::read_to_string(file).context("read file")?;
        let lines: Vec<&str> = text.lines().collect();
        let start = start_line.saturating_sub(1);
        let end = (start + max_lines).min(lines.len());
        let mut out = format!(
            "--- {} (lines {}-{} of {}) ---\n",
            path,
            start + 1,
            end,
            lines.len()
        );
        let mut used = out.chars().count();
        for (i, line) in lines[start..end].iter().enumerate() {
            let truncated: String = line.chars().take(MAX_LINE_LENGTH).collect();
            let rendered = format!("{:5}| {}\n", start + i + 1, truncated);
            let rendered_len = rendered.chars().count();
            if used + rendered_len > MAX_TOOL_OUTPUT {
                out.push_str(&format!("…[truncated at {} chars]", MAX_TOOL_OUTPUT));
                break;
            }
            out.push_str(&rendered);
            used += rendered_len;
        }
        Ok(out)
    }

    /// Full file write (create/overwrite).
    pub fn write_file(&self, path: &str, content: &str) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rel = normalize_path(std::path::Path::new(path))
            .to_string_lossy()
            .into_owned();
        let mut file = self.open_beneath(
            &rel,
            OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
            0o644,
        )?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        Ok(format!("Wrote {} bytes → {}", content.len(), path))
    }

    /// Search-and-replace edit (Grok Build `search_replace` semantics).
    ///
    /// - `old_string` empty → create new file (like write_file).
    /// - `replace_all` → replace every occurrence.
    /// - Otherwise replace the first occurrence (must be unique).
    pub fn search_replace(
        &self,
        path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<String> {
        let p = self.safe_resolve(path)?;
        if p.is_dir() {
            return Ok("Error: file path is a directory".into());
        }

        if old_string.is_empty() {
            if p.exists() {
                return Ok(format!(
                    "Error: {} already exists; cannot create with empty old_string",
                    path
                ));
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let rel = normalize_path(std::path::Path::new(path))
                .to_string_lossy()
                .into_owned();
            let mut file = self.open_beneath(
                &rel,
                OpenFlags::WRONLY | OpenFlags::CREATE | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
                0o644,
            )?;
            use std::io::Write;
            file.write_all(new_string.as_bytes())?;
            return Ok(format!("Created {} ({} bytes)", path, new_string.len()));
        }

        if !p.is_file() {
            return Ok(format!("Error: {} is not a file", path));
        }

        // Normalize the path lexically so `..` components that cancel out
        // (e.g. `newdir/../f.txt`) resolve to a bare relative path before we
        // hand it to `openat2` (RESOLVE_BENEATH). Without this, openat2 tries
        // to traverse `newdir` literally and fails with ENOENT when the
        // intermediate dir does not exist — even though `safe_resolve` already
        // validated the normalized path. (Issues #104, #108.)
        let rel = normalize_path(std::path::Path::new(path))
            .to_string_lossy()
            .into_owned();

        // Open via openat2 (kernel-enforced confinement, no TOCTOU race).
        let file = self.open_beneath(&rel, OpenFlags::RDONLY | OpenFlags::CLOEXEC, 0)?;
        let content = std::io::read_to_string(file).context("read file before edit")?;

        if replace_all {
            let count = content.matches(old_string).count();
            if count == 0 {
                return Ok(format!("Error: old_string not found in {}", path));
            }
            let new_content = content.replace(old_string, new_string);
            let mut file = self.open_beneath(
                &rel,
                OpenFlags::WRONLY | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
                0,
            )?;
            use std::io::Write;
            file.write_all(new_content.as_bytes())?;
            if count > REPLACE_ALL_WARN_THRESHOLD {
                return Ok(format!(
                    "Replaced {} occurrence(s) in {} (warning: large count, \
                     verify this was intended)",
                    count, path
                ));
            }
            return Ok(format!("Replaced {} occurrence(s) in {}", count, path));
        }

        let first = content.find(old_string);
        let last = content.rfind(old_string);
        match (first, last) {
            (Some(f), Some(l)) if f == l => {
                let mut new_content = String::with_capacity(content.len() + new_string.len());
                new_content.push_str(&content[..f]);
                new_content.push_str(new_string);
                new_content.push_str(&content[f + old_string.len()..]);
                let mut file = self.open_beneath(
                    &rel,
                    OpenFlags::WRONLY | OpenFlags::TRUNC | OpenFlags::CLOEXEC,
                    0,
                )?;
                use std::io::Write;
                file.write_all(new_content.as_bytes())?;
                Ok(format!("Edited {}", path))
            }
            (Some(_), Some(_)) => Ok(format!(
                "Error: old_string is not unique in {}. \
                     Provide more context or use replace_all.",
                path
            )),
            _ => Ok(format!("Error: old_string not found in {}", path)),
        }
    }

    /// Regex content search (Grok Build `grep` semantics, pure-Rust fallback).
    ///
    /// Walks the workspace collecting file paths, then searches them in
    /// parallel. Files larger than 1 MiB are skipped to avoid dominating the
    /// search. Returns early once `max_results` matches are found.
    pub fn grep(
        &self,
        pattern: &str,
        path: &str,
        include: Option<&str>,
        max_results: usize,
    ) -> Result<String> {
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(format!("Error: invalid regex: {}", e)),
        };
        let search_root = if path.is_empty() {
            self.workspace.clone()
        } else {
            self.safe_resolve(path)?
        };
        if !search_root.exists() {
            return Ok(format!("Error: {} does not exist", path));
        }

        const MAX_FILE_SIZE: u64 = 1_048_576;
        // Cap how many files one grep will walk. Repo workspaces stay far
        // under this; a workspace of `/` (system scope) holds hundreds of
        // thousands of files, so an unscoped grep there must stop somewhere
        // instead of stalling for minutes.
        const MAX_WALK_FILES: usize = 20_000;

        let skip = [
            ".git",
            "node_modules",
            "__pycache__",
            ".venv",
            "venv",
            "target",
            "dist",
            "build",
            // Kernel pseudo-filesystems: huge, side-effectful to read, and
            // meaningless to search. Only relevant when the root is `/`.
            "proc",
            "sys",
            "dev",
            "run",
            "boot",
        ];

        let mut files: Vec<PathBuf> = Vec::new();
        for entry in WalkDir::new(&search_root)
            .into_iter()
            .filter_entry(|e| {
                if e.depth() == 0 {
                    return true;
                }
                let name = e.file_name().to_string_lossy();
                !skip.iter().any(|s| *s == name) && !name.starts_with('.')
            })
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let p = entry.path().to_path_buf();
            if let Some(inc) = include {
                if !glob_matches(&p, inc) {
                    continue;
                }
            }
            if p.metadata()
                .map(|m| m.len() > MAX_FILE_SIZE)
                .unwrap_or(true)
            {
                continue;
            }
            files.push(p);
            if files.len() >= MAX_WALK_FILES {
                break;
            }
        }

        let searched = files.len() as u32;
        let results: Mutex<Vec<String>> = Mutex::new(Vec::new());
        let done = AtomicBool::new(false);
        let next = AtomicU32::new(0);

        std::thread::scope(|s| {
            let num_threads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4)
                .min(files.len().max(1));
            for _ in 0..num_threads {
                s.spawn(|| loop {
                    if done.load(Ordering::Relaxed) {
                        return;
                    }
                    let idx = next.fetch_add(1, Ordering::Relaxed) as usize;
                    if idx >= files.len() {
                        return;
                    }
                    let p = &files[idx];
                    let Ok(text) = std::fs::read_to_string(p) else {
                        continue;
                    };
                    for (i, line) in text.lines().enumerate() {
                        if re.is_match(line) {
                            let rel = p.strip_prefix(&self.workspace).unwrap_or(p);
                            let snippet: String = line.trim().chars().take(220).collect();
                            let mut r = results.lock().unwrap_or_else(|e| e.into_inner());
                            r.push(format!("{}:{}: {}", rel.display(), i + 1, snippet));
                            if r.len() >= max_results {
                                done.store(true, Ordering::Relaxed);
                                return;
                            }
                        }
                    }
                });
            }
        });

        let results = results.into_inner().unwrap_or_else(|e| e.into_inner());
        let tail = if files.len() >= MAX_WALK_FILES {
            format!(
                "\n…[file walk stopped at {MAX_WALK_FILES} files; narrow the search with a path argument]"
            )
        } else {
            String::new()
        };
        Ok(if results.is_empty() {
            format!("No matches (searched {} files){}", searched, tail)
        } else {
            results.join("\n") + &tail
        })
    }
}

/// Literal case-insensitive search (kept for compatibility).
pub(crate) fn sandbox_search_code(
    sandbox: &Sandbox,
    query: &str,
    max_results: usize,
) -> Result<String> {
    let q = query.to_lowercase();
    let exts = [
        "py", "js", "ts", "tsx", "jsx", "rs", "go", "java", "cpp", "c", "h", "md", "txt", "toml",
        "yaml", "yml", "json", "sh", "bash", "css", "html", "sql",
    ];
    const MAX_WALK_FILES: usize = 20_000;
    let skip = [
        ".git",
        "node_modules",
        "__pycache__",
        ".venv",
        "venv",
        "target",
        "dist",
        "build",
        // Kernel pseudo-filesystems (relevant when the root is `/`).
        "proc",
        "sys",
        "dev",
        "run",
        "boot",
    ];
    let mut results = Vec::new();
    let mut searched = 0usize;
    let mut truncated = false;
    for entry in WalkDir::new(&sandbox.workspace)
        .into_iter()
        .filter_entry(|e| {
            if e.depth() == 0 {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !skip.iter().any(|s| *s == name) && !name.starts_with('.')
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        searched += 1;
        if searched > MAX_WALK_FILES {
            truncated = true;
            break;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !exts.contains(&ext) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&q) {
                let rel = path.strip_prefix(&sandbox.workspace).unwrap_or(path);
                let snippet: String = line.trim().chars().take(220).collect();
                results.push(format!("{}:{}: {}", rel.display(), i + 1, snippet));
                if results.len() >= max_results {
                    return Ok(results.join("\n"));
                }
            }
        }
    }
    let mut tail = String::new();
    if truncated {
        tail = format!(
            "\n…[search stopped after {MAX_WALK_FILES} files; narrow the search with a path argument]"
        );
    }
    Ok(if results.is_empty() {
        format!(
            "No matches (searched {} files){}",
            searched.min(MAX_WALK_FILES),
            tail
        )
    } else {
        results.join("\n") + &tail
    })
}
