# Raven

A **privacy-first** local coding-agent harness written in Rust for [Ollama](https://ollama.com) and any OpenAI-compatible endpoint. It distills the useful core of agent-harness ideas into a single binary that runs entirely against a local (or cloud) endpoint — no managed cloud auth, no MCP marketplace, no kernel sandbox. Ideal for locked-down or air-gapped work environments where Ollama is the only reachable model endpoint.

> **Inspiration:** Inspired by the agent-harness ideas in xAI's [Grok Build](https://github.com/xai-org/grok-build); not affiliated. Raven keeps only the pieces that remain useful when the only model endpoint you can reach is local Ollama, so you get a real agent loop with tools, plan mode, compaction, and parallel sub-agents — in one small, auditable binary.

---

## Features

| In scope | Intentionally out of scope |
|---|---|
| Streaming agent loop (OpenAI-compatible `/v1/chat/completions`) | MCP server marketplace |
| 14 tools: `list_dir`, `read_file`, `search_replace`, `write_file`, `grep`, `run_shell`, `search_code`, `todo_write`, `memory_update`, `git_status`, `git_diff`, `git_log`, `apply_patch`, `run_tests` | Skills / plugin system |
| Workspace sandbox (path confinement + dangerous-command filter) | OS-level kernel sandbox (Landlock/seccomp) |
| Structured plan mode (parse → approve → revise → execute) | Worktree isolation |
| Lightweight parallel sub-agents (`--parallel`) | Web search / web fetch |
| Parallel tool execution within a single model turn | GUI / web frontend |
| Pure-Rust BPE tokenizer for accurate token/context counting | Multi-model routing |
| Context-window inference + automatic compaction (tool-result pruning) | Remote config sync |
| JSONL session persistence (`--resume`, `--list-sessions`) | Telemetry / usage tracking |
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

## Install & build

```bash
cd ollama-grok-rs
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

# Model / workspace
raven -m qwen2.5-coder:14b -w /path/to/project -p "Fix the tests"

# Skip plan approval
raven --no-plan -p "Refactor utils"

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
plan_first = true
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

- `gemma4`, `gemma3`, `qwen2.5`, `qwen3`, `llama3.1`, `llama3.2`, `deepseek`, `codestral` → 128K
- `llama3`, `codellama` → 32K
- `mistral` → 8K
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

## Testing

```bash
cargo test                    # 105 tests, all offline
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
  tools.rs      # 14 tools + workspace sandbox (path confinement, shell filter)
  tui.rs        # ratatui TUI with status bar, streaming, scrollback
  config.rs     # Settings, config.toml loading, context window inference
  context.rs    # Compaction strategy, tool-result pruning
  tokenizer.rs  # Pure-Rust BPE tokenizer (no external vocab)
  session.rs    # JSONL session persistence, resume, list
  plan.rs       # Structured plan mode, parse_plan, format_plan
  memory.rs     # Cross-session MEMORY.md
  error.rs      # Typed AgentError enum
```

---

## License

MIT