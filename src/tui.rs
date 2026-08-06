//! Interactive TUI — Grok Build–inspired layout for Raven.
//!
//! ┌─ raven ─ qwen2.5-coder:14b ──────────── 12.4K/128K 10% ─ plan:on ─┐
//! │ You                                                                  │
//! │   add auth middleware                                                │
//! │                                                                      │
//! │ → read_file(src/main.rs)                                             │
//! │   [read_file] --- src/main.rs (lines 1-40 of 120) ---                │
//! │                                                                      │
//! │ Here's a plan:                                                       │
//! │ 1. Add middleware module                                             │
//! │ 2. Wire into router                                                  │
//! │                                                                      │
//! ├─ PLAN ───────────────────────────────────────────────────────────────┤
//! │ 1. Add middleware module                                             │
//! │ 2. Wire into router                                                  │
//! │ Type yes to execute · or describe changes                            │
//! ├─ ready ──────────────────────────────────────────────────────────────┤
//! │ ❯ _                                                                  │
//! └──────────────────────────────────────────────────────────────────────┘

use anyhow::Result;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseButton, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, ChatMessage};
use crate::commands;
use crate::config::Settings;
use crate::context::history_tokens;
use crate::plan::{self, AgentState};
use crate::session::{Session, SessionStore};

// ── Theme (Ravenwood emerald-forest) ─────────────────────────────────────
//
// Dark-medium Ravenwood palette (see the ravenwood-theme skill). Warm beige
// foreground, olive-tinged backgrounds, green hero accent, pastel brights.

struct Theme;
impl Theme {
    const FG: Color = Color::Rgb(0xE8, 0xD5, 0xB7); // fg — warm beige
    const DIM: Color = Color::Rgb(0x85, 0x92, 0x89); // grey1
    const ACCENT: Color = Color::Rgb(0x22, 0xD3, 0xEE); // blue
    const USER: Color = Color::Rgb(0x4A, 0xDE, 0x80); // green — hero
    const TOOL: Color = Color::Rgb(0xE6, 0x98, 0x75); // orange
    const SYSTEM: Color = Color::Rgb(0x7F, 0x89, 0x7D); // grey0
    const ERROR: Color = Color::Rgb(0xE6, 0x7E, 0x80); // red
    const PLAN: Color = Color::Rgb(0xF4, 0x72, 0xB6); // purple
    const BORDER: Color = Color::Rgb(0x4A, 0x5A, 0x4D); // bg4
    const STATUS_BG: Color = Color::Rgb(0x1F, 0x24, 0x1F); // bg1
}

// ── Log model ────────────────────────────────────────────────────────────

#[derive(Clone)]
enum LogKind {
    User,
    Assistant,
    Tool,
    System,
    Error,
}

#[derive(Clone)]
struct LogEntry {
    kind: LogKind,
    text: String,
}

impl LogEntry {
    fn user(s: impl Into<String>) -> Self {
        Self {
            kind: LogKind::User,
            text: s.into(),
        }
    }
    fn assistant(s: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Assistant,
            text: s.into(),
        }
    }
    fn tool(s: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Tool,
            text: s.into(),
        }
    }
    fn system(s: impl Into<String>) -> Self {
        Self {
            kind: LogKind::System,
            text: s.into(),
        }
    }
    fn error(s: impl Into<String>) -> Self {
        Self {
            kind: LogKind::Error,
            text: s.into(),
        }
    }
}

// ── TUI state ────────────────────────────────────────────────────────────

struct TuiState {
    log: Vec<LogEntry>,
    log_dirty: bool,
    cached_log_lines: Vec<Line<'static>>,
    last_assistant_lines: usize,
    stream_patch: bool,
    cached_est_tokens: usize,
    messages_dirty: bool,
    input: String,
    status: String,
    plan_pending: bool,
    plan_preview: Vec<String>,
    active_plan: Option<crate::plan::Plan>,
    running: bool,
    plan_first: bool,
    assistant_text: String,
    agent_state: AgentState,
    scroll: u16,
    auto_scroll: bool,
    quit: bool,
    tick: u64,
    live_tool: Option<String>,
    turn_tool_count: usize,
    pending_question: Option<tokio::sync::oneshot::Sender<String>>,
    pending_question_text: Option<String>,
    session_messages: Vec<ChatMessage>,
    task_handle: Option<tokio::task::JoinHandle<anyhow::Result<Vec<ChatMessage>>>>,
}

impl TuiState {
    fn new(settings: &Settings, app_name: &str, compact_at: usize) -> Self {
        Self {
            log: vec![
                LogEntry::system(format!(
                    "{app_name} · {} · {}",
                    settings.model, settings.base_url
                )),
                LogEntry::system(format!("workspace {}", settings.workspace.display())),
                LogEntry::system(format!(
                    "context {} · compact ~{}",
                    fmt_tokens(settings.context_window as u64),
                    fmt_tokens(compact_at as u64),
                )),
                LogEntry::system(String::new()),
                LogEntry::system(
                    "enter submit · /help · /plan · /model · /new · ctrl+c quit · wheel/pgup scroll",
                ),
            ],
            log_dirty: true,
            cached_log_lines: Vec::new(),
            last_assistant_lines: 0,
            stream_patch: false,
            cached_est_tokens: 0,
            messages_dirty: false,
            input: String::new(),
            status: "ready".to_string(),
            plan_pending: false,
            plan_preview: Vec::new(),
            active_plan: None,
            running: false,
            plan_first: settings.plan_first,
            assistant_text: String::new(),
            agent_state: AgentState::Idle,
            scroll: 0,
            auto_scroll: true,
            quit: false,
            tick: 0,
            live_tool: None,
            turn_tool_count: 0,
            pending_question: None,
            pending_question_text: None,
            session_messages: Vec::new(),
            task_handle: None,
        }
    }

    fn toggle_plan_mode(&mut self) -> bool {
        self.plan_first = !self.plan_first;
        if !self.plan_first {
            self.plan_pending = false;
            self.plan_preview.clear();
            if matches!(
                self.agent_state,
                AgentState::AwaitingApproval | AgentState::Planning
            ) {
                self.agent_state = AgentState::Idle;
                self.status = "ready".into();
            }
        }
        self.plan_first
    }
}

// ── Formatting helpers ───────────────────────────────────────────────────

fn fmt_tokens(n: u64) -> String {
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

fn usage_color(pct: f64) -> Color {
    if pct >= 85.0 {
        Theme::ERROR
    } else if pct >= 65.0 {
        Theme::TOOL
    } else {
        Theme::ACCENT
    }
}

/// Braille spinner frames for the live-tool "glimmer" (Grok Build-style).
/// A slow frame divisor (~4 redraws per frame at 60ms = ~3.7 fps) keeps the
/// spinner visible but calm.
fn spinner_frame(tick: u64) -> &'static str {
    const FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    FRAMES[(tick / 4) as usize % FRAMES.len()]
}

/// Pulsing diamond for "waiting on you" cues (ask_user / plan approval), the
/// same visual language Grok Build uses. Brightness pulses on a ~1.3s cadence.
fn waiting_diamond(tick: u64) -> &'static str {
    const DIAMONDS: &[&str] = &["◆", "◇"];
    DIAMONDS[(tick / 8) as usize % DIAMONDS.len()]
}

fn state_label(state: &AgentState, status: &str) -> (&'static str, Color) {
    match state {
        AgentState::Planning => ("planning", Theme::PLAN),
        AgentState::AwaitingApproval => ("awaiting approval", Theme::PLAN),
        AgentState::Executing => ("executing", Theme::ACCENT),
        _ if status.starts_with("tool:") => ("tool", Theme::TOOL),
        _ if status.starts_with("thinking") => ("thinking", Theme::DIM),
        _ if status.starts_with("awaiting answer") => ("awaiting answer", Theme::PLAN),
        _ => ("ready", Theme::USER),
    }
}

// ── Render log lines (only when dirty) ───────────────────────────────────

/// Render every log entry into display lines, returning the count of trailing
/// lines owned by the *last* assistant entry (0 if the log ends on any other
/// kind). That tail count lets the streaming path patch just the active
/// assistant block instead of re-rendering the whole log per token.
fn render_log_lines(log: &[LogEntry]) -> (Vec<Line<'static>>, usize) {
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
fn render_assistant_lines(text: &str) -> Vec<Line<'static>> {
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
fn prewrap_lines(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
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

fn message_to_log_entry(msg: &ChatMessage) -> Option<LogEntry> {
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

// ── Main TUI ─────────────────────────────────────────────────────────────

pub async fn run_tui(mut settings: Settings, resume_session: Option<Session>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let compact_at = ((settings.context_window - settings.context_window / 8) as f32
        * settings.compact_threshold) as usize;

    let app_name = "raven";

    let mut state = TuiState::new(&settings, app_name, compact_at);

    // Hard cap on rendered log entries so per-frame cost stays bounded for
    // long sessions. Old entries are dropped from the on-screen log only; the
    // session file keeps the full history.
    const MAX_LOG_ENTRIES: usize = 2000;

    // Throttle full redraws during steady streaming to keep the CPU/terminal
    // cost flat while the model is mid-response.
    const DRAW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(60);
    let mut last_draw = std::time::Instant::now();

    let store = SessionStore::for_workspace(&settings.workspace)?;
    let mut session = if let Some(s) = resume_session {
        state.log.push(LogEntry::system(format!(
            "resumed session {} ({} messages)",
            s.summary.id,
            s.messages.len()
        )));
        state.log_dirty = true;
        state.session_messages = s.messages.clone();
        state.messages_dirty = true;
        for msg in &s.messages {
            if let Some(entry) = message_to_log_entry(msg) {
                state.log.push(entry);
            }
        }
        state.log.push(LogEntry::system(String::new()));
        state.log.push(LogEntry::system(
            "resumed · enter submit · /help · /plan · /model · /new · ctrl+c quit · wheel/pgup scroll",
        ));
        state.log_dirty = true;
        s
    } else {
        store.create(&settings.model)?
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);

    loop {
        if state.log.len() > MAX_LOG_ENTRIES {
            let drop = state.log.len() - MAX_LOG_ENTRIES;
            state.log.drain(..drop);
            state.log_dirty = true;
        }

        if state.log_dirty {
            let (rendered, tail) = render_log_lines(&state.log);
            state.cached_log_lines = rendered;
            state.last_assistant_lines = tail;
            state.log_dirty = false;
            state.stream_patch = false;
        } else if state.stream_patch {
            let tail_text = state
                .log
                .iter()
                .rev()
                .find(|e| matches!(e.kind, LogKind::Assistant))
                .map(|e| e.text.as_str())
                .unwrap_or("");
            let new_tail = render_assistant_lines(tail_text);
            let new_tail_len = new_tail.len();
            state.cached_log_lines.truncate(
                state
                    .cached_log_lines
                    .len()
                    .saturating_sub(state.last_assistant_lines),
            );
            state.cached_log_lines.extend(new_tail);
            state.last_assistant_lines = new_tail_len;
            state.stream_patch = false;
        }

        if state.messages_dirty {
            state.cached_est_tokens = history_tokens(&state.session_messages);
            state.messages_dirty = false;
        }

        let force_draw = state.log_dirty
            || state.messages_dirty
            || state.stream_patch
            || !state.running
            || state.live_tool.is_some();
        if force_draw || last_draw.elapsed() >= DRAW_INTERVAL {
            if state.running || state.live_tool.is_some() {
                state.tick = state.tick.wrapping_add(1);
            }
            terminal.draw(|f| {
                draw_ui(f, app_name, &settings, &state);
            })?;
            last_draw = std::time::Instant::now();
        }

        // Input + mouse
        if event::poll(std::time::Duration::from_millis(40))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
                {
                    match key.code {
                        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            break
                        }
                        KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let on = state.toggle_plan_mode();
                            state.log.push(LogEntry::system(format!(
                                "plan mode {}",
                                if on { "on" } else { "off" }
                            )));
                            state.log_dirty = true;
                        }
                        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let _ = store.save_all_messages(&session, &state.session_messages);
                            let _ = store.update_summary(&mut session, None);
                            session = store.create(&settings.model)?;
                            state.session_messages.clear();
                            state.log.clear();
                            state.log.push(LogEntry::system(format!(
                                "{app_name} · {} · {}",
                                settings.model, settings.base_url
                            )));
                            state.log.push(LogEntry::system(format!(
                                "workspace {}",
                                settings.workspace.display()
                            )));
                            state.log.push(LogEntry::system(String::new()));
                            state.log.push(LogEntry::system(
                                "new session · enter submit · ctrl+n new · ctrl+p plan · ctrl+c quit",
                            ));
                            state.log_dirty = true;
                            state.plan_preview.clear();
                            state.plan_pending = false;
                            state.running = false;
                            state.agent_state = AgentState::Idle;
                            state.status = "ready".into();
                            state.assistant_text.clear();
                            state.input.clear();
                            state.scroll = 0;
                            state.auto_scroll = true;
                        }
                        KeyCode::Up => {
                            state.scroll = state.scroll.saturating_add(1);
                            state.auto_scroll = false;
                        }
                        KeyCode::Down => {
                            state.scroll = state.scroll.saturating_sub(1);
                            if state.scroll == 0 {
                                state.auto_scroll = true;
                            }
                        }
                        KeyCode::PageUp => {
                            state.scroll = state.scroll.saturating_add(10);
                            state.auto_scroll = false;
                        }
                        KeyCode::PageDown => {
                            state.scroll = state.scroll.saturating_sub(10);
                            if state.scroll == 0 {
                                state.auto_scroll = true;
                            }
                        }
                        KeyCode::Char(c) => {
                            if !state.running || state.pending_question.is_some() {
                                state.input.push(c);
                            }
                        }
                        KeyCode::Backspace => {
                            if !state.running || state.pending_question.is_some() {
                                state.input.pop();
                            }
                        }
                        KeyCode::Enter => {
                            if state.input.trim().is_empty() {
                                continue;
                            }
                            let text = state.input.trim().to_string();
                            state.input.clear();
                            state.scroll = 0;
                            state.auto_scroll = true;
                            state.turn_tool_count = 0;
                            state.live_tool = None;

                            if let Some(pc) = commands::parse(&text) {
                                dispatch_slash_command(
                                    &mut state,
                                    &pc,
                                    &mut settings,
                                    &store,
                                    &mut session,
                                )?;
                                continue;
                            }

                            if let Some(reply) = state.pending_question.take() {
                                let _ = reply.send(text.clone());
                                state.pending_question_text = None;
                                state.status = "running".into();
                                continue;
                            }

                            if state.running {
                                if let Some(handle) = state.task_handle.take() {
                                    handle.abort();
                                }
                                state
                                    .log
                                    .push(LogEntry::system("⏸ interrupted — redirecting…"));
                                state.log_dirty = true;
                                start_task(
                                    &mut state,
                                    &text,
                                    &settings,
                                    &store,
                                    &session,
                                    tx.clone(),
                                )?;
                                continue;
                            }

                            if state.plan_pending {
                                handle_plan_response(
                                    &mut state,
                                    &text,
                                    &settings,
                                    &store,
                                    &session,
                                    tx.clone(),
                                )?;
                            } else {
                                start_task(
                                    &mut state,
                                    &text,
                                    &settings,
                                    &store,
                                    &session,
                                    tx.clone(),
                                )?;
                            }
                        }
                        KeyCode::Esc => break,
                        _ => {}
                    }
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollUp => {
                        state.scroll = state.scroll.saturating_add(3);
                        state.auto_scroll = false;
                    }
                    MouseEventKind::ScrollDown => {
                        state.scroll = state.scroll.saturating_sub(3);
                        if state.scroll == 0 {
                            state.auto_scroll = true;
                        }
                    }
                    MouseEventKind::Down(MouseButton::Left) if state.running => {
                        let size = terminal.size().unwrap_or_default();
                        let input_h = input_box_height(&state.input, size.width);
                        let status_y = size.height.saturating_sub(input_h).saturating_sub(1);
                        if m.row == status_y
                            && m.column >= size.width.saturating_sub(STOP_BTN.len() as u16)
                        {
                            if let Some(handle) = state.task_handle.take() {
                                handle.abort();
                                let _ = store.save_all_messages(&session, &state.session_messages);
                                let _ = store.update_summary(&mut session, None);
                                state.log.push(LogEntry::system("⏹ stopped (click)"));
                                state.log_dirty = true;
                            }
                            state.running = false;
                            state.agent_state = AgentState::Idle;
                            state.status = "ready".into();
                            state.assistant_text.clear();
                            state.live_tool = None;
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
        }

        // Agent events
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::TextDelta(t) => {
                    state.assistant_text.push_str(&t);
                    if let Some(last) = state.log.last_mut() {
                        if matches!(last.kind, LogKind::Assistant) {
                            last.text.push_str(&t);
                            state.stream_patch = true;
                        } else {
                            state.log.push(LogEntry::assistant(t));
                            state.log_dirty = true;
                        }
                    } else {
                        state.log.push(LogEntry::assistant(t));
                        state.log_dirty = true;
                    }
                    if state.auto_scroll {
                        state.scroll = 0;
                    }
                }
                AgentEvent::ToolStart { name, args } => {
                    let args_str = if args.is_null() {
                        String::new()
                    } else {
                        args.to_string()
                    };
                    let snip: String = args_str.chars().take(60).collect();
                    state.live_tool = Some(format!("⇢ {name}({snip})"));
                    state.turn_tool_count += 1;
                    state.status = "running".into();
                }
                AgentEvent::ToolEnd { name, preview } => {
                    let _ = name;
                    let _ = preview;
                    state.live_tool = None;
                    state.status = "running".into();
                }
                AgentEvent::Iteration(n) => {
                    state.status = format!("thinking… (iter {n})");
                }
                AgentEvent::Compacted {
                    before_tokens,
                    after_tokens,
                } => {
                    state.log.push(LogEntry::system(format!(
                        "⟳ compacted ~{before_tokens} → ~{after_tokens} tokens"
                    )));
                    state.log_dirty = true;
                }
                AgentEvent::Retry { attempt, delay_ms } => {
                    state.log.push(LogEntry::system(format!(
                        "⟳ retry {attempt}/3 in {delay_ms}ms"
                    )));
                    state.log_dirty = true;
                }
                AgentEvent::VerifyRequired => {
                    state.log.push(LogEntry::system(
                        "⟳ verify required — re-running to enforce run_tests".to_string(),
                    ));
                    state.log_dirty = true;
                }
                AgentEvent::PlanReady => {
                    if let Some(handle) = state.task_handle.take() {
                        if let Ok(Ok(msgs)) = handle.await {
                            state.session_messages = msgs;
                            state.messages_dirty = true;
                            let _ = store.save_all_messages(&session, &state.session_messages);
                            let _ = store.update_summary(&mut session, None);
                        }
                    }

                    if state.plan_first && state.agent_state == AgentState::Planning {
                        let plan = plan::parse_plan(&state.assistant_text);
                        state.active_plan = Some(plan.clone());
                        state.plan_preview = plan::format_plan(&plan)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        state.log.push(LogEntry::system(String::new()));
                        state
                            .log
                            .push(LogEntry::system("plan ready — auto-executing"));
                        state.log_dirty = true;

                        state.running = true;
                        state.agent_state = AgentState::Executing;
                        state.status = "executing plan…".into();
                        let exec_prompt = plan::EXECUTE_PROMPT.to_string();
                        let _ = store.append_message(
                            &session,
                            &ChatMessage {
                                role: "user".into(),
                                content: Some(exec_prompt.clone()),
                                tool_calls: None,
                                tool_call_id: None,
                            },
                        );
                        state.assistant_text.clear();
                        let mut agent =
                            Agent::with_messages(settings.clone(), state.session_messages.clone())?
                                .with_plan(plan);
                        let tx_exec = tx.clone();
                        state.task_handle = Some(tokio::spawn(async move {
                            agent.run(&exec_prompt, tx_exec).await?;
                            Ok(agent.messages)
                        }));
                        state.plan_preview.clear();
                    } else {
                        state.plan_preview.clear();
                        state.status = "ready".into();
                        state.agent_state = AgentState::Idle;
                        state.running = false;
                        state.assistant_text.clear();
                    }
                }
                AgentEvent::PlanProgress(plan) => {
                    state.plan_preview = plan::format_plan(&plan)
                        .lines()
                        .map(|s| s.to_string())
                        .collect();
                }
                AgentEvent::AskUser { question, reply } => {
                    state.log.push(LogEntry::system(format!("❓ {question}")));
                    state.log_dirty = true;
                    state.pending_question = Some(reply);
                    state.pending_question_text = Some(question);
                    state.status = "awaiting answer".into();
                    state.input.clear();
                    state.scroll = 0;
                }
                AgentEvent::Done => {
                    if let Some(handle) = state.task_handle.take() {
                        if let Ok(Ok(msgs)) = handle.await {
                            state.session_messages = msgs;
                            state.messages_dirty = true;
                            let _ = store.save_all_messages(&session, &state.session_messages);
                            let _ = store.update_summary(&mut session, None);
                        }
                    }

                    if state.plan_first && state.agent_state == AgentState::Planning {
                        let plan = plan::parse_plan(&state.assistant_text);
                        state.active_plan = Some(plan.clone());
                        state.plan_preview = plan::format_plan(&plan)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        state.log.push(LogEntry::system(String::new()));
                        state
                            .log
                            .push(LogEntry::system("plan ready — approve or revise below"));
                        state.plan_pending = true;
                        state.agent_state = AgentState::AwaitingApproval;
                        state.status = "awaiting plan approval".into();
                    } else {
                        state.plan_preview.clear();
                        state.status = "ready".into();
                        state.agent_state = AgentState::Idle;
                    }
                    state.running = false;
                    state.assistant_text.clear();
                    if state.turn_tool_count > 0 {
                        state.log.push(LogEntry::tool(format!(
                            "⇢ {} tool call{} this turn",
                            state.turn_tool_count,
                            if state.turn_tool_count == 1 { "" } else { "s" }
                        )));
                    }
                    state.turn_tool_count = 0;
                    state.live_tool = None;
                    state.log_dirty = true;
                }
                AgentEvent::Error(e) => {
                    state.log.push(LogEntry::error(e));
                    state.plan_preview.clear();
                    state.status = "ready".into();
                    state.agent_state = AgentState::Idle;
                    state.running = false;
                    state.assistant_text.clear();
                    if state.turn_tool_count > 0 {
                        state.log.push(LogEntry::tool(format!(
                            "⇢ {} tool call{} this turn",
                            state.turn_tool_count,
                            if state.turn_tool_count == 1 { "" } else { "s" }
                        )));
                    }
                    state.turn_tool_count = 0;
                    state.live_tool = None;
                    state.log_dirty = true;
                }
            }
        }

        if state.quit {
            break;
        }
    }

    let _ = store.save_all_messages(&session, &state.session_messages);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    Ok(())
}

// ── Drawing ──────────────────────────────────────────────────────────────

fn draw_ui(f: &mut Frame, app_name: &str, settings: &Settings, state: &TuiState) {
    let pct = if settings.context_window > 0 {
        (state.cached_est_tokens as f64 / settings.context_window as f64) * 100.0
    } else {
        0.0
    };
    let (state_txt, state_color) = state_label(&state.agent_state, &state.status);

    let show_plan = state.plan_pending && !state.plan_preview.is_empty();
    let plan_h = if show_plan {
        (state.plan_preview.len().saturating_add(2) as u16).clamp(3, 10)
    } else {
        0
    };

    let input_h = input_box_height(&state.input, f.size().width);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top chrome
            Constraint::Min(5),    // log
            Constraint::Length(plan_h),
            Constraint::Length(1), // status
            Constraint::Length(input_h),
        ])
        .split(f.size());

    // Top bar — product · model · context
    let top = Line::from(vec![
        Span::styled(
            format!(" {app_name} "),
            Style::default()
                .fg(Color::Black)
                .bg(Theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            settings.model.clone(),
            Style::default().fg(Theme::FG).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(Theme::DIM)),
        Span::styled(
            format!(
                "{}/{} ({:.0}%)",
                fmt_tokens(state.cached_est_tokens as u64),
                fmt_tokens(settings.context_window as u64),
                pct
            ),
            Style::default().fg(usage_color(pct)),
        ),
        Span::styled("  ·  ", Style::default().fg(Theme::DIM)),
        Span::styled(
            if state.plan_first {
                "plan-first:on"
            } else {
                "plan-first:off"
            },
            Style::default().fg(if state.plan_first {
                Theme::PLAN
            } else {
                Theme::DIM
            }),
        ),
    ]);
    f.render_widget(Paragraph::new(top), chunks[0]);

    // Log
    let content_width = (chunks[1].width.saturating_sub(4)) as usize;
    let display_lines = prewrap_lines(&state.cached_log_lines, content_width.max(1));

    let log_h = chunks[1].height.saturating_sub(2) as usize;
    let max_scroll = display_lines.len().saturating_sub(log_h);
    let scroll_eff = (state.scroll as usize).min(max_scroll);
    let offset = max_scroll.saturating_sub(scroll_eff) as u16;

    let log_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(Style::default().fg(Theme::BORDER))
        .padding(Padding::horizontal(1));
    let log_widget = Paragraph::new(display_lines)
        .block(log_block)
        .scroll((offset, 0));
    f.render_widget(log_widget, chunks[1]);

    // Plan panel
    if show_plan && plan_h > 0 {
        let mut lines: Vec<Line> = state
            .plan_preview
            .iter()
            .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(Theme::PLAN))))
            .collect();
        lines.push(Line::from(Span::styled(
            "yes to execute · or type revisions",
            Style::default().fg(Theme::DIM),
        )));
        let plan_widget = Paragraph::new(lines).block(
            Block::default()
                .title(Span::styled(" plan ", Style::default().fg(Theme::PLAN)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Theme::PLAN)),
        );
        f.render_widget(plan_widget, chunks[2]);
    }

    // Status strip
    let mut status_line = vec![
        Span::styled(
            format!(" {state_txt} "),
            Style::default()
                .fg(state_color)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", settings.workspace.display()),
            Style::default().fg(Theme::DIM),
        ),
    ];
    if let Some(tool) = &state.live_tool {
        status_line.push(Span::styled(
            format!(" {} {}", spinner_frame(state.tick), tool),
            Style::default().fg(Theme::TOOL),
        ));
    }
    if state.pending_question_text.is_some() {
        status_line.push(Span::styled(
            format!(" {}", waiting_diamond(state.tick)),
            Style::default().fg(Theme::PLAN),
        ));
    }

    if state.running {
        let status_row_w = chunks[3].width;
        let line_w: usize = status_line.iter().map(|s| s.content.chars().count()).sum();
        let btn_w = STOP_BTN.chars().count();
        let pad = status_row_w.saturating_sub(line_w as u16 + btn_w as u16 + 1);
        if pad > 0 {
            status_line.push(Span::raw(" ".repeat(pad as usize)));
        }
        status_line.push(Span::raw(" "));
        status_line.push(stop_span());
    }

    f.render_widget(
        Paragraph::new(Line::from(status_line)).style(Style::default().bg(Theme::STATUS_BG)),
        chunks[3],
    );

    // Input
    let title = if let Some(q) = &state.pending_question_text {
        format!(" answer: {q} ")
    } else if state.plan_pending {
        " approve / revise ".into()
    } else {
        " task ".into()
    };
    let prompt = if state.pending_question_text.is_some() {
        "▸ "
    } else if state.plan_pending {
        "? "
    } else {
        "❯ "
    };
    let input_line = Line::from(vec![
        Span::styled(
            prompt,
            Style::default()
                .fg(
                    if state.pending_question_text.is_some() || state.plan_pending {
                        Theme::PLAN
                    } else {
                        Theme::ACCENT
                    },
                )
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(state.input.to_string(), Style::default().fg(Theme::FG)),
    ]);
    let input_w = Paragraph::new(input_line).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if state.plan_pending {
                Theme::PLAN
            } else {
                Theme::BORDER
            }))
            .title(Span::styled(title, Style::default().fg(Theme::DIM))),
    );
    f.render_widget(input_w, chunks[4]);
}

/// Number of wrapped lines a string occupies at the given width (char count).
///
/// A single `\n` forces a new line; a run of chars longer than `width` wraps
/// to the next line. Used to size the input box so long tasks are visible
/// instead of clipping.
fn wrapped_line_count(s: &str, width: usize) -> usize {
    if s.is_empty() {
        return 1;
    }
    let mut lines = 0usize;
    for seg in s.split('\n') {
        let w = width.max(1);
        if seg.is_empty() {
            lines += 1;
        } else {
            lines += seg.chars().count().div_ceil(w);
        }
    }
    lines
}

/// Height of the input box (in rows, including borders) for a given input and
/// terminal width. Shared by the draw path and click-hit-testing so both agree
/// on where the status strip (the row just above the input) sits.
fn input_box_height(input: &str, term_width: u16) -> u16 {
    let avail = term_width.saturating_sub(4).max(1) as usize; // minus 2 border cols
    let avail = avail.saturating_sub(2).max(1); // minus prompt glyph "❯ "
    let lines = wrapped_line_count(input, avail).clamp(1, 6) as u16;
    lines.saturating_add(2) // + top/bottom border rows
}

/// The `[stop]` button rendered at the right edge of the status strip.
const STOP_BTN: &str = "[stop]";

/// Build a `Span` for the `[stop]` button in the status strip (right-aligned).
/// The caller right-aligns it against the status row's width.
fn stop_span() -> Span<'static> {
    Span::styled(
        STOP_BTN.to_string(),
        Style::default()
            .fg(Theme::ERROR)
            .add_modifier(Modifier::BOLD),
    )
}

// ── Task / plan helpers (keeps the event loop readable) ──────────────────

fn start_task(
    state: &mut TuiState,
    text: &str,
    settings: &Settings,
    store: &SessionStore,
    session: &crate::session::Session,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<()> {
    state.running = true;
    state.status = "running…".into();
    state.log.push(LogEntry::user(text.to_string()));
    state.log_dirty = true;

    let mut prompt = text.to_string();
    if state.plan_first {
        prompt.push_str(
            "\n\nFirst propose a concise step-by-step plan. You may use read-only tools (list_dir, read_file, grep, search_code, git_status, git_diff, git_log) to inspect the workspace, but you CANNOT edit files or run shell until the plan is approved. Just list the numbered steps.",
        );
        state.agent_state = AgentState::Planning;
    }
    state.assistant_text.clear();

    let user_msg = ChatMessage {
        role: "user".into(),
        content: Some(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
    };
    let _ = store.append_message(session, &user_msg);

    let mut agent = Agent::with_messages(settings.clone(), state.session_messages.clone())?;
    if state.plan_first {
        agent = agent.plan_only();
    }
    state.task_handle = Some(tokio::spawn(async move {
        agent.run(&prompt, tx).await?;
        Ok(agent.messages)
    }));
    Ok(())
}

fn handle_plan_response(
    state: &mut TuiState,
    text: &str,
    settings: &Settings,
    store: &SessionStore,
    session: &crate::session::Session,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<()> {
    let low = text.to_lowercase();
    let approve = matches!(
        low.as_str(),
        "yes" | "y" | "approve" | "go" | "execute" | "ok"
    );

    state.plan_pending = false;
    state.plan_preview.clear();
    state.running = true;
    state.log.push(LogEntry::user(text.to_string()));
    state.log_dirty = true;

    let prompt = if approve {
        state.agent_state = AgentState::Executing;
        state.status = "executing plan…".into();
        plan::EXECUTE_PROMPT.to_string()
    } else {
        state.agent_state = AgentState::Planning;
        state.status = "revising plan…".into();
        format!("Revise the plan based on this feedback:\n{text}")
    };

    let _ = store.append_message(
        session,
        &ChatMessage {
            role: "user".into(),
            content: Some(prompt.clone()),
            tool_calls: None,
            tool_call_id: None,
        },
    );

    state.assistant_text.clear();
    let mut agent = Agent::with_messages(settings.clone(), state.session_messages.clone())?;
    if approve {
        if let Some(plan) = state.active_plan.take() {
            agent = agent.with_plan(plan);
        }
    } else {
        agent = agent.plan_only();
    }
    state.task_handle = Some(tokio::spawn(async move {
        agent.run(&prompt, tx).await?;
        Ok(agent.messages)
    }));
    Ok(())
}

/// Dispatch a parsed slash command, mutating TUI state as needed.
///
/// Returns `Ok(true)` if the command was handled (the input should not be
/// treated as a task or plan response). All user-visible feedback is pushed
/// to the log.
fn dispatch_slash_command(
    state: &mut TuiState,
    pc: &commands::ParsedCommand,
    settings: &mut Settings,
    store: &SessionStore,
    session: &mut crate::session::Session,
) -> Result<bool> {
    match pc.name.as_str() {
        "help" => {
            let text = if pc.args.is_empty() {
                commands::help_text()
            } else {
                commands::command_help(&pc.args)
                    .unwrap_or_else(|| format!("Unknown command: /{}", pc.args))
            };
            state.log.push(LogEntry::system(text));
            state.log_dirty = true;
        }
        "plan" => {
            let on = state.toggle_plan_mode();
            state.log.push(LogEntry::system(format!(
                "plan mode {}",
                if on { "on" } else { "off" }
            )));
            state.log_dirty = true;
        }
        "new" => {
            let _ = store.save_all_messages(session, &state.session_messages);
            let _ = store.update_summary(session, None);
            *session = store.create(&settings.model)?;
            state.session_messages.clear();
            state.log.clear();
            state.log.push(LogEntry::system(format!(
                "raven · {} · {}",
                settings.model, settings.base_url
            )));
            state.log.push(LogEntry::system(format!(
                "workspace {}",
                settings.workspace.display()
            )));
            state.log.push(LogEntry::system(String::new()));
            state.log.push(LogEntry::system(
                "new session · enter submit · /plan · /model · /new · /help · /quit",
            ));
            state.log_dirty = true;
            state.plan_preview.clear();
            state.plan_pending = false;
            state.running = false;
            state.agent_state = AgentState::Idle;
            state.status = "ready".into();
            state.assistant_text.clear();
        }
        "clear" => {
            state.log.clear();
            state.log_dirty = true;
        }
        "stop" => {
            if let Some(handle) = state.task_handle.take() {
                handle.abort();
                let _ = store.save_all_messages(session, &state.session_messages);
                let _ = store.update_summary(session, None);
                state
                    .log
                    .push(LogEntry::system("⏹ stopped (partial turn saved)"));
                state.log_dirty = true;
            } else {
                state.log.push(LogEntry::system("nothing running to stop"));
                state.log_dirty = true;
            }
            state.running = false;
            state.agent_state = AgentState::Idle;
            state.status = "ready".into();
            state.assistant_text.clear();
            state.live_tool = None;
        }
        "model" => {
            let name = pc.args.trim();
            if name.is_empty() {
                state.log.push(LogEntry::system(format!(
                    "current model: {}  (try /model <name>)",
                    settings.model
                )));
                state.log_dirty = true;
            } else {
                settings.model = name.to_string();
                settings.context_window = crate::context::infer_context_window(&settings.model);
                settings.max_tokens = Settings::derived_max_tokens(settings.context_window);
                state.log.push(LogEntry::system(format!(
                    "model → {} · context {} · max_tokens {}",
                    settings.model,
                    crate::context::infer_context_window(&settings.model),
                    settings.max_tokens
                )));
                state.log_dirty = true;
            }
        }
        "quit" => {
            state.quit = true;
        }
        "undo" => {
            let sandbox = crate::tools::Sandbox::new(settings.workspace.clone());
            match sandbox.git_undo() {
                Ok(out) => state.log.push(LogEntry::system(out)),
                Err(e) => state
                    .log
                    .push(LogEntry::system(format!("undo failed: {e}"))),
            }
            state.log_dirty = true;
        }
        _ => {
            state.log.push(LogEntry::system(format!(
                "Unknown command: /{}  (try /help)",
                pc.name
            )));
            state.log_dirty = true;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::AgentState;

    #[test]
    fn toggle_plan_off_clears_stuck_pending_approval() {
        let mut state = TuiState {
            plan_first: true,
            plan_pending: true,
            plan_preview: vec!["1. Do X".into()],
            agent_state: AgentState::AwaitingApproval,
            status: "awaiting plan approval".into(),
            ..dummy_state()
        };

        let on = state.toggle_plan_mode();
        assert!(!on, "plan mode should be off");
        assert!(!state.plan_pending, "pending approval must be cleared");
        assert!(
            state.plan_preview.is_empty(),
            "plan preview must be cleared"
        );
        assert_eq!(
            state.agent_state,
            AgentState::Idle,
            "state must reset to Idle"
        );
        assert_eq!(state.status, "ready");
    }

    #[test]
    fn toggle_plan_off_clears_stuck_planning_state() {
        let mut state = TuiState {
            plan_first: true,
            plan_pending: false,
            plan_preview: Vec::new(),
            agent_state: AgentState::Planning,
            status: "planning".into(),
            ..dummy_state()
        };

        state.toggle_plan_mode();
        assert_eq!(state.agent_state, AgentState::Idle);
        assert_eq!(state.status, "ready");
    }

    #[test]
    fn toggle_plan_on_keeps_pending_state() {
        let mut state = TuiState {
            plan_first: false,
            plan_pending: false,
            plan_preview: Vec::new(),
            agent_state: AgentState::Idle,
            status: "ready".into(),
            ..dummy_state()
        };

        let on = state.toggle_plan_mode();
        assert!(on, "plan mode should be on");
        assert_eq!(state.agent_state, AgentState::Idle);
        assert_eq!(state.status, "ready");
    }

    #[test]
    fn spinner_frame_cycles() {
        let f0 = spinner_frame(0);
        let f1 = spinner_frame(4);
        let f2 = spinner_frame(8);
        assert_ne!(f0, f1, "frames should differ");
        assert_ne!(f1, f2, "frames should differ");
        assert!(!f0.is_empty());
    }

    #[test]
    fn waiting_diamond_alternates() {
        let a = waiting_diamond(0);
        let b = waiting_diamond(8);
        assert_ne!(a, b, "diamond should pulse between frames");
    }

    #[test]
    fn state_label_awaiting_answer() {
        let (txt, _color) = state_label(&AgentState::Idle, "awaiting answer");
        assert_eq!(txt, "awaiting answer");
    }

    #[test]
    fn input_box_height_baseline() {
        assert_eq!(input_box_height("hi", 120), 3);
    }

    #[test]
    fn input_box_height_grows_with_multiline_input() {
        let tall = "line1\nline2\nline3\nline4\nline5\nline6\nline7";
        let h = input_box_height(tall, 120);
        assert!(h >= 4, "multi-line input should grow the box, got {h}");
        assert!(h <= 8, "box height should be capped, got {h}");
    }

    #[test]
    fn stop_button_hit_region_is_right_edge() {
        let width = 120u16;
        let btn_len = STOP_BTN.len() as u16;
        let region_start = width.saturating_sub(btn_len);
        assert_eq!(region_start, 120 - 6);
        let term_h = 30u16;
        let input_h = 3u16;
        let status_y = term_h.saturating_sub(input_h).saturating_sub(1);
        assert_eq!(status_y, 26);
    }

    #[test]
    fn prewrap_lines_splits_long_lines() {
        let input = vec![Line::from(Span::raw("abcdefghijklmnopqrstuvwxyz"))];
        let out = prewrap_lines(&input, 10);
        assert_eq!(out.len(), 3, "long line should wrap into 3 rows");
        let joined: String = out.iter().map(|l| l.to_string()).collect();
        assert_eq!(joined, "abcdefghijklmnopqrstuvwxyz");
    }

    #[test]
    fn prewrap_lines_preserves_newlines() {
        let input = vec![Line::from(Span::raw("line1\nline2"))];
        let out = prewrap_lines(&input, 100);
        assert_eq!(out.len(), 2, "newline should split into 2 rows");
        assert_eq!(out[0].to_string(), "line1");
        assert_eq!(out[1].to_string(), "line2");
    }

    #[test]
    fn prewrap_lines_short_line_stays_one_row() {
        let input = vec![Line::from(Span::raw("hi"))];
        let out = prewrap_lines(&input, 20);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_string(), "hi");
    }

    fn dummy_state() -> TuiState {
        TuiState {
            log: Vec::new(),
            log_dirty: false,
            cached_log_lines: Vec::new(),
            last_assistant_lines: 0,
            stream_patch: false,
            cached_est_tokens: 0,
            messages_dirty: false,
            input: String::new(),
            status: String::new(),
            plan_pending: false,
            plan_preview: Vec::new(),
            active_plan: None,
            running: false,
            plan_first: false,
            assistant_text: String::new(),
            agent_state: AgentState::Idle,
            scroll: 0,
            auto_scroll: true,
            quit: false,
            tick: 0,
            live_tool: None,
            turn_tool_count: 0,
            pending_question: None,
            pending_question_text: None,
            session_messages: Vec::new(),
            task_handle: None,
        }
    }
}
