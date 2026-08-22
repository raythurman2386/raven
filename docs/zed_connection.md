# Connecting Zed to Raven via ACP

Raven speaks **Agent Client Protocol (ACP) v1** over stdio, so you can run it
directly inside [Zed](https://zed.dev) as an *external agent*. Zed hosts the
thread in the Agent Panel and Threads Sidebar; Raven owns its own runtime,
model selection, provider auth, and tools. All the usual Raven features work
from the editor — plan mode, verification gates, workspace isolation,
`.raven/MEMORY.md`, skills, and the full tool set.

> **ACP registry install (when available).** Raven is not yet in the official
> [ACP registry](https://agentclientprotocol.com/registry) — the config below
> wires it up as a *custom* agent, which works today. Once Raven is merged into
> the registry you can install it directly from Zed's agent list instead:
> open **Agent Settings** (`agent: open settings`) → **External Agents** →
> **Add Agent** → **Install from Registry**, then pick **Raven** from the
> new-thread menu. The custom `agent_servers` entry then becomes optional.

## Prerequisites

- **Zed 1.14+** (external agents require a recent Zed)
- **Raven** installed and on your `PATH` (see the root [README](../README.md#install));
  verify with `raven --version` → should print `raven 0.4.1` or newer.
- A reachable model endpoint for Raven (local Ollama, Ollama Cloud, or
  OpenRouter — whatever your `~/.raven/config.toml` already uses).

## 1. Add Raven as a custom agent

Zed registers external agents in `~/.config/zed/settings.json` under the
`agent_servers` key. Add a `Raven` entry:

```jsonc
{
  "agent_servers": {
    "Raven": {
      "type": "custom",
      "command": "raven",
      "args": ["--acp", "--provider", "ollama", "--model", "deepseek-v4-flash:cloud"],
      "env": {}
    }
  }
}
```

Notes:

- The **key** (`"Raven"`) is the display name in Zed's thread menu.
- `--acp` is required — it puts Raven in ACP stdio mode.
- `--provider` and `--model` are **optional** but recommended to pin what Raven
  uses. If omitted, Raven falls back to the active provider and default model in
  `~/.raven/config.toml`.
- `env` lets you pass provider credentials to the Raven process, e.g.
  `"OLLAMA_API_KEY": "..."`. Otherwise Raven reads them from its own
  config / `.env` / shell environment.
- Zed does **not** need a restart — it picks up the `agent_servers` change
  automatically.

> **Alternative:** run `agent: open settings` in Zed, go to the External Agents
> page, click **Add Agent → Add Custom Agent**. Zed inserts the skeleton for
> you; just fill in `command`/`args` as above.

## 2. Verify the connection

The quickest end-to-end check is to replay the ACP handshake Raven sends on
startup. Run the same `initialize` frame Zed will:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":true,"writeTextFile":true,"listDirectory":true,"editTextFile":true,"getFileInfo":true},"terminal":{"startTerminal":true}},"clientInfo":{"name":"verify","version":"0.0.0"}}}' \
  | timeout 15 raven --acp
```

You should get back a JSON-RPC `result` advertising `agentInfo.name: "raven"`,
`agentCapabilities.loadSession: true`, `sessionCapabilities`, and an
`agent`-type `authMethods` entry. That confirms Zed will connect cleanly.

For live Zed↔Raven traffic, open the Command Palette and run **`dev: open acp
logs`**. Include those logs when reporting issues.

## 3. Use Raven from the editor

1. Open the **Agent Panel** (`cmd-1` / `ctrl-1`) or **Threads Sidebar**.
2. Open the new-thread / agent selector menu and choose **Raven**.
3. Start a thread and type your task. Everything runs through Raven's loop —
   plan mode, tools, verify-before-commit — just as it would in the terminal.

### Handy Zed actions

| Action | Purpose |
|---|---|
| `agent: new external agent thread` | Bind a key to start a Raven thread directly |
| `dev: open acp logs` | Inspect ACP frames between Zed and Raven |
| `agent: open settings` | View / edit the External Agents config |

## Configuration boundaries

Because Raven runs as its own process, Zed and Raven config stay separate:

| Concern | Owned by |
|---|---|
| Model & provider selection | Raven (`~/.raven/config.toml`, `--provider`/`--model`) |
| Auth / API keys | Raven (provider env vars, `.env`, or `env` in `agent_servers`) |
| Tools | Raven (its 25 built-in tools) |
| Skills / instructions | Raven (native `SKILL.md` discovery, `AGENTS.md` auto-load) |
| Zed Skills | Do **not** apply — Raven does not read Zed skills |
| MCP servers | Zed-configured MCP servers may be forwarded to Raven over ACP; Raven also reads its own native config |

## Provider selection per connection

If you keep several Raven profiles, add one `agent_servers` entry per profile
and give each a distinct key:

```jsonc
{
  "agent_servers": {
    "Raven (Ollama)": {
      "type": "custom",
      "command": "raven",
      "args": ["--acp", "--provider", "ollama", "--model", "deepseek-v4-flash:cloud"]
    },
    "Raven (OpenRouter)": {
      "type": "custom",
      "command": "raven",
      "args": ["--acp", "--provider", "openrouter", "--model", "x-ai/grok-4.5"]
    }
  }
}
```

## Troubleshooting

- **Raven doesn't appear in the thread menu.** Confirm the settings file is
  valid JSON, `raven` is on `PATH` (restart Zed if you edited the env), and you
  saved the `agent_servers` block. Re-open `agent: open settings` to confirm the
  entry is registered.
- **Thread fails to start.** Run the handshake command above. If it errors,
  check Raven's config (`~/.raven/config.toml`) and provider auth.
- **Credentials not picked up.** Zed doesn't automatically share its LLM keys
  with Raven. Set the key via `env` in the `agent_servers` entry, an
  `OLLAMA_API_KEY`/`OPENROUTER_API_KEY` env var available to the process, or
  Raven's own `.env`.
- **MCP tools missing.** Check both Zed's MCP server config (forwarded) and
  Raven's native MCP config.

For general ACP failure modes, see [troubleshooting.md](troubleshooting.md).
