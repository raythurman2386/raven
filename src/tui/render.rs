//! Log rendering: convert `LogEntry`s into pre-wrapped display lines.
//!
//! The current flat-log model renders every entry into display lines on each
//! dirty frame. This module is the seam where the block-based virtualization
//! (Tasks 1-2) will land; for now it preserves the existing behavior.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::agent::ChatMessage;

use super::{LogEntry, LogKind, Theme};

/// Render every log entry into display lines, returning the count of trailing
/// lines owned by the *last* assistant entry (0 if the log ends on any other
/// kind). That tail count lets the streaming path patch just the active
/// assistant block instead of re-rendering the whole log per token.
pub fn render_log_lines(log: &[LogEntry]) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::with_capacity(log.len().saturating_mul(2));
    let mut last_assistant_start: Option<usize> = None;
    for e in log {
        match e.kind {
            LogKind::User => {
                lines.push(Line::from(Span::styled(
                    "You",
                    Style::default()
                        .fg(Theme::USER)
                        .add_modifier(Modifier::BOLD),
                )));
                for part in e.text.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default()),
                        Span::styled(part.to_string(), Style::default().fg(Theme::FG)),
                    ]));
                }
                lines.push(Line::from(""));
                last_assistant_start = None;
            }
            LogKind::Assistant => {
                last_assistant_start = Some(lines.len());
                for part in e.text.lines() {
                    lines.push(Line::from(Span::styled(
                        part.to_string(),
                        Style::default().fg(Theme::FG),
                    )));
                }
            }
            LogKind::Tool => {
                lines.push(Line::from(Span::styled(
                    e.text.clone(),
                    Style::default().fg(Theme::TOOL),
                )));
                last_assistant_start = None;
            }
            LogKind::System => {
                if e.text.is_empty() {
                    lines.push(Line::from(""));
                } else {
                    lines.push(Line::from(Span::styled(
                        e.text.clone(),
                        Style::default().fg(Theme::SYSTEM),
                    )));
                }
                last_assistant_start = None;
            }
            LogKind::Error => {
                lines.push(Line::from(Span::styled(
                    format!("✗ {}", e.text),
                    Style::default()
                        .fg(Theme::ERROR)
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

/// Render just one assistant text block into display lines (for streaming).
pub fn render_assistant_lines(text: &str) -> Vec<Line<'static>> {
    text.lines()
        .map(|part| {
            Line::from(Span::styled(
                part.to_string(),
                Style::default().fg(Theme::FG),
            ))
        })
        .collect()
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

/// Convert a persisted chat message into a log entry for display.
pub fn message_to_log_entry(msg: &ChatMessage) -> Option<LogEntry> {
    match msg.role.as_str() {
        "user" => msg.content.as_ref().map(|c| LogEntry::user(c.clone())),
        "assistant" => {
            if let Some(content) = &msg.content {
                if !content.is_empty() {
                    return Some(LogEntry::assistant(content.clone()));
                }
            }
            if let Some(tool_calls) = &msg.tool_calls {
                let mut text = String::new();
                for tc in tool_calls {
                    let args_snip: String = tc.function.arguments.chars().take(60).collect();
                    text.push_str(&format!("⇢ {}({})\n", tc.function.name, args_snip));
                }
                if !text.is_empty() {
                    return Some(LogEntry::tool(text.trim_end().to_string()));
                }
            }
            None
        }
        "tool" => msg.content.as_ref().map(|c| {
            let preview: String = c.chars().take(200).collect();
            LogEntry::tool(format!("[tool result] {}", preview))
        }),
        "system" => msg.content.as_ref().map(|c| LogEntry::system(c.clone())),
        _ => None,
    }
}
