# Testing

## Running tests

All tests run offline. No Ollama server required.

```bash
cargo test
```

## Test structure

- **Unit tests** (`#[cfg(test)] mod tests` in each source file):
  - `src/config.rs` — context window inference, max_tokens derivation, AGENTS.md loading, config.toml parsing
  - `src/agent/` — ephemeral reminder computation (loop-breaker, iteration nudge); **mock-server integration tests** for the full `Agent::run` loop (streaming text, tool-call dispatch, 5xx retry, non-streaming JSON, model-not-found fails-fast) against a fake `/chat/completions` endpoint; **offline fake-model tests** (`src/agent/tests/fake_model.rs`) that drive the loop via a scripted `CompletionSource` with no HTTP (finish, blank-stall recovery + cap, tool round-trip, same-file serial edits, max_tokens clamp)
  - `src/commands.rs` — slash-command parsing, alias resolution, registry uniqueness, help rendering
  - `src/context.rs` — token estimation, compaction (preserves system message, reduces tokens, keeps tool-call/result pairs), tool-result pruning
  - `src/tools/` — sandbox path confinement (including symlink-escape rejection and `openat2`/`open_beneath` traversal rejection), list_dir, read_file, write_file, search_replace, grep, run_shell (dangerous command blocking, API key stripping, direct-exec classification, confined-child behavior incl. Landlock/network-block/RLIMIT_FSIZE), worktree isolation between branches, dispatch routing, glob matching, unified diff parsing, apply_patch, document extraction
  - `src/plan.rs` — plan parsing (JSON, numbered list, bullet list, code block), plan formatting
  - `src/tokenizer.rs` — token counting behavior
  - `src/tui/render.rs` + `src/tui/markdown.rs` — markdown rendering (headings, bold/italic, inline code, fenced code blocks, ordered/unordered lists, blockquotes, links, tables, unclosed-token degradation), scrollback pre-wrapping, tool-call glimmer/fade
- **Integration tests** (`tests/`):
  - `tests/cli_smoke.rs` — black-box tests of the compiled binary (`CARGO_BIN_EXE_raven`): `--help`/`--version` output, no-task error, and session persistence round-trip

## Bugs found by tests

Writing the test suite caught two real bugs in production code:

1. **`glob_segment_match` index bug** — compared `t[pi]` instead of `t[ti]` after star-backtracking, causing `*.rs` to never match `main.rs`
2. **`WalkDir` root filtering** — `filter_entry` skipped the root directory entry because temp dirs start with `.`, causing grep/search_code to find zero files
3. **`apply_patch` error handling** — context mismatch errors were silently written to the file instead of being returned to the caller
4. **`safe_resolve` traversal detection** — `Path::starts_with` on non-canonicalized paths passed traversal checks because `..` components matched the workspace prefix

## Coverage

Install llvm-cov:

```bash
rustup component add llvm-tools-preview
cargo install cargo-llvm-cov
```

Run coverage:

```bash
cargo llvm-cov
cargo llvm-cov --html    # opens HTML report in browser
```

Target: >=80% line coverage on `config.rs` and `tools/`. Overall coverage
as high as practical without testing pure glue (main.rs argument parsing,
tui/ rendering).

## Mutation testing

Install cargo-mutants:

```bash
cargo install cargo-mutants
```

Run mutation tests (focuses on logic-heavy modules):

```bash
cargo mutants --jobs 2
```

To exclude noisy modules (UI glue, main entry):

```bash
cargo mutants --exclude-file src/main.rs --exclude-file src/tui/mod.rs --jobs 2
```

Fix surviving mutants that indicate weak assertions. Don't chase 100% kill
rate on logging-only or display-only code.

## Agent eval suite

Task-level agent strength is measured by the opt-in suite under [`evals/`](../evals/README.md).

| Layer | Command | Model? |
|-------|---------|--------|
| A — offline harness | `cargo test eval_suite` | No (scripted fake model) |
| B — live fixtures | `python3 evals/run.py --smoke` or full | Yes |
| C — arena | full suite × multiple models | Yes |

Live runs copy each `evals/cases/<id>/repo` to a temp workspace, invoke headless
`raven`, then grade with deterministic `checks.sh` (not LLM-as-judge). Reports
go to `evals/out/`; update `evals/baselines/default.md` only after a deliberate
pinned run.

CI policy: `cargo test` always (includes Layer A). Live smoke/full is manual or
nightly when an endpoint is available.

## What is NOT tested

- **Live Ollama API calls in `cargo test`** — real model responses require a
  running endpoint and are **not** part of the default suite. Use
  `python3 evals/run.py` for live task evals. The HTTP/streaming loop *is*
  covered by mock-server integration tests (see above).
- **TUI rendering** — the markdown renderer and scrollback pre-wrapping logic
  *are* unit-tested (see `src/tui/render.rs` + `src/tui/markdown.rs`), but the
  ratatui/crossterm event loop and interactive layout are not; those are
  exercised manually.
- **Network retries against a real host** — the retry-with-backoff logic is
  covered against the mock (a scripted 503), and the connection-refused path by
  pointing at an unreachable host.