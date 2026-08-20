//! Markdown rendering for assistant output.
//!
//! Walks a [`pulldown_cmark`] event stream and produces styled ratatui
//! [`Line`]s/`Span`s, so model responses render as readable markdown instead
//! of a wall of plain text. This is the "own the rendering" half of the
//! design: `pulldown-cmark` does the parsing (the genuinely hard, boring
//! part), and this module owns how the AST becomes terminal lines — the part
//! that is raven's aesthetic.
//!
//! The renderer is deliberately streaming-friendly: re-parsing a partial
//! document is cheap, and `pulldown-cmark` degrades unclosed tokens (e.g. a
//! half-typed `**bold`) to literal text, so a mid-stream re-render never
//! flashes garbage — the unclosed span just stays plain until it closes.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::Theme;

/// Inline style flags accumulated while walking a span's children.
#[derive(Clone, Copy, Default)]
struct InlineStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    code: bool,
}

impl InlineStyle {
    fn to_style(self, theme: Theme) -> Style {
        let mut s = Style::default().fg(theme.fg);
        if self.bold {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.code {
            s = s.fg(theme.tool);
        }
        s
    }
}

/// A renderer that accumulates styled lines from a markdown event stream.
struct Renderer {
    /// Completed display lines.
    lines: Vec<Line<'static>>,
    /// The line currently being built (spans accumulate here).
    cur: Vec<Span<'static>>,
    /// Active inline style (bold/italic/strike/code) for the current span.
    inline: InlineStyle,
    /// List nesting depth (for indentation + bullet markers).
    list_depth: usize,
    /// Ordered-list item counters, one per nesting level.
    list_counters: Vec<u64>,
    /// Whether we're inside a blockquote (prefix lines with a gutter).
    in_quote: bool,
    /// Destination URL of the link currently being rendered (if any).
    link_url: Option<String>,
    /// Language tag of the code block currently being rendered (if any).
    code_lang: Option<String>,
    /// The active color theme.
    theme: Theme,
}

impl Renderer {
    fn new(theme: Theme) -> Self {
        Self {
            lines: Vec::new(),
            cur: Vec::new(),
            inline: InlineStyle::default(),
            list_depth: 0,
            list_counters: Vec::new(),
            in_quote: false,
            link_url: None,
            code_lang: None,
            theme,
        }
    }

    /// Flush the current line into `lines` and start a fresh one. Emits
    /// nothing when the current line is empty, so a flush at the start or end
    /// of a document (or a block boundary with no pending text) doesn't
    /// produce a spurious blank line.
    fn flush(&mut self) {
        if !self.cur.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.cur)));
        }
    }

    /// Push a plain text span with the current inline style.
    fn text(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let style = self.inline.to_style(self.theme);
        self.cur.push(Span::styled(s.to_string(), style));
    }

    /// Push a styled span with an explicit style override (e.g. links).
    fn styled(&mut self, s: &str, style: Style) {
        if s.is_empty() {
            return;
        }
        self.cur.push(Span::styled(s.to_string(), style));
    }

    /// Prefix the current line with a blockquote gutter if inside a quote.
    fn quote_prefix(&mut self) {
        if self.in_quote {
            self.cur
                .push(Span::styled("│ ", Style::default().fg(self.theme.dim)));
        }
    }

    /// Render a fenced/indented code block (already collected as `code`).
    fn code_block(&mut self, code: &str, lang: Option<String>) {
        self.flush();
        let label = lang.unwrap_or_else(|| "code".to_string());
        self.lines.push(Line::from(Span::styled(
            format!("┌─ {label}"),
            Style::default().fg(self.theme.dim),
        )));
        for line in code.lines() {
            self.lines.push(Line::from(Span::styled(
                format!("│ {line}"),
                Style::default().fg(self.theme.tool),
            )));
        }
        self.lines.push(Line::from(Span::styled(
            "└─".to_string(),
            Style::default().fg(self.theme.dim),
        )));
        self.lines.push(Line::from(""));
    }

    /// Render a horizontal rule.
    fn rule(&mut self) {
        self.flush();
        self.lines.push(Line::from(Span::styled(
            "─".repeat(40),
            Style::default().fg(self.theme.dim),
        )));
        self.lines.push(Line::from(""));
    }
}

/// Render a markdown string into styled display lines.
///
/// This is the entry point used by both the full-block renderer and the
/// streaming tail renderer. It re-parses the whole (possibly partial) text
/// each call — cheap for typical assistant responses, and safe because
/// `pulldown-cmark` degrades unclosed tokens to literal text.
pub fn render_markdown(text: &str, theme: Theme) -> Vec<Line<'static>> {
    let mut r = Renderer::new(theme);
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(text, opts);
    for ev in parser {
        match ev {
            Event::Start(tag) => r.start(&tag),
            Event::End(tag) => r.end(&tag),
            Event::Text(t) => r.text(&t),
            Event::Code(t) => {
                let prev = r.inline;
                r.inline.code = true;
                r.text(&t);
                r.inline = prev;
            }
            Event::SoftBreak => {
                r.cur.push(Span::raw(" "));
            }
            Event::HardBreak => {
                r.flush();
            }
            Event::Rule => r.rule(),
            Event::Html(t) => {
                // Render raw HTML as dim text so it's visible but quiet.
                r.styled(&t, Style::default().fg(r.theme.dim));
            }
            _ => {}
        }
    }
    r.flush();
    r.lines
}

impl Renderer {
    fn start(&mut self, tag: &Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.quote_prefix();
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.quote_prefix();
                let marker = match level {
                    HeadingLevel::H1 => "█ ",
                    HeadingLevel::H2 => "▌ ",
                    _ => "· ",
                };
                self.cur
                    .push(Span::styled(marker, Style::default().fg(self.theme.accent)));
                self.inline.bold = true;
            }
            Tag::BlockQuote(_) => {
                self.in_quote = true;
            }
            Tag::CodeBlock(kind) => {
                // Capture the language tag (e.g. `rust`) so the block's top border
                // can label it. Code text accumulates into `cur`; rendered on End.
                self.code_lang = match kind {
                    CodeBlockKind::Fenced(info) if !info.trim().is_empty() => {
                        Some(info.trim().to_string())
                    }
                    _ => None,
                };
            }
            Tag::List(Some(_)) => {
                self.list_depth += 1;
                self.list_counters.push(1);
            }
            Tag::List(None) => {
                self.list_depth += 1;
                self.list_counters.push(0);
            }
            Tag::Item => {
                self.flush();
                self.quote_prefix();
                let indent = "  ".repeat(self.list_depth.saturating_sub(1));
                let marker = if let Some(counter) = self.list_counters.last_mut() {
                    if *counter > 0 {
                        let n = *counter;
                        *counter += 1;
                        format!("{n}. ")
                    } else {
                        "• ".to_string()
                    }
                } else {
                    "• ".to_string()
                };
                self.cur.push(Span::styled(
                    format!("{indent}{marker}"),
                    Style::default().fg(self.theme.accent),
                ));
            }
            Tag::Emphasis => self.inline.italic = true,
            Tag::Strong => self.inline.bold = true,
            Tag::Strikethrough => self.inline.strike = true,
            Tag::Link { dest_url, .. } => {
                // Render the link text, then append the URL dimmed.
                self.cur
                    .push(Span::styled("[", Style::default().fg(self.theme.accent)));
                self.inline.bold = false;
                self.link_url = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                // Images render as a dim `[img: url]` affordance.
                self.styled(
                    &format!("[img: {dest_url}]"),
                    Style::default().fg(self.theme.dim),
                );
            }
            Tag::Table(_) => {}
            Tag::TableHead => {}
            Tag::TableRow => {
                self.flush();
                self.quote_prefix();
            }
            Tag::TableCell if !self.cur.is_empty() => {
                // Separator between cells (except the first).
                self.cur
                    .push(Span::styled(" │ ", Style::default().fg(self.theme.dim)));
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: &TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush();
            }
            TagEnd::Heading(_) => {
                self.inline.bold = false;
                self.flush();
            }
            TagEnd::BlockQuote(_) => {
                self.in_quote = false;
                self.flush();
            }
            TagEnd::CodeBlock => {
                // The code text accumulated in `cur` as plain spans; pull it
                // out and render as a bordered block.
                let code: String = self.cur.iter().map(|s| s.content.as_ref()).collect();
                self.cur.clear();
                let lang = self.code_lang.take();
                self.code_block(&code, lang);
            }
            TagEnd::List(_) => {
                self.list_depth = self.list_depth.saturating_sub(1);
                self.list_counters.pop();
                self.flush();
            }
            TagEnd::Item => {
                self.flush();
            }
            TagEnd::Emphasis => self.inline.italic = false,
            TagEnd::Strong => self.inline.bold = false,
            TagEnd::Strikethrough => self.inline.strike = false,
            TagEnd::Link => {
                if let Some(url) = self.link_url.take() {
                    self.cur.push(Span::styled(
                        format!("]({url})"),
                        Style::default().fg(self.theme.dim),
                    ));
                }
            }
            TagEnd::TableHead => {}
            TagEnd::TableRow => {
                self.flush();
            }
            TagEnd::TableCell => {}
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    fn render(text: &str) -> Vec<Line<'static>> {
        render_markdown(text, Theme::RAVENWOOD)
    }

    #[test]
    fn renders_plain_text() {
        let lines = render("hello world");
        assert_eq!(plain(&lines), vec!["hello world"]);
    }

    #[test]
    fn renders_heading_bold() {
        let lines = render("# Title");
        let first = &lines[0];
        assert!(first.to_string().contains("Title"));
        assert!(first
            .spans
            .iter()
            .any(|s| s.style.add_modifier.contains(Modifier::BOLD)));
    }

    #[test]
    fn renders_bold_and_italic() {
        let lines = render("**bold** and *italic*");
        let line = &lines[0];
        let bold = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "bold")
            .expect("bold span");
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        let italic = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "italic")
            .expect("italic span");
        assert!(italic.style.add_modifier.contains(Modifier::ITALIC));
    }

    #[test]
    fn renders_inline_code() {
        let lines = render("run `cargo test` now");
        let line = &lines[0];
        let code = line
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "cargo test")
            .expect("code span");
        assert_eq!(code.style.fg, Some(Theme::RAVENWOOD.tool));
    }

    #[test]
    fn renders_code_block_bordered() {
        let lines = render("```rust\nfn main() {}\n```");
        let text = plain(&lines);
        assert!(text.iter().any(|l| l.contains("┌─ rust")));
        assert!(text.iter().any(|l| l.contains("fn main() {}")));
        assert!(text.iter().any(|l| l.contains("└─")));
    }

    #[test]
    fn renders_code_block_without_language_defaults_to_code() {
        let lines = render("```\nplain\n```");
        let text = plain(&lines);
        assert!(text.iter().any(|l| l.contains("┌─ code")));
    }

    #[test]
    fn renders_unordered_list() {
        let lines = render("- one\n- two");
        let text = plain(&lines);
        assert!(text.iter().any(|l| l.contains("• one")));
        assert!(text.iter().any(|l| l.contains("• two")));
    }

    #[test]
    fn renders_ordered_list() {
        let lines = render("1. first\n2. second");
        let text = plain(&lines);
        assert!(text.iter().any(|l| l.contains("1. first")));
        assert!(text.iter().any(|l| l.contains("2. second")));
    }

    #[test]
    fn renders_blockquote_gutter() {
        let lines = render("> quoted text");
        let text = plain(&lines);
        assert!(text
            .iter()
            .any(|l| l.contains("│") && l.contains("quoted text")));
    }

    #[test]
    fn renders_link_with_url() {
        let lines = render("[docs](https://example.com)");
        let text = plain(&lines);
        assert!(text
            .iter()
            .any(|l| l.contains("docs") && l.contains("https://example.com")));
    }

    #[test]
    fn renders_table() {
        let lines = render("| a | b |\n|---|---|\n| 1 | 2 |");
        let text = plain(&lines);
        assert!(text.iter().any(|l| l.contains("a") && l.contains("b")));
        assert!(text.iter().any(|l| l.contains("1") && l.contains("2")));
    }

    #[test]
    fn unclosed_emphasis_degrades_to_plain() {
        // A half-typed `**bold` (streaming) must not crash or emit garbage.
        let lines = render("this is **unclosed");
        let text = plain(&lines);
        assert!(text.iter().any(|l| l.contains("unclosed")));
    }

    #[test]
    fn empty_input_renders_empty() {
        let lines = render("");
        assert!(lines.is_empty() || lines.iter().all(|l| l.to_string().is_empty()));
    }
}
