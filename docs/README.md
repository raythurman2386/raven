# Raven documentation

User and contributor guides for **Raven**. Start with the [root README](../README.md) for install and quick start.

## Guides

| Guide | Audience | Contents |
|---|---|---|
| [usage.md](usage.md) | Users | Day-to-day workflows, examples, plan mode, parallel sub-agents |
| [configuration.md](configuration.md) | Users | Env vars, CLI flags, context window, API keys, AGENTS.md |
| [example.config.toml](example.config.toml) | Users | Fully-commented reference config |
| [zed_connection.md](zed_connection.md) | Users | Connect Raven to Zed / Hearth as an ACP external agent |
| [omarchy.md](omarchy.md) | Users (Omarchy Linux) | Default agent + Agents bar panel: wrappers, usage collector, Raven-only setup |
| [troubleshooting.md](troubleshooting.md) | Users | Common failure modes: stream errors, sandbox denies, SearXNG fallback, ACP |
| [tools.md](tools.md) | Users + Contributors | Tool contracts, parameters, sandbox rules, blocked commands |
| [architecture.md](architecture.md) | Contributors | Design, agent loop, compaction, sandbox, module map |
| [contributing.md](contributing.md) | Contributors | Build, style, how to add a tool or event |
| [testing.md](testing.md) | Contributors | Test structure, coverage, mutation testing |
| [security.md](security.md) | Security reviewers | Threat model, defense layers, platform caveats |
| [../evals/README.md](../evals/README.md) | Contributors | Agent eval suite (offline + live task fixtures) |

## Quick links

- [Install & quick start](../README.md#quick-start)
- [Configuration](configuration.md)
- [Omarchy agents panel](omarchy.md)
- [Compaction](architecture.md#compaction)
- [Adding a tool](contributing.md#adding-a-tool)
