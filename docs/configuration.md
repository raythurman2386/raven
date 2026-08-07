# Configuration guide

**Raven** reads a layered **TOML config file** in addition to CLI flags and environment variables. Precedence (highest wins): **CLI flag > env var > workspace config `.raven/config.toml` > global config `~/.raven/config.toml` > built-in default**.

See the [root README quick start](../README.md#quick-start) for the full flag list.

---

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `RAVEN_MODEL` / `OLLAMA_MODEL` | `gemma4:latest` | Default model (overridden by `-m`) |
| `RAVEN_HOST` / `OLLAMA_HOST` | `http://localhost:11434/v1` | OpenAI-compatible base URL (overridden by `--host`) |
| `RAVEN_API_KEY` / `OLLAMA_API_KEY` | _(unset)_ | Bearer token for Ollama Cloud or any authenticated host |
| `RAVEN_MAX_ITER` / `OG_MAX_ITER` | `30` | Max agent iterations per run |
| `RAVEN_CONTEXT_WINDOW` / `OG_CONTEXT_WINDOW` | _(inferred)_ | Override the model's context window size (tokens) |
| `RAVEN_COMPACT_THRESHOLD` / `OG_COMPACT_THRESHOLD` | `0.75` | Fraction of usable context at which compaction triggers |
| `RUST_LOG` | _(unset)_ | `tracing` filter (e.g. `debug`, `raven=trace`) |

### Examples

```bash
# Use a bigger model
export RAVEN_MODEL=qwen2.5-coder:14b
raven -p "Explain this repo"

# Point at a remote Ollama
export RAVEN_HOST=http://gpu-box:11434/v1
raven -p "Explain this repo"

# Allow more iterations for a big task
export RAVEN_MAX_ITER=50
raven -p "Refactor the whole codebase"

# Debug logging
RUST_LOG=raven=debug raven -p "..."
```

---

## Config file

Layered TOML config, loaded from the workspace first (higher priority), then the global file. Both are optional; missing keys fall through to env vars / CLI flags / built-in defaults.

| Key | Default | Meaning |
|---|---|---|
| `model` | `gemma4:latest` | Default model (overridden by `-m`) |
| `host` | `http://localhost:11434/v1` | OpenAI-compatible base URL (overridden by `--host`) |
| `context_window` | inferred from model | Override the model's context window size (tokens) |
| `compact_threshold` | `0.75` | Fraction of usable context at which compaction triggers |
| `max_iterations` | `30` | Max agent iterations per run |
| `plan_first` | `true` | Propose a plan before executing |
| `temperature` | `0.2` | Sampling temperature |
| `no_stream` | `false` | Disable streaming (single request per turn) |
| `verify` | `true` | Enforce verification gate (agent must run tests after edits) |

```toml
# .raven/config.toml  (workspace)  or  ~/.raven/config.toml  (global)
model = "qwen2.5-coder:14b"
host = "http://localhost:11434/v1"
context_window = 131072
compact_threshold = 0.75
max_iterations = 30
plan_first = true
temperature = 0.2
no_stream = false
verify = true
```

CLI flags still win over config file values; env vars take precedence over the config file but lose to explicit CLI flags.

---

## Context window

The context window is fetched from the model's actual metadata via Ollama's `/api/show` endpoint when `--context-window` / `RAVEN_CONTEXT_WINDOW` / `OG_CONTEXT_WINDOW` are unset. This returns the real `context_length` from the model file. If the API is unreachable, a name-based heuristic is used as fallback:

| Model name contains | Inferred window |
|---|---|
| `glm` + `cloud` | 1 000 000 |
| `gemma4`, `gemma3`, `qwen2.5`, `qwen3`, `llama3.1`, `llama3.2`, `deepseek`, `codestral`, `glm` | 128 000 |
| `llama3`, `codellama`, `32k` | 32 768 |
| `mistral`, `8k` | 8 192 |
| _(anything else)_ | 32 768 |

Override it if the inference is wrong:

```bash
raven --context-window 65536 -p "..."
RAVEN_CONTEXT_WINDOW=65536 raven -p "..."
OG_CONTEXT_WINDOW=65536 raven -p "..."
```

The `max_tokens` output budget is derived as `context_window / 8`, clamped to `[1024, 32768]`. Per iteration, it is further clamped so `prompt_tokens + max_tokens + 64 ≤ context_window`.

---

## Compaction threshold

Compaction triggers when estimated history tokens exceed `compact_threshold × (context_window − output_reserve)`.

- Default: `0.75` (compact when 75% full)
- Lower it to compact earlier: `--compact-threshold 0.5`
- Raise it to compact later (riskier): `--compact-threshold 0.9`

See [architecture.md#compaction](architecture.md#compaction) for the algorithm.

---

## API keys

### Local (default)

Leave `RAVEN_API_KEY` / `OLLAMA_API_KEY` unset and keep the host on `localhost` — no `Authorization` header is sent.

```bash
raven -p "Explain this repo"
```

### Ollama Cloud / remote

```bash
export RAVEN_API_KEY="your-key-here"
export RAVEN_HOST="https://ollama.com/v1"
raven -m llama3.1 -p "Explain this repo"
```

Or via flags (less secure — lands in shell history):

```bash
raven --api-key "your-key" --host https://ollama.com/v1 -m llama3.1 -p "..."
```

### Security notes

- **Prefer the env var over `--api-key`** so the secret does not land in shell history.
- The sandboxed `run_shell` tool strips these env vars from child processes:
  - `RAVEN_API_KEY`
  - `OLLAMA_API_KEY`
  - `OPENAI_API_KEY`
  - `XAI_API_KEY`
  - `ANTHROPIC_API_KEY`
  - `AWS_SECRET_ACCESS_KEY`
- **Raven** uses `rustls` with webpki roots for TLS.

---

## AGENTS.md

Project instructions are auto-loaded from the first matching file in the workspace root (checked in order):

1. `AGENTS.md`
2. `CLAUDE.md`
3. `.grok/AGENTS.md`
4. `AGENT.md`

Contents (up to 8000 chars) are appended to the system prompt under a `--- Project instructions (AGENTS.md) ---` header.

### Example AGENTS.md

```markdown
# Project rules

- Use tabs, not spaces.
- Run `cargo fmt` before considering a task done.
- All public functions must have doc comments.
- Prefer `anyhow` for error handling.
```

### Session overrides

Use `--rules` for session-specific rules that should not be committed:

```bash
raven --rules "Focus only on the auth module. Ignore everything else." -p "Review security"
```

`--rules` content is appended after any `AGENTS.md` content, under a `--- Session rules ---` header.