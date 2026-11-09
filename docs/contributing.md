# Contributing

How to build, test, and extend **Raven**.

## Build

```bash
# Debug build (every member)
cargo build --workspace

# Release build (LTO + strip). Installs from the root package.
cargo build --release
cargo install --path .

# Run
./target/release/raven --help

# Docs
cargo doc --workspace --no-deps
```

## Style

- Rust 2021 edition. Target MSRV: 1.88+ (pinned in `rust-toolchain.toml`).
- Keep the binary small and dependency-light. Thin stdio MCP client only
  (no marketplace, no HTTP/SSE transport). No telemetry.
- Every public struct, enum, and fn should have a doc comment.
- `cargo doc --no-deps` must build with no warnings.
- `cargo build` must build with no warnings.
- Prefer `anyhow` for error handling in the binary; `thiserror` is available if a crate-level error type is needed.

## Project layout

One binary, `raven`. `cargo install --path .` installs the root package. Implementation is under `crates/`; dependencies point only downward. Root `src/*.rs` files other than `main.rs`, `runner.rs`, and `update.rs` are `pub use` facades (`raven::agent`, `raven::tokenizer`, `raven::context`, `raven::repomap`, `raven::testutil`, and the other historical paths). From the repo root, `cargo test` selects every member (`default-members`).

```
crates/
├── raven-proto/       # ChatMessage, events, errors, plan types, TodoItem, Goal
├── raven-tokenizer/   # estimator, usage meters, pricing
├── raven-config/      # Settings, named providers, onboarding, Grok auth, MCP specs
├── raven-persist/     # session JSONL, session-scoped goal/todos, memory
├── raven-sandbox/     # path confinement, Landlock/seccomp/rlimits, file and shell ops
├── raven-tools/       # definitions, dispatch, git, patch, documents, validate,
│                      # web, skills, repomap, plugins, stdio MCP client
├── raven-agent/       # streaming loop, compaction (context.rs), stream parse,
│                      # tool exec, parallel / delegate_task
├── raven-tui/         # ratatui UI, markdown, slash commands
├── raven-acp/         # ACP v1 stdio adapter
└── raven-testutil/    # outer_sandbox_restrictive, home_env_lock
src/
├── main.rs            # clap CLI
├── lib.rs             # facade module declarations
├── runner.rs          # headless runner and plan approval
└── update.rs          # signed self-update (`raven self update`)
docs/
├── README.md           # index
├── usage.md            # user workflows
├── configuration.md    # env, flags, context, keys
├── architecture.md     # design, agent loop, compaction, sandbox
├── tools.md            # tool contracts and sandbox rules
├── testing.md          # test structure, coverage, mutation testing
├── security.md         # threat model and defense layers
└── contributing.md     # this file
```

---

## Adding a tool

Tools live in [`raven-tools`](../crates/raven-tools/). Sandbox methods live in [`raven-sandbox`](../crates/raven-sandbox/). To add a new tool:

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

In [`definitions.rs`](../crates/raven-tools/src/definitions.rs), add a new entry to the `tool_definitions()` function:

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

In [`dispatch.rs`](../crates/raven-tools/src/dispatch.rs), add a match arm to the `dispatch()` function:

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

Events are defined in [`chat.rs`](../crates/raven-proto/src/chat.rs) as the `AgentEvent` enum.

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
- **TUI** (`raven-tui`): add a match arm in the `while let Ok(ev) = rx.try_recv()` loop.

### 4. Document it

- Add a doc comment to the variant.
- Update [docs/architecture.md](architecture.md) if it affects the loop design.

---

## Adding a slash command

Slash commands live in [`commands.rs`](../crates/raven-tui/src/commands.rs). The registry
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

In [`dispatch.rs`](../crates/raven-tui/src/dispatch.rs), add a match arm in `dispatch_slash_command`.
It receives the parsed command, shared UI state (`log`, `mode`,
`session`, `quit`, ...), and `&SessionStore` — push any user-visible feedback
to `log`.

### 3. Document it

- Add a row to the slash-command table in [docs/usage.md](usage.md).

---

## Changing compaction

Compaction lives in [`context.rs`](../crates/raven-agent/src/context.rs).

- **Threshold**: `compact_threshold` in `Settings` (default `0.75`). Change the default in `raven-config` or via `--compact-threshold` / `RAVEN_COMPACT_THRESHOLD` / `OG_COMPACT_THRESHOLD`.
- **Trailing budget**: hardcoded at 40% of usable context in `compact_if_needed`. Change the `0.40` factor.
- **Summary cap**: `MAX_SUMMARY_CHARS` (4000) and `MAX_TOOL_BODY_CHARS` (200) in `build_summary_user`.
- **Token estimate**: implemented by the pure-Rust token estimator in `raven-tokenizer` (`count_tokens`). Non-newline whitespace is free (BPE glues a leading space to the following word) and a ~12% structural-overhead factor keeps the estimate biased slightly above the real count so compaction triggers early rather than late.

### Invariants to preserve

- The system message (index 0) is **never** dropped.
- Tool-call / tool-result pairs must stay together (see `find_safe_tail_start`).

---

## Running tests

```bash
cargo test --workspace        # offline suite; every member (no live model)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

See [testing.md](testing.md) for coverage and mutation-testing instructions.

When adding tests, prefer testing pure functions (`count_tokens`, `infer_context_window`, `derived_max_tokens`, `glob_segment_match`, `search_replace`, `safe_resolve`, compaction) over the async agent loop.