# Testing

## Running tests

All tests run offline. No Ollama server required.

```bash
cargo test
```

## Test structure

- **Unit tests** (`#[cfg(test)] mod tests` in each source file):
  - `src/config.rs` — context window inference, max_tokens derivation, AGENTS.md loading, config.toml parsing
  - `src/agent.rs` — ephemeral reminder computation (loop-breaker, iteration nudge); **mock-server integration tests** for the full `Agent::run` loop (streaming text, tool-call dispatch, 5xx retry, non-streaming JSON, model-not-found fails-fast) against a fake `/chat/completions` endpoint
  - `src/commands.rs` — slash-command parsing, alias resolution, registry uniqueness, help rendering
  - `src/context.rs` — token estimation, compaction (preserves system message, reduces tokens, keeps tool-call/result pairs), tool-result pruning
  - `src/tools/` — sandbox path confinement (including symlink-escape rejection), list_dir, read_file, write_file, search_replace, grep, run_shell (dangerous command blocking, API key stripping), dispatch routing, glob matching, unified diff parsing, apply_patch, document extraction
  - `src/plan.rs` — plan parsing (JSON, numbered list, bullet list, code block), plan formatting
  - `src/tokenizer.rs` — token counting behavior
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

Target: >=80% line coverage on `config.rs` and `tools.rs`. Overall coverage
as high as practical without testing pure glue (main.rs argument parsing,
tui.rs rendering).

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
cargo mutants --exclude-file src/main.rs --exclude-file src/tui.rs --jobs 2
```

Fix surviving mutants that indicate weak assertions. Don't chase 100% kill
rate on logging-only or display-only code.

## What is NOT tested

- **Live Ollama API calls** — real model responses require a running Ollama
  server and are not exercised in CI. The HTTP/streaming loop *is* covered by
  mock-server integration tests (see above); live behavior is tested manually
  via `--headless` mode.
- **TUI rendering** — ratatui/crossterm integration is not unit-tested. The TUI
  is exercised manually.
- **Network retries against a real host** — the retry-with-backoff logic is
  covered against the mock (a scripted 503), and the connection-refused path by
  pointing at an unreachable host.