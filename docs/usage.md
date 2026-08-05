# Usage guide

Day-to-day workflows for **Raven**. See the [root README](../README.md) for install and the [configuration guide](configuration.md) for all flags and env vars.

## Modes

**Raven** has three modes, chosen automatically or via flags:

| Mode | When | Flag |
|---|---|---|
| TUI | No task given (default), or `--tui` | `raven` |
| Headless | A task is given (`-p` or positional) | `raven -p "..."` |
| Parallel | `--parallel` with ≥1 tasks | `raven --parallel "A" "B"` |

Force headless with no task using `--headless` (exits with an error if no prompt is given).

---

## Headless one-shot

```bash
# Positional task
raven "Add a README and .gitignore"

# -p flag (equivalent)
raven -p "Add a README and .gitignore"

# Pick a model and workspace
raven -m qwen2.5-coder:14b -w ~/projects/myapp -p "Fix the failing tests"
```

Output is streamed to stdout: assistant text deltas, tool calls (`→ tool(args)`), tool results, and iteration markers (`[iter N]`) on stderr.

---

## Plan mode

Plan mode asks the model to propose a plan first, then proceeds to execution.

```bash
# Plan mode is ON by default
raven -p "Refactor the auth module"

# The model proposes a plan. If it signals completion (exit_plan_mode), the
# harness auto-executes it without prompting. Otherwise you see:
#   ── Plan ready. Approve? [Y/n] ──
# Press Enter (or y/yes/ok) to execute; anything else aborts.

# Skip planning entirely
raven --no-plan -p "Fix the typo in config.rs"

# Skip ALL confirmations (fully autonomous)
raven --yolo -p "Write unit tests for auth"
```

`--yolo` implies `--no-plan` (no approval step).

### Model-driven auto-execution

Plan mode transitions out automatically (Grok Build–style): during the
plan-proposal turn the agent may call the `exit_plan_mode` tool to signal its
plan is complete. When it does, the harness parses the plan, prints it, and
immediately proceeds to execution — no human approval prompt blocks the flow.
If the model finishes without calling `exit_plan_mode` (some models ignore
tool calls), the harness falls back to prompting you to approve/revise/abort.

### Plan-mode tool restriction

During the plan-proposal turn (and any revision turn), the agent is limited to a **read-only toolset** so it can gather context without modifying the workspace before you approve:

- `list_dir`, `read_file`, `grep`, `search_code`, `git_status`, `git_diff`, `git_log`

The mutating and shell tools (`write_file`, `search_replace`, `run_shell`, `todo_write`, `memory_update`, `apply_patch`, `run_tests`) are **not advertised** to the model during planning, so it physically cannot change files or run commands until you approve. Only after approval does the full toolset become available.

---

## Parallel sub-agents

Run N independent agent tasks concurrently. Each gets a fresh conversation; results are printed in order.

```bash
raven --parallel \
  "Summarize the architecture of src/" \
  "List all TODO comments" \
  "Suggest three error-handling improvements"
```

Output:

```
Running 3 parallel sub-agents…

══ Sub-agent 0 ══
<summary of src/ architecture>

══ Sub-agent 1 ══
<list of TODOs>

══ Sub-agent 2 ══
<suggestions>
```

Use cases: codebase exploration, multi-perspective review, independent research tasks.

---

## TUI

```bash
raven          # launches TUI if no task given
raven --tui    # force TUI even with a task
```

| Key | Action |
|---|---|
| Type + `Enter` | Submit a task |
| `Backspace` | Edit input |
| `Ctrl+P` | Toggle plan mode (also `/plan`) |
| `Ctrl+C` / `Esc` | Quit |

### Slash commands

Raven uses slash commands (Grok Build-style) to drive the TUI. They work
identically in editor-like terminals where `Ctrl+` shortcuts collide with the
host. Type one and press `Enter`; `/help` lists everything.

| Command | Aliases | Action |
|---|---|---|
| `/help [cmd]` | `/h`, `/?` | List all commands, or detail one |
| `/plan` | `/p` | Toggle plan-first mode |
| `/new` | `/n` | Save the current session and start a fresh one |
| `/clear` | `/c` | Clear the on-screen log (history preserved) |
| `/model <name>` | `/m` | Switch the model for subsequent turns |
| `/quit` | `/q`, `/exit` | Quit Raven |

The command registry lives in `src/commands.rs`; adding a command is one
registry entry plus one TUI dispatch arm, and it auto-appears in `/help`.

### TUI limitations

- Each submission spawns a **fresh agent** — conversation history is not carried across turns.
- Plan approval is heuristic: type `yes`/`y`/`approve`/`go`/`execute`/`ok` to execute, or any other text to revise.
- No scrollback navigation or multi-line input.

---

## Session rules

Append rules to the system prompt for a single session without editing files:

```bash
raven --rules "Always use TypeScript. Prefer functional components." -p "Add a dark mode toggle"
raven --rules "Run cargo fmt before considering the task done." -p "Refactor utils"
```

Rules are appended under a `--- Session rules ---` header, after any `AGENTS.md` content.

---

## Remote / cloud Ollama

```bash
# Point at a GPU box on the LAN
raven --host http://gpu-box:11434/v1 -p "Explain this repo"

# Ollama Cloud (authenticated)
export RAVEN_API_KEY="your-key"
export RAVEN_HOST="https://ollama.com/v1"
raven -m llama3.1 -p "Explain this repo"
```

See [configuration.md](configuration.md#api-keys) for security notes.

---

## Context window tuning

If the model errors with "context length exceeded", tune compaction:

```bash
# Compact earlier (default 0.75)
raven --compact-threshold 0.5 -p "Refactor the whole codebase"

# Override the inferred window
raven --context-window 32768 -p "..."
```

See [architecture.md#compaction](architecture.md#compaction) for how it works.