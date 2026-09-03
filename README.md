# Raven

A small, privacy-first coding-agent harness written in Rust for [Ollama](https://ollama.com) and any OpenAI-compatible endpoint. It's a single binary that runs a real agent loop — tools, plan mode, verification, workspace isolation — against a model endpoint you control. No cloud auth layer, no MCP marketplace, no telemetry, no personality layer. Just the loop, the tools, and your workspace.

I built it to learn Rust and to have a harness that only contains the pieces I actually use. It's small enough to read end-to-end, and it runs fine on a Raspberry Pi.

---

## Why Raven?

I wanted a coding agent I could actually understand and trust. The big harnesses are powerful, but they bring a lot I don't need — managed auth, plugin marketplaces, telemetry, personality layers. Raven keeps the parts that matter for real work and drops the rest.

**What it's like to use:**

- **Local first** — runs against Ollama on your machine by default; dial in OpenRouter when a task needs a bigger model. Only LLM requests leave your network.
- **No telemetry** — ever. No usage tracking, no phone-home, no cloud sync. All session state stays on disk, locally.
- **Auditable** — about 23K lines of Rust excluding tests (~31K with tests). You can read the whole harness. No proprietary plugins, no external service dependencies.
- **Small footprint** — a single binary that runs comfortably on a Raspberry Pi. No daemon, no background indexing.
- **No personality layer** — no "soul file". It keeps a plain `.raven/MEMORY.md` with exactly what you tell it to remember.
- **Production-grade safety** — workspace confinement (Landlock + seccomp on Linux), shell command filters, git-worktree isolation, and a verify-before-done gate.

It's built for focused, supervised work — not an autonomous loop trying to complete entire projects unsupervised. I review everything it produces.

## How I use it

Daily driver is the TUI, usually attached to Zed over ACP (`raven --acp`) so model switching happens in the editor's picker. In a terminal: `raven -p "…"` for agent-mode tasks, `--mode plan` when you want a gated plan, `--yolo` for throwaway work.

- **Sessions** — every turn lands in `.raven/sessions/` as JSONL. `--resume` continues the latest, `--list-sessions` browses, `/export` bundles a session as Markdown/JSON.
- **System scope** — `raven --system` runs the same TUI against the whole machine (sandbox root `/`, tiered shell policy: diagnostics auto-run, mutations confirm — or `--system --yolo` for full autonomy on a trusted machine, OS-administration prompt) for Omarchy/desktop management; sessions audit-trailed under `~/.raven/system/sessions/`.
- **Models** — `glm-5.3-flash:cloud` for day-to-day work, `glm-5.3:cloud` when a task needs the flagship, `x-ai/grok-4.5` on OpenRouter for frontier reasoning, `qwen3.8:latest` offline. `/model` + Tab completes; `/provider` switches endpoints.
- **Hermes lineage** — `read_file` document extraction mirrors Hermes Agent's `read_extract.py` (same `anydoc` core), and loop-control fallbacks mirror Hermes's max-iteration recovery. Ideas borrowed from good harnesses; the code is all here.

---

## Install

**Linux / Raspberry Pi:**

```bash
curl -fsSL https://raw.githubusercontent.com/raythurman2386/raven/master/install.sh | sh
```

The script detects your platform, downloads the latest prebuilt binary from GitHub Releases, verifies the SHA-256 checksum against a manifest signed with a pinned Ed25519 key (a bad signature refuses the install), and installs to `~/.cargo/bin`. Build from source with `cargo build --release` or `cargo install --path .`.

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

### In the editor (Zed)

Raven speaks Agent Client Protocol (ACP) over stdio (`raven --acp`), so it runs
as an external agent inside [Zed](https://zed.dev). It advertises a `model`
session config option listing every configured provider's models, so you can
switch providers and models from the editor's model selector. See
[`docs/zed_connection.md`](docs/zed_connection.md) for the complete setup.

---

## Features

| In scope | Intentionally out of scope |
|---|---|
| Streaming agent loop (OpenAI-compatible `/v1/chat/completions`) | MCP marketplace — Raven stays lean |
| 24 tools: `list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`, `search_code`, `todo_write`, `goal_set`, `delegate_task`, `think`, `memory_update`, `memory_search`, `git_status`, `git_diff`, `git_log`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`, `web_search`, `web_fetch`, `skill_search`, `skill_load` | Remote config sync |
| Small footprint — runs on a Raspberry Pi | Personality / "soul file" |
| Document extraction (`read_file` → Markdown via `anydoc`: `.docx`, `.pdf`, `.xlsx`, …) | Multi-model routing |
| Workspace sandbox (path confinement + dangerous-command filter) | GUI / web frontend |
| OS-level subprocess confinement (Landlock, seccomp, rlimits) | Container/VM isolation |
| Git worktree isolation (isolated branches per task) | Cloud sync of sessions |
| Structured plan mode (parse → approve → revise → execute) | Native IDE integration beyond ACP |
| Skills (`SKILL.md` discovery + `skill_search`/`skill_load`) | Plugin marketplace / auth |
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
| [docs/omarchy.md](docs/omarchy.md) | Users (Omarchy Linux) | Default agent + Agents bar panel integration |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Users | Common failure modes (streams, sandbox, ACP) |
| [docs/tools.md](docs/tools.md) | Users + Contributors | Tool contracts, parameters, sandbox rules |
| [docs/security.md](docs/security.md) | Security reviewers | Threat model, defense layers, platform caveats |
| [docs/architecture.md](docs/architecture.md) | Contributors | Design, agent loop, compaction, sandbox |
| [docs/contributing.md](docs/contributing.md) | Contributors | Build, style, how to add a tool or event |
| [docs/testing.md](docs/testing.md) | Contributors | Test structure, coverage, mutation testing |

See also the full [docs index](docs/README.md) and [CHANGELOG.md](CHANGELOG.md).

Raven keeps a plain, editable file at `.raven/MEMORY.md`. The first 25KB is injected into the system prompt on each run. The `memory_update` tool lets the agent persist conventions, decisions, and context across sessions — but it's just a Markdown file you can read and edit yourself. There's no hidden state, no "soul" or persona. It remembers exactly what you (or the agent) put in that file, and nothing else.

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
- **`^C`** — stop the current task (session auto-saves)
- **Tab** — complete the current slash command or its argument; press again to cycle candidates. **Enter** submits when the box already holds a complete candidate (Tab-filled or fully typed), otherwise it fills the highlighted one
- **Up/Down** — recall previous prompts (keep pressing Up to walk back through history; Down returns toward the live input; typing resets). When you are not recalling (typed text at the live position), Up/Down scroll the transcript instead
- **Wheel / PgUp / PgDn** — scroll the transcript (wheel never walks prompt history). Home jumps to the top when the input is empty; End returns to the live tail
- **Mouse drag** — select text in the transcript to copy it to your clipboard
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

## Privacy & local-only operation

**No telemetry, ever.** Nothing is collected, even anonymously. All agent state stays on your machine (except LLM requests to your chosen endpoint). The only network access is:

1. **LLM requests** to your endpoint (Ollama local, OpenRouter cloud, etc.) — you control this.
2. **Optional** `web_search` requests to DuckDuckGo or a self-hosted SearXNG instance.

Everything else stays local. Sessions are stored as plain JSONL under `.raven/sessions/` with local debug-event logs and git-diff snapshots for audit/rollback.

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
  plugins.rs        # Agent Plugins v1.0.0 (skills-only) discovery
  repomap/          # Repo symbol map for large codebases
  web.rs            # web_search / web_fetch
evals/              # Agent evaluation suite
docs/               # Documentation
```

---

## License

MIT
