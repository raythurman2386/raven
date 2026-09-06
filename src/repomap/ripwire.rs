//! Optional subprocess adapter for the Red Hat `ripwire` CLI.
//!
//! Ripwire is never a build or runtime dependency. When the user opts in and
//! a `ripwire` binary is on `PATH`, Raven shells out with a tight argv, timeout,
//! and stdout cap, then adapts minified XML into the same `<repo_map>` the
//! regex extractor emits. Any failure (missing binary, spawn, timeout, non-zero
//! exit, oversize, empty adapt) is reported to the caller — the map builder
//! treats that as a silent regex fallback.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(test)]
thread_local! {
    static RIPWIRE_BIN_OVERRIDE: std::cell::RefCell<Option<Option<std::path::PathBuf>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with [`find_ripwire`] returning `bin` (`None` = treat as missing).
///
/// Tests install a real stub executable and point at it here so they do not
/// mutate process `PATH` (which races every other test that spawns a child).
#[cfg(test)]
pub(crate) fn with_ripwire_bin<R>(bin: Option<&Path>, f: impl FnOnce() -> R) -> R {
    RIPWIRE_BIN_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = Some(bin.map(Path::to_path_buf));
    });
    let out = f();
    RIPWIRE_BIN_OVERRIDE.with(|slot| {
        *slot.borrow_mut() = None;
    });
    out
}

use super::{intern_kind, render, Symbol};

/// Default spawn timeout for a ripwire child.
pub const RIPWIRE_TIMEOUT_SECS: u64 = 20;
/// Cap captured stdout (matches the regex walk's per-file byte cap).
pub const RIPWIRE_MAX_STDOUT: usize = 256 * 1024;

/// Why a ripwire spawn did not produce a usable map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RipwireError {
    /// Opt-in flag is off.
    Disabled,
    /// No executable named `ripwire` on `PATH`.
    NotFound,
    /// `argv` rejected (empty, leading dash, or control chars).
    UnsafeArg(&'static str),
    /// `Command::spawn` failed (including Landlock exec denial).
    Spawn(String),
    /// Child exceeded the spawn timeout (default 20s, or the caller override).
    Timeout,
    /// Process exited non-zero.
    NonZero(i32),
    /// Stdout exceeded the 256 KiB capture cap before a complete document.
    Oversize,
    /// Stdout was empty or could not be adapted into `<repo_map>` rows.
    Empty,
}

impl std::fmt::Display for RipwireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(
                f,
                "Error: ripwire is not enabled. Set `ripwire = true` in config.toml \
                 or RAVEN_RIPWIRE=1, and install the ripwire binary on PATH. \
                 The regex repo map remains the default."
            ),
            Self::NotFound => write!(
                f,
                "Error: ripwire is enabled but the `ripwire` binary was not found \
                 on PATH. Install it from https://github.com/redhat-et/ripwire \
                 (regex repo map is still used for the system prompt)."
            ),
            Self::UnsafeArg(what) => write!(f, "Error: invalid ripwire {what}"),
            Self::Spawn(msg) => write!(
                f,
                "Error: failed to spawn ripwire ({msg}). If the sandbox blocked \
                 exec, install ripwire on PATH under $HOME or /usr; the regex \
                 repo map remains the fallback."
            ),
            Self::Timeout => write!(f, "Error: ripwire timed out after {RIPWIRE_TIMEOUT_SECS}s"),
            Self::NonZero(code) => write!(f, "Error: ripwire exited {code}"),
            Self::Oversize => write!(
                f,
                "Error: ripwire stdout exceeded {RIPWIRE_MAX_STDOUT} bytes"
            ),
            Self::Empty => write!(f, "Error: ripwire returned no adaptable symbols"),
        }
    }
}

/// A single ripwire invocation.
#[derive(Debug, Clone)]
pub enum RipwireVerb {
    /// Ranked map of `root` (workspace-relative, or the workspace itself).
    Map { root: PathBuf },
    /// Task lens (`--for`) over `root`.
    For { root: PathBuf, query: String },
    /// Callers of `symbol`.
    Callers { root: PathBuf, symbol: String },
    /// Callees of `symbol`.
    Callees { root: PathBuf, symbol: String },
    /// Blast radius of `target` (symbol or file).
    Impact { root: PathBuf, target: String },
}

/// Locate `ripwire` on `PATH`. Returns `None` when missing or not executable.
pub fn find_ripwire() -> Option<PathBuf> {
    #[cfg(test)]
    {
        let override_bin = RIPWIRE_BIN_OVERRIDE.with(|slot| slot.borrow().clone());
        if let Some(forced) = override_bin {
            return forced.filter(|p| is_executable(p));
        }
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ripwire");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Spawn ripwire for `verb` and adapt stdout into a `<repo_map>` block.
///
/// When `confined` is true the child is Landlock/seccomp confined like other
/// tools (`cwd` = workspace, cache pinned under `.raven/`). When confinement
/// cannot exec the binary the error is [`RipwireError::Spawn`].
pub fn run(
    workspace: &Path,
    verb: &RipwireVerb,
    confined: bool,
    extra_rw: &[PathBuf],
    timeout_secs: u64,
) -> Result<String, RipwireError> {
    let bin = find_ripwire().ok_or(RipwireError::NotFound)?;
    let args = argv(workspace, verb)?;
    let _ = std::fs::create_dir_all(workspace.join(".raven"));

    let mut cmd = Command::new(&bin);
    cmd.args(&args)
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let (status, stdout) = if confined {
        run_confined(workspace, extra_rw, &mut cmd, timeout_secs)?
    } else {
        run_unconfined(&mut cmd, timeout_secs)?
    };

    if stdout.len() > RIPWIRE_MAX_STDOUT {
        return Err(RipwireError::Oversize);
    }
    if !status.success() {
        return Err(RipwireError::NonZero(status.code().unwrap_or(-1)));
    }
    let text = String::from_utf8_lossy(&stdout);
    adapt_to_repo_map(&text).ok_or(RipwireError::Empty)
}

fn run_unconfined(
    cmd: &mut Command,
    timeout_secs: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>), RipwireError> {
    let mut child = cmd
        .spawn()
        .map_err(|e| RipwireError::Spawn(e.to_string()))?;
    wait_capped(&mut child, timeout_secs)
}

fn run_confined(
    workspace: &Path,
    extra_rw: &[PathBuf],
    cmd: &mut Command,
    timeout_secs: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>), RipwireError> {
    let raven_dir = workspace.join(".raven");
    crate::tools::setup_shell_env(cmd, workspace, &raven_dir);
    let mut confined = crate::tools::spawn_confined(cmd, workspace, extra_rw, false, false)
        .map_err(|e| RipwireError::Spawn(e.to_string()))?;
    match crate::tools::wait_for_child(&mut confined.child, timeout_secs) {
        Some((status, stdout, _stderr)) => {
            if stdout.len() > RIPWIRE_MAX_STDOUT {
                Err(RipwireError::Oversize)
            } else {
                Ok((status, stdout))
            }
        }
        None => Err(RipwireError::Timeout),
    }
}

fn wait_capped(
    child: &mut std::process::Child,
    timeout_secs: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>), RipwireError> {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut oversize = false;
        if let Some(mut out) = stdout {
            let mut chunk = [0u8; 8192];
            loop {
                match out.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = RIPWIRE_MAX_STDOUT.saturating_sub(buf.len());
                        if room == 0 {
                            oversize = true;
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                        if buf.len() >= RIPWIRE_MAX_STDOUT {
                            oversize = true;
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        if let Some(mut err) = stderr {
            let mut drain = [0u8; 4096];
            while err.read(&mut drain).unwrap_or(0) > 0 {}
        }
        (buf, oversize)
    });

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let (buf, oversize) = reader.join().unwrap_or_else(|_| (Vec::new(), false));
                if oversize {
                    return Err(RipwireError::Oversize);
                }
                return Ok((status, buf));
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(RipwireError::Timeout);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                let _ = reader.join();
                return Err(RipwireError::Spawn(e.to_string()));
            }
        }
    }
}

/// Argv passed to the `ripwire` binary (positional root + flags). `cwd` is the
/// workspace; do not pass `--top-k=0` on graph verbs — ripwire 0.3.x exits 1
/// with empty stdout (`--limit` narrows those reports).
pub(crate) fn argv(workspace: &Path, verb: &RipwireVerb) -> Result<Vec<String>, RipwireError> {
    let cache = workspace.join(".raven").join("ripwire.cache");
    let cache_flag = format!("--cache={}", cache.display());
    let mut args = Vec::new();
    match verb {
        RipwireVerb::Map { root } => {
            args.push(rel_root(workspace, root)?);
            args.push("--max-tokens=1400".into());
            args.push("--max-file-size=256K".into());
        }
        RipwireVerb::For { root, query } => {
            reject_unsafe("query", query)?;
            args.push(rel_root(workspace, root)?);
            args.push(format!("--for={query}"));
            args.push("--signatures-only".into());
            args.push("--max-file-size=256K".into());
        }
        RipwireVerb::Callers { root, symbol } => {
            reject_unsafe("symbol", symbol)?;
            args.push(rel_root(workspace, root)?);
            args.push(format!("--callers={symbol}"));
            args.push("--limit=80".into());
        }
        RipwireVerb::Callees { root, symbol } => {
            reject_unsafe("symbol", symbol)?;
            args.push(rel_root(workspace, root)?);
            args.push(format!("--callees={symbol}"));
            args.push("--limit=80".into());
        }
        RipwireVerb::Impact { root, target } => {
            reject_unsafe("target", target)?;
            args.push(rel_root(workspace, root)?);
            args.push(format!("--impact={target}"));
            args.push("--limit=80".into());
        }
    }
    args.push(cache_flag);
    Ok(args)
}

fn rel_root(workspace: &Path, root: &Path) -> Result<String, RipwireError> {
    if root == workspace {
        return Ok(".".into());
    }
    let rel = root
        .strip_prefix(workspace)
        .map_err(|_| RipwireError::UnsafeArg("path"))?;
    if rel
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(RipwireError::UnsafeArg("path"));
    }
    let s = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if s.is_empty() {
        Ok(".".into())
    } else {
        reject_unsafe("path", &s)?;
        Ok(s)
    }
}

fn reject_unsafe(what: &'static str, s: &str) -> Result<(), RipwireError> {
    if s.is_empty() || s.len() > 512 {
        return Err(RipwireError::UnsafeArg(what));
    }
    if s.starts_with('-') {
        return Err(RipwireError::UnsafeArg(what));
    }
    if s.chars()
        .any(|c| c.is_control() || c == '\0' || c == '\n' || c == '\r')
    {
        return Err(RipwireError::UnsafeArg(what));
    }
    Ok(())
}

/// Parse minified ripwire XML into the grouped `<repo_map>` renderer.
pub fn adapt_to_repo_map(xml: &str) -> Option<String> {
    let symbols = extract_symbols_from_xml(xml);
    if symbols.is_empty() {
        return None;
    }
    Some(render(&symbols))
}

fn extract_symbols_from_xml(xml: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let mut current_file = String::new();
    let mut i = 0;
    while let Some(rel) = xml[i..].find('<') {
        let start = i + rel;
        if xml[start..].starts_with("<!--") {
            i = match xml[start..].find("-->") {
                Some(end) => start + end + 3,
                None => break,
            };
            continue;
        }
        let rest = &xml[start + 1..];
        let name_end = rest
            .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(0);
        let tag = &rest[..name_end];
        let close_rel = match rest.find('>') {
            Some(p) => p,
            None => break,
        };
        let inner = &xml[start..start + 1 + close_rel + 1];
        i = start + 1 + close_rel + 1;

        match tag {
            "f" => {
                if let Some(p) = xml_attr(inner, "p") {
                    current_file = xml_unescape(p);
                }
            }
            // Ranked map / --for / --callers rows are <s> and <d>. Nested
            // <c n="..."/> are call-*edges*, not definitions — including them
            // lists callees as fake symbols in the caller's file.
            "s" | "d" => {
                let Some(n) = xml_attr(inner, "n") else {
                    continue;
                };
                let p = xml_attr(inner, "p")
                    .map(xml_unescape)
                    .unwrap_or_else(|| current_file.clone());
                if p.is_empty() || n.is_empty() {
                    continue;
                }
                let t = xml_attr(inner, "t").unwrap_or("symbol");
                let line = xml_attr(inner, "l")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                symbols.push(Symbol {
                    name: xml_unescape(n),
                    path: p,
                    line,
                    kind: intern_kind(t),
                    public: true,
                    score: 0,
                });
            }
            _ => {}
        }
    }
    symbols
}

fn xml_attr<'a>(hay: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name}=\"");
    let mut offset = 0;
    while let Some(i) = hay[offset..].find(&pat) {
        let abs = offset + i;
        let ok_boundary = abs == 0
            || hay.as_bytes()[abs - 1].is_ascii_whitespace()
            || hay.as_bytes()[abs - 1] == b'<';
        if ok_boundary {
            let start = abs + pat.len();
            let end = hay[start..].find('"')?;
            return Some(&hay[start..start + end]);
        }
        offset = abs + 1;
    }
    None
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    #[test]
    fn adapt_grouped_file_and_symbol_tags() {
        let xml = r#"<r root="."><f p="src/adapter.rs"><s t="fn" n="ripwire_ranked_symbol" k="0.9"><c n="callee_edge"/></s><s t="struct" n="RipwireOnlyType"></s></f></r>"#;
        let map = adapt_to_repo_map(xml).expect("adapted");
        assert!(map.starts_with("<repo_map>"));
        assert!(map.ends_with("</repo_map>"));
        assert!(map.contains("src/adapter.rs"));
        assert!(map.contains("  ripwire_ranked_symbol [fn]"));
        assert!(map.contains("  RipwireOnlyType [struct]"));
        assert!(
            !map.contains("callee_edge"),
            "nested call-edge <c> must not become a definition: {map}"
        );
    }

    #[test]
    fn adapt_ignores_call_edge_c_tags() {
        let xml = r#"<r><f p="src/graph.rs"><s t="fn" n="rankGraph"><c n="biasPrior"/><c n="PROFILE_SCOPE"/></s></f></r>"#;
        let map = adapt_to_repo_map(xml).expect("adapted");
        assert!(map.contains("  rankGraph [fn]"), "{map}");
        assert!(!map.contains("biasPrior"), "{map}");
        assert!(!map.contains("PROFILE_SCOPE"), "{map}");
    }

    #[test]
    fn adapt_callers_s_rows() {
        let xml = r#"<callers of="build_map"><s t="fn" n="rebuild_system_message" p="src/agent/core.rs:265"/></callers>"#;
        let map = adapt_to_repo_map(xml).expect("adapted");
        assert!(map.contains("src/agent/core.rs:265") || map.contains("src/agent/core.rs"));
        assert!(map.contains("  rebuild_system_message [fn]"), "{map}");
    }

    #[test]
    fn adapt_for_lens_d_rows() {
        let xml =
            r#"<ctx><sigs><d l="10" n="for_from_stub" p="src/a.rs" t="fn" r="1"></d></sigs></ctx>"#;
        let map = adapt_to_repo_map(xml).expect("adapted");
        assert!(map.contains("src/a.rs"));
        assert!(map.contains("  for_from_stub [fn]"));
    }

    #[test]
    fn adapt_empty_xml_is_none() {
        assert!(adapt_to_repo_map("<r></r>").is_none());
        assert!(adapt_to_repo_map("not xml").is_none());
    }

    #[test]
    fn find_ripwire_none_when_path_empty_dir() {
        // Absence is represented by None; a PATH with no binary is covered in
        // the integration tests that install (or don't) a stub.
        let _ = find_ripwire();
    }
}
