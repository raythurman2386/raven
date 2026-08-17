# Contributing

How to build, test, and extend **Raven**.

## Build

```bash
# Debug build
cargo build

# Release build (LTO + strip)
cargo build --release

# Run
./target/release/raven --help

# Docs
cargo doc --no-deps
```

## Style

- Rust 2021 edition. Target MSRV: 1.88+ (pinned in `rust-toolchain.toml`).
- Keep the binary small and dependency-light. No MCP, no telemetry.
- Every public struct, enum, and fn should have a doc comment.
- `cargo doc --no-deps` must build with no warnings.
- `cargo build` must build with no warnings.
- Prefer `anyhow` for error handling in the binary; `thiserror` is available if a crate-level error type is needed.

## Project layout

```
src/
├── main.rs       # CLI entry, headless runner
├── lib.rs        # Library crate re-exports
├── config/mod.rs  # Settings, defaults, context-window inference, AGENTS.md loader
├── agent/       # Streaming loop (core, stream, tools_exec, loop_control, parallel, types)
├── context.rs    # Token estimation, compaction
├── tools/
│   ├── mod.rs        # Tool module root, glob matcher, todo_write
│   ├── definitions.rs # OpenAI function-calling tool schemas
│   ├── dispatch.rs    # Tool dispatch by name
│   ├── sandbox.rs     # Sandbox (path confinement, shell filtering, file ops)
│   ├── document.rs    # Document extraction (.docx, .pdf, .xlsx, .odt, .epub)
│   ├── git.rs         # Git operations (status, diff, log, commit, undo)
│   └── patch.rs       # Unified diff parsing and application
├── tui/          # ratatui interactive UI (mod, render, markdown, blocks, status, selection)
├── commands.rs   # Slash-command registry and parsing
├── plan.rs       # Structured plan data model, parsing, step advancement
├── skills.rs     # SKILL.md discovery + skill_search/skill_load
├── session.rs    # JSONL session persistence, resume, list
├── memory.rs     # Project memory (MEMORY.md) loading, update, search
├── state.rs      # Persistent agent state (.raven/state/todos.json + goal.json)
├── repomap/mod.rs # Lightweight repo symbol map
├── tokenizer.rs  # Pure-Rust token estimator
├── web.rs        # Keyless web tools (web_fetch, web_search)
├── error.rs      # Typed error enums (AgentError, ToolError)
└── runner.rs     # Shared event-draining and plan-approval flow
docs/
├── README.md           # index
├── usage.md            # user workflows
├── configuration.md    # env, flags, context, keys
├── architecture.md     # design, agent loop, compaction, sandbox
├── tools.md            # tool contracts and sandbox rules
├── testing.md          # test structure, coverage, mutation testing
└── contributing.md     # this file
```

---

## Adding a tool

Tools live in [`src/tools/`](../src/tools/). To add a new tool:

### 1. Implement the method on `Sandbox`

```rust
impl Sandbox {
    pub fn my_tool(&self, arg: &str) -> Result<String> {
        // Resolve paths via self.safe_resolve() to stay workspace-confined.
        // Return a human-readable string result.
        todo!()
    }
}
```

### 2. Add the OpenAI function schema to `tool_definitions`

In [`src/tools/definitions.rs`](../src/tools/definitions.rs), add a new entry to the `tool_definitions()` function:

```rust
pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        // ...existing tools...
        {
            "type": "function",
            "function": {
                "name": "my_tool",
                "description": "What it does (the model reads this).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "arg": { "type": "string" }
                    },
                    "required": ["arg"]
                }
            }
        }
    ])
}
```

### 3. Add a dispatch arm

In [`src/tools/dispatch.rs`](../src/tools/dispatch.rs), add a match arm to the `dispatch()` function:

```rust
pub fn dispatch(sandbox: &Sandbox, name: &str, args: &serde_json::Value) -> String {
    let res = match name {
        // ...existing arms...
        "my_tool" => {
            let arg = args.get("arg").and_then(|v| v.as_str()).unwrap_or("");
            sandbox.my_tool(arg)
        }
        // ...
    };
    match res {
        Ok(s) => s,
        Err(e) => format!("Tool error: {}", e),
    }
}
```

### 4. Document it

- Add a row to the tools table in [docs/tools.md](tools.md).
- Add a doc comment to the `Sandbox` method.

### 5. Build and verify

```bash
cargo build
cargo doc --no-deps
./target/release/raven --help
```

---

## Adding an event

Events are defined in [`src/agent/types.rs`](../src/agent/types.rs) as the `AgentEvent` enum.

### 1. Add the variant

```rust
pub enum AgentEvent {
    // ...existing variants...
    /// Describe when it fires.
    MyEvent { detail: String },
}
```

### 2. Emit it in `Agent::run`

```rust
let _ = tx.send(AgentEvent::MyEvent { detail: "..." }).await;
```

### 3. Handle it in consumers

- **Headless runner** (`main.rs`): add a match arm in the `while let Some(ev) = rx.recv().await` loop.
- **TUI** (`src/tui/`): add a match arm in the `while let Ok(ev) = rx.try_recv()` loop.

### 4. Document it

- Add a doc comment to the variant.
- Update [docs/architecture.md](architecture.md) if it affects the loop design.

---

## Adding a slash command

Slash commands live in [`src/commands.rs`](../src/commands.rs). The registry
is the single source of truth, so `/help` auto-lists any command you add.

### 1. Add a `CommandSpec` to the registry

```rust
CommandSpec {
    name: "mycmd",
    aliases: &["m"],
    summary: "What it does (shown in /help)",
    arg_help: None, // or Some("[arg]")
},
```

### 2. Handle it in the TUI dispatcher

In [`src/tui/mod.rs`](../src/tui/mod.rs), add a match arm in `dispatch_slash_command`.
It receives the parsed command, shared UI state (`log`, `mode`,
`session`, `quit`, ...), and `&SessionStore` — push any user-visible feedback
to `log`.

### 3. Document it

- Add a row to the slash-command table in [docs/usage.md](usage.md).

---

## Changing compaction

Compaction lives in [`src/context.rs`](../src/context.rs).

- **Threshold**: `compact_threshold` in `Settings` (default `0.75`). Change the default in `config/mod.rs` or via `--compact-threshold` / `RAVEN_COMPACT_THRESHOLD` / `OG_COMPACT_THRESHOLD`.
- **Trailing budget**: hardcoded at 40% of usable context in `compact_if_needed`. Change the `0.40` factor.
- **Summary cap**: `MAX_SUMMARY_CHARS` (4000) and `MAX_TOOL_BODY_CHARS` (200) in `build_summary_user`.
- **Token estimate**: implemented by the pure-Rust token estimator in `src/tokenizer.rs` (`count_tokens`). Non-newline whitespace is free (BPE glues a leading space to the following word) and a ~12% structural-overhead factor keeps the estimate biased slightly above the real count so compaction triggers early rather than late.

### Invariants to preserve

- The system message (index 0) is **never** dropped.
- Tool-call / tool-result pairs must stay together (see `find_safe_tail_start`).

---

## Running tests

```bash
cargo test                    # 570+ tests, all offline
cargo clippy --all-targets -- -D warnings
cargo fmt
```

See [testing.md](testing.md) for coverage and mutation-testing instructions.

When adding tests, prefer testing pure functions (`count_tokens`, `infer_context_window`, `derived_max_tokens`, `glob_segment_match`, `search_replace`, `safe_resolve`, compaction) over the async agent loop.