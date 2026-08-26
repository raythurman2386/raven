# Tools reference

All file paths are **relative to the workspace root** and confined to it. See [architecture.md#sandbox](architecture.md#sandbox) for confinement details.

---

## Tool list

| Tool | Purpose | Sandbox behavior |
|---|---|---|
| `list_dir` | List files/directories | Read-only, workspace-confined |
| `read_file` | Read a file (optional line range, 1-based) | Read-only, workspace-confined; lines truncated at 2000 chars; extracts .docx/.pdf/.xlsx/.odt/.epub/.pptx/.csv/.rtf/.ods/.odp/.doc/.xls/.ppt to Markdown |
| `search_replace` | Edit a file by replacing an exact string | Workspace-confined; rejects directories |
| `write_file` | Full file write (create/overwrite) | Workspace-confined; creates parent dirs |
| `grep` | Regex content search with optional glob filter | Read-only; skips hidden dirs and build artifacts |
| `run_shell` | Run a shell command | `cwd` forced to workspace; dangerous patterns blocked; secret env vars stripped; direct-exec for safe commands; OS-level confinement (Landlock/seccomp/rlimits/Job Object); Landlock writes are workspace + extras + `/dev` only (`TMPDIR` pinned under `.raven/tmp`); 60s default timeout; output capped at 12 000 chars |
| `search_code` | Literal case-insensitive search across source files | Read-only; source extensions only |
| `todo_write` | Create/replace a structured task list (full-replace) | Persists to `.raven/state/todos.json`; injected into the system prompt |
| `goal_set` | Set/update the current goal for a task | Persists to `.raven/state/goal.json`; injected into the system prompt |
| `delegate_task` | Spawn a focused sub-agent in a fresh context window, return its summary | Runs a nested agent; shares the workspace; nesting disabled (no recursive delegate); output capped |
| `think` | Record a thought for structured mid-task reasoning | Read-only no-op; available in plan/chat |
| `memory_update` | Save a durable project fact to `.raven/MEMORY.md` | Writes to workspace memory file |
| `memory_search` | Search project memory by keyword | Read-only; scans `.raven/MEMORY.md` |
| `git_status` | Show working tree status | Read-only; runs `git status --porcelain` |
| `git_diff` | Show unstaged or staged changes | Read-only; runs `git diff` |
| `git_log` | Show recent commit history | Read-only; runs `git log` |
| `apply_patch` | Apply a unified diff patch to files | Workspace-confined; parses and applies patches |
| `run_tests` | Auto-detect and run project test suite | Runs `cargo test` / `npm test` / `pytest`; skips rlimits (large linker outputs, long builds) and the seccomp network block for npm projects |
| `run_lint` | Auto-detect and run project linter/type checker | Runs `cargo clippy` / `tsc` / `eslint` / `python -m compileall`; skips rlimits |
| `ask_user` | Ask the user a question | Pauses agent; user response fed back as tool result |
| `web_search` | Search the web via DuckDuckGo (or a self-hosted SearXNG instance if configured) | Read-only; no API key needed; 10 results per page |
| `web_fetch` | Fetch a URL and return readable text | Read-only; only http/https; strips HTML; 20s timeout |
| `skill_search` | List skills matching a query | Read-only; searches SKILL.md files |
| `skill_load` | Load a skill's instructions into context | Read-only; returns skill body wrapped in `<skill>` envelope |

---

## Tool contracts

### `list_dir`

```json
{
  "path": "string (optional, default '.')"
}
```

Returns a listing with dirs first, then files (alphabetical). Each line: `dir|file <name>  (<size> B)`.

### `read_file`

```json
{
  "path": "string (required)",
  "start_line": "integer (optional, 1-based, default 1)",
  "max_lines": "integer (optional, default 400)"
}
```

Returns a numbered line range. Header: `--- <path> (lines A-B of N) ---`. Lines longer than 2000 chars are truncated.

### `search_replace`

```json
{
  "path": "string (required)",
  "old_string": "string (required; empty = create new file)",
  "new_string": "string (required)",
  "replace_all": "boolean (optional, default false)"
}
```

Behavior:

- `old_string` empty → create a new file (fails if the file already exists).
- `replace_all: true` → replace every occurrence.
- `replace_all: false` (default) → replace the first occurrence; **must be unique**. If not unique, returns an error suggesting more context or `replace_all`.

Returns: `Edited <path>`, `Replaced N occurrence(s) in <path>`, or `Created <path> (N bytes)`.

### `write_file`

```json
{
  "path": "string (required)",
  "content": "string (required)"
}
```

Full file write (create or overwrite). Creates parent directories as needed. Prefer `search_replace` for edits to existing files.

### `grep`

```json
{
  "pattern": "string (required, Rust regex)",
  "path": "string (optional, relative dir, default workspace root)",
  "include": "string (optional, glob filter e.g. '*.rs')",
  "max_results": "integer (optional, default 50)"
}
```

Regex content search. Returns `file:line: snippet` lines. Skips: `.git`, `node_modules`, `__pycache__`, `.venv`, `venv`, `target`, `dist`, `build`, and any hidden directory (starting with `.`).

The `include` glob supports `*` and `?` against the file name only (not the full path).

### `run_shell`

```json
{
  "command": "string (required)",
  "timeout": "integer (optional, seconds, default 60)"
}
```

Runs the command with `cwd` forced to the workspace. Allowlisted commands with no shell metacharacters run via **direct exec** (`Command::new(bin).args(...)`); everything else runs via `sh -c <command>`. Output format: `exit=<code>\n<stdout><stderr>`. Output capped at 12 000 chars (truncated with `...[truncated]`). Confined subprocesses additionally run under OS-level sandboxing (Landlock, seccomp, rlimits, or Windows Job Objects — see [security.md](security.md)). Commands matching the verification-gate predicate (`cargo test`, `cargo clippy`, `cargo fmt --check`, `npm test`, `pytest`, `tsc`, `eslint`, …) skip both the seccomp network block and rlimits, since sanctioned test/lint/format commands legitimately need network sockets and large linker outputs.

### `search_code`

```json
{
  "query": "string (required, literal, case-insensitive)",
  "max_results": "integer (optional, default 25)"
}
```

Literal case-insensitive search across source files. Only searches known source extensions: `py`, `js`, `ts`, `tsx`, `jsx`, `rs`, `go`, `java`, `cpp`, `c`, `h`, `md`, `txt`, `toml`, `yaml`, `yml`, `json`, `sh`, `bash`, `css`, `html`, `sql`. Prefer `grep` for regex.

### `todo_write`

```json
{
  "todos": [
    {
      "content": "string (required)",
      "status": "pending | in_progress | completed (required)",
      "priority": "low | medium | high (optional, default medium)"
    }
  ]
}
```

Full-replace semantics: each call replaces the entire todo list. Returns a summary like:

```
[completed] 1: Set up project structure
[in_progress] 2: Implement auth module
[pending] 3: Write tests
```

State is **persisted** to `.raven/state/todos.json` (atomic write) and injected
into the system prompt on each turn, so it survives context compaction and
session resume.

### `goal_set`

```json
{
  "description": "string (required)",
  "status": "pending | in_progress | completed (optional, default in_progress)"
}
```

Sets or updates the current goal. Persisted to `.raven/state/goal.json` and
injected into the system prompt on each turn. Use at the start of a multi-step
task and whenever the objective changes.

### `delegate_task`

```json
{
  "description": "string (required)"
}
```

Spawns a focused sub-agent in a **fresh context window** to work on a
self-contained sub-task, then returns its distilled output (capped at 2000
chars). The sub-agent shares the workspace and inherits the same sandbox
confinement. Nesting is disabled: the child cannot spawn another `delegate_task`
or overwrite the parent's goal/todos. Use it to offload exploration or isolated
work without bloating your own context.

### `think`

```json
{
  "thought": "string (required)"
}
```

Appends a thought to the log and returns nothing. It does not obtain new
information or change any state — it is a scratchpad for structured reasoning
mid-task. Read-only and available during planning.

### `memory_update`

```json
{
  "section": "Conventions | Decisions | Context (required)",
  "content": "string (required)"
}
```

Appends a section to `.raven/MEMORY.md`. The first 25KB of this file is injected into the system prompt on each run.

### `memory_search`

```json
{
  "query": "string (required)"
}
```

Keyword-scans `.raven/MEMORY.md` and returns matching lines as ranked `path:line — content` snippets. Lines with more query-token hits rank first. Read-only and available during planning.

### `git_status`

No parameters. Returns `git status --porcelain` output.

### `git_diff`

```json
{
  "staged": "boolean (optional, default false)"
}
```

Returns `git diff` (unstaged) or `git diff --staged` output.

### `git_log`

```json
{
  "n": "integer (optional, default 10)"
}
```

Returns the last N commits in `git log --oneline` format.

The harness does not create commits. Use `git_status` / `git_diff` / `git_log` to inspect; only create a commit via `run_shell` if the user explicitly asks.

### `apply_patch`

```json
{
  "patch": "string (required, unified diff)"
}
```

Parses and applies a unified diff patch to workspace files. Returns a summary of files changed.

Before each file is patched, a safety backup is written to `<file>.bak` (e.g. `main.rs.bak`). These backups exist purely as a manual safety net in the working tree.

### `run_tests`

No parameters. Auto-detects the project test runner (`cargo test` for Rust, `npm test` for Node, `pytest` for Python) and runs it. Returns the test output.

### `run_lint`

No parameters. Auto-detects the project linter/type checker (`cargo clippy` for Rust, `tsc`/`eslint` for TypeScript/JavaScript, `python -m compileall` for Python) and runs it. Returns the lint output. After file-editing turns, Raven auto-runs the linter and feeds errors back to the model.

### `ask_user`

```json
{
  "question": "string (required)"
}
```

Pauses the agent and asks the user a question. In the TUI the input box repurposes to show the question; in headless mode the question is printed to stderr and the answer is read from stdin. The user's response is fed back as the tool result.

### `web_search`

```json
{
  "query": "string (required)",
  "page": "integer (optional, 1-indexed, default 1)"
}
```

Searches the web via DuckDuckGo's HTML endpoint by default. No API key required. Returns up to 10 results per page (title + URL). Output capped at 12 000 chars. Read-only and available during planning.

When a [SearXNG](https://docs.searxng.org/) base URL is configured (`RAVEN_SEARXNG_URL` or the `searxng_url` config key), `web_search` queries the SearXNG JSON API instead and returns title + URL + a short snippet. If SearXNG is unreachable, returns an error, or returns empty results, it automatically falls back to DuckDuckGo so search never bricks. No API key is needed for a typical SearXNG install. See [configuration.md](configuration.md#searxng) for setup.

### `web_fetch`

```json
{
  "url": "string (required, http/https only)"
}
```

Fetches a URL and returns the page content as readable text (HTML stripped). Only `http://` and `https://` URLs are allowed. 20s total timeout, 10s connect timeout. Output capped at 12 000 chars. Read-only and available during planning.

### `skill_search`

```json
{
  "query": "string (required, empty = list all)"
}
```

Searches for skills (SKILL.md files) by name or description. Searches `.raven/skills/` (project) and `~/.raven/skills/` (global). Returns matching skill names and descriptions. Read-only and available during planning.

### `skill_load`

```json
{
  "name": "string (required, exact skill name)"
}
```

Loads a skill's full instructions into context. Returns the skill body wrapped in a `<skill>` envelope. Read-only and available during planning.

---

## Blocked commands

`run_shell` blocks commands matching this regex (case-insensitive):

```
(rm\s+(-[a-z]*f[a-z]*\s+)?/|mkfs|: \(\)\s*\{\s*:\|:&\s*\};:|dd\s+if=/dev/(zero|random|urandom)|chmod\s+(-R\s+)?777\s+/|curl\s+.*\|\s*(ba)?sh|wget\s+.*\|\s*(ba)?sh|format\s+[A-Za-z]:|del\s+/[sfq]\s+[A-Za-z]:\\|rd\s+/[sq]\s+[A-Za-z]:\\|rmdir\s+/[sq]\s+[A-Za-z]:\\|powershell\s+-[Cc]ommand\s+.*Remove-Item.*-Recurse.*-Force|Remove-Item\s+-Recurse\s+-Force\s+[A-Za-z]:\\|diskpart|/dev/tcp|bash\s+-i|nc(at)?\s+[^\n]*-e|mkfifo|powershell[^\n]*-[Ee]nc(odedcommand)?|certutil[^\n]*-decode|Invoke-Expression|\biex\s*\(|base64\s+[^\n]*\|\s*(ba)?sh|curl\s+[^\n]*\|\s*(pwsh|powershell|cmd)|wget\s+[^\n]*\|\s*(pwsh|powershell|cmd))
```

This catches:

- `rm -rf /` and variants
- `mkfs` (filesystem formatting)
- Fork bombs (`:(){ :|:& };:`)
- `dd if=/dev/zero|random|urandom`
- `chmod -R 777 /`
- `curl ... | sh` and `wget ... | sh` (pipe-to-shell)
- Windows destructive patterns: `format <drive>:`, `del /f/s/q <drive>:\`, `rd /s/q <drive>:\`, `rmdir /s/q <drive>:\`, `powershell -Command ... Remove-Item -Recurse -Force`, `Remove-Item -Recurse -Force <drive>:\`, and `diskpart`
- Reverse-shell primitives: `/dev/tcp`, `bash -i`, `nc -e` / `ncat -e`, `mkfifo`
- Encoded or decoded droppers: `powershell -enc`, `certutil -decode`, `base64 … | sh`
- PowerShell `Invoke-Expression` / `iex (`
- Pipe-to-shell via `pwsh` / `powershell` / `cmd` as well as `sh`/`bash`

These patterns are blocked even under `--yolo`. This is a **guardrail, not a complete blocklist**. A determined model can craft commands that evade it. For untrusted models, use a container or VM. Tool arguments are also length-capped and schema-checked before dispatch (see [security.md §8](security.md)).

---

## Stripped environment variables

`run_shell` removes these from the child process environment:

- `RAVEN_API_KEY`
- `OLLAMA_API_KEY`
- `OPENAI_API_KEY`
- `XAI_API_KEY`
- `ANTHROPIC_API_KEY`
- `AWS_SECRET_ACCESS_KEY`

This prevents a model from exfiltrating secrets via shell commands.