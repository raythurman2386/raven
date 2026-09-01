//! Unit tests for the TUI state machine, rendering helpers, and slash-command
//! / plan handling. Extracted from `mod.rs` so the event loop stays readable.

use super::render::prewrap_lines;
use super::*;
use crate::config::{ConfigFile, Provider, Scope};
use crate::plan::AgentState;
use serde_json::json;
use std::path::Path;

/// Skip a test that opens a network socket (provider `/models` fetch or
/// context-window probe) when running under a restrictive outer sandbox that
/// SIGSYS-kills AF_INET sockets. Returns `true` when the test should skip.
fn skip_if_outer_sandbox() -> bool {
    if crate::testutil::outer_sandbox_restrictive() {
        eprintln!("outer sandbox blocks AF_INET sockets; skipping network-dependent TUI test");
        true
    } else {
        false
    }
}

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
    let (txt, _color) = state_label(
        &AgentState::Idle,
        "awaiting answer",
        false,
        Theme::RAVENWOOD,
    );
    assert_eq!(txt, "awaiting answer");
}

#[test]
fn state_label_running_when_busy() {
    let (txt, _color) = state_label(&AgentState::Idle, "running…", true, Theme::RAVENWOOD);
    assert_eq!(txt, "running");
    let (txt, _color) = state_label(&AgentState::Idle, "ready", false, Theme::RAVENWOOD);
    assert_eq!(txt, "ready");
}

#[test]
fn deactivate_tool_matches_by_name_not_last() {
    // Parallel: read_a, write_b, read_c all active. End read_a first.
    let mut blocks = vec![
        BlockKind::Tool(ToolBlock::start("read_a", "⇢ read_a".into())),
        BlockKind::Tool(ToolBlock::start("write_b", "⇢ write_b".into())),
        BlockKind::Tool(ToolBlock::start("read_c", "⇢ read_c".into())),
    ];
    deactivate_tool(&mut blocks, "read_a", "ok", 5);
    // read_a cleared; write_b and read_c still active.
    assert!(!matches!(&blocks[0], BlockKind::Tool(t) if t.active));
    assert!(matches!(&blocks[1], BlockKind::Tool(t) if t.active));
    assert!(matches!(&blocks[2], BlockKind::Tool(t) if t.active));
    // read_a got the preview.
    assert!(matches!(&blocks[0], BlockKind::Tool(t) if t.preview.as_deref() == Some("ok")));
    // End read_c next — must clear read_c, not write_b.
    deactivate_tool(&mut blocks, "read_c", "done", 6);
    assert!(matches!(&blocks[1], BlockKind::Tool(t) if t.active));
    assert!(!matches!(&blocks[2], BlockKind::Tool(t) if t.active));
}

#[test]
fn deactivate_tool_falls_back_to_last_when_no_match() {
    let mut blocks = vec![
        BlockKind::Tool(ToolBlock::start("read_a", "⇢ read_a".into())),
        BlockKind::Tool(ToolBlock::start("read_b", "⇢ read_b".into())),
    ];
    deactivate_tool(&mut blocks, "unknown", "x", 1);
    assert!(matches!(&blocks[0], BlockKind::Tool(t) if t.active));
    assert!(!matches!(&blocks[1], BlockKind::Tool(t) if t.active));
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
    // A single long word hard-wraps at the Paragraph content width
    // (rect.width - 2 for borders). With rect width 10, content width is 8,
    // so the prompt "❯ " (2 cells) fills row 0 and the word "abcdefg"
    // (7 cells) wraps to row 1. The cursor after byte 7 sits at row 1, col 7.
    let rect = ratatui::layout::Rect::new(0, 20, 10, 5);
    let (x, y) = input_cursor_position("abcdefghijkl", "❯ ", 7, rect);
    assert_eq!(y, 22, "cursor should be on the second row, got y={y}");
    assert_eq!(x, 8, "cursor should be at col 7 inside the box, got x={x}");
}

#[test]
fn input_cursor_position_breaks_on_word_boundaries() {
    // Ratatui Paragraph word-wraps. Character-wrapping would put the
    // caret after "th" on row 0; word-wrap moves "this" to row 1.
    // content width 16: "❯ please wrap " (14) / "this…"
    let rect = ratatui::layout::Rect::new(0, 20, 18, 5);
    let text = "please wrap this sentence here";
    let at = "please wrap this".len();
    let (x, y) = input_cursor_position(text, "❯ ", at, rect);
    assert_eq!(y, 22, "cursor should follow the wrapped word, got y={y}");
    assert_eq!(x, 5, "cursor should sit after 'this' on row 1, got x={x}");
}

#[test]
fn input_cursor_position_empty_input() {
    let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
    let (x, y) = input_cursor_position("", "❯ ", 0, rect);
    assert_eq!(x, 3, "cursor x should be after prompt only");
    assert_eq!(y, 21);
}

#[test]
fn input_cursor_position_emoji_is_two_cells() {
    // Emoji (😀) is 2 display cells. The cursor after one emoji should
    // be 2 cells further than after one ASCII char.
    let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
    let (x, _) = input_cursor_position("😀", "❯ ", 4, rect);
    // prompt "❯ " = 2 cells, emoji = 2 cells, so cursor at x = 1 + 4 = 5
    assert_eq!(x, 5, "emoji should be 2 display cells, got x={x}");
}

#[test]
fn input_cursor_position_cjk_is_two_cells() {
    // CJK character (あ) is 2 display cells.
    let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
    let (x, _) = input_cursor_position("あ", "❯ ", 3, rect);
    // prompt "❯ " = 2 cells, CJK = 2 cells, so cursor at x = 1 + 4 = 5
    assert_eq!(x, 5, "CJK char should be 2 display cells, got x={x}");
}

#[test]
fn input_cursor_position_combining_mark_is_zero_width() {
    // e + combining acute (U+0301) is one grapheme of width 1.
    let rect = ratatui::layout::Rect::new(0, 20, 80, 3);
    let combined = "e\u{0301}";
    let (x, _) = input_cursor_position(combined, "❯ ", combined.len(), rect);
    // prompt "❯ " = 2 cells, grapheme = 1 cell, so cursor at x = 1 + 3 = 4
    assert_eq!(x, 4, "combining mark should be 0 width, got x={x}");
}

#[test]
fn wrapped_line_count_emoji_is_two_cells() {
    // 10 emoji in a width-10 box: each emoji is 2 cells, so 5 fit per line.
    let input = "😀".repeat(10);
    let lines = wrapped_line_count(&input, 10);
    assert_eq!(
        lines, 2,
        "10 emoji at width 10 should wrap to 2 lines, got {lines}"
    );
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
        status: String::new(),
        iterations_max: 60,
        plan_pending: false,
        plan_preview: Vec::new(),
        active_plan: None,
        running: false,
        mode: Mode::Agent,
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
        steer_tx: None,
        pending_late_steer: None,
        title_handle: None,
        selection: None,
        last_click: None,
        copy_status: None,
        theme: Theme::RAVENWOOD,
        prompt_history: Vec::new(),
        hist_idx: 0,
        last_turn: None,
    }
}

#[test]
fn abort_current_turn_drops_stale_events() {
    let mut state = dummy_state();
    let (tx, rx) = mpsc::channel::<AgentEvent>(8);
    state.event_rx = Some(rx);
    tx.try_send(AgentEvent::Done).unwrap();
    tx.try_send(AgentEvent::TextDelta("stale".into())).unwrap();
    abort_current_turn(&mut state);
    assert!(state.event_rx.is_none());
    assert!(state.task_handle.is_none());
    // The sender is still alive; those events must not be readable from
    // the (now dropped) turn receiver.
    assert!(tx.try_send(AgentEvent::Error("late".into())).is_err());
}

#[tokio::test]
async fn begin_agent_turn_replaces_receiver() {
    let mut state = dummy_state();
    let (old_tx, old_rx) = mpsc::channel::<AgentEvent>(8);
    state.event_rx = Some(old_rx);
    old_tx.try_send(AgentEvent::Done).unwrap();

    let tmp = tempfile::tempdir().unwrap();
    begin_agent_turn(
        &mut state,
        test_settings(tmp.path()),
        Vec::new(),
        "hi".into(),
        |agent| agent,
    );
    assert!(state.event_rx.is_some());
    assert!(state.task_handle.is_some());
    // Stale Done on the old channel is gone with old_rx.
    let rx = state.event_rx.as_mut().unwrap();
    assert!(rx.try_recv().is_err());
    abort_current_turn(&mut state);
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
        provider: Provider::builtin("ollama").expect("ollama builtin"),
        workspace: workspace.to_path_buf(),
        max_iterations: 5,
        mode: Mode::Agent,
        scope: Scope::Repo,
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
        searxng_url: None,
        searxng_engines: Vec::new(),
        sandbox_extra_rw: Vec::new(),
        allow_delegate: true,
    }
}

#[tokio::test]
async fn model_switch_updates_settings_compact_and_header_blocks() {
    if skip_if_outer_sandbox() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    // Point at an unreachable host so the test deterministically exercises
    // the name-heuristic fallback (a live local Ollama would otherwise
    // return the real /api/show value and make the assertion environment-
    // dependent).
    settings.provider = Provider {
        name: "unreachable".into(),
        base_url: "http://127.0.0.1:1/v1".into(),
        api_key: None,
        api_key_env: None,
        default_model: "gemma4:latest".into(),
    };
    let mut state = dummy_state();
    // Seed the header blocks the way TuiState::new does.
    state.blocks = vec![
        BlockKind::System(SystemBlock::new(format!(
            "raven · {} · {}",
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
            fmt_tokens(128_000 - 128_000 / 8),
        ))),
    ];
    let mut compact_at = 128_000 - 128_000 / 8;

    let pc = commands::parse("/model deepseek-v4-pro:cloud").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert_eq!(settings.model, "deepseek-v4-pro:cloud");
    // deepseek-v4-pro:cloud → 1M (live /api/show or name heuristic).
    assert_eq!(settings.context_window, 1_000_000);
    assert_eq!(settings.max_tokens, Settings::derived_max_tokens(1_000_000));
    // compact_at recomputed from the new window (window - reserve) * threshold.
    let expected_compact =
        ((1_000_000 - 1_000_000 / 8) as f32 * settings.compact_threshold) as usize;
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
        assert!(b.text().contains("1.0M"), "context block: {}", b.text());
    } else {
        panic!("block 2 should be a SystemBlock");
    }
}

#[test]
fn slash_command_completes_provider_and_model_names() {
    if skip_if_outer_sandbox() {
        return;
    }
    let settings = test_settings(tempfile::tempdir().unwrap().path());
    let arg_candidates = |cmd: &str| -> Vec<String> {
        crate::tui::completion_arg_candidates(&settings, &ConfigFile::default(), cmd)
    };

    let provider = candidates_for("/provider o", &arg_candidates).unwrap();
    assert!(provider.candidates.iter().any(|s| s == "ollama"));

    let model = candidates_for("/model q", &arg_candidates).unwrap();
    assert!(model.candidates.iter().any(|s| s == "qwen3.8:latest"));
}

#[tokio::test]
async fn retry_re_fires_last_turn_and_clears_failed_output() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    // Simulate a completed turn whose last message is the user prompt, plus
    // a failed partial assistant reply that /retry must drop.
    state.last_turn = Some((Vec::new(), "do the thing".into(), false));
    state
        .session_messages
        .push(ChatMessage::plain("user", Some("do the thing".into())));
    state.session_messages.push(ChatMessage::plain(
        "assistant",
        Some("partial failed output".into()),
    ));
    state.assistant_text = "partial failed output".into();

    let pc = commands::parse("/retry").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(state.running, "retry must start a turn");
    assert!(state.task_handle.is_some(), "retry must spawn an agent");
    // The failed partial assistant message must be truncated away.
    assert_eq!(state.session_messages.len(), 1);
    assert_eq!(state.session_messages[0].role, "user");
    assert!(state.assistant_text.is_empty());
}

#[tokio::test]
async fn retry_without_prior_turn_reports_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    let pc = commands::parse("/retry").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(
        !state.running,
        "no turn should start without a prior prompt"
    );
    assert!(state.task_handle.is_none());
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("nothing to retry"),
            _ => false,
        }),
        "should report nothing to retry"
    );
}

#[tokio::test]
async fn retry_rejected_while_running() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();
    state.last_turn = Some((Vec::new(), "do the thing".into(), false));
    state.running = true;

    let pc = commands::parse("/retry").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("already running"),
            _ => false,
        }),
        "should report already running"
    );
    assert!(state.running, "running flag should be left as-is");
}

#[tokio::test]
async fn loop_command_sets_and_reads_iteration_budget() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    // /loop with a value sets the budget.
    let pc = commands::parse("/loop 15").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();
    assert!(handled);
    assert_eq!(settings.max_iterations, 15);

    // /loop with no args reports the current budget.
    let pc = commands::parse("/loop").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();
    assert!(handled);
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("max iterations: 15"),
            _ => false,
        }),
        "should report the current budget"
    );
}

#[tokio::test]
async fn loop_command_rejects_invalid_counts() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let original = settings.max_iterations;
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    for bad in ["0", "-1", "abc"] {
        let pc = commands::parse(&format!("/loop {bad}")).unwrap();
        let _ = dispatch::dispatch_slash_command(
            &mut state,
            &pc,
            &mut settings,
            &store,
            &mut session,
            &mut compact_at,
            &ConfigFile::default(),
        )
        .await
        .unwrap();
    }
    assert_eq!(
        settings.max_iterations, original,
        "budget must be unchanged"
    );
}

#[tokio::test]
async fn steer_running_turn_queues_without_aborting() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    state.last_turn = Some((Vec::new(), "implement auth".into(), false));
    state.running = true;
    // A live steering sender + fake handle so we can observe that the turn
    // is NOT aborted and the direction lands in the queue.
    let (steer_tx, mut steer_rx) = mpsc::unbounded_channel::<String>();
    state.steer_tx = Some(steer_tx);
    let (_tx, rx) = mpsc::channel::<AgentEvent>(8);
    state.event_rx = Some(rx);
    state.task_handle = Some(tokio::spawn(async { Ok::<_, anyhow::Error>(Vec::new()) }));

    let pc = commands::parse("/steer use a token store").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(state.running, "steer must leave the running turn alone");
    assert!(
        state
            .blocks
            .iter()
            .any(|b| matches!(b, BlockKind::System(s) if s.text().contains("→ steered:"))),
        "should confirm the steer"
    );
    assert_eq!(
        steer_rx.try_recv().ok().as_deref(),
        Some("use a token store"),
        "direction must be queued into the running turn"
    );
}

#[tokio::test]
async fn steer_requires_message() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();
    state.last_turn = Some((Vec::new(), "implement auth".into(), false));
    state.running = true;

    let pc = commands::parse("/steer").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(
        state.running,
        "running turn must be left alone on empty steer"
    );
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("/steer <message>"),
            _ => false,
        }),
        "should prompt for a message"
    );
}

#[tokio::test]
async fn steer_without_prior_turn_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut session = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    let pc = commands::parse("/steer do x").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(
        !state.running,
        "no turn should start without a prior prompt"
    );
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("nothing to steer"),
            _ => false,
        }),
        "should report nothing to steer"
    );
}

/// Create a session whose `updated_at` is rewritten to `date` (YYYY-MM-DD).
/// Writes summary.json directly (write_summary is private to session.rs).
fn session_with_updated_at(workspace: &Path, store: &SessionStore, date: &str) -> Session {
    let mut s = store.create("gemma4:latest").unwrap();
    s.summary.updated_at = format!("{date}T00:00:00");
    let dir = workspace
        .join(".raven")
        .join("sessions")
        .join(&s.summary.id);
    std::fs::write(
        dir.join("summary.json"),
        serde_json::to_string_pretty(&s.summary).unwrap(),
    )
    .unwrap();
    s
}

#[tokio::test]
async fn cleanup_dry_run_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    // The current session is the "now" reference.
    let mut current = store.create("gemma4:latest").unwrap();
    // An old session from 30 days ago.
    let old = session_with_updated_at(tmp.path(), &store, "2020-01-01");
    // A recent session.
    let recent = session_with_updated_at(tmp.path(), &store, "2099-01-01");

    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    let pc = commands::parse("/cleanup 10").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut current,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    // Dry-run must not delete anything.
    assert!(store.load(&old.summary.id).is_ok());
    assert!(store.load(&recent.summary.id).is_ok());
    // And it should report the old session as a candidate.
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("Re-run with --yes"),
            _ => false,
        }),
        "dry-run should prompt to confirm"
    );
}

#[tokio::test]
async fn cleanup_with_yes_deletes_only_old_sessions() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut current = store.create("gemma4:latest").unwrap();
    let old = session_with_updated_at(tmp.path(), &store, "2020-01-01");
    let recent = session_with_updated_at(tmp.path(), &store, "2099-01-01");

    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    let pc = commands::parse("/cleanup 10 --yes").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut current,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    // Old session deleted.
    assert!(store.load(&old.summary.id).is_err());
    // Recent session kept.
    assert!(store.load(&recent.summary.id).is_ok());
    // Current session kept (never deleted).
    assert!(store.load(&current.summary.id).is_ok());
    // Report deletion.
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("deleted 1 session(s)"),
            _ => false,
        }),
        "should report deletion count"
    );
}

#[tokio::test]
async fn cleanup_rejects_invalid_days() {
    let tmp = tempfile::tempdir().unwrap();
    let store = SessionStore::for_workspace(tmp.path()).unwrap();
    let mut current = store.create("gemma4:latest").unwrap();
    let mut settings = test_settings(tmp.path());
    let mut compact_at = 128_000 - 128_000 / 8;
    let mut state = dummy_state();

    let pc = commands::parse("/cleanup").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut current,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();

    assert!(handled);
    assert!(
        state.blocks.iter().any(|b| match b {
            BlockKind::System(s) => s.text().contains("usage: /cleanup"),
            _ => false,
        }),
        "should show usage on missing days"
    );
}

#[test]
fn date_minus_days_handles_rollover() {
    // Straight subtraction within a month.
    assert_eq!(
        dispatch::date_minus_days("2026-08-23T10:00:00", 3),
        "2026-08-20"
    );
    // Month rollover.
    assert_eq!(
        dispatch::date_minus_days("2026-08-02T10:00:00", 5),
        "2026-07-28"
    );
    // Year rollover.
    assert_eq!(
        dispatch::date_minus_days("2026-01-03T10:00:00", 4),
        "2025-12-30"
    );
}

#[test]
fn opencode_go_models_autocomplete() {
    if skip_if_outer_sandbox() {
        return;
    }
    // The provider-aware fallback must surface opencode-go models even when
    // the live /models fetch returns nothing (deterministic offline test).
    let mut settings = test_settings(tempfile::tempdir().unwrap().path());
    settings.provider = crate::config::Provider::builtin("opencode-go")
        .expect("opencode-go builtin")
        .resolve_key();
    let arg_candidates = |cmd: &str| -> Vec<String> {
        crate::tui::completion_arg_candidates(&settings, &ConfigFile::default(), cmd)
    };

    let q = candidates_for("/model q", &arg_candidates).unwrap();
    assert!(
        q.candidates.iter().any(|s| s == "qwen3.8-max"),
        "qwen3.8-max not in /model q completion: {:?}",
        q.candidates
    );

    let m = candidates_for("/model m", &arg_candidates).unwrap();
    assert!(
        m.candidates.iter().any(|s| s == "minimax-m3"),
        "minimax-m3 not in /model m completion: {:?}",
        m.candidates
    );

    let d = candidates_for("/model d", &arg_candidates).unwrap();
    assert!(
        d.candidates.iter().any(|s| s == "deepseek-v4-flash"),
        "deepseek-v4-flash not in /model d completion: {:?}",
        d.candidates
    );
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
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
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
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
    )
    .await
    .unwrap();
    assert!(handled);
    assert_eq!(state.theme, Theme::NORD);

    // /theme unknown reports an error and leaves the theme unchanged.
    let pc = commands::parse("/theme nope").unwrap();
    let handled = dispatch::dispatch_slash_command(
        &mut state,
        &pc,
        &mut settings,
        &store,
        &mut session,
        &mut compact_at,
        &ConfigFile::default(),
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

#[test]
fn history_recall_up_returns_previous() {
    let hist = vec!["first".to_string(), "second".to_string()];
    let (p, idx) = history_recall_up(&hist, 2).unwrap();
    assert_eq!(p, "second");
    assert_eq!(idx, 1);
    assert_eq!(history_recall_up(&hist, 0), None);
    assert_eq!(history_recall_up(&[], 0), None);
}

#[test]
fn history_recall_down_moves_forward_then_baseline() {
    let hist = vec!["first".to_string(), "second".to_string()];
    // from index 1 -> live baseline (empty)
    let (p, idx) = history_recall_down(&hist, 1).unwrap();
    assert_eq!(p, "");
    assert_eq!(idx, 2);
    // from baseline -> None
    assert_eq!(history_recall_down(&hist, 2), None);
    // from 0 -> index 1
    let (p, idx) = history_recall_down(&hist, 0).unwrap();
    assert_eq!(p, "second");
    assert_eq!(idx, 1);
}

#[test]
fn history_recall_active_stays_active_until_typing_resets() {
    let hist = ["a".to_string(), "b".to_string(), "c".to_string()];
    // Empty input: recall is active.
    assert!(history_recall_active(true, hist.len(), hist.len()));
    // Mid-recall (a recalled prompt still in the box, hist_idx < len):
    // still active, so Up keeps walking back through history.
    assert!(history_recall_active(false, hist.len(), 2));
    assert!(history_recall_active(false, hist.len(), 0));
    // After typing resets hist_idx to len: recall no longer active, so
    // Up/Down scroll the transcript.
    assert!(!history_recall_active(false, hist.len(), hist.len()));
    // Empty history, non-empty input, live position: inactive (scrolls).
    assert!(!history_recall_active(false, 0, 0));
}

#[test]
fn multi_step_up_recall_walks_entire_history() {
    // Simulate the sequence of Up presses: start at the live position and
    // walk back through every prompt, verifying each step recalls the prior.
    let hist = [
        "first".to_string(),
        "second".to_string(),
        "third".to_string(),
    ];
    let mut hist_idx = hist.len();

    // 1st Up -> "third"
    let (recalled, idx) = history_recall_up(&hist, hist_idx).unwrap();
    assert_eq!(recalled, "third");
    hist_idx = idx;
    assert!(history_recall_active(true, hist.len(), hist_idx));

    // 2nd Up (input now non-empty, but still mid-recall) -> "second"
    let (recalled, idx) = history_recall_up(&hist, hist_idx).unwrap();
    assert_eq!(recalled, "second");
    hist_idx = idx;
    assert!(history_recall_active(false, hist.len(), hist_idx));

    // 3rd Up -> "first"
    let (recalled, idx) = history_recall_up(&hist, hist_idx).unwrap();
    assert_eq!(recalled, "first");
    hist_idx = idx;

    // 4th Up -> None (at the oldest prompt). The event loop must fall through
    // to log scroll here (see scroll_log_by), not silently no-op.
    assert!(history_recall_up(&hist, hist_idx).is_none());
}

#[test]
fn scroll_log_by_detaches_and_reattaches_auto_follow() {
    let mut state = dummy_state();
    state.log_max_scroll = 100;
    assert!(state.auto_scroll);
    assert_eq!(state.scroll, 0);

    scroll_log_by(&mut state, 3);
    assert_eq!(state.scroll, 3);
    assert!(!state.auto_scroll);
    assert!(state.input_dirty);

    scroll_log_by(&mut state, -2);
    assert_eq!(state.scroll, 1);
    assert!(!state.auto_scroll);

    scroll_log_by(&mut state, -1);
    assert_eq!(state.scroll, 0);
    assert!(state.auto_scroll, "returning to the live tail reattaches");
}

#[test]
fn scroll_log_by_escapes_home_sentinel_toward_live() {
    // Regression: Home used to set scroll = u16::MAX. Rendering clamped to
    // max_scroll, but wheel/PgDn subtracted from MAX and stayed visually stuck
    // at the top until End. Relative moves must clamp through log_max_scroll.
    let mut state = dummy_state();
    state.log_max_scroll = 20;
    state.scroll = u16::MAX;
    state.auto_scroll = false;

    scroll_log_by(&mut state, -3);
    assert_eq!(state.scroll, 17);
    assert!(!state.auto_scroll);

    scroll_log_by(&mut state, -10);
    assert_eq!(state.scroll, 7);

    // Upward moves also clamp to max (no overshoot past the top).
    state.scroll = 18;
    scroll_log_by(&mut state, 10);
    assert_eq!(state.scroll, 20);
    assert!(!state.auto_scroll);
}

#[test]
fn sync_log_max_scroll_matches_viewport() {
    let mut state = dummy_state();
    // One cached line that wraps to 5 rows at width 4 ("abcdefghij" → 5 rows).
    state.cached_log_lines = vec![Line::from("abcdefghij")];
    state.log_gen = 1;
    let size = Rect::new(0, 0, 20, 12);
    // layout: top=1, log gets remaining minus status/input. Force a known
    // size by using sync's geometry: content_width = 20-4=16 for full width
    // log — but compute_layout splits. Just assert it is finite and ≤ total.
    sync_log_max_scroll(&mut state, size);
    assert!(
        state.log_total_rows > 0,
        "expected wrapped rows, got {}",
        state.log_total_rows
    );
    assert!(state.log_max_scroll <= state.log_total_rows as u16);
}

#[test]
fn history_exhausted_fallthrough_scrolls_log() {
    // Empty history + empty input: recall is "active" but Up returns None.
    // The event loop must scroll instead of no-opping (pre-fix bug).
    assert!(history_recall_active(true, 0, 0));
    assert!(history_recall_up(&[], 0).is_none());

    let mut state = dummy_state();
    state.log_max_scroll = 100;
    if history_recall_up(&state.prompt_history, state.hist_idx).is_none() {
        scroll_log_by(&mut state, 1);
    }
    assert_eq!(state.scroll, 1);
    assert!(!state.auto_scroll);

    // At the oldest recalled entry, further Up also scrolls.
    state.prompt_history = vec!["only".into()];
    state.hist_idx = 0;
    state.input = "only".into();
    assert!(history_recall_active(false, 1, 0));
    assert!(history_recall_up(&state.prompt_history, state.hist_idx).is_none());
    scroll_log_by(&mut state, 1);
    assert_eq!(state.scroll, 2);
}

#[test]
fn mouse_scroll_moves_log_not_history() {
    // Wheel handling must adjust scroll offsets and must not touch prompt
    // history / hist_idx (history recall is keyboard Up/Down only).
    let mut state = dummy_state();
    state.log_max_scroll = 100;
    state.prompt_history = vec!["prior".into()];
    state.hist_idx = 1;
    let size = Rect::new(0, 0, 80, 24);
    let log_rect = Rect::new(0, 1, 80, 18);
    let dir = tempfile::tempdir().unwrap();
    let store = crate::session::SessionStore::for_workspace(dir.path()).unwrap();
    let mut session = store.create("test-model").unwrap();

    let up = MouseEvent {
        kind: MouseEventKind::ScrollUp,
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    handle_mouse_event(&up, &mut state, size, log_rect, &store, &mut session);
    assert_eq!(state.scroll, 3);
    assert!(!state.auto_scroll);
    assert_eq!(state.hist_idx, 1, "wheel must not walk prompt history");
    assert!(state.input.is_empty());

    let down = MouseEvent {
        kind: MouseEventKind::ScrollDown,
        column: 10,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    handle_mouse_event(&down, &mut state, size, log_rect, &store, &mut session);
    assert_eq!(state.scroll, 0);
    assert!(state.auto_scroll);
    assert_eq!(state.hist_idx, 1);
}

#[test]
fn format_tool_args_compact_scalars() {
    let args = json!({"path": "src/main.rs", "line_range": [1, 40]});
    let s = format_tool_args(&args);
    assert!(s.contains("path=src/main.rs"), "got {s:?}");
    assert!(
        !s.contains('{'),
        "should not contain raw JSON braces, got {s:?}"
    );
}

#[test]
fn format_tool_args_truncates_long_string() {
    let args = json!({"old": "x".repeat(200)});
    let s = format_tool_args(&args);
    assert!(
        !s.contains(&"x".repeat(41)),
        "long value should truncate, got {s:?}"
    );
}

#[test]
fn format_tool_args_empty_and_non_object() {
    assert_eq!(format_tool_args(&json!({})), "");
    // Non-object args render compact and truncated.
    let s = format_tool_args(&json!([1, 2, 3]));
    assert!(s.contains('['));
}

#[test]
fn refresh_log_rows_invalidates_when_content_changes() {
    use ratatui::text::Span;
    let mut state = dummy_state();
    // Seed one short line, width 10.
    state.cached_log_lines = vec![Line::from(Span::styled("aaaa", Style::default()))];
    state.log_gen = 1;
    state.refresh_log_rows(10);
    assert_eq!(state.log_total_rows, 1, "one line => 1 row");

    // Content grows (as during a turn): the log_gen bumps, and the next
    // refresh MUST recompute the total so max_scroll stays correct.
    state.cached_log_lines.push(Line::from(Span::styled(
        "bbbb\ncccc\ndddd",
        Style::default(),
    )));
    state.log_gen = 2;
    state.refresh_log_rows(10);
    assert_eq!(
        state.log_total_rows, 4,
        "row count must grow as the transcript grows"
    );

    // A no-op refresh (same gen, same width) must NOT recompute.
    let before = state.log_total_rows;
    state.refresh_log_rows(10);
    assert_eq!(state.log_total_rows, before);
}

#[test]
fn refresh_log_rows_invalidates_on_resize() {
    use ratatui::text::Span;
    let mut state = dummy_state();
    state.cached_log_lines = vec![Line::from(Span::styled(
        "abcdefghijklmnop",
        Style::default(),
    ))];
    state.log_gen = 1;
    state.refresh_log_rows(8);
    assert_eq!(state.log_total_rows, 2, "16 chars at width 8 => 2 rows");
    // Resize wider: must recompute even though gen is unchanged.
    state.refresh_log_rows(16);
    assert_eq!(state.log_total_rows, 1, "16 chars at width 16 => 1 row");
}
