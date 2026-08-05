# Tools reference

All file paths are **relative to the workspace root** and confined to it. See [architecture.md#sandbox](architecture.md#sandbox) for confinement details.

---

## Tool list

| Tool | Purpose | Sandbox behavior |
|---|---|---|
| `list_dir` | List files/directories | Read-only, workspace-confined |
| `read_file` | Read a file (optional line range, 1-based) | Read-only, workspace-confined; lines truncated at 2000 chars |
| `search_replace` | Edit a file by replacing an exact string | Workspace-confined; rejects directories |
| `write_file` | Full file write (create/overwrite) | Workspace-confined; creates parent dirs |
| `grep` | Regex content search with optional glob filter | Read-only; skips hidden dirs and build artifacts |
| `run_shell` | Run a shell command | `cwd` forced to workspace; dangerous patterns blocked; secret env vars stripped; 60s default timeout; output capped at 12 000 chars |
| `search_code` | Literal case-insensitive search across source files | Read-only; source extensions only |
| `todo_write` | Create/replace a structured task list (full-replace) | In-memory, per agent run |

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

Runs `sh -c <command>` with `cwd` forced to the workspace. Output format: `exit=<code>\n<stdout><stderr>`. Output capped at 12 000 chars (truncated with `...[truncated]`).

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

State is in-memory and per agent run (not persisted across sessions).

---

## Blocked commands

`run_shell` blocks commands matching this regex (case-insensitive):

```
(rm\s+(-[a-z]*f[a-z]*\s+)?/|mkfs|: \(\)\s*\{\s*:\|:&\s*\};:|dd\s+if=/dev/(zero|random|urandom)|chmod\s+(-R\s+)?777\s+/|curl\s+.*\|\s*(ba)?sh|wget\s+.*\|\s*(ba)?sh)
```

This catches:

- `rm -rf /` and variants
- `mkfs` (filesystem formatting)
- Fork bombs (`:(){ :|:& };:`)
- `dd if=/dev/zero|random|urandom`
- `chmod -R 777 /`
- `curl ... | sh` and `wget ... | sh` (pipe-to-shell)

This is a **guardrail, not a complete blocklist**. A determined model can craft commands that evade it. For untrusted models, use a container or VM.

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