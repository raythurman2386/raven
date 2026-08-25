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

### ACP (editor attachment)

`raven --acp` speaks [Agent Client Protocol](https://agentclientprotocol.com/) v1
on stdin/stdout. Point an ACP-capable editor at the `raven` binary with the
`acp` flag (for example Zed's custom agent command: `raven --acp`).

Supported: `initialize`, `authenticate`, `session/new`, `session/prompt`,
`session/cancel`, `session/load` (replays history), `session/resume`,
`session/list`, `session/close`, `session/set_mode` (`plan` / `agent` / `chat`),
`session/set_config_option` (`mode` and `model`), `session/set_model`.
`ask_user` and shell confirmation become
`session/request_permission`. `initialize` advertises a single `agent`-type
auth method (`agent-auth`): credentials are already resolved in-process, so
`authenticate` is a no-op acknowledgement.

Not advertised: MCP servers, images/audio, client `fs/*` / `terminal/*`.
Raven keeps its own sandbox. Other CLI flags (`--provider`, `--model`,
`--workspace`, `--yolo`, `--mode`) still apply to the ACP process.

### Interaction modes

Within a session, Raven runs in one of three **interaction modes**, which control
whether the model proposes a plan first and which tools it can use:

| Mode | Plan first? | Toolset | Use case |
|---|---|---|---|
| `plan` | Yes | Read-only | Propose a plan, approve, then execute (default) |
| `agent` | No | Full | Work directly with all tools |
| `chat` | No | Read-only | Q&A / exploration without modifying the workspace |

Choose the mode with `--mode <plan|agent|chat>` on the CLI, or cycle with
`Shift+Tab` in the TUI. The default is `plan`.

```bash
raven --mode agent -p "Refactor the auth module"   # full tools, no plan
raven --mode chat -p "Explain the architecture"     # read-only, no plan
```

`--yolo` implies `--mode agent` (no plan step, no confirmations).

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

# The model proposes a plan, then you approve it:
#   ── Plan ready. Approve? [Y/n] ──
# Press Enter (or y/yes/ok) to execute; anything else aborts.

# Skip planning entirely (full tools, no plan step)
raven --mode agent -p "Fix the typo in config.rs"

# Skip ALL confirmations (fully autonomous)
raven --yolo -p "Write unit tests for auth"
```

`--yolo` implies `--mode agent` (no approval step).

### Plan approval

Plan mode always requires your approval before execution. When the plan turn
finishes, the harness parses and prints the plan, then prompts you to
approve/revise/abort. Type `y`/`yes`/`ok` (or press Enter) to execute, `n` to
abort, or any other text to revise the plan.

### Plan-mode tool restriction

During the plan-proposal turn (and any revision turn), the agent is limited to a **read-only toolset** so it can gather context without modifying the workspace before you approve:

- `list_dir`, `read_file`, `grep`, `search_code`, `git_status`, `git_diff`, `git_log`
- `web_search`, `web_fetch`, `skill_search`, `skill_load`, `memory_search`, `think`

The mutating and shell tools (`write_file`, `search_replace`, `run_shell`, `todo_write`, `goal_set`, `delegate_task`, `memory_update`, `apply_patch`, `run_tests`, `run_lint`, `ask_user`) are **not advertised** to the model during planning, so it physically cannot change files or run commands until you approve. Only after approval does the full toolset become available.

---

## Parallel sub-agents

Run N independent agent tasks concurrently. Each gets a fresh conversation;
in a git workspace each also gets an isolated worktree. Results are printed
in order. Sub-agent diffs are applied to your working tree without committing.

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
| `Left` / `Right` | Move the edit cursor |
| `Up` / `Down` | Recall prior prompts when the input is empty or mid-recall; otherwise scroll the transcript. With the completion popup open, move the highlight |
| `Home` / `End` | Empty input: jump to the top of the transcript / reattach to the live tail. Otherwise: jump to start / end of the input line |
| `PgUp` / `PgDn` | Scroll the transcript |
| Mouse wheel | Scroll the transcript (or the plan panel when the pointer is over it). Never walks prompt history |
| Mouse drag | Select transcript text to copy |
| `Tab` | Cycle slash-command autocomplete (accept if one match) |
| `Shift+Tab` | When idle: cycle completion backward, or cycle mode when no completion is open |
| `Backspace` | Delete the char before the cursor |
| `Ctrl+C` / `Esc` | Quit (Esc first dismisses completion, selection, or an `ask_user` prompt) |

Assistant responses render as markdown — headings, bold/italic, code blocks,
lists, tables, and links are styled in the terminal. Tool calls show a live
spinner while running, then settle to a dim line once finished.

### Slash commands

Raven uses slash commands (Grok Build-style) to drive the TUI. They work
identically in editor-like terminals where `Ctrl+` shortcuts collide with the
host. Type one and press `Enter`; `/help` lists everything.

| Command | Aliases | Action |
|---|---|---|
| `/help [cmd]` | `/h`, `/?` | List all commands, or detail one |
| `/new` | `/n` | Save the current session and start a fresh one |
| `/clear` | `/c` | Clear the on-screen log (history preserved) |
| `/model <name>` | `/m` | Switch the model for subsequent turns |
| `/theme [name]` | `/t` | List themes, or switch the active color theme |
| `/stop` | `/s` | Interrupt the running task |
| `/provider [name]` | `/p` | List providers, or switch the active one |
| `/export [dir]` | `/x` | Write a local Markdown/JSON bundle of this session |
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

### Git inspect (`git_status`, `git_diff`, `git_log`)

The agent can inspect the repo with `git_status`, `git_diff`, and `git_log`.
It does **not** create commits on its own — no `git_commit` tool, and no
auto-checkpoint when the iteration budget is exhausted. Dirty work stays in
the working tree for you to review. Only create a commit if you explicitly
ask.

If a parallel sub-agent's diff cannot be applied, a recovery patch is
written to `.raven/recovery-sub-N.patch` and indexed in `.raven/RECOVERY.md`
(`git apply .raven/recovery-sub-N.patch`). After each TUI turn that produced
a git diff, a snapshot is saved to `.raven/sessions/<id>/last.patch`.

### Long-horizon task management (`goal_set`, `todo_write`, `delegate_task`, `think`)

For multi-step tasks, Raven keeps the agent on track with persistent state and
context hygiene:

- **`goal_set(description)`** records the current objective to
  `.raven/state/goal.json`. It is injected into the system prompt each turn, so
  the agent re-anchors on its goal even after context compaction or a session
  resume.
- **`todo_write(todos)`** maintains a structured task list persisted to
  `.raven/state/todos.json` (full-replace semantics). The pending items are
  injected into the system prompt, and from iteration 4 the harness injects a
  reminder restating the goal and the next pending task.
- **`delegate_task(description)`** spawns a focused sub-agent in a **fresh
  context window** and returns a distilled summary, so exploration or isolated
  work doesn't bloat the main conversation.
- **`think(thought)`** is a read-only scratchpad for structured mid-task
  reasoning (useful across long chains of tool calls).

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

### Enforced verification (`--no-verify`)

Raven enforces that the agent runs the test suite after editing files before it
can finish a turn. When a turn calls `write_file`/`search_replace`/`apply_patch`
and the model finishes **without** calling `run_tests`, Raven injects a recovery
reminder and re-runs the turn (capped at 3 attempts) instead of finishing — so
the agent cannot "forget" to verify its changes. This mirrors Grok Build's
`CompletionRequirement` (must-call-tool + recovery re-run).

- **On by default** in both the TUI and headless mode.
- Disable with `--no-verify` on the CLI, or `verify = false` in
  `.raven/config.toml` / `~/.raven/config.toml`.
- In the TUI, a `⟳ verify required` line appears in the log and the model's
  `run_tests` call shows in the status strip with the spinner. In headless mode,
  a `[verify required]` notice is printed.
- The gate only fires when the turn actually edited files — read-only and
  Q&A turns never trigger it, so it stays efficient.

### Session durability

Session files are written atomically with a **unique temp name**
(`.{pid}.{counter}.tmp` + rename) so concurrent writers — like a running turn
and a `/stop` flush — can never clobber each other's in-flight temp file.
Interrupting a task with `/stop` now **persists the partial turn** (`/stop`
saves what the interrupted turn already produced) instead of dropping it, and
quitting (Ctrl+C/Esc/`/quit`) always flushes the session before exit.

### TUI limitations

- Each submission spawns a **fresh agent** — conversation history is carried across turns via in-memory session messages and persisted to `.raven/sessions/`.
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
# Point the ollama provider at a GPU box on the LAN (via config.toml)
#   [providers.ollama]
#   base_url = "http://gpu-box:11434/v1"
raven -p "Explain this repo"

# Ollama Cloud (authenticated)
export OLLAMA_API_KEY="your-key"
#   [providers.ollama]
#   base_url = "https://ollama.com/v1"
raven -m llama3.1 -p "Explain this repo"

# Or switch to the openrouter provider entirely
export OPENROUTER_API_KEY="sk-or-..."
raven --provider openrouter -p "Explain this repo"
```

See [configuration.md](configuration.md#providers) for the full provider model.

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