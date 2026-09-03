# Raven TUI Polish — Phased UX Program

**North star:** *A serious operator's cockpit for a local agent* — dense, local,
honest, keyboard-first, terminal-native. Not a chat app with a theme, and not a
Grok Build skin. "Beautiful like a well-worn tool."

This doc is the standing brief for TUI PRs. Each slice is a **vertical UX win**
plus only the module extraction that win needs — no big-bang rewrite, no
agent-loop behavior change unless fixing a TUI bug.

## Non-goals (frozen for this effort)

- No web UI / GUI, no MCP marketplace panels, no cloud-sync chrome.
- No redesign of the agent loop "while we're in the TUI".
- No blocking a release on a total rewrite. Polish lands as 0.2.x incremental;
  a real leap would be 0.4.0 with a CHANGELOG "TUI" section.
- No theme store. One default ravenwood + the existing minimal alts.

## Current state (keybinds verified at v0.5.19)

- `src/tui/mod.rs` — event loop, layout, draw_ui, input, mouse, plan handling,
  agent-turn spawn. Extract only when a surface stabilizes.
- Already extracted and healthy: `blocks.rs`, `completion.rs`, `dispatch.rs`,
  `markdown.rs`, `render.rs`, `selection.rs`, `status.rs`, `theme.rs`.
- Layout (`compute_layout`): top bar | log | plan | status | completion |
  input. No sidebar (default off).
- Streaming: tail-patch + draw throttle + tool "glimmer" already shipped.
- Markdown: headings/code/lists/tables/links/blockquote/tasklists render;
  fenced blocks label the language when present.
- Chrome: top bar has app·model·provider·mode plus a right-aligned
  `used/window` context meter (`━━─` gauge, usage-colored; dropped when the
  terminal is too narrow); status strip has
  state·workspace·steps·live-tool·waiting·copy-toast·[stop] (or the
  quit-confirm hint); footer shows context-sensitive keyhints.
- Keybinds (v0.5.19): `Esc` is the layered key — completion → selection →
  pending prompt (permission gates deny on Esc) → interrupt a running turn →
  double-press-quit (3s window). `Ctrl+C` interrupts while running and needs a
  second press to quit when idle. `Enter` while running queues a steer.
  Permission gates answer in one keystroke (`y`/`n`; bare `Enter` allows).
- Permission gates render as a distinct bordered `permission` block
  (`$ <command>` + `y allow · n deny`), driven by the new
  `AgentEvent::AskPermission` (separate from free-form `ask_user`).
- Event loop: the input-drain loop polls with a zero timeout before each
  blocking `event::read()`, so handler `continue`s can never park the UI
  (the answer-a-prompt freeze).
- Completion: Esc dismissal is sticky (stays closed while typing at/past the
  dismissal point; reopens after deleting below it). Enter submits once the
  replace span holds a complete candidate; Tab cycles and fills.
- Theme: ravenwood default + nord/dracula/solarized-dark, `/theme` works.
- Empty state: guidance line + keyhints; errors show a recovery action.
- Scroll / recall (v0.5.3): mouse wheel and PgUp/PgDn always move the
  transcript (`state.scroll`); Up/Down recall prompt history when the input
  is empty or mid-recall, otherwise scroll; at history boundaries they fall
  through to scroll. Home/End jump top/live when the input is empty.
  `auto_scroll` detaches on upward scroll and reattaches at scroll 0.
  Alternate-scroll (`?1007`) is disabled so the wheel is not remapped to
  Up/Down keys on the alternate screen.

## Slice backlog (in execution order)

Each slice = one focused conventional commit + full gate + a live TUI look.

- [x] **1. Tool blocks visually distinct** (Phase 2/3) — `feat(tui): render tool calls as distinct bordered blocks`. Tool calls now render as a dim bordered box with a label (`┌─ read_file` / `│ ⇢ read_file(x)` / `└─`), distinct from model prose.
- [x] **2. Code block language label** (Phase 3) — `feat(tui): label code blocks with their language tag`. `┌─ rust` instead of `┌─ code`; falls back to `code` when no language.
- [x] **3. Jump-to-live key + robust scroll reattach** (Phase 1) — `feat(tui): Home/End jump to top/live when input is empty`. Empty input: Home jumps to top, End reattaches to the live tail.
- [x] **4. Context-sensitive keyhints footer** (Phase 4) — `feat(tui): context-sensitive keyhint footer`. Static top SystemBlock replaced by a bottom footer that changes with state (answer / approve / interrupt / idle).
- [x] **5. Provider in status line** (Phase 4) — `feat(tui): show provider name in top bar`. Top bar is now `app · model · provider · ctx% · mode`.
- [x] **6. Empty-state "what to try" + error recovery line** (Phase 6) — `feat(tui): empty-state guidance and error recovery line`. Guidance line on empty transcript; recovery action under `✗`.
- [x] **7. Prompt history recall** (Phase 4) — `feat(tui): prompt history recall with up/down on empty input`. Empty input: Up recalls the previous prompt, Down recalls forward; bounded to 100 entries; resets when typing.
- [x] **8. Table cell width cap** (Phase 3) — `feat(tui): cap markdown table cell width`. Cells truncate to a 32-char budget with a `…` marker so wide tables wrap on cell boundaries instead of blowing out a row.
- [x] **9. Compact tool-call args** (Phase 2/4) — `feat(tui): compact key=value tool-call args`. Tool blocks read `read_file path=src/main.rs line=1-40` instead of raw JSON braces; long values truncated.
- [x] **10. Width-aware transcript wrap/scroll** (Phase 2, correctness) — `fix(tui): width-aware transcript wrap/scroll (CJK/emoji)`. The transcript wrap/scroll math now uses display width (`unicode_width`), so CJK/emoji content scrolls correctly instead of drifting. Stays in lockstep with the already-correct input path.
- [x] **11. O(viewport) virtualization** (Phase 2, efficiency) — `perf(tui): cache total row count so virtualization is O(viewport)`. `prewrap_visible` recomputed every line's wrapped-row count each frame (O(history)); the total is now cached and refreshed only on log change or resize.
- [x] **12. Wheel vs history recall** (Phase 1, correctness) — `fix(tui): mouse wheel scrolls log, not prompt history`. Disable alternate-scroll, lite mouse capture, drain pending events, history-boundary fallthrough to scroll; Shift+Tab mode cycle only when idle.

## Acceptance per slice

- Before/after notes or screenshot.
- `cargo test` (656 green baseline) + `cargo clippy --all-targets -- -D warnings`
  + `cargo fmt --all --check`.
- No agent-loop behavior change unless fixing a TUI bug.
- Manual path the user actually uses, examined in the live TUI between slices.

## Cadence

- One theme PR per week max if it's only colors (we're not doing theme work).
- Prefer vertical slices over "extract three modules with no UX change".
- Dogfood every slice on a real repo.

## Icebox (later, not this focus)

- Split diff view for `search_replace`.
- Session browser UI.
- Fancy graphs / token meters beyond the simple ctx bar.
- Image / screenshot previews.
- Custom layout JSON.
- Sidebar (default off).
