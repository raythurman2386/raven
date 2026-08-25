---
name: raven
description: "Use when developing the Raven coding-agent harness — a privacy-first local coding-agent CLI in Rust for Ollama / OpenAI-compatible endpoints. Provides architecture, module map, conventions, verify commands, and the enforced-verification gate."
version: 1.1.0
author: Hermes Agent
license: MIT
metadata:
  hermes:
    tags: [rust, cli, agent, ollama, coding-agent, raven]
    related_skills: []
---

# Raven — Development Skill

Raven is a privacy-first local coding-agent harness written in Rust. It runs a
streaming agent loop against a local (or cloud) Ollama / OpenAI-compatible
`/v1/chat/completions` endpoint, with tools, plan mode, context compaction, and
parallel sub-agents. This skill gives you the full context to work on it
correctly.

## When to Use

- Implementing or fixing issues in the raven repo
- Adding or modifying tools in `src/tools/`
- Changing the agent loop, compaction, or context management
- Modifying the TUI, session persistence, or CLI
- Reviewing PRs to the repo

Don't use for: porting the Ravenwood palette to other apps — use the
`ravenwood-theme` porting skill instead.

## Architecture

```
CLI (main.rs)
  └─ Settings (config/mod.rs) ── named providers, context-window inference
  └─ Agent (src/agent/)
       ├─ system prompt (SYSTEM_BASE + AGENTS.md + repo map + --rules)
       ├─ streaming loop ── POST /v1/chat/completions (Ollama / OpenRouter / …)
       ├─ compaction (context.rs) ── estimate tokens, summarize middle
       ├─ tool dispatch (tools/) ── mutators serial; others spawn_blocking
       └─ events (mpsc) ── TextDelta, ToolStart/End, Iteration, Compacted,
                           VerifyRequired, AskUser, PlanProgress, Done, Error
  └─ TUI (tui/)  ── ratatui event loop, drains agent events
  └─ run_parallel ── N independent Agent tasks on git worktrees
```

### Module Layout

| Path | Purpose |
|------|---------|
| `src/main.rs` | CLI entry, headless runner, session management |
| `src/lib.rs` | Library crate re-exports for benchmarks/integration tests |
| `src/agent/` | Streaming loop (`core.rs`), stream parse, tool exec, loop control, parallel sub-agents |
| `src/commands.rs` | Slash-command registry + parsing for the TUI |
| `src/config/mod.rs` | `Settings`, config.toml, AGENTS.md loader; `provider.rs` holds `Provider`/resolution |
| `src/context.rs` | Token estimation, compaction strategy, tool-result pruning |
| `src/error.rs` | Typed `AgentError` / `ToolError` |
| `src/memory.rs` | Cross-session project memory (`.raven/MEMORY.md`) |
| `src/plan.rs` | Structured plan mode, `parse_plan`, `format_plan` |
| `src/repomap/mod.rs` | Cached, walk-capped repo symbol map (`<repo_map>`); `patterns.rs` holds the language regexes |
| `src/runner.rs` | Shared event-draining and plan-approval flow |
| `src/session.rs` | JSONL session persistence, resume, list (atomic writes) |
| `src/skills.rs` | `SKILL.md` discovery + `skill_search`/`skill_load` |
| `src/state.rs` | Persistent agent state — `.raven/state/todos.json` + `goal.json`, injected into the system prompt |
| `src/tokenizer.rs` | Pure-Rust BPE token estimator (no external vocab) |
| `src/tools/` | 24 tools + sandbox — `mod.rs`, `definitions.rs`, `dispatch.rs`, `sandbox/` (`mod.rs` + `confinement.rs`), `document.rs`, `git.rs`, `patch.rs`, `windows.rs` |
| `src/tui/` | ratatui TUI with status bar, streaming, scrollback, /commands |
| `src/web.rs` | Web tools (`web_search`, `web_fetch`) |
| `src/acp/` | ACP v1 stdio adapter (`raven acp`) — protocol + server, no MCP |

## Conventions

### Rust

- **Rust 2021 edition.** Target MSRV: 1.97 (pinned in `rust-toolchain.toml`).
- Keep the binary small and dependency-light. No MCP, no telemetry.
- Every public struct, enum, and fn should have a doc comment.
- `cargo doc --no-deps` must build with no warnings.
- `cargo build` must build with no warnings.
- Prefer `anyhow` for error handling in the binary; `thiserror` is available if a crate-level error type is needed.
- **DO NOT add comments unless asked** — keep code self-documenting.

### File Organization

- Modules are declared in `src/lib.rs` and consumed by `src/main.rs` (so
  benchmarks and integration tests can import them).
- Tests live in `#[cfg(test)] mod tests` blocks at the bottom of each source
  file (agent tests under `src/agent/tests/`), plus black-box tests in `tests/`.

## Build, Lint, Test

```bash
cargo build                    # debug build, must be warning-free
cargo build --release          # LTO + strip
cargo test                     # 661 tests, all offline (no Ollama needed)
cargo clippy --all-targets -- -D warnings   # must be zero warnings
cargo fmt --all --check        # formatting check
cargo doc --no-deps            # docs must build with no warnings
cargo bench                    # Criterion benchmarks (tokenizer, context, repomap)
```

CI runs: `cargo fmt --all --check`, `cargo clippy --all-targets -- -D warnings`,
`cargo build --all-targets`, `cargo test`, `cargo doc --no-deps`, and
`cargo audit`. **All of these must pass before a PR is mergeable.**

## The Enforced-Verify Gate

The agent has a built-in verification gate (default on, `--no-verify` to
disable). When the model edits files in a turn but does **not** call `run_tests`
before finishing, the harness injects a recovery reminder and re-runs the turn
(capped at 3 attempts). This is the project's core "verifiable changes" contract:

- `AgentEvent::VerifyRequired` is emitted when the gate re-runs.
- The gate is skipped when the workspace has no detectable test runner
  (`has_test_runner`: `Cargo.toml`; `package.json` **and** `node_modules`;
  Python only if `pytest` is spawnable).
- Credit is fail-closed: `run_tests` **or** `run_shell` with
  `is_verification_command` counts, and only when `parse_exit_code` is `0`
  (do not use `contains("exit=0")` — `exit=10` must fail).
- Failed / sandbox-rejected edits must not arm the gate.

When implementing a fix, **always run the real verification commands** (`cargo
test`, `cargo clippy`, `cargo fmt`) — do not rely on the agent's simulated
`run_tests` tool.

## Common Pitfalls

1. **`tools/` directory** — the tool module was split from a single `tools.rs`
   into `tools/mod.rs`, `definitions.rs`, `dispatch.rs`, `sandbox/`
   (`mod.rs` + `confinement.rs`), `document.rs`, `git.rs`, `patch.rs`. When
   adding a tool, keep the existing structure; definitions go in
   `definitions.rs`, dispatch in `dispatch.rs`. OS confinement (Landlock /
   seccomp / rlimits / Job Objects) lives in `sandbox/confinement.rs`.
2. **Tokenizer is a fast estimator, not exact** — it over-estimates tiktoken counts by roughly 10–35% (mean ~28%), biased slightly high so compaction triggers early. It treats non-newline whitespace as free and applies a ~12% structural-overhead factor. Don't "fix" it to be exact without a real tokenizer to validate against.
3. **Compaction invariants** — `messages[0]` is always the system message;
   tool-call/tool-result pairs are never split. Don't break these.
4. **Sandbox** — all file paths are relative to the workspace root and confined
   to it. `run_shell` blocks dangerous patterns and strips known secret env
   vars. The blocklist is a denylist (best-effort), not an allowlist (issue #14).
   Landlock write roots are workspace + explicit extras + `/dev` — never the
   process temp dir. `TMPDIR` is pinned under `.raven/tmp`.
5. **Session IDs** — `{iso}-{pid}-{counter}` (issue #10 is fixed). Still
   don't invent a different scheme without updating `generate_session_id`.
6. **Repo map is cached** per workspace path until `repomap::invalidate`
   (called when `repo_map_stale` after a successful file edit). Discovery
   prefers `git ls-files --exclude-standard`, else an `ignore`-crate walk;
   both honor `.gitignore` and hard `SKIP_DIRS`. Candidates are path-scored
   before the extract budget (`MAX_WALK_DEPTH` / `MAX_SOURCE_FILES_SCANNED`).
   Cache key is the raw `Path`, not canonical — keep `settings.workspace`
   consistent.
7. **Session writes are atomic** (`write_atomic` temp+rename) for both
   `append_message` and `save_all_messages`. Do not reintroduce in-place
   truncate.
8. **TUI `Agent::new` is off-thread** — never construct an `Agent` on the
   event-loop thread. Each turn owns its own `mpsc` pair (`begin_agent_turn`).
   Interrupt/`/stop`/`/new` must `abort_current_turn` (abort handle **and**
   drop `event_rx`) so a leftover `Done` cannot join the next handle. Do not
   share one session-long channel.
9. **Named providers** — `Provider` bundles endpoint + auth + default model.
   Resolution: `--provider` > `RAVEN_PROVIDER` > config `provider` > builtin
   `ollama`. `/provider` with no args lists names; unknown names must not
   silently become ollama. Session summary stores `model` only — resume can
   pair a stored OpenRouter model with the default Ollama provider.
10. **Headless plan revise** — after `[r]evise` + approve, execute the
    *revised* plan and *revised* message list (`run_plan_flow`), not the first
    proposal.
11. **Persistent state** — todos/goal live in `.raven/state/` (`src/state.rs`),
    not a `static`. `todo_write`/`goal_set` persist; rebuild the system
    message at the start of every turn *and* after those tools. Nested
    `delegate_task` is depth-1 (`allow_delegate = false` on the child; no
    parent goal/todo overwrite). `think` is read-only and available in
    plan/chat.
12. **Compaction thrashing** — `Agent.compact_thrash_count` pauses auto-
    compaction after 3 consecutive no-reduction compactions; retry every 4th
    iteration so a later prune can resume.

## Verification Checklist

- [ ] `cargo build` succeeds with no warnings
- [ ] `cargo clippy --all-targets -- -D warnings` passes (zero warnings)
- [ ] `cargo fmt --all --check` passes
- [ ] `cargo test` passes (all offline)
- [ ] `cargo doc --no-deps` builds with no warnings
- [ ] New public items have doc comments
- [ ] New logic has unit tests in the module's `#[cfg(test)]` block
- [ ] No new dependencies unless justified (keep the binary small)

## Related Files

- `README.md` — User-facing documentation, feature list, quick start
- `ROADMAP.md` — Current state, planned work, tool count
- `docs/architecture.md` — Detailed design walkthrough
- `docs/tools.md` — Tool contracts and sandbox rules
- `docs/testing.md` — Test structure, coverage, mutation testing
- `docs/contributing.md` — Build, style, and layout
