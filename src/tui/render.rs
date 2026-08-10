//! Log rendering: convert `BlockKind`s into pre-wrapped display lines.
//!
//! `render_blocks` flattens the block scrollback into display lines, and
//! `prewrap_visible` pre-wraps only the visible window each frame so a long
//! session stays O(viewport) to render.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::agent::ChatMessage;

use super::blocks::{AssistantBlock, BlockKind, SystemBlock, ToolBlock, UserBlock};
use super::markdown;
use super::status::spinner_frame;
use super::Theme;

/// Render every block into display lines, returning the count of trailing
/// lines owned by the *last* assistant block (0 if the log ends on any other
/// kind). Mirrors `render_log_lines` but operates on the block model.
///
/// `tick` drives the tool-call "glimmer": an active tool renders bright orange
/// with a spinner; a recently-finished tool fades toward dim over ~50 ticks
/// (~1s at 60ms/frame); an old tool stays dim.
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
                lines.push(Line::from(Span::styled(
                    "You",
                    Style::default().fg(theme.user).add_modifier(Modifier::BOLD),
                )));
                for part in u.text().lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(part.to_string(), Style::default().fg(theme.fg)),
                    ]));
                }
                lines.push(Line::from(""));
                last_assistant_start = None;
            }
            BlockKind::Assistant(a) => {
                last_assistant_start = Some(lines.len());
                lines.push(Line::from(Span::styled(
                    "Raven",
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(markdown::render_markdown(a.text(), theme));
            }
            BlockKind::Tool(t) => {
                let is_last_active = last_active_tool == Some(i);
                let style = tool_style(t, tick, is_last_active, theme);
                let prefix = if is_last_active {
                    format!("{} ", spinner_frame(tick))
                } else {
                    String::new()
                };
                lines.push(Line::from(Span::styled(
                    format!("{prefix}{}", t.text()),
                    style,
                )));
                if let Some(preview) = t.preview.as_ref() {
                    for pline in preview.lines().take(3) {
                        let snip: String = pline.chars().take(120).collect();
                        if snip.trim().is_empty() {
                            continue;
                        }
                        lines.push(Line::from(Span::styled(
                            format!("  {snip}"),
                            Style::default().fg(theme.dim),
                        )));
                    }
                }
                last_assistant_start = None;
            }
            BlockKind::System(s) => {
                if s.text().is_empty() {
                    lines.push(Line::from(""));
                } else {
                    lines.push(Line::from(Span::styled(
                        s.text().to_string(),
                        Style::default().fg(theme.system),
                    )));
                }
                last_assistant_start = None;
            }
            BlockKind::Error(e) => {
                lines.push(Line::from(Span::styled(
                    format!("✗ {}", e.text()),
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                )));
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

/// Render just one assistant text block into display lines (for streaming),
/// including the "Raven" tag so the streaming patch matches `render_blocks`.
pub fn render_assistant_lines(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(text.lines().count().saturating_add(1));
    lines.push(Line::from(Span::styled(
        "Raven",
        Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
    )));
    lines.extend(markdown::render_markdown(text, theme));
    lines
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
        let mut chars = text.chars().peekable();
        loop {
            let mut seg = String::new();
            let mut took = 0usize;
            while let Some(&c) = chars.peek() {
                if c == '\n' {
                    chars.next();
                    break;
                }
                if took == width {
                    break;
                }
                seg.push(c);
                chars.next();
                took += 1;
            }
            if seg.is_empty() && chars.peek().is_none() {
                break;
            }
            let seg_style = line.spans.first().map(|s| s.style).unwrap_or_default();
            out.push(Line::from(Span::styled(seg, seg_style)));
            if chars.peek().is_none() {
                break;
            }
        }
    }
    out
}

/// Number of wrapped rows a single logical line occupies at `width` columns.
/// Mirrors `prewrap_lines` exactly (same char-count + `\n` break semantics,
/// including the empty/trailing-newline case where `prewrap_lines` emits 0
/// rows). Must stay in lockstep with `prewrap_lines` or scroll math drifts.
fn wrapped_row_count(text: &str, width: usize) -> usize {
    let w = width.max(1);
    let mut chars = text.chars().peekable();
    let mut rows = 0usize;
    loop {
        let mut seg = String::new();
        let mut took = 0usize;
        while let Some(&c) = chars.peek() {
            if c == '\n' {
                chars.next();
                break;
            }
            if took == w {
                break;
            }
            seg.push(c);
            chars.next();
            took += 1;
        }
        if seg.is_empty() && chars.peek().is_none() {
            break;
        }
        rows += 1;
        if chars.peek().is_none() {
            break;
        }
    }
    rows
}

/// Pre-wrap only the *visible window* of `lines`, given a scroll offset (in
/// display rows) and a viewport height. Returns the visible pre-wrapped lines
/// and the number of rows scrolled past (the `Paragraph::scroll` offset).
///
/// This is the virtualization win: instead of pre-wrapping the whole history
/// every frame, we walk the logical lines accumulating wrapped-row counts
/// until we reach the scroll offset, then pre-wrap only the visible slice.
/// The walk is O(lines-before-scroll) but cheap (row counting, no allocation);
/// the pre-wrap itself is O(viewport).
pub fn prewrap_visible(
    lines: &[Line<'static>],
    width: usize,
    scroll: usize,
    viewport_h: usize,
) -> (Vec<Line<'static>>, u16) {
    if lines.is_empty() || viewport_h == 0 {
        return (Vec::new(), 0);
    }
    // Total wrapped rows across all lines.
    let total_rows: usize = lines
        .iter()
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            wrapped_row_count(&text, width)
        })
        .sum();
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
    use crate::tui::blocks::AssistantBlock;

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
        let (visible, offset) = prewrap_visible(&lines, 10, 0, 2);
        assert_eq!(offset, 1);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["bbbb", "cccc"]);
    }

    #[test]
    fn prewrap_visible_scrolled_to_top_shows_head() {
        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // viewport 2 rows, scroll 1 → top, offset 0
        let (visible, offset) = prewrap_visible(&lines, 10, 1, 2);
        assert_eq!(offset, 0);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["aaaa", "bbbb"]);
    }

    #[test]
    fn prewrap_visible_wraps_long_lines() {
        let lines = mk(&["abcdefghijklmnop"]);
        // width 5 → 4 wrapped rows; viewport 2, scroll 0 → bottom, offset 2
        let (visible, offset) = prewrap_visible(&lines, 5, 0, 2);
        assert_eq!(offset, 2);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["klmno", "p"]);
    }

    #[test]
    fn prewrap_visible_scroll_clamped_to_top() {
        let lines = mk(&["aaaa", "bbbb", "cccc"]);
        // scroll way past the end → clamp to max_scroll (1), offset = 0 (top)
        let (visible, offset) = prewrap_visible(&lines, 10, 99, 2);
        assert_eq!(offset, 0);
        let text: Vec<String> = visible.iter().map(|l| l.to_string()).collect();
        assert_eq!(text, vec!["aaaa", "bbbb"]);
    }

    #[test]
    fn prewrap_visible_empty() {
        let (visible, offset) = prewrap_visible(&[], 10, 0, 5);
        assert!(visible.is_empty());
        assert_eq!(offset, 0);
    }

    #[test]
    fn wrapped_row_count_matches_prewrap_lines() {
        // `wrapped_row_count` must stay in lockstep with `prewrap_lines` or
        // scroll math drifts. Check the tricky cases: empty, trailing newline,
        // multi-line, and exact-width wraps.
        for (text, width) in [
            ("", 10),
            ("\n", 10),
            ("abc", 10),
            ("abc\n", 10),
            ("abc\ndef", 10),
            ("abcdefghij", 5),
            ("abcdefghijklmnop", 5),
            ("a\nb\nc", 3),
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
        let (visible, _offset) = prewrap_visible(&lines, 10, 0, 2);
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
    fn render_blocks_adds_raven_tag() {
        let blocks = vec![BlockKind::Assistant(AssistantBlock::new("hi".to_string()))];
        let (lines, _tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let first = lines[0].to_string();
        assert!(
            first.contains("Raven"),
            "assistant block should be tagged Raven, got {first:?}"
        );
    }

    #[test]
    fn render_assistant_lines_includes_raven_tag() {
        // The streaming patch must match `render_blocks` (tag + text) so the
        // "Raven" tag doesn't flicker out mid-stream.
        let lines = render_assistant_lines("hello\n\nworld", Theme::RAVENWOOD);
        assert_eq!(lines.len(), 3, "tag + 2 paragraph lines");
        assert!(lines[0].to_string().contains("Raven"));
        assert_eq!(lines[1].to_string(), "hello");
        assert_eq!(lines[2].to_string(), "world");
    }

    #[test]
    fn render_blocks_active_tool_glimmers() {
        let mut tb = ToolBlock::new("⇢ read_file(x)".to_string());
        tb.active = true;
        let blocks = vec![BlockKind::Tool(tb)];
        let (lines, _tail) = render_blocks(&blocks, 0, Theme::RAVENWOOD);
        let text = lines[0].to_string();
        assert!(
            text.contains("⇢ read_file(x)"),
            "tool text should render, got {text:?}"
        );
        // Active tool should be bold (glimmer).
        assert!(lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
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

        assert_eq!(lines.len(), 2, "both tool lines should render");
        let rendered = lines.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        assert!(
            rendered.iter().any(|l| l.contains("search_code(b)")),
            "newest tool should render, got {rendered:?}"
        );
        assert!(
            rendered.iter().any(|l| l.contains("read_file(a)")),
            "earlier tool should render too, got {rendered:?}"
        );
        // Newest active tool glimmers (bold + spinner prefix).
        assert!(
            lines[1].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "newest active tool should be bold, got {rendered:?}"
        );
        assert!(
            rendered[1].starts_with('⠋'),
            "newest active tool should carry a spinner prefix, got {rendered:?}"
        );
    }

    #[test]
    fn render_blocks_finished_tool_fades_to_dim() {
        // Freshly finished → mid-fade (not dim yet).
        let mut tb = ToolBlock::new("⇢ read_file(x)".to_string());
        tb.active = false;
        tb.end_tick = Some(0);
        let blocks = vec![BlockKind::Tool(tb)];
        let (lines, _tail) = render_blocks(&blocks, 10, Theme::RAVENWOOD);
        let style = lines[0].spans[0].style;
        assert!(
            !style.add_modifier.contains(Modifier::BOLD),
            "finished tool should not be bold"
        );

        // Old finished → fully dim.
        let mut tb2 = ToolBlock::new("⇢ read_file(x)".to_string());
        tb2.active = false;
        tb2.end_tick = Some(0);
        let blocks2 = vec![BlockKind::Tool(tb2)];
        let (lines2, _tail) = render_blocks(&blocks2, 1000, Theme::RAVENWOOD);
        assert_eq!(lines2[0].spans[0].style.fg, Some(Theme::RAVENWOOD.dim));
    }
}
