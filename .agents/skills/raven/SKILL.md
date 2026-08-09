---
name: raven
description: "Use when developing the Raven coding-agent harness — a privacy-first local coding-agent CLI in Rust for Ollama / OpenAI-compatible endpoints. Provides architecture, module map, conventions, verify commands, and the enforced-verification gate."
version: 1.0.0
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
  └─ Settings (config.rs) ── context window inference, defaults
  └─ Agent (agent.rs)
       ├─ system prompt (SYSTEM_BASE + AGENTS.md + --rules)
       ├─ streaming loop ── POST /v1/chat/completions (Ollama)
       ├─ compaction (context.rs) ── estimate tokens, summarize middle
       ├─ tool dispatch (tools/) ── parallel via spawn_blocking
       └─ events (mpsc) ── TextDelta, ToolStart/End, Iteration, Compacted, Done, Error
  └─ TUI (tui/)  ── ratatui event loop, drains agent events
  └─ run_parallel ── N independent Agent tasks on tokio tasks
```

### Module Layout

| Path | Purpose |
|------|---------|
| `src/main.rs` | CLI entry, headless runner, session management |
| `src/lib.rs` | Library crate re-exports for benchmarks/integration tests |
| `src/agent.rs` | Streaming agent loop, `AgentEvent`, parallel sub-agents, enforced-verify gate |
| `src/commands.rs` | Slash-command registry + parsing for the TUI |
| `src/config.rs` | `Settings`, config.toml loading, context-window inference, AGENTS.md loader |
| `src/context.rs` | Token estimation, compaction strategy, tool-result pruning |
| `src/error.rs` | Typed `AgentError` enum with retry classification |
| `src/memory.rs` | Cross-session project memory (`.raven/MEMORY.md`) |
| `src/plan.rs` | Structured plan mode, `parse_plan`, `format_plan` |
| `src/repomap.rs` | Lightweight repo symbol map (`<repo_map>` for large workspaces) |
| `src/runner.rs` | Shared event-draining and plan-approval flow |
| `src/session.rs` | JSONL session persistence, resume, list |
| `src/skills.rs` | `SKILL.md` discovery + `skill_search`/`skill_load` |
| `src/tokenizer.rs` | Pure-Rust BPE token estimator (no external vocab) |
| `src/tools/` | 22 tools + workspace sandbox (path confinement, shell filter) — split into `mod.rs`, `definitions.rs`, `dispatch.rs`, `sandbox.rs`, `document.rs`, `git.rs`, `patch.rs` |
| `src/tui/` | ratatui TUI with status bar, streaming, scrollback, /commands |
| `src/web.rs` | Web tools (`web_search`, `web_fetch`) |

## Conventions

### Rust

- **Rust 2021 edition.** Target MSRV: 1.88+ (pinned in `rust-toolchain.toml`).
- Keep the binary small and dependency-light. No MCP, no telemetry.
- Every public struct, enum, and fn should have a doc comment.
- `cargo doc --no-deps` must build with no warnings.
- `cargo build` must build with no warnings.
- Prefer `anyhow` for error handling in the binary; `thiserror` is available if a crate-level error type is needed.
- **DO NOT add comments unless asked** — keep code self-documenting.

### File Organization

- Modules are declared in `src/main.rs` and re-exported via `src/lib.rs` (so
  benchmarks and integration tests can import them).
- Tests live in `#[cfg(test)] mod tests` blocks at the bottom of each source
  file, plus black-box integration tests in `tests/`.

## Build, Lint, Test

```bash
cargo build                    # debug build, must be warning-free
cargo build --release          # LTO + strip
cargo test                     # 348 tests, all offline (no Ollama needed)
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
  (`has_test_runner` checks for `Cargo.toml`, `package.json`, `pytest.ini`,
  `pyproject.toml`, `setup.py`).
- `run_tests` auto-detects the runner: `cargo test` for Rust, `npm test` for
  Node, `pytest` for Python.

When implementing a fix, **always run the real verification commands** (`cargo
test`, `cargo clippy`, `cargo fmt`) — do not rely on the agent's simulated
`run_tests` tool.

## Common Pitfalls

1. **`tools/` directory** — the tool module was split from a single `tools.rs`
   into `tools/mod.rs`, `definitions.rs`, `dispatch.rs`, `sandbox.rs`,
   `document.rs`, `git.rs`, `patch.rs`. When adding a tool, keep the existing
   structure; definitions go in `definitions.rs`, dispatch in `dispatch.rs`.
2. **Tokenizer is a fast estimator, not exact** — it over-estimates tiktoken counts by roughly 10–35% (mean ~28%), biased slightly high so compaction triggers early. It treats non-newline whitespace as free and applies a ~12% structural-overhead factor. Don't "fix" it to be exact without a real tokenizer to validate against.
3. **Compaction invariants** — `messages[0]` is always the system message;
   tool-call/tool-result pairs are never split. Don't break these.
4. **Sandbox** — all file paths are relative to the workspace root and confined
   to it. `run_shell` blocks dangerous patterns and strips known secret env
   vars. The blocklist is a denylist (best-effort), not an allowlist (issue #14).
5. **Session IDs** — currently the ISO timestamp string; can collide within a
   second (issue #10). Don't assume uniqueness.
6. **Repo map is static** — built once in `Agent::new`; not invalidated when
   files change mid-session (issue #15).
7. **`append_message` is not atomic** — a crash mid-write can leave a partial
   JSONL line (issue #19).

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
