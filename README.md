# Raven

A **privacy-first** local coding-agent harness written in Rust for [Ollama](https://ollama.com) and any OpenAI-compatible endpoint. It distills the useful core of agent-harness ideas into a single binary that runs entirely against a local (or cloud) endpoint — no managed cloud auth, no MCP marketplace, no kernel sandbox. Ideal for locked-down or air-gapped work environments where Ollama is the only reachable model endpoint.

> **Inspiration:** Inspired by the agent-harness ideas in xAI's [Grok Build](https://github.com/xai-org/grok-build); not affiliated. Raven keeps only the pieces that remain useful when the only model endpoint you can reach is local Ollama, so you get a real agent loop with tools, plan mode, compaction, and parallel sub-agents — in one small, auditable binary.

---

## Features

| In scope | Intentionally out of scope |
|---|---|
| Streaming agent loop (OpenAI-compatible `/v1/chat/completions`) | MCP server marketplace |
| 22 tools: `list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`, `search_code`, `todo_write`, `memory_update`, `memory_search`, `git_status`, `git_diff`, `git_log`, `git_commit`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`, `web_search`, `web_fetch`, `skill_search`, `skill_load` | Remote config sync |
| Document extraction: `read_file` converts `.docx`, `.pdf`, `.xlsx`, `.odt`, `.epub`, `.pptx`, `.csv`, `.rtf`, `.ods`, `.odp`, `.doc`, `.xls`, `.ppt` and more to Markdown (via the `anydoc` engine) | Multi-model routing |
| Workspace sandbox (path confinement + dangerous-command filter) | OS-level kernel sandbox (Landlock/seccomp) |
| Structured plan mode (parse → approve → revise → execute) | Worktree isolation |
| Skills (`SKILL.md` discovery + `skill_search`/`skill_load`) | Rhai workflow engine |
| Repo symbol map (`<repo_map>` injected for large workspaces) | GUI / web frontend |
| Parallel tool execution within a single model turn | Telemetry / usage tracking |
| Pure-Rust BPE tokenizer for accurate token/context counting |  |
| Context-window inference + automatic compaction (tool-result pruning) |  |
| JSONL session persistence (`--resume`, `--list-sessions`) |  |
| Cross-session project memory (`.raven/MEMORY.md`) | |
| Config file (`~/.raven/config.toml` + workspace config) | |
| Typed errors with retry + exponential backoff | |
| Non-streaming fallback (`--no-stream`) | |
| Simple ratatui TUI + headless CLI | |
| `AGENTS.md` / `CLAUDE.md` auto-load + `--rules` session overrides | |
| Local by default; optional Bearer auth for Ollama Cloud | |

---

## Requirements

- **Rust 1.85+** (latest stable recommended; pinned in `rust-toolchain.toml`)
- **Ollama** running locally (or a reachable OpenAI-compatible endpoint)
- A coding-capable model, e.g.

```bash
ollama pull qwen2.5-coder:7b
```

Suggested models: `qwen2.5-coder:7b`, `qwen2.5-coder:14b`, `llama3.1:8b`, `deepseek-coder-v2`, `codestral`.

---

## Install

### One-liner (recommended)

**Linux / macOS / Raspberry Pi:**

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
curl -fsSL .../install.sh | sh -s -- --version 0.1.0   # pin a version
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

```bash
# Interactive TUI (default when no task is given)
raven

# Headless one-shot
raven -p "Explain the structure of this repository"
raven "Add a README and .gitignore"

# NOTE: positional args only capture the first word unless quoted.
# `raven add a test` captures just "add" — use quotes or -p for multi-word tasks.
raven -p "add a test"       # recommended
raven "add a test"          # also works

# Model / workspace
raven -m qwen2.5-coder:14b -w /path/to/project -p "Fix the tests"

# Skip plan approval (full tools, no plan step)
raven --mode agent -p "Refactor utils"

# Fully autonomous (no confirmations)
raven --yolo -p "Write unit tests for auth"

# Append session-specific rules to the system prompt
raven --rules "Always use TypeScript. Prefer functional components." -p "Add a dark mode toggle"

# Parallel focused sub-agents
raven --parallel \
  "Summarize the architecture of src/" \
  "List all TODOs and FIXMEs" \
  "Check for security issues in tools.rs"

# Resume a previous session
raven --resume            # resume latest
raven --resume <id>       # resume specific session
raven --list-sessions      # list all sessions in workspace

# Override context window (default: inferred from model name)
raven --context-window 32768 --compact-threshold 0.5 -p "Analyze this large file"

# Non-streaming mode (for models that don't support SSE)
raven --no-stream -p "Hello"
```

---

## Configuration

### Config file

Layered config, highest priority wins:

1. **CLI flags** (highest)
2. **Environment variables** (`RAVEN_*` prefix, with `OLLAMA_*` / `OG_*` fallbacks)
3. **Workspace config** (`.raven/config.toml`)
4. **Global config** (`~/.raven/config.toml`)
5. **Built-in defaults** (lowest)

```toml
# ~/.raven/config.toml or .raven/config.toml
model = "qwen2.5-coder:14b"
host = "http://localhost:11434/v1"
context_window = 131072
compact_threshold = 0.75
max_iterations = 30
mode = "plan"
temperature = 0.2
no_stream = false
```

### Environment variables

`RAVEN_*` vars take priority; legacy `OLLAMA_*` / `OG_*` vars are accepted as fallbacks.

| Variable | Description | Default |
|---|---|---|
| `RAVEN_MODEL` / `OLLAMA_MODEL` | Default model name | `gemma4:latest` |
| `RAVEN_HOST` / `OLLAMA_HOST` | API endpoint | `http://localhost:11434/v1` |
| `RAVEN_API_KEY` / `OLLAMA_API_KEY` | Bearer token for authenticated hosts | none |
| `RAVEN_CONTEXT_WINDOW` / `OG_CONTEXT_WINDOW` | Override context window size | inferred from model |
| `RAVEN_COMPACT_THRESHOLD` / `OG_COMPACT_THRESHOLD` | Compaction trigger (0.0–1.0) | 0.75 |
| `RAVEN_MAX_ITER` / `OG_MAX_ITER` | Max agent iterations per run | 30 |

### Project instructions

On startup, the agent loads `AGENTS.md` (or `CLAUDE.md`) from the workspace root and injects it into the system prompt. Use `--rules` for per-session overrides.

### Project memory

The agent maintains a workspace memory file at `.raven/MEMORY.md`. The first 25KB is injected into the system prompt on each run. The `memory_update` tool lets the agent persist conventions, decisions, and context across sessions.

---

## Context management

The agent tracks token usage with a built-in BPE tokenizer (no external vocab file needed). When the conversation approaches the context window limit, it automatically:

1. **Prunes old tool results** — soft-trims tool outputs older than 3 turns (keeps head + tail with a truncation marker)
2. **Compacts the conversation** — summarizes the middle of the conversation, preserving the system message and the last ~40% of the context budget for recent messages

Context window sizes are fetched from the model's actual metadata via Ollama's `/api/show` endpoint. This returns the real `context_length` from the model file (e.g. `gemma4` → 131K, `qwen3.5` → 262K, `deepseek-v4` → 1M). If the API is unreachable (Ollama not running, model not found), a name-based heuristic is used as fallback:

- `gemma4`, `gemma3`, `qwen2.5`, `qwen3`, `llama3.1`, `llama3.2`, `deepseek`, `codestral`, `glm` → 128K
- `llama3`, `codellama` → 32K
- `mistral` → 8K
- `glm:cloud` → 1M
- Unknown models → 32K (safe default)

---

## Session persistence

Sessions are stored as JSONL under `.raven/sessions/`:

```
.raven/sessions/
  2026-08-04T12:34:56/
    summary.json    # metadata: id, model, timestamps, title
    messages.jsonl  # one ChatMessage per line
```

All writes are atomic (temp file + rename). Use `--resume` to continue a previous session or `--list-sessions` to browse.

---

## Shell safety

The `run_shell` tool uses two complementary filters, neither of which is a security boundary:

1. **Denylist** — a regex that blocks obviously destructive patterns (recursive root deletes, fork bombs, `curl | sh`, etc.). This is a **best-effort guard**, not a security boundary. A denylist is inherently incomplete — it can always be bypassed (e.g. `rm -rf ~` is not blocked even though `rm -rf /` is).

2. **Allowlist** — a regex that matches known-safe development commands (`cargo`, `git`, `npm`, `ls`, `grep`, etc.). When `confirm_shell` is enabled (the default, non-`--yolo` path), commands matching the allowlist run without a confirmation prompt. Anything outside the allowlist requires explicit user approval.

The `--yolo` flag disables confirmation entirely, but the denylist still applies as a last-resort filter. Neither mechanism replaces OS-level sandboxing (Landlock/seccomp), which is intentionally out of scope.

---

## Testing

```bash
cargo test                    # 304 tests, all offline
cargo clippy                  # zero warnings
cargo clippy -- -W clippy::pedantic  # stricter linting
```

See `docs/testing.md` for coverage and mutation testing instructions.

---

## Project layout

```
src/
  main.rs       # CLI, headless runner, session management
  agent.rs      # Streaming agent loop, retry, parallel sub-agents
  commands.rs   # Slash-command registry + parsing for the TUI
  tools/        # Tool modules: definitions, dispatch, document, git, patch, sandbox
  tui.rs        # ratatui TUI with status bar, streaming, scrollback, /commands
  config.rs     # Settings, config.toml loading, context window inference
  context.rs    # Compaction strategy, tool-result pruning
  tokenizer.rs  # Pure-Rust BPE tokenizer (no external vocab)
  session.rs    # JSONL session persistence, resume, list
  plan.rs       # Structured plan mode, parse_plan, format_plan
  memory.rs     # Cross-session MEMORY.md
  skills.rs     # SKILL.md discovery + skill_search/skill_load
  repomap.rs    # Lightweight repo symbol map
  web.rs        # Web tools (web_search, web_fetch)
  error.rs      # Typed AgentError enum
  runner.rs     # Shared event-draining and plan-approval flow
```

---

## License

MIT