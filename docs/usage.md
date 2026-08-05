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
| `/stop` | `/s` | Interrupt the running task |
| `/quit` | `/q`, `/exit` | Quit Raven |

### Interrupting and steering

While a task is running you can still type:

- `/stop` (or `/s`) aborts the current turn immediately.
- Any non-command text followed by `Enter` **steers**: it interrupts the
  current turn and starts a new one with that instruction appended to the
  session, so the agent redirects with full context instead of losing the
  conversation.

The command registry lives in `src/commands.rs`; adding a command is one
registry entry plus one TUI dispatch arm, and it auto-appears in `/help`.

### Tool activity is collapsed

Tool calls are shown live in the status strip (`⇢ read_file(...)`) while they
run, but are **not** spammed into the conversation log line-by-line. When a
turn finishes, the log records a single compact summary line
(`⇢ 8 tool calls this turn`), keeping the log focused on user/assistant text
and plan content.

### Asking you questions mid-task (`ask_user`)

The agent has an `ask_user` tool: when it needs a decision or clarification it
cannot resolve from the workspace, it stops and asks. In the TUI the input box
repurposes to show the question; type your answer and press Enter to continue
the task. In headless mode the question is printed to stderr and your answer
is read from stdin. If you dismiss/close without answering, the agent is told
no answer was provided and proceeds (or re-decides) on its own.

The same prompt is used as a **shell permission gate**: unless you run with
`--yolo`, every `run_shell` command is confirmed first. Type `y`/`yes` to
allow; anything else (or dismissing) blocks the command and tells the model it
was declined.

### Web research (`web_search` / `web_fetch`)

The agent can research the web with no API key: `web_search(query)` returns a
ranked list of result titles and URLs (via DuckDuckGo's HTML endpoint), and
`web_fetch(url)` retrieves a page and strips the HTML to readable text. Only
`http`/`https` URLs are allowed — `file://`, `data://`, etc. are rejected so
the tools can't read local files. Both are read-only and available during
planning, and their output is capped.

### Skills (`skill_search` / `skill_load`)

Skills are reusable instruction files — `SKILL.md` with YAML frontmatter
(`name`, `description`) and a markdown body. Drop them under `.raven/skills/`
(project) or `~/.raven/skills/` (global), e.g.:

```text
.raven/skills/commit/SKILL.md
```

The agent can discover them with `skill_search` (match by name or
description) and load one into context with `skill_load`, which returns the
body wrapped in a `<skill>` envelope. Both are read-only and available during
planning.

### Repo symbol map (`<repo_map>`)

For large workspaces (≥50 source files) Raven injects a compact `<repo_map>`
into the system prompt: a list of `symbol — path:line` declarations extracted
via per-language regex (fn/struct/enum/impl/class/function/etc.). This lets
the agent know the codebase structure up front instead of burning turns
listing and reading files. Small workspaces skip the map, and its output is
capped at ~2K chars.

### Memory recall (`memory_search`)

The agent can recall past decisions and conventions with `memory_search(query)`,
which keyword-scans `.raven/MEMORY.md` and returns the matching lines as ranked
`path:line — content` snippets (lines with more query-token hits rank first).
Grok Build uses indexed keyword + vector search; a mini harness gets the
high-value subset with a dependency-light keyword scan of the single memory
file. Read-only and available during planning.

### Git checkpointing (`git_commit`) and `/undo`

The agent can checkpoint its own work with `git_commit(message)`, which stages
all changes (`git add -A`) and commits them. It only appears in the **full**
toolset — never during planning, since it mutates the repo.

To step back from the last commit, use the `/undo` (or `/u`) slash command in
the TUI. It runs `git reset --soft HEAD~1`, which undoes the commit while
**keeping all changes in the working tree** — nothing is lost, and you can
re-commit once the agent's next move is clearer.

### Context compaction (LLM-structured)

When history grows past the soft limit, Raven **summarizes the middle turns
with the model** (a dedicated non-streaming summarization request) and keeps a
recent tail, so the agent retains more signal per token than the old
extractive (message-joins) summarizer. The system message is always preserved,
and tool-call/tool-result pairs are never split. If the summarization request
fails, Raven falls back to the extractive summarizer so compaction always
proceeds.

### Auto-lint reflection (`run_lint`)

The agent has a `run_lint` tool (auto-detects `cargo clippy` / `tsc` /
`eslint` / Python `compileall` and reports problems without fixing them). After
a turn that edited files (`write_file`/`search_replace`/`apply_patch`), Raven
automatically runs the linter and feeds any errors back to the model as a
reminder on the *next* request — so the agent self-corrects before you see the
damage. On a clean lint pass nothing is injected, keeping the loop quiet.

### TUI limitations

- Each submission spawns a **fresh agent** — conversation history is not carried across turns.
- Plan approval is heuristic: type `yes`/`y`/`approve`/`go`/`execute`/`ok` to execute, or any other text to revise.
- Scrollback is limited to the on-screen log window.

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