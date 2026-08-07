//! Mouse selection (copy-on-highlight) over the scrollback.
//!
//! Pure functions operating on pre-wrapped display lines. Kept dependency-free
//! (no terminal, no clipboard) so they can be unit-tested in isolation.

use ratatui::text::{Line, Span};
use std::process::Command;

use super::Theme;

/// A row/column anchor in the *display-line* coordinate space (i.e. indices
/// into the `prewrap_lines` output, not the raw log entries).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayPos {
    pub row: usize,
    pub col: usize,
}

/// An active or completed mouse-drag selection over the scrollback. Both
/// endpoints are display-line coordinates; `start` is the anchor where the
/// press began and `end` is the current/dragged position. Order is normalised
/// when extracting text so the user can drag upwards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub start: DisplayPos,
    pub end: DisplayPos,
}

impl Selection {
    pub fn new(start: DisplayPos, end: DisplayPos) -> Self {
        Self { start, end }
    }

    /// Update the moving endpoint on drag.
    pub fn extend(&mut self, end: DisplayPos) {
        self.end = end;
    }

    /// Order the two endpoints so `lo` ≤ `hi` (row-major).
    pub fn ordered(&self) -> (DisplayPos, DisplayPos) {
        let (lo, hi) = if self.start.row < self.end.row
            || (self.start.row == self.end.row && self.start.col <= self.end.col)
        {
            (self.start, self.end)
        } else {
            (self.end, self.start)
        };
        (lo, hi)
    }
}

/// Reconstruct the selected text from the pre-wrapped display lines.
///
/// `lines` is the output of `prewrap_lines` — one `Line` per visible row. The
/// selection is clamped to the available rows/columns. Multi-row selections
/// join rows with `\n`. This is a pure function so it can be unit-tested
/// without a terminal or clipboard.
pub fn selection_text(lines: &[Line<'static>], sel: Selection) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let (lo, hi) = sel.ordered();
    if lo.row >= lines.len() {
        return String::new();
    }
    let last_row = lines.len().saturating_sub(1);
    let hi_row = hi.row.min(last_row);

    let mut out = String::new();
    for (r, row_line) in lines.iter().enumerate().take(hi_row + 1).skip(lo.row) {
        let row_text: String = row_line.spans.iter().map(|s| s.content.as_ref()).collect();
        let chars: Vec<char> = row_text.chars().collect();
        let row_len = chars.len();
        if r == lo.row && r == hi_row {
            // single row slice
            let start = lo.col.min(row_len);
            let end = hi.col.min(row_len).max(start);
            for &c in &chars[start..end] {
                out.push(c);
            }
        } else if r == lo.row {
            let start = lo.col.min(row_len);
            for &c in &chars[start..] {
                out.push(c);
            }
        } else if r == hi_row {
            let end = hi.col.min(row_len);
            for &c in &chars[..end] {
                out.push(c);
            }
            break;
        } else {
            out.push_str(&row_text);
        }
        if r < hi_row {
            out.push('\n');
        }
    }
    out
}

/// Apply a `SELECT_BG` highlight to the lines within the selection range.
///
/// Each display line is split into up to three spans: before the selection,
/// the selected segment (with `SELECT_BG`), and after. Lines outside the
/// selection are untouched. This is a pure transform on display lines.
pub fn apply_selection_highlight(
    lines: Vec<Line<'static>>,
    sel: Option<Selection>,
) -> Vec<Line<'static>> {
    let Some(sel) = sel else {
        return lines;
    };
    let (lo, hi) = sel.ordered();
    lines
        .into_iter()
        .enumerate()
        .map(|(row, line)| {
            if row < lo.row || row > hi.row {
                return line;
            }
            // Character range of the selection on this row.
            let (start, end) = if row == lo.row && row == hi.row {
                (lo.col, hi.col)
            } else if row == lo.row {
                (lo.col, usize::MAX)
            } else if row == hi.row {
                (0, hi.col)
            } else {
                (0, usize::MAX)
            };

            // Walk the original spans, splitting each at the selection
            // boundaries so every span keeps its own style. Only the selected
            // segment gets the SELECT_BG background.
            let mut out: Vec<Span<'static>> = Vec::new();
            let mut offset = 0usize;
            for span in line.spans {
                let text = span.content.as_ref();
                let span_len = text.chars().count();
                let span_end = offset + span_len;

                let sel_start = start.max(offset);
                let sel_end = end.min(span_end);
                if sel_start < sel_end {
                    // Split into before / selected / after, each styled.
                    let before: String = text.chars().take(sel_start - offset).collect();
                    let mid: String = text
                        .chars()
                        .skip(sel_start - offset)
                        .take(sel_end - sel_start)
                        .collect();
                    let after: String = text.chars().skip(sel_end - offset).collect();
                    if !before.is_empty() {
                        out.push(Span::styled(before, span.style));
                    }
                    out.push(Span::styled(mid, span.style.bg(Theme::SELECT_BG)));
                    if !after.is_empty() {
                        out.push(Span::styled(after, span.style));
                    }
                } else {
                    out.push(span);
                }
                offset = span_end;
            }
            Line::from(out)
        })
        .collect()
}

/// Select the word under a display-line position. A "word" is a maximal run of
/// non-whitespace characters. Returns `None` if the position is outside the
/// lines or on whitespace.
pub fn word_bounds(lines: &[Line<'static>], pos: DisplayPos) -> Option<Selection> {
    let row_text: String = lines
        .get(pos.row)?
        .spans
        .iter()
        .map(|s| s.content.as_ref())
        .collect();
    let chars: Vec<char> = row_text.chars().collect();
    let col = pos.col.min(chars.len().saturating_sub(1));
    let is_ws = |c: char| c.is_whitespace();
    if chars.is_empty() || is_ws(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && !is_ws(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col + 1;
    while end < chars.len() && !is_ws(chars[end]) {
        end += 1;
    }
    Some(Selection::new(
        DisplayPos {
            row: pos.row,
            col: start,
        },
        DisplayPos {
            row: pos.row,
            col: end,
        },
    ))
}

/// Best-effort clipboard write shelling out to the platform clipboard tool.
/// Returns the number of characters copied on success, or `None` if no tool
/// was available/failed. Dependency-free: `pbcopy` (macOS), `wl-copy`
/// (Wayland), `xclip`/`xsel` (X11).
pub fn copy_to_clipboard(text: &str) -> Option<usize> {
    if text.is_empty() {
        return Some(0);
    }
    let n = text.chars().count();
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[
            ("wl-copy", &[]),
            ("xclip", &["-selection", "clipboard"]),
            ("xsel", &["--clipboard", "--input"]),
        ]
    };
    for (bin, args) in candidates {
        let mut cmd = Command::new(bin);
        cmd.args(*args);
        cmd.stdin(std::process::Stdio::piped());
        if let Ok(mut child) = cmd.spawn() {
            if let Some(stdin) = child.stdin.as_mut() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().map(|s| s.success()).unwrap_or(false) {
                return Some(n);
            }
        }
    }
    None
}
