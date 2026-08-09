//! Formatting helpers and status-strip rendering.
//!
//! Pure helpers for token counts, colors, spinner/diamond frames, and the
//! agent state label. Kept separate from the event loop so they can be
//! unit-tested without a terminal.

use ratatui::style::Color;

use crate::plan::AgentState;

use super::Theme;

/// Format a token count as a compact human string (e.g. `12.4K`, `1.2M`).
pub fn fmt_tokens(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else if n < 10_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else if n < 1_000_000 {
        format!("{}K", n / 1_000)
    } else {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    }
}

/// Color for the context-usage readout based on how full the window is.
pub fn usage_color(pct: f64, theme: Theme) -> Color {
    if pct >= 85.0 {
        theme.error
    } else if pct >= 65.0 {
        theme.tool
    } else {
        theme.accent
    }
}

/// Braille spinner frames for the live-tool "glimmer" (Grok Build-style).
/// A slow frame divisor (~4 redraws per frame at 60ms = ~3.7 fps) keeps the
/// spinner visible but calm.
pub fn spinner_frame(tick: u64) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(tick / 4) as usize % FRAMES.len()]
}

/// Pulsing diamond for "waiting on you" cues (ask_user / plan approval), the
/// same visual language Grok Build uses. Brightness pulses on a ~1.3s cadence.
pub fn waiting_diamond(tick: u64) -> &'static str {
    const DIAMONDS: &[&str] = &["◆", "◇"];
    DIAMONDS[(tick / 8) as usize % DIAMONDS.len()]
}

/// The agent state label + color shown in the status strip.
pub fn state_label(state: &AgentState, status: &str, theme: Theme) -> (&'static str, Color) {
    match state {
        AgentState::Planning => ("planning", theme.plan),
        AgentState::AwaitingApproval => ("awaiting approval", theme.plan),
        AgentState::Executing => ("executing", theme.accent),
        _ if status.starts_with("tool:") => ("tool", theme.tool),
        _ if status.starts_with("thinking") => ("thinking", theme.dim),
        _ if status.starts_with("awaiting answer") => ("awaiting answer", theme.plan),
        _ => ("ready", theme.user),
    }
}
