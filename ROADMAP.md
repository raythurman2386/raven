# Raven — Best Mini Coding-Agent Harness Roadmap

**Goal:** Rework every component of `raven` so it is as helpful and fast as
Grok Build, while remaining a lean single-crate *mini* harness (not a 60-crate
monorepo). Design decisions are grounded in `xai-org/grok-build` source and in
cross-agent research (Aider/Goose/Cline/Continue).

**Guiding principles:**
- Zero dead code, zero `unreachable!()`, comprehensive offline tests. `unsafe` is allowed only where the platform requires it (Linux `pre_exec`, Windows Job Object FFI), each block carrying a sound SAFETY comment.
- Clean full transitions — never leave fallback flags or conditional cruft behind.
- Focused conventional commits (`feat:`/`fix:`/`perf:`/`chore:`), one per logical change.
- Every change: `cargo test` → `cargo clippy --all-targets -- -D warnings` → `cargo fmt`, then `cargo install --path . --force`.
- `mini` means: borrow grok-build's *patterns*, not its monorepo sprawl.

**Reference roots (read before each phase):**
- grok-build: `crates/codegen/`
  - tools: `xai-grok-tools/src/implementations/`
  - memory: `xai-grok-memory/src/`
  - agent/session: `xai-grok-agent/`, `xai-chat-state/`
  - codebase graph: `xai-codebase-graph/`
  - sampler/http: `xai-grok-sampler/src/shared_http.rs`
- skill refs: `rust-llm-agent-harness` → `references/coding-agent-architecture-research.md`, `references/grok-build-architecture.md`, `references/claude-code-architecture.md`.

---

## Current State (audited 2026-08-09)

**Tools (22):** list_dir, read_file, search_replace, write_file, grep, run_shell,
search_code, todo_write, memory_update, memory_search, git_status, git_diff,
git_log, git_commit, apply_patch, run_tests, run_lint, ask_user, web_search,
web_fetch, skill_search, skill_load.
(`src/tools/` module: definitions.rs, dispatch.rs, document.rs, git.rs, mod.rs, patch.rs, sandbox.rs, windows.rs)

**Already production-grade (done):**
- Linear token estimator (`src/tokenizer.rs`) — O(n), was O(n²) perf blocker.
- Phase-timing telemetry (`RUST_LOG=info`: pre_http_ms / send_http_ms / stream_ms).
- Plan mode with human-gated approval + read-only plan toolset (`src/plan.rs`, `src/agent/`).
- `/stop` + mid-task steering, `/model`, slash-command registry (`src/commands.rs`).
- TUI: streaming tail-patch, ravenwood theme, wrapped multi-line input, markdown rendering (`src/tui/`).
- Session persistence (atomic, `messages.jsonl` + `summary.json`), sub-agent `run_parallel`.
- Compaction: tool-result pruning + truncated-excerpt summary.

---

## Phase 1 — Permission Gate + AskUser tool  (grok-build: `PermissionRule`, `ask_user_question/`)

**Why:** grok-build defaults permission to **Deny** (`CWE-1188`). Raven's only
guard is a regex on `run_shell`. The biggest helpfulness gap is that the model
cannot ask the user anything mid-task — grok-build's `AskUserQuestion` tool
blocks on a oneshot and returns the typed answer. This is the #1 way to make
raven feel like a real agent.

### 1.1 Permission rule model
- `src/tools.rs`: add a `PermissionRule { allow_read, allow_write, allow_shell }`
  model (default read/write allow, shell **ask**), mirroring grok-build's
  `RuleAction`/`ToolFilter`. Keep it a plain struct — no macro crate.
- Wire: `run_shell` returns a sentinel `PermissionRequired` when the user has
  not enabled `--yolo`; surface it as a real prompt in the TUI/headless.
- **Reference:** `xai-grok-sandbox/src/` + `xai-grok-tools/.../ask_user_question/types.rs`.

### 1.2 AskUser tool
- New tool `ask_user` (schema + dispatch). Blocking over a `oneshot::channel`:
  agent sends `AskUser { question, options }` event → TUI/headless renders a
  prompt → user answers → result returned to the tool dispatch.
- Add `AgentEvent::AskUser { ... }` and handle it in both TUI and headless.
- **Reference:** grok-build `ask_user_question/` (blocking flow, `QuestionAnnotation`).

**Acceptance:** `cargo test` (offline: dispatch returns blocked result when no
consumer), live test where the model calls `ask_user` and the TUI prompts.
**Commit:** `feat(tools): add ask_user tool + permission gate`.

---

## Phase 2 — Web search + fetch tools  (grok-build: `web_search/`, `web_fetch`)

**Why:** grok-build has `WebSearch`/`WebFetch` in its ToolKind taxonomy; without
them raven can't research anything current. For a "helpful" harness this is
foundational.

- New tools: `web_search(query)` and `web_fetch(url)`, both read-only, both in
  the plan toolset. No new dependency needed if we use the already-present
  `reqwest` + a public search/fetch API; keep results capped at `MAX_TOOL_OUTPUT`.
- **Reference:** `xai-grok-tools/src/implementations/web_search/`.
- Sandbox: `web_fetch` must stay read-only and never execute downloaded content.

**Acceptance:** offline tests for URL/scheme validation + output capping; live
test `-p "search for the current rust edition"`.
**Commit:** `feat(tools): add web_search and web_fetch`.

---

## Phase 3 — Skills system  (grok-build: `xai-grok-tools/.../skills/`)

**Why:** grok-build auto-discovers `SKILL.md` files and injects them on demand.
Raven's harness already lives inside a skill ecosystem — giving the *agent*
the same primitive makes it self-educating.

- Add `Skill { name, path, description }` discovery over `.raven/skills/` and
  `~/.raven/skills/`; a `skill_search(query)` tool returns matching skill names.
- On a requested skill, inject its `SKILL.md` content into the system prompt
  for that turn (capped, e.g. 8K chars — mirror `load_agents_md`).
- **Reference:** `skills/discovery.rs`, `skills/skill.rs`.

**Acceptance:** offline tests for discovery + cap; live test the agent finds and
uses a skill in `.raven/skills/`.
**Commit:** `feat(agent): add skills discovery + skill_search tool`.

---

## Phase 4 — Repo map / codebase graph  (grok-build: `xai-codebase-graph/`; Aider research)

**Why:** Aider's research calls repo map the **#1 differentiator** for
large-codebase work. Without it the agent is blind to structure and burns turns
listing/reading. grok-build has a full `xai-codebase-graph` crate; the mini
version is a lightweight symbol index, not tree-sitter+PageRank.

- Build a `repo_map.rs`: walk the workspace, extract function/struct/enum/impl
  names via the already-present `regex` (line-anchored, no new dep), emit a
  compact `<symbol> path:line` tree capped at ~2K tokens.
- Inject as a `<repo_map>` block in the system prompt when the workspace is
  large (> ~50 files); skip for small workspaces (keep it cheap).
- Invalidate on `git`/file changes — simplest: regenerate per turn, only for
  large workspaces (map gen is O(files), cheap enough).

**Acceptance:** offline test asserts symbol extraction + cap; live test the
agent references a symbol it could only know from the map.
**Commit:** `feat(agent): inject a lightweight repo symbol map`.

---

## Phase 5 — Memory upgrade: file index + search  (grok-build: `xai-grok-memory/`)

**Why:** raven's memory is a single `MEMORY.md` (append-only, no recall beyond
the 25KB inject). grok-build has SQLite FTS5+vec hybrid search. The mini
version drops the vector store and uses **file-based memory chunks + keyword
recall** — no new deps, zero unsafe.

- Keep `MEMORY.md` as the curated inject (already works). ADD a
  `memory_search(query)` tool that keyword-scans `.raven/memory/*.md` (session
  logs + knowledge) and returns ranked snippets.
- Persist per-session logs: on turn end, append a dated markdown file to
  `.raven/memory/sessions/` (mirror grok-build's session-log layout).
- **Reference:** `xai-grok-memory/src/{index,storage,chunker,search}.rs` (FTS5 →
  our keyword scan; skip vec/embeddings — YAGNI for a mini harness).

**Acceptance:** offline test for chunk-write + keyword recall; live test the
agent recalls a fact stored in a prior session.
**Commit:** `feat(memory): add file-based session logs + memory_search tool`.

---

## Phase 6 — Git auto-commit + `/undo`  (Aider research: "cheapest reliable undo")

**Why:** Aider's `commit_before_message` + `/undo` is the cheapest reliable
rollback. grok-build leans on git for worktree/hunk tracking. Raven already has
`git_status`/`diff`/`log` read tools — add the write side.

- `commit_before_message`: at the start of each execution turn, record `HEAD`
  SHA (already have `git_log` plumbing). Provide a `git_commit(message)`
  tool the model can call after edits, and a `/undo` slash command that
  `git reset --hard` back to the recorded SHA.
- **Reference:** Aider `commit_before_message` + `/undo` (in
  `coding-agent-architecture-research.md`).

**Acceptance:** offline test that `/undo` computes the right reset target.
**Commit:** `feat(git): add git_commit tool + /undo via HEAD tracking`.

---

## Phase 7 — LLM-based structured compaction  (Goose research; grok-build `xai-grok-compaction`)

**Why:** raven's compaction is truncated excerpts. Both Aider and Goose use
**LLM summaries** (Goose's `StructuredSummary`: user_intent, files,
errors_and_fixes, pending_tasks, current_work, next_step) with truncated-excerpt
fallback. grok-build has three compaction styles + a wall-clock budget.

- In `src/context.rs`, replace the excerpt-only `build_summary_user` with a
  **call to the model** producing Goose-style JSON; keep the current excerpt
  logic as the offline/fallback path.
- Add a `wall_clock_budget_secs` (default 300) so a hung summarize call can't
  stall the loop (reuse `send_with_retry`).
- **Reference:** `xai-grok-compaction/` + `CompactionPolicy`; Goose
  `StructuredSummary`.

**Acceptance:** offline test that fallback still compacts; live test a long
session compacts with a coherent structured summary.
**Commit:** `feat(context): LLM-structured compaction with excerpt fallback`.

---

## Phase 8 — Auto-lint + reflection after edits  (cross-agent research, Tier 2)

**Why:** the fastest loop feeds the model its own failures. grok-build runs
hooks; the research recommends auto-lint + reflection messages.

- After a turn that edited files (`write_file`/`search_replace`/`apply_patch`),
  if the project has `cargo`/`npm`/`pytest`, run the linter and feed errors
  back as a `system` reminder in the **next** request (never into
  `self.messages` — reuse the existing ephemeral-reminder mechanism).
- Add a `run_lint` tool (auto-detect like `run_tests`).

**Acceptance:** offline test `run_lint` detection; live test edits trigger a
lint pass.
**Commit:** `feat(tools): add run_lint + auto-lint reflection on edits`.

---

## Phase 9 — Session-state actor + durability polish  (grok-build: `ChatStateActor`, `xai-chat-state/`)

**Why:** grok-build owns conversation state in a tokio actor (no locks,
message-passing) and writes atomically. Raven is a single-threaded TUI with a
task handle. This phase hardens durability + concurrency only where it matters
for the mini harness — it does NOT port the full actor.

- Add `fsync`/atomic-rename to `session.rs` (currently `write_atomic` uses a
  fixed `.tmp` name — make it unique `.{uuid}.tmp` to avoid races).
- Add a `flush` on `/stop` and on SIGINT so an interrupted turn still persists
  what it wrote (currently abort drops partial `session_messages`).
- **Reference:** `xai-chat-state/src/` (`ChatPersistence` trait), `updates.jsonl`
  vs `chat_history.jsonl` durable-vs-cache split.

**Acceptance:** offline test unique-tmp rename; crash-sim test.
**Commit:** `fix(session): unique tmp + flush-on-stop`.

---

## Deferred (documented, NOT built — YAGNI for a mini harness)

These are real grok-build features but would bloat raven past "mini":

- NFS-safe SQLite journaling (`xai-sqlite-journal`) — raven is local-only.
- sqlite-vec embeddings / MMR / dream consolidation (`xai-grok-memory`) — needs
  a DB + embedding provider; Phase 5 keyword recall covers the need.
- Rhai workflow engine + deterministic journal (`xai-workflow`) — over-engineered
  for a per-turn agent.
- Background task pool / kill-task supervisor — `spawn_blocking` + `/stop`
  covers the mini case.
- Full official `agent-client-protocol` crate / MCP-over-ACP — Raven
  ships a thin v1 stdio adapter (`raven acp`) instead: no MCP, no
  client FS/terminal. Enough for editor attachment.

---

## Cross-cutting: performance budget (keep the tokenizer fix honest)

Every new tool and every phase must re-run the phase-timing live check:
`RUST_LOG=info` against a cloud model must show `pre_http_ms < 100` on a
reasonable history. If a phase regresses the hot path (per-turn tokenization,
map gen, memory scan), gate the new work behind the size checks in Phase 4 and
re-profile. Grok-build's own sampler reuses one pooled client
(`xai-grok-sampler/src/shared_http.rs`) — raven's per-`Agent` client is already
correct (one per turn, never per-iteration).

---

## Execution order & gating

1. Do phases **in order** — each is independently committable and testable.
2. After each phase: `cargo test` + clippy + fmt + `cargo install --path . --force`,
   then a **live run** against `deepseek-v4-flash:cloud` / `glm-5.2:cloud`.
3. Update this ROADMAP.md with ✅ when a phase ships.
4. Keep the `rust-llm-agent-harness` skill in lockstep (patch after each phase).

**Phase status:**
- [x] Phase 1 — Permission gate + AskUser
- [x] Phase 2 — Web search + fetch
- [x] Phase 3 — Skills system
- [x] Phase 4 — Repo symbol map
- [x] Phase 5 — Memory file index + recall
- [x] Phase 6 — Git auto-commit + /undo
- [x] Phase 7 — LLM structured compaction
- [x] Phase 8 — Auto-lint + reflection
- [x] Phase 9 — Session durability + flush-on-stop

**All 9 phases complete.** The mini-harness rework against Grok Build is done;
remaining ideas are documented under "Deferred (YAGNI)" above.

---

## Phase 10 — Document extraction in `read_file`  (Hermes Agent parity)

**Why:** Hermes Agent's `read_file` auto-extracts non-text documents (`.docx`,
`.pdf`, `.xlsx`, `.odt`, `.epub`, ...) to Markdown so the model can read them
instead of hitting a binary blob. Raven's `read_file` previously only did
`read_to_string`, so any document was unreadable.

- New `src/tools/document.rs`: binary-extension guard + `is_extractable_document`
  + `extract_document_text`, delegating conversion to the **`anydoc` crate**
  (v0.1.6, MIT, zero external deps) — the same Rust core Hermes uses through its
  `firecrawl-anydoc` binding. Runs entirely locally (no API key, no network).
- Wired into `Sandbox::read_file` (mirrors Hermes ordering): try extraction
  before the binary guard, so `.docx`/`.xlsx`/`.pdf` render as text; malformed
  documents fall through to the normal text/binary handling.
- Bumped `rust-version` 1.85 → 1.88 (anydoc's MSRV; toolchain is 1.97).
- Added `zip` as a dev-dependency for the offline DOCX round-trip test.

**Acceptance:** `cargo test` (10 new offline tests: 8 document-module incl. a
real DOCX round-trip, plus 2 end-to-end `read_file` tests), clippy `-D warnings`,
fmt clean.
**Commit:** `feat(tools): add document extraction to read_file via anydoc`.

**Phase status:**
- [x] Phase 10 — Document extraction in `read_file`

---

## Phase 11 — Named providers (Ollama / OpenRouter)

**Why:** switching between local (Ollama) and cloud (OpenRouter / Ollama Cloud)
was three independent knobs (`--model` + `--host` + `--api-key`), and `/model`
couldn't move you to a different host mid-session. A named-provider abstraction
bundles endpoint + auth + default model into one switchable unit, mirroring
Grok Build / Hermes / Opencode.

- New `Provider` struct (`src/config/mod.rs`) bundling `{ name, base_url, api_key,
  default_model }`, with built-in `ollama` and `openrouter` presets.
- `[providers.<name>]` config table + `provider` selection key; `resolve_provider`
  merges builtin + config + env (`--provider` > `RAVEN_PROVIDER` > config > builtin).
- `Settings` now holds a resolved `Provider`; `model` is a per-session override
  layered on the provider's `default_model`.
- `/provider <name>` slash command switches provider at runtime (re-resolves
  model + context window); `/model` switches within the current provider.
- `fetch_context_window` threaded through the resolved provider (uses its key).
- **Removed** (clean transition): `--host` / `--api-key` flags and the legacy
  `RAVEN_HOST` / `OLLAMA_HOST` / `RAVEN_MODEL` / `OLLAMA_MODEL` env vars.
  Key resolution is now provider-scoped: `RAVEN_API_KEY` (universal) →
  `OPENROUTER_API_KEY` / `OLLAMA_API_KEY`.

**Acceptance:** `cargo test` (506 tests incl. new provider + `/provider` tests),
clippy `-D warnings`, fmt clean, `cargo check --target x86_64-pc-windows-gnu`.
**Commit:** `feat(config): add named providers (ollama/openrouter)`.

**Phase status:**
- [x] Phase 11 — Named providers (Ollama / OpenRouter)

---

## Flake fixes (found while verifying Phase 10)

Two pre-existing flaky tests surfaced during Phase 10 verification:

1. **`wait_for_child_times_out`** — `wait_for_child` compared `start.elapsed().as_secs() > timeout_secs`, but `as_secs()` truncates to whole seconds, so a 1s timeout actually fired at ~2s. Under parallel load the extra second could push past the test's 4s bound. Fixed to compare `Duration` precisely (`start.elapsed() >= deadline`).

2. **`verify_*` agent tests** — the mock HTTP server accepted one connection per response and dropped the stream. The agent uses a shared `reqwest::Client` that reuses one TCP connection, so a request sent on a reused connection sat unread while the mock blocked on `accept()` for a new connection that never came → the agent hung → timed out → ended with `Error` instead of `Done`. Rewrote the mock (`serve_mock`) to be keep-alive aware: read multiple requests per connection, serve the next scripted response each time, and fall back to a benign empty response once scripted responses are exhausted.

**Acceptance:** 30 consecutive full-suite runs with 0 failures (previously ~3/25), clippy `-D warnings`, fmt clean.
**Commit:** `fix(tests): make mock server keep-alive aware + precise timeout`.

---

## Phase 12 — Long-horizon task management  (Claude Code / Grok Build research)

**Why:** raven's todo state was an in-memory `static` (lost across turns and
sessions), there was no persistent goal, no model-spawnable sub-agent, no
`think` tool, no compaction thrashing protection, and no goal-aware reflection.
These are the mechanisms the research identifies for keeping an agent on track
over long tasks with clean changes.

### 12.1 Persistent todo + goal state  (`src/state.rs`)
- New `src/state.rs`: disk-backed state under `.raven/state/` — `todos.json`
  (`Vec<TodoItem>`) + `goal.json` (`Goal`), atomic writes, loaded lazily.
- Replaced the `static TODO_STATE` in `tools/mod.rs` with per-workspace
  persistence; `todo_write` now takes the workspace and persists.
- New `goal_set(description, status)` tool (schema + dispatch + `is_write_tool`).
- Injected `<Current goal>` + `<Task list>` into the system prompt each turn
  (`build_system_message`), so the objective survives compaction and sessions.

### 12.2 `delegate_task` sub-agent tool
- New `delegate_task(description)` tool: spawns a focused sub-agent in a fresh
  context window and returns a distilled (capped 2K) summary, keeping the main
  context clean (Claude Code subagent pattern).
- Depth 1 only: the child sets `allow_delegate = false` so it cannot nest
  another delegate or overwrite the parent goal/todos. Shares the workspace
  (no worktree). `tokio::join!` + `Box::pin` breaks the recursive-async cycle.

### 12.3 `think` tool
- New `think(thought)` tool — appends a thought to the log, returns nothing.
  Read-only; available in plan/chat toolsets. (Claude Code: 54% τ-Bench gain.)

### 12.4 Compaction thrashing protection
- `Agent.compact_thrash_count`: when compaction fails to reduce the history
  (`after >= before`, context refills immediately), count toward a cap (3);
  past the cap, auto-compaction is paused so the loop doesn't spin on repeated
  summarize calls (Claude Code thrashing protection).

### 12.5 Goal-aware reflection
- `compute_reminders` now takes the goal + todos and, from iteration 4, injects
  a re-anchor reminder restating the goal and the next pending task (Goose
  `next_step` carry-over).

**Acceptance:** `cargo test` (570+ tests incl. state round-trips, thrashing cap,
goal-aware reminders, eval_suite long-horizon cases), clippy `-D warnings`, fmt clean. Live run: set a goal,
run a long session, resume, confirm goal+todos persist and the agent re-anchors.
**Commit:** `feat(agent): long-horizon task management (persistent state, delegate_task, think, thrash guard, goal reflection)`.

**Phase status:**
- [x] Phase 12 — Long-horizon task management
