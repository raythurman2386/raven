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

## Done (≤ 0.2.6)

The mini-harness rework against Grok Build is complete. All phases below are
shipped and verified by `cargo test` (573+ offline tests), clippy `-D warnings`,
fmt, and `cargo check --target x86_64-pc-windows-gnu`.

### Agent loop & context
- **Streaming agent loop** — OpenAI-compatible `/v1/chat/completions`, incremental SSE parsing (UTF-8-safe across TCP chunks), non-streaming fallback (`--no-stream`).
- **Plan mode** — parse → approve → revise → execute, human-gated, read-only plan toolset (`src/plan.rs`, `src/agent/`).
- **Context management** — pure-Rust token estimator, context-window inference (live `/api/show` + name heuristic), automatic compaction (tool-result pruning + LLM-structured summary with extractive fallback), compaction thrashing protection.
- **Stall/verify recovery** — blank-response retry (capped), enforced-verify gate (must run tests after edits, `exit=0` exact), repeated-failure detection, goal-aware reflection reminders.
- **Long-horizon task management** — persistent goal + todos (`.raven/state/`), `delegate_task` (depth-1 sub-agents), `think` tool, goal-aware reminders.
- **Parallel sub-agents** — `--parallel` runs N focused agents concurrently.

### Tools (25)
`list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`,
`search_code`, `todo_write`, `goal_set`, `delegate_task`, `think`,
`memory_update`, `memory_search`, `git_status`, `git_diff`, `git_log`,
`git_commit`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`, `web_search`,
`web_fetch`, `skill_search`, `skill_load`.

- **Document extraction** — `read_file` converts `.docx`, `.pdf`, `.xlsx`, `.odt`, `.epub`, `.pptx`, `.csv`, `.rtf`, `.ods`, `.odp`, `.doc`, `.xls`, `.ppt` and more to Markdown (via the `anydoc` engine).
- **Web search + fetch** — keyless DuckDuckGo by default; optional self-hosted SearXNG backend with automatic DDG fallback.
- **Skills** — `SKILL.md` discovery over `.raven/skills/` + `~/.raven/skills/`, `skill_search`/`skill_load`.
- **Repo symbol map** — `<repo_map>` injected for large workspaces (≥15 files / ≥80 symbols), cached per workspace, off the hot path.
- **Memory** — `.raven/MEMORY.md` injected (first 25KB) + `memory_search` keyword recall.
- **Git** — `git_commit` (checkpoint mode excludes stray temp files + collateral deletions), `/undo` (`git reset --soft HEAD~1`), worktree isolation for sub-agents.

### Sandbox & safety
- **Path confinement** — `openat2`/`RESOLVE_BENEATH` on Linux (atomic, no TOCTOU); lexical `..` rejection + canonicalization elsewhere.
- **Landlock filesystem confinement** (Linux) — RW under workspace, RO for `/usr`/`/bin`/`/lib`/`/etc`/`/proc`/`$HOME`; `TMPDIR` pinned under `.raven/tmp` (closes the `/tmp` escape).
- **seccomp network block** (Linux) — denies `AF_INET`/`AF_INET6` sockets; sanctioned test runners exempted.
- **Resource limits** — `RLIMIT_CPU`/`RLIMIT_FSIZE`/`RLIMIT_NOFILE` (Linux + macOS); Windows Job Objects (process-tree + memory caps + kill-on-close).
- **Shell safety** — denylist + allowlist + direct-exec (no shell for safe single-binary commands), `confirm_shell` gate.

### Config, providers, sessions
- **Named providers** — `ollama`/`openrouter` presets + `[providers.<name>]` config; `--provider`/`/provider` switch endpoint + auth + default model as one unit. Removed legacy `--host`/`--api-key`/`RAVEN_HOST`/`OLLAMA_MODEL` surface.
- **Layered config** — CLI > env > `.env` > workspace `.raven/config.toml` > global `~/.raven/config.toml` > built-in defaults.
- **Session persistence** — atomic JSONL (`messages.jsonl` + `summary.json`), unique-tmp rename, flush-on-`/stop`/SIGINT, `--resume`/`--list-sessions`, local `debug-events.jsonl` + `last.patch` snapshots.
- **ACP v1 stdio** — `raven acp` for editor attachment (Zed etc.).

### TUI
- Streaming tail-patch, ravenwood theme, wrapped multi-line input, markdown rendering (headings/code/lists/tables/links), slash-command completion (`/provider`, `/model` with live endpoint discovery), abort/steer that can't leak a stale `Done` into the next turn.
- TUI polish pass (`docs/tui-polish.md`): tool calls as bordered blocks, code-block language labels, Home/End jump-to-top/live, context-sensitive keyhint footer, provider in the top bar, empty-state guidance + error recovery, prompt history recall (Up/Down), table-cell width cap, compact `key=value` tool-call args.

### Eval suite
- **Layer A** — offline fake-model harness (`cargo test eval_suite`) covering blank-stall, verify gate, sandbox escape, git-commit cleanliness, secrets-stay-uncommitted, goal persistence.
- **Layer B/C** — live fixtures (`evals/run.py`) + batch runner (`evals/run_all_models.py`); recommended-model table with Recommended / Passing / Partial / Flaky status.

---

## Next (polish — 0.3)

Release gate, not a feature roadmap. Check items off when the path feels
boringly reliable. See `docs/troubleshooting.md` for the operational notes.

- [ ] **A4** — Stream mid-response failure keeps partial assistant text + a one-line "stream interrupted — retry or `--no-stream`" hint. *(Implemented; verify in a live run.)*
- [ ] **A6** — `raven --help` / README install snippets match the current version and flags. *(README stale flags fixed; re-verify after each release.)*
- [ ] **F1** — ROADMAP stays in Done / Next / Non-goals shape; no "build X" fiction for shipped features. *(This rewrite.)*
- [ ] **F2** — Troubleshooting page covers Windows `.exe`, stream decode error, sandbox deny, SearXNG fallback, ACP one-liner. *(Added.)*
- [ ] **E3** — Re-run the eval suite after prompt/tool changes that could affect pass rate.
- [ ] **G3** — CHANGELOG **0.3.0** section written in user language (polish/fixes, not only internals).
- [ ] **G4** — Run Raven on a real repo for a full session without a "I don't trust this" moment.

---

## Non-goals (explicit, YAGNI for a mini harness)

These are real grok-build features but would bloat raven past "mini". Not
planned unless a checklist item forces a small split.

- In-tree CDP / Browser Use
- MCP marketplace (ACP clients may still attach)
- Full VM / container isolation (run Raven inside a container yourself if you need it)
- NFS-safe SQLite journaling — raven is local-only
- sqlite-vec embeddings / MMR / dream consolidation — keyword recall covers the need
- Rhai workflow engine + deterministic journal — over-engineered for a per-turn agent
- Background task pool / kill-task supervisor — `spawn_blocking` + `/stop` covers the mini case
- Full official `agent-client-protocol` crate / MCP-over-ACP — Raven ships a thin v1 stdio adapter (`raven acp`) instead
- Large TUI redesign unless a checklist item requires a small split
- New protocols beyond fixing ACP if something is broken

---

## Cross-cutting: performance budget

Every new tool and every phase must re-run the phase-timing live check:
`RUST_LOG=info` against a cloud model must show `pre_http_ms < 100` on a
reasonable history. If a phase regresses the hot path (per-turn tokenization,
map gen, memory scan), gate the new work behind the size checks and re-profile.
Grok-build's own sampler reuses one pooled client — raven's per-`Agent` client
is already correct (one per turn, never per-iteration).

## Execution order & gating

1. Do checklist items in order — each is independently committable and testable.
2. After each item: `cargo test` + clippy + fmt + `cargo install --path . --force`, then a **live run** against `deepseek-v4-flash:cloud` / `glm-5.2:cloud`.
3. Update this ROADMAP.md with ✅ when an item ships.
4. Keep the `rust-llm-agent-harness` skill in lockstep (patch after each item).
