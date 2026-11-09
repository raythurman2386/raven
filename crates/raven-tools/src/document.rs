//! Document-to-text extraction for `read_file`.
//!
//! Mirrors Hermes Agent's `read_extract.py`: when `read_file` targets a
//! non-text document (`.docx`, `.pdf`, `.xlsx`, `.odt`, `.epub`, ...), we
//! convert it to GitHub-Flavored Markdown so the model can read it instead of
//! hitting a binary blob. Conversion is delegated to the [`anydoc`] crate —
//! the same Rust core Hermes uses through its `firecrawl-anydoc` binding —
//! which runs entirely locally (no API key, no network).
//!
//! [`anydoc`]: https://crates.io/crates/anydoc

use anyhow::{bail, Result};
use std::path::Path;

/// Known binary extensions that are *not* convertible documents. These are
/// blocked outright by `read_file` (images, audio, video, archives, ...).
/// Convertible document extensions are handled by [`anydoc`] and never reach
/// this set.
const BINARY_EXTENSIONS: &[&str] = &[
    // Images
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "webp", "tiff", "tif", "svg", // Video
    "mp4", "mov", "avi", "mkv", "webm", "wmv", "flv", "m4v", "mpeg", "mpg", // Audio
    "mp3", "wav", "ogg", "flac", "aac", "m4a", "wma", "aiff", "opus", // Archives
    "zip", "tar", "gz", "bz2", "7z", "rar", "xz", "z", "tgz", "iso",
    // Executables / binaries
    "exe", "dll", "so", "dylib", "bin", "o", "a", "obj", "lib", "app", "msi", "deb", "rpm",
    // Fonts
    "ttf", "otf", "woff", "woff2", "eot", // Bytecode / VM artifacts
    "pyc", "pyo", "class", "jar", "war", "ear", "node", "wasm", "rlib",
    // Database files
    "sqlite", "sqlite3", "db", "mdb", "idx", // Design / 3D
    "psd", "ai", "eps", "sketch", "fig", "xd", "blend", "3ds", "max", // Flash
    "swf", "fla", // Lock / profiling data
    "lockb", "dat", "data",
];

/// Whether `path` names a known binary file that cannot be read as text or
/// converted to text. Pure string check, no I/O.
pub fn has_binary_extension(path: &str) -> bool {
    let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) else {
        return false;
    };
    BINARY_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str())
}

/// Whether `path` is a document `anydoc` can convert to Markdown.
///
/// Uses the extension only (no I/O), matching how `read_file` decides whether
/// to attempt extraction before falling through to the binary guard.
pub fn is_extractable_document(path: &str) -> bool {
    anydoc::Format::from_path(Path::new(path)).is_some()
}

/// Convert a document at `path` to Markdown text.
///
/// The format is detected from the file content, with the extension as a
/// fallback (mirroring `anydoc`'s own example). Returns an error string
/// suitable for surfacing to the model when conversion is impossible.
pub fn extract_document_text(path: &str) -> Result<String> {
    let bytes = std::fs::read(path).map_err(|e| anyhow::anyhow!("read document: {e}"))?;
    let format =
        anydoc::Format::from_bytes(&bytes).or_else(|| anydoc::Format::from_path(Path::new(path)));
    let Some(format) = format else {
        bail!("unrecognized document content and extension: {path}");
    };
    let markdown = anydoc::to_markdown_bytes(&bytes, format)
        .map_err(|e| anyhow::anyhow!("convert document: {e}"))?;
    if markdown.trim().is_empty() {
        bail!("document contains no extractable text: {path}");
    }
    Ok(markdown)
}

/// Read a workspace file for the model.
///
/// Extractable documents become Markdown. Known binary extensions are
/// rejected. Everything else uses the sandbox text path.
pub fn read_file(
    sandbox: &raven_sandbox::Sandbox,
    path: &str,
    start_line: usize,
    max_lines: usize,
) -> Result<String> {
    let p = sandbox.safe_resolve(path)?;
    if p.exists() && p.is_file() {
        // Structured-document extraction: try before the binary guard so
        // .docx/.xlsx/.pdf render as text. Malformed documents fall through
        // to the normal text/binary handling.
        if is_extractable_document(path) {
            match extract_document_text(&p.to_string_lossy()) {
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
                        let truncated: String =
                            line.chars().take(raven_sandbox::MAX_LINE_LENGTH).collect();
                        let rendered = raven_sandbox::render_numbered_line(
                            start + i + 1,
                            &truncated,
                            sandbox.sparse_lines,
                            i == 0,
                        );
                        let rendered_len = rendered.chars().count();
                        if used + rendered_len > raven_sandbox::MAX_TOOL_OUTPUT {
                            out.push_str(&format!(
                                "…[truncated at {} chars]",
                                raven_sandbox::MAX_TOOL_OUTPUT
                            ));
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
        if has_binary_extension(path) {
            return Ok(format!(
                "Error: {} is a binary file. Use list_dir to see available files, or run_shell to inspect it.",
                path
            ));
        }
    }
    sandbox.read_file(path, start_line, max_lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn binary_extensions_are_detected() {
        assert!(has_binary_extension("photo.png"));
        assert!(has_binary_extension("clip.mp4"));
        assert!(has_binary_extension("archive.tar.gz"));
        assert!(has_binary_extension("UPPER.PNG"));
    }

    #[test]
    fn text_and_documents_are_not_binary() {
        assert!(!has_binary_extension("main.rs"));
        assert!(!has_binary_extension("README.md"));
        assert!(!has_binary_extension("report.pdf"));
        assert!(!has_binary_extension("no_extension"));
    }

    #[test]
    fn extractable_documents_are_detected() {
        assert!(is_extractable_document("report.pdf"));
        assert!(is_extractable_document("notes.docx"));
        assert!(is_extractable_document("data.xlsx"));
        assert!(is_extractable_document("book.epub"));
        assert!(is_extractable_document("slides.pptx"));
    }

    #[test]
    fn non_documents_are_not_extractable() {
        assert!(!is_extractable_document("main.rs"));
        assert!(!is_extractable_document("README.md"));
        assert!(!is_extractable_document("photo.png"));
    }

    #[test]
    fn extract_docx_returns_markdown() {
        // Build a minimal valid .docx in-memory: a zip with word/document.xml.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("test.docx");

        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default();
        zip.start_file("word/document.xml", opts).unwrap();
        zip.write_all(
            br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>Hello from a docx</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
        )
        .unwrap();
        let cursor = zip.finish().unwrap();
        std::fs::write(&path, cursor.into_inner()).unwrap();

        let text = extract_document_text(path.to_str().unwrap()).unwrap();
        assert!(text.contains("Hello from a docx"), "text: {text}");
    }

    #[test]
    fn extract_missing_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nope.pdf");
        let res = extract_document_text(path.to_str().unwrap());
        assert!(res.is_err());
    }

    #[test]
    fn extract_proper_docx_with_content_types() {
        // A real-world .docx includes [Content_Types].xml and _rels/.rels.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("proper.docx");

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
    <w:p><w:r><w:t>Raven document extraction works</w:t></w:r></w:p>
    <w:p><w:r><w:t>Second paragraph here</w:t></w:r></w:p>
  </w:body>
</w:document>"#,
        )
        .unwrap();
        let cursor = zip.finish().unwrap();
        std::fs::write(&path, cursor.into_inner()).unwrap();

        let text = extract_document_text(path.to_str().unwrap()).unwrap();
        assert!(
            text.contains("Raven document extraction works"),
            "text: {text}"
        );
        assert!(text.contains("Second paragraph here"), "text: {text}");
    }
}
