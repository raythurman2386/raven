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
/// fade after completion. `name` matches [`crate::agent::AgentEvent::ToolEnd`]
/// so parallel tools deactivate the correct row (not always `blocks.last()`).
#[derive(Clone)]
pub struct ToolBlock {
    text: String,
    /// Tool function name from the agent event (e.g. `read_file`).
    pub name: String,
    pub active: bool,
    pub end_tick: Option<u64>,
    /// Optional short result preview (from `ToolEnd`), shown dim under the call.
    pub preview: Option<String>,
}

impl ToolBlock {
    pub fn new(text: String) -> Self {
        Self {
            text,
            name: String::new(),
            active: false,
            end_tick: None,
            preview: None,
        }
    }

    /// Build an active tool row with a stable name for `ToolEnd` matching.
    pub fn start(name: impl Into<String>, text: String) -> Self {
        Self {
            text,
            name: name.into(),
            active: true,
            end_tick: None,
            preview: None,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    /// Attach a capped result preview after the tool finishes.
    pub fn set_preview(&mut self, preview: impl Into<String>) {
        let p: String = preview.into().chars().take(300).collect();
        if !p.trim().is_empty() {
            self.preview = Some(p);
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

    #[test]
    fn tool_block_start_sets_name_and_active() {
        let b = ToolBlock::start("read_file", "⇢ read_file(a)".into());
        assert!(b.active);
        assert_eq!(b.name, "read_file");
        assert!(b.preview.is_none());
    }

    #[test]
    fn tool_block_set_preview_caps_length() {
        let mut b = ToolBlock::start("run_shell", "⇢ run_shell".into());
        b.set_preview("x".repeat(500));
        assert_eq!(b.preview.as_ref().unwrap().chars().count(), 300);
    }
}
