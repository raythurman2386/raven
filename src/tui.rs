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
        MouseEventKind,
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
use crate::session::SessionStore;

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

fn state_label(state: &AgentState, status: &str) -> (&'static str, Color) {
    match state {
        AgentState::Planning => ("planning", Theme::PLAN),
        AgentState::AwaitingApproval => ("awaiting approval", Theme::PLAN),
        AgentState::Executing => ("executing", Theme::ACCENT),
        _ if status.starts_with("tool:") => ("tool", Theme::TOOL),
        _ if status.starts_with("thinking") => ("thinking", Theme::DIM),
        _ => ("ready", Theme::USER),
    }
}

// ── Render log lines (only when dirty) ───────────────────────────────────

fn render_log_lines(log: &[LogEntry]) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(log.len().saturating_mul(2));
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
            }
            LogKind::Assistant => {
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
            }
            LogKind::Error => {
                lines.push(Line::from(Span::styled(
                    format!("✗ {}", e.text),
                    Style::default()
                        .fg(Theme::ERROR)
                        .add_modifier(Modifier::BOLD),
                )));
            }
        }
    }
    lines
}

// ── Main TUI ─────────────────────────────────────────────────────────────

pub async fn run_tui(settings: Settings) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let compact_at = ((settings.context_window - settings.context_window / 8) as f32
        * settings.compact_threshold) as usize;

    let app_name = "raven";

    let mut log: Vec<LogEntry> = vec![
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
            "enter submit · ctrl+n new · ctrl+p plan · ctrl+c quit · wheel/pgup scroll",
        ),
    ];

    let mut log_dirty = true;
    let mut cached_log_lines: Vec<Line<'static>> = Vec::new();
    // When only the last assistant line streams, patch it without full rebuild.
    let mut stream_patch = false;

    let mut cached_est_tokens: usize = 0;
    let mut messages_dirty = false;

    let mut input = String::new();
    let mut status = "ready".to_string();
    let mut plan_pending = false;
    let mut plan_preview: Vec<String> = Vec::new();
    let mut running = false;
    let mut plan_first = settings.plan_first;
    let mut assistant_text = String::new();
    let mut agent_state = AgentState::Idle;
    let mut scroll: u16 = 0;
    let mut auto_scroll = true;
    let mut quit = false;

    let store = SessionStore::for_workspace(&settings.workspace)?;
    let mut session = store.create(&settings.model)?;
    let mut session_messages: Vec<ChatMessage> = Vec::new();
    let mut task_handle: Option<tokio::task::JoinHandle<anyhow::Result<Vec<ChatMessage>>>> = None;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(128);

    loop {
        if log_dirty {
            cached_log_lines = render_log_lines(&log);
            log_dirty = false;
            stream_patch = false;
        } else if stream_patch {
            // Rebuild only from last assistant block — simplest correct approach:
            // full rebuild is still cheap for typical session sizes; flag kept for future.
            cached_log_lines = render_log_lines(&log);
            stream_patch = false;
        }

        if messages_dirty {
            cached_est_tokens = history_tokens(&session_messages);
            messages_dirty = false;
        }

        let est_tokens = cached_est_tokens;
        let pct = if settings.context_window > 0 {
            (est_tokens as f64 / settings.context_window as f64) * 100.0
        } else {
            0.0
        };
        let (state_txt, state_color) = state_label(&agent_state, &status);

        terminal.draw(|f| {
            draw_ui(
                f,
                app_name,
                &settings,
                &cached_log_lines,
                &input,
                plan_pending,
                &plan_preview,
                plan_first,
                est_tokens,
                pct,
                state_txt,
                state_color,
                scroll,
                auto_scroll,
            );
        })?;

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
                            plan_first = !plan_first;
                            log.push(LogEntry::system(format!(
                                "plan mode {}",
                                if plan_first { "on" } else { "off" }
                            )));
                            log_dirty = true;
                        }
                        KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            // Save current session, start a fresh one
                            let _ = store.save_all_messages(&session, &session_messages);
                            let _ = store.update_summary(&mut session, None);
                            session = store.create(&settings.model)?;
                            session_messages.clear();
                            log.clear();
                            log.push(LogEntry::system(format!(
                                "{app_name} · {} · {}",
                                settings.model, settings.base_url
                            )));
                            log.push(LogEntry::system(format!(
                                "workspace {}",
                                settings.workspace.display()
                            )));
                            log.push(LogEntry::system(String::new()));
                            log.push(LogEntry::system(
                                "new session · enter submit · ctrl+n new · ctrl+p plan · ctrl+c quit",
                            ));
                            log_dirty = true;
                            plan_preview.clear();
                            plan_pending = false;
                            running = false;
                            agent_state = AgentState::Idle;
                            status = "ready".into();
                            assistant_text.clear();
                            input.clear();
                            scroll = 0;
                            auto_scroll = true;
                        }
                        KeyCode::Up => {
                            scroll = scroll.saturating_add(1);
                            auto_scroll = false;
                        }
                        KeyCode::Down => {
                            scroll = scroll.saturating_sub(1);
                            if scroll == 0 {
                                auto_scroll = true;
                            }
                        }
                        KeyCode::PageUp => {
                            scroll = scroll.saturating_add(10);
                            auto_scroll = false;
                        }
                        KeyCode::PageDown => {
                            scroll = scroll.saturating_sub(10);
                            if scroll == 0 {
                                auto_scroll = true;
                            }
                        }
                        KeyCode::Char(c) => {
                            if !running {
                                input.push(c);
                            }
                        }
                        KeyCode::Backspace => {
                            if !running {
                                input.pop();
                            }
                        }
                        KeyCode::Enter => {
                            if running || input.trim().is_empty() {
                                continue;
                            }
                            let text = input.trim().to_string();
                            input.clear();
                            scroll = 0;
                            auto_scroll = true;

                            // Slash commands take precedence over plan-response
                            // and normal task submission.
                            if let Some(pc) = commands::parse(&text) {
                                dispatch_slash_command(
                                    &pc,
                                    &settings,
                                    &mut log,
                                    &mut log_dirty,
                                    &mut plan_first,
                                    &mut plan_pending,
                                    &mut plan_preview,
                                    &mut running,
                                    &mut agent_state,
                                    &mut status,
                                    &mut assistant_text,
                                    &mut session_messages,
                                    &store,
                                    &mut session,
                                    &mut quit,
                                )?;
                                continue;
                            }

                            if plan_pending {
                                handle_plan_response(
                                    &text,
                                    &settings,
                                    &mut log,
                                    &mut log_dirty,
                                    &mut plan_pending,
                                    &mut plan_preview,
                                    &mut running,
                                    &mut agent_state,
                                    &mut status,
                                    &mut assistant_text,
                                    &mut session_messages,
                                    &store,
                                    &session,
                                    &mut task_handle,
                                    tx.clone(),
                                )?;
                            } else {
                                start_task(
                                    &text,
                                    plan_first,
                                    &settings,
                                    &mut log,
                                    &mut log_dirty,
                                    &mut running,
                                    &mut agent_state,
                                    &mut status,
                                    &mut assistant_text,
                                    &mut session_messages,
                                    &store,
                                    &session,
                                    &mut task_handle,
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
                        scroll = scroll.saturating_add(3);
                        auto_scroll = false;
                    }
                    MouseEventKind::ScrollDown => {
                        scroll = scroll.saturating_sub(3);
                        if scroll == 0 {
                            auto_scroll = true;
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
                    assistant_text.push_str(&t);
                    if let Some(last) = log.last_mut() {
                        if matches!(last.kind, LogKind::Assistant) {
                            last.text.push_str(&t);
                            stream_patch = true;
                        } else {
                            log.push(LogEntry::assistant(t));
                            log_dirty = true;
                        }
                    } else {
                        log.push(LogEntry::assistant(t));
                        log_dirty = true;
                    }
                    if auto_scroll {
                        scroll = 0;
                    }
                }
                AgentEvent::ToolStart { name, args } => {
                    // Flush open assistant bubble before tool line
                    log.push(LogEntry::tool(format!("→ {name}({args})")));
                    log_dirty = true;
                    status = format!("tool: {name}");
                }
                AgentEvent::ToolEnd { name, preview } => {
                    let snip: String = preview.chars().take(180).collect();
                    log.push(LogEntry::tool(format!("  [{name}] {snip}")));
                    log_dirty = true;
                }
                AgentEvent::Iteration(n) => {
                    status = format!("thinking… (iter {n})");
                }
                AgentEvent::Compacted {
                    before_tokens,
                    after_tokens,
                } => {
                    log.push(LogEntry::system(format!(
                        "⟳ compacted ~{before_tokens} → ~{after_tokens} tokens"
                    )));
                    log_dirty = true;
                }
                AgentEvent::Retry { attempt, delay_ms } => {
                    log.push(LogEntry::system(format!(
                        "⟳ retry {attempt}/3 in {delay_ms}ms"
                    )));
                    log_dirty = true;
                }
                AgentEvent::PlanReady => {
                    // Model signalled the plan is complete. Await the planning
                    // task, save messages, show the plan, then auto-execute
                    // without a human approval prompt.
                    if let Some(handle) = task_handle.take() {
                        if let Ok(Ok(msgs)) = handle.await {
                            session_messages = msgs;
                            messages_dirty = true;
                            let _ = store.save_all_messages(&session, &session_messages);
                            let _ = store.update_summary(&mut session, None);
                        }
                    }

                    if plan_first && agent_state == AgentState::Planning {
                        let plan = plan::parse_plan(&assistant_text);
                        plan_preview = plan::format_plan(&plan)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        log.push(LogEntry::system(String::new()));
                        log.push(LogEntry::system("plan ready — auto-executing"));
                        log_dirty = true;

                        // Auto-approve: run the execution phase directly.
                        running = true;
                        agent_state = AgentState::Executing;
                        status = "executing plan…".into();
                        let exec_prompt =
                            "Plan approved. Execute it now using tools as needed.".to_string();
                        let _ = store.append_message(
                            &session,
                            &ChatMessage {
                                role: "user".into(),
                                content: Some(exec_prompt.clone()),
                                tool_calls: None,
                                tool_call_id: None,
                            },
                        );
                        assistant_text.clear();
                        let mut agent =
                            Agent::with_messages(settings.clone(), session_messages.clone())?;
                        let tx_exec = tx.clone();
                        task_handle = Some(tokio::spawn(async move {
                            agent.run(&exec_prompt, tx_exec).await?;
                            Ok(agent.messages)
                        }));
                        plan_preview.clear();
                    } else {
                        plan_preview.clear();
                        status = "ready".into();
                        agent_state = AgentState::Idle;
                        running = false;
                        assistant_text.clear();
                    }
                }
                AgentEvent::Done => {
                    if let Some(handle) = task_handle.take() {
                        if let Ok(Ok(msgs)) = handle.await {
                            session_messages = msgs;
                            messages_dirty = true;
                            let _ = store.save_all_messages(&session, &session_messages);
                            let _ = store.update_summary(&mut session, None);
                        }
                    }

                    if plan_first && agent_state == AgentState::Planning {
                        let plan = plan::parse_plan(&assistant_text);
                        plan_preview = plan::format_plan(&plan)
                            .lines()
                            .map(|s| s.to_string())
                            .collect();
                        log.push(LogEntry::system(String::new()));
                        log.push(LogEntry::system("plan ready — approve or revise below"));
                        log_dirty = true;
                        plan_pending = true;
                        agent_state = AgentState::AwaitingApproval;
                        status = "awaiting plan approval".into();
                    } else {
                        plan_preview.clear();
                        status = "ready".into();
                        agent_state = AgentState::Idle;
                    }
                    running = false;
                    assistant_text.clear();
                }
                AgentEvent::Error(e) => {
                    log.push(LogEntry::error(e));
                    log_dirty = true;
                    plan_preview.clear();
                    status = "ready".into();
                    agent_state = AgentState::Idle;
                    running = false;
                    assistant_text.clear();
                }
            }
        }

        if quit {
            break;
        }
    }

    let _ = store.save_all_messages(&session, &session_messages);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    Ok(())
}

// ── Drawing ──────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn draw_ui(
    f: &mut Frame,
    app_name: &str,
    settings: &Settings,
    log_lines: &[Line<'static>],
    input: &str,
    plan_pending: bool,
    plan_preview: &[String],
    plan_first: bool,
    est_tokens: usize,
    pct: f64,
    state_txt: &str,
    state_color: Color,
    scroll: u16,
    _auto_scroll: bool,
) {
    let show_plan = plan_pending && !plan_preview.is_empty();
    let plan_h = if show_plan {
        (plan_preview.len().saturating_add(2) as u16).clamp(3, 10)
    } else {
        0
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // top chrome
            Constraint::Min(5),    // log
            Constraint::Length(plan_h),
            Constraint::Length(1), // status
            Constraint::Length(3), // input
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
                fmt_tokens(est_tokens as u64),
                fmt_tokens(settings.context_window as u64),
                pct
            ),
            Style::default().fg(usage_color(pct)),
        ),
        Span::styled("  ·  ", Style::default().fg(Theme::DIM)),
        Span::styled(
            if plan_first { "plan:on" } else { "plan:off" },
            Style::default().fg(if plan_first { Theme::PLAN } else { Theme::DIM }),
        ),
    ]);
    f.render_widget(Paragraph::new(top), chunks[0]);

    // Log
    let log_h = chunks[1].height.saturating_sub(2) as usize;
    let max_scroll = log_lines.len().saturating_sub(log_h);
    let scroll_eff = (scroll as usize).min(max_scroll);
    let offset = max_scroll.saturating_sub(scroll_eff) as u16;

    let log_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(Style::default().fg(Theme::BORDER))
        .padding(Padding::horizontal(1));
    let log_widget = Paragraph::new(log_lines.to_vec())
        .block(log_block)
        .wrap(Wrap { trim: false })
        .scroll((offset, 0));
    f.render_widget(log_widget, chunks[1]);

    // Plan panel (Grok Build plan viewer lite)
    if show_plan && plan_h > 0 {
        let mut lines: Vec<Line> = plan_preview
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
    let status_line = Line::from(vec![
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
    ]);
    f.render_widget(
        Paragraph::new(status_line).style(Style::default().bg(Theme::STATUS_BG)),
        chunks[3],
    );

    // Input — prompt glyph like a real agent CLI
    let title = if plan_pending {
        " approve / revise "
    } else {
        " task "
    };
    let prompt = if plan_pending { "? " } else { "❯ " };
    let input_line = Line::from(vec![
        Span::styled(
            prompt,
            Style::default()
                .fg(if plan_pending {
                    Theme::PLAN
                } else {
                    Theme::ACCENT
                })
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(input.to_string(), Style::default().fg(Theme::FG)),
    ]);
    let input_w = Paragraph::new(input_line).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(if plan_pending {
                Theme::PLAN
            } else {
                Theme::BORDER
            }))
            .title(Span::styled(title, Style::default().fg(Theme::DIM))),
    );
    f.render_widget(input_w, chunks[4]);
}

// ── Task / plan helpers (keeps the event loop readable) ──────────────────

#[allow(clippy::too_many_arguments, clippy::ptr_arg)]
fn start_task(
    text: &str,
    plan_first: bool,
    settings: &Settings,
    log: &mut Vec<LogEntry>,
    log_dirty: &mut bool,
    running: &mut bool,
    agent_state: &mut AgentState,
    status: &mut String,
    assistant_text: &mut String,
    session_messages: &mut Vec<ChatMessage>,
    store: &SessionStore,
    session: &crate::session::Session,
    task_handle: &mut Option<tokio::task::JoinHandle<anyhow::Result<Vec<ChatMessage>>>>,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<()> {
    *running = true;
    *status = "running…".into();
    log.push(LogEntry::user(text.to_string()));
    *log_dirty = true;

    let mut prompt = text.to_string();
    if plan_first {
        prompt.push_str(
            "\n\nFirst propose a concise step-by-step plan. You may use read-only tools (list_dir, read_file, grep, search_code, git_status, git_diff, git_log) to inspect the workspace, but you CANNOT edit files or run shell until the plan is approved. Just list the numbered steps.",
        );
        *agent_state = AgentState::Planning;
    }
    assistant_text.clear();

    let user_msg = ChatMessage {
        role: "user".into(),
        content: Some(text.to_string()),
        tool_calls: None,
        tool_call_id: None,
    };
    let _ = store.append_message(session, &user_msg);

    let mut agent = Agent::with_messages(settings.clone(), session_messages.clone())?;
    if plan_first {
        agent = agent.plan_only();
    }
    *task_handle = Some(tokio::spawn(async move {
        agent.run(&prompt, tx).await?;
        Ok(agent.messages)
    }));
    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::ptr_arg)]
fn handle_plan_response(
    text: &str,
    settings: &Settings,
    log: &mut Vec<LogEntry>,
    log_dirty: &mut bool,
    plan_pending: &mut bool,
    plan_preview: &mut Vec<String>,
    running: &mut bool,
    agent_state: &mut AgentState,
    status: &mut String,
    assistant_text: &mut String,
    session_messages: &mut Vec<ChatMessage>,
    store: &SessionStore,
    session: &crate::session::Session,
    task_handle: &mut Option<tokio::task::JoinHandle<anyhow::Result<Vec<ChatMessage>>>>,
    tx: mpsc::Sender<AgentEvent>,
) -> Result<()> {
    let low = text.to_lowercase();
    let approve = matches!(
        low.as_str(),
        "yes" | "y" | "approve" | "go" | "execute" | "ok"
    );

    *plan_pending = false;
    plan_preview.clear();
    *running = true;
    log.push(LogEntry::user(text.to_string()));
    *log_dirty = true;

    let prompt = if approve {
        *agent_state = AgentState::Executing;
        *status = "executing plan…".into();
        "Plan approved. Execute it now using tools as needed.".to_string()
    } else {
        *agent_state = AgentState::Planning;
        *status = "revising plan…".into();
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

    assistant_text.clear();
    let mut agent = Agent::with_messages(settings.clone(), session_messages.clone())?;
    if !approve {
        // Revising the plan: keep the model on the read-only toolset.
        agent = agent.plan_only();
    }
    *task_handle = Some(tokio::spawn(async move {
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
#[allow(clippy::too_many_arguments, clippy::ptr_arg)]
fn dispatch_slash_command(
    pc: &commands::ParsedCommand,
    settings: &Settings,
    log: &mut Vec<LogEntry>,
    log_dirty: &mut bool,
    plan_first: &mut bool,
    plan_pending: &mut bool,
    plan_preview: &mut Vec<String>,
    running: &mut bool,
    agent_state: &mut AgentState,
    status: &mut String,
    assistant_text: &mut String,
    session_messages: &mut Vec<ChatMessage>,
    store: &SessionStore,
    session: &mut crate::session::Session,
    quit: &mut bool,
) -> Result<bool> {
    match pc.name.as_str() {
        "help" => {
            let text = if pc.args.is_empty() {
                commands::help_text()
            } else {
                commands::command_help(&pc.args)
                    .unwrap_or_else(|| format!("Unknown command: /{}", pc.args))
            };
            log.push(LogEntry::system(text));
            *log_dirty = true;
        }
        "plan" => {
            *plan_first = !*plan_first;
            log.push(LogEntry::system(format!(
                "plan mode {}",
                if *plan_first { "on" } else { "off" }
            )));
            *log_dirty = true;
        }
        "new" => {
            // Save the current session, then start a fresh one.
            let _ = store.save_all_messages(session, session_messages);
            let _ = store.update_summary(session, None);
            *session = store.create(&settings.model)?;
            session_messages.clear();
            log.clear();
            log.push(LogEntry::system(format!(
                "raven · {} · {}",
                settings.model, settings.base_url
            )));
            log.push(LogEntry::system(format!(
                "workspace {}",
                settings.workspace.display()
            )));
            log.push(LogEntry::system(String::new()));
            log.push(LogEntry::system(
                "new session · enter submit · /plan · /new · /help · /quit",
            ));
            *log_dirty = true;
            plan_preview.clear();
            *plan_pending = false;
            *running = false;
            *agent_state = AgentState::Idle;
            *status = "ready".into();
            assistant_text.clear();
        }
        "clear" => {
            log.clear();
            *log_dirty = true;
        }
        "quit" => {
            *quit = true;
        }
        _ => {
            // Unknown command — surface a helpful message.
            log.push(LogEntry::system(format!(
                "Unknown command: /{}  (try /help)",
                pc.name
            )));
            *log_dirty = true;
        }
    }
    Ok(true)
}
