# Omarchy integration (agents panel + default agent)

Huginn is not one of Omarchy's stock coding agents (`claude`, `codex`, `grok`,
…). On [Omarchy](https://omarchy.org/) you wire it in **locally** so it can be:

1. **Your default agent** — launched from the menu / keybinding (`omarchy-agent`)
2. **A tab in the Agents bar panel** — session counts, real provider token
   meters by day and model (`ret.agents`, cloned from `omarchy.agents`)

Stock Omarchy binaries live under `/usr/share/omarchy/` and are overwritten on
update. Everything below stays in your home directory and survives
`omarchy update`.

> **Audience:** Omarchy Linux users who already have `huginn` on `PATH`.
> Not applicable to macOS desktop shells.

## Prerequisites

- Omarchy with the shell bar (`omarchy-shell` / Quickshell)
- `huginn` on `PATH` (`command -v huginn`)
- Sessions under workspace `.huginn/sessions/` (Huginn creates these automatically
  when you work in a repo — typically `~/Work/<project>/.huginn/sessions/`)
- Brand mark at [`assets/huginn.svg`](../assets/huginn.svg) (monochrome,
  `currentColor`) for the panel / Hearth icons

## Architecture (why wrappers exist)

| Concern | Stock Omarchy | Local Huginn wiring |
|---|---|---|
| Default agent list | Hardcoded in `omarchy-default-agent` | `~/.local/bin/omarchy-default-agent` adds a `huginn` branch |
| Launch | `omarchy-agent` case statement | `~/.local/bin/omarchy-agent` runs `huginn --yolo` when default is `huginn` |
| Usage collectors | Only `$OMARCHY_PATH/bin/omarchy-agent-usage-*` | `~/.local/bin/omarchy-agent-usage-huginn` + update wrapper |
| Panel refresh | Collector discovery is only under `$OMARCHY_PATH/bin` | Cloned plugin calls `~/.local/bin/omarchy-agent-usage-update` by **absolute path** (do not rely on Quickshell `PATH` order) |
| Panel UI / mark | Packaged `omarchy.agents` | `omarchy plugin clone omarchy.agents` → `~/.config/omarchy/plugins/<user>.agents/` |

`~/.local/bin` must be on your interactive `PATH` (Omarchy normally puts it
there). Menu actions that invoke `omarchy-agent` / `omarchy-default-agent` by
name pick up the wrappers. Prefer an **absolute** Hyprland bind for the Agent
key (see below) so launch does not depend on compositor `PATH`. The
`omarchy …` dispatcher still execs binaries under `$OMARCHY_PATH/bin` directly —
use the bare wrapper names or the menu for Huginn, not `omarchy default agent huginn`.

---

## 1. Default agent (picker + launch)

### Menu entry

Extend `~/.config/omarchy/extensions/omarchy-menu.jsonc`:

```jsonc
{
  "setup.default.agent.huginn": {
    "icon": "󰇥",
    "label": "Huginn",
    "description": "Local Huginn coding agent",
    "when": "omarchy-cmd-present huginn",
    "checked": "[[ \"$(omarchy-default-agent)\" == \"huginn\" ]]",
    "action": "omarchy-default-agent huginn"
  }
}
```

The menu hot-reloads; or run `omarchy menu refresh`.

### Wrappers

Install executable wrappers ahead of `/usr/share/omarchy/bin` on `PATH`
(usually `~/.local/bin`):

**`omarchy-default-agent`** — accept `huginn`, write
`~/.config/omarchy/defaults/agent`, then launch; otherwise exec
`/usr/bin/omarchy-default-agent`.

**`omarchy-agent`** — if the default file contains `huginn`, launch:

```bash
huginn --yolo          # interactive (via omarchy-launch-tui)
huginn --yolo -p "…"   # when given --prompt
```

Otherwise exec `/usr/bin/omarchy-agent` unchanged. Match stock behavior: if
`$PWD` is `$HOME` and `~/Work` exists, `cd` there before starting.

### Verify

```bash
command -v omarchy-default-agent   # → ~/.local/bin/…
omarchy-default-agent huginn        # sets default and opens Huginn
# or: omarchy menu summon setup.default.agent
```

Keybinding `Super+Shift+Ctrl+A` should call the wrapper by absolute path so it
cannot fall through to stock `omarchy-agent` (which has no `huginn` branch):

```lua
-- ~/.config/hypr/bindings.lua
hl.unbind("SUPER + SHIFT + CTRL + A")
o.bind("SUPER + SHIFT + CTRL + A", "Agent", "/home/YOU/.local/bin/omarchy-agent --pick")
```

The shell alias `a='omarchy-agent --inline'` uses the same wrapper once Huginn is
the default (interactive shells normally put `~/.local/bin` early on `PATH`).

---

## 2. Agents bar panel (usage)

### Clone the stock plugin

Never edit `/usr/share/omarchy/shell/plugins/agents/`. Clone it:

```bash
omarchy plugin clone omarchy.agents
# → ~/.config/omarchy/plugins/<username>.agents/
# bar id becomes e.g. ret.agents
```

### Place it next to Tailscale

Stock Omarchy keeps agents on the **right** section beside Tailscale. After a
clone it may land in the center — move it:

```bash
omarchy bar move ret.agents --after omarchy.tailscale
# use your cloned id if different: omarchy plugin list
```

### Huginn-only providers (recommended)

With several agents enabled, the panel sorts tabs alphabetically (`codex`
before `huginn`). Disable everything else so the panel is Huginn-only:

```bash
omarchy bar set ret.agents providers '{
  "huginn": { "enabled": true },
  "claude": { "enabled": false },
  "codex": { "enabled": false },
  "fireworks": { "enabled": false }
}' --json
```

Also set the same defaults in the clone's `manifest.json` under
`barWidget.defaults.providers` so a later reset stays Huginn-first.

### Marks

Omarchy's panel loads SVGs as images, so it needs real fills — not
`currentColor`. Source of truth in this repo: [`assets/huginn.svg`](../assets/huginn.svg).

```bash
PLUGIN=~/.config/omarchy/plugins/ret.agents   # adjust username
SRC=assets/huginn.svg

# Dark surfaces → light glyph; light surfaces → dark glyph
sed 's/fill="currentColor"/fill="#fff"/' "$SRC" > "$PLUGIN/assets/huginn.svg"
sed 's/fill="currentColor"/fill="#111"/' "$SRC" > "$PLUGIN/assets/huginn-light.svg"
```

(Hearth, a separate project, can reuse the same path art as a `currentColor`
icon — e.g. `crates/ui/assets/icons/raven-mark.svg` in that repo.)

### Point the panel at a local usage updater

Quickshell's environment puts `/usr/share/omarchy/bin` **before**
`~/.local/bin`, so a PATH wrapper alone never runs for panel refreshes. In the
clone's `Main.qml`, change `updateCommand` to call the wrapper by absolute
path:

```qml
var command = [root.home + "/.local/bin/omarchy-agent-usage-update"]
```

Update `moduleName` / `ipcTarget` in `Panel.qml` to the cloned id
(e.g. `ret.agents`) so IPC matches:

```bash
omarchy-shell ret.agents refresh
omarchy-shell ret.agents toggle
```

### Collectors

1. **`~/.local/bin/omarchy-agent-usage-huginn`** — scans session roots and prints
   one JSON record (`id: "huginn"`) on stdout.
2. **`~/.local/bin/omarchy-agent-usage-update`** — runs
   `/usr/bin/omarchy-agent-usage-update`, then runs the Huginn collector and
   writes `~/.local/state/omarchy/agents/usage/huginn.json`. With no explicit
   agent list it **`--except`s `claude` / `codex` / `fireworks` by default**
   and **deletes** those agents' stale `*.json` under the usage dir so the
   panel cannot rediscover them. Pass e.g. `omarchy-agent-usage-update codex`
   only if you intentionally want a stock collector again.

Session roots (in order):

- `~/.huginn/sessions`
- `~/.huginn/system/sessions` (OS-administration / `huginn --system` audit trail)
- `~/Work/.huginn/sessions` and `~/Work/*/.huginn/sessions` (one level)
- `~/Development` and `~/src` the same way, when those trees exist
- Extra entries from `HUGINN_SESSION_ROOTS` (colon-separated) — each entry must
  be a **sessions directory** (the folder that contains session id subdirs),
  e.g. `~/src/myapp/.huginn/sessions`, not the project root alone

#### Token totals

Huginn persists the provider's real token meter on each assistant message in
`messages.jsonl` (`usage` with `promptTokens` / `completionTokens` /
`totalTokens`), including tool-call-only turns. The collector prefers those
meters and reports input and output separately. **Tokens by day** and
`todayTotalTokens` are input+output — the same contract the panel uses for
Claude/Codex — so they can be compared with **Tokens by model** (which is
lifetime input+output). Transcripts written by older Huginn builds (or
providers that never report usage) fall back to the ≈ `ceil(len(text) / 4)`
estimate, counted as output. Prompt and session counts follow billed
requests (one per metered assistant message).

#### Manual refresh

```bash
omarchy-agent-usage-update --force huginn
omarchy-shell ret.agents refresh
```

---

## 3. Efficiency checklist

Do these once; the panel stays out of the way afterward.

| Goal | Action |
|---|---|
| Only Huginn in the panel | Disable `claude` / `codex` / `fireworks` (above) |
| Correct bar slot | `--after omarchy.tailscale` on the right section |
| Faster refreshes | `omarchy bar set ret.agents refreshIntervalSec 300 --json` (default 900) |
| Extra project trees | `export HUGINN_SESSION_ROOTS=~/src/app/.huginn/sessions:~/Projects/foo/.huginn/sessions` (sessions dirs, not project roots) |
| Skip empty machines | Panel hides itself until a usage JSON has data — run Huginn once under `~/Work/…` |
| Survive Omarchy updates | Keep wrappers + clone under `~`; never edit `/usr/share/omarchy/` |
| Default launch feels native | Set Huginn via the menu; wrapper uses `--yolo` so keybindings do not block on confirmations |

Stock collectors stay quiet while Huginn-only mode is on (wrapper default
`--except` plus panel `providers.*.enabled: false`).

---

## 4. Verify end-to-end

```bash
# Wrappers resolve locally
command -v omarchy-agent omarchy-default-agent omarchy-agent-usage-update

# Usage record exists and looks like Huginn
omarchy-agent-usage-huginn | jq '{id,name,totalSessions,totalPrompts,activeDays}'

# Panel data file
jq '{id,ready,totalSessions}' ~/.local/state/omarchy/agents/usage/huginn.json

# Plugin enabled on the bar
omarchy plugin list --json | jq '.[] | select(.id|test("agents"))'
```

Then open the Agents icon on the bar: one hero (**Huginn** / Local), tokens by
day, tokens by model — no provider switcher when only one agent is enabled.

---

## Troubleshooting

| Symptom | Fix |
|---|---|
| Menu has no Huginn row | `when` failed — `huginn` not on `PATH`; or menu JSONC syntax error |
| `omarchy default agent huginn` fails | Expected — dispatcher bypasses `~/.local/bin`. Use `omarchy-default-agent huginn` or the menu |
| Panel never shows Huginn | No `huginn.json`, or `providers.huginn.enabled` is false; run the collector and `omarchy-shell <id>.agents refresh` |
| Codex/Claude still appear | Re-apply the providers JSON; delete stale `~/.local/state/omarchy/agents/usage/{claude,codex,fireworks}.json`; confirm you edited **`ret.agents`** (not `omarchy.agents`). The local update wrapper should `--except` those by default |
| Keybind opens OpenCode/Codex | Default file was not `huginn` — run `omarchy-default-agent huginn` (or `printf 'huginn\n' > ~/.config/omarchy/defaults/agent`). Point the Hyprland bind at `~/.local/bin/omarchy-agent` |
| Usage stuck / never refreshes | Clone still calls bare `omarchy-agent-usage-update` — switch `Main.qml` to the absolute `~/.local/bin/…` path |
| Mark missing | Add `assets/huginn.svg` (+ optional `huginn-light.svg`) under the clone; panel falls back to the bar glyph otherwise |
| Sessions missing from totals | Work outside `~/Work` without `HUGINN_SESSION_ROOTS`; or empty `messages.jsonl` |

For Huginn itself (models, sandbox, ACP), see [troubleshooting.md](troubleshooting.md)
and [zed_connection.md](zed_connection.md).

---

## System scope (`huginn --system`)

Beyond the repo-scoped coding default, Huginn ships an opt-in OS-administration
scope. It runs the **same TUI** as the default agent — same interface, same
slash commands — with better system knowledge and troubleshooting ability.
Key differences:

- The sandbox is rooted at `/` (write access anywhere), which is what lets it
  reach system config. The shell policy is tiered: read-only diagnostics
  (`pacman -Q`, `systemctl status`, `journalctl`, `hyprctl` reads, …) run
  without a prompt; mutations (installs, service changes, `sudo`) confirm
  first. `--system --yolo` is a supported opt-in to full autonomy on a
  trusted single-user machine — this device's wrappers use it (the
  destructive-command denylist and network block still apply).
- It loads a system OS-administration prompt that prefers
  `omarchy <group> <action>` commands, never edits `/usr/share/omarchy/` (that
  is package-owned and overwritten on `omarchy update`), backs up configs
  before writing, and reads before changing.
- Sessions persist under `~/.huginn/system/sessions/` (audit trail); memory is
  `~/.huginn/system/MEMORY.md`. No enforced-verify gate (no workspace test
  runner for OS work), no sub-agents.
- Network in shell commands is blocked by default — set
  `HUGINN_SANDBOX_NETWORK_BLOCK=0` (e.g. in `~/.huginn/.env`) for package
  installs; the block applies in both scopes and survives `--yolo`.

Launch contract:

```bash
huginn --system        # interactive TUI (same UI as the default agent)
huginn --system -p "show the disk layout and running services"   # one-shot
huginn --system --list-sessions   # audit trail lives in ~/.huginn/system/sessions
```

### Desktop wiring for system scope (local wrappers)

On this machine the system scope is reachable from the desktop, not only from
a shell. All of it lives under `~` and survives `omarchy update`:

| Surface | Entry point |
|---|---|
| Terminal / script | `omarchy-agent-system [--inline] [--prompt "task"]` |
| Agent wrapper passthrough | `omarchy-agent --system [--prompt "task"]` |
| Omarchy menu | `Trigger → Huginn System Task` (`omarchy menu summon trigger.huginn-system`) |
| Crash notification | Click "Process crashed" → `omarchy-agent-crash` → Huginn in system scope |

- **`~/.local/bin/omarchy-agent-system`** — asks for a task (gum when
  available) and runs `huginn --system -p "…"` inside `omarchy-launch-tui`
  under the shared `org.omarchy.agent` app-id; `--inline` execs huginn directly.
  It never adds `--yolo`: system scope forces `confirm_shell` on, and
  state-changing commands prompt in the terminal.
- **`~/.local/bin/omarchy-agent`** — when the default agent is `huginn`,
  `--system` passthrough switches to the OS-administration scope; everything
  else keeps launching the repo-scoped `huginn --yolo`.
- **`~/.local/bin/omarchy-agent-crash`** — when the default agent is `huginn`,
  routes crash diagnosis to `omarchy-agent-system` with the same
  systemd-coredump facts the stock script gathers, pointing at the
  `diagnose-crash` skill via `~/.huginn/skills/` (Huginn's skill root).
  Otherwise it falls through to the stock binary.

The diagnose-crash skill is symlinked into `~/.huginn/skills/` next to the
omarchy skill, so both are discoverable with `skill_search` in either scope.

The wrappers and Agents panel otherwise drive the **repo** default. When you
want Huginn to act on the OS, use any of the entries above; the effective
command is always `huginn --system "<task>"`. See
[security.md](security.md) for the system-scope trust posture.

---

## Related files (this machine's layout)

```
~/.local/bin/omarchy-default-agent
~/.local/bin/omarchy-agent
~/.local/bin/omarchy-agent-system
~/.local/bin/omarchy-agent-crash
~/.local/bin/omarchy-agent-usage-huginn
~/.local/bin/omarchy-agent-usage-update
~/.config/omarchy/extensions/omarchy-menu.jsonc   # setup.default.agent.huginn + trigger.huginn-system
~/.config/omarchy/plugins/<user>.agents/           # clone + huginn assets + Main.qml path fix
~/.config/omarchy/shell.json                       # bar placement + providers
~/.local/state/omarchy/agents/usage/huginn.json     # panel input
~/.huginn/skills/omarchy                            # symlink → omarchy skill
~/.huginn/skills/diagnose-crash                     # symlink → diagnose-crash skill
assets/huginn.svg                                     # brand mark (repo)
```
