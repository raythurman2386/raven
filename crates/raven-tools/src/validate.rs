//! Pre-dispatch hygiene for tool calls: size caps and required-field checks.
//!
//! These checks run before the sandbox executes anything. They are not a
//! security boundary — they stop truncated/malformed model output from
//! firing a write or shell command on empty inputs, and they bound memory
//! spent on a single arguments blob.

use serde_json::Value;

/// Maximum size of a tool-call `arguments` JSON string.
pub const MAX_ARGUMENTS_BYTES: usize = 1_048_576;

/// Maximum length of a filesystem path argument.
pub const MAX_PATH_CHARS: usize = 4096;

/// Maximum length of a `run_shell` command string.
pub const MAX_COMMAND_CHARS: usize = 32_768;

/// Validate a parsed tool-call payload.
///
/// `raw` is the original arguments JSON string (used for the byte cap).
/// Returns `Err` with a model-visible error string when the call must not
/// be dispatched.
pub fn validate_tool_call(name: &str, raw: &str, args: &Value) -> Result<(), String> {
    if raw.len() > MAX_ARGUMENTS_BYTES {
        return Err(format!(
            "Tool error: arguments for {name} exceed {MAX_ARGUMENTS_BYTES} bytes"
        ));
    }
    if !args.is_object() {
        return Err(format!(
            "Tool error: arguments for {name} must be a JSON object"
        ));
    }

    match name {
        "read_file" | "write_file" | "search_replace" => {
            require_nonempty_str(name, args, "path")?;
            cap_str(name, args, "path", MAX_PATH_CHARS)?;
        }
        "list_dir" | "grep" => {
            cap_str(name, args, "path", MAX_PATH_CHARS)?;
        }
        _ => {}
    }

    match name {
        "read_file" => {}
        "write_file" => {
            require_str(name, args, "content")?;
        }
        "search_replace" => {
            require_str(name, args, "old_string")?;
            require_str(name, args, "new_string")?;
        }
        "grep" => {
            require_nonempty_str(name, args, "pattern")?;
        }
        "run_shell" => {
            require_nonempty_str(name, args, "command")?;
            cap_str(name, args, "command", MAX_COMMAND_CHARS)?;
        }
        "search_code" => {
            require_nonempty_str(name, args, "query")?;
        }
        "todo_write" => {
            require_array(name, args, "todos")?;
        }
        "think" => {
            require_nonempty_str(name, args, "thought")?;
        }
        "delegate_task" => {
            require_nonempty_str(name, args, "description")?;
        }
        "goal_set" => {
            require_nonempty_str(name, args, "description")?;
        }
        "memory_update" => {
            require_nonempty_str(name, args, "section")?;
            require_str(name, args, "content")?;
        }
        "memory_search" | "web_search" | "skill_search" => {
            // query may be empty for "list all" style searches
            if let Some(v) = args.get("query") {
                require_type_str(name, "query", v)?;
            }
        }
        "skill_load" => {
            require_nonempty_str(name, args, "name")?;
        }
        "apply_patch" => {
            require_nonempty_str(name, args, "patch")?;
        }
        "ask_user" => {
            require_nonempty_str(name, args, "question")?;
        }
        "web_fetch" => {
            require_nonempty_str(name, args, "url")?;
        }
        _ => {}
    }
    Ok(())
}

fn require_str(tool: &str, args: &Value, field: &str) -> Result<(), String> {
    match args.get(field) {
        Some(v) => require_type_str(tool, field, v),
        None => Err(format!(
            "Tool error: {tool} requires string field '{field}'"
        )),
    }
}

fn require_nonempty_str(tool: &str, args: &Value, field: &str) -> Result<(), String> {
    require_str(tool, args, field)?;
    let s = args.get(field).and_then(Value::as_str).unwrap_or("");
    if s.trim().is_empty() {
        return Err(format!(
            "Tool error: {tool} field '{field}' must be a non-empty string"
        ));
    }
    Ok(())
}

fn require_type_str(tool: &str, field: &str, v: &Value) -> Result<(), String> {
    if v.as_str().is_some() {
        Ok(())
    } else {
        Err(format!(
            "Tool error: {tool} field '{field}' must be a string"
        ))
    }
}

fn require_array(tool: &str, args: &Value, field: &str) -> Result<(), String> {
    match args.get(field) {
        Some(v) if v.is_array() => Ok(()),
        Some(_) => Err(format!(
            "Tool error: {tool} field '{field}' must be an array"
        )),
        None => Err(format!("Tool error: {tool} requires array field '{field}'")),
    }
}

fn cap_str(tool: &str, args: &Value, field: &str, max_chars: usize) -> Result<(), String> {
    if let Some(s) = args.get(field).and_then(Value::as_str) {
        if s.chars().count() > max_chars {
            return Err(format!(
                "Tool error: {tool} field '{field}' exceeds {max_chars} characters"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_oversized_raw_arguments() {
        let raw = "x".repeat(MAX_ARGUMENTS_BYTES + 1);
        let err = validate_tool_call("read_file", &raw, &json!({"path": "a.rs"})).unwrap_err();
        assert!(err.contains("exceed"));
    }

    #[test]
    fn rejects_non_object_args() {
        let err = validate_tool_call("read_file", "[]", &json!([])).unwrap_err();
        assert!(err.contains("JSON object"));
    }

    #[test]
    fn rejects_missing_required_path() {
        let err = validate_tool_call("read_file", "{}", &json!({})).unwrap_err();
        assert!(err.contains("path"));
    }

    #[test]
    fn rejects_empty_run_shell_command() {
        let err = validate_tool_call("run_shell", "{}", &json!({"command": "   "})).unwrap_err();
        assert!(err.contains("command"));
    }

    #[test]
    fn rejects_wrong_type_for_path() {
        let err =
            validate_tool_call("write_file", "{}", &json!({"path": 1, "content": ""})).unwrap_err();
        assert!(err.contains("string"));
    }

    #[test]
    fn accepts_valid_write_file() {
        validate_tool_call(
            "write_file",
            r#"{"path":"a.rs","content":""}"#,
            &json!({"path": "a.rs", "content": ""}),
        )
        .unwrap();
    }

    #[test]
    fn accepts_empty_old_string_for_create() {
        validate_tool_call(
            "search_replace",
            "{}",
            &json!({"path": "a.rs", "old_string": "", "new_string": "fn main() {}"}),
        )
        .unwrap();
    }

    #[test]
    fn rejects_oversized_path() {
        let path = "a".repeat(MAX_PATH_CHARS + 1);
        let err = validate_tool_call("read_file", "{}", &json!({"path": path})).unwrap_err();
        assert!(err.contains("exceeds"));
    }

    #[test]
    fn todo_write_requires_array() {
        let err = validate_tool_call("todo_write", "{}", &json!({"todos": "nope"})).unwrap_err();
        assert!(err.contains("array"));
    }

    #[test]
    fn unknown_tools_are_not_schema_checked() {
        validate_tool_call("nonexistent_tool", "{}", &json!({})).unwrap();
    }
}
