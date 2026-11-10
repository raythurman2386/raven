# Connecting Zed / Hearth to Huginn via ACP

Huginn speaks **Agent Client Protocol (ACP) v1** over stdio, so you can run it
directly inside [Zed](https://zed.dev) or
[Hearth](https://github.com/raythurman2386/hearth) as an *external agent*. The
editor hosts the thread; Huginn owns its own runtime, model selection, provider
auth, and tools. All the usual Huginn features work from the editor — plan
mode, verification gates, workspace isolation, `.huginn/MEMORY.md`, skills, and
the full tool set. The examples below use Zed's config surface; Hearth
discovers ACP agents on its own — point it at the same `huginn --acp` command.

> **ACP registry install (when available).** Huginn is not yet in the official
> [ACP registry](https://agentclientprotocol.com/registry) — the config below
> wires it up as a *custom* agent, which works today. Once Huginn is merged into
> the registry you can install it directly from Zed's agent list instead:
> open **Agent Settings** (`agent: open settings`) → **External Agents** →
> **Add Agent** → **Install from Registry**, then pick **Huginn** from the
> new-thread menu. The custom `agent_servers` entry then becomes optional.

## Prerequisites

- **Zed 1.14+** (external agents require a recent Zed)
- **Huginn** installed and on your `PATH` (see the root [README](../README.md#install));
  verify with `huginn --version` → should print `huginn 0.5.17` or newer.
- A reachable model endpoint. The default provider is `grok` (`grok-4.7`).
  On a new machine run `huginn login` once before the first Zed thread.
  Zed's ACP process is not a terminal, so the first-run wizard does not run
  there. Ollama and OpenRouter still work if you pin `--provider`.

## 1. Add Huginn as a custom agent

Zed registers external agents in `~/.config/zed/settings.json` under the
`agent_servers` key. Add a `Huginn` entry:

```jsonc
{
  "agent_servers": {
    "Huginn": {
      "type": "custom",
      "command": "huginn",
      "args": ["--acp"],
      "env": {}
    }
  }
}
```

Notes:

- The **key** (`"Huginn"`) is the display name in Zed's thread menu.
- `--acp` is required — it puts Huginn in ACP stdio mode.
- `--provider` and `--model` are **optional** but recommended to pin what Huginn
  uses. If omitted, Huginn falls back to the active provider and default model in
  `~/.huginn/config.toml`.
- `env` lets you pass provider credentials to the Huginn process, e.g.
  `"OLLAMA_API_KEY": "..."`. Otherwise Huginn reads them from its own
  config / `.env` / shell environment.
- Zed does **not** need a restart — it picks up the `agent_servers` change
  automatically.

> **Alternative:** run `agent: open settings` in Zed, go to the External Agents
> page, click **Add Agent → Add Custom Agent**. Zed inserts the skeleton for
> you; just fill in `command`/`args` as above.

## 2. Verify the connection

The quickest end-to-end check is to replay the ACP handshake Huginn sends on
startup. Run the same `initialize` frame Zed will:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true,"listDirectory":true,"editTextFile":true,"getFileInfo":true},"terminal":{"startTerminal":true}},"clientInfo":{"name":"verify","version":"0.0.0"}}}' \
  | timeout 15 huginn --acp
```

You should get back a JSON-RPC `result` advertising `agentInfo.name: "huginn"`,
`agentCapabilities.loadSession: true`, `sessionCapabilities`, and an
`agent`-type `authMethods` entry. That confirms Zed will connect cleanly.

For live Zed↔Huginn traffic, open the Command Palette and run **`dev: open acp
logs`**. Include those logs when reporting issues.

## 3. Use Huginn from the editor

1. Open the **Agent Panel** (`cmd-1` / `ctrl-1`) or **Threads Sidebar**.
2. Open the new-thread / agent selector menu and choose **Huginn**.
3. Start a thread and type your task. Everything runs through Huginn's loop —
   plan mode, tools, verify-before-done — just as it would in the terminal.

### Handy Zed actions

| Action | Purpose |
|---|---|
| `agent: new external agent thread` | Bind a key to start a Huginn thread directly |
| `dev: open acp logs` | Inspect ACP frames between Zed and Huginn |
| `agent: open settings` | View / edit the External Agents config |

## Configuration boundaries

Because Huginn runs as its own process, Zed and Huginn config stay separate:

| Concern | Owned by |
|---|---|
| Model & provider selection | Huginn (`~/.huginn/config.toml`, `--provider`/`--model`) |
| Auth / API keys | Huginn (provider env vars, `.env`, or `env` in `agent_servers`) |
| Tools | Huginn (its 25 built-in tools) |
| Skills / instructions | Huginn (native `SKILL.md` discovery, `AGENTS.md` auto-load) |
| Zed Skills | Do **not** apply — Huginn does not read Zed skills |
| MCP servers | Native `[mcp.servers]` in Huginn's `config.toml`, plus any servers Zed forwards on `session/new` (stdio only) |

## Provider selection per connection

If you keep several Huginn profiles, add one `agent_servers` entry per profile
and give each a distinct key:

```jsonc
{
  "agent_servers": {
    "Huginn (Ollama)": {
      "type": "custom",
      "command": "huginn",
      "args": ["--acp", "--provider", "ollama", "--model", "deepseek-v4-flash:cloud"]
    },
    "Huginn (OpenRouter)": {
      "type": "custom",
      "command": "huginn",
      "args": ["--acp", "--provider", "openrouter", "--model", "x-ai/grok-4.5"]
    }
  }
}
```

## Switching providers / models in the editor

Huginn advertises session config options over ACP:

- **`mode`** (`plan` / `agent` / `chat`) — editors that support config options
  use this picker instead of the older `modes` field. Without it, a client
  that prefers `configOptions` would hide the mode selector and leave you
  stuck in the default (agent).
- **`model`** — every configured provider's models as provider-qualified ids
  (`provider/model`), so you can switch providers and models without
  restarting the thread.

If the editor exposes ACP session config options (e.g. selectors in the
Agent Panel), picking a model entry switches that Huginn session onto the
chosen provider + model:

- Each provider's list comes from its live `/models` endpoint when reachable,
  else its curated fallback list.
- Selections are `provider/model` ids (e.g. `ollama/deepseek-v4-flash:cloud`,
  `opencode-go/glm-5.3-flash`). A value with a known provider prefix switches
  providers; a plain model name stays on the current provider.
- Both the modern `session/set_config_option` and the legacy `session/set_model`
  are supported, so older editors still work.
- Per-provider lists are capped (200 models) so a huge OpenRouter catalog
  can't flood the dropdown.

> **Tip:** you can still pin the startup provider/model with `--provider` /
> `--model` in the `agent_servers` entry; the in-editor selector starts from
> that and can switch away from it.


## Troubleshooting

- **Huginn doesn't appear in the thread menu.** Confirm the settings file is
  valid JSON, `huginn` is on `PATH` (restart Zed if you edited the env), and you
  saved the `agent_servers` block. Re-open `agent: open settings` to confirm the
  entry is registered.
- **Thread fails to start.** Run the handshake command above. If it errors,
  check Huginn's config (`~/.huginn/config.toml`) and provider auth.
- **Credentials not picked up.** Zed doesn't automatically share its LLM keys
  with Huginn. Set the key via `env` in the `agent_servers` entry, an
  `OLLAMA_API_KEY`/`OPENROUTER_API_KEY` env var available to the process, or
  Huginn's own `.env`.
- **MCP tools missing.** Confirm the server is stdio (not HTTP/SSE), the
  command is on `PATH` for the Huginn process, and either `[mcp.servers]` is
  set in `~/.huginn/config.toml` or Zed is forwarding `mcpServers` on
  `session/new`. A failed spawn is skipped; check stderr / `RUST_LOG=huginn=debug`.

For general ACP failure modes, see [troubleshooting.md](troubleshooting.md).
