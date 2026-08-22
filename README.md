# Raven

A small, privacy-first coding-agent harness written in Rust for [Ollama](https://ollama.com) and any OpenAI-compatible endpoint. It's a single binary that runs a real agent loop — tools, plan mode, verification, workspace isolation — against a model endpoint you control. No cloud auth layer, no MCP marketplace, no telemetry, no personality layer. Just the loop, the tools, and your workspace.

I built it to learn Rust and to have a harness that only contains the pieces I actually use. It's small enough to read end-to-end, and it runs fine on a Raspberry Pi.

---

## Why Raven?

I wanted a coding agent I could actually understand and trust. The big harnesses are powerful, but they bring a lot I don't need — managed auth, plugin marketplaces, telemetry, personality layers. Raven keeps the parts that matter for real work and drops the rest.

**What it's like to use:**
- **Local first** — I run it against Ollama on my machine by default. When a task needs a bigger model, I dial in OpenRouter. Only LLM requests leave my network.
- **No telemetry** — ever. No usage tracking, no phone-home, no managed auth. All session state stays on disk, locally.
- **Auditable** — about 21K lines of Rust (26K with tests). You can read the whole harness. No proprietary plugins, no external service dependencies, no magic.
- **Small footprint** — it's a single binary that runs comfortably on a Raspberry Pi. No daemon, no background indexing, no heavy runtime.
- **No personality layer** — there's no "soul file" or persona. It keeps a plain `.raven/MEMORY.md` with exactly what you tell it to remember, and nothing else.
- **Production-grade safety** — workspace confinement (Landlock + seccomp on Linux), shell command filters, git-worktree isolation per task, and a verify-before-commit gate so it won't finish a turn that edited files without running tests.

It's built for focused, supervised work — not an autonomous loop trying to complete entire projects unsupervised. I use it for real coding tasks every day, and I review what it produces.

---

## How I use it

Raven is my daily driver for coding work. Here's the shape of that, in case it helps you find your own workflow.

**In the editor.** I run Raven inside [Zed](https://zed.dev) for standard agent work — attach it via ACP (`raven --acp`) and work on a task directly in the editor. It's also a plain headless CLI, so it drops into whatever editor or terminal you like.

**As a sub-agent for other agents.** Raven is small and focused, so other agents can drive it to save their own context window. When a larger agent needs code changes or parallel work done, it hands the task to Raven rather than burning its own context on file edits and tool calls.

**Models.** I default to local Ollama. When a task needs a bigger model, I switch to OpenRouter for that session. Nothing else changes — same tools, same loop, just a different endpoint.

**Sessions.** I do a mix of both:
- **Start fresh** for most tasks — a clean context, no baggage.
- **`--resume`** when I want long-term continuity — pick up a session from a previous day and keep going.

**Automation.** Raven isn't just interactive. Other agents can run it headless to implement fixes. In my setup, if a larger agent finds a bug it files an issue, and an automated loop picks that issue up and uses Raven to implement the fix. It's a small, predictable tool that slots into a bigger pipeline.

---

## Features

| In scope | Intentionally out of scope |
|---|---|
| Streaming agent loop (OpenAI-compatible `/v1/chat/completions`) | MCP marketplace — I keep MCP connections in a separate agent; Raven stays lean |
| 25 tools: `list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`, `search_code`, `todo_write`, `goal_set`, `delegate_task`, `think`, `memory_update`, `memory_search`, `git_status`, `git_diff`, `git_log`, `git_commit`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`, `web_search`, `web_fetch`, `skill_search`, `skill_load` | Remote config sync |
| Small footprint — single binary, runs on a Raspberry Pi | Personality / "soul file" — no persona layer, just a plain `.raven/MEMORY.md` |
| Document extraction: `read_file` converts `.docx`, `.pdf`, `.xlsx`, `.odt`, `.epub`, `.pptx`, `.csv`, `.rtf`, `.ods`, `.odp`, `.doc`, `.xls`, `.ppt` and more to Markdown (via the `anydoc` engine) | Multi-model routing |
| Workspace sandbox (path confinement + dangerous-command filter) | Rhai workflow engine |
| OS-level subprocess confinement (Landlock, seccomp network block, rlimits) | GUI / web frontend |
| Git worktree isolation (isolated branches per task) | Container/VM isolation |
| Windows Job Object confinement (process-tree + memory limits) | Telemetry / usage tracking |
| Structured plan mode (parse → approve → revise → execute) | Cloud sync of sessions |
| Skills (`SKILL.md` discovery + `skill_search`/`skill_load`) | Chat federation or multi-provider routing |
| Repo symbol map (`<repo_map>` injected for large workspaces) | Native IDE integration (VSCode ext, etc) |
| Parallel tool execution within a single model turn | Plugins or marketplace auth |
| Pure-Rust token estimator for context-window management | Managed workflow orchestration |
| Context-window inference + automatic compaction (tool-result pruning) | |
| JSONL session persistence + local replay (`--resume`, `--list-sessions`) | |
| Local debug event logs per session (no networking) | |
| Git patch snapshots per session for audit/rollback | |
| Cross-session project memory (`.raven/MEMORY.md`) | |
| Persistent goal + task list (`.raven/state/`, injected into the system prompt) | |
| Model-spawnable sub-agents (`delegate_task`) + `think` tool | |
| Compaction thrashing protection | |
| Config file (`~/.raven/config.toml` + workspace config) | |
| Typed errors with retry + exponential backoff | |
| Non-streaming fallback (`--no-stream`) | |
| Simple ratatui TUI + headless CLI | |
| ACP v1 stdio (`raven --acp`) for editor attachment | |
| Markdown rendering in the TUI (headings, code blocks, lists, tables, links) | |
| Slash-command completion (`/provider`, `/model` with live endpoint discovery) | |
| `AGENTS.md` / `CLAUDE.md` auto-load + `--rules` session overrides | |
| Local by default; optional Bearer auth for Ollama Cloud or OpenRouter | |
| Optional self-hosted SearXNG backend for `web_search` (falls back to DuckDuckGo) | |

---

## Requirements

- **Rust 1.88+** (MSRV; pinned to 1.97 in `rust-toolchain.toml`)
- **A model endpoint** — any of:
  - Local [Ollama](https://ollama.com) instance (`ollama pull <model> && ollama serve`)
  - [Ollama Cloud](https://ollama.ai/cloud) — fastest deployment, works with all Ollama models, recommended for daily use
  - [OpenRouter](https://openrouter.ai) — for access to Grok, Gemma, and frontier models
  - Any OpenAI-compatible API (vLLM, LocalAI, etc.)

**Recommended models:**
- **Best all-around (Ollama Cloud):** `qwen3.8` (latest, excellent), `deepseek-v4-pro:cloud` (high quality, fast), `deepseek-v4-flash:cloud` (efficient)
- **Long-horizon (Ollama Cloud):** `glm-5.2:cloud` (optimized for long-horizon tasks)
- **Good local fallback:** `qwen3.8:latest` (strong local performance)
- **For frontier results (OpenRouter):** `x-ai/grok-4.5` (multimodal, best reasoning), `x-ai/grok-4.6` (frontier)

---

## Install

### One-liner (recommended)

**Linux / Raspberry Pi:**

```bash
curl -fsSL https://raw.githubusercontent.com/raythurman2386/raven/master/install.sh | sh
```

**Windows (PowerShell):**

```powershell
irm https://raw.githubusercontent.com/raythurman2386/raven/master/install.ps1 | iex
```

The script detects your platform, downloads the latest prebuilt binary from
GitHub Releases, verifies the SHA-256 checksum, and installs to `~/.cargo/bin`.

Options:

```bash
curl -fsSL .../install.sh | sh -s -- --version 0.4.0   # pin a version
curl -fsSL .../install.sh | sh -s -- --to /usr/local/bin  # custom install dir
curl -fsSL .../install.sh | sh -s -- --force              # overwrite existing
```

### Build from source

```bash
cd raven
cargo build --release
# binary: target/release/raven
```

Optional install to `~/.cargo/bin`:

```bash
cargo install --path .
```

---

## Quick start

### First run

```bash
# Interactive TUI (default when no task is given)
raven
# On first run (no config yet), Raven prompts you to choose a provider
# (local Ollama, Ollama Cloud, OpenRouter, or any custom OpenAI-compatible
# endpoint via "name:base_url"), pick a model (listing live models when
# available), and saves your choice to ~/.raven/config.toml. Any API key
# you enter is written to ~/.raven/.env (mode 0600), keeping config.toml
# secret-free. Subsequent runs skip the wizard.
```

### One-shot tasks (headless)

```bash
# Explain the repo structure
raven -p "Explain the structure of this repository"

# Add a feature (quotes required for multi-word tasks)
raven -p "Add a README and .gitignore"

# Plan mode (default): propose, approve, execute
raven -p "Fix the failing tests"

# Agent mode (skip plan): full tools immediately
raven --mode agent -p "Refactor utils"

# Fully autonomous (no confirmations)
raven --yolo -p "Write unit tests for auth"
```

### Daily workflows

```bash
# Switch providers mid-session (TUI: /provider)
raven --provider openrouter -p "Use this task on OpenRouter"

# Override model for a specific task
raven -m qwen3.8 -p "This needs the big model"

# Continue a previous session
raven --resume            # resume latest
raven --resume <id>       # resume specific session
raven --list-sessions     # see all sessions in workspace

# Append workspace rules to the system prompt
raven --rules "Always use TypeScript. Prefer functional components." \
       -p "Add a dark mode toggle"

# Parallel sub-agents (three focused agents in parallel)
raven --parallel \
  "Summarize the architecture of src/" \
  "List all TODOs and FIXMEs" \
  "Check for security issues in tools.rs"
```

### Advanced

```bash
# Override context window and compaction threshold
raven --context-window 32768 --compact-threshold 0.5 -p "Analyze this large file"

# Non-streaming mode (for models/endpoints that don't support SSE)
raven --no-stream -p "Hello"

# Point the ollama provider at a custom endpoint (via config.toml)
#   [providers.ollama]
#   base_url = "http://remote-server:11434/v1"
raven --provider ollama -p "Task"

# Workspace-specific settings
cd /path/to/project
raven -p "Read my local config from .raven/config.toml"
```

### Provider setup

**Ollama Cloud (recommended for daily use):**
```bash
# Sign up at https://ollama.ai/cloud
# Set your API key:
export RAVEN_API_KEY=...  # Your Ollama Cloud token

# Use cloud models:
raven --provider ollama -m deepseek-v4-flash:cloud -p "Task"
raven --provider ollama -m deepseek-v4-pro:cloud -p "Task"
```

**Ollama (local):**
```bash
ollama pull qwen3.8:latest
ollama serve
# In another terminal:
raven --provider ollama -m qwen3.8:latest -p "Task"
```

**OpenRouter (for Grok and frontier models):**
```bash
# Sign up at https://openrouter.ai
export RAVEN_API_KEY=sk-or-v1-...  # Your OpenRouter key

# Or add to ~/.env:
echo "RAVEN_API_KEY=sk-or-v1-..." >> .env

raven --provider openrouter -m grok-4.5 -p "Task"
```

---

## Configuration

### Config file

Layered config, highest priority wins:

1. **CLI flags** (highest)
2. **Environment variables** (`RAVEN_*` prefix, with legacy `OG_*` fallbacks)
3. **`.env` file** (if present in workspace root or repo root; auto-loaded, never overwrites exports)
4. **Workspace config** (`.raven/config.toml`)
5. **Global config** (`~/.raven/config.toml`)
6. **Built-in defaults** (lowest)

```toml
# ~/.raven/config.toml or .raven/config.toml
provider = "ollama"          # or "openrouter"

[providers.ollama]
base_url = "http://localhost:11434/v1"
default_model = "qwen3.8:latest"
# api_key = "optional"       # for Ollama Cloud

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
default_model = "grok-4.5"
api_key = "sk-or-v1-..."     # or use OPENROUTER_API_KEY env var

context_window = 131072
compact_threshold = 0.75
max_iterations = 30
mode = "plan"                # or "agent" for skip-plan mode
temperature = 0.2
no_stream = false
theme = "ravenwood"
```

### Environment variables

`RAVEN_*` vars take priority; legacy `OG_*` vars are accepted as fallbacks.

| Variable | Description | Default |
|---|---|---|
| `RAVEN_PROVIDER` | Active provider name (`ollama`, `openrouter`, etc.) | `ollama` |
| `RAVEN_API_KEY` | **Universal** Bearer token (highest priority) | none |
| `OPENROUTER_API_KEY` | Bearer token for OpenRouter provider | none |
| `OLLAMA_API_KEY` | Bearer token for Ollama Cloud | none |
| `RAVEN_CONTEXT_WINDOW` / `OG_CONTEXT_WINDOW` | Override context window size (default: auto-detected) | inferred from model |
| `RAVEN_COMPACT_THRESHOLD` / `OG_COMPACT_THRESHOLD` | Compaction trigger (0.0–1.0) | 0.75 |
| `RAVEN_MAX_ITER` / `OG_MAX_ITER` | Max agent iterations per run | 30 |
| `RAVEN_SEARXNG_URL` | Optional self-hosted SearXNG base URL for `web_search` | none |
| `RAVEN_SEARXNG_ENGINES` | Optional comma-separated SearXNG engine list | none |

**API key precedence** (highest to lowest):
1. `RAVEN_API_KEY` (universal override, works for all providers)
2. `<PROVIDER>_API_KEY` (provider-scoped, e.g., `OPENROUTER_API_KEY`, `OLLAMA_API_KEY`)
3. `api_key` in config.toml
4. None (public/local-only endpoints)

**Example: OpenRouter with .env**
```bash
# Create .env in the workspace root (gets auto-loaded on startup)
cat > .env << EOF
OPENROUTER_API_KEY=sk-or-v1-abc123...
EOF

# Raven will use this key automatically
raven --provider openrouter -p "Task"
```

### Project instructions

On startup, the agent loads `AGENTS.md` (or `CLAUDE.md`) from the workspace root and injects it into the system prompt. Use `--rules` for per-session overrides.

### Project memory

Raven keeps a plain, editable file at `.raven/MEMORY.md`. The first 25KB is injected into the system prompt on each run. The `memory_update` tool lets the agent persist conventions, decisions, and context across sessions — but it's just a Markdown file you can read and edit yourself. There's no hidden state, no "soul" or persona. It remembers exactly what you (or the agent) put in that file, and nothing else.

---

## Context management

The agent tracks token usage with a built-in token estimator (no external vocab file needed). When the conversation approaches the context window limit, it automatically:

1. **Prunes old tool results** — soft-trims tool outputs older than 3 turns (keeps head + tail with a truncation marker)
2. **Compacts the conversation** — summarizes the middle of the conversation, preserving the system message and the last ~40% of the context budget for recent messages

Context window sizes are fetched from the model's actual metadata via Ollama's `/api/show` endpoint. This returns the real `context_length` from the model file (e.g. `gemma4` → 128K, `qwen3.5` → 256K, `deepseek-v4-pro:cloud` → 1M). If the API is unreachable (Ollama not running, model not found), a name-based heuristic is used as fallback:

- `glm:cloud`, `deepseek-v4:cloud` (flash and pro) → 1M
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
```

---

## Workflow & tips

### Plan mode (default)

Plan mode is the recommended workflow for important changes:
1. **Propose** — agent creates a step-by-step plan
2. **Review** — you read and approve (or revise)
3. **Execute** — agent runs the plan with full tools

```bash
raven -p "Add type safety to this handler"
# (agent proposes a plan)
# ── Approve? [Y]es / [n]o / [r]evise ──
# y
# (agent executes)
```

### Quick edits (agent mode)

Skip the plan step for quick, exploratory tasks:
```bash
raven --mode agent -p "Refactor this function"
```

### TUI tips

- **`/model`** — switch models or check the live model list for the active provider
- **`/provider`** — switch providers (ollama, openrouter, etc.) — slash-command autocomplete shows all available
- **`/clear`** — start a fresh turn (keeps session history)
- **`^C`** — stop the current task (session auto-saves)
- **Up/Down** (empty input) — recall a previous prompt; type to reset. Home jumps to the top of the transcript, End returns to the live tail
- **Mouse drag** — select text in the transcript to copy it to your clipboard
- The footer below the input box shows context-sensitive keyhints (approve / answer / interrupt / idle)

### For large codebases

Raven injects a repo symbol map for files >50KB. The map helps the agent navigate structure without reading entire files:
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

The `--yolo` flag disables confirmation entirely, but the denylist still applies as a last-resort filter. In addition to these filters, confined subprocesses run under OS-level sandboxing: **Landlock** (filesystem confinement) and **seccomp** (network-block) on Linux, plus **resource limits** (CPU / file size / fds) on Linux + macOS, and **Job Object** confinement on Windows. See [`docs/security.md`](docs/security.md) for the full threat model.

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
- **Ollama Cloud (daily-use recommended):** `kimi-k3:cloud` (latest, excellent), `deepseek-v4-pro:cloud` (high quality), `deepseek-v4-flash:cloud` (efficient), `glm-5.2:cloud` (long-horizon)
- **OpenRouter (frontier):** `x-ai/grok-4.5` (best reasoning, multimodal), `x-ai/grok-4.6` (frontier), `Stealth/ox-alpha` (pretty noice)
- **Local (when cloud unavailable):** `qwen3.8:latest`

See `docs/testing.md` for coverage and mutation testing details.

**Troubleshooting:** common failure modes (Windows `.exe`, stream errors,
sandbox denies, SearXNG fallback, ACP) are covered in
[`docs/troubleshooting.md`](docs/troubleshooting.md).

---

## Privacy & local-only operation

**No telemetry, ever.** Raven is built with privacy as a hard constraint:

- **No analytics or usage tracking** — nothing is collected, even anonymously
- **No remote call home** — all agent state stays on your machine (except LLM requests to your chosen endpoint)
- **No cloud sync** — sessions are stored locally under `.raven/sessions/`; they never leave your disk
- **No plugin marketplace** — no dependency on external services or registries
- **No auth layer** — you own the API key to your endpoint; Raven never stores it remotely

**What network access happens:**
1. **LLM requests** to your chosen endpoint (Ollama local, OpenRouter cloud, etc.) — you control this
2. **Optional** `web_search` requests to DuckDuckGo or a self-hosted SearXNG instance
3. That's it. Everything else stays local.

**Session audit trail:**
- Each session stores a local debug-events log (`debug-events.jsonl`) for reproducible debugging
- Git diffs are snapshotted (`last.patch`) so you can review or rollback changes
- All session state is stored as plain JSONL and JSON — auditable, portable, no binary blobs

---

## Project layout


```
src/
  main.rs           # CLI, TUI, headless runner, session management
  lib.rs            # Library re-exports for benchmarks/integration tests
  agent/            # Core agent loop
    core.rs         # Agent struct, LLM streaming, tool dispatch
    tools_exec.rs   # Parallel tool execution, verification gates
    stream.rs       # OpenAI-compatible response parsing
    loop_control.rs # Iteration counting, early-stop logic
    parallel.rs     # Sub-agent spawning and delegation
    types.rs        # ChatMessage, AgentEvent, AgentError
  commands.rs       # Slash-command registry (/provider, /model, /clear, etc.)
  tools/            # Tool implementations (25 total)
    definitions.rs  # JSON schema for all tools
    dispatch.rs     # Tool execution router
    sandbox/        # Sandboxing (Landlock, seccomp, Job Object, rlimits)
    git.rs          # git_status, git_diff, git_log, git_commit
  tui/              # ratatui TUI
    mod.rs          # Event loop, input handling, TUI state
    completion.rs   # Slash-command completion (commands, args, models)
    render.rs       # Terminal rendering
    markdown.rs     # Markdown parsing and rendering
  config/           # Configuration
    mod.rs          # Settings, config.toml loading, layered config
    provider.rs     # Provider presets (Ollama, OpenRouter, etc.)
  context.rs        # Context-window management and compaction
  tokenizer.rs      # Pure-Rust token counter (no vocab file)
  session.rs        # JSONL persistence, resume, list, local event logging
  plan.rs           # Structured plan mode (parse, format, execute)
  memory.rs         # Cross-session `.raven/MEMORY.md`
  state.rs          # Persistent `.raven/state/` (goals, todos)
  skills.rs         # SKILL.md discovery and skill_search/skill_load
  repomap/          # Lightweight repo symbol map for large codebases
  web.rs            # web_search and web_fetch (DuckDuckGo or SearXNG)
  error.rs          # Typed error enums with retry logic
  runner.rs         # Shared event-draining and plan-approval flow
evals/              # Agent evaluation suite
  run.py            # Test harness (fixtures, grading, reporting)
  cases/            # Eval fixtures (013+ cases covering core functionality)
docs/               # Documentation
```

---

## License

MIT
