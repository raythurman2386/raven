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
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, Event, KeyCode,
        KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    Command,
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Padding, Paragraph},
    Frame, Terminal,
};
use std::collections::HashMap;
use std::fmt;
use std::io::stdout;
use std::sync::{Mutex, OnceLock};
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, ChatMessage};
use crate::commands;
use crate::config::{Mode, Settings};
use crate::context::history_tokens;
use crate::plan::{self, AgentState};
use crate::session::{Session, SessionStore};

mod blocks;
mod completion;
mod dispatch;
mod markdown;
mod render;
mod selection;
mod status;
mod theme;

pub use theme::Theme;

use blocks::{AssistantBlock, BlockKind, ErrorBlock, SystemBlock, ToolBlock, UserBlock};
use completion::{apply as apply_completion, candidates_for, Completion};
use render::{
    message_to_block, prewrap_visible, render_assistant_lines, render_blocks, total_rows,
};
use selection::{
    apply_selection_highlight, copy_to_clipboard, selection_text, word_bounds, DisplayPos,
    Selection,
};
use status::{fmt_tokens, spinner_frame, state_label, usage_color, waiting_diamond};

static MODEL_COMPLETION_CACHE: OnceLock<Mutex<HashMap<String, Option<Vec<String>>>>> =
    OnceLock::new();

fn provider_model_candidates(provider: &crate::config::Provider) -> Vec<String> {
    let cache = MODEL_COMPLETION_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    let key = format!(
        "{}|{}|{}",
        provider.name,
        provider.base_url,
        provider.api_key.as_deref().unwrap_or_default()
    );

    if let Ok(cache) = cache.lock() {
        if let Some(cached) = cache.get(&key) {
            return cached.clone().unwrap_or_default();
        }
    }

    let fetched = fetch_live_provider_models(provider);
    let cached_value = if fetched.is_empty() {
        None
    } else {
        Some(fetched.clone())
    };
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, cached_value);
    }
    fetched
}

pub(crate) fn fetch_live_provider_models(provider: &crate::config::Provider) -> Vec<String> {
    let base = provider.base_url.trim_end_matches('/');
    let urls = if base.contains("/v1") {
        vec![
            format!("{}/models", base),
            format!("{}/api/tags", base.trim_end_matches("/v1")),
        ]
    } else {
        vec![format!("{}/models", base), format!("{}/api/tags", base)]
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return Vec::new(),
    };

    for url in urls {
        let mut req = client.get(&url);
        if let Some(key) = &provider.api_key {
            req = req.bearer_auth(key);
        }

        let Ok(resp) = req.send() else {
            continue;
        };
        if !resp.status().is_success() {
            continue;
        }

        let Ok(body) = resp.json::<serde_json::Value>() else {
            continue;
        };

        let mut names = Vec::new();
        if let Some(data) = body.get("data").and_then(serde_json::Value::as_array) {
            for entry in data {
                if let Some(id) = entry.get("id").and_then(serde_json::Value::as_str) {
                    names.push(id.to_string());
                } else if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) {
                    names.push(name.to_string());
                }
            }
        }
        if let Some(models) = body.get("models").and_then(serde_json::Value::as_array) {
            for entry in models {
                if let Some(name) = entry.get("name").and_then(serde_json::Value::as_str) {
                    names.push(name.to_string());
                }
            }
        }

        if !names.is_empty() {
            names.sort();
            names.dedup();
            return names;
        }
    }

    Vec::new()
}

/// Argument completion candidates for slash commands that accept a value.
pub fn completion_arg_candidates(
    settings: &Settings,
    config_file: &crate::config::ConfigFile,
    cmd: &str,
) -> Vec<String> {
    match cmd {
        "theme" => Theme::all().iter().map(|(n, _)| n.to_string()).collect(),
        "provider" => crate::config::known_provider_names(config_file),
        "model" => {
            let live_names = provider_model_candidates(&settings.provider);
            let mut preferred = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            let mut push_unique = |value: &str| {
                if !value.trim().is_empty() && seen.insert(value.to_string()) {
                    preferred.push(value.to_string());
                }
            };

            for value in [
                settings.model.as_str(),
                settings.provider.default_model.as_str(),
            ] {
                push_unique(value);
            }
            if let Some(cfg) = config_file.providers.get(&settings.provider.name) {
                if let Some(model) = &cfg.default_model {
                    push_unique(model);
                }
            }
            for name in live_names {
                push_unique(&name);
            }
            // Provider-aware curated fallback (e.g. opencode-go models) so
            // autocomplete always has candidates even before a live fetch.
            for candidate in crate::config::fallback_models(&settings.provider.name) {
                push_unique(&candidate);
            }
            // Provider-agnostic commonly-used models, kept as a final net.
            for candidate in [
                "qwen3.8:latest",
                "gemma4:latest",
                "deepseek-v4-flash:cloud",
                "deepseek-v4-pro:cloud",
                "glm-5.3-flash:cloud",
                "x-ai/grok-4.5",
                "x-ai/grok-4.6",
            ] {
                push_unique(candidate);
            }

            preferred
        }
        _ => Vec::new(),
    }
}

// ── TUI state ────────────────────────────────────────────────────────────

struct TuiState {
    blocks: Vec<BlockKind>,
    log_dirty: bool,
    cached_log_lines: Vec<Line<'static>>,
    /// Sum of per-line wrapped-row counts for `cached_log_lines`. Recomputed
    /// only when the log content changes, so virtualization stays O(viewport).
    log_total_rows: usize,
    /// Width (in columns) used to compute `log_total_rows`. Tracked so the
    /// total is recomputed on terminal resize.
    log_width: usize,
    /// Generation counter bumped whenever `cached_log_lines` changes. Used to
    /// invalidate the cached `log_total_rows` — unlike `log_dirty`, this is
    /// NOT cleared before `draw_ui` runs, so the cache stays correct.
    log_gen: u64,
    /// Generation seen when `log_total_rows` was last computed. Guards the
    /// cache in `refresh_log_rows`.
    last_rows_gen: u64,
    last_assistant_lines: usize,
    stream_patch: bool,
    cached_est_tokens: usize,
    messages_dirty: bool,
    /// Set when the input box / cursor / completion changed so an idle TUI
    /// still redraws (typing must not freeze the chatbox).
    input_dirty: bool,
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
    /// Max `scroll` for the current log viewport (`total_rows - viewport_h`).
    /// Kept in sync from `draw_ui` / `sync_log_max_scroll` so relative moves
    /// (wheel, PgDn) clamp correctly after Home (which used to set `u16::MAX`
    /// and trap the view at the top).
    log_max_scroll: u16,
    plan_scroll: u16,
    quit: bool,
    tick: u64,
    live_tool: Option<String>,
    turn_tool_count: usize,
    pending_question: Option<tokio::sync::oneshot::Sender<String>>,
    pending_question_text: Option<String>,
    session_messages: Vec<ChatMessage>,
    task_handle: Option<tokio::task::JoinHandle<anyhow::Result<Vec<ChatMessage>>>>,
    /// Receiver for the in-flight turn only. Replaced on every send so a
    /// leftover `Done` from an aborted turn cannot join the next handle.
    event_rx: Option<mpsc::Receiver<AgentEvent>>,
    selection: Option<Selection>,
    last_click: Option<(u64, DisplayPos)>,
    copy_status: Option<(u64, String)>,
    theme: Theme,
    /// Most-recently-submitted prompts, oldest first. Bounded to HISTORY_MAX.
    prompt_history: Vec<String>,
    /// Recall cursor into `prompt_history`; `== prompt_history.len()` means
    /// "live" (the empty baseline). Up decrements, Down increments.
    hist_idx: usize,
    /// (preload, prompt, read_only) of the last user-initiated turn, for
    /// `/retry`. Cleared on session reset.
    last_turn: Option<(Vec<ChatMessage>, String, bool)>,
}

impl TuiState {
    fn new(settings: &Settings, app_name: &str, compact_at: usize) -> Self {
        Self {
            blocks: vec![
                BlockKind::System(SystemBlock::new(format!(
                    "{app_name} · {} · {}",
                    settings.model,
                    settings.base_url()
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
                    "try: describe a task, e.g. \"add auth middleware\" · /help for commands"
                        .to_string(),
                )),
            ],
            log_dirty: true,
            cached_log_lines: Vec::new(),
            log_total_rows: 0,
            log_width: 0,
            log_gen: 0,
            last_rows_gen: 0,
            last_assistant_lines: 0,
            stream_patch: false,
            cached_est_tokens: 0,
            messages_dirty: false,
            input_dirty: false,
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
            log_max_scroll: 0,
            plan_scroll: 0,
            quit: false,
            tick: 0,
            live_tool: None,
            turn_tool_count: 0,
            pending_question: None,
            pending_question_text: None,
            session_messages: Vec::new(),
            task_handle: None,
            event_rx: None,
            selection: None,
            last_click: None,
            copy_status: None,
            theme: Theme::by_name(&settings.theme).unwrap_or_else(Theme::default_theme),
            prompt_history: Vec::new(),
            hist_idx: 0,
            last_turn: None,
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

    /// Recompute the cached total row count when the log content or viewport
    /// width changes. Call this after any log re-render or terminal resize.
    fn refresh_log_rows(&mut self, width: usize) {
        if self.log_gen == self.last_rows_gen && self.log_width == width {
            return;
        }
        self.last_rows_gen = self.log_gen;
        self.log_width = width;
        self.log_total_rows = total_rows(&self.cached_log_lines, width.max(1));
    }
}

// ── Terminal mode helpers ────────────────────────────────────────────────

/// Mouse tracking for clicks, drags, and wheel — without any-event mode
/// (`?1003`), which floods the queue with hover moves and can starve scroll
/// events when the loop only drains one event per tick.
struct EnableMouseCaptureLite;

impl Command for EnableMouseCaptureLite {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str(concat!(
            // Normal tracking: button press/release with coordinates
            "\x1b[?1000h",
            // Button-event tracking: drag motion while a button is held
            "\x1b[?1002h",
            // SGR encoding: coordinates beyond 223 and unambiguous parsing
            "\x1b[?1006h",
        ))
    }

    // Required on Windows by crossterm's Command trait. Prefer ANSI (default
    // `is_ansi_code_supported`) on modern consoles; legacy WinAPI has no
    // equivalent of these selective DECSET modes, so this is a no-op there.
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Disable xterm alternate-scroll (`?1007`). When enabled, the wheel is
/// translated to Up/Down *keys* on the alternate screen whenever the terminal
/// is not delivering mouse-wheel reports — which then hits prompt-history
/// recall instead of scrolling the log.
struct DisableAlternateScroll;

impl Command for DisableAlternateScroll {
    fn write_ansi(&self, f: &mut impl fmt::Write) -> fmt::Result {
        f.write_str("\x1b[?1007l")
    }

    // Alternate-scroll is an xterm/DEC private mode; legacy WinAPI consoles
    // have nothing to toggle. ANSI path handles Windows Terminal / VT hosts.
    #[cfg(windows)]
    fn execute_winapi(&self) -> std::io::Result<()> {
        Ok(())
    }
}

// ── Main TUI ─────────────────────────────────────────────────────────────

pub async fn run_tui(
    mut settings: Settings,
    config_file: crate::config::ConfigFile,
    resume_session: Option<Session>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCaptureLite,
        DisableAlternateScroll,
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
        state.log_dirty = true;
        s
    } else {
        store.create(&settings.model)?
    };

    // Argument-completion candidates per command. Slash commands with values
    // should complete from real, live choices: theme names, known providers,
    // and a useful set of model names for the current provider.
    let completion_settings = settings.clone();
    let completion_config = config_file.clone();
    let arg_candidates = |cmd: &str| -> Vec<String> {
        completion_arg_candidates(&completion_settings, &completion_config, cmd)
    };

    'ui: loop {
        if state.blocks.len() > MAX_LOG_ENTRIES {
            let drop = state.blocks.len() - MAX_LOG_ENTRIES;
            state.blocks.drain(..drop);
            state.log_dirty = true;
        }

        // Capture whether the log/stream changed this tick *before* the flags
        // are cleared below, so we can force an immediate draw (rather than
        // waiting for the DRAW_INTERVAL throttle).
        let dirty =
            state.log_dirty || state.stream_patch || state.messages_dirty || state.input_dirty;

        if state.log_dirty {
            let (rendered, tail) = render_blocks(&state.blocks, state.tick, state.theme);
            state.cached_log_lines = rendered;
            state.log_gen += 1;
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
            state.log_gen += 1;
            state.last_assistant_lines = new_tail_len;
            state.stream_patch = false;
        }

        if state.messages_dirty {
            state.cached_est_tokens = history_tokens(&state.session_messages);
            state.messages_dirty = false;
        }

        // Draw when dirty, when animating (running / live tool / ask_user /
        // copy toast), or on the throttled interval while a turn is in flight.
        // Idle with nothing to animate must NOT spin a full redraw every 40ms.
        let animating = state.running
            || state.live_tool.is_some()
            || state.pending_question.is_some()
            || state
                .copy_status
                .as_ref()
                .is_some_and(|(start, _)| state.tick.wrapping_sub(*start) < 50);
        let force_draw = dirty || animating;
        if force_draw || (state.running && last_draw.elapsed() >= DRAW_INTERVAL) {
            if animating {
                state.tick = state.tick.wrapping_add(1);
            }
            terminal.draw(|f| {
                draw_ui(f, app_name, &settings, &mut state);
            })?;
            state.input_dirty = false;
            last_draw = std::time::Instant::now();
        }

        // Input + mouse. Drain every pending event each tick so a flood of
        // motion/scroll reports cannot starve wheel or key handling (the old
        // one-event-per-poll path did exactly that under mouse tracking).
        if event::poll(std::time::Duration::from_millis(40))? {
            loop {
                match event::read()? {
                    Event::Key(key)
                        if key.kind == KeyEventKind::Press || key.kind == KeyEventKind::Repeat =>
                    {
                        match key.code {
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                break 'ui
                            }
                            KeyCode::BackTab => {
                                // Only when idle: completion prev, else mode cycle.
                                // Previously the else-branch ran even while a turn
                                // was in flight, flipping mode mid-run.
                                if !state.running && state.pending_question.is_none() {
                                    if let Some(comp) = state.completion.as_mut() {
                                        comp.prev();
                                        state.input_dirty = true;
                                    } else {
                                        let m = state.cycle_mode();
                                        state.push_system(format!("mode: {}", m.label()));
                                        state.log_dirty = true;
                                    }
                                }
                            }
                            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                reset_session(
                                    &mut state,
                                    &mut session,
                                    &store,
                                    &settings,
                                    app_name,
                                )?;
                            }
                            KeyCode::Up => {
                                if let Some(comp) = state.completion.as_mut() {
                                    // Completion popup open: move the highlight
                                    // backward (Up = previous candidate).
                                    comp.prev();
                                    state.input_dirty = true;
                                } else if history_recall_active(
                                    state.input.is_empty(),
                                    state.prompt_history.len(),
                                    state.hist_idx,
                                ) {
                                    // Empty input, or mid-recall (a recalled prompt
                                    // still in the box): walk backward through the
                                    // prompt history. Gating on hist_idx rather than
                                    // only on empty input lets Up recall older
                                    // prompts repeatedly instead of returning a
                                    // single entry then scrolling. At the oldest
                                    // entry (or with empty history) fall through to
                                    // log scroll instead of a silent no-op.
                                    if let Some((recalled, idx)) =
                                        history_recall_up(&state.prompt_history, state.hist_idx)
                                    {
                                        state.input = recalled;
                                        state.cursor = state.input.len();
                                        state.hist_idx = idx;
                                        state.input_dirty = true;
                                    } else {
                                        scroll_log_by(&mut state, 1);
                                    }
                                } else {
                                    scroll_log_by(&mut state, 1);
                                }
                            }
                            KeyCode::Down => {
                                if let Some(comp) = state.completion.as_mut() {
                                    // Completion popup open: move the highlight
                                    // forward (Down = next candidate).
                                    comp.next();
                                    state.input_dirty = true;
                                } else if history_recall_active(
                                    state.input.is_empty(),
                                    state.prompt_history.len(),
                                    state.hist_idx,
                                ) {
                                    // Empty input or mid-recall: walk forward
                                    // toward the empty baseline (or stay empty at
                                    // the live position). At the live baseline,
                                    // fall through to log scroll.
                                    if let Some((recalled, idx)) =
                                        history_recall_down(&state.prompt_history, state.hist_idx)
                                    {
                                        state.input = recalled;
                                        state.cursor = state.input.len();
                                        state.hist_idx = idx;
                                        state.input_dirty = true;
                                    } else {
                                        scroll_log_by(&mut state, -1);
                                    }
                                } else {
                                    scroll_log_by(&mut state, -1);
                                }
                            }
                            KeyCode::PageUp => {
                                scroll_log_by(&mut state, 10);
                            }
                            KeyCode::PageDown => {
                                scroll_log_by(&mut state, -10);
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
                                        state.input_dirty = true;
                                    }
                                }
                            }
                            KeyCode::Right => {
                                if !state.running || state.pending_question.is_some() {
                                    // Move right by one char (byte-safe). Advance
                                    // past the char at the cursor, including the
                                    // last char so the cursor can reach the true
                                    // end of the line. (char_indices().nth(1)
                                    // returns None when only one char remains,
                                    // stranding the cursor before the last char.)
                                    if let Some(c) = state.input[state.cursor..].chars().next() {
                                        state.cursor += c.len_utf8();
                                        state.input_dirty = true;
                                    }
                                }
                            }
                            KeyCode::Home => {
                                if state.input.is_empty() {
                                    // Empty input: Home jumps to the top of the
                                    // transcript (detaches from the live tail).
                                    // Use the real max offset — not u16::MAX —
                                    // so wheel/PgDn can move back toward live.
                                    let size: Rect = terminal.size().unwrap_or_default().into();
                                    sync_log_max_scroll(&mut state, size);
                                    state.scroll = state.log_max_scroll;
                                    state.auto_scroll = false;
                                    state.input_dirty = true;
                                } else if !state.running || state.pending_question.is_some() {
                                    state.cursor = 0;
                                    state.input_dirty = true;
                                }
                            }
                            KeyCode::End => {
                                if state.input.is_empty() {
                                    // Empty input: End jumps back to the live tail
                                    // (reattaches auto-follow without scrolling all
                                    // the way back down).
                                    state.scroll = 0;
                                    state.auto_scroll = true;
                                    state.input_dirty = true;
                                } else if !state.running || state.pending_question.is_some() {
                                    state.cursor = state.input.len();
                                    state.input_dirty = true;
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
                                            state.input_dirty = true;
                                            state.completion = None;
                                        } else {
                                            comp.next();
                                            state.input_dirty = true;
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
                                    state.input_dirty = true;
                                }
                                state.hist_idx = state.prompt_history.len();
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
                                        state.input_dirty = true;
                                    }
                                }
                                state.completion = candidates_for(&state.input, &arg_candidates);
                            }
                            KeyCode::Enter => {
                                // Enter accepts the highlighted completion when the
                                // popup is open (so `/th` + Enter → `/theme`).
                                if let Some(comp) = state.completion.take() {
                                    if let Some(candidate) = comp.candidates.get(comp.selected) {
                                        state.input.replace_range(
                                            comp.replace_start..comp.replace_end,
                                            candidate,
                                        );
                                        state.cursor = state.input.len();
                                        state.input_dirty = true;
                                    }
                                    continue;
                                }
                                if state.input.trim().is_empty() {
                                    continue;
                                }
                                let text = state.input.trim().to_string();
                                state.input.clear();
                                state.cursor = 0;
                                state.input_dirty = true;
                                state.completion = None;
                                state.scroll = 0;
                                state.auto_scroll = true;
                                state.turn_tool_count = 0;
                                state.live_tool = None;

                                if let Some(pc) = commands::parse(&text) {
                                    dispatch::dispatch_slash_command(
                                        &mut state,
                                        &pc,
                                        &mut settings,
                                        &store,
                                        &mut session,
                                        &mut compact_at,
                                        &config_file,
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
                                    abort_current_turn(&mut state);
                                    state.push_system("⏸ interrupted — redirecting…");
                                    state.log_dirty = true;
                                    start_task(&mut state, &text, &settings, &store, &session)?;
                                    continue;
                                }

                                // Record the prompt for Up/Down recall. Only real
                                // task prompts / plan revisions land here — slash
                                // commands and ask_user answers `continue` earlier.
                                if !text.is_empty() {
                                    state.prompt_history.push(text.clone());
                                    if state.prompt_history.len() > MAX_HISTORY {
                                        state.prompt_history.remove(0);
                                    }
                                }
                                state.hist_idx = state.prompt_history.len();
                                if state.plan_pending {
                                    handle_plan_response(
                                        &mut state, &text, &settings, &store, &session,
                                    )?;
                                } else {
                                    start_task(&mut state, &text, &settings, &store, &session)?;
                                }
                            }
                            KeyCode::Esc => {
                                // Layered dismiss: completion → selection → ask_user → quit.
                                if state.completion.take().is_some()
                                    || state.selection.take().is_some()
                                {
                                    state.input_dirty = true;
                                } else if state.pending_question.take().is_some() {
                                    state.pending_question_text = None;
                                    state.status = if state.running {
                                        "running".into()
                                    } else {
                                        "ready".into()
                                    };
                                    state.push_system("question dismissed");
                                    state.log_dirty = true;
                                } else {
                                    break 'ui;
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Paste(text) => {
                        // Bracketed paste: insert at the cursor (not always at end).
                        // Without this handler, a large paste arrives as a rapid
                        // stream of Char events that get dropped by the poll loop,
                        // and any newline in the pasted text is treated as Enter.
                        if !state.running || state.pending_question.is_some() {
                            let remaining =
                                MAX_INPUT_CHARS.saturating_sub(state.input.chars().count());
                            let pasted: String = text
                                .chars()
                                .filter(|c| *c != '\r')
                                .take(remaining)
                                .collect();
                            if !pasted.is_empty() {
                                let at = state.cursor.min(state.input.len());
                                state.input.insert_str(at, &pasted);
                                state.cursor = at + pasted.len();
                                state.input_dirty = true;
                                state.hist_idx = state.prompt_history.len();
                                state.completion = candidates_for(&state.input, &arg_candidates);
                            }
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
                if !event::poll(std::time::Duration::from_millis(0))? {
                    break;
                }
            }
        }

        // Agent events for the current turn only. Drain into a vec first so
        // the match can mutate `state` (including replacing `event_rx`).
        let mut turn_events = Vec::new();
        if let Some(rx) = state.event_rx.as_mut() {
            while let Ok(ev) = rx.try_recv() {
                turn_events.push(ev);
            }
        }
        for ev in turn_events {
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
                    if name == "run_shell" {
                        if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                            let snippet: String = cmd.chars().take(500).collect();
                            let _ = store.log_event(&session, "shell", &snippet);
                        }
                    }
                    let args_str = format_tool_args(&args);
                    let snip: String = args_str.chars().take(60).collect();
                    state.live_tool = Some(format!("⇢ {name}({snip})"));
                    state.turn_tool_count += 1;
                    state.status = "running".into();
                    // Named active block so ToolEnd can match under parallelism.
                    let tb = ToolBlock::start(name.clone(), format!("⇢ {name}({snip})"));
                    state.blocks.push(BlockKind::Tool(tb));
                    state.log_dirty = true;
                }
                AgentEvent::ToolEnd { name, preview } => {
                    state.status = "running".into();
                    // Deactivate the matching active tool (not always last).
                    deactivate_tool(&mut state.blocks, &name, &preview, state.tick);
                    // Keep live_tool pointing at another still-active tool if any.
                    state.live_tool = state.blocks.iter().rev().find_map(|b| match b {
                        BlockKind::Tool(tb) if tb.active => Some(tb.text().to_string()),
                        _ => None,
                    });
                    state.log_dirty = true;
                }
                AgentEvent::Iteration(n) => {
                    state.status = format!("thinking… (iter {n})");
                }
                AgentEvent::Compacted {
                    before_tokens,
                    after_tokens,
                    note,
                } => {
                    let line = if note.is_empty() {
                        format!("⟳ compacted ~{before_tokens} → ~{after_tokens} tokens")
                    } else {
                        format!("⟳ compacted ~{before_tokens} → ~{after_tokens} tokens — {note}")
                    };
                    state.push_system(line);
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
                AgentEvent::RecoveryPatch { path, reason } => {
                    state.push_system(format!(
                        "⚠ recovery patch → {path} ({reason})  ·  git apply {path}"
                    ));
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
                    // Drop any unanswered ask_user channel (agent is finished).
                    state.pending_question = None;
                    state.pending_question_text = None;
                    if let Some(handle) = state.task_handle.take() {
                        // Prefer try_join-style: the agent task should already
                        // be finished when Done is emitted; await is then cheap.
                        // Never persist an empty construction-failure result
                        // over an existing session.
                        if let Ok(Ok(msgs)) = handle.await {
                            if !msgs.is_empty() || state.session_messages.is_empty() {
                                state.session_messages = msgs;
                                state.messages_dirty = true;
                                let _ = store.save_all_messages(&session, &state.session_messages);
                                let _ = store.update_summary(&mut session, None);
                                if store.snapshot_patch(&session).unwrap_or(false) {
                                    state.push_system(format!(
                                        "diff snapshot → .raven/sessions/{}/last.patch",
                                        session.summary.id
                                    ));
                                }
                            }
                        }
                    }
                    state.event_rx = None;

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
                    state.pending_question = None;
                    state.pending_question_text = None;
                    abort_current_turn(&mut state);
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
            break 'ui;
        }
    }

    let _ = store.save_all_messages(&session, &state.session_messages);
    let _ = store.snapshot_patch(&session);
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

/// Apply Up-arrow recall: move `hist_idx` back one and return the recalled
/// prompt (or `None` if already at the oldest / empty history).
fn history_recall_up(prompt_history: &[String], hist_idx: usize) -> Option<(String, usize)> {
    if hist_idx == 0 || prompt_history.is_empty() {
        return None;
    }
    let idx = hist_idx - 1;
    Some((prompt_history[idx].clone(), idx))
}

/// Apply Down-arrow recall: move `hist_idx` forward one. Returns
/// `Some((input, idx))` on success, or `Some((String::new(), len))` when
/// moving onto the live baseline. Returns `None` if already at baseline.
fn history_recall_down(prompt_history: &[String], hist_idx: usize) -> Option<(String, usize)> {
    if hist_idx >= prompt_history.len() {
        return None;
    }
    let idx = hist_idx + 1;
    if idx == prompt_history.len() {
        Some((String::new(), idx))
    } else {
        Some((prompt_history[idx].clone(), idx))
    }
}

/// Whether Up/Down should walk the prompt history rather than scroll the
/// transcript. True when the input is empty (fresh recall) or when the user
/// is already mid-recall (`hist_idx < len`, i.e. a recalled prompt is still
/// in the box and has not been reset by typing). Once `hist_idx` has been
/// reset to the live position by typing, Up/Down scroll again.
fn history_recall_active(input_is_empty: bool, prompt_history_len: usize, hist_idx: usize) -> bool {
    input_is_empty || hist_idx < prompt_history_len
}

/// Recompute [`TuiState::log_max_scroll`] from the current terminal layout so
/// Home / relative scroll agree with what `prewrap_visible` will render.
fn sync_log_max_scroll(state: &mut TuiState, size: Rect) {
    let chunks = compute_layout(size, state);
    let log_rect = chunks[1];
    let content_width = (log_rect.width.saturating_sub(4) as usize).max(1);
    let log_h = log_rect.height.saturating_sub(1) as usize;
    state.refresh_log_rows(content_width);
    state.log_max_scroll = state
        .log_total_rows
        .saturating_sub(log_h)
        .min(usize::from(u16::MAX)) as u16;
}

/// Adjust log scroll by `delta` rows (positive = toward older content / up).
/// Detaches auto-follow when moving up; reattaches when the offset returns to
/// the live tail (`scroll == 0`).
///
/// Clamps through [`TuiState::log_max_scroll`] first so a prior Home jump (or
/// any overshoot) does not trap relative wheel/PgDn moves at the top.
fn scroll_log_by(state: &mut TuiState, delta: i32) {
    let max = state.log_max_scroll;
    let cur = state.scroll.min(max);
    if delta >= 0 {
        state.scroll = cur.saturating_add(delta as u16).min(max);
        state.auto_scroll = false;
    } else {
        state.scroll = cur.saturating_sub((-delta) as u16);
        if state.scroll == 0 {
            state.auto_scroll = true;
        }
    }
    state.input_dirty = true;
}

/// Compact `key=value` formatting of a tool-call arg object, so a tool block
/// reads `read_file path=src/main.rs line=1-40` instead of raw JSON braces.
/// Long string values are truncated to `ARG_VALUE_MAX` chars. Nested
/// objects/arrays render as a short `{..}`/`[..]` token to avoid wasting the
/// budget on structure.
fn format_tool_args(args: &serde_json::Value) -> String {
    const ARG_VALUE_MAX: usize = 40;
    let Some(obj) = args.as_object() else {
        // Non-object args (rare): render compact, truncated.
        let s = args.to_string();
        return s.chars().take(ARG_VALUE_MAX).collect();
    };
    if obj.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut remaining = 60usize;
    for (k, v) in obj {
        let val = match v {
            serde_json::Value::String(s) => {
                let t: String = s.chars().take(ARG_VALUE_MAX).collect();
                if s.chars().count() > ARG_VALUE_MAX {
                    format!("{t}…")
                } else {
                    t
                }
            }
            serde_json::Value::Bool(b) => b.to_string(),
            serde_json::Value::Number(n) => n.to_string(),
            serde_json::Value::Array(_) => "[..]".to_string(),
            serde_json::Value::Object(_) => "{..}".to_string(),
            serde_json::Value::Null => "null".to_string(),
        };
        // Skip empty-string args (they add no signal).
        if val.is_empty() {
            continue;
        }
        let piece = format!("{k}={val}");
        if remaining == 0 {
            parts.push("…".to_string());
            break;
        }
        let used = piece.chars().count();
        if used >= remaining && !parts.is_empty() {
            parts.push("…".to_string());
            break;
        }
        remaining = remaining.saturating_sub(used);
        parts.push(piece);
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join(" ")
    }
}

/// Context-sensitive keyhint footer text, shown in the bottom row.
fn keyhint(state: &TuiState) -> String {
    if state.pending_question_text.is_some() {
        "enter answer · esc dismiss".to_string()
    } else if state.plan_pending {
        "yes approve · type revise · esc dismiss".to_string()
    } else if state.running {
        "enter interrupt · ctrl+c quit".to_string()
    } else {
        "enter send · /help · /model · /new · shift+tab mode · ctrl+c quit · up/down recall · wheel/pgup scroll · home/end jump · ↑/↓ completion when popup open".to_string()
    }
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
            Constraint::Length(1),
        ])
        .split(area)
        .to_vec()
}

fn draw_ui(f: &mut Frame, app_name: &str, settings: &Settings, state: &mut TuiState) {
    let theme = state.theme;
    let pct = if settings.context_window > 0 {
        (state.cached_est_tokens as f64 / settings.context_window as f64) * 100.0
    } else {
        0.0
    };
    let (state_txt, state_color) = state_label(
        &state.agent_state,
        &state.status,
        state.running,
        state.theme,
    );

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
            settings.provider.name.clone(),
            Style::default().fg(theme.dim),
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
        Span::styled(
            state.mode.label(),
            Style::default().fg(match state.mode {
                Mode::Plan => theme.plan,
                Mode::Agent => theme.accent,
                Mode::Chat => theme.user,
            }),
        ),
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
    state.refresh_log_rows(content_width.max(1));
    state.log_max_scroll = state
        .log_total_rows
        .saturating_sub(log_h)
        .min(usize::from(u16::MAX)) as u16;
    // Heal overshoot (legacy Home sentinel, PageUp past the end, resize).
    if state.scroll > state.log_max_scroll {
        state.scroll = state.log_max_scroll;
    }
    let (display_lines, _offset) = prewrap_visible(
        &state.cached_log_lines,
        state.log_total_rows,
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
        if state.plan_pending || state.agent_state == AgentState::AwaitingApproval {
            lines.push(Line::from(Span::styled(
                "yes to execute · or type revisions",
                Style::default().fg(theme.dim),
            )));
        }
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
        // Window the candidates around `selected` so the highlighted entry is
        // always visible even when there are more candidates than rows.
        const MAX_ROWS: usize = 6;
        let total = comp.candidates.len();
        let start = if total <= MAX_ROWS {
            0
        } else {
            comp.selected
                .saturating_sub(MAX_ROWS / 2)
                .min(total.saturating_sub(MAX_ROWS))
        };
        let end = (start + MAX_ROWS).min(total);
        let lines: Vec<Line> = comp.candidates[start..end]
            .iter()
            .enumerate()
            .map(|(i, cand)| {
                let idx = start + i;
                let style = if idx == comp.selected {
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
    let prompt_style = Style::default()
        .fg(
            if state.pending_question_text.is_some() || state.plan_pending {
                theme.plan
            } else {
                theme.accent
            },
        )
        .add_modifier(Modifier::BOLD);
    let input_style = Style::default().fg(theme.fg);
    let content_width = chunks[5].width.saturating_sub(2).max(1) as usize;
    let wrapped = wrap_display_lines(&format!("{}{}", prompt, state.input), content_width);
    let input_lines: Vec<Line> = wrapped
        .into_iter()
        .enumerate()
        .map(|(i, s)| {
            if i == 0 && s.starts_with(prompt) {
                Line::from(vec![
                    Span::styled(prompt.to_string(), prompt_style),
                    Span::styled(s[prompt.len()..].to_string(), input_style),
                ])
            } else {
                Line::from(Span::styled(s, input_style))
            }
        })
        .collect();
    let input_w = Paragraph::new(input_lines).block(
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

    // Context-sensitive keyhint footer.
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            keyhint(state),
            Style::default().fg(theme.dim),
        ))),
        chunks[6],
    );
}

/// Word-wrap `s` to `width` display cells (`trim: false`).
///
/// Matches ratatui `Paragraph::wrap(Wrap { trim: false })`: break on
/// whitespace when a word would overflow, otherwise hard-break. Used for
/// both the painted input lines and the caret so they cannot drift.
fn wrap_display_lines(s: &str, width: usize) -> Vec<String> {
    let w = width.max(1);
    let mut out = Vec::new();
    for para in s.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0usize;
        for token in wrap_tokens(para) {
            append_wrapped_token(&mut out, &mut line, &mut line_w, token, w);
        }
        out.push(line);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn wrap_tokens(s: &str) -> Vec<&str> {
    use unicode_segmentation::UnicodeSegmentation;

    let mut out = Vec::new();
    let mut start = 0usize;
    let mut prev_ws: Option<bool> = None;
    for (i, g) in s.grapheme_indices(true) {
        let ws = g.chars().all(char::is_whitespace);
        if let Some(prev) = prev_ws {
            if prev != ws {
                out.push(&s[start..i]);
                start = i;
            }
        } else {
            start = i;
        }
        prev_ws = Some(ws);
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn append_wrapped_token(
    out: &mut Vec<String>,
    line: &mut String,
    line_w: &mut usize,
    token: &str,
    width: usize,
) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;

    let tw = token.width();
    if *line_w > 0 && *line_w + tw > width {
        out.push(std::mem::take(line));
        *line_w = 0;
    }
    if tw <= width {
        line.push_str(token);
        *line_w += tw;
        return;
    }
    for g in token.graphemes(true) {
        let gw = g.width();
        if *line_w > 0 && *line_w + gw > width {
            out.push(std::mem::take(line));
            *line_w = 0;
        }
        line.push_str(g);
        *line_w += gw;
    }
}

/// Number of wrapped lines a string occupies at the given display width.
fn wrapped_line_count(s: &str, width: usize) -> usize {
    wrap_display_lines(s, width).len()
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
    use unicode_width::UnicodeWidthStr;

    let content_width = input_rect.width.saturating_sub(2).max(1) as usize;
    let prefix = format!("{}{}", prompt, &input[..cursor.min(input.len())]);
    let lines = wrap_display_lines(&prefix, content_width);
    let mut row = lines.len().saturating_sub(1);
    let mut col = lines.last().map(|l| l.width()).unwrap_or(0);
    if col >= content_width {
        row += 1;
        col = 0;
    }

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

const MAX_HISTORY: usize = 100;

/// The effective content width (in display cells) of the input box for a
/// given terminal width.
///
/// The input box has `Borders::ALL` (2 border cols) plus a prompt glyph
/// (e.g. `❯ `, 2 display cells). Both the wrap-width computation and the
/// cursor position must use this same value, or the cursor will land in the
/// wrong place once the input wraps to a second line.
fn input_content_width(term_width: u16) -> usize {
    let avail = term_width.saturating_sub(2).max(1) as usize; // minus 2 border cols
    avail.saturating_sub(2).max(1) // minus prompt glyph "❯ "
}

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
fn current_display(state: &mut TuiState, log_rect: Rect) -> (Vec<Line<'static>>, u16) {
    let content_width = (log_rect.width.saturating_sub(4)) as usize;
    // Match `draw_ui`: the log block has LEFT|RIGHT|BOTTOM borders (no top),
    // so only the bottom border consumes a content row.
    let log_h = log_rect.height.saturating_sub(1) as usize;
    state.refresh_log_rows(content_width.max(1));
    prewrap_visible(
        &state.cached_log_lines,
        state.log_total_rows,
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
            // the log. Wheel must never touch prompt-history recall — that is
            // keyboard-only (Up/Down on the input).
            let chunks = compute_layout(size, state);
            if show_plan(state) && m.row >= chunks[2].top() && m.row < chunks[2].bottom() {
                state.plan_scroll = state.plan_scroll.saturating_add(1);
                state.input_dirty = true;
            } else {
                scroll_log_by(state, 3);
            }
        }
        MouseEventKind::ScrollDown => {
            let chunks = compute_layout(size, state);
            if show_plan(state) && m.row >= chunks[2].top() && m.row < chunks[2].bottom() {
                state.plan_scroll = state.plan_scroll.saturating_sub(1);
                state.input_dirty = true;
            } else {
                scroll_log_by(state, -3);
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
                if state.task_handle.is_some() {
                    abort_current_turn(state);
                    let _ = store.save_all_messages(session, &state.session_messages);
                    let _ = store.update_summary(session, None);
                    state.push_system("⏹ stopped (click)");
                    state.log_dirty = true;
                }
                state.pending_question = None;
                state.pending_question_text = None;
                state.running = false;
                state.agent_state = AgentState::Idle;
                state.status = "ready".into();
                state.assistant_text.clear();
                state.live_tool = None;
                state.turn_tool_count = 0;
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
                state.input_dirty = true;
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
                    state.input_dirty = true;
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
                state.input_dirty = true;
            }
        }
        _ => {}
    }
}

// ── Task / plan helpers (keeps the event loop readable) ──────────────────

/// Deactivate the tool block matching `name` (the most recent active one).
///
/// Parallel tools can finish out of order, so we must not always clear
/// `blocks.last()`. Falls back to the last tool block when no active block
/// matches (e.g. a tool that started before the TUI attached).
fn deactivate_tool(blocks: &mut [BlockKind], name: &str, preview: &str, tick: u64) {
    for b in blocks.iter_mut().rev() {
        if let BlockKind::Tool(tb) = b {
            if tb.active && (tb.name == name || tb.name.is_empty()) {
                tb.active = false;
                tb.end_tick = Some(tick);
                tb.set_preview(preview);
                return;
            }
        }
    }
    // Fallback: last tool block.
    if let Some(BlockKind::Tool(tb)) = blocks.last_mut() {
        tb.active = false;
        tb.end_tick = Some(tick);
        tb.set_preview(preview);
    }
}

/// Abort any in-flight agent task and reset the UI + session to a fresh state.
///
/// Shared by Ctrl+N and `/new` so both paths abort the running task, drop the
/// ask_user oneshot, and clear live tool/plan/assistant state — otherwise a
/// stale agent could keep delivering events into the new session.
fn reset_session(
    state: &mut TuiState,
    session: &mut Session,
    store: &SessionStore,
    settings: &Settings,
    app_name: &str,
) -> Result<()> {
    abort_current_turn(state);
    let _ = store.save_all_messages(session, &state.session_messages);
    let _ = store.update_summary(session, None);
    *session = store.create(&settings.model)?;
    state.session_messages.clear();
    state.blocks.clear();
    state.pending_question = None;
    state.pending_question_text = None;
    state.push_system(format!(
        "{app_name} · {} · {}",
        settings.model,
        settings.base_url()
    ));
    state.push_system(format!("workspace {}", settings.workspace.display()));
    state.push_system(String::new());
    state.log_dirty = true;
    state.plan_preview.clear();
    state.plan_pending = false;
    state.active_plan = None;
    state.running = false;
    state.agent_state = AgentState::Idle;
    state.status = "ready".into();
    state.assistant_text.clear();
    state.input.clear();
    state.cursor = 0;
    state.completion = None;
    state.selection = None;
    state.live_tool = None;
    state.turn_tool_count = 0;
    state.last_turn = None;
    state.scroll = 0;
    state.auto_scroll = true;
    Ok(())
}

fn start_task(
    state: &mut TuiState,
    text: &str,
    settings: &Settings,
    store: &SessionStore,
    session: &crate::session::Session,
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
    // Keep the user line in TUI history so /stop does not wipe it, but
    // preload the agent without it — `Agent::run` appends `prompt` itself.
    let preload = state.session_messages.clone();
    state.session_messages.push(user_msg);
    state.messages_dirty = true;

    // Construct the agent off the TUI thread. `Agent::new` still does a
    // workspace walk + sandboxed git; a parent folder of many repos used
    // to freeze Enter→paint. The walk is now cached/capped, and this
    // construction must not block the next frame.
    let read_only = state.mode.read_only();
    // Remember this turn so `/retry` can re-fire it after a failure.
    state.last_turn = Some((preload.clone(), prompt.clone(), read_only));
    begin_agent_turn(state, settings.clone(), preload, prompt, move |agent| {
        if read_only {
            agent.plan_only()
        } else {
            agent
        }
    });
    Ok(())
}

fn handle_plan_response(
    state: &mut TuiState,
    text: &str,
    settings: &Settings,
    store: &SessionStore,
    session: &crate::session::Session,
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

    let user_msg = ChatMessage {
        role: "user".into(),
        content: Some(prompt.clone()),
        tool_calls: None,
        tool_call_id: None,
    };
    let _ = store.append_message(session, &user_msg);
    let preload = state.session_messages.clone();
    state.session_messages.push(user_msg);
    state.messages_dirty = true;

    state.assistant_text.clear();
    let plan = if approve {
        state.active_plan.take()
    } else {
        None
    };
    begin_agent_turn(state, settings.clone(), preload, prompt, move |agent| {
        if let Some(plan) = plan {
            agent.with_plan(plan)
        } else {
            agent.plan_only()
        }
    });
    Ok(())
}

/// Abort the in-flight turn and drop its event receiver.
///
/// Dropping `event_rx` is what closes the leftover-`Done` race: the aborted
/// task may still send, but nothing is listening.
fn abort_current_turn(state: &mut TuiState) {
    if let Some(handle) = state.task_handle.take() {
        handle.abort();
    }
    state.event_rx = None;
}

/// Bind a fresh channel to this turn and spawn the agent off the TUI thread.
fn begin_agent_turn(
    state: &mut TuiState,
    settings: Settings,
    messages: Vec<ChatMessage>,
    prompt: String,
    configure: impl FnOnce(Agent) -> Agent + Send + 'static,
) {
    let (tx, rx) = mpsc::channel::<AgentEvent>(128);
    state.event_rx = Some(rx);
    state.task_handle = Some(spawn_agent_turn(settings, messages, prompt, tx, configure));
}
/// Spawn one agent turn, building the [`Agent`] off the TUI thread.
///
/// The user bubble is already in the log; this must return immediately so
/// the next frame can paint it. Construction failures are reported as
/// [`AgentEvent::Error`] rather than blocking Enter.
fn spawn_agent_turn(
    settings: Settings,
    messages: Vec<ChatMessage>,
    prompt: String,
    tx: mpsc::Sender<AgentEvent>,
    configure: impl FnOnce(Agent) -> Agent + Send + 'static,
) -> tokio::task::JoinHandle<anyhow::Result<Vec<ChatMessage>>> {
    tokio::spawn(async move {
        let constructed =
            tokio::task::spawn_blocking(move || Agent::with_messages(settings, messages))
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("agent construction cancelled: {e}")));
        let mut agent = match constructed {
            Ok(agent) => configure(agent),
            Err(e) => {
                let _ = tx
                    .send(AgentEvent::Error(format!("failed to start agent: {e}")))
                    .await;
                return Err(e);
            }
        };
        agent.run(&prompt, tx).await?;
        Ok(agent.messages)
    })
}

/// The canonical name of a theme, for display in `/theme` output.
pub(crate) fn theme_name(theme: Theme) -> &'static str {
    Theme::all()
        .iter()
        .find(|(_, t)| *t == theme)
        .map(|(n, _)| *n)
        .unwrap_or("?")
}

#[cfg(test)]
mod tests;
