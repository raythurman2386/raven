//! Tool dispatch: serial vs parallel policy, result bookkeeping, and the
//! auto-lint reflection pass.
//!
//! File-mutating tools (`write_file`, `search_replace`, `apply_patch`) run
//! **serially in call order** so two edits to the same file apply in order
//! instead of racing. All other tools may run in parallel via `spawn_blocking`.
//! Results are recorded through [`Agent::record_tool_result`], which shares
//! identical bookkeeping (consecutive-failure tracking, read-only caching,
//! `ToolEnd` event, `tool` message push) across both paths.

use anyhow::Result;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::error::ToolError;
use crate::tools::{dispatch, safe_command_re, validate_tool_call, Sandbox, MAX_ARGUMENTS_BYTES};

use super::core::Agent;
use super::types::{AgentEvent, ChatMessage, ToolCall};

/// Outcome of a parallel `spawn_blocking` tool dispatch.
type ParallelToolJoin = (String, String, Result<String, ToolError>, String, bool);

/// Deferred tool outcome held until all calls finish so results can be
/// appended in original `tool_calls[]` order.
struct PendingToolResult {
    id: String,
    name: String,
    cache_key: String,
    result: Result<String, ToolError>,
    is_verification: bool,
}

impl PendingToolResult {
    fn ready(
        id: String,
        name: String,
        cache_key: String,
        result: Result<String, ToolError>,
    ) -> Self {
        Self {
            id,
            name,
            cache_key,
            result,
            is_verification: false,
        }
    }

    fn with_verification(mut self, v: bool) -> Self {
        self.is_verification = v;
        self
    }
}

impl Agent {
    /// Execute a batch of tool calls, applying the serial/parallel policy.
    ///
    /// Pushes the assistant message carrying the tool calls, dispatches each
    /// tool (file-mutating tools serially in call order, others in parallel),
    /// records results, advances plan progress, and runs the auto-lint
    /// reflection pass if this iteration edited files.
    ///
    /// `edited` is per-iteration state (whether this iteration's tools edited
    /// files, gating the lint pass); `edited_any` is turn-level state
    /// (persists across iterations, gating the enforced-verify gate). Both
    /// are updated in place.
    pub(crate) async fn execute_tool_calls(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        tcs: Vec<ToolCall>,
        assistant: ChatMessage,
        edited: &mut bool,
        edited_any: &mut bool,
    ) -> Result<()> {
        let mut assistant = assistant;
        assistant.tool_calls = Some(tcs.clone());
        self.messages.push(assistant);

        // Slot results by original call index so tool messages always follow
        // `tool_calls[]` order (OpenAI-compatible validators and some cloud
        // routers reject interleaved / reordered tool results).
        // File-mutating tools still run serially; other tools may run in
        // parallel via spawn_blocking. Only *recording* is deferred/ordered.
        let n = tcs.len();
        let mut slots: Vec<Option<PendingToolResult>> = (0..n).map(|_| None).collect();
        let mut handles: Vec<(usize, tokio::task::JoinHandle<ParallelToolJoin>)> = Vec::new();

        for (idx, tc) in tcs.iter().enumerate() {
            // If the streamed `arguments` JSON is malformed (e.g. a
            // truncated chunk), surface a clear error to the model instead
            // of silently dispatching with empty args — a write_file or
            // run_shell firing on nothing is far worse than a retry.
            if tc.function.arguments.len() > MAX_ARGUMENTS_BYTES {
                let result = format!(
                    "Tool error: arguments for {} exceed {} bytes",
                    tc.function.name, MAX_ARGUMENTS_BYTES
                );
                slots[idx] = Some(PendingToolResult::ready(
                    tc.id.clone(),
                    tc.function.name.clone(),
                    String::new(),
                    Ok(result),
                ));
                continue;
            }
            let parsed: Result<Value, serde_json::Error> =
                serde_json::from_str(&tc.function.arguments);
            let args = match parsed {
                Ok(v) => v,
                Err(e) => {
                    let result = format!(
                        "Tool error: arguments for {} are not valid JSON: {}\nRaw: {}",
                        tc.function.name, e, tc.function.arguments
                    );
                    slots[idx] = Some(PendingToolResult::ready(
                        tc.id.clone(),
                        tc.function.name.clone(),
                        String::new(),
                        Ok(result),
                    ));
                    continue;
                }
            };
            if let Err(result) =
                validate_tool_call(&tc.function.name, &tc.function.arguments, &args)
            {
                slots[idx] = Some(PendingToolResult::ready(
                    tc.id.clone(),
                    tc.function.name.clone(),
                    String::new(),
                    Ok(result),
                ));
                continue;
            }
            // ask_user is special: it blocks on a user reply over a oneshot
            // channel rather than dispatching to the (sync) tool sandbox. The
            // consumer renders the question and sends the answer back. If the
            // sender is dropped (user dismissed / no consumer), the model sees
            // that no answer was given.
            if tc.function.name == "ask_user" {
                let question = args
                    .get("question")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel::<String>();
                let _ = tx
                    .send(AgentEvent::AskUser {
                        question,
                        reply: reply_tx,
                    })
                    .await;
                let answer = reply_rx
                    .await
                    .unwrap_or_else(|_| "The user did not provide an answer.".to_string());
                let result = format!("User answered: {answer}");
                slots[idx] = Some(PendingToolResult::ready(
                    tc.id.clone(),
                    "ask_user".into(),
                    String::new(),
                    Ok(result),
                ));
                continue;
            }

            // Shell permission gate: when confirm_shell is on (not --yolo),
            // every run_shell command is confirmed with the user first,
            // reusing the ask_user oneshot path. Commands matching the
            // safe_command_re allowlist (cargo, git, ls, etc.) skip the
            // prompt. If the user declines, the command is replaced with a
            // no-op explanation the model can see.
            if self.settings.confirm_shell && tc.function.name == "run_shell" {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !safe_command_re().is_match(&command) {
                    let (confirm_tx, confirm_rx) = tokio::sync::oneshot::channel::<String>();
                    let question = format!("Allow this shell command?\n\n$ {command}\n\n(type 'y' to allow, anything else to deny)");
                    let _ = tx
                        .send(AgentEvent::AskUser {
                            question,
                            reply: confirm_tx,
                        })
                        .await;
                    let answer = confirm_rx.await.unwrap_or_default();
                    let allowed = answer.trim().eq_ignore_ascii_case("y")
                        || answer.trim().eq_ignore_ascii_case("yes");
                    if !allowed {
                        let result = "Shell command NOT run: the user declined permission. Do not retry unless you have a safer alternative.".to_string();
                        slots[idx] = Some(PendingToolResult::ready(
                            tc.id.clone(),
                            "run_shell".into(),
                            String::new(),
                            Ok(result),
                        ));
                        continue;
                    }
                }
            }

            if matches!(
                tc.function.name.as_str(),
                "delegate_task" | "goal_set" | "todo_write"
            ) && !self.settings.allow_delegate
            {
                slots[idx] = Some(PendingToolResult::ready(
                    tc.id.clone(),
                    tc.function.name.clone(),
                    String::new(),
                    Ok(
                        "This sub-agent cannot nest delegate_task or change the parent goal/todos."
                            .into(),
                    ),
                ));
                continue;
            }

            // delegate_task spawns a fresh sub-agent in a new context window
            // and returns its distilled output — async, so special-case it
            // like the web tools.
            if tc.function.name == "delegate_task" {
                let description = args
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let sub_settings = self.settings.clone();
                let result = super::parallel::delegate_task(sub_settings, description).await;
                let result = match result {
                    Ok(out) => {
                        let capped: String = out.chars().take(2000).collect();
                        format!("Sub-agent result:\n{capped}")
                    }
                    Err(e) => format!("Sub-agent error: {e}"),
                };
                slots[idx] = Some(PendingToolResult::ready(
                    tc.id.clone(),
                    "delegate_task".into(),
                    String::new(),
                    Ok(result),
                ));
                continue;
            }

            // Web tools are async HTTP — special-case them like ask_user.
            if tc.function.name == "web_search" || tc.function.name == "web_fetch" {
                let result = if tc.function.name == "web_search" {
                    let query = args
                        .get("query")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let page = args.get("page").and_then(|v| v.as_u64()).map(|p| p as u32);
                    let searxng = crate::web::SearxngConfig {
                        base_url: self.settings.searxng_url.clone(),
                        engines: self.settings.searxng_engines.clone(),
                    };
                    crate::web::search(&query, page, Some(&searxng)).await
                } else {
                    let url = args
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    crate::web::fetch_text(&url).await
                };
                slots[idx] = Some(PendingToolResult::ready(
                    tc.id.clone(),
                    tc.function.name.clone(),
                    String::new(),
                    Ok(result),
                ));
                continue;
            }

            let _ = tx
                .send(AgentEvent::ToolStart {
                    name: tc.function.name.clone(),
                    args: args.clone(),
                })
                .await;
            let sandbox = self.sandbox.clone();
            let name = tc.function.name.clone();
            let id = tc.id.clone();
            let cache_key = format!("{}:{}", name, tc.function.arguments);
            let read_only = self.plan_only;

            // Track verification intent: the model dispatched run_tests or
            // ran a test/typecheck/lint command via run_shell this turn.
            // The actual credit is deferred until the tool result is available
            // so we can check the exit code and output (fail-closed, issue #136).
            let is_verification = name == "run_tests"
                || (name == "run_shell"
                    && args
                        .get("command")
                        .and_then(|v| v.as_str())
                        .map(Sandbox::is_verification_command)
                        .unwrap_or(false));

            // Check cache for read-only tools
            let is_read_only = matches!(
                name.as_str(),
                "list_dir"
                    | "read_file"
                    | "grep"
                    | "search_code"
                    | "git_status"
                    | "git_diff"
                    | "git_log"
                    | "skill_search"
                    | "skill_load"
                    | "memory_search"
            );
            if is_read_only {
                if let Some(cached) = self.tool_cache.get(&cache_key) {
                    slots[idx] = Some(PendingToolResult::ready(
                        id,
                        name,
                        cache_key,
                        Ok(cached.clone()),
                    ));
                    continue;
                }
            }

            if matches!(
                name.as_str(),
                "write_file" | "search_replace" | "apply_patch"
            ) {
                // Serialize file-mutating tools: dispatch and await inline so
                // two edits to the same file apply in call order instead of
                // racing (issue #111). Recording still goes through slots.
                let dispatch_name = name.clone();
                let dispatch_result: Result<String, ToolError> =
                    tokio::task::spawn_blocking(move || {
                        dispatch(&sandbox, &dispatch_name, &args, read_only)
                    })
                    .await
                    .unwrap_or_else(|e| {
                        Err(ToolError::Other(format!("Tool error: join failed: {e}")))
                    });
                if mutating_tool_succeeded(&dispatch_result) {
                    *edited = true;
                    *edited_any = true;
                    self.repo_map_stale = true;
                    self.tool_cache.clear();
                }
                slots[idx] = Some(
                    PendingToolResult::ready(id, name, cache_key, dispatch_result)
                        .with_verification(is_verification),
                );
            } else {
                handles.push((
                    idx,
                    tokio::task::spawn_blocking(move || {
                        let result = dispatch(&sandbox, &name, &args, read_only);
                        (id, name, result, cache_key, is_verification)
                    }),
                ));
            }
        }

        for (idx, h) in handles {
            // Preserve the original tool_call id even if the join fails —
            // empty ids produce provider 400s on the next request.
            let fallback_id = tcs.get(idx).map(|t| t.id.clone()).unwrap_or_default();
            let fallback_name = tcs
                .get(idx)
                .map(|t| t.function.name.clone())
                .unwrap_or_else(|| "unknown".into());
            let (id, name, dispatch_result, cache_key, is_verification) =
                h.await.unwrap_or_else(|e| {
                    (
                        fallback_id,
                        fallback_name,
                        Err(ToolError::Other(format!("Tool error: join failed: {e}"))),
                        String::new(),
                        false,
                    )
                });
            slots[idx] = Some(
                PendingToolResult::ready(id, name, cache_key, dispatch_result)
                    .with_verification(is_verification),
            );
        }

        let mut refresh_state = false;
        for slot in slots.into_iter().flatten() {
            if matches!(slot.name.as_str(), "goal_set" | "todo_write") {
                refresh_state = true;
            }
            self.record_tool_result(
                tx,
                slot.id,
                slot.name,
                slot.cache_key,
                slot.result,
                slot.is_verification,
            )
            .await;
        }
        if refresh_state && !self.messages.is_empty() {
            self.messages[0] = super::core::rebuild_system_message(&self.settings);
        }

        // Plan progress: mark the current step Completed and advance to
        // the next step after tool calls finish.
        if let Some(ref mut plan) = self.plan {
            crate::plan::advance_step(plan, &mut self.current_step, true, false);
            let _ = tx.send(AgentEvent::PlanProgress(plan.clone())).await;
        }

        // Auto-lint reflection: if this iteration edited files, run the
        // project's linter (off the blocking pool) and stash the feedback
        // so the *next* request gets it as an ephemeral reminder — the
        // model can then self-correct before the user sees the damage.
        if *edited && !self.plan_only {
            let sandbox = self.sandbox.clone();
            let lint = tokio::task::spawn_blocking(move || sandbox.run_lint())
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()))
                .unwrap_or_else(|e| format!("Error running linter: {e}"));
            // Only surface as a reflection reminder when lint found problems
            // (non-zero exit or error text), not on a clean pass.
            let has_problems =
                parse_exit_code(&lint).is_some_and(|c| c != 0) || lint.contains("Error");
            if has_problems {
                self.pending_lint = Some(format!(
                    "Lint found problems after your edits — fix them:\n{lint}"
                ));
            }
        }

        Ok(())
    }

    /// Record a single tool-call result: consecutive-failure tracking, read-only
    /// result caching, the `ToolEnd` event, and the `tool` ChatMessage push.
    ///
    /// Called for both serially-dispatched (file-mutating) tools and
    /// parallel-dispatched (read-only) tools so both paths share identical
    /// result bookkeeping.
    ///
    /// When `is_verification` is true, the result is inspected for exit code
    /// and failure markers before crediting `self.verified` (fail-closed,
    /// issue #136).
    pub(crate) async fn record_tool_result(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        id: String,
        name: String,
        cache_key: String,
        dispatch_result: Result<String, ToolError>,
        is_verification: bool,
    ) {
        let result = match dispatch_result {
            Ok(s) => {
                if s.starts_with("Error:") || s.starts_with("Tool error:") {
                    let failure_key = (name.clone(), cache_key.clone());
                    if self.consecutive_failure_key.as_ref() == Some(&failure_key) {
                        self.consecutive_failure_count += 1;
                    } else {
                        self.consecutive_failure_key = Some(failure_key);
                        self.consecutive_failure_count = 1;
                    }
                    if self.consecutive_failure_count >= 3 {
                        self.pending_repeated_failure = Some(
                            "Your last tool call failed with the same error 3+ times in a row. \
                             Do NOT retry the same call. Try a different approach — read the \
                             file first, use a different tool, or explain the problem to the user."
                                .into(),
                        );
                    }
                } else {
                    self.consecutive_failure_key = None;
                    self.consecutive_failure_count = 0;
                }
                s
            }
            Err(e) => {
                // Deterministic failures already reach the model/transcript;
                // keep them at debug under default RUST_LOG=warn.
                if e.is_transient() {
                    tracing::warn!("Transient tool error (retryable): {e}");
                } else {
                    tracing::debug!("Tool error: {e}");
                }
                let retry_hint = if e.is_transient() {
                    " This may be transient; a single retry is reasonable."
                } else {
                    " This is a deterministic error; do not retry the same call — adjust the inputs or use a different approach."
                };
                let msg = format!("Tool error: {e}{retry_hint}");
                let failure_key = (name.clone(), cache_key.clone());
                if self.consecutive_failure_key.as_ref() == Some(&failure_key) {
                    self.consecutive_failure_count += 1;
                } else {
                    self.consecutive_failure_key = Some(failure_key);
                    self.consecutive_failure_count = 1;
                }
                if self.consecutive_failure_count >= 3 {
                    self.pending_repeated_failure = Some(
                        "Your last tool call failed with the same error 3+ times in a row. \
                         Do NOT retry the same call. Try a different approach — read the \
                         file first, use a different tool, or explain the problem to the user."
                            .into(),
                    );
                }
                msg
            }
        };

        if is_verification {
            self.verified = verification_passed(&result);
        }

        // Cache read-only results
        let is_read_only = matches!(
            name.as_str(),
            "list_dir"
                | "read_file"
                | "grep"
                | "search_code"
                | "git_status"
                | "git_diff"
                | "git_log"
                | "skill_search"
                | "skill_load"
                | "memory_search"
        );
        if is_read_only && !cache_key.is_empty() && !is_tool_error_text(&result) {
            self.tool_cache.insert(cache_key, result.clone());
        }
        let preview: String = result.chars().take(600).collect();
        let _ = tx
            .send(AgentEvent::ToolEnd {
                name: name.clone(),
                preview,
            })
            .await;
        self.messages.push(ChatMessage {
            role: "tool".into(),
            content: Some(result),
            tool_calls: None,
            tool_call_id: Some(id),
        });
    }
}

/// Inspect a verification tool result to determine whether it represents a
/// genuinely successful run (fail-closed, issue #136).
///
/// Returns `true` only when the output shows `exit=0` and contains no signal
/// kill, timeout, or test-failure markers. A SIGSYS-killed, timed-out, or
/// non-zero-exit "verification" does NOT count as verified.
fn verification_passed(output: &str) -> bool {
    if output.contains("Error: command killed by signal")
        || output.contains("killed by signal")
        || output.contains("timed out")
    {
        return false;
    }

    if parse_exit_code(output) != Some(0) {
        return false;
    }

    if output.contains("FAILED")
        || output.contains("failures:")
        || output.contains("test result: FAILED")
    {
        return false;
    }

    true
}

/// First `exit=N` token in tool output (`--- run_tests (cargo) exit=10 ---`
/// or a bare `exit=10\n` from `run_shell`).
///
/// Must not treat `exit=10` as `exit=0` via substring match.
fn parse_exit_code(output: &str) -> Option<i32> {
    let bytes = output.as_bytes();
    let mut i = 0;
    while i + 5 <= bytes.len() {
        if &bytes[i..i + 5] == b"exit=" {
            let rest = &output[i + 5..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit() && c != '-')
                .unwrap_or(rest.len());
            if end > 0 {
                if let Ok(n) = rest[..end].parse::<i32>() {
                    return Some(n);
                }
            }
        }
        i += 1;
    }
    None
}

fn is_tool_error_text(s: &str) -> bool {
    s.starts_with("Error:") || s.starts_with("Tool error:")
}

fn mutating_tool_succeeded(result: &Result<String, ToolError>) -> bool {
    match result {
        Ok(s) => !is_tool_error_text(s),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verification_passed_clean_exit() {
        let output = "--- run_tests (cargo) exit=0 ---\ntest result: ok. 10 passed; 0 failed\n";
        assert!(verification_passed(output));
    }

    #[test]
    fn verification_passed_non_zero_exit() {
        let output = "--- run_tests (cargo) exit=1 ---\ncompilation failed\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_killed_by_signal() {
        let output = "Error: command killed by signal 31\nsome output\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_killed_by_signal_in_run_tests() {
        let output = "--- run_tests (cargo) killed by signal 31 ---\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_timed_out() {
        let output = "Error: test runner timed out\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_test_failures() {
        let output = "--- run_tests (cargo) exit=0 ---\ntest result: FAILED. 5 passed; 2 failed\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_no_exit_line() {
        let output = "No test runner detected\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_run_shell_exit_0() {
        let output = "exit=0\nrunning tests...\nok\n";
        assert!(verification_passed(output));
    }

    #[test]
    fn verification_passed_run_shell_exit_nonzero() {
        let output = "exit=2\ntests failed\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_run_shell_killed_by_signal() {
        let output = "Error: command killed by signal 9\n";
        assert!(!verification_passed(output));
    }

    #[test]
    fn verification_passed_exit_10_is_not_exit_0() {
        let output = "--- run_tests (cargo) exit=10 ---\n10 tests failed\n";
        assert!(!verification_passed(output));
        assert_eq!(parse_exit_code(output), Some(10));
    }

    #[test]
    fn parse_exit_code_reads_first_token() {
        assert_eq!(parse_exit_code("exit=0\nok\n"), Some(0));
        assert_eq!(parse_exit_code("exit=-1\n"), Some(-1));
        assert_eq!(parse_exit_code("no status here"), None);
    }

    #[test]
    fn mutating_tool_error_does_not_count_as_edit() {
        assert!(!mutating_tool_succeeded(&Err(ToolError::Other(
            "blocked".into()
        ))));
        assert!(!mutating_tool_succeeded(&Ok(
            "Error: path escapes workspace".into()
        )));
        assert!(mutating_tool_succeeded(&Ok("wrote a.rs".into())));
    }
}
