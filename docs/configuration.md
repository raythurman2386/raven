# Configuration guide

**Raven** reads a layered **TOML config file** in addition to CLI flags and environment variables. Precedence (highest wins): **CLI flag > env var > workspace config `.raven/config.toml` > global config `~/.raven/config.toml` > built-in default**.

See the [root README quick start](../README.md#quick-start) for the full flag list.

---

## Environment variables

| Variable | Default | Meaning |
|---|---|---|
| `RAVEN_PROVIDER` | `ollama` | Active provider name (overridden by `--provider`) |
| `RAVEN_API_KEY` | _(unset)_ | Universal Bearer token override for the active provider |
| `OPENROUTER_API_KEY` | _(unset)_ | Bearer token for the `openrouter` provider |
| `OLLAMA_API_KEY` | _(unset)_ | Bearer token for the `ollama` provider (Ollama Cloud / authenticated hosts) |
| `RAVEN_MAX_ITER` / `OG_MAX_ITER` | `30` | Max agent iterations per run |
| `RAVEN_CONTEXT_WINDOW` / `OG_CONTEXT_WINDOW` | _(inferred)_ | Override the model's context window size (tokens) |
| `RAVEN_COMPACT_THRESHOLD` / `OG_COMPACT_THRESHOLD` | `0.75` | Fraction of usable context at which compaction triggers |
| `RUST_LOG` | _(unset)_ | `tracing` filter (e.g. `debug`, `raven=trace`) |
| `RAVEN_SANDBOX_NETWORK_BLOCK` | _(unset)_ | Set to `0` to skip the seccomp network block (Linux only) |
| `RAVEN_SEARXNG_URL` | _(unset)_ | Optional self-hosted SearXNG base URL for `web_search` (e.g. `http://127.0.0.1:8080`) |
| `RAVEN_SEARXNG_ENGINES` | _(unset)_ | Optional comma-separated SearXNG engine list (e.g. `google,bing`) |

### Examples

```bash
# Use the openrouter provider (endpoint + default model from config)
export RAVEN_PROVIDER=openrouter
raven -p "Explain this repo"

# Point the ollama provider at a remote GPU box
# (via config.toml [providers.ollama] base_url, or RAVEN_API_KEY for auth)
raven -p "Explain this repo"

# Allow more iterations for a big task
export RAVEN_MAX_ITER=50
raven -p "Refactor the whole codebase"

# Debug logging
RUST_LOG=raven=debug raven -p "..."
```

---

## Providers

Raven talks to any OpenAI-compatible `/v1/chat/completions` endpoint through a
**named provider**. A provider bundles the endpoint, auth, and default model so
switching between local (Ollama) and cloud (OpenRouter / Ollama Cloud) is a
single unit — `--provider`, `/provider`, or the `provider` config key.

### Built-in presets

Two providers ship built in (no config needed):

| Name | Base URL | Default model |
|---|---|---|
| `ollama` | `http://localhost:11434/v1` | `gemma4:latest` |
| `openrouter` | `https://openrouter.ai/api/v1` | `deepseek-v4-flash:cloud` |

### Declaring providers

Add a `[providers.<name>]` table to `config.toml` to override a preset or add
your own:

```toml
# ~/.raven/config.toml
provider = "ollama"   # active provider

[providers.ollama]
base_url = "http://gpu-box:11434/v1"
default_model = "qwen2.5-coder:14b"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
default_model = "deepseek-v4-pro:cloud"
# api_key = "sk-..."   # prefer OPENROUTER_API_KEY env var
```

### Selecting the active provider

Precedence (highest wins): **`--provider` flag > `RAVEN_PROVIDER` env > config `provider` key > built-in `ollama`**.

```bash
# CLI flag
raven --provider openrouter -p "..."

# Env var
export RAVEN_PROVIDER=openrouter
raven -p "..."
```

### Switching at runtime

- `/provider <name>` switches the provider for subsequent turns (re-resolves
  the model + context window). `/provider` with no args shows the current one.
- `/model <name>` switches the model **within the current provider**. To use a
  model on a different provider, switch providers first with `/provider`.

### API keys

Keys are resolved per provider: `RAVEN_API_KEY` (universal override) →
provider-scoped var (`OPENROUTER_API_KEY` / `OLLAMA_API_KEY`). Prefer the
provider-scoped env var over a config-file `api_key` so the secret doesn't
land in a committed file.

```bash
# OpenRouter
export OPENROUTER_API_KEY="sk-or-..."
raven --provider openrouter -p "..."

# Ollama Cloud / remote
export OLLAMA_API_KEY="your-key-here"
raven -p "..."
```

### Removed legacy surface

The old flat `--host` / `--api-key` flags and the `RAVEN_HOST` / `OLLAMA_HOST` /
`RAVEN_MODEL` / `OLLAMA_MODEL` env vars are **removed**. Endpoint + auth now
come from `[providers.*]` config and provider-scoped key env vars. `--model`
remains as a per-session override of the active provider's `default_model`.

---

## Config file

Layered TOML config, loaded from the workspace first (higher priority), then the global file. Both are optional; missing keys fall through to env vars / CLI flags / built-in defaults.

| Key | Default | Meaning |
|---|---|---|
| `provider` | `ollama` | Active provider name (overridden by `--provider`) |
| `[providers.<name>]` | _(built-in presets)_ | Named provider definitions: `base_url`, `api_key`, `default_model` |
| `context_window` | inferred from model | Override the model's context window size (tokens) |
| `compact_threshold` | `0.75` | Fraction of usable context at which compaction triggers |
| `max_iterations` | `30` | Max agent iterations per run |
| `mode` | `plan` | Interaction mode: `plan`, `agent`, or `chat` |
| `temperature` | `0.2` | Sampling temperature |
| `no_stream` | `false` | Disable streaming (single request per turn) |
| `verify` | `true` | Enforce verification gate (agent must run tests after edits) |
| `theme` | `ravenwood` | TUI color theme: `ravenwood`, `nord`, `dracula`, `solarized-dark` |
| `searxng_url` | _(unset)_ | Optional self-hosted SearXNG base URL for `web_search` (e.g. `http://127.0.0.1:8080`) |
| `searxng_engines` | _(unset)_ | Optional SearXNG engine list (e.g. `["google", "bing"]`) |

```toml
# .raven/config.toml  (workspace)  or  ~/.raven/config.toml  (global)
provider = "ollama"

[providers.ollama]
base_url = "http://localhost:11434/v1"
default_model = "qwen2.5-coder:14b"

context_window = 131072
compact_threshold = 0.75
max_iterations = 30
mode = "plan"
temperature = 0.2
no_stream = false
verify = true
theme = "ravenwood"
```

CLI flags still win over config file values; env vars take precedence over the config file but lose to explicit CLI flags.

---

## SearXNG

`web_search` uses DuckDuckGo's HTML endpoint by default (keyless, no setup). To route searches through a **self-hosted [SearXNG](https://docs.searxng.org/)** instance — which keeps query traffic on your own network — set its base URL:

```bash
# Point web_search at a local SearXNG instance
export RAVEN_SEARXNG_URL="http://127.0.0.1:8080"
raven -p "What's the latest Rust edition?"

# Optionally pin which engines SearXNG should use (comma-separated)
export RAVEN_SEARXNG_ENGINES="google,bing"
```

Or via the config file:

```toml
searxng_url = "https://searx.example.com"
searxng_engines = ["google", "bing"]
```

Precedence: `RAVEN_SEARXNG_URL` env var > config-file `searxng_url`. `RAVEN_SEARXNG_ENGINES` > config-file `searxng_engines`.

Behavior:

- When configured, `web_search` queries `GET {base}/search?q=…&format=json` and returns up to 10 results (title + URL + short snippet).
- The base URL must be `http://` or `https://` only; `file://`, `data://`, etc. are rejected.
- No API key is required for a typical SearXNG install (`RAVEN_SEARXNG_KEY` is not implemented — SearXNG instances are usually open or IP-restricted).
- If SearXNG is **unreachable**, returns an HTTP error, or returns empty/unparseable results, `web_search` automatically **falls back to DuckDuckGo** so search keeps working. SearXNG is an opt-in enhancement, never a requirement.

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

Leave `OLLAMA_API_KEY` unset and keep the `ollama` provider on `localhost` — no `Authorization` header is sent.

```bash
raven -p "Explain this repo"
```

### Ollama Cloud / remote

```bash
export OLLAMA_API_KEY="your-key-here"
# point the ollama provider at the cloud endpoint in config.toml:
#   [providers.ollama]
#   base_url = "https://ollama.com/v1"
raven -m llama3.1 -p "Explain this repo"
```

### OpenRouter

```bash
export OPENROUTER_API_KEY="sk-or-..."
raven --provider openrouter -p "..."
```

### Security notes

- **Prefer the provider-scoped env var** (`OLLAMA_API_KEY` / `OPENROUTER_API_KEY`) over a config-file `api_key` so the secret does not land in a committed file or shell history.
- The sandboxed `run_shell` tool strips these env vars from child processes:
  - `RAVEN_API_KEY`
  - `OLLAMA_API_KEY`
  - `OPENROUTER_API_KEY`
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