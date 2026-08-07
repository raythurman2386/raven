//! ChatBlock-based scrollback model (Grok Build-style virtualization).
//!
//! Each chat entry is a self-rendering [`ChatBlock`] that knows its own height at
//! a given width (`desired_height`) and can render itself into a buffer area
//! (`render`). The scrollback is a `Vec<BlockKind>`; only the *visible* window
//! of blocks is rendered each frame, so scrolling through a long session is
//! O(viewport) rather than O(total history).
//!
//! This module defines the trait and the block variants. Rendering dispatch
//! lives in `render.rs`; the event-loop wiring lives in `mod.rs`.
//!
//! `dead_code` is allowed until Task 3 wires the block model into the event
//! loop; the model is defined and unit-tested here first.

#![allow(dead_code)]

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use super::Theme;

/// A self-rendering chat block. Height must be cheap (ideally O(1)) at a given
/// width so scroll-position math stays fast with thousands of blocks.
pub trait ChatBlock {
    /// Height in rows at `width` columns. Must be cheap.
    fn desired_height(&self, width: u16) -> u16;
    /// Render into the given area.
    fn render(&self, area: Rect, buf: &mut Buffer);
}

/// The concrete block variants, mirroring the old `LogKind`.
#[derive(Clone)]
pub enum BlockKind {
    User(UserBlock),
    Assistant(AssistantBlock),
    Tool(ToolBlock),
    System(SystemBlock),
    Error(ErrorBlock),
}

impl BlockKind {
    /// Wrap a plain text entry into the matching block variant.
    pub fn from_kind(kind: super::LogKind, text: String) -> Self {
        match kind {
            super::LogKind::User => BlockKind::User(UserBlock::new(text)),
            super::LogKind::Assistant => BlockKind::Assistant(AssistantBlock::new(text)),
            super::LogKind::Tool => BlockKind::Tool(ToolBlock::new(text)),
            super::LogKind::System => BlockKind::System(SystemBlock::new(text)),
            super::LogKind::Error => BlockKind::Error(ErrorBlock::new(text)),
        }
    }
}

impl ChatBlock for BlockKind {
    fn desired_height(&self, width: u16) -> u16 {
        match self {
            BlockKind::User(b) => b.desired_height(width),
            BlockKind::Assistant(b) => b.desired_height(width),
            BlockKind::Tool(b) => b.desired_height(width),
            BlockKind::System(b) => b.desired_height(width),
            BlockKind::Error(b) => b.desired_height(width),
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        match self {
            BlockKind::User(b) => b.render(area, buf),
            BlockKind::Assistant(b) => b.render(area, buf),
            BlockKind::Tool(b) => b.render(area, buf),
            BlockKind::System(b) => b.render(area, buf),
            BlockKind::Error(b) => b.render(area, buf),
        }
    }
}

/// A user prompt: bold green "You" tag + indented text.
#[derive(Clone)]
pub struct UserBlock {
    text: String,
}

impl UserBlock {
    pub fn new(text: String) -> Self {
        Self { text }
    }
}

impl ChatBlock for UserBlock {
    fn desired_height(&self, width: u16) -> u16 {
        // "You" tag + one row per text line + trailing blank row.
        let body = wrapped_rows(&self.text, width);
        (1 + body + 1) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut row = area.top();
        if row >= area.bottom() {
            return;
        }
        buf.set_string(
            area.left(),
            row,
            "You",
            Style::default()
                .fg(Theme::USER)
                .add_modifier(Modifier::BOLD),
        );
        row += 1;
        for part in self.text.lines() {
            if row >= area.bottom() {
                break;
            }
            buf.set_string(
                area.left(),
                row,
                format!("  {part}"),
                Style::default().fg(Theme::FG),
            );
            row += 1;
        }
    }
}

/// An assistant response: bold accent "Raven" tag + text. Supports streaming
/// via `push_chunk` (appends without re-rendering the whole block).
#[derive(Clone)]
pub struct AssistantBlock {
    text: String,
}

impl AssistantBlock {
    pub fn new(text: String) -> Self {
        Self { text }
    }

    /// Append a streaming chunk of text.
    pub fn push_chunk(&mut self, chunk: &str) {
        self.text.push_str(chunk);
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

impl ChatBlock for AssistantBlock {
    fn desired_height(&self, width: u16) -> u16 {
        // "Raven" tag + one row per text line.
        (1 + wrapped_rows(&self.text, width)) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut row = area.top();
        if row >= area.bottom() {
            return;
        }
        buf.set_string(
            area.left(),
            row,
            "Raven",
            Style::default()
                .fg(Theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        );
        row += 1;
        for part in self.text.lines() {
            if row >= area.bottom() {
                break;
            }
            buf.set_string(area.left(), row, part, Style::default().fg(Theme::FG));
            row += 1;
        }
    }
}

/// A tool call. `active` glimmers bright while running; `end_tick` drives the
/// fade after completion.
#[derive(Clone)]
pub struct ToolBlock {
    text: String,
    pub active: bool,
    pub end_tick: Option<u64>,
}

impl ToolBlock {
    pub fn new(text: String) -> Self {
        Self {
            text,
            active: false,
            end_tick: None,
        }
    }
}

impl ChatBlock for ToolBlock {
    fn desired_height(&self, width: u16) -> u16 {
        (1 + wrapped_rows(&self.text, width)) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let mut row = area.top();
        if row >= area.bottom() {
            return;
        }
        let style = if self.active {
            Style::default()
                .fg(Theme::TOOL)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Theme::TOOL)
        };
        buf.set_string(area.left(), row, &self.text, style);
        row += 1;
        for part in self.text.lines().skip(1) {
            if row >= area.bottom() {
                break;
            }
            buf.set_string(area.left(), row, part, style);
            row += 1;
        }
    }
}

/// A system message (dimmed).
#[derive(Clone)]
pub struct SystemBlock {
    text: String,
}

impl SystemBlock {
    pub fn new(text: String) -> Self {
        Self { text }
    }
}

impl ChatBlock for SystemBlock {
    fn desired_height(&self, width: u16) -> u16 {
        (1 + wrapped_rows(&self.text, width)) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let row = area.top();
        if row >= area.bottom() {
            return;
        }
        buf.set_string(
            area.left(),
            row,
            &self.text,
            Style::default().fg(Theme::SYSTEM),
        );
    }
}

/// An error message (bold red).
#[derive(Clone)]
pub struct ErrorBlock {
    text: String,
}

impl ErrorBlock {
    pub fn new(text: String) -> Self {
        Self { text }
    }
}

impl ChatBlock for ErrorBlock {
    fn desired_height(&self, width: u16) -> u16 {
        (1 + wrapped_rows(&self.text, width)) as u16
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        let row = area.top();
        if row >= area.bottom() {
            return;
        }
        buf.set_string(
            area.left(),
            row,
            format!("✗ {}", self.text),
            Style::default()
                .fg(Theme::ERROR)
                .add_modifier(Modifier::BOLD),
        );
    }
}

/// Number of wrapped rows `text` occupies at `width` columns (char-count based,
/// matching the existing `prewrap_lines` semantics). Empty text is 1 row.
fn wrapped_rows(text: &str, width: u16) -> usize {
    let w = width.max(1) as usize;
    if text.is_empty() {
        return 1;
    }
    let mut rows = 0usize;
    for seg in text.split('\n') {
        if seg.is_empty() {
            rows += 1;
        } else {
            rows += seg.chars().count().div_ceil(w);
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_block_height_counts_tag_and_lines() {
        let b = UserBlock::new("line one\nline two".to_string());
        // "You" + 2 body lines + trailing blank = 4
        assert_eq!(b.desired_height(80), 4);
    }

    #[test]
    fn assistant_block_height_wraps_long_lines() {
        let b = AssistantBlock::new("a".repeat(30));
        // "Raven" + 30 chars at width 10 = 3 wrapped rows = 4
        assert_eq!(b.desired_height(10), 4);
    }

    #[test]
    fn assistant_block_push_chunk_appends() {
        let mut b = AssistantBlock::new("hello".to_string());
        b.push_chunk(" world");
        assert_eq!(b.text(), "hello world");
    }

    #[test]
    fn tool_block_active_flag() {
        let mut b = ToolBlock::new("⇢ read_file(x)".to_string());
        assert!(!b.active);
        b.active = true;
        assert!(b.active);
    }

    #[test]
    fn block_kind_from_kind_maps_variants() {
        let u = BlockKind::from_kind(super::super::LogKind::User, "hi".to_string());
        assert!(matches!(u, BlockKind::User(_)));
        let a = BlockKind::from_kind(super::super::LogKind::Assistant, "hi".to_string());
        assert!(matches!(a, BlockKind::Assistant(_)));
    }
}
