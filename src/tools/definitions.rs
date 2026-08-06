//! OpenAI-style function-calling tool definitions for the model.

/// OpenAI-style function-calling tool definitions for the model.
///
/// Returns a JSON array of tool schemas consumed by the `/v1/chat/completions`
/// `tools` field. The names here must match the arms in [`crate::tools::dispatch`].
pub fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "type": "function",
            "function": {
                "name": "list_dir",
                "description": "List files and directories relative to the workspace root.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path (default '.')" }
                    },
                    "required": []
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file (optionally a line range). Always prefer reading before editing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer", "description": "1-based start" },
                        "max_lines": { "type": "integer", "description": "Max lines (default 400)" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_replace",
                "description": "Edit a file by replacing an exact string. If old_string is empty, create a new file. Use replace_all to replace every occurrence; otherwise old_string must be unique.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "old_string": { "type": "string", "description": "Exact text to find (empty = create new file)" },
                        "new_string": { "type": "string" },
                        "replace_all": { "type": "boolean", "description": "Replace all occurrences (default false)" }
                    },
                    "required": ["path", "old_string", "new_string"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write full content to a file (creates parent dirs). Prefer search_replace for edits to existing files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "grep",
                "description": "Search file contents with a regex pattern. Returns matching lines with file:line: snippet.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "pattern": { "type": "string", "description": "Rust regex pattern" },
                        "path": { "type": "string", "description": "Relative directory to search (default workspace root)" },
                        "include": { "type": "string", "description": "Glob filter for file names, e.g. '*.rs'" },
                        "max_results": { "type": "integer" }
                    },
                    "required": ["pattern"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_shell",
                "description": "Run a shell command inside the workspace sandbox. Prefer dedicated tools (read_file, grep, list_dir) over cat/grep/find.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" },
                        "timeout": { "type": "integer", "description": "Seconds (default 60)" }
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "Search source files for a literal text query (case-insensitive). Prefer grep for regex.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "max_results": { "type": "integer" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "todo_write",
                "description": "Create or replace a structured task list. Use for any task with 3+ steps. Send the complete list each call (full-replace).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "todos": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "content": { "type": "string" },
                                    "status": { "type": "string", "enum": ["pending", "in_progress", "completed"] },
                                    "priority": { "type": "string", "enum": ["low", "medium", "high"] }
                                },
                                "required": ["content", "status"]
                            }
                        }
                    },
                    "required": ["todos"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "memory_update",
                "description": "Save a durable project fact to memory (persists across sessions). Use for conventions, decisions, or context — not ephemeral task progress.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "section": { "type": "string", "enum": ["Conventions", "Decisions", "Context"], "description": "Memory section to append to" },
                        "content": { "type": "string", "description": "Content to append (one fact per call)" }
                    },
                    "required": ["section", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_status",
                "description": "Show working tree status (git status --porcelain). Read-only.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_diff",
                "description": "Show unstaged changes (git diff). Set staged=true for staged changes. Read-only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "staged": { "type": "boolean", "description": "Show staged changes instead of unstaged (default false)" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_log",
                "description": "Show recent commit history (git log --oneline). Read-only.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "n": { "type": "integer", "description": "Number of commits to show (default 10)" }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "apply_patch",
                "description": "Apply a unified diff patch to files. Supports multiple hunks and files. Rejects if context doesn't match.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "patch": { "type": "string", "description": "Unified diff format patch text" }
                    },
                    "required": ["patch"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_tests",
                "description": "Auto-detect and run the project's test suite (cargo test, npm test, or pytest). Returns output with exit code.",
                "parameters": { "type": "object", "properties": {} }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "ask_user",
                "description": "Ask the user a question and wait for their typed answer. Use when you need a decision, clarification, or confirmation you cannot resolve from the workspace. Keep the question concise and specific.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "question": { "type": "string", "description": "The question to ask the user" }
                    },
                    "required": ["question"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web and return a ranked list of result titles and URLs (10 per page). No API key required. Use for current, factual, or unfamiliar topics. Use the page parameter to fetch subsequent pages.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "The search query" },
                        "page": { "type": "integer", "description": "Page number (1-indexed, default 1). Each page returns up to 10 results." }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a URL and return its readable text (HTML stripped). Use to read a page found via web_search or a known URL. Only http/https is allowed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Absolute http(s) URL to fetch" }
                    },
                    "required": ["url"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "skill_search",
                "description": "List skills whose name or description matches a query, or all skills when the query is empty. Skills are SKILL.md files in .raven/skills/ or ~/.raven/skills/.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Keyword to match against skill names/descriptions (empty lists all)" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "skill_load",
                "description": "Load a skill's instructions into context as a <skill> envelope. Call skill_search first to find the exact name.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Exact skill name to load" }
                    },
                    "required": ["name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "memory_search",
                "description": "Search project memory (.raven/MEMORY.md) for lines matching a query. Use to recall past decisions, conventions, or context stored in memory.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Keywords to search for in memory" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "git_commit",
                "description": "Stage all changes and create a git commit. Use to checkpoint your work when you've finished a coherent unit. Requires a non-empty commit message.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "message": { "type": "string", "description": "Commit message describing the change" }
                    },
                    "required": ["message"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_lint",
                "description": "Auto-detect and run the project's linter or type checker (cargo clippy for Rust, tsc for TypeScript, eslint for JS, compileall for Python). Reports problems without fixing them. Run after editing to catch issues.",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        }
    ])
}

/// The read-only tool subset exposed during plan mode.
///
/// These let the model gather context (list/read/search/git-inspect) to
/// produce a good plan, but exclude every tool that writes to the workspace
/// or runs shell. Kept in sync with the full [`tool_definitions`] list.
pub fn plan_tool_definitions() -> serde_json::Value {
    const PLAN_TOOLS: &[&str] = &[
        "list_dir",
        "read_file",
        "grep",
        "search_code",
        "git_status",
        "git_diff",
        "git_log",
        "web_search",
        "web_fetch",
        "skill_search",
        "skill_load",
        "memory_search",
        "exit_plan_mode",
    ];

    let all = tool_definitions();
    let mut arr = all.as_array().cloned().unwrap_or_default();
    arr.push(serde_json::json!({
        "type": "function",
        "function": {
            "name": "exit_plan_mode",
            "description": "Signal that the plan is complete and ready to execute. Call this with a short plan summary when your plan is finished.",
            "parameters": {
                "type": "object",
                "properties": {
                    "summary": { "type": "string", "description": "Short summary of the plan" }
                },
                "required": []
            }
        }
    }));
    let filtered: Vec<serde_json::Value> = arr
        .into_iter()
        .filter(|tool| {
            tool.get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .map(|name| PLAN_TOOLS.contains(&name))
                .unwrap_or(false)
        })
        .collect();
    serde_json::Value::Array(filtered)
}
