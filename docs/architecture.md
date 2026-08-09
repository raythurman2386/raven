# Architecture

Design overview for **Raven**. See the [project layout](../README.md#project-layout) for a summary, and [contributing.md](contributing.md) for how to extend the codebase.

## High-level data flow

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

### Step-by-step

1. **CLI** (`main.rs`) parses flags with `clap`, builds a [`Settings`](../src/config.rs) struct (resolving env vars, loading config files, and querying the model's actual context window via Ollama's `/api/show` endpoint).
2. **Agent construction** (`Agent::new`): validates the workspace, builds the system prompt (`SYSTEM_BASE` + workspace root + `AGENTS.md` + `--rules`), and seeds `messages[0]` as the system message.
3. **Agent loop** (`Agent::run`): appends the user message, then loops up to `max_iterations`:
   - **Compaction check**: estimate history tokens; if over the soft limit, summarize the middle (see [Compaction](#compaction)).
   - **Clamp `max_tokens`**: so `prompt_tokens + max_tokens + 64 ≤ context_window`.
   - **Stream completion**: `POST {base_url}/chat/completions` with `stream: true`, parsing SSE `data:` lines.
   - **Accumulate tool calls**: tool-call deltas arrive incrementally; they are accumulated by index into `(id, name, arguments)`.
   - **No tool calls**: append the assistant message, emit `Done`, return.
   - **Tool calls**: append the assistant message, execute all tools in parallel via `tokio::task::spawn_blocking`, append each result as a `tool`-role message, loop back.
4. **Events**: progress flows through an `mpsc` channel as [`AgentEvent`](../src/agent.rs) variants. The headless runner and TUI consume these.

---

## The agent loop

```
┌─────────────────────────────────────────────────┐
│ Agent::run(user_text)                           │
│   messages.push(user)                           │
│   for iter in 0..max_iterations:                │
│     compact_if_needed(messages)                 │
│     clamp max_tokens                             │
│     stream completion ──┐                       │
│                         ▼                        │
│     accumulate content + tool_calls             │
│     if no tool_calls:                            │
│       messages.push(assistant)                   │
│       emit Done ── return                        │
│     else:                                        │
│       messages.push(assistant with tool_calls)   │
│       for each tool_call (parallel):            │
│         dispatch(sandbox, name, args)            │
│         messages.push(tool result)               │
│       loop ───────────────────────────────────── │
│   emit Error("max iterations")                   │
└─────────────────────────────────────────────────┘
```

### Invariants

- `messages[0]` is **always** the system message. Compaction never drops it.
- Tool-call / tool-result pairs are kept together during compaction.
- The assistant message with `tool_calls` is appended before tool results, so the conversation stays well-formed for the OpenAI API.

---

## Compaction

Implemented in [`context.rs`](../src/context.rs). Token counting uses a built-in token estimator ([`tokenizer.rs`](../src/tokenizer.rs)) for fast, conservative estimates.

Compaction is guaranteed to never grow the history: if the assembled compacted form (summary + kept tail) would be no smaller than the original, the original is left unchanged. This guards the degenerate case of many tiny, near-identical messages where the extractive summary's per-line prefixes can cost more than the verbatim middle they replace.

### When it triggers

When `history_tokens(messages) > compact_threshold × (context_window − output_reserve)`:

- `output_reserve` = `context_window / 8` (clamped to ≥ 1024)
- Default `compact_threshold` = `0.75`

### What it does

1. **Keep the system message** (index 0) — always.
2. **Soft-prune old tool results** — trim tool outputs older than 3 turns (keep head 1500 + tail 1500 chars with a truncation marker). If this brings tokens under the limit, stop here.
3. **Compute a trailing budget** = ~40% of `(context_window − output_reserve)`.
4. **Find the tail**: walk backward from the end, accumulating messages until the trailing budget is exceeded. Adjust the start so tool-call/tool-result pairs aren't split (see `find_safe_tail_start`).
5. **LLM summarization**: the middle messages are sent to the model in a dedicated non-streaming request for summarization (max ~150 words). If the LLM call fails, an extractive fallback summarizer condenses the middle into a synthetic user message (`[Compacted conversation summary]`) + an assistant acknowledgement. User asks, assistant actions, tool names, and truncated tool bodies are captured. Summary is capped at 4000 chars.
6. **Replace history**: `[system, summary_user, summary_assistant, ...tail]`.
7. **Emit a `Compacted` event** with `before_tokens` / `after_tokens`.

### Limitations

- The heuristic overestimates tokens (by design), so compaction may trigger slightly earlier than strictly necessary.
- Summaries are lossy — the model loses exact details of compacted turns.
- There is no re-summarization of prior summaries (each compaction summarizes the then-current middle fresh).

---

## Sandbox

Implemented in [`src/tools/`](../src/tools/). See [tools.md](tools.md) for the full tool contracts.

### Path confinement

All file tools confine paths to the workspace root. On Linux, file opens go
through `openat2` with `RESOLVE_BENEATH | NO_MAGICLINKS`, which makes the
kernel refuse any path escaping the workspace — atomically, with no TOCTOU
race (see [security.md §1](security.md)). On non-Linux platforms, and for the
workspace-relative path computation that feeds `open_beneath`, paths are
resolved via `Sandbox::safe_resolve`, which applies two defenses:

1. **Lexical normalization** — `.` and `..` components are resolved in-memory, and the result must still start with the workspace root. This rejects `../` traversal for both existing and non-existent targets.
2. **Symlink escape defense** — the nearest existing ancestor of the requested path is canonicalized (resolving symlinks) and must still lie inside the canonicalized workspace root. This blocks `workspace/link -> /etc` from being read or written through (including writes whose parent directory is a symlink pointing outside the workspace). The remaining non-existent suffix is re-appended to the canonical anchor to form the target.

### Shell sandboxing

`run_shell`:

- Forces `cwd` to the workspace.
- Strips secret env vars (`RAVEN_API_KEY`, `OLLAMA_API_KEY`, `OPENAI_API_KEY`, `XAI_API_KEY`, `ANTHROPIC_API_KEY`, `AWS_SECRET_ACCESS_KEY`).
- Blocks destructive command patterns (see [tools.md#blocked-commands](tools.md#blocked-commands)).
- Runs allowlisted, metacharacter-free commands via **direct exec** (`Command::new(bin).args(...)`, no `sh -c`) — see [security.md §6](security.md).
- Enforces a timeout (default 60s, overridable per call).
- Caps output at 12 000 chars.
- Every confined subprocess additionally runs under OS-level confinement: **Landlock** (filesystem) + **seccomp** (network-block) + **rlimits** (CPU/file-size/fds) on Linux; rlimits on macOS; **Job Object** (process-tree + committed-memory) on Windows. See [security.md](security.md) for the full defense layers.

### Honest limits

The sandbox confines the agent's subprocesses at the OS level (Landlock, seccomp, rlimits, Job Objects) and confines file-path resolution with `openat2`/`safe_resolve`. It does **not** use containers or VMs. These layers are best-effort on some platforms and each has documented caveats (see [security.md](security.md)); defense-in-depth is the point. For the strongest isolation, run Raven inside a container or VM.

---

## Parallel tool execution

When the model returns multiple tool calls in one turn, they are executed concurrently:

```rust
for tc in &tcs {
    let sandbox = self.sandbox.clone();
    let name = tc.function.name.clone();
    let id = tc.id.clone();
    handles.push(tokio::task::spawn_blocking(move || {
        let result = dispatch(&sandbox, &name, &args);
        (id, name, result)
    }));
}
```

Each `dispatch` is sync, so `spawn_blocking` moves it off the async runtime. Results are collected in order and appended as `tool`-role messages.

---

## Parallel sub-agents

`run_parallel` spawns N independent `Agent`s, each with a fresh conversation. Tool events are consumed silently; only `TextDelta` output is accumulated and returned in order.

---

## TUI limitations

The TUI (`src/tui/`) is intentionally minimal:

- **Plan approval is model-driven with a fallback**: during the plan turn the model may call `exit_plan_mode`; when it does the TUI auto-executes the plan without a prompt. If the model finishes without that signal, the TUI sets `plan_pending` and waits for `yes`/`y`/`approve`/`go`/`execute`/`ok` to execute, or any other text to revise.
- **No multi-line input**: the input box is single-line only.

Assistant output is rendered as markdown (`src/tui/markdown.rs`, via
`pulldown-cmark`): headings, bold/italic/strikethrough, inline code, fenced
code blocks, ordered/unordered lists, blockquotes, links, and tables. The
renderer re-parses the accumulated text on each stream delta and degrades
unclosed tokens (e.g. a half-typed `**bold`) to literal text, so streaming
never flashes raw markdown. Tool calls render as a live line with a spinner
while active, then settle to a dim line once finished.

Conversation history **is** carried across turns via `session_messages` (in-memory) and persisted to `.raven/sessions/`. Scrollback is supported with `↑`/`↓`/`PgUp`/`PgDn` and mouse wheel.