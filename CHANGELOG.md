# Changelog

All notable changes to Raven are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Persistent goal + todos** — `.raven/state/` (`goal.json`, `todos.json`)
  survive compaction and resume; injected into the system prompt each turn
  and after `goal_set` / `todo_write`.
- **`goal_set`, `delegate_task`, `think` tools** — long-horizon tracking,
  depth-1 sub-agents (shared workspace, no nested delegate / parent state
  overwrite), and a read-only reasoning scratchpad.
- **Eval case `13_long_horizon`** — multi-step fix that must set a goal and
  track todos.

### Changed

- **Compaction thrashing protection** — pause after 3 no-reduction
  compactions; retry every 4th iteration so a later prune can resume.

## [0.2.3] - 2026-08-14

### Added

- **Eval cases `11_secrets_stay_uncommitted` and `12_verify_before_done`** —
  live graders plus Layer A: `.env` must stay untracked after `git_commit`,
  and an edit that finishes without tests must trip the verify gate.
- **`raven acp`** — Agent Client Protocol v1 on stdin/stdout so editors can
  attach. Text prompts, `session/load` replay, `session/list` /
  `session/close` / `session/set_mode`, cancel, and
  `session/request_permission` for `ask_user` / shell confirm. No MCP, no
  client-owned FS/terminal. Offline protocol tests cover the wire subset.

### Changed

- **Criterion benches measure the real work again** — `repomap` invalidates
  the cache so a sample is a cold walk (plus a cached-hit contrast);
  `context` reuses one tokio runtime instead of constructing one per iter.
- **Eval `12_verify_before_done` grader reads stdout/stderr files** —
  `EVAL_STDOUT` / `EVAL_STDERR` are paths from `evals/run.py`, not the
  captured text. The previous grep ran against the path strings and
  always failed.

- **Sandbox filesystem policy is now explicit** — OS confinement lives in
  `src/tools/sandbox/confinement.rs` as `FsPolicy`. Write roots are the
  workspace, caller-supplied extras (git worktrees), and `/dev`. The process
  temp dir is never granted; children get `TMPDIR` pinned under
  `.raven/tmp`.

### Fixed

- **`run_shell` can no longer write `/tmp` from a home-directory workspace** —
  Landlock used to grant the whole process temp dir whenever the workspace
  was not nested under it. That was the live `06_sandbox_escape` hole.
- **`git_commit` no longer depends on host git identity or hooks** — commits
  set `user.name`/`user.email`, disable `commit.gpgsign`, and point
  `core.hooksPath` at a null path. The `05_git_commit_clean` eval no longer
  fails because the host has no `user.email` or because `cargo test` left
  `Cargo.lock` dirty.

## [0.2.2] - 2026-08-13

### Fixed

- **Interrupted TUI turns can no longer steal the next turn's handle** —
  the session used one shared event channel, so a leftover `Done` from an
  aborted send could `await` the successor and wipe session history. Each
  turn now owns its own channel; abort drops the old receiver.
- **Enter no longer freezes the TUI before the user bubble paints** — each
  send constructed a new `Agent` on the UI thread, which rebuilt the repo
  map by walking the whole workspace. A typical crate was cheap; opening
  Raven on a parent folder of many repos (e.g. `~/Work`) stalled every
  submit. Construction now runs off-thread, the HTTP client is reused, and
  the repo map is cached (and the walk is capped) until a file edit
  invalidates it.
- **`exit=10` no longer counts as a passing test** — verify/lint used
  `contains("exit=0")`, so exit codes 10/20/30 were treated as success.
- **`save_all_messages` is atomic** — it used in-place truncate; a crash
  mid-write could leave a partial JSONL. It now uses the same temp+rename
  path as `append_message`.
- **Headless plan revise actually executes the revised plan** — after
  `[r]evise` + approve, execution still used the first plan and first
  message list.
- **`/provider` lists known names** and rejects unknown names instead of
  silently switching to builtin ollama.
- **Missing-file tool errors are not retry-nudged**; HTTP error bodies
  truncate on a UTF-8 boundary instead of panicking.
- **Multi-line input caret no longer drifts** — the box used ratatui
  word-wrap while the caret character-wrapped, so a long prompt with
  spaces put the cursor on the wrong cell. Input is now pre-wrapped with
  the same word-break rules used to place the caret.

## [0.2.1] - 2026-08-13

### Added

- **Named providers (Ollama / OpenRouter)** — a `Provider` abstraction bundles
  endpoint + auth + default model so switching between local and cloud is a
  single unit, mirroring Grok Build / Hermes / Opencode. Built-in `ollama` and
  `openrouter` presets, a `[providers.<name>]` config table, a `/provider`
  slash command for runtime switching, and a resolution order of
  `--provider` > `RAVEN_PROVIDER` env > config `provider` > builtin. `model`
  stays a per-session override layered on the provider's default.
- **Per-provider `api_key_env`** — a provider's key env var is declared in
  config (e.g. `api_key_env = "ANTHROPIC_API_KEY"`), so adding a new provider
  is a pure config change with no code edit. Unknown providers fall back to a
  conventional `{NAME}_API_KEY`. Only the var *name* is stored in config — the
  secret stays in the environment.
- **Field-wise provider table merge** — workspace `[providers.<name>]` tables
  overlay global ones per field, so a partial workspace entry (e.g. only
  `base_url`) no longer wipes global `api_key_env` / `default_model`.

### Changed

- **Streaming is now truly incremental** — SSE lines are parsed and emitted
  token-by-token as bytes arrive instead of buffering the whole response and
  bursting at the end. Multi-byte UTF-8 split across TCP chunks is still never
  lossy-decoded.
- **Provider-aware errors** — `OllamaUnreachable` renamed to
  `ProviderUnreachable`, and `ModelNotFound` / `HttpError` now carry the
  provider name. Error text adapts: local Ollama gets an `ollama pull` hint,
  other providers get a check-the-model-id message.

### Fixed

- **Tracing no longer corrupts the TUI on tool error** — `tracing` was writing
  to stdout (which the TUI owns in raw mode + alternate screen), so a `WARN
  Transient tool error` line from a failed tool call overlapped the input bar
  and knocked out the status bar. Logs now go to stderr, out of the alternate
  screen.

## [0.2.0] - 2026-08-12

### Fixed

- **Verify gate no longer credits false green** — the model could assert tests
  passed when the runner was actually blocked (e.g. the sandbox SIGSYS-killed
  the test runner before it ran). The gate is now fail-closed: it only credits
  a pass when the runner genuinely executed and exited 0. (`#136`)
- **`run_tests` network-block exemption was dead code** — the `#137` fix set
  `RAVEN_SANDBOX_NETWORK_BLOCK=0` via `Command::env`, but the seccomp filter
  reads `std::env::var` inside the `pre_exec` closure, which runs after
  `fork()` and before `execve()` — at that point it sees the parent env, not
  the `Command::env` override. So the exemption never took effect and
  `run_tests` still SIGSYS-killed vitest/v8 (signal 31). The flag is now
  threaded through `spawn_confined → apply_os_confinement →
  apply_seccomp_network_block` and captured directly in the `pre_exec`
  closure where it is visible. `run_tests` passes `true` for npm runners;
  `run_shell`/`run_lint`/git pass `false`, preserving the exfiltration
  guarantee for arbitrary model output. (`#137` follow-up)
- **Default sandbox still SIGSYS-killed vitest** — the seccomp network block
  killed vitest/v8 (which opens an AF_INET socket for V8 coverage / worker
  IPC). `run_tests` on npm projects now skips the network-block filter for
  that one test-runner invocation. The npm network-block test was also made
  cross-platform (uses `node -e` instead of Unix shell `$VAR` syntax, which
  broke on Windows `cmd`). (`#137`)
- **vitest hangs inside `run_shell` with network block disabled** — the pipe
  drain waited 2s for stdout then 2s for stderr sequentially (4s total), plus
  the 1s child timeout, so a grandchild holding a pipe open could push a
  timeout past its budget. The total drain is now bounded to a single shared
  2s deadline across both pipes, so a timeout returns promptly regardless of
  shell. (`#138`)
- **Max-iteration exhaustion left verified-but-uncommitted work** — when the
  iteration budget was exhausted with verified changes still uncommitted, the
  work could be lost. The checkpoint path now preserves it. (`#140`)
- **Uncommitted sub-agent work lost in parallel mode** — a sub-agent's
  uncommitted worktree changes could be silently discarded on merge. Parallel
  mode now auto-commits uncommitted worktree changes before merging, writes
  recovery patches to `.raven/recovery-sub-N.patch` on merge conflicts or
  errors, and surfaces the `recovery_patch` in the CLI. (`#139`)
- **Checkpoint auto-commit swept in collateral tracked-file deletions** — the
  harness-internal checkpoint commit used `git add -A`, which staged every
  working-tree change, including a sub-agent's collateral deletion of a
  tracked file (e.g. a failed `npm install` removing `package-lock.json`).
  `git_commit_checkpoint()` now stages additions/modifications but unstages
  tracked-file deletions before committing; the model-facing `git_commit`
  tool keeps full `git add -A` semantics. (Finding 28)
- **Windows build failed on the `skip_network_block` param** — the seccomp
  network-block exemption flag is only consumed inside the `#[cfg(unix)]`
  `pre_exec` block, so on Windows it was an unused variable and `-D warnings`
  turned it into a hard error. Added the same `cfg_attr(not(unix),
  allow(unused_variables))` already used for `workspace`/`extra_rw`.
- **`run_tests`/`run_shell` cargo-verify 'passes' tests gated to Linux** — the
  two tests that assert the verify gate credits a pass create a cargo project
  in a temp workspace and run `cargo test` through the sandbox; on Windows
  that fails at MSVC link time (`link.exe: missing operand after '\377\376'`),
  so cargo exits 101 and the fail-closed gate correctly refuses to credit.
  Gated to `#[cfg(target_os = "linux")]`, matching the existing confinement
  tests. The two 'gates' tests (which expect the gate to refuse) stay ungated.

### Changed

- **Sandbox HOME/proc grants for Node tooling** — when the workspace is under
  `$HOME`, Landlock now grants `$HOME` itself read-only (not just the leaf
  toolchain dirs), because Landlock requires Execute on every path component
  to exec a binary — granting only `~/.rustup`, `~/.cargo`, `~/.config`,
  `~/.local/share/mise` left the intermediate components ungranted and exec of
  mise-managed `node`/`npx` failed with EACCES. `/proc` is also granted
  read-only so node/v8 can read `/proc/self/status` (EACCES otherwise made
  vitest hang after tests passed). Build caches stay pinned into the workspace
  (no EXDEV risk).

## [0.1.10] - 2026-08-11

### Added

- **Optional self-hosted SearXNG backend for `web_search`** — when a base URL
  is configured (`RAVEN_SEARXNG_URL` env var or the `searxng_url` config key),
  `web_search` queries the SearXNG JSON API (`GET {base}/search?q=…&format=json`)
  and returns up to 10 results (title + URL + short snippet). `RAVEN_SEARXNG_ENGINES`
  / `searxng_engines` optionally pins the engine list. The base URL must be
  `http`/`https` only; no API key is required. On any failure (HTTP error,
  empty results, or unparseable JSON) it **falls back to DuckDuckGo**, so a
  down local instance never bricks search. Precedence: env var > config file.

### Changed

- **Sandbox temp-dir scoping** — Landlock now grants the process temp dir RW
  **only when the workspace is not under it**. Previously a workspace nested
  under `/tmp` (e.g. `evals/run.py`'s `raven-eval-…/workspace`) granted RW on
  the whole temp dir, letting a confined child write arbitrary siblings under
  `/tmp` (the `06_sandbox_escape` probe). Build caches/temps are pinned into the
  workspace via `pin_build_tool_dirs`; callers that genuinely need a sibling
  (git worktrees) pass it as an explicit extra RW root instead.
- **Git worktree confinement** — `create_worktree`/`remove_worktree` grant RW
  on just the worktree's parent dir (via a new `run_git_with_extra`), and
  parallel sub-agents get `sandbox_extra_rw` for the shared main repo's `.git`,
  instead of opening up the whole temp dir. `Sandbox` gained a
  `with_extra_rw` constructor and an `extra_rw` field threaded through
  `spawn_confined`/`run_confined`.

### Fixed

- **`git_commit` fails for workspaces under `$HOME`** — the Landlock sandbox
  granted `$HOME` read-only but, in the "workspace under HOME" branch, only
  granted RO to sibling toolchain dirs (`.rustup`, `.cargo`, `.config`), not
  `~/.gitconfig`. Git couldn't read user identity, so `git_commit` failed with
  "unable to access '~/.gitconfig': Permission denied". Now grants RO to
  `~/.gitconfig` and `~/.git-credentials` (read-only, so no write-surface
  widening). Found via stress-testing in `/home/ret/Work/raven-stress`.
- **`install.ps1` closes the window on failure** — when piped into `iex` (the
  documented one-liner), every error path called `exit 1`, which terminated the
  host PowerShell session and closed the window before the user saw the error.
  The whole install is now wrapped in a `try/catch` that prints the failure in
  red and pauses (`Press Enter to close…`, skipped when non-interactive) before
  exiting.

## [0.1.9] - 2026-08-10

### Added

- **Live agent eval suite** — `evals/run.py` runs 10 fixture cases (read-only
  symbol, single edit, multi-file refactor, fix failing test, git commit,
  sandbox escape, memory recall, skill use, plan-then-execute, add test)
  against a built headless `raven`, graded by deterministic `checks.sh`
  (not LLM-as-judge). Layer A (`cargo test eval_suite`) is an offline
  scripted-fake-model harness; Layer B is live fixtures; Layer C is the
  arena. Reports land in `evals/out/`; baseline in `evals/baselines/default.md`.
- **Cloud API support** — a repo-root/CWD `.env` is auto-loaded (without
  overriding exported vars), and API keys resolve via
  `RAVEN_API_KEY` → `OLLAMA_API_KEY` → `OPENROUTER_API_KEY` → `XAI_API_KEY` →
  `OPENAI_API_KEY`. `/model` now fetches the real context window from the
  provider's `/models` endpoint (with bearer auth) for non-Ollama providers,
  falling back to name heuristics.
- **macOS release targets** — `aarch64-apple-darwin` and `x86_64-apple-darwin`
  added to the release matrix (native runners, no cross-compile).
- **Chat toolset** — Chat mode now includes `ask_user` (and all read-only
  tools); the system prompt is mode-aware and `dispatch()` has a runtime
  read-only guard as a defense-in-depth backstop.
- **Ground-truth anchors** — the system prompt includes the current git
  working-tree state, and parallel sub-agent merge results now carry a
  `merge_status` ("merged"/"conflict"/"no changes"/"error") surfaced to the
  model so it can't assume unmerged work landed.
- **Sandbox escape hatches** — `RAVEN_SANDBOX_LANDLOCK=0` and
  `RAVEN_SANDBOX_NETWORK_BLOCK=0` to skip confinement for tests/recovery.

### Changed

- **seccomp network block** — now blocks only `socket()` for `AF_INET`/
  `AF_INET6` with `KillProcess` (immediate kill, not `EPERM`). `AF_UNIX`
  sockets and `socketpair()` are allowed, so esbuild and git ssh helpers
  work without an escape hatch while the exfiltration guarantee holds.
  **Note**: vitest/v8 still opens an AF_INET socket (for V8 coverage /
  worker IPC), so it is killed by the seccomp filter. `run_tests` on npm
  projects now sets `RAVEN_SANDBOX_NETWORK_BLOCK=0` to skip the filter for
  that one test-runner invocation — the test runner is a user-sanctioned
  command, not arbitrary model output, so the exfiltration guarantee is
  preserved.
- **Landlock ABI V3** — `AccessFs::from_all` now includes `REFER`, fixing
  `rustc` `.rmeta` hardlinks into `target/`. `CARGO_HOME`, `CARGO_TARGET_DIR`,
  `TMPDIR`, and the npm cache are pinned under `workspace/.raven/`, and `$HOME`
  is read-only (gitconfig/rustup) instead of read-write.
- **`--yolo` implies `--mode agent`** — full toolset, no plan step, no
  confirmations. An explicit `--mode` still overrides.
- **`replace_all` always writes** — when the match count exceeds the warning
  threshold it still performs the replacement and appends a warning, instead
  of silently returning success without modifying the file.
- **Release profile** — `opt-level="z"` + `panic="abort"` cut the binary from
  13M to 8.3M (36%); tokio narrowed to specific features.
- **TUI polish** — parallel tool calls deactivate the correct named block
  (no stuck spinner), idle redraw is gated on `input_dirty`, cursor alignment
  is grapheme-aware display width, a mode indicator shows in the top bar,
  tool-result previews render dim under each call, and Esc is layered
  (completion → selection → ask_user → quit).

### Fixed

- **`run_shell` hang past timeout** — the reader-thread `join()` blocked on an
  inherited stdout pipe held open by a grandchild, so a green (non-timeout)
  run could hang forever. Pipes are now drained with a bounded deadline and
  reader handles are dropped instead of joined.
- **`search_replace` on `..`-paths** — the raw path was passed to `openat2`
  instead of the lexically normalized path, so `newdir/../f.txt` failed with
  ENOENT even though it was validated as inside the workspace.
- **Iteration-budget dirty tree** — when the budget is exhausted with
  uncommitted changes, raven now injects a commit nudge and gives the model
  one more tooled iteration to checkpoint its work before summarizing.
- **`install.ps1`** — saves the Windows binary with a `.exe` extension
  (removing any old extensionless file) so it no longer triggers the
  "How do you want to open this file" dialog.
- **TUI cursor alignment** — uses grapheme-aware display width (unicode-width
  + unicode-segmentation) instead of char count, fixing CJK/emoji/combining-mark
  drift at the root rather than patching symptoms.

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

[Unreleased]: https://github.com/raythurman2386/raven/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/raythurman2386/raven/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/raythurman2386/raven/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/raythurman2386/raven/compare/v0.1.10...v0.2.0
[0.1.10]: https://github.com/raythurman2386/raven/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/raythurman2386/raven/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/raythurman2386/raven/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/raythurman2386/raven/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/raythurman2386/raven/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/raythurman2386/raven/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/raythurman2386/raven/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/raythurman2386/raven/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/raythurman2386/raven/compare/v0.1.1...v0.1.2
