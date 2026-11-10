//! Log rendering: convert `BlockKind`s into pre-wrapped display lines.
//!
//! `render_blocks` flattens the block scrollback into display lines, and
//! `prewrap_visible` pre-wraps only the visible window each frame so a long
//! session stays O(viewport) to render.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use huginn_agent::ChatMessage;

use super::blocks::{AssistantBlock, BlockKind, SystemBlock, ToolBlock, UserBlock};
use super::markdown;
use super::status::spinner_frame;
use super::Theme;

/// Render every block into display lines, returning the count of trailing
/// lines owned by the *last* assistant block (0 if the log ends on any other
/// kind). Mirrors `render_log_lines` but operates on the block model.
///
/// `tick` drives the tool-call glimmer: the newest active tool uses
/// `theme.tool` and a spinner; a recently finished tool fades toward dim over
/// ~50 ticks (~1s at 60ms/frame); an older tool stays dim.
pub fn render_blocks(blocks: &[BlockKind], tick: u64, theme: Theme) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::with_capacity(blocks.len().saturating_mul(2));
    let mut last_assistant_start: Option<usize> = None;
    // The last active tool (if any) is the one that glimmers with a spinner.
    let last_active_tool = blocks
        .iter()
        .rposition(|b| matches!(b, BlockKind::Tool(t) if t.active));
    for (i, b) in blocks.iter().enumerate() {
        match b {
            BlockKind::User(u) => {
                push_user(&mut lines, u.text(), theme);
                last_assistant_start = None;
            }
            BlockKind::Assistant(a) => {
                last_assistant_start = Some(lines.len());
                lines.extend(render_assistant_lines(a.text(), theme));
            }
            BlockKind::Tool(t) => {
                push_tool(&mut lines, t, tick, last_active_tool == Some(i), theme);
                last_assistant_start = None;
            }
            BlockKind::System(s) => {
                push_system(&mut lines, s.text(), theme);
                last_assistant_start = None;
            }
            BlockKind::Error(e) => {
                lines.push(Line::from(Span::styled(
                    format!("! {}", e.text()),
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "  retry with a fresh prompt, or /new to reset the session",
                    Style::default().fg(theme.dim),
                )));
                last_assistant_start = None;
            }
            BlockKind::Permission(p) => {
                let rule = Style::default().fg(theme.dim);
                lines.push(Line::from(Span::styled("┌─ permission", rule)));
                lines.push(Line::from(vec![
                    Span::styled("│ $ ", Style::default().fg(theme.dim)),
                    Span::styled(
                        p.command().to_string(),
                        Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
                    ),
                ]));
                lines.push(Line::from(vec![
                    Span::styled("│ ", Style::default().fg(theme.dim)),
                    Span::styled("y allow · n deny", Style::default().fg(theme.dim)),
                ]));
                lines.push(Line::from(Span::styled("└─", rule)));
                last_assistant_start = None;
            }
        }
    }
    let tail_count = match last_assistant_start {
        Some(s) => lines.len().saturating_sub(s),
        None => 0,
    };
    (lines, tail_count)
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans
        .iter()
        .all(|s| s.content.chars().all(char::is_whitespace))
}

/// One blank before a user band when the log already has content. Skip it when
/// the previous block (an assistant reply) already ended on a blank.
fn push_gap(lines: &mut Vec<Line<'static>>) {
    if lines.is_empty() || lines.last().is_some_and(line_is_blank) {
        return;
    }
    lines.push(Line::from(""));
}

fn push_user(lines: &mut Vec<Line<'static>>, text: &str, theme: Theme) {
    push_gap(lines);
    let bar = Style::default().fg(theme.user);
    let body = Style::default().fg(theme.fg).add_modifier(Modifier::BOLD);
    let mut any = false;
    for part in text.lines() {
        any = true;
        let mut spans = vec![Span::styled("▌ ", bar)];
        if !part.is_empty() {
            spans.push(Span::styled(part.to_string(), body));
        }
        lines.push(Line::from(spans));
    }
    if !any {
        lines.push(Line::from(Span::styled("▌ ", bar)));
    }
}

fn push_system(lines: &mut Vec<Line<'static>>, text: &str, theme: Theme) {
    // Empty system blocks are the header spacers. Keep them as one blank —
    // do not prefix them with `· `.
    if text.is_empty() {
        lines.push(Line::from(""));
        return;
    }
    let mut any = false;
    for part in text.lines() {
        any = true;
        if part.is_empty() {
            lines.push(Line::from(""));
        } else {
            lines.push(Line::from(Span::styled(
                format!("· {part}"),
                Style::default().fg(theme.system),
            )));
        }
    }
    if !any {
        lines.push(Line::from(""));
    }
}

fn tool_label(t: &ToolBlock) -> String {
    // `text` is the call line (`⇢ read_file(path)`). The bare name drops the
    // argument, which is the only record of what the tool was aimed at.
    let first = t.text().lines().next().unwrap_or("").trim();
    if !first.is_empty() {
        return first.to_string();
    }
    if !t.name.is_empty() {
        return t.name.clone();
    }
    "tool".to_string()
}

/// Collapse a tool preview onto one line, capped near 80 characters.
fn one_line_preview(preview: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut prev_space = false;
    let mut count = 0usize;
    for ch in preview.chars() {
        let ch = if ch.is_whitespace() { ' ' } else { ch };
        if ch == ' ' {
            if out.is_empty() || prev_space {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        if count >= max_chars {
            break;
        }
        out.push(ch);
        count += 1;
    }
    out.trim_end().to_string()
}

fn push_tool(
    lines: &mut Vec<Line<'static>>,
    t: &ToolBlock,
    tick: u64,
    is_last_active: bool,
    theme: Theme,
) {
    let style = tool_style(t, tick, is_last_active, theme);
    let label = tool_label(t);
    if t.active && is_last_active {
        lines.push(Line::from(Span::styled(
            format!("{} {label}", spinner_frame(tick)),
            style,
        )));
        if let Some(preview) = t.preview.as_deref() {
            let snip = one_line_preview(preview, 80);
            if !snip.is_empty() {
                lines.push(Line::from(Span::styled(
                    format!("  {snip}"),
                    Style::default().fg(theme.dim),
                )));
            }
        }
    } else {
        // Finished, or a parallel tool that is still running: one quiet row.
        // `style` carries the glimmer fade onto the name.
        lines.push(Line::from(Span::styled(format!("· {label}"), style)));
    }
}

/// Style for a tool block based on its glimmer phase.
///
/// - `active` AND it is the most recent active tool → bright orange + bold
///   (the "glimmer" while running).
/// - `active` but not the most recent (a parallel sibling still running) →
///   dimmed, no spinner, so many concurrent tools don't all glow at once.
/// - finished within the last `GLIMMER_TICKS` → interpolate toward dim.
/// - otherwise → dimmed.
fn tool_style(t: &ToolBlock, tick: u64, is_last_active: bool, theme: Theme) -> Style {
    const GLIMMER_TICKS: u64 = 50; // ~1s at 60ms/frame
    if t.active {
        if is_last_active {
            return Style::default().fg(theme.tool).add_modifier(Modifier::BOLD);
        }
        // Running but not the newest — keep it quiet so the UI doesn't
        // spin a spinner on every parallel tool call at once.
        return Style::default().fg(theme.dim);
    }
    match t.end_tick {
        Some(end) => {
            let age = tick.wrapping_sub(end);
            if age < GLIMMER_TICKS {
                // Fade from TOOL toward DIM as the glimmer ages.
                let t = age as f32 / GLIMMER_TICKS as f32;
                let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t) as u8;
                let (r1, g1, b1) = theme.tool_rgb;
                let (r2, g2, b2) = theme.dim_rgb;
                Style::default().fg(Color::Rgb(mix(r1, r2), mix(g1, g2), mix(b1, b2)))
            } else {
                Style::default().fg(theme.dim)
            }
        }
        None => Style::default().fg(theme.dim),
    }
}

/// Render one assistant block the same way [`render_blocks`] does.
///
/// The streaming tail patch replaces this slice in place, so the diamond, the
/// indent, and the single trailing blank have to match or the row flickers.
pub fn render_assistant_lines(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !text.is_empty() {
        let rendered = markdown::render_markdown(text, theme);
        let diamond = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::DIM);
        let mut marked = false;
        for line in rendered {
            if !marked && !line_is_blank(&line) {
                lines.push(prefix_line(line, "◇ ", diamond));
                marked = true;
            } else if marked && !line_is_blank(&line) {
                lines.push(prefix_line(line, "  ", Style::default()));
            } else {
                lines.push(line);
            }
        }
    }
    while lines.last().is_some_and(line_is_blank) {
        lines.pop();
    }
    lines.push(Line::from(""));
    lines
}

fn prefix_line(line: Line<'static>, prefix: &str, style: Style) -> Line<'static> {
    let mut spans = Vec::with_capacity(line.spans.len() + 1);
    spans.push(Span::styled(prefix.to_string(), style));
    spans.extend(line.spans);
    Line::from(spans)
}

/// Pre-wrap each log line to `width` columns so one display row maps to one
/// rendered segment (grok-build's `split_into_line_segments` scrollback
/// model). Without this, `Paragraph::wrap()` expands long lines to several
/// rows while the scroll range is computed from the unwrapped count, so the
/// tail of a long response becomes unreachable.
pub fn prewrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(lines.len().saturating_add(16));
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        if let Some((prefix, cont, body)) = classify_hanging(&text) {
            out.extend(prewrap_hanging(line, prefix, cont, body, width));
        } else {
            out.extend(prewrap_plain(line, width));
        }
    }
    out
}

/// User bars (`▌ `) and the assistant diamond (`◇ `) hang: the marker stays on
/// the first row and later rows indent two columns so the text lines up.
fn classify_hanging(text: &str) -> Option<(&'static str, &'static str, &str)> {
    if let Some(body) = text.strip_prefix("▌ ") {
        Some(("▌ ", "  ", body))
    } else if let Some(body) = text.strip_prefix("◇ ") {
        Some(("◇ ", "  ", body))
    } else {
        None
    }
}

fn cols(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

/// Each span keeps its color. A fully empty line is one spacer row (header
/// gaps, turn spacing). A line that is only a newline still yields nothing.
fn prewrap_plain(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let mut cur = SpanCursor::new(&line.spans);
    if cur.peek().is_none() {
        return vec![Line::from("")];
    }
    let mut out = Vec::new();
    loop {
        let mut row: Vec<Span<'static>> = Vec::new();
        let mut took = 0usize;
        while let Some((c, style)) = cur.peek() {
            if c == '\n' {
                cur.bump();
                break;
            }
            let cw = c.width().unwrap_or(0);
            // A row that already has text breaks before a char that would
            // overflow. An empty row always takes one char, even when that
            // char is wider than the budget, so the cursor advances.
            if took > 0 && took + cw > width {
                break;
            }
            cur.bump();
            took += cw;
            push_styled_char(&mut row, c, style);
            if took >= width {
                break;
            }
        }
        let more = cur.peek().is_some();
        if row.is_empty() && !more {
            break;
        }
        out.push(Line::from(row));
        if !more {
            break;
        }
    }
    out
}

fn prewrap_hanging(
    line: &Line<'static>,
    prefix: &str,
    cont: &str,
    body: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let w = width.max(1);
    let pw = cols(prefix);
    if pw == 0 || w <= pw {
        return prewrap_plain(line, w);
    }
    let body_line = strip_prefix_spans(line, prefix.chars().count());
    debug_assert_eq!(
        body_line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>(),
        body
    );
    let mut rows = prewrap_plain(&body_line, w - pw);
    // An empty body is already one spacer from `prewrap_plain`. A body that
    // wraps to nothing (a lone newline) still needs the marker row.
    if rows.is_empty() {
        rows.push(Line::from(""));
    }
    let prefix_style = line.spans.first().map(|s| s.style).unwrap_or_default();
    let mut out = Vec::with_capacity(rows.len());
    for (i, row) in rows.into_iter().enumerate() {
        let (marker, marker_style) = if i == 0 {
            (prefix, prefix_style)
        } else {
            (cont, Style::default())
        };
        let mut spans = Vec::with_capacity(row.spans.len() + 1);
        spans.push(Span::styled(marker.to_string(), marker_style));
        for span in row.spans {
            if !span.content.is_empty() {
                spans.push(span);
            }
        }
        out.push(Line::from(spans));
    }
    out
}

fn strip_prefix_spans(line: &Line<'static>, nchars: usize) -> Line<'static> {
    let mut skip = nchars;
    let mut spans = Vec::new();
    for span in &line.spans {
        if skip == 0 {
            spans.push(span.clone());
            continue;
        }
        let count = span.content.chars().count();
        if count <= skip {
            skip -= count;
            continue;
        }
        let rest: String = span.content.chars().skip(skip).collect();
        skip = 0;
        if !rest.is_empty() {
            spans.push(Span::styled(rest, span.style));
        }
    }
    Line::from(spans)
}

fn push_styled_char(row: &mut Vec<Span<'static>>, c: char, style: Style) {
    if let Some(last) = row.last_mut() {
        if last.style == style {
            last.content.to_mut().push(c);
            return;
        }
    }
    row.push(Span::styled(c.to_string(), style));
}

struct SpanCursor<'a> {
    spans: &'a [Span<'static>],
    si: usize,
    byte: usize,
}

impl<'a> SpanCursor<'a> {
    fn new(spans: &'a [Span<'static>]) -> Self {
        Self {
            spans,
            si: 0,
            byte: 0,
        }
    }

    fn normalize(&mut self) {
        while self.si < self.spans.len() && self.byte >= self.spans[self.si].content.len() {
            self.si += 1;
            self.byte = 0;
        }
    }

    fn peek(&mut self) -> Option<(char, Style)> {
        self.normalize();
        let span = self.spans.get(self.si)?;
        let c = span.content[self.byte..].chars().next()?;
        Some((c, span.style))
    }

    fn bump(&mut self) {
        let Some((c, _)) = self.peek() else {
            return;
        };
        self.byte += c.len_utf8();
        self.normalize();
    }
}

/// Segment breaks shared by [`prewrap_plain`] and [`wrapped_row_count`].
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut chars = text.chars().peekable();
    loop {
        let mut seg = String::new();
        let mut took = 0usize;
        while let Some(&c) = chars.peek() {
            if c == '\n' {
                chars.next();
                break;
            }
            let cw = c.width().unwrap_or(0);
            // Same rule as `prewrap_plain`: overflow breaks a non-empty row,
            // and an empty row always consumes one char.
            if took > 0 && took + cw > width {
                break;
            }
            seg.push(c);
            chars.next();
            took += cw;
            if took >= width {
                break;
            }
        }
        if seg.is_empty() && chars.peek().is_none() {
            break;
        }
        out.push(seg);
        if chars.peek().is_none() {
            break;
        }
    }
    out
}

/// Number of wrapped rows a single logical line occupies at `width` columns.
/// Mirrors `prewrap_lines` (same char-count + `\n` breaks, hanging markers,
/// and the empty-line spacer). A lone or trailing newline still adds no row.
/// Must stay in lockstep with `prewrap_lines` or scroll math drifts.
fn wrapped_row_count(text: &str, width: usize) -> usize {
    if let Some((prefix, _cont, body)) = classify_hanging(text) {
        return hanging_row_count(prefix, body, width);
    }
    wrap_text(text, width.max(1)).len()
}

fn hanging_row_count(prefix: &str, body: &str, width: usize) -> usize {
    let w = width.max(1);
    let pw = cols(prefix);
    if pw == 0 || w <= pw {
        return wrap_text(&format!("{prefix}{body}"), w).len();
    }
    // At least the marker row, even when the body itself wraps to nothing.
    wrap_text(body, w - pw).len().max(1)
}

/// Total wrapped-row count across all lines at `width` columns. Cached by
/// the caller and passed to `prewrap_visible` so it need not re-walk the
/// whole history every frame.
pub fn total_rows(lines: &[Line<'static>], width: usize) -> usize {
    lines
        .iter()
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            wrapped_row_count(&text, width)
        })
        .sum()
}

/// Pre-wrap only the *visible window* of `lines`, given a scroll offset (in
/// display rows) and a viewport height. Returns the visible pre-wrapped lines
/// and the number of rows scrolled past (the `Paragraph::scroll` offset).
///
/// This is the virtualization win: `total_rows` (the caller's cached sum of
/// per-line wrapped-row counts) avoids re-walking the whole history, and only
/// the visible slice is pre-wrapped each frame — O(viewport) plus the cost of
/// `total_rows`. The walk before the scroll offset is O(lines-before-scroll)
/// but cheap (row counting, no allocation); the pre-wrap itself is O(viewport).
pub fn prewrap_visible(
    lines: &[Line<'static>],
    total_rows: usize,
    width: usize,
    scroll: usize,
    viewport_h: usize,
) -> (Vec<Line<'static>>, u16) {
    if lines.is_empty() || viewport_h == 0 {
        return (Vec::new(), 0);
    }
    let max_scroll = total_rows.saturating_sub(viewport_h);
    let scroll_eff = scroll.min(max_scroll);
    // The Paragraph scroll offset is inverted: 0 = top, max_scroll = bottom.
    let offset = max_scroll.saturating_sub(scroll_eff);

    // Walk logical lines, skipping `offset` wrapped rows, collecting up to
    // `viewport_h` wrapped rows.
    let mut out = Vec::with_capacity(viewport_h);
    let mut rows_skipped = 0usize;
    let mut collecting = false;
    for line in lines {
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        let n = wrapped_row_count(&text, width);
        if !collecting {
            if rows_skipped + n <= offset {
                rows_skipped += n;
                continue;
            }
            collecting = true;
        }
        // Pre-wrap this line, but only keep the rows that fall in the window.
        let wrapped = prewrap_lines(std::slice::from_ref(line), width);
        let start_row = offset.saturating_sub(rows_skipped);
        rows_skipped += n;
        for row in wrapped.into_iter().skip(start_row) {
            if out.len() >= viewport_h {
                break;
            }
            out.push(row);
        }
        if out.len() >= viewport_h {
            break;
        }
    }
    (out, offset as u16)
}

/// Convert a persisted chat message into a display block.
pub fn message_to_block(msg: &ChatMessage) -> Option<BlockKind> {
    match msg.role.as_str() {
        "user" => msg
            .content
            .as_ref()
            .map(|c| BlockKind::User(UserBlock::new(c.clone()))),
        "assistant" => {
            if let Some(content) = &msg.content {
                if !content.is_empty() {
                    return Some(BlockKind::Assistant(AssistantBlock::new(content.clone())));
                }
            }
            if let Some(tool_calls) = &msg.tool_calls {
                let mut text = String::new();
                for tc in tool_calls {
                    let args_snip: String = tc.function.arguments.chars().take(60).collect();
                    text.push_str(&format!("⇢ {}({})\n", tc.function.name, args_snip));
                }
                if !text.is_empty() {
                    return Some(BlockKind::Tool(ToolBlock::new(text.trim_end().to_string())));
                }
            }
            None
        }
        "tool" => msg.content.as_ref().map(|c| {
            let preview: String = c.chars().take(200).collect();
            BlockKind::Tool(ToolBlock::new(format!("[tool result] {}", preview)))
        }),
        "system" => msg
            .content
            .as_ref()
            .map(|c| BlockKind::System(SystemBlock::new(c.clone()))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blocks::{AssistantBlock, ErrorBlock};

    fn mk(texts: &[&str]) -> Vec<Line<'static>> {
        texts
            .iter()
            .map(|t| Line::from(Span::styled(t.to_string(), Style::default())))
            .collect()
    }

    #[test]
    fn prewrap_visible_bottom_shows_tail() {
        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // viewport 2 rows, scroll 0 → bottom (auto-follow), offset = max_scroll = 1
        let total = total_rows(&lines, 10);
        let (visible, offset) = prewrap_visible(&lines, total, 10, 0, 2);
        assert_eq!(offset, 1);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["bbbb", "cccc"]);
    }

    #[test]
    fn prewrap_visible_scrolled_to_top_shows_head() {
        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // viewport 2 rows, scroll 1 → top, offset 0
        let total = total_rows(&lines, 10);
        let (visible, offset) = prewrap_visible(&lines, total, 10, 1, 2);
        assert_eq!(offset, 0);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["aaaa", "bbbb"]);
    }

    #[test]
    fn prewrap_visible_wraps_long_lines() {
        let lines = mk(&["abcdefghijklmnop"]);
        // width 5 → 4 wrapped rows; viewport 2, scroll 0 → bottom, offset 2
        let total = total_rows(&lines, 5);
        let (visible, offset) = prewrap_visible(&lines, total, 5, 0, 2);
        assert_eq!(offset, 2);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["klmno", "p"]);
    }

    #[test]
    fn prewrap_visible_scroll_clamped_to_top() {
        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // scroll way past the end → clamp to max_scroll (1), offset = 0 (top)
        let total = total_rows(&lines, 10);
        let (visible, offset) = prewrap_visible(&lines, total, 10, 99, 2);
        assert_eq!(offset, 0);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["aaaa", "bbbb"]);
    }

    #[test]
    fn prewrap_visible_empty() {
        let total = total_rows(&[], 10);
        let (visible, offset) = prewrap_visible(&[], total, 10, 0, 5);
        assert!(visible.is_empty());
        assert_eq!(offset, 0);
    }

    #[test]
    fn total_rows_sums_wrapped_counts() {
        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // width 10: each line is 1 row, total 3.
        assert_eq!(total_rows(&lines, 10), 3);
        // width 2: each "aaaa" wraps to 2 rows => 6.
        assert_eq!(total_rows(&lines, 2), 6);
    }

    #[test]
    fn wrapped_row_count_matches_prewrap_lines() {
        // `wrapped_row_count` must stay in lockstep with `prewrap_lines` or
        // scroll math drifts. Empty logical lines are one spacer row; a lone
        // newline still adds none. Also check multi-line and exact-width wraps.
        for (text, width) in [
            ("", 10),
            ("\n", 10),
            ("abc", 10),
            ("abc\n", 10),
            ("abc\ndef", 10),
            ("abcdefghij", 5),
            ("abcdefghijklmnop", 5),
            ("a\nb\nc", 3),
            ("你", 1),
            ("▌ 你", 3),
            ("◇ 😀", 3),
        ] {
            let line = Line::from(Span::styled(text.to_string(), Style::default()));
            let wrapped = prewrap_lines(std::slice::from_ref(&line), width);
            let count = wrapped_row_count(text, width);
            assert_eq!(
                count,
                wrapped.len(),
                "wrapped_row_count({text:?}, {width}) = {count}, but prewrap_lines emits {} rows",
                wrapped.len()
            );
        }
    }

    #[test]
    fn prewrap_widths_wide_chars_match_row_count() {
        // CJK chars are display-width 2. "你"x6 at width 4 wraps into 3 rows
        // (2 wide chars per row). The wrap math and row count must agree and
        // must break on the display-width boundary.
        let text = "你你你你你你"; // 6 wide chars, display width 12
        let line = Line::from(Span::styled(text.to_string(), Style::default()));
        let wrapped = prewrap_lines(std::slice::from_ref(&line), 4);
        let count = wrapped_row_count(text, 4);
        assert_eq!(count, wrapped.len(), "row count must match prewrap output");
        // 6 wide chars at width 4 => 2 chars per row => 3 rows.
        assert_eq!(count, 3, "expected 3 rows for 6 width-2 chars at width 4");
        // No row should be wider than the budget (4 display columns).
        for row in &wrapped {
            let w: usize = row
                .to_string()
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            assert!(w <= 4, "row width {w} exceeds budget 4");
        }
        // Mixed ASCII + CJK: "ab你cd" at width 4 should not split "你" across rows.
        let mixed = "ab你cd";
        let line2 = Line::from(Span::styled(mixed.to_string(), Style::default()));
        let w2 = prewrap_lines(std::slice::from_ref(&line2), 4);
        let c2 = wrapped_row_count(mixed, 4);
        assert_eq!(c2, w2.len());
        assert!(c2 >= 2, "ab(2) + 你(2) = 4 fits row 1, c(1) wraps to row 2");
    }

    #[test]
    fn prewrap_visible_window_renders_without_double_scroll() {
        // Regression: `prewrap_visible` already slices to the visible window.
        // Rendering those lines through a Paragraph with scroll((0,0)) must
        // show exactly the window — NOT scroll it again (which would push the
        // content off-screen). This mirrors the draw path in `draw_ui`.
        use ratatui::backend::TestBackend;
        use ratatui::widgets::{Block, Borders, Padding, Paragraph};
        use ratatui::Terminal;

        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // viewport 2 rows, auto-follow bottom (scroll=0) → window = ["bbbb","cccc"]
        let total = total_rows(&lines, 10);
        let (visible, _offset) = prewrap_visible(&lines, total, 10, 0, 2);
        assert_eq!(
            visible.iter().map(|l| l.to_string()).collect::<Vec<_>>(),
            vec!["bbbb", "cccc"]
        );

        // Render the window through a Paragraph with scroll((0,0)) — the same
        // as the fixed draw path. Terminal is 3 rows tall so the bottom border
        // (1 row) leaves 2 content rows.
        let backend = TestBackend::new(10, 3);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                let widget = Paragraph::new(visible.clone())
                    .block(
                        Block::default()
                            .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                            .padding(Padding::horizontal(1)),
                    )
                    .scroll((0, 0));
                f.render_widget(widget, area);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // Content starts at x=2 (left border + padding), y=0. Read 4 cols.
        let row0: String = (2..6).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        let row1: String = (2..6).map(|x| buf[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row0, "bbbb", "row 0 should be 'bbbb', got {row0:?}");
        assert_eq!(row1, "cccc", "row 1 should be 'cccc', got {row1:?}");
    }

    #[test]
    fn render_blocks_error_has_recovery_line() {
        let blocks = vec![BlockKind::Error(ErrorBlock::new("boom".to_string()))];
        let (lines, _tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|l| l.contains("! boom")),
            "error line should render, got {rendered:?}"
        );
        assert!(
            rendered.iter().all(|l| !l.contains('┌')),
            "errors are not boxed, got {rendered:?}"
        );
        assert!(
            rendered
                .iter()
                .any(|l| l.contains("retry with a fresh prompt")),
            "recovery line should render, got {rendered:?}"
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Theme::RAVENWOOD.error));
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    #[test]
    fn render_blocks_user_bar_not_title() {
        let blocks = vec![
            BlockKind::System(SystemBlock::new("ready".to_string())),
            BlockKind::User(UserBlock::new("ship the scrollback".to_string())),
        ];
        let (lines, _tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert!(
            rendered.iter().all(|l| l.trim() != "You"),
            "user block has no standalone You title, got {rendered:?}"
        );
        let bar = lines
            .iter()
            .find(|l| l.to_string().contains('▌'))
            .expect("user bar");
        assert!(bar.to_string().contains("ship the scrollback"));
        assert_eq!(bar.spans[0].style.fg, Some(Theme::RAVENWOOD.user));
        let text = bar
            .spans
            .iter()
            .find(|s| s.content.contains("ship"))
            .expect("user text");
        assert_eq!(text.style.fg, Some(Theme::RAVENWOOD.fg));
        assert!(text.style.add_modifier.contains(Modifier::BOLD));
        // One blank between the system line and the band, not two.
        assert_eq!(rendered, ["· ready", "", "▌ ship the scrollback"]);
    }

    #[test]
    fn render_blocks_assistant_has_no_name_header() {
        let text = "hello\n\nworld";
        let blocks = vec![BlockKind::Assistant(AssistantBlock::new(text.to_string()))];
        let (lines, tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let direct = render_assistant_lines(text, Theme::RAVENWOOD);
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert!(
            rendered.iter().all(|l| !l.contains("Huginn")),
            "assistant block has no Huginn title, got {rendered:?}"
        );
        assert!(rendered[0].starts_with("◇ "), "got {rendered:?}");
        assert!(rendered[0].contains("hello"));
        assert_eq!(rendered[1], "  world");
        assert_eq!(rendered.last().map(String::as_str), Some(""));
        assert_eq!(
            rendered.iter().filter(|l| l.is_empty()).count(),
            1,
            "one trailing blank, not two: {rendered:?}"
        );
        assert_eq!(tail, direct.len());
        assert_eq!(
            rendered,
            direct.iter().map(|l| l.to_string()).collect::<Vec<_>>()
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Theme::RAVENWOOD.accent));
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn render_blocks_assistant_then_user_is_one_blank() {
        let blocks = vec![
            BlockKind::Assistant(AssistantBlock::new("hi".to_string())),
            BlockKind::User(UserBlock::new("ok".to_string())),
        ];
        let (lines, tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(rendered, ["◇ hi", "", "▌ ok"]);
        assert_eq!(tail, 0, "tail clears once a later block is rendered");
    }

    #[test]
    fn render_blocks_system_prefix_and_empty_spacer() {
        let blocks = vec![
            BlockKind::System(SystemBlock::new("huginn".to_string())),
            BlockKind::System(SystemBlock::new(String::new())),
            BlockKind::System(SystemBlock::new("hint".to_string())),
        ];
        let (lines, _) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert_eq!(rendered, ["· huginn", "", "· hint"]);
        assert_eq!(lines[0].spans[0].style.fg, Some(Theme::RAVENWOOD.system));
    }

    #[test]
    fn render_blocks_active_tool_glimmers() {
        let mut tb = ToolBlock::start("read_file", "⇢ read_file(x)".to_string());
        tb.active = true;
        let blocks = vec![BlockKind::Tool(tb)];
        let (lines, _tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        assert_eq!(lines.len(), 1, "active tool is one row");
        let text = lines[0].to_string();
        assert!(
            text.contains("read_file"),
            "active tool line should contain the name, got {text:?}"
        );
        assert!(
            text.contains(spinner_frame(0)),
            "active tool should carry a spinner, got {text:?}"
        );
        assert!(!text.contains('┌'), "no box chrome, got {text:?}");
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(lines[0].spans[0].style.fg, Some(Theme::RAVENWOOD.tool));
    }

    #[test]
    fn render_blocks_active_tool_preview_is_one_dim_line() {
        let mut tb = ToolBlock::start("read_file", "⇢ read_file".to_string());
        tb.set_preview(format!("{}\nsecond line", "x".repeat(200)));
        let (lines, _) = render_blocks(&[BlockKind::Tool(tb)], 0, Theme::RAVENWOOD);
        assert_eq!(lines.len(), 2, "name row plus one preview line");
        assert!(lines[0].to_string().contains("read_file"));
        let preview = lines[1].to_string();
        assert!(
            preview.starts_with("  "),
            "preview is indented, got {preview:?}"
        );
        assert!(!preview.contains('\n'));
        assert!(
            preview.trim().chars().count() <= 80,
            "preview capped near 80 chars, got {} chars",
            preview.trim().chars().count()
        );
        assert!(!preview.contains("second"));
        assert_eq!(lines[1].spans[0].style.fg, Some(Theme::RAVENWOOD.dim));
        assert!(!lines[0].to_string().contains('┌'));
    }

    #[test]
    fn render_blocks_renders_all_tools() {
        // Two parallel tools: both render as live lines so the user can see
        // every tool the agent ran. Only the newest active tool glimmers.
        let mut tb1 = ToolBlock::new("→ read_file(a)".to_string());
        tb1.active = true;
        let mut tb2 = ToolBlock::new("→ search_code(b)".to_string());
        tb2.active = true;
        let blocks = vec![BlockKind::Tool(tb1), BlockKind::Tool(tb2)];
        let (lines, _tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);

        assert_eq!(lines.len(), 2, "each tool is one row");
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|l| l.contains("search_code(b)")),
            "newest tool should render, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("read_file(a)")),
            "earlier tool should render too, got {rendered:?}"
        );
        assert!(
            rendered.iter().all(|l| !l.contains('┌')),
            "tools are not boxed, got {rendered:?}"
        );
        assert!(
            !lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "older parallel tool stays quiet, got {rendered:?}"
        );
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "newest active tool should be bold, got {rendered:?}"
        );
        assert!(
            rendered[1].contains('⠋'),
            "newest active tool should carry a spinner prefix, got {rendered:?}"
        );
        assert!(
            !rendered[0].contains('⠋'),
            "older tool has no spinner, got {rendered:?}"
        );
    }

    #[test]
    fn render_blocks_finished_tool_is_one_line() {
        let mut tb = ToolBlock::start("read_file", "⇢ read_file(a)".to_string());
        tb.active = false;
        tb.end_tick = Some(0);
        tb.set_preview("lots of output that should not show");
        let (lines, _) = render_blocks(&[BlockKind::Tool(tb)], 1000, Theme::RAVENWOOD);
        let content: Vec<_> = lines
            .iter()
            .filter(|l| !l.to_string().trim().is_empty())
            .collect();
        assert_eq!(content.len(), 1, "finished tool is one content line");
        let text = content[0].to_string();
        assert!(text.contains("read_file"), "got {text:?}");
        assert!(text.contains("(a)"), "argument snippet stays, got {text:?}");
        assert!(text.starts_with("· "), "got {text:?}");
        assert!(!text.contains('┌'), "got {text:?}");
        assert!(
            !text.contains("lots of output"),
            "finished tools hide the preview, got {text:?}"
        );
        assert_eq!(content[0].spans[0].style.fg, Some(Theme::RAVENWOOD.dim));
    }

    #[test]
    fn render_blocks_finished_tool_fades_to_dim() {
        // Freshly finished → mid-fade (not dim yet). The glimmer color is on
        // the name row.
        let mut tb = ToolBlock::new("⇢ read_file(x)".to_string());
        tb.active = false;
        tb.end_tick = Some(0);
        let blocks = vec![BlockKind::Tool(tb)];
        let (lines, _tail) = render_blocks(&blocks, 10, Theme::RAVENWOOD);
        assert_eq!(lines.len(), 1);
        let style = lines[0].spans[0].style;
        assert!(
            !style.add_modifier.contains(Modifier::BOLD),
            "finished tool should not be bold"
        );
        assert_ne!(style.fg, Some(Theme::RAVENWOOD.dim));

        // Old finished → fully dim.
        let mut tb2 = ToolBlock::new("⇢ read_file(x)".to_string());
        tb2.active = false;
        tb2.end_tick = Some(0);
        let blocks2 = vec![BlockKind::Tool(tb2)];
        let (lines2, _tail) = render_blocks(&blocks2, 1000, Theme::RAVENWOOD);
        assert_eq!(lines2[0].spans[0].style.fg, Some(Theme::RAVENWOOD.dim));
        assert!(lines2[0].to_string().contains("read_file"));
    }

    #[test]
    fn prewrap_empty_line_is_one_spacer_row() {
        let line = Line::from("");
        let wrapped = prewrap_lines(std::slice::from_ref(&line), 10);
        assert_eq!(wrapped.len(), 1, "header spacers must survive prewrap");
        assert!(wrapped[0].to_string().is_empty());
        assert_eq!(wrapped_row_count("", 10), 1);
        assert_eq!(wrapped_row_count("\n", 10), 0, "a lone newline adds no row");
    }

    #[test]
    fn prewrap_keeps_span_styles() {
        let line = Line::from(vec![
            Span::styled("ab", Style::default().fg(Color::Red)),
            Span::styled(
                "cd",
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        let wrapped = prewrap_lines(std::slice::from_ref(&line), 10);
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].to_string(), "abcd");
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(wrapped[0].spans[1].style.fg, Some(Color::Blue));
        assert!(wrapped[0].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));

        let split = prewrap_lines(std::slice::from_ref(&line), 3);
        assert_eq!(split.len(), wrapped_row_count("abcd", 3));
        assert_eq!(split[0].to_string(), "abc");
        assert_eq!(split[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(split[0].spans[1].content.as_ref(), "c");
        assert_eq!(split[0].spans[1].style.fg, Some(Color::Blue));
        assert_eq!(split[1].to_string(), "d");
        assert_eq!(split[1].spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn prewrap_user_bar_and_diamond_indent_continuations() {
        let user = Line::from(vec![
            Span::styled("▌ ", Style::default().fg(Color::Green)),
            Span::styled(
                "abcdef",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
        let wrapped = prewrap_lines(std::slice::from_ref(&user), 5);
        assert_eq!(wrapped.len(), 2);
        assert_eq!(wrapped_row_count(&user.to_string(), 5), 2);
        assert_eq!(wrapped[0].to_string(), "▌ abc");
        assert_eq!(wrapped[1].to_string(), "  def");
        assert_eq!(wrapped[0].spans[0].style.fg, Some(Color::Green));
        assert_eq!(wrapped[0].spans[1].style.fg, Some(Color::White));
        assert!(wrapped[1].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));

        let diamond = Line::from(vec![
            Span::styled(
                "◇ ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            ),
            Span::styled("abcdef", Style::default().fg(Color::White)),
        ]);
        let w2 = prewrap_lines(std::slice::from_ref(&diamond), 5);
        assert_eq!(w2[0].to_string(), "◇ abc");
        assert_eq!(w2[1].to_string(), "  def");
        assert_eq!(w2[0].spans[0].style.fg, Some(Color::Cyan));
        assert!(w2[0].spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(wrapped_row_count(&diamond.to_string(), 5), w2.len());
    }

    #[test]
    fn prewrap_hanging_places_char_wider_than_the_body_budget() {
        // Log content width 3 leaves a 1-column body after `▌ `. A width-2
        // character must still land, or the wrap retries it forever.
        let line = Line::from(vec![
            Span::styled("▌ ", Style::default()),
            Span::styled("你", Style::default().fg(Color::White)),
        ]);
        let wrapped = prewrap_lines(std::slice::from_ref(&line), 3);
        assert_eq!(wrapped_row_count("▌ 你", 3), wrapped.len());
        assert_eq!(wrapped.len(), 1);
        assert!(wrapped[0].to_string().contains('你'));
        assert_eq!(
            wrapped[0].spans.last().unwrap().style.fg,
            Some(Color::White)
        );
    }
}
