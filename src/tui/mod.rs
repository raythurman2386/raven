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
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
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
mod completion;
mod markdown;
mod render;
mod selection;
mod status;
mod theme;

pub use theme::Theme;

use blocks::{AssistantBlock, BlockKind, ErrorBlock, SystemBlock, ToolBlock, UserBlock};
use completion::{apply as apply_completion, candidates_for, Completion};
use render::{message_to_block, prewrap_visible, render_assistant_lines, render_blocks};
use selection::{
    apply_selection_highlight, copy_to_clipboard, selection_text, word_bounds, DisplayPos,
    Selection,
};
use status::{fmt_tokens, spinner_frame, state_label, usage_color, waiting_diamond};

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
    /// Byte index of the edit cursor within `input`. Text is inserted/removed
    /// at this position; `Left`/`Right`/`Home`/`End` move it.
    cursor: usize,
    /// Active slash-command autocomplete, if any.
    completion: Option<Completion>,
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
    plan_scroll: u16,
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
    theme: Theme,
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
            cursor: 0,
            completion: None,
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
            plan_scroll: 0,
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
            theme: Theme::by_name(&settings.theme).unwrap_or_else(Theme::default_theme),
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
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut compact_at = ((settings.context_window - settings.context_window / 8) as f32
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
            if let Some(block) = message_to_block(msg) {
                state.blocks.push(block);
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

    // Argument-completion candidates per command. `/theme` completes from the
    // theme registry; other commands have no argument candidates.
    let arg_candidates = |cmd: &str| -> Vec<String> {
        if cmd == "theme" {
            Theme::all().iter().map(|(n, _)| n.to_string()).collect()
        } else {
            Vec::new()
        }
    };

    loop {
        if state.blocks.len() > MAX_LOG_ENTRIES {
            let drop = state.blocks.len() - MAX_LOG_ENTRIES;
            state.blocks.drain(..drop);
            state.log_dirty = true;
        }

        // Capture whether the log/stream changed this tick *before* the flags
        // are cleared below, so we can force an immediate draw (rather than
        // waiting for the DRAW_INTERVAL throttle).
        let dirty = state.log_dirty || state.stream_patch || state.messages_dirty;

        if state.log_dirty {
            let (rendered, tail) = render_blocks(&state.blocks, state.tick, state.theme);
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
            let new_tail = render_assistant_lines(tail_text, state.theme);
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

        let force_draw = dirty || !state.running || state.live_tool.is_some();
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
                            if !state.running
                                && state.pending_question.is_none()
                                && state.completion.is_some()
                            {
                                // Cycle completion backward; fall through to
                                // mode-cycle when no completion is active.
                                if let Some(comp) = state.completion.as_mut() {
                                    comp.prev();
                                }
                            } else {
                                let m = state.cycle_mode();
                                state.push_system(format!("mode: {}", m.label()));
                                state.log_dirty = true;
                            }
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
                        KeyCode::Left => {
                            if !state.running || state.pending_question.is_some() {
                                // Move cursor left by one char (byte-safe).
                                if let Some(prev) = state.input[..state.cursor]
                                    .char_indices()
                                    .next_back()
                                    .map(|(i, _)| i)
                                {
                                    state.cursor = prev;
                                }
                            }
                        }
                        KeyCode::Right => {
                            if !state.running || state.pending_question.is_some() {
                                if let Some(next) = state.input[state.cursor..]
                                    .char_indices()
                                    .nth(1)
                                    .map(|(i, _)| state.cursor + i)
                                {
                                    state.cursor = next;
                                }
                            }
                        }
                        KeyCode::Home => {
                            if !state.running || state.pending_question.is_some() {
                                state.cursor = 0;
                            }
                        }
                        KeyCode::End => {
                            if !state.running || state.pending_question.is_some() {
                                state.cursor = state.input.len();
                            }
                        }
                        KeyCode::Tab => {
                            if !state.running || state.pending_question.is_some() {
                                if let Some(comp) = state.completion.as_mut() {
                                    if comp.candidates.len() == 1 {
                                        // Single candidate: accept it immediately.
                                        let cand = comp.candidates[0].clone();
                                        let (new_input, new_cursor) =
                                            apply_completion(&state.input, comp, &cand);
                                        state.input = new_input;
                                        state.cursor = new_cursor;
                                        state.completion = None;
                                    } else {
                                        comp.next();
                                    }
                                }
                            }
                        }
                        KeyCode::Char(c) => {
                            if (!state.running || state.pending_question.is_some())
                                && state.input.chars().count() < MAX_INPUT_CHARS
                            {
                                state.input.insert(state.cursor, c);
                                state.cursor += c.len_utf8();
                            }
                            state.completion = candidates_for(&state.input, &arg_candidates);
                        }
                        KeyCode::Backspace => {
                            if !state.running || state.pending_question.is_some() {
                                if let Some(prev) = state.input[..state.cursor]
                                    .char_indices()
                                    .next_back()
                                    .map(|(i, _)| i)
                                {
                                    state.input.remove(prev);
                                    state.cursor = prev;
                                }
                            }
                            state.completion = candidates_for(&state.input, &arg_candidates);
                        }
                        KeyCode::Enter => {
                            if state.input.trim().is_empty() {
                                continue;
                            }
                            let text = state.input.trim().to_string();
                            state.input.clear();
                            state.cursor = 0;
                            state.completion = None;
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
                                    &mut compact_at,
                                )
                                .await?;
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
                Event::Paste(text) => {
                    // Bracketed paste: append the entire pasted block at once.
                    // Without this, a large paste arrives as a rapid stream of
                    // individual Char events that get dropped by the poll loop,
                    // and any newline in the pasted text is treated as Enter
                    // (submitting mid-paste).
                    if !state.running || state.pending_question.is_some() {
                        // Cap the paste so a multi-MB paste can't grow memory
                        // or slow down per-frame re-wrap unboundedly.
                        let remaining = MAX_INPUT_CHARS.saturating_sub(state.input.chars().count());
                        state
                            .input
                            .push_str(&text.chars().take(remaining).collect::<String>());
                    }
                }
                Event::Mouse(m) => {
                    let size: Rect = terminal.size().unwrap_or_default().into();
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
                AgentEvent::PlanProgress(plan) => {
                    state.plan_preview = plan::format_plan(&plan)
                        .lines()
                        .map(|s| s.to_string())
                        .collect();
                    // Keep `active_plan` in sync so the status-strip "N/M steps"
                    // readout reflects live `[x]`/`[~]` progress, not the plan
                    // as it was at approval time.
                    state.active_plan = Some(plan);
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
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    Ok(())
}

// ── Drawing ──────────────────────────────────────────────────────────────

/// Compute the vertical chunk layout for the TUI. Shared by `draw_ui` and the
/// mouse handler so hit-testing agrees with what was actually rendered.
/// Whether the plan panel should be visible. It shows while a plan is pending
/// approval *and* while the agent is executing a plan, so the live
/// `[ ]` → `[~]` → `[x]` step updates are visible as the agent works through
/// the task list.
fn show_plan(state: &TuiState) -> bool {
    !state.plan_preview.is_empty() && (state.plan_pending || state.running)
}

/// Count completed vs total plan steps for the status-strip progress readout.
/// A step counts as done when `Completed` or `Skipped`.
fn plan_step_progress(plan: &crate::plan::Plan) -> (usize, usize) {
    let total = plan.steps.len();
    let done = plan
        .steps
        .iter()
        .filter(|s| {
            matches!(
                s.status,
                crate::plan::PlanStepStatus::Completed | crate::plan::PlanStepStatus::Skipped
            )
        })
        .count();
    (done, total)
}

fn compute_layout(area: Rect, state: &TuiState) -> Vec<Rect> {
    let plan_h = if show_plan(state) {
        (state.plan_preview.len().saturating_add(2) as u16).clamp(3, 10)
    } else {
        0
    };
    let input_h = input_box_height(&state.input, area.width);
    // Completion popup sits between the status strip and the input box.
    let completion_h = if let Some(c) = &state.completion {
        (c.candidates.len() as u16).clamp(1, 6).saturating_add(2)
    } else {
        0
    };
    Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(5),
            Constraint::Length(plan_h),
            Constraint::Length(1),
            Constraint::Length(completion_h),
            Constraint::Length(input_h),
        ])
        .split(area)
        .to_vec()
}

fn draw_ui(f: &mut Frame, app_name: &str, settings: &Settings, state: &TuiState) {
    let theme = state.theme;
    let pct = if settings.context_window > 0 {
        (state.cached_est_tokens as f64 / settings.context_window as f64) * 100.0
    } else {
        0.0
    };
    let (state_txt, state_color) = state_label(&state.agent_state, &state.status, state.theme);

    let show_plan = show_plan(state);
    let plan_h = if show_plan {
        (state.plan_preview.len().saturating_add(2) as u16).clamp(3, 10)
    } else {
        0
    };

    let chunks = compute_layout(f.area(), state);

    // Top bar — product · model · context
    let top = Line::from(vec![
        Span::styled(
            format!(" {app_name} "),
            Style::default()
                .fg(Color::Black)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            settings.model.clone(),
            Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ·  ", Style::default().fg(theme.dim)),
        Span::styled(
            format!(
                "{}/{} ({:.0}%)",
                fmt_tokens(state.cached_est_tokens as u64),
                fmt_tokens(settings.context_window as u64),
                pct
            ),
            Style::default().fg(usage_color(pct, theme)),
        ),
        Span::styled("  ·  ", Style::default().fg(theme.dim)),
    ]);
    f.render_widget(Paragraph::new(top), chunks[0]);

    // Log
    let content_width = (chunks[1].width.saturating_sub(4)) as usize;
    // The log block has LEFT|RIGHT|BOTTOM borders (no top), so only the
    // bottom border consumes a content row.
    let log_h = chunks[1].height.saturating_sub(1) as usize;
    // Virtualized: pre-wrap only the visible window of the log, not the whole
    // history. `prewrap_visible` returns the visible lines (already sliced to
    // the viewport) plus the scroll offset. The offset is used only for mouse
    // hit-testing (`current_display`); the Paragraph must NOT scroll again,
    // or the visible window would be pushed off-screen.
    let (display_lines, _offset) = prewrap_visible(
        &state.cached_log_lines,
        content_width.max(1),
        state.scroll as usize,
        log_h,
    );

    // Apply selection highlight to the visible display lines.
    let display_lines = apply_selection_highlight(display_lines, state.selection, theme);

    let log_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(Style::default().fg(theme.border))
        .padding(Padding::horizontal(1));
    let log_widget = Paragraph::new(display_lines)
        .block(log_block)
        .scroll((0, 0));
    f.render_widget(log_widget, chunks[1]);

    // Plan panel
    if show_plan && plan_h > 0 {
        let mut lines: Vec<Line> = state
            .plan_preview
            .iter()
            .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(theme.plan))))
            .collect();
        lines.push(Line::from(Span::styled(
            "yes to execute · or type revisions",
            Style::default().fg(theme.dim),
        )));
        let plan_widget = Paragraph::new(lines).scroll((state.plan_scroll, 0)).block(
            Block::default()
                .title(Span::styled(" plan ", Style::default().fg(theme.plan)))
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.plan)),
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
            Style::default().fg(theme.dim),
        ),
    ];
    // Plan step progress: "N/M steps" while a plan is active.
    if let Some(plan) = &state.active_plan {
        let (done, total) = plan_step_progress(plan);
        if total > 0 {
            status_line.push(Span::styled(
                format!("  {done}/{total} steps"),
                Style::default().fg(theme.plan),
            ));
        }
    }
    if let Some(tool) = &state.live_tool {
        status_line.push(Span::styled(
            format!(" {} {}", spinner_frame(state.tick), tool),
            Style::default().fg(theme.tool),
        ));
    }
    if state.pending_question_text.is_some() {
        status_line.push(Span::styled(
            format!(" {}", waiting_diamond(state.tick)),
            Style::default().fg(theme.plan),
        ));
    }
    if let Some((start_tick, msg)) = &state.copy_status {
        if state.tick.wrapping_sub(*start_tick) < 50 {
            status_line.push(Span::styled(
                format!("  {msg}"),
                Style::default().fg(theme.accent),
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
        status_line.push(stop_span(theme));
    }

    f.render_widget(
        Paragraph::new(Line::from(status_line)).style(Style::default().bg(theme.status_bg)),
        chunks[3],
    );

    // Completion popup (between status strip and input box).
    if let Some(comp) = &state.completion {
        let lines: Vec<Line> = comp
            .candidates
            .iter()
            .enumerate()
            .map(|(i, cand)| {
                let style = if i == comp.selected {
                    Style::default()
                        .fg(theme.status_bg)
                        .bg(theme.accent)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(theme.fg)
                };
                Line::from(Span::styled(cand.clone(), style))
            })
            .collect();
        let popup = Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(theme.accent))
                .title(Span::styled(" tab ", Style::default().fg(theme.dim))),
        );
        f.render_widget(popup, chunks[4]);
    }

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
                        theme.plan
                    } else {
                        theme.accent
                    },
                )
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(state.input.to_string(), Style::default().fg(theme.fg)),
    ]);
    let input_w = Paragraph::new(input_line).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if state.plan_pending {
                theme.plan
            } else {
                theme.border
            }))
            .title(Span::styled(title, Style::default().fg(theme.dim))),
    );
    f.render_widget(input_w, chunks[5]);

    let (cx, cy) = input_cursor_position(&state.input, prompt, state.cursor, chunks[5]);
    f.set_cursor_position((cx, cy));
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

/// The effective content width (in chars) of the input box for a given
/// terminal width.
///
/// The input box has `Borders::ALL` (2 border cols) plus a prompt glyph
/// (e.g. `❯ `, 2 chars). Both the wrap-width computation and the cursor
/// position must use this same value, or the cursor will land in the wrong
/// place once the input wraps to a second line.
fn input_content_width(term_width: u16) -> usize {
    let avail = term_width.saturating_sub(2).max(1) as usize; // minus 2 border cols
    avail.saturating_sub(2).max(1) // minus prompt glyph "❯ "
}

/// Compute the terminal (x, y) where the cursor should sit after the input
/// text, accounting for the prompt prefix, wrapping width, and the input box's
/// top-left position.
fn input_cursor_position(
    input: &str,
    prompt: &str,
    cursor: usize,
    input_rect: ratatui::layout::Rect,
) -> (u16, u16) {
    // The input line is `prompt + input`, rendered in a Paragraph with
    // Borders::ALL. Ratatui wraps the whole line at the content area width
    // (rect.width - 2 for the two border columns). The prompt is part of the
    // wrapped line, NOT a separate column reservation, so the wrap width here
    // must be rect.width - 2 — not input_content_width (which also subtracts
    // the prompt and would put the cursor 2 columns off once input wraps).
    let content_width = input_rect.width.saturating_sub(2).max(1) as usize;
    // Walk the combined prompt+input up to the cursor byte offset.
    let mut col = 0usize;
    let mut row = 0usize;
    for c in prompt.chars().chain(input[..cursor].chars()) {
        if c == '\n' {
            row += 1;
            col = 0;
        } else if col >= content_width {
            row += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    // Clamp the cursor to the last visible row of the box so a long input
    // doesn't push the cursor off-screen (the box stops growing at
    // MAX_INPUT_BOX_HEIGHT, but the input text continues).
    let max_row = input_rect.height.saturating_sub(2).max(1) as usize;
    let row = row.min(max_row.saturating_sub(1));
    let x = input_rect.x + 1 + col as u16;
    let y = input_rect.y + 1 + row as u16;
    (x, y)
}

/// Maximum height (in rows, including borders) the input box may grow to.
///
/// The box grows as the input wraps so long tasks stay visible. The cap is a
/// safety bound so a very long input doesn't consume the whole terminal; the
/// input itself is never truncated, only the box stops growing (the cursor
/// stays on the last visible row).
const MAX_INPUT_BOX_HEIGHT: u16 = 12;

/// Maximum number of characters the input buffer may hold. A hard cap prevents
/// a multi-MB paste from growing memory or slowing per-frame re-wrap without
/// bound.
const MAX_INPUT_CHARS: usize = 100_000;

/// Height of the input box (in rows, including borders) for a given input and
/// terminal width. Shared by the draw path and click-hit-testing so both agree
/// on where the status strip (the row just above the input) sits.
fn input_box_height(input: &str, term_width: u16) -> u16 {
    let avail = input_content_width(term_width);
    let lines = wrapped_line_count(input, avail).clamp(1, MAX_INPUT_BOX_HEIGHT as usize) as u16;
    lines.saturating_add(2) // + top/bottom border rows
}

/// The `[stop]` button rendered at the right edge of the status strip.
const STOP_BTN: &str = "[stop]";

/// Build a `Span` for the `[stop]` button in the status strip (right-aligned).
/// The caller right-aligns it against the status row's width.
fn stop_span(theme: Theme) -> Span<'static> {
    Span::styled(
        STOP_BTN.to_string(),
        Style::default()
            .fg(theme.error)
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
    // Match `draw_ui`: the log block has LEFT|RIGHT|BOTTOM borders (no top),
    // so only the bottom border consumes a content row.
    let log_h = log_rect.height.saturating_sub(1) as usize;
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
            // If the mouse is over the plan panel, scroll the plan instead of
            // the log.
            let chunks = compute_layout(size, state);
            if show_plan(state) && m.row >= chunks[2].top() && m.row < chunks[2].bottom() {
                state.plan_scroll = state.plan_scroll.saturating_add(1);
            } else {
                state.scroll = state.scroll.saturating_add(3);
                state.auto_scroll = false;
            }
        }
        MouseEventKind::ScrollDown => {
            let chunks = compute_layout(size, state);
            if show_plan(state) && m.row >= chunks[2].top() && m.row < chunks[2].bottom() {
                state.plan_scroll = state.plan_scroll.saturating_sub(1);
            } else {
                state.scroll = state.scroll.saturating_sub(3);
                if state.scroll == 0 {
                    state.auto_scroll = true;
                }
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
            let (display, _offset) = current_display(state, log_rect);
            if let Some(pos) = mouse_to_display_pos(m, log_rect) {
                // `pos` is already in visible-window coordinates (relative to
                // the log viewport). The selection is stored in the same space
                // that `apply_selection_highlight` and `selection_text` read
                // (the visible window), so do NOT add the scroll offset here —
                // doing so would put the highlight/copy on the wrong rows once
                // the log is scrolled.
                let display_pos = DisplayPos {
                    row: pos.row,
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
            let (display, _offset) = current_display(state, log_rect);
            if let Some(pos) = mouse_to_display_pos(m, log_rect) {
                // Same visible-window coordinate space as the Down handler.
                let display_pos = DisplayPos {
                    row: pos.row,
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
    // Keep the plan preview visible during execution so the live step updates
    // (`[ ]` → `[~]` → `[x]`) are shown as the agent works through the list.
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

/// The canonical name of a theme, for display in `/theme` output.
fn theme_name(theme: Theme) -> &'static str {
    Theme::all()
        .iter()
        .find(|(_, t)| *t == theme)
        .map(|(n, _)| *n)
        .unwrap_or("?")
}

/// Dispatch a parsed slash command, mutating TUI state as needed.
///
/// Returns `Ok(true)` if the command was handled (the input should not be
/// treated as a task or plan response). All user-visible feedback is pushed
/// to the log.
async fn dispatch_slash_command(
    state: &mut TuiState,
    pc: &commands::ParsedCommand,
    settings: &mut Settings,
    store: &SessionStore,
    session: &mut crate::session::Session,
    compact_at: &mut usize,
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
                // Match startup behaviour: prefer the live Ollama `/api/show`
                // value, falling back to the name heuristic when unreachable.
                settings.context_window =
                    crate::context::fetch_context_window(&settings.base_url, &settings.model).await;
                settings.max_tokens = Settings::derived_max_tokens(settings.context_window);
                *compact_at = ((settings.context_window - settings.context_window / 8) as f32
                    * settings.compact_threshold) as usize;

                // Persist the new model on the session so a resume shows it.
                let _ = store.update_model(session, &settings.model);

                // Refresh the static header blocks (model + context/compact).
                if let Some(BlockKind::System(b)) = state.blocks.get_mut(0) {
                    b.set_text(format!(
                        "raven · {} · {}",
                        settings.model, settings.base_url
                    ));
                }
                if let Some(BlockKind::System(b)) = state.blocks.get_mut(2) {
                    b.set_text(format!(
                        "context {} · compact ~{}",
                        fmt_tokens(settings.context_window as u64),
                        fmt_tokens(*compact_at as u64),
                    ));
                }

                state.push_system(format!(
                    "model → {} · context {} · max_tokens {}",
                    settings.model, settings.context_window, settings.max_tokens
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
        "theme" => {
            let name = pc.args.trim();
            if name.is_empty() {
                // List available themes.
                let names: Vec<&str> = Theme::all().iter().map(|(n, _)| *n).collect();
                state.push_system(format!(
                    "themes: {}  (current: {})  ·  /theme <name>",
                    names.join(", "),
                    theme_name(state.theme)
                ));
                state.log_dirty = true;
            } else if let Some(t) = Theme::by_name(name) {
                state.theme = t;
                // Force a full re-render so the whole scrollback recolors.
                state.push_system(format!("theme → {}", theme_name(t)));
                state.log_dirty = true;
            } else {
                state.push_system(format!("unknown theme: {name}  (try /theme to list)"));
                state.log_dirty = true;
            }
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
        let (txt, _color) = state_label(&AgentState::Idle, "awaiting answer", Theme::RAVENWOOD);
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
        assert!(
            h <= MAX_INPUT_BOX_HEIGHT + 2,
            "box height should be capped at MAX_INPUT_BOX_HEIGHT + borders, got {h}"
        );
    }

    #[test]
    fn input_box_height_caps_at_max() {
        // A very long single-line input should not grow the box past the cap.
        let long = "x".repeat(1000);
        let h = input_box_height(&long, 40);
        assert_eq!(h, MAX_INPUT_BOX_HEIGHT + 2, "box should cap at max height");
    }

    #[test]
    fn input_chars_capped_by_max_input_chars() {
        // A paste larger than the cap is truncated to MAX_INPUT_CHARS.
        let over = "x".repeat(MAX_INPUT_CHARS + 5000);
        let capped = over.chars().take(MAX_INPUT_CHARS).collect::<String>();
        assert_eq!(capped.chars().count(), MAX_INPUT_CHARS);
    }

    #[test]
    fn input_cursor_position_clamps_to_box_height() {
        // A very long input that exceeds the box height should clamp the cursor
        // to the last visible row, not push it off-screen.
        let rect = ratatui::layout::Rect::new(0, 20, 40, MAX_INPUT_BOX_HEIGHT + 2);
        let long = "x".repeat(1000);
        let (_x, y) = input_cursor_position(&long, "❯ ", long.len(), rect);
        assert!(
            y < rect.bottom(),
            "cursor y should stay within the box, got {y} (box bottom {})",
            rect.bottom()
        );
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
        let (x, y) = input_cursor_position("hello", "❯ ", 5, rect);
        assert_eq!(x, 8, "cursor x should be after prompt + input");
        assert_eq!(y, 21, "cursor y should be one row below input box top");
    }

    #[test]
    fn input_cursor_position_wraps_long_input() {
        let rect = ratatui::layout::Rect::new(0, 20, 10, 5);
        let (_x, y) = input_cursor_position("abcdefghijkl", "❯ ", 12, rect);
        assert!(y > 21, "cursor should wrap to next row for long input");
    }

    #[test]
    fn input_cursor_position_wraps_at_content_width_not_prompt_width() {
        // Regression: the cursor must wrap at the Paragraph content width
        // (rect.width - 2 for borders), NOT at input_content_width (which also
        // subtracts the prompt). With rect width 10, content width is 8, so
        // "❯ " + 6 input chars fill row 0 and the 7th input char starts row 1.
        // A cursor at byte 7 (after "abcdefg") must be on row 1, col 1.
        let rect = ratatui::layout::Rect::new(0, 20, 10, 5);
        let (x, y) = input_cursor_position("abcdefghijkl", "❯ ", 7, rect);
        assert_eq!(y, 22, "cursor should be on the second row, got y={y}");
        assert_eq!(x, 2, "cursor should be at col 1 inside the box, got x={x}");
    }

    #[test]
    fn input_cursor_position_empty_input() {
        let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
        let (x, y) = input_cursor_position("", "❯ ", 0, rect);
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
        let out = apply_selection_highlight(lines, sel, Theme::RAVENWOOD);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].spans.len(), 2);
        assert_eq!(out[0].spans[0].content, "hello");
        assert_eq!(out[0].spans[1].content, " world");
    }

    #[test]
    fn apply_selection_highlight_none_unchanged() {
        let lines = mk_lines(&["hello", "world"]);
        let out = apply_selection_highlight(lines, None, Theme::RAVENWOOD);
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
        let out = apply_selection_highlight(lines, sel, Theme::RAVENWOOD);
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
        let out = apply_selection_highlight(vec![line], sel, Theme::RAVENWOOD);
        assert_eq!(out[0].spans.len(), 4);
        assert_eq!(out[0].spans[0].content, "a");
        assert_eq!(out[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(out[0].spans[0].style.bg, None);
        assert_eq!(out[0].spans[1].content, "b");
        assert_eq!(out[0].spans[1].style.fg, Some(Color::Red));
        assert_eq!(out[0].spans[1].style.bg, Some(Theme::RAVENWOOD.select_bg));
        assert_eq!(out[0].spans[2].content, "c");
        assert_eq!(out[0].spans[2].style.fg, Some(Color::Blue));
        assert_eq!(out[0].spans[2].style.bg, Some(Theme::RAVENWOOD.select_bg));
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
            cursor: 0,
            completion: None,
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
            plan_scroll: 0,
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
            theme: Theme::RAVENWOOD,
        }
    }

    #[test]
    fn show_plan_visible_while_pending() {
        let mut state = dummy_state();
        state.plan_pending = true;
        state.plan_preview = vec!["1. Do X".into()];
        assert!(show_plan(&state));
    }

    #[test]
    fn show_plan_visible_while_running() {
        let mut state = dummy_state();
        state.running = true;
        state.plan_preview = vec!["1. Do X".into()];
        assert!(show_plan(&state));
    }

    #[test]
    fn show_plan_hidden_when_no_preview() {
        let mut state = dummy_state();
        state.running = true;
        state.plan_preview.clear();
        assert!(!show_plan(&state));
    }

    #[test]
    fn plan_step_progress_counts_completed() {
        use crate::plan::{Plan, PlanStep, PlanStepStatus};
        let plan = Plan {
            title: "t".into(),
            created_at: "now".into(),
            steps: vec![
                PlanStep {
                    description: "a".into(),
                    status: PlanStepStatus::Completed,
                },
                PlanStep {
                    description: "b".into(),
                    status: PlanStepStatus::InProgress,
                },
                PlanStep {
                    description: "c".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "d".into(),
                    status: PlanStepStatus::Skipped,
                },
            ],
        };
        let (done, total) = plan_step_progress(&plan);
        assert_eq!(done, 2);
        assert_eq!(total, 4);
    }

    fn test_settings(workspace: &std::path::Path) -> Settings {
        Settings {
            model: "gemma4:latest".into(),
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            workspace: workspace.to_path_buf(),
            max_iterations: 5,
            mode: Mode::Agent,
            yolo: true,
            temperature: 0.0,
            max_tokens: 4096,
            rules: None,
            context_window: 128_000,
            compact_threshold: 0.75,
            no_stream: false,
            verify: false,
            confirm_shell: false,
            theme: "ravenwood".into(),
        }
    }

    #[tokio::test]
    async fn model_switch_updates_settings_compact_and_header_blocks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let mut session = store.create("gemma4:latest").unwrap();
        let mut settings = test_settings(tmp.path());
        let mut state = dummy_state();
        // Seed the header blocks the way TuiState::new does.
        state.blocks = vec![
            BlockKind::System(SystemBlock::new(format!(
                "raven · {} · {}",
                settings.model, settings.base_url
            ))),
            BlockKind::System(SystemBlock::new(format!(
                "workspace {}",
                settings.workspace.display()
            ))),
            BlockKind::System(SystemBlock::new(format!(
                "context {} · compact ~{}",
                fmt_tokens(settings.context_window as u64),
                fmt_tokens(128_000 - 128_000 / 8),
            ))),
        ];
        let mut compact_at = 128_000 - 128_000 / 8;

        let pc = commands::parse("/model deepseek-v4-pro:cloud").unwrap();
        let handled = dispatch_slash_command(
            &mut state,
            &pc,
            &mut settings,
            &store,
            &mut session,
            &mut compact_at,
        )
        .await
        .unwrap();

        assert!(handled);
        assert_eq!(settings.model, "deepseek-v4-pro:cloud");
        // deepseek-v4-pro:cloud → 524_288 (name heuristic; API unreachable in test).
        assert_eq!(settings.context_window, 524_288);
        assert_eq!(settings.max_tokens, Settings::derived_max_tokens(524_288));
        // compact_at recomputed from the new window (window - reserve) * threshold.
        let expected_compact =
            ((524_288 - 524_288 / 8) as f32 * settings.compact_threshold) as usize;
        assert_eq!(compact_at, expected_compact);
        // Session model persisted.
        assert_eq!(session.summary.model, "deepseek-v4-pro:cloud");
        // Header blocks refreshed.
        if let BlockKind::System(b) = &state.blocks[0] {
            assert!(b.text().contains("deepseek-v4-pro:cloud"));
        } else {
            panic!("block 0 should be a SystemBlock");
        }
        if let BlockKind::System(b) = &state.blocks[2] {
            assert!(b.text().contains("524K"), "context block: {}", b.text());
        } else {
            panic!("block 2 should be a SystemBlock");
        }
    }

    #[tokio::test]
    async fn theme_command_switches_theme_and_lists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = SessionStore::for_workspace(tmp.path()).unwrap();
        let mut session = store.create("gemma4:latest").unwrap();
        let mut settings = test_settings(tmp.path());
        let mut state = dummy_state();
        let mut compact_at = 128_000 - 128_000 / 8;

        // /theme with no args lists available themes.
        let pc = commands::parse("/theme").unwrap();
        let handled = dispatch_slash_command(
            &mut state,
            &pc,
            &mut settings,
            &store,
            &mut session,
            &mut compact_at,
        )
        .await
        .unwrap();
        assert!(handled);
        let listed = state
            .blocks
            .iter()
            .find_map(|b| match b {
                BlockKind::System(s) => Some(s.text().to_string()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(
            listed.contains("nord"),
            "list should mention nord: {listed}"
        );
        assert!(
            listed.contains("ravenwood"),
            "list should mention ravenwood: {listed}"
        );

        // /theme nord switches the active theme.
        let pc = commands::parse("/theme nord").unwrap();
        let handled = dispatch_slash_command(
            &mut state,
            &pc,
            &mut settings,
            &store,
            &mut session,
            &mut compact_at,
        )
        .await
        .unwrap();
        assert!(handled);
        assert_eq!(state.theme, Theme::NORD);

        // /theme unknown reports an error and leaves the theme unchanged.
        let pc = commands::parse("/theme nope").unwrap();
        let handled = dispatch_slash_command(
            &mut state,
            &pc,
            &mut settings,
            &store,
            &mut session,
            &mut compact_at,
        )
        .await
        .unwrap();
        assert!(handled);
        assert_eq!(
            state.theme,
            Theme::NORD,
            "unknown theme must not change theme"
        );
    }
}
