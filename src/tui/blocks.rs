//! Block-based scrollback model.
//!
//! Each chat entry is a [`BlockKind`] variant holding its own text and any
//! per-block state (e.g. a tool call's glimmer phase). The scrollback is a
//! `Vec<BlockKind>`; rendering to display lines happens in `render.rs`
//! (`render_blocks`), and only the *visible window* is pre-wrapped each frame
//! (`prewrap_visible`) so scrolling through a long session is O(viewport)
//! rather than O(total history).

/// The concrete block variants.
#[derive(Clone)]
pub enum BlockKind {
    User(UserBlock),
    Assistant(AssistantBlock),
    Tool(ToolBlock),
    System(SystemBlock),
    Error(ErrorBlock),
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

    pub fn text(&self) -> &str {
        &self.text
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

    pub fn text(&self) -> &str {
        &self.text
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

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace the block's text in place (used to refresh header readouts
    /// such as the model name and context/compact figures on `/model`).
    pub fn set_text(&mut self, text: String) {
        self.text = text;
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

    pub fn text(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
