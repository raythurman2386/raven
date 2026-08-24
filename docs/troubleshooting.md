# Troubleshooting

Operational notes for the common failure modes. If your issue isn't here, open
an issue with the `RUST_LOG=raven=debug` output from a repro.

---

## Windows: `raven` isn't found / "open with?" prompt

The Windows installer (`install.ps1`) installs **`raven.exe`** to
`%USERPROFILE%\.cargo\bin` and adds that directory to your **user** PATH. A few
things to check:

- **New terminal required.** PATH changes from the installer only apply to
  terminals opened *after* the install. Reopen your shell (or run
  `refreshenv` in a new PowerShell) before trying `raven`.
- **Bare `raven` on PATH.** If you type `raven` and Windows shows an "open
  with?" dialog instead of running it, the extensionless `raven` (left by an
  older install) is shadowing `raven.exe`. The installer removes the old
  extensionless binary, but if you installed manually, delete it:
  ```powershell
  Remove-Item "$env:USERPROFILE\.cargo\bin\raven" -Force
  ```
  Then run `raven` again — it should resolve to `raven.exe`.
- **PATH not updated.** Verify the directory is on your user PATH:
  ```powershell
  [Environment]::GetEnvironmentVariable("PATH", "User")
  ```
  If `.cargo\bin` is missing, re-run `install.ps1` (it adds it) or add it
  manually.

---

## Stream decode / "stream interrupted" errors

Raven streams SSE from the model endpoint. A mid-stream failure (connection
reset, provider hiccup, proxy timeout) surfaces as:

```
[stream interrupted — retry or use --no-stream]
```

- **Partial text is preserved.** Raven keeps whatever the model produced before
  the stream broke and appends the interruption hint, so you don't lose the
  turn.
- **Retry the same prompt** — transient stream failures usually succeed on a
  second attempt.
- **Use `--no-stream`** for endpoints/proxies that don't reliably support SSE:
  ```bash
  raven --no-stream -p "Your task"
  ```
  This makes a single non-streaming request per turn instead of a stream.
- **UTF-8 decode.** Raven decodes SSE only at line boundaries, so a multi-byte
  character split across TCP chunks is never lossy-decoded. If you see garbled
  text, it's the endpoint emitting malformed UTF-8, not a Raven bug.

---

## Sandbox denies a command / "command blocked by sandbox filter"

Raven confines subprocesses with several layers. A denial usually shows one of:

- **`Error: command blocked by sandbox filter`** — the command matched the
  destructive-command denylist (`rm -rf /`, `mkfs`, `curl | sh`, fork bombs,
  etc.). This is a hard block; rephrase the command.
- **`Error: command killed by signal`** — the child was killed by the seccomp
  network block (SIGSYS) or a resource limit. Sanctioned test runners
  (`cargo test`, `npm test`, `vitest`, `pytest`, …) are exempted from the
  network block; an arbitrary command that opens an internet socket is not.
- **`Error: path escapes workspace`** — a file tool tried to read/write outside
  the workspace (Landlock / `openat2`). Use paths relative to the workspace.

**Escape hatches** (all documented in `docs/security.md`):
- `RAVEN_SANDBOX_LANDLOCK=0` — skip Landlock filesystem confinement.
- `RAVEN_SANDBOX_NETWORK_BLOCK=0` — skip the seccomp network block (e.g. a
  legitimate tool needs network access).

These are for recovery/testing — they weaken the sandbox. Prefer rephrasing the
command.

---

## `web_search` returns DuckDuckGo results when SearXNG is configured

This is **by design**, not a bug. When `RAVEN_SEARXNG_URL` (or the
`searxng_url` config key) is set, `web_search` queries your SearXNG instance
first and **falls back to DuckDuckGo** on any failure — HTTP error, empty
results, or unparseable JSON — so search keeps working when the local instance
is down.

To confirm SearXNG is actually being used:
1. Check the base URL is reachable and returns JSON:
   ```bash
   curl "http://127.0.0.1:8080/search?q=test&format=json"
   ```
2. Verify the URL is `http://`/`https://` only (other schemes are rejected).
3. If SearXNG returns empty results for a query, DDG may have hits SearXNG's
   engine set didn't — that's the intended fallback.

SearXNG is an opt-in enhancement, never a requirement. Unset `RAVEN_SEARXNG_URL`
to use DuckDuckGo directly.

---

## ACP: editor won't connect / "raven --acp" does nothing

`raven --acp` speaks [Agent Client Protocol](https://agentclientprotocol.com/) v1
on **stdin/stdout** — it is not a server you point a browser at. Point an
ACP-capable editor at the `raven` binary with the `acp` flag, e.g. Zed's custom
agent command:

```
raven --acp
```

- **Supported methods:** `initialize`, `authenticate`, `session/new`,
  `session/prompt`, `session/cancel`, `session/load`, `session/resume`,
  `session/list`, `session/close`, `session/set_mode` (`plan`/`agent`/`chat`),
  `session/set_config_option` (`mode` / `model`), `session/set_model`.
  `ask_user` and shell confirmation become
  `session/request_permission`. `initialize` advertises one `agent`-type auth
  method (`agent-auth`); `authenticate` acknowledges it (credentials are
  resolved in-process).
- **Not advertised:** MCP servers, images/audio, client `fs/*`/`terminal/*`.
  Raven keeps its own sandbox.
- **Other CLI flags still apply** to the ACP process: `--provider`, `--model`,
  `--workspace`, `--yolo`, `--mode`.

If the editor shows nothing, run `raven --acp` in a terminal and type a JSON-RPC
`initialize` frame to confirm it responds on stdin/stdout.

---

## First-run / provider errors

- **`<provider> unreachable at <url>`** — the endpoint isn't reachable. For
  local Ollama, start it with `ollama serve` (or `ollama pull <model>` first).
- **`Model '<model>' not found on ollama. Pull it with: ollama pull <model>`**
  — the model isn't installed locally. Run the suggested `ollama pull`.
- **`HTTP 401/403 from <provider>`** — bad or missing API key. Set
  `RAVEN_API_KEY` (universal) or the provider-scoped var
  (`OPENROUTER_API_KEY` / `OLLAMA_API_KEY`), or `api_key`/`api_key_env` in
  `[providers.<name>]` in `config.toml`. See `docs/configuration.md`.
