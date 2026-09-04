# Raven

My opinionated, personal coding-agent harness. Single Rust binary for Linux (x86_64, aarch64) and Raspberry Pi, speaking plain OpenAI-compatible `/v1/chat/completions` to whatever model endpoint I point it at — local Ollama, Ollama Cloud, or OpenRouter.

Modern harnesses are getting bloated: managed auth layers, plugin marketplaces, personality files, and telemetry that not everyone really trusts. I didn't want any of that. I wanted to build and learn a harness end-to-end — the agent loop, streaming, context compaction, sandboxing, TUI, session persistence — and then work through each issue as it comes up in real use. Raven is that project. It's ~23K lines of Rust you can read in an afternoon, and when something breaks, the fix is mine to make.

---

## Where it stands

I use raven as my daily harness for the majority of real work:

- **TUI** (`raven`) — terminal driver, streaming output, slash commands, permission gates, plan mode
- **Editor agent** (`raven --acp`) — ACP over stdio as an external agent in [Zed](https://zed.dev) and [Hearth](https://github.com/raythurman2386/hearth), with provider/model switching from the editor's picker
- **Headless** (`raven -p "…"`) — scripted/CI tasks, and as the backend other agents drive for debugging (I let Grok Build run it headless to help debug raven itself)
- **System scope** (`raven --system`) — the same harness pointed at the whole machine for OS administration (Omarchy desktop management), with tiered shell confirmations

I still reach for other harnesses when one fits a job better, but the loop of daily work — edit, test, fix, commit — I trust to raven. It got there the honest way: I set the models I want to use, leave it alone until it fails at something, then fix that thing in the harness instead of working around it.

**MCP:** stdio only, opt-in. Configure servers in `config.toml` (`[mcp.servers.<name>]`) or let an ACP editor forward them on `session/new`. Tools show up as `{name}__{tool}` — for example `sysmetrics-mcp` becomes `sysmetrics__get_cpu_metrics`. No marketplace, no HTTP/SSE transport.

---

## Install

**Linux / Raspberry Pi** (x86_64, aarch64, armv7 — the only supported platforms):

```bash
curl -fsSL https://raw.githubusercontent.com/raythurman2386/raven/master/install.sh | sh
```

The script detects your platform, downloads the latest prebuilt binary from GitHub Releases, verifies the SHA-256 checksum against a manifest signed with a pinned Ed25519 key (a bad signature refuses the install), and installs to `~/.cargo/bin`. Build from source with `cargo build --release` or `cargo install --path .`. macOS code paths still compile (rlimits-only confinement) but there are no release builds for it.

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

# Agent mode (default): full tools immediately, no plan step
raven -p "Fix the failing tests"

# Plan mode: propose → approve → execute
raven --mode plan -p "Refactor utils"

# Fully autonomous (no confirmations)
raven --yolo -p "Write unit tests for auth"

# Pick a provider / model for this session
raven --provider openrouter -m x-ai/grok-4.5 -p "Task"

# Continue a previous session
raven --resume          # resume latest
raven --list-sessions    # browse all sessions
```

### In the editor (Zed / Hearth)

Raven speaks Agent Client Protocol (ACP) over stdio (`raven --acp`), so it runs
as an external agent inside [Zed](https://zed.dev) and
[Hearth](https://github.com/raythurman2386/hearth). It advertises a `model`
session config option listing every configured provider's models, so you can
switch providers and models from the editor's model selector. See
[`docs/zed_connection.md`](docs/zed_connection.md) for the complete setup.

---

## Features

| In the harness | Deliberately not in it |
|---|---|
| Streaming agent loop (OpenAI-compatible `/v1/chat/completions`) | MCP marketplace / HTTP+SSE MCP transports |
| 24 tools: `list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`, `search_code`, `todo_write`, `goal_set`, `delegate_task`, `think`, `memory_update`, `memory_search`, `git_status`, `git_diff`, `git_log`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`, `web_search`, `web_fetch`, `skill_search`, `skill_load` | Telemetry / usage tracking / phone-home — I don't trust it either |
| Small footprint — single binary, runs on a Raspberry Pi | Personality / "soul file" — it's an agent loop, not a character |
| Document extraction (`read_file` → Markdown via `anydoc`: `.docx`, `.pdf`, `.xlsx`, …) | Plugin marketplace / managed auth layer |
| Workspace sandbox (path confinement + dangerous-command filter) | GUI / web frontend |
| OS-level subprocess confinement (Landlock, seccomp, rlimits) | Cloud sync of sessions — state stays on your disk |
| Git worktree isolation (isolated branches per task) | Multi-model routing — one model per turn, switched explicitly |
| Structured plan mode (parse → approve → revise → execute) | Windows / macOS release targets — Linux + Pi is the whole surface |
| Skills (`SKILL.md` discovery + `skill_search`/`skill_load`) | |
| Repo symbol map (`<repo_map>` for large workspaces) | |
| Parallel tool execution within a single model turn | |
| Context-window inference + automatic compaction | |
| JSONL session persistence + `--resume` / `--list-sessions` | |
| Cross-session project memory (`.raven/MEMORY.md`) — a plain file, nothing hidden | |
| ACP v1 stdio (`raven --acp`) for editor attachment (Zed, Hearth) | |
| Stdio MCP client (`[mcp.servers]`, ACP `mcpServers`, Agent Plugins `mcp.json`) | |
| ratatui TUI + headless CLI | |

---

## Documentation

| Guide | Audience | Contents |
|---|---|---|
| [docs/usage.md](docs/usage.md) | Users | Day-to-day workflows, plan mode, parallel sub-agents |
| [docs/configuration.md](docs/configuration.md) | Users | Config, env vars, providers, API keys, AGENTS.md |
| [docs/example.config.toml](docs/example.config.toml) | Users | Fully-commented reference config |
| [docs/zed_connection.md](docs/zed_connection.md) | Users | Connect Raven to Zed / Hearth via ACP |
| [docs/omarchy.md](docs/omarchy.md) | Users (Omarchy Linux) | Default agent + Agents bar panel integration |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Users | Common failure modes (streams, sandbox, ACP) |
| [docs/tools.md](docs/tools.md) | Users + Contributors | Tool contracts, parameters, sandbox rules |
| [docs/security.md](docs/security.md) | Security reviewers | Threat model, defense layers, platform caveats |
| [docs/architecture.md](docs/architecture.md) | Contributors | Design, agent loop, compaction, sandbox |
| [docs/contributing.md](docs/contributing.md) | Contributors | Build, style, how to add a tool or event |
| [docs/testing.md](docs/testing.md) | Contributors | Test structure, coverage, mutation testing |

See also the full [docs index](docs/README.md) and [CHANGELOG.md](CHANGELOG.md).

Raven keeps a plain, editable file at `.raven/MEMORY.md`. The first 25KB is injected into the system prompt on each run, and the `memory_update` tool lets the agent persist conventions, decisions, and context across sessions. It's just a Markdown file — open it, edit it, delete it. It remembers exactly what you (or the agent) put in that file, and nothing else.

---

## Context management

The agent tracks token usage with a built-in token estimator (no external vocab file needed). When the conversation approaches the context window limit, it automatically:

1. **Prunes old tool results** — soft-trims tool outputs older than 3 turns (keeps head + tail with a truncation marker)
2. **Compacts the conversation** — summarizes the middle of the conversation, preserving the system message, a short facts block (goal, open todos, key paths, last verification), and the last ~40% of the context budget for recent messages. The TUI shows a one-line "what was compacted" note.

Context window sizes are fetched from the model's actual metadata via Ollama's `/api/show` endpoint. This returns the real `context_length` from the model file (e.g. `gemma4` → 128K, `qwen3.5` → 256K, `deepseek-v4-pro:cloud` → 1M). If the API is unreachable (Ollama not running, model not found), a name-based heuristic is used as fallback:

- `glm-5.3:cloud`, `glm-5.3-flash:cloud`, `deepseek-v4:cloud` (flash and pro) → 1M
- `qwen3.5` → 256K
- `gemma4`, `gemma3`, `qwen2.5`, `qwen3`, `llama3.1`, `llama3.2`, `deepseek`, `codestral`, `glm` → 128K
- `llama3`, `codellama`, `"32k"` in name → 32K
- `mistral`, `"8k"` in name → 8K
- Unknown models → 32K (safe default)

---

## Session persistence

Sessions are stored as JSONL under `.raven/sessions/`:

```
.raven/sessions/
  2026-08-17T12-34-56-12345-0001/          # collision-proof ID (timestamp + PID + counter)
    summary.json                             # metadata: id, model, timestamps, title
    messages.jsonl                           # one ChatMessage per line (append-only)
    debug-events.jsonl                       # local-only event log (model changes, saves, etc.)
    last.patch                               # git diff snapshot (for audit/rollback)
```

**Local-only guarantees:**
- All writes are atomic (temp file + rename) for crash safety.
- **Debug events** (model changes, saves, etc.) are logged locally for reproducible debugging — never networked.
- **Patch snapshots** (`last.patch`) are created after each session, recording the full git diff for audit or rollback decisions.
- No telemetry, no remote reporting, no cloud sync.

**Usage:**
```bash
raven --resume            # continue the most recent session
raven --resume <id>       # continue a specific session by ID
raven --list-sessions     # browse all sessions and their metadata
raven --export            # write a local Markdown/JSON bundle of the latest session
raven --export <id>       # export a specific session (see also TUI `/export`)
```

---

## Workflow & tips

### Agent mode (default)

The default is full tools immediately — same shape as a Grok Build turn.
Use `--mode plan` (or Shift+Tab in the TUI) when you want a gated plan:

1. **Propose** — agent creates a step-by-step plan
2. **Review** — you read and approve (or revise)
3. **Execute** — agent runs the plan with full tools

```bash
raven --mode plan -p "Add type safety to this handler"
# (agent proposes a plan)
# ── Approve? [Y]es / [n]o / [r]evise ──
# y
# (agent executes)
```

### Quick edits

Skip the plan step (this is the default):
```bash
raven -p "Refactor this function"
```

### TUI tips

- **`/model`** — switch models or check the live model list for the active provider
- **`/provider`** — switch providers (ollama, openrouter, etc.) — slash-command autocomplete shows all available
- **`/clear`** — start a fresh turn (keeps session history)
- **`/retry`** — re-run the last user prompt after a failed turn
- **`/loop [N]`** — show or set the max iteration budget for new turns
- **`/steer <message>`** — redirect the running agent (queued into the turn; re-fires the last turn when idle)
- **`/cleanup <days> [--yes]`** — prune sessions older than N days (dry-run unless `--yes`; never deletes the current session)
- **`Esc`** — interrupt the running task (session auto-saves the partial turn); layered dismiss when completion/selection/prompt is open. When idle, `Esc` `Esc` quits
- **`Ctrl+C`** — interrupts like Esc while a turn runs; needs a second press within 3s to quit when idle
- **Tab** — complete the current slash command or its argument; press again to cycle candidates. **Enter** submits when the box already holds a complete candidate (Tab-filled or fully typed), otherwise it fills the highlighted one
- **Up/Down** — recall previous prompts (keep pressing Up to walk back through history; Down returns toward the live input; typing resets). When you are not recalling (typed text at the live position), Up/Down scroll the transcript instead
- **Wheel / PgUp / PgDn** — scroll the transcript (wheel never walks prompt history). Home jumps to the top when the input is empty; End returns to the live tail
- **Mouse drag** — select text in the transcript to copy it to your clipboard
- **`y` / `n`** — answer a shell-permission prompt in one keystroke (bare `Enter` allows, `Esc` denies)
- The footer below the input box shows context-sensitive keyhints (approve / answer / interrupt / idle)

### For large codebases

Raven injects a repo symbol map for larger workspaces (15+ source files or 80+ symbols; per-file cap 256KB). The map helps the agent navigate structure without reading entire files:
```bash
raven --context-window 131072 -p "Find all database queries and optimize them"
```

If the agent seems stuck compacting, raise the threshold:
```bash
raven --compact-threshold 0.85 -p "Task"
```

### Workspace memory

Raven keeps a plain `.raven/MEMORY.md` file across sessions. The agent can read and update it:
- Use `memory_search` to find past decisions
- Use `memory_update` to record conventions or context

Example: *"Remember we prefer async/await over promises in this codebase"*

It's just a Markdown file — open it, edit it, or delete it. It only holds what you put there.

### Workspace rules

Create an `AGENTS.md` or `CLAUDE.md` file in your repo root:
```markdown
# Coding Guidelines

- Always write tests for new features
- Use TypeScript, not JavaScript
- Follow the style guide in docs/STYLE.md
```

Raven auto-loads this and injects it into every session. You can also override per-session:
```bash
raven --rules "Use Python 3.11+; no type hints optional." -p "Task"
```

### Multi-agent tasks

Use `--parallel` to spawn multiple focused agents and gather results in parallel:
```bash
raven --parallel \
  "Summarize the architecture" \
  "List all TODOs" \
  "Check for secrets in git history"
```

---

## Shell safety

The `run_shell` tool uses two complementary filters, neither of which is a security boundary:

1. **Denylist** — a regex that blocks obviously destructive patterns (recursive root deletes, fork bombs, `curl | sh`, etc.). This is a **best-effort guard**, not a security boundary. A denylist is inherently incomplete — it can always be bypassed (e.g. `rm -rf ~` is not blocked even though `rm -rf /` is).

2. **Allowlist** — a regex that matches known-safe development commands (`cargo`, `git`, `npm`, `ls`, `grep`, etc.). When `confirm_shell` is enabled (the default, non-`--yolo` path), commands matching the allowlist run without a confirmation prompt. Anything outside the allowlist requires explicit user approval. Commands whose first token is allowlisted and contain no shell metacharacters run via **direct exec** (no `sh -c`), removing the shell-injection surface for the common case.

The `--yolo` flag disables confirmation entirely, but the denylist still applies as a last-resort filter. In addition to these filters, confined subprocesses run under OS-level sandboxing: **Landlock** (filesystem confinement) and **seccomp** (network-block) on Linux, plus **resource limits** (CPU / file size / fds) on Linux + macOS. See [`docs/security.md`](docs/security.md) for the full threat model.

---

## Testing

### Unit & integration tests

```bash
cargo test                    # offline unit + integration tests
cargo test eval_suite         # Layer A (fake model) eval harness
cargo clippy                  # zero warnings
cargo clippy -- -W clippy::pedantic  # stricter linting
```

### Live agent eval suite

The eval suite runs real agent tasks against a live model endpoint and grades the results. See [`evals/README.md`](evals/README.md) for full details. I run this to decide how well new models *could* run in this harness for your average usage, not as a hard evaluation of strength.

```bash
cargo build --release

# Against Ollama Cloud (needs RAVEN_API_KEY)
python3 evals/run.py --model qwen3.8 --host https://api.ollama.ai/api/v1

# Against local Ollama
python3 evals/run.py --model qwen3.8:latest --host http://127.0.0.1:11434/v1

# Against OpenRouter (needs RAVEN_API_KEY)
python3 evals/run.py --model grok-4.5 --host https://openrouter.ai/api/v1

# View results
cat evals/out/<run-id>.md
```

**Top-performing models (current):**
- **Ollama Cloud (daily-use recommended):** `glm-5.3-flash:cloud` (default, excellent), `glm-5.3:cloud` (flagship, strongest coding/agentic), `kimi-k3:cloud` (latest, excellent), `deepseek-v4-pro:cloud` (high quality), `deepseek-v4-flash:cloud` (efficient)
- **OpenRouter (frontier):** `x-ai/grok-4.5` (best reasoning, multimodal), `x-ai/grok-4.6` (frontier)
- **Local (when cloud unavailable):** `qwen3.8:latest`

See `docs/testing.md` for coverage and mutation testing details.

**Troubleshooting:** common failure modes (stream errors, sandbox denies,
SearXNG fallback, ACP) are covered in
[`docs/troubleshooting.md`](docs/troubleshooting.md).

---

## What talks to the network

Raven makes exactly two kinds of outbound requests, both under your control:

1. **LLM requests** to your configured endpoint (Ollama local, Ollama Cloud, OpenRouter, …).
2. **Optional** `web_search` requests to DuckDuckGo or a self-hosted SearXNG instance.

That's the whole list. No update checks run on their own (self-update is an explicit `raven self update`), nothing is collected or reported, and all session state — JSONL transcripts, debug-event logs, git-diff snapshots — lives as plain files under `.raven/` on your disk. If you want to verify that, it's ~23K lines; the network calls are all in `agent/stream.rs` and `web.rs`.

---

## Project layout

```
src/
  main.rs           # CLI, TUI, headless runner, session management
  lib.rs            # Library re-exports for benchmarks/integration tests
  agent/            # Core agent loop (core, tools_exec, stream, parallel)
  commands/         # Slash-command registry + parser (/retry, /loop, /steer, /cleanup, ...)
  tools/            # Tool implementations (24 total) + sandbox/
  tui/              # ratatui TUI (render, markdown, completion)
  config/           # Layered config.toml loading, provider presets
  context.rs        # Context-window management and compaction
  tokenizer.rs      # Pure-Rust token counter (no vocab file)
  session.rs        # JSONL persistence, resume, list
  plan.rs           # Structured plan mode
  memory.rs         # Cross-session `.raven/MEMORY.md`
  skills.rs         # SKILL.md discovery
  plugins/          # Agent Plugins v1.0.0 (skills + stdio MCP) discovery
  repomap/          # Repo symbol map for large codebases
  web.rs            # web_search / web_fetch
evals/              # Agent evaluation suite
docs/               # Documentation
```

---

## License

MIT
