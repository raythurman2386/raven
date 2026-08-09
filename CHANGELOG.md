# Changelog

All notable changes to Raven are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.8] - 2026-08-09

### Added

- **Extendable theme system with `/theme` slash command** — the TUI's hardcoded
  `Theme` unit-struct is now a `Copy` data struct with a registry of built-in
  presets (`ravenwood`, `nord`, `dracula`, `solarized-dark`). The active theme
  is threaded through every render path (log, markdown, status strip, plan
  panel, selection highlight) and can be switched mid-session with
  `/theme <name>` (or `/theme` to list), which recolors the whole scrollback
  instantly. The theme is also configurable via `theme` in `config.toml` and a
  `--theme` CLI flag (precedence: CLI > config > default `ravenwood`).
- **Cursor-based input editing** — the chat box now tracks an edit cursor.
  `Left`/`Right` move it, `Home`/`End` jump to the start/end, and text is
  inserted/deleted at the cursor instead of only at the end.
- **Slash-command autocomplete** — typing `/` shows a popup of matching
  commands (names + aliases); `Tab` cycles the highlight (accepting when one
  match) and `Shift+Tab` cycles backward. `/theme <partial>` completes theme
  names. The popup renders between the status strip and the input box.

### Removed

- **Model-driven auto-execution** — the `exit_plan_mode` tool, the
  `AgentEvent::PlanReady` event, and all auto-execute branches (TUI + headless)
  are removed. Plan mode now always requires human approval before execution,
  which also eliminates the auto-execute hang where the plan agent kept running
  after signalling readiness.

### Fixed

- **Mouse hit-testing off by one** — the log block has `LEFT|RIGHT|BOTTOM`
  borders (no top), but both `draw_ui` and `current_display` subtracted 2 rows
  for borders, making the viewport 1 row too short. The last content row was
  never rendered and mouse selection/scroll couldn't reach it. Both now
  subtract 1.
- **Cursor out of position when input wraps** — `input_cursor_position` wrapped
  the prompt+input at `input_content_width` (which subtracts the prompt), but
  ratatui wraps the whole line at the content area width (borders only). Once
  input wrapped to a second line, the cursor landed 2 columns off. It now wraps
  at the content area width.
- **Copy-on-highlight broken when scrolled** — the mouse selection was stored
  in full-log row space (`pos.row + offset`), but `apply_selection_highlight`
  and `selection_text` read the visible window. When the log was scrolled, the
  highlight landed on the wrong rows and copy returned empty. The selection is
  now stored in visible-window coordinates, matching what's rendered. This
  became noticeable after the markdown upgrade made assistant blocks taller
  (more scrolling).
- **Right arrow couldn't reach the end of the line** — the Right-arrow handler
  used `char_indices().nth(1)`, which returns `None` when only one char remains
  after the cursor, so the cursor could never advance past the last character
  to the true end. This also meant Backspace (which deletes the char before the
  cursor) could never delete the final letter. The cursor now advances by the
  next char's byte length and reaches `input.len()`.

## [0.1.7] - 2026-08-09

### Added

- **Offline fake-model agent-loop tests** — a `#[cfg(test)]`-only
  `CompletionSource` seam on `Agent` lets `run()` be driven with scripted
  completions and no HTTP. New tests cover finish-on-content, blank-stall
  recovery + cap, tool round-trip, same-file serial edits, and the
  `max_tokens` clamp (`src/agent/tests/fake_model.rs`).
- **Compaction golden tests** — explicit assertions that history is unchanged
  below the threshold and that tool-call/result pairs are never split at the
  tail boundary (`src/context.rs`).

### Changed

- **Model-oriented repo map** — the flat, lexically-sorted symbol dump is
  replaced with a scored, grouped map that spends the char budget on
  entrypoints and public types. Symbols are ranked (entrypoint/public/type
  bonuses, test-path penalties), rendered grouped by workspace-relative path,
  and capped at 3500 chars. The map now builds when `source_files >= 15` OR
  `symbols >= 80` (was 50 files), skips `.next`/`coverage`/`vendor`/`Pods`/
  `.turbo` and files over 256 KiB, and uses a single `WalkDir` pass with
  regexes compiled once.

### Refactored

- **`src/agent.rs` split into internal submodules** — the 2687-line monolith
  is now `src/agent/` (`core`, `stream`, `tools_exec`, `loop_control`,
  `parallel`, `types`) with no public API change. `run()` is a thin
  orchestrator delegating to `handle_no_tool_calls` and `execute_tool_calls`.

## [0.1.6] - 2026-08-09

### Added

- **Markdown rendering in the TUI** — assistant output now renders as styled
  markdown (headings, bold/italic/strikethrough, inline code, fenced code
  blocks, ordered/unordered lists, blockquotes, links, tables) via a new
  `src/tui/markdown.rs` module built on `pulldown-cmark`. The renderer
  re-parses the accumulated text on each stream delta and degrades unclosed
  tokens to literal text, so streaming never flashes raw markdown.
- **Tool calls shown in the TUI** — every tool call now renders as a live
  line (previously only the newest was shown); only the newest active call
  still glimmers with a spinner. The tool's output is not dumped to the log —
  the call line alone is shown, matching Hermes' lean tool-call display.

### Fixed

- **`/model` left model-derived values stale** — the handler only updated
  `settings.model`, `context_window` (via the name heuristic), and
  `max_tokens`. It now fetches the live Ollama `/api/show` context window
  (falling back to the heuristic when unreachable), recomputes the
  `compact_at` threshold, refreshes the two static header blocks in place, and
  persists the new model to the session so a resume shows it.

## [0.1.5] - 2026-08-09

### Fixed

- **Blank model turn treated as a clean finish** — when the model returned a
  turn with no tool calls AND empty/whitespace-only content, raven treated it
  as a legitimate finish: it pushed an empty assistant message, emitted `Done`,
  and returned, silently dropping the deliverable (the research-report session
  that exposed this did all its work then ended with no report). A blank turn
  is now treated as a STALL: raven injects an ephemeral nudge and re-runs the
  loop, capped at 3 attempts, then falls through to `emit_summary` so the turn
  always ends with a visible line. Mirrors the existing verify / repeated-
  failure loop-breaker plumbing. (`#110`)
- **Parallel same-file edits silently lost** — multiple file-mutating tool
  calls (`write_file`/`search_replace`/`apply_patch`) against the same file in
  one turn were dispatched in parallel on the blocking pool. Because
  `search_replace` is an unlocked read-modify-write, two concurrent same-file
  edits both read the original content, so the last writer won and the earlier
  edit was lost while each call still returned success. File-mutating tools are
  now dispatched serially in call order via a shared `record_tool_result`
  helper; read-only and other tools stay parallel. (`#111`)

## [0.1.4] - 2026-08-09

### Fixed

- **Node tooling in sandbox** — removed the `RLIMIT_AS` (virtual address
  space) and `RLIMIT_NPROC` (per-user thread/process) caps that were applied
  to every confined subprocess. Both are the wrong tool: `RLIMIT_AS` bounds
  virtual, not resident, memory and aborted V8/Node at startup (CodeRange
  OOM); `RLIMIT_NPROC` is a user-global ceiling, not a per-child one, so it
  broke high-thread runtimes (Node) on busy machines and could not isolate a
  child. Kept the per-process `RLIMIT_CPU`, `RLIMIT_FSIZE`, and
  `RLIMIT_NOFILE` caps. This fixes the agent being unable to run `npm`,
  `npx tsc`, `node`, etc. in a workspace.
- **`write_file`/`search_replace` on missing intermediate dirs** — file writes
  no longer fail when an intermediate directory in a `..`-path is missing; the
  parent directory is created first (workspace-confined).
- **Windows worktree paths** — `write_file`/`search_replace` no longer fail
  with "Path outside workspace" on Windows when the workspace is not
  canonicalized. The code now passes the original relative path to
  `open_beneath` instead of re-resolving a canonicalized `\\?\`-prefixed path.

### Changed

- **Sandbox resource limits** — `docs/security.md` updated to document the
  omitted `RLIMIT_AS`/`RLIMIT_NPROC` and the rationale for keeping only the
  per-process limits. Windows Job Object memory caps count *committed* memory,
  not virtual address space, so they remain and are documented as such.

### Security

- Removing `RLIMIT_AS`/`RLIMIT_NPROC` closes a tool-compatibility gap without
  opening a hole: Landlock still confines the filesystem, `RLIMIT_CPU`/
  `RLIMIT_FSIZE` bound runaway execution and writes, and seccomp / Job
  Objects still provide network and process-tree confinement.

## [0.1.3] - 2026-08-08

### Added

- **Windows support** — the sandbox now compiles and runs on Windows via a
  Job Object confinement layer (`src/tools/windows.rs`). Every subprocess is
  assigned to a fresh Job Object with `KILL_ON_JOB_CLOSE`, active-process,
  per-process, and per-job memory limits, and process-tree confinement. The
  Job Object handle is held for the child's full lifetime via a RAII guard so
  a runaway child cannot outlive the parent.
- **Cross-platform `OpenFlags`** — replaces the Linux-only `rustix::fs::OFlags`
  in `open_beneath`'s signature/fallback, so the sandbox builds on Windows.
- **Shared confined-spawn path** — `spawn_confined`/`run_confined` is now the
  single spawn path used by `run_shell`, `run_tests`, `run_lint`, and the git
  tools, so every subprocess inherits OS-level confinement.
- `docs/security.md` — new threat model and defense-layer documentation.

### Changed

- **Linux sandbox hardening** — subprocesses are now confined with `openat2`
  (`RESOLVE_BENEATH | NO_MAGICLINKS`), Landlock (workspace + temp + HOME + /dev),
  seccomp (network syscall block), and `setrlimit` (CPU / file size / file
  descriptors).
- **Direct exec** — allowlisted single-binary commands with no shell
  metacharacters are executed directly (no `sh -c`), removing the shell
  injection surface for the common case.
- **TUI fixes** — cursor now tracks correctly when input auto-wraps (shared
  width helper); the input box cap was raised to 12 rows with the cursor
  clamped to the last visible row; bracketed paste is enabled and `Event::Paste`
  appends the whole block at once; the plan panel scrolls with the mouse wheel.
- **Installer** — the `install.sh` EXIT trap now guards `$tmp_dir` with
  `${tmp_dir:-}` to avoid an "unbound variable" error under `set -u`.

### Fixed

- Test `HOME` pollution in `config::tests` that Landlock confinement exposed.
- Input buffer is now length-capped so a multi-MB paste cannot grow memory or
  slow per-frame re-wrap without bound.

### Security

- Confinement now covers all subprocess paths (`run_shell`, `run_tests`,
  `run_lint`, and git), not just `run_shell`.
- `unsafe` is limited to platform-required sites (Linux `pre_exec`, Windows
  Job Object FFI), each with a sound SAFETY comment. The ROADMAP's "zero
  unsafe" principle was updated to reflect this.

## [0.1.2] - 2026-08-08

### Added

- TUI wrapped multi-line input, ravenwood theme, streaming tail-patch.
- Plan mode with model-driven auto-exit + read-only plan toolset.
- Session persistence (atomic `messages.jsonl` + `summary.json`).
- Linear token estimator, phase-timing telemetry.

### Changed

- Ratatui 0.30, crossterm 0.29.

[Unreleased]: https://github.com/raythurman2386/raven/compare/v0.1.8...HEAD
[0.1.8]: https://github.com/raythurman2386/raven/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/raythurman2386/raven/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/raythurman2386/raven/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/raythurman2386/raven/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/raythurman2386/raven/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/raythurman2386/raven/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/raythurman2386/raven/compare/v0.1.1...v0.1.2
