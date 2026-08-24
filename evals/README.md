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

## Current cases

Case directory names are **stable IDs** (not a dense sequence). Retired
ids are not reused:

| id | what it grades |
|----|----------------|
| `01_readonly_symbol` | read-only Q&A, no writes |
| `02_single_edit` | one targeted edit |
| `03_multi_file_refactor` | multi-file change |
| `04_fix_failing_test` | make existing tests pass |
| `06_sandbox_escape` | path confinement (harness bug if this fails) |
| `07_memory_recall` | MEMORY.md injection |
| `08_skill_use` | skill_search / skill_load |
| `09_plan_then_execute` | plan mode then execute |
| `10_add_test` | add a unit test |
| `12_verify_before_done` | enforced verify gate after edits |
| `13_long_horizon` | multi-step task |
| `14_large_tool_output` | paging past the default `read_file` window |
| `15_windows_fs_edge` | Windows-only filesystem edges (`requires_os = windows`) |

Retired (removed with the `git_commit` tool / autocommit path):
`05_git_commit_clean`, `11_secrets_stay_uncommitted`.

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
| `requires_os` | `""` | skip unless the host matches (`windows` / `linux` / `macos`) |

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

- `cargo test` — always (includes Layer A, including sandbox `/tmp` escape
  regressions)
- `python3 evals/run.py --smoke` — optional when a model endpoint is available
- full suite — manual / nightly

`06_sandbox_escape` and `12_verify_before_done` are **not** flaky cases. A
fail is a harness bug (sandbox grant or a dead verify gate) or a model that
never ran tests. Do not mark them `flaky = true`.

`14_large_tool_output` grades whether the model paged past the default
400-line `read_file` window. `15_windows_fs_edge` is skipped unless
`requires_os = windows` matches the host.

Do not grow vanity cases. Add a case when a real failure mode appears.

---

## Recommended models for evaluation

**High-quality models** that consistently pass the full eval suite with Raven:

| Model | Provider | Type | Status | Notes |
|-------|----------|------|--------|-------|
| `qwen3.8` | Ollama Cloud | General/Reasoning | ✅ Recommended | Latest Qwen; strong on agentic long-horizon tasks |
| `deepseek-v4-flash:cloud` | Ollama Cloud | Reasoning | ✅ Passing | Efficient alternative to pro; good token usage |
| `deepseek-v4-pro:cloud` | Ollama Cloud | Reasoning/Coding | ✅ Recommended | High quality, fast; strong on complex reasoning |
| `grok-4.5` | OpenRouter | Multimodal/Reasoning | ✅ Recommended | Frontier performance; best reasoning and cost for complex evals |
| `grok-4.6` | OpenRouter | Multimodal/Reasoning | ✅ Recommended | Frontier performance; excellent reasoning for complex evals |
| `nemotron-3-ultra:cloud` | Ollama Cloud | Agentic | ✅ Passing | Built for long-running agent workflows |
| `glm-5.2:cloud` | Ollama Cloud | Long-horizon | ✅ Passing | Optimized for long-horizon tasks; some flakiness |
| `kimi-k3:cloud` | Ollama Cloud | Agentic/Multimodal | ⚠️ Partial | Passes most cases; may need auth/usage credits |
| `kimi-k2.7-code:cloud` | Ollama Cloud | Coding/Agentic | ⚠️ Partial | Coding-focused; long-horizon improvements over k2.6 |
| `gemma4:cloud` | Ollama Cloud | General | ❌ Flaky | Fails long-horizon eval (case 13) regularly; inconsistent |
| `minimax-m3:cloud` | Ollama Cloud | Agentic | ❌ Flaky | Fails multiple evals; poor tool-use consistency |
| `minimax-m2.7:cloud` | Ollama Cloud | Coding | ❌ Flaky | Predecessor to m3; similar reliability issues |

**Legend:**
- **✅ Recommended** — Passes full eval suite; production-ready for daily use
- **✅ Passing** — Passes full eval suite; good for specific use cases or features
- **⚠️ Partial** — Passes most evals; may fail edge cases or require special setup (auth, credits, quota)
- **❌ Flaky** — Fails multiple evals; unreliable for agent workflows; not recommended

### Running evals against multiple models

Use the batch evaluation script:

```bash
# List all available cloud models
python3 evals/run_all_models.py --list-only

# Run evals against all official cloud models (requires network access)
python3 evals/run_all_models.py

# Test specific models
python3 evals/run_all_models.py --models qwen3.8,deepseek-v4-pro:cloud,grok-4.5

# Local Ollama only
python3 evals/run_all_models.py --host http://127.0.0.1:11434/v1
```

Reports are saved to `evals/out/<timestamp>.{json,md}` for analysis.

### Model selection guidance

**For daily coding work:**
- **First choice:** `qwen3.8` (all-around excellence for local; Ollama Cloud recommended)
- **Alternative:** `deepseek-v4-flash:cloud` (for cheap high performance in the cloud)
- **Fallback:** `glm-5.2` (if qwen and deepseek are unavailable; fast, reliable, high reasoning)

**For frontier/complex reasoning:**
- `grok-4.5` (via OpenRouter; multimodal, excellent model)
- `grok-4.6` (via OpenRouter; multimodal, best reasoning)
- `deepseek-v4-pro:cloud` (strong reasoning within cloud models)
