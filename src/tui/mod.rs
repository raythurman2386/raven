//! Interactive TUI — Grok Build–inspired layout for Raven.
//!
//! ┌─ raven ─ qwen2.5-coder:14b ──────────── 12.4K/128K 10% ─ plan:on ─┐
//! │ You                                                                  │
//! │   add auth middleware                                                │
//! │                                                                      │
//! │ → read_file(src/main.rs)                                             │
//! │   \[read_file\] --- src/main.rs (lines 1-40 of 120) ---                │
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
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph, Wrap},
    Frame, Terminal,
};
use std::io::stdout;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, ChatMessage};
use crate::commands;
use crate::config::{Mode, Settings};
use crate::context::history_tokens;
use crate::plan::{self, AgentState};
use crate::session::{Session, SessionStore};

mod blocks;
mod render;
mod selection;
mod status;

use blocks::{AssistantBlock, BlockKind, ErrorBlock, SystemBlock, ToolBlock, UserBlock};
use render::{message_to_log_entry, prewrap_visible, render_assistant_lines, render_blocks};
use selection::{
    apply_selection_highlight, copy_to_clipboard, selection_text, word_bounds, DisplayPos,
    Selection,
};
use status::{fmt_tokens, spinner_frame, state_label, usage_color, waiting_diamond};

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
    const SELECT_BG: Color = Color::Rgb(0x3A, 0x4F, 0x3D); // bg visual selection

    /// Red channel of the tool (orange) accent, for glimmer interpolation.
    const fn tool_r() -> u8 {
        0xE6
    }
    const fn tool_g() -> u8 {
        0x98
    }
    const fn tool_b() -> u8 {
        0x75
    }
    /// Red channel of the dim (grey1) color, for glimmer interpolation.
    const fn dim_r() -> u8 {
        0x85
    }
    const fn dim_g() -> u8 {
        0x92
    }
    const fn dim_b() -> u8 {
        0x89
    }
}

// ── Log model ────────────────────────────────────────────────────────────

#[derive(Clone)]
enum LogKind {
    User,
    Assistant,
    Tool,
    System,
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
}

// ── TUI state ────────────────────────────────────────────────────────────

struct TuiState {
    blocks: Vec<BlockKind>,
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
    mode: Mode,
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
    selection: Option<Selection>,
    last_click: Option<(u64, DisplayPos)>,
    copy_status: Option<(u64, String)>,
}

impl TuiState {
    fn new(settings: &Settings, app_name: &str, compact_at: usize) -> Self {
        Self {
            blocks: vec![
                BlockKind::System(SystemBlock::new(format!(
                    "{app_name} · {} · {}",
                    settings.model, settings.base_url
                ))),
                BlockKind::System(SystemBlock::new(format!(
                    "workspace {}",
                    settings.workspace.display()
                ))),
                BlockKind::System(SystemBlock::new(format!(
                    "context {} · compact ~{}",
                    fmt_tokens(settings.context_window as u64),
                    fmt_tokens(compact_at as u64),
                ))),
                BlockKind::System(SystemBlock::new(String::new())),
                BlockKind::System(SystemBlock::new(
                    "enter submit · /help · /model · /new · shift+tab mode · ctrl+c quit · wheel/pgup scroll".to_string(),
                )),
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
            mode: settings.mode,
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
            selection: None,
            last_click: None,
            copy_status: None,
        }
    }

    fn cycle_mode(&mut self) -> Mode {
        self.mode = self.mode.next();
        if !self.mode.plans_first() {
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
        self.mode
    }

    fn push_user(&mut self, text: impl Into<String>) {
        self.blocks
            .push(BlockKind::User(UserBlock::new(text.into())));
        self.log_dirty = true;
    }

    fn push_assistant(&mut self, text: impl Into<String>) {
        self.blocks
            .push(BlockKind::Assistant(AssistantBlock::new(text.into())));
        self.log_dirty = true;
    }

    fn push_tool(&mut self, text: impl Into<String>) {
        self.blocks
            .push(BlockKind::Tool(ToolBlock::new(text.into())));
        self.log_dirty = true;
    }

    fn push_system(&mut self, text: impl Into<String>) {
        self.blocks
            .push(BlockKind::System(SystemBlock::new(text.into())));
        self.log_dirty = true;
    }

    fn push_error(&mut self, text: impl Into<String>) {
        self.blocks
            .push(BlockKind::Error(ErrorBlock::new(text.into())));
        self.log_dirty = true;
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
        state.push_system(format!(
            "resumed session {} ({} messages)",
            s.summary.id,
            s.messages.len()
        ));
        state.log_dirty = true;
        state.session_messages = s.messages.clone();
        state.messages_dirty = true;
        for msg in &s.messages {
            if let Some(entry) = message_to_log_entry(msg) {
                state
                    .blocks
                    .push(BlockKind::from_kind(entry.kind, entry.text));
            }
        }
        state.push_system(String::new());
        state.push_system(
            "resumed · enter submit · /help · /model · /new · shift+tab mode · ctrl+c quit · wheel/pgup scroll",
        );
        state.log_dirty = true;
        s
    } else {
        store.create(&settings.model)?
    };

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);

    loop {
        if state.blocks.len() > MAX_LOG_ENTRIES {
            let drop = state.blocks.len() - MAX_LOG_ENTRIES;
            state.blocks.drain(..drop);
            state.log_dirty = true;
        }

        if state.log_dirty {
            let (rendered, tail) = render_blocks(&state.blocks, state.tick);
            state.cached_log_lines = rendered;
            state.last_assistant_lines = tail;
            state.log_dirty = false;
            state.stream_patch = false;
        } else if state.stream_patch {
            let tail_text = state
                .blocks
                .iter()
                .rev()
                .find_map(|b| match b {
                    BlockKind::Assistant(a) => Some(a.text()),
                    _ => None,
                })
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
                        KeyCode::BackTab => {
                            let m = state.cycle_mode();
                            state.push_system(format!("mode: {}", m.label()));
                            state.log_dirty = true;
                        }
                        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            let _ = store.save_all_messages(&session, &state.session_messages);
                            let _ = store.update_summary(&mut session, None);
                            session = store.create(&settings.model)?;
                            state.session_messages.clear();
                            state.blocks.clear();
                            state.push_system(format!(
                                "{app_name} · {} · {}",
                                settings.model, settings.base_url
                            ));
                            state
                                .push_system(format!("workspace {}", settings.workspace.display()));
                            state.push_system(String::new());
                            state.push_system(
                                "new session · enter submit · ctrl+n new · shift+tab mode · ctrl+c quit",
                            );
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
                                state.push_system("⏸ interrupted — redirecting…");
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
                Event::Mouse(m) => {
                    let size = terminal.size().unwrap_or_default();
                    let chunks = compute_layout(size, &state);
                    let log_rect = chunks[1];
                    handle_mouse_event(&m, &mut state, size, log_rect, &store, &mut session);
                }
                _ => {}
            }
        }

        // Agent events
        while let Ok(ev) = rx.try_recv() {
            match ev {
                AgentEvent::TextDelta(t) => {
                    state.assistant_text.push_str(&t);
                    if let Some(BlockKind::Assistant(a)) = state.blocks.last_mut() {
                        a.push_chunk(&t);
                        state.stream_patch = true;
                    } else {
                        state.push_assistant(t);
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
                    // Push an inline tool block that glimmers while active.
                    let mut tb = ToolBlock::new(format!("⇢ {name}({snip})"));
                    tb.active = true;
                    state.blocks.push(BlockKind::Tool(tb));
                    state.log_dirty = true;
                }
                AgentEvent::ToolEnd { name, preview } => {
                    let _ = name;
                    let _ = preview;
                    state.live_tool = None;
                    state.status = "running".into();
                    // Mark the last tool block finished so it fades to dim.
                    if let Some(BlockKind::Tool(tb)) = state.blocks.last_mut() {
                        tb.active = false;
                        tb.end_tick = Some(state.tick);
                    }
                    state.log_dirty = true;
                }
                AgentEvent::Iteration(n) => {
                    state.status = format!("thinking… (iter {n})");
                }
                AgentEvent::Compacted {
                    before_tokens,
                    after_tokens,
                } => {
                    state.push_system(format!(
                        "⟳ compacted ~{before_tokens} → ~{after_tokens} tokens"
                    ));
                    state.log_dirty = true;
                }
                AgentEvent::Retry { attempt, delay_ms } => {
                    state.push_system(format!("⟳ retry {attempt}/3 in {delay_ms}ms"));
                    state.log_dirty = true;
                }
                AgentEvent::VerifyRequired => {
                    state.push_system(
                        "⟳ verify required — re-running to enforce run_tests".to_string(),
                    );
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

                    if state.mode.plans_first() && state.agent_state == AgentState::Planning {
                        let plan = plan::parse_plan(&state.assistant_text);
                        state.active_plan = Some(plan.clone());
                        state.plan_preview = plan::format_plan(&plan)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        state.push_system(String::new());
                        state.push_system("plan ready — auto-executing");
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
                    state.push_system(format!("❓ {question}"));
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

                    if state.mode.plans_first() && state.agent_state == AgentState::Planning {
                        let plan = plan::parse_plan(&state.assistant_text);
                        state.active_plan = Some(plan.clone());
                        state.plan_preview = plan::format_plan(&plan)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        state.push_system(String::new());
                        state.push_system("plan ready — approve or revise below");
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
                        state.push_tool(format!(
                            "⇢ {} tool call{} this turn",
                            state.turn_tool_count,
                            if state.turn_tool_count == 1 { "" } else { "s" }
                        ));
                    }
                    state.turn_tool_count = 0;
                    state.live_tool = None;
                    state.log_dirty = true;
                }
                AgentEvent::Error(e) => {
                    state.push_error(e);
                    state.plan_preview.clear();
                    state.status = "ready".into();
                    state.agent_state = AgentState::Idle;
                    state.running = false;
                    state.assistant_text.clear();
                    if state.turn_tool_count > 0 {
                        state.push_tool(format!(
                            "⇢ {} tool call{} this turn",
                            state.turn_tool_count,
                            if state.turn_tool_count == 1 { "" } else { "s" }
                        ));
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

/// Compute the vertical chunk layout for the TUI. Shared by `draw_ui` and the
/// mouse handler so hit-testing agrees with what was actually rendered.
fn compute_layout(area: Rect, state: &TuiState) -> Vec<Rect> {
    let show_plan = state.plan_pending && !state.plan_preview.is_empty();
    let plan_h = if show_plan {
        (state.plan_preview.len().saturating_add(2) as u16).clamp(3, 10)
    } else {
        0
    };
    let input_h = input_box_height(&state.input, area.width);
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(plan_h),
            Constraint::Length(1),
            Constraint::Length(input_h),
        ])
        .split(area)
        .to_vec()
}

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

    let chunks = compute_layout(f.size(), state);

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
    ]);
    f.render_widget(Paragraph::new(top), chunks[0]);

    // Log
    let content_width = (chunks[1].width.saturating_sub(4)) as usize;
    let log_h = chunks[1].height.saturating_sub(2) as usize;
    // Virtualized: pre-wrap only the visible window of the log, not the whole
    // history. `prewrap_visible` returns the visible lines + the scroll offset.
    let (display_lines, offset) = prewrap_visible(
        &state.cached_log_lines,
        content_width.max(1),
        state.scroll as usize,
        log_h,
    );

    // Apply selection highlight to the visible display lines.
    let display_lines = apply_selection_highlight(display_lines, state.selection);

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
    if let Some((start_tick, msg)) = &state.copy_status {
        if state.tick.wrapping_sub(*start_tick) < 50 {
            status_line.push(Span::styled(
                format!("  {msg}"),
                Style::default().fg(Theme::ACCENT),
            ));
        }
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

    let (cx, cy) = input_cursor_position(&state.input, prompt, chunks[4]);
    f.set_cursor(cx, cy);
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

/// Compute the terminal (x, y) where the cursor should sit after the input
/// text, accounting for the prompt prefix, wrapping width, and the input box's
/// top-left position.
fn input_cursor_position(
    input: &str,
    prompt: &str,
    input_rect: ratatui::layout::Rect,
) -> (u16, u16) {
    let content_width = input_rect.width.saturating_sub(2).max(1) as usize;
    let combined = format!("{prompt}{input}");
    let mut col = 0usize;
    let mut row = 0usize;
    for c in combined.chars() {
        if c == '\n' {
            row += 1;
            col = 0;
            continue;
        }
        if col >= content_width {
            row += 1;
            col = 0;
        }
        col += 1;
    }
    let x = input_rect.x + 1 + col as u16;
    let y = input_rect.y + 1 + row as u16;
    (x, y)
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

// ── Mouse selection handling (copy-on-highlight) ──────────────────────────

/// Map a terminal mouse position to a display-line coordinate inside the log
/// region. Returns `None` if the click is outside `log_rect`. The column is
/// adjusted for the log block's left border + horizontal padding (2 cols).
fn mouse_to_display_pos(m: &MouseEvent, log_rect: Rect) -> Option<DisplayPos> {
    if m.row < log_rect.top() || m.row >= log_rect.bottom() {
        return None;
    }
    if m.column < log_rect.left() || m.column >= log_rect.right() {
        return None;
    }
    // The log block has Borders::LEFT | Borders::RIGHT + Padding::horizontal(1),
    // so 2 columns are consumed on each side by border+padding. We only need
    // the left offset to map to the content column.
    let left = log_rect.left() + 2;
    let col = m.column.saturating_sub(left) as usize;
    let row = m.row.saturating_sub(log_rect.top()) as usize;
    Some(DisplayPos { row, col })
}

/// Compute the display lines + scroll offset currently rendered, matching the
/// draw path so hit-testing agrees.
fn current_display(state: &TuiState, log_rect: Rect) -> (Vec<Line<'static>>, u16) {
    let content_width = (log_rect.width.saturating_sub(4)) as usize;
    let log_h = log_rect.height.saturating_sub(2) as usize;
    prewrap_visible(
        &state.cached_log_lines,
        content_width.max(1),
        state.scroll as usize,
        log_h,
    )
}

#[allow(clippy::too_many_arguments)]
fn handle_mouse_event(
    m: &MouseEvent,
    state: &mut TuiState,
    size: Rect,
    log_rect: Rect,
    store: &SessionStore,
    session: &mut Session,
) {
    match m.kind {
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
        MouseEventKind::Down(MouseButton::Left) => {
            // Check the [stop] button first (right edge of status strip).
            let input_h = input_box_height(&state.input, size.width);
            let status_y = size.height.saturating_sub(input_h).saturating_sub(1);
            if state.running
                && m.row == status_y
                && m.column >= size.width.saturating_sub(STOP_BTN.len() as u16)
            {
                if let Some(handle) = state.task_handle.take() {
                    handle.abort();
                    let _ = store.save_all_messages(session, &state.session_messages);
                    let _ = store.update_summary(session, None);
                    state.push_system("⏹ stopped (click)");
                    state.log_dirty = true;
                }
                state.running = false;
                state.agent_state = AgentState::Idle;
                state.status = "ready".into();
                state.assistant_text.clear();
                state.live_tool = None;
                return;
            }

            // Otherwise begin a log selection.
            let (display, offset) = current_display(state, log_rect);
            if let Some(pos) = mouse_to_display_pos(m, log_rect) {
                let display_pos = DisplayPos {
                    row: pos.row + offset as usize,
                    col: pos.col,
                };
                // Double-click → word select.
                if let Some((last_tick, last_pos)) = state.last_click {
                    if state.tick.wrapping_sub(last_tick) < 30
                        && last_pos.row == display_pos.row
                        && (last_pos.col as isize - display_pos.col as isize).abs() <= 2
                    {
                        if let Some(ws) = word_bounds(&display, display_pos) {
                            state.selection = Some(ws);
                            state.copy_status = None;
                        }
                    } else {
                        state.selection = Some(Selection::new(display_pos, display_pos));
                        state.copy_status = None;
                    }
                } else {
                    state.selection = Some(Selection::new(display_pos, display_pos));
                    state.copy_status = None;
                }
                state.last_click = Some((state.tick, display_pos));
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let (display, offset) = current_display(state, log_rect);
            if let Some(pos) = mouse_to_display_pos(m, log_rect) {
                let display_pos = DisplayPos {
                    row: pos.row + offset as usize,
                    col: pos.col,
                };
                if let Some(sel) = state.selection.as_mut() {
                    sel.extend(display_pos);
                    state.copy_status = None;
                }
                let _ = display;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(sel) = state.selection {
                let (display, _offset) = current_display(state, log_rect);
                let text = selection_text(&display, sel);
                if !text.is_empty() {
                    let n = text.chars().count();
                    let copied = copy_to_clipboard(&text);
                    let msg = match copied {
                        Some(_) => format!("copied {n} chars"),
                        None => format!("selected {n} chars (no clipboard tool)"),
                    };
                    state.copy_status = Some((state.tick, msg));
                } else {
                    state.selection = None;
                }
            }
        }
        _ => {}
    }
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
    state.push_user(text.to_string());
    state.log_dirty = true;

    let mut prompt = text.to_string();
    if state.mode.plans_first() {
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
    if state.mode.read_only() {
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
    state.push_user(text.to_string());
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
            state.push_system(text);
            state.log_dirty = true;
        }
        "new" => {
            let _ = store.save_all_messages(session, &state.session_messages);
            let _ = store.update_summary(session, None);
            *session = store.create(&settings.model)?;
            state.session_messages.clear();
            state.blocks.clear();
            state.push_system(format!(
                "raven · {} · {}",
                settings.model, settings.base_url
            ));
            state.push_system(format!("workspace {}", settings.workspace.display()));
            state.push_system(String::new());
            state.push_system("new session · enter submit · /model · /new · /help · /quit");
            state.log_dirty = true;
            state.plan_preview.clear();
            state.plan_pending = false;
            state.running = false;
            state.agent_state = AgentState::Idle;
            state.status = "ready".into();
            state.assistant_text.clear();
        }
        "clear" => {
            state.blocks.clear();
            state.log_dirty = true;
        }
        "stop" => {
            if let Some(handle) = state.task_handle.take() {
                handle.abort();
                let _ = store.save_all_messages(session, &state.session_messages);
                let _ = store.update_summary(session, None);
                state.push_system("⏹ stopped (partial turn saved)");
                state.log_dirty = true;
            } else {
                state.push_system("nothing running to stop");
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
                state.push_system(format!(
                    "current model: {}  (try /model <name>)",
                    settings.model
                ));
                state.log_dirty = true;
            } else {
                settings.model = name.to_string();
                settings.context_window = crate::context::infer_context_window(&settings.model);
                settings.max_tokens = Settings::derived_max_tokens(settings.context_window);
                state.push_system(format!(
                    "model → {} · context {} · max_tokens {}",
                    settings.model,
                    crate::context::infer_context_window(&settings.model),
                    settings.max_tokens
                ));
                state.log_dirty = true;
            }
        }
        "quit" => {
            state.quit = true;
        }
        "undo" => {
            let sandbox = crate::tools::Sandbox::new(settings.workspace.clone());
            match sandbox.git_undo() {
                Ok(out) => state.push_system(out),
                Err(e) => state.push_system(format!("undo failed: {e}")),
            }
            state.log_dirty = true;
        }
        _ => {
            state.push_system(format!("Unknown command: /{}  (try /help)", pc.name));
            state.log_dirty = true;
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::AgentState;
    use render::prewrap_lines;

    #[test]
    fn cycle_mode_clears_stuck_pending_approval_when_leaving_plan() {
        let mut state = TuiState {
            mode: Mode::Plan,
            plan_pending: true,
            plan_preview: vec!["1. Do X".into()],
            agent_state: AgentState::AwaitingApproval,
            status: "awaiting plan approval".into(),
            ..dummy_state()
        };

        let m = state.cycle_mode();
        assert_eq!(m, Mode::Agent, "plan should cycle to agent");
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
    fn cycle_mode_clears_stuck_planning_state_when_leaving_plan() {
        let mut state = TuiState {
            mode: Mode::Plan,
            plan_pending: false,
            plan_preview: Vec::new(),
            agent_state: AgentState::Planning,
            status: "planning".into(),
            ..dummy_state()
        };

        state.cycle_mode();
        assert_eq!(state.agent_state, AgentState::Idle);
        assert_eq!(state.status, "ready");
    }

    #[test]
    fn cycle_mode_cycles_through_all_three() {
        let mut state = TuiState {
            mode: Mode::Plan,
            plan_pending: false,
            plan_preview: Vec::new(),
            agent_state: AgentState::Idle,
            status: "ready".into(),
            ..dummy_state()
        };

        assert_eq!(state.cycle_mode(), Mode::Agent);
        assert_eq!(state.cycle_mode(), Mode::Chat);
        assert_eq!(state.cycle_mode(), Mode::Plan);
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
    fn input_cursor_position_at_end_of_input() {
        let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
        let (x, y) = input_cursor_position("hello", "❯ ", rect);
        assert_eq!(x, 8, "cursor x should be after prompt + input");
        assert_eq!(y, 21, "cursor y should be one row below input box top");
    }

    #[test]
    fn input_cursor_position_wraps_long_input() {
        let rect = ratatui::layout::Rect::new(0, 20, 10, 5);
        let (_x, y) = input_cursor_position("abcdefghijkl", "❯ ", rect);
        assert!(y > 21, "cursor should wrap to next row for long input");
    }

    #[test]
    fn input_cursor_position_empty_input() {
        let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
        let (x, y) = input_cursor_position("", "❯ ", rect);
        assert_eq!(x, 3, "cursor x should be after prompt only");
        assert_eq!(y, 21);
    }

    #[test]
    fn prewrap_lines_short_line_stays_one_row() {
        let input = vec![Line::from(Span::raw("hi"))];
        let out = prewrap_lines(&input, 20);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to_string(), "hi");
    }

    fn mk_lines(texts: &[&str]) -> Vec<Line<'static>> {
        texts
            .iter()
            .map(|t| Line::from(Span::raw((*t).to_string())))
            .collect()
    }

    #[test]
    fn selection_text_single_row_slice() {
        let lines = mk_lines(&["hello world"]);
        let sel = Selection::new(DisplayPos { row: 0, col: 0 }, DisplayPos { row: 0, col: 5 });
        assert_eq!(selection_text(&lines, sel), "hello");
    }

    #[test]
    fn selection_text_single_row_middle() {
        let lines = mk_lines(&["hello world"]);
        let sel = Selection::new(
            DisplayPos { row: 0, col: 6 },
            DisplayPos { row: 0, col: 11 },
        );
        assert_eq!(selection_text(&lines, sel), "world");
    }

    #[test]
    fn selection_text_multi_row() {
        let lines = mk_lines(&["line one", "line two", "line three"]);
        let sel = Selection::new(DisplayPos { row: 0, col: 5 }, DisplayPos { row: 2, col: 5 });
        assert_eq!(selection_text(&lines, sel), "one\nline two\nline ");
    }

    #[test]
    fn selection_text_drag_upwards_normalises() {
        let lines = mk_lines(&["abc", "def"]);
        let sel = Selection::new(DisplayPos { row: 1, col: 2 }, DisplayPos { row: 0, col: 1 });
        assert_eq!(selection_text(&lines, sel), "bc\nde");
    }

    #[test]
    fn selection_text_clamps_past_end() {
        let lines = mk_lines(&["hi"]);
        let sel = Selection::new(
            DisplayPos { row: 0, col: 0 },
            DisplayPos { row: 0, col: 100 },
        );
        assert_eq!(selection_text(&lines, sel), "hi");
    }

    #[test]
    fn selection_text_empty_lines() {
        let lines: Vec<Line<'static>> = Vec::new();
        let sel = Selection::new(DisplayPos { row: 0, col: 0 }, DisplayPos { row: 0, col: 3 });
        assert_eq!(selection_text(&lines, sel), "");
    }

    #[test]
    fn selection_text_wrapped_line_rows() {
        let raw = vec![Line::from(Span::raw("abcdefghij"))];
        let display = prewrap_lines(&raw, 5);
        assert_eq!(display.len(), 2);
        let sel = Selection::new(DisplayPos { row: 0, col: 3 }, DisplayPos { row: 1, col: 2 });
        assert_eq!(selection_text(&display, sel), "de\nfg");
    }

    #[test]
    fn word_bounds_finds_word() {
        let lines = mk_lines(&["foo bar baz"]);
        let pos = DisplayPos { row: 0, col: 4 };
        let sel = word_bounds(&lines, pos).unwrap();
        let (lo, hi) = sel.ordered();
        assert_eq!(lo.col, 4);
        assert_eq!(hi.col, 7);
        assert_eq!(selection_text(&lines, sel), "bar");
    }

    #[test]
    fn word_bounds_on_whitespace_returns_none() {
        let lines = mk_lines(&["foo bar"]);
        let pos = DisplayPos { row: 0, col: 3 };
        assert!(word_bounds(&lines, pos).is_none());
    }

    #[test]
    fn word_bounds_first_word() {
        let lines = mk_lines(&["hello world"]);
        let pos = DisplayPos { row: 0, col: 0 };
        let sel = word_bounds(&lines, pos).unwrap();
        assert_eq!(selection_text(&lines, sel), "hello");
    }

    #[test]
    fn apply_selection_highlight_single_row() {
        let lines = mk_lines(&["hello world"]);
        let sel = Some(Selection::new(
            DisplayPos { row: 0, col: 0 },
            DisplayPos { row: 0, col: 5 },
        ));
        let out = apply_selection_highlight(lines, sel);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans.len(), 2);
        assert_eq!(out[0].spans[0].content, "hello");
        assert_eq!(out[0].spans[1].content, " world");
    }

    #[test]
    fn apply_selection_highlight_none_unchanged() {
        let lines = mk_lines(&["hello", "world"]);
        let out = apply_selection_highlight(lines, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].spans.len(), 1);
        assert_eq!(out[0].spans[0].content, "hello");
    }

    #[test]
    fn apply_selection_highlight_multi_row() {
        let lines = mk_lines(&["abc", "def", "ghi"]);
        let sel = Some(Selection::new(
            DisplayPos { row: 0, col: 1 },
            DisplayPos { row: 2, col: 2 },
        ));
        let out = apply_selection_highlight(lines, sel);
        assert_eq!(out[0].spans.len(), 2);
        assert_eq!(out[0].spans[0].content, "a");
        assert_eq!(out[0].spans[1].content, "bc");
        assert_eq!(out[1].spans.len(), 1);
        assert_eq!(out[1].spans[0].content, "def");
        assert_eq!(out[2].spans.len(), 2);
        assert_eq!(out[2].spans[0].content, "gh");
        assert_eq!(out[2].spans[1].content, "i");
    }

    #[test]
    fn apply_selection_highlight_preserves_span_styles() {
        // A line with two differently-styled spans; the selection must keep
        // each span's own style and only add the SELECT_BG to the selected
        // segment.
        let line = Line::from(vec![
            Span::styled("ab", Style::default().fg(Color::Red)),
            Span::styled("cd", Style::default().fg(Color::Blue)),
        ]);
        let sel = Some(Selection::new(
            DisplayPos { row: 0, col: 1 },
            DisplayPos { row: 0, col: 3 },
        ));
        let out = apply_selection_highlight(vec![line], sel);
        assert_eq!(out[0].spans.len(), 4);
        assert_eq!(out[0].spans[0].content, "a");
        assert_eq!(out[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(out[0].spans[0].style.bg, None);
        assert_eq!(out[0].spans[1].content, "b");
        assert_eq!(out[0].spans[1].style.fg, Some(Color::Red));
        assert_eq!(out[0].spans[1].style.bg, Some(Theme::SELECT_BG));
        assert_eq!(out[0].spans[2].content, "c");
        assert_eq!(out[0].spans[2].style.fg, Some(Color::Blue));
        assert_eq!(out[0].spans[2].style.bg, Some(Theme::SELECT_BG));
        assert_eq!(out[0].spans[3].content, "d");
        assert_eq!(out[0].spans[3].style.fg, Some(Color::Blue));
        assert_eq!(out[0].spans[3].style.bg, None);
    }

    #[test]
    fn mouse_to_display_pos_outside_returns_none() {
        let rect = Rect::new(0, 1, 80, 20);
        let m = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        assert!(mouse_to_display_pos(&m, rect).is_none());
    }

    #[test]
    fn mouse_to_display_pos_inside_adjusts_for_border() {
        let rect = Rect::new(0, 1, 80, 20);
        let m = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 3,
            modifiers: KeyModifiers::NONE,
        };
        let pos = mouse_to_display_pos(&m, rect).unwrap();
        assert_eq!(pos.row, 2);
        assert_eq!(pos.col, 3);
    }

    fn dummy_state() -> TuiState {
        TuiState {
            blocks: Vec::new(),
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
            mode: Mode::Agent,
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
            selection: None,
            last_click: None,
            copy_status: None,
        }
    }
}
