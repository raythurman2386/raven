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

## Current state (verified at v0.2.7)

- `src/tui/mod.rs` — 2,045 lines / 82KB. Holds TuiState, event loop, layout,
  draw_ui, input, mouse, slash commands, plan handling, agent-turn spawn.
  This is the bottleneck; extract only when a surface stabilizes.
- Already extracted and healthy: `blocks.rs`, `completion.rs`, `markdown.rs`,
  `render.rs`, `selection.rs`, `status.rs`, `theme.rs`.
- Layout (`compute_layout`, mod.rs:972): top bar | log | plan | status |
  completion | input. No sidebar (default off).
- Streaming: tail-patch + 60ms draw throttle + tool "glimmer" already shipped
  (`render.rs:120`). Phase 2 streaming is largely done.
- Markdown: headings/code/lists/tables/links/blockquote/tasklists render
  (`markdown.rs`). Code blocks use a generic `┌─ code` label — no language.
- Chrome: top bar has app·model·ctx%·mode; status strip has state·workspace·
  steps·live-tool·waiting-diamond·copy-toast·[stop]. Missing: provider name.
- Theme: ravenwood default + nord/dracula/solarized-dark, `/theme` works.
  Phase 5 is essentially complete — do not churn it.
- Empty state: 5 static SystemBlocks (app info, workspace, context, blank,
  keyhints). No "what to try" guidance.
- Errors: `✗ msg` bold red, one line, no recovery action.
- Scroll: Up/Down/PgUp/PgDn adjust `state.scroll`; auto_scroll detaches on Up,
  reattaches only when scroll hits 0. No single "jump to live" key.

## Slice backlog (in execution order)

Each slice = one focused conventional commit + full gate + a live TUI look.

1. **Tool blocks visually distinct** (Phase 2/3) — highest-value gap. Tool
   calls are inline colored lines (`⇢ read_file(x)`), not distinct from prose.
   Give them a bordered/labeled/dimmed block so "working" reads at a glance.
   Reuse `ToolBlock`/`render_blocks`/`tool_style`; do not redefine.
2. **Code block language label** (Phase 3) — `┌─ rust` instead of `┌─ code`.
   Pairs with #1 in the same render pass.
3. **Jump-to-live key + robust scroll reattach** (Phase 1) — a key (e.g. `End`
   or `g`) that sets `scroll=0; auto_scroll=true`; reattach must not require
   scrolling all the way back to 0.
4. **Context-sensitive keyhints footer** (Phase 4) — replace the static top
   SystemBlock keyhint with a footer that changes with state (plan pending →
   "yes approve · type revise", ask_user → "answer", running → "enter
   interrupt").
5. **Provider in status line** (Phase 4) — add provider name next to model.
6. **Empty-state "what to try" + error recovery line** (Phase 6) — one short
   guidance line on empty transcript; one recovery action under `✗`.

## Acceptance per slice

- Before/after notes or screenshot.
- `cargo test` (579 green baseline) + `cargo clippy --all-targets -- -D warnings`
  + `cargo fmt --all --check` + `cargo check --target x86_64-pc-windows-gnu`.
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
