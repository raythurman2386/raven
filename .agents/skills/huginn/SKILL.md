---
name: huginn
description: "Use when developing the Huginn coding-agent harness — an opinionated personal coding-agent CLI in Rust for Linux/Pi (Ollama / OpenAI-compatible endpoints). Provides architecture, module map, conventions, verify commands, and the enforced-verification gate."
version: 1.1.0
author: Hermes Agent
license: MIT
metadata:
  hermes:
    tags: [rust, cli, agent, ollama, coding-agent, huginn]
    related_skills: []
---

# Huginn — Development Skill

Huginn is an opinionated, personal coding-agent harness written in Rust for
Linux and Raspberry Pi. It runs a streaming agent loop against a local (or
cloud) Ollama / OpenAI-compatible `/v1/chat/completions` endpoint, with tools,
plan mode, context compaction, and parallel sub-agents. This skill gives you
the full context to work on it correctly.

## When to Use

- Implementing or fixing issues in the huginn repo
- Adding or modifying tools in `crates/huginn-tools/` or confinement in `crates/huginn-sandbox/`
- Changing the agent loop, compaction, or context management
- Modifying the TUI, session persistence, or CLI
- Reviewing PRs to the repo

Don't use for: porting the Ravenwood palette to other apps — use the
`ravenwood-theme` porting skill instead.

## Architecture

```
CLI (src/main.rs)
  └─ Settings (huginn-config) ── named providers, context-window inference
  └─ Agent (huginn-agent)
       ├─ system prompt (stable instructions + mode) and a following setup message
       ├─ streaming loop ── POST /v1/chat/completions (Ollama / OpenRouter / …)
       ├─ compaction (huginn-agent context) ── estimate tokens, summarize middle
       ├─ tool dispatch (huginn-tools) ── mutators serial; others spawn_blocking
       └─ events (mpsc) ── TextDelta, ToolStart/End, Iteration, Compacted,
                           VerifyRequired, AskUser, PlanProgress, Done, Error
  └─ TUI (huginn-tui)  ── ratatui event loop, drains agent events
  └─ run_parallel ── N independent Agent tasks on git worktrees
```

One binary, `huginn`. Dependencies point only downward. Do not add a crate. `delegate_task` runs in `huginn-agent` (`tools_exec.rs`), not in `huginn-tools`.

### Module Layout

| Crate | Owns |
|------|------|
| `crates/huginn-proto` | `ChatMessage`, `ToolCall`, `FunctionCall`, `AgentEvent`, `AgentError` / `ToolError`, plan types, `TodoItem`, `Goal`. Depends on serde, thiserror, tokio, serde_json. |
| `crates/huginn-tokenizer` | Estimator, usage meters, pricing. Depends on proto. |
| `crates/huginn-config` | `Settings`, named providers, onboarding, Grok auth, MCP server specs. Depends on proto. |
| `crates/huginn-persist` | Session JSONL (atomic writes), session-scoped goal/todos, memory. Depends on proto, config. |
| `crates/huginn-sandbox` | Path confinement, shell, Landlock, seccomp, rlimits. Depends on proto. `landlock` and `seccompiler` are Linux target-cfg deps of this crate only. |
| `crates/huginn-tools` | Definitions, dispatch, git, patch, documents, validate, web, skills, repomap, plugins, stdio MCP client. Depends on proto, config, persist, sandbox. |
| `crates/huginn-agent` | Streaming loop (`core`, `stream`, `tools_exec`, `loop_control`, `parallel`), compaction (`context.rs`). Depends on proto, tokenizer, config, persist, tools. Re-exports the MCP client and plugin specs the TUI and ACP call. |
| `crates/huginn-tui` | Ratatui UI, markdown, slash commands. Depends on proto, config, persist, agent. ratatui, crossterm, and pulldown-cmark stay here. |
| `crates/huginn-acp` | ACP v1 stdio adapter (`huginn --acp`). Depends on proto, config, persist, agent. |
| `crates/huginn-testutil` | `outer_sandbox_restrictive`, `home_env_lock`. Depends only on libc. |
| root `huginn` | `src/main.rs` (clap), `src/runner.rs`, `src/update.rs`, and `src/lib.rs` facade re-exports. |

Root `src/<module>.rs` files other than `main.rs`, `runner.rs`, and `update.rs` are `pub use` facades. Public paths that benches and integration tests use stay: `huginn::agent::ChatMessage`, `huginn::tokenizer`, `huginn::context`, `huginn::repomap`, `huginn::testutil`. Session store: repo → `{workspace}/.huginn/sessions`, system → `~/.huginn/system/sessions`. Goal and todos: `.huginn/sessions/<id>/state/`; legacy `.huginn/state/` is read-only.

## Conventions

### Rust

- **Rust 2021 edition.** Target MSRV: 1.97 (pinned in `rust-toolchain.toml`).
- Keep the binary small and dependency-light. Thin stdio MCP client only
  (no marketplace, no HTTP/SSE). Agent Plugins `mcp.json` is stdio-only.
  No telemetry.
- Every public struct, enum, and fn should have a doc comment.
- `cargo doc --workspace --no-deps` must build with no warnings.
- `cargo build --workspace` must build with no warnings.
- Prefer `anyhow` for error handling in the binary; `thiserror` is available if a crate-level error type is needed.
- **DO NOT add comments unless asked** — keep code self-documenting.

### File Organization

- The root `src/lib.rs` declares the facade modules. Implementation lives in
  `crates/<name>/src`. Benches stay at the repo root and call the facades.
- Tests live in `#[cfg(test)]` modules next to the code they cover (agent
  tests under `crates/huginn-agent/src/tests/`), plus black-box tests in `tests/`.

## Build, Lint, Test

```bash
cargo build                    # debug build, must be warning-free
cargo build --release          # LTO + strip
cargo test --workspace         # every member, all offline (no Ollama needed)
cargo clippy --workspace --all-targets -- -D warnings   # must be zero warnings
cargo fmt --all --check        # formatting check
cargo doc --workspace --no-deps # docs must build with no warnings
cargo bench                    # Criterion benchmarks (tokenizer, context, repomap)
```

CI runs: `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo build --workspace --all-targets`, `cargo test --workspace`, `cargo doc --workspace --no-deps`, and
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

When implementing a fix, **always run the real verification commands**
(`cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`,
`cargo fmt --all`) — do not rely on the agent's simulated `run_tests` tool.

## Common Pitfalls

1. **Tools and sandbox are separate crates** — definitions and dispatch live
   in `crates/huginn-tools` (`definitions.rs`, `dispatch.rs`). OS confinement
   (Landlock / seccomp / rlimits) lives in `crates/huginn-sandbox`
   (`confinement.rs`). `landlock` and `seccompiler` stay on that crate.
   `huginn-tokenizer` does not depend on the sandbox. `delegate_task` stays
   in `huginn-agent`'s `tools_exec.rs`.
2. **Tokenizer is a fast estimator, not exact** — it over-estimates tiktoken counts by roughly 10–35% (mean ~28%), biased slightly high so compaction triggers early. It treats non-newline whitespace as free and applies a ~12% structural-overhead factor. Don't "fix" it to be exact without a real tokenizer to validate against.
3. **Compaction invariants** — `messages[0]` is always the system message;
   tool-call/tool-result pairs are never split. Don't break these.
4. **Sandbox** — all file paths are relative to the workspace root and confined
   to it. `run_shell` blocks dangerous patterns and strips known secret env
   vars. The blocklist is a denylist (best-effort), not an allowlist (issue #14).
   Landlock write roots are workspace + explicit extras + `/dev` — never the
   process temp dir. `TMPDIR` is pinned under `.huginn/tmp`.
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
   Resolution: `--provider` > `HUGINN_PROVIDER` > config `provider` > builtin
   `ollama`. `/provider` with no args lists names; unknown names must not
   silently become ollama. Session summary stores `model` only — resume can
   pair a stored OpenRouter model with the default Ollama provider.
10. **Headless plan revise** — after `[r]evise` + approve, execute the
    *revised* plan and *revised* message list (`run_plan_flow`), not the first
    proposal.
11. **Persistent state** — todos/goal live under `.huginn/sessions/<id>/state/`
    (`crates/huginn-persist/src/state.rs`); legacy `.huginn/state/` is read-only.
    Not a `static`. `todo_write`/`goal_set` persist; rebuild the system
    message at the start of every turn *and* after those tools. Nested
    `delegate_task` is depth-1 (`allow_delegate = false` on the child; no
    parent goal/todo overwrite). `think` is read-only and available in
    plan/chat.
12. **Compaction thrashing** — `Agent.compact_thrash_count` pauses auto-
    compaction after 3 consecutive no-reduction compactions; retry every 4th
    iteration so a later prune can resume.

## Verification Checklist

- [ ] `cargo build` succeeds with no warnings
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` passes (zero warnings)
- [ ] `cargo fmt --all --check` passes
- [ ] `cargo test --workspace` passes (all offline)
- [ ] `cargo doc --workspace --no-deps` builds with no warnings
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
