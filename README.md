# Raven

A small, privacy-first coding-agent harness written in Rust for [Ollama](https://ollama.com) and any OpenAI-compatible endpoint. It's a single binary that runs a real agent loop — tools, plan mode, verification, workspace isolation — against a model endpoint you control. No cloud auth layer, no MCP marketplace, no telemetry, no personality layer. Just the loop, the tools, and your workspace.

I built it to learn Rust and to have a harness that only contains the pieces I actually use. It's small enough to read end-to-end, and it runs fine on a Raspberry Pi.

---

## Why Raven?

I wanted a coding agent I could actually understand and trust. The big harnesses are powerful, but they bring a lot I don't need — managed auth, plugin marketplaces, telemetry, personality layers. Raven keeps the parts that matter for real work and drops the rest.

**What it's like to use:**

- **Local first** — runs against Ollama on your machine by default; dial in OpenRouter when a task needs a bigger model. Only LLM requests leave your network.
- **No telemetry** — ever. No usage tracking, no phone-home, no cloud sync. All session state stays on disk, locally.
- **Auditable** — about 21K lines of Rust. You can read the whole harness. No proprietary plugins, no external service dependencies.
- **Small footprint** — a single binary that runs comfortably on a Raspberry Pi. No daemon, no background indexing.
- **No personality layer** — no "soul file". It keeps a plain `.raven/MEMORY.md` with exactly what you tell it to remember.
- **Production-grade safety** — workspace confinement (Landlock + seccomp on Linux), shell command filters, git-worktree isolation, and a verify-before-commit gate.

It's built for focused, supervised work — not an autonomous loop trying to complete entire projects unsupervised. I review everything it produces.

---

## Install

**Linux / Raspberry Pi:**

```bash
curl -fsSL https://raw.githubusercontent.com/raythurman2386/raven/master/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/raythurman2386/raven/master/install.ps1 | iex
```

The script detects your platform, downloads the latest prebuilt binary from GitHub Releases, verifies the SHA-256 checksum, and installs to `~/.cargo/bin`. Build from source with `cargo build --release` or `cargo install --path .`.

**Requirements:** Rust 1.88+ (MSRV) and a model endpoint — local Ollama, [Ollama Cloud](https://ollama.ai/cloud), [OpenRouter](https://openrouter.ai), or any OpenAI-compatible `/v1` API.

---

## Quick start

```bash
# Interactive TUI (default when no task is given).
# On first run Raven walks you through provider + model selection.
raven

# One-shot tasks (quotes required for multi-word tasks)
raven -p "Explain the structure of this repository"
raven -p "Add a README and .gitignore"

# Plan mode (default): propose → approve → execute
raven -p "Fix the failing tests"

# Agent mode: full tools immediately, no plan step
raven --mode agent -p "Refactor utils"

# Fully autonomous (no confirmations)
raven --yolo -p "Write unit tests for auth"

# Pick a provider / model for this session
raven --provider openrouter -m x-ai/grok-4.5 -p "Task"

# Continue a previous session
raven --resume          # resume latest
raven --list-sessions    # browse all sessions
```

### In the editor (Zed)

Raven speaks Agent Client Protocol (ACP) over stdio (`raven --acp`), so it runs
as an external agent inside [Zed](https://zed.dev). See
[`docs/zed_connection.md`](docs/zed_connection.md) for the complete setup.

---

## Features

| In scope | Intentionally out of scope |
|---|---|
| Streaming agent loop (OpenAI-compatible `/v1/chat/completions`) | MCP marketplace — Raven stays lean |
| 25 tools: `list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`, `search_code`, `todo_write`, `goal_set`, `delegate_task`, `think`, `memory_update`, `memory_search`, `git_status`, `git_diff`, `git_log`, `git_commit`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`, `web_search`, `web_fetch`, `skill_search`, `skill_load` | Remote config sync |
| Small footprint — runs on a Raspberry Pi | Personality / "soul file" |
| Document extraction (`read_file` → Markdown via `anydoc`: `.docx`, `.pdf`, `.xlsx`, …) | Multi-model routing |
| Workspace sandbox (path confinement + dangerous-command filter) | GUI / web frontend |
| OS-level subprocess confinement (Landlock, seccomp, rlimits; Windows Job Object) | Container/VM isolation |
| Git worktree isolation (isolated branches per task) | Cloud sync of sessions |
| Structured plan mode (parse → approve → revise → execute) | Native IDE integration beyond ACP |
| Skills (`SKILL.md` discovery + `skill_search`/`skill_load`) | Plugins / marketplace auth |
| Repo symbol map (`<repo_map>` for large workspaces) | Managed workflow orchestration |
| Parallel tool execution within a single model turn | Telemetry / usage tracking |
| Context-window inference + automatic compaction | |
| JSONL session persistence + `--resume` / `--list-sessions` | |
| Cross-session project memory (`.raven/MEMORY.md`) | |
| ACP v1 stdio (`raven --acp`) for editor attachment | |
| ratatui TUI + headless CLI | |

---

## Documentation

| Guide | Audience | Contents |
|---|---|---|
| [docs/usage.md](docs/usage.md) | Users | Day-to-day workflows, plan mode, parallel sub-agents |
| [docs/configuration.md](docs/configuration.md) | Users | Config, env vars, providers, API keys, AGENTS.md |
| [docs/example.config.toml](docs/example.config.toml) | Users | Fully-commented reference config |
| [docs/zed_connection.md](docs/zed_connection.md) | Users | Connect Raven to Zed via ACP |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Users | Common failure modes (Windows, streams, sandbox, ACP) |
| [docs/tools.md](docs/tools.md) | Users + Contributors | Tool contracts, parameters, sandbox rules |
| [docs/security.md](docs/security.md) | Security reviewers | Threat model, defense layers, platform caveats |
| [docs/architecture.md](docs/architecture.md) | Contributors | Design, agent loop, compaction, sandbox |
| [docs/contributing.md](docs/contributing.md) | Contributors | Build, style, how to add a tool or event |
| [docs/testing.md](docs/testing.md) | Contributors | Test structure, coverage, mutation testing |

See also the full [docs index](docs/README.md) and [CHANGELOG.md](CHANGELOG.md).

---

## Privacy & local-only operation

**No telemetry, ever.** Nothing is collected, even anonymously. All agent state stays on your machine (except LLM requests to your chosen endpoint). The only network access is:

1. **LLM requests** to your endpoint (Ollama local, OpenRouter cloud, etc.) — you control this.
2. **Optional** `web_search` requests to DuckDuckGo or a self-hosted SearXNG instance.

Everything else stays local. Sessions are stored as plain JSONL under `.raven/sessions/` with local debug-event logs and git-diff snapshots for audit/rollback.

---

## Testing

```bash
cargo test                # offline unit + integration tests
cargo clippy              # zero warnings
```

The live eval suite runs real agent tasks against a live model endpoint and grades results — see [`evals/README.md`](evals/README.md).

---

## Project layout

```
src/
  main.rs           # CLI, TUI, headless runner, session management
  lib.rs            # Library re-exports for benchmarks/integration tests
  agent/            # Core agent loop (core, tools_exec, stream, parallel)
  commands.rs       # Slash-command registry (/provider, /model, /clear)
  tools/            # Tool implementations (25 total) + sandbox/
  tui/              # ratatui TUI (render, markdown, completion)
  config/           # Layered config.toml loading, provider presets
  context.rs        # Context-window management and compaction
  tokenizer.rs      # Pure-Rust token counter (no vocab file)
  session.rs        # JSONL persistence, resume, list
  plan.rs           # Structured plan mode
  memory.rs         # Cross-session `.raven/MEMORY.md`
  skills.rs         # SKILL.md discovery
  repomap/          # Repo symbol map for large codebases
  web.rs            # web_search / web_fetch
evals/              # Agent evaluation suite
docs/               # Documentation
```

---

## License

MIT
