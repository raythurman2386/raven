# Raven agent eval suite

Measures **task completion** and secondary efficiency metrics for the coding
agent harness. Offline unit tests stay in `cargo test`; this suite is opt-in
and may call a live OpenAI-compatible endpoint.

## Layers

| Layer | How | Needs model? |
|-------|-----|--------------|
| A — offline harness | `cargo test eval_suite` (scripted fake model) | No |
| B — live fixtures | `python3 evals/run.py` (headless `raven --yolo`) | Yes |
| C — arena / nightly | full tag set, multiple models | Yes |

## Quick start (live)

```bash
# Build the binary first
cargo build --release

# Smoke subset (fast, deterministic checks)
python3 evals/run.py --smoke

# Full suite against defaults (RAVEN_MODEL / Ollama)
python3 evals/run.py

# Pin model + host (local Ollama)
python3 evals/run.py --model deepseek-v4-flash:cloud --host http://127.0.0.1:11434/v1

# Cloud / OpenRouter: put RAVEN_API_KEY in repo-root .env (auto-loaded) or export it.
# Use https:// — plain http:// fails immediately with 0 tool calls.
python3 evals/run.py --model x-ai/grok-4.5 --host https://openrouter.ai/api/v1

# One case
python3 evals/run.py --case 02_single_edit
```

The harness writes the `--host`/`--api-key` into a `[providers.eval]` table in
each case's workspace config and invokes raven with `--provider eval` (the old
`--host`/`--api-key` CLI flags were removed in favor of named providers).

If every case fails in **<1s with `tools=0`**, the model never ran tools —
almost always missing `RAVEN_API_KEY` or wrong host scheme/URL, not a model
quality issue.

Reports land in `evals/out/<run-id>.json` and `evals/out/<run-id>.md`.
Checked-in baseline: `evals/baselines/default.md` (update deliberately).

## Case layout

```
evals/cases/<id>/
  meta.toml     # id, tags, timeout_secs, mode, yolo, flaky, requires_git
  task.md       # user prompt
  checks.sh     # executable grader; exit 0 = pass (cwd = run workspace)
  repo/         # seed project (copied to a temp dir per run)
```

### `meta.toml` keys

| Key | Default | Meaning |
|-----|---------|---------|
| `id` | dir name | stable case id |
| `tags` | `[]` | e.g. `smoke`, `edit`, `git`, `sandbox` |
| `timeout_secs` | `300` | wall clock for raven + checks |
| `mode` | `"agent"` | CLI `--mode` |
| `yolo` | `true` | pass `--yolo` (skip plan / confirmations) |
| `flaky` | `false` | report but do not fail the run hard |
| `requires_git` | `false` | `git init` the temp workspace before run |
| `expect_raven_fail` | `false` | raven non-zero exit is OK (optional) |
| `skip_live` | `false` | offline-only case |
| `stdin_approve` | `false` | pipe `y\n` for headless plan approval |

### Grading

`checks.sh` runs with:

- `cwd` = temp workspace (copy of `repo/`)
- env: `EVAL_CASE`, `EVAL_STDOUT`, `EVAL_STDERR`, `EVAL_EXIT`, `EVAL_REPO`
- should be deterministic (file content, `git status`, tests) — not LLM-as-judge

## Metrics captured

- `pass` / `fail` / `skip` / `timeout`
- wall time, raven exit code
- tool-call count and iteration markers (parsed from headless stdout)
- whether working tree is dirty after the run
- whether `checks.sh` passed

## CI policy

- `cargo test` — always (includes Layer A)
- `python3 evals/run.py --smoke` — optional when a model endpoint is available
- full suite — manual / nightly

Do not grow vanity cases. Add a case when a real failure mode appears.
