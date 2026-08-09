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
use crate::tools::{dispatch, safe_command_re, Sandbox};

use super::core::Agent;
use super::types::{AgentEvent, ChatMessage, ToolCall};

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

        // Execute tools in parallel. Each dispatch is sync, so run them
        // on the blocking pool and collect results in call-id order.
        // Results are cached by (name, args) to avoid redundant calls.
        let mut handles = Vec::new();
        for tc in &tcs {
            // If the streamed `arguments` JSON is malformed (e.g. a
            // truncated chunk), surface a clear error to the model instead
            // of silently dispatching with empty args — a write_file or
            // run_shell firing on nothing is far worse than a retry.
            let parsed: Result<Value, serde_json::Error> =
                serde_json::from_str(&tc.function.arguments);
            let args = match parsed {
                Ok(v) => v,
                Err(e) => {
                    let result = format!(
                        "Tool error: arguments for {} are not valid JSON: {}\nRaw: {}",
                        tc.function.name, e, tc.function.arguments
                    );
                    let _ = tx
                        .send(AgentEvent::ToolEnd {
                            name: tc.function.name.clone(),
                            preview: result.chars().take(600).collect(),
                        })
                        .await;
                    self.messages.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(result),
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                    });
                    continue;
                }
            };
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
                let _ = tx
                    .send(AgentEvent::ToolEnd {
                        name: "ask_user".into(),
                        preview: result.chars().take(600).collect(),
                    })
                    .await;
                self.messages.push(ChatMessage {
                    role: "tool".into(),
                    content: Some(result),
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
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
                        let _ = tx
                            .send(AgentEvent::ToolEnd {
                                name: "run_shell".into(),
                                preview: result.chars().take(600).collect(),
                            })
                            .await;
                        self.messages.push(ChatMessage {
                            role: "tool".into(),
                            content: Some(result),
                            tool_calls: None,
                            tool_call_id: Some(tc.id.clone()),
                        });
                        continue;
                    }
                }
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
                    crate::web::search(&query, page).await
                } else {
                    let url = args
                        .get("url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    crate::web::fetch_text(&url).await
                };
                let _ = tx
                    .send(AgentEvent::ToolEnd {
                        name: tc.function.name.clone(),
                        preview: result.chars().take(600).collect(),
                    })
                    .await;
                self.messages.push(ChatMessage {
                    role: "tool".into(),
                    content: Some(result),
                    tool_calls: None,
                    tool_call_id: Some(tc.id.clone()),
                });
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

            // Track file-editing tools so we can auto-lint after this turn.
            if matches!(
                name.as_str(),
                "write_file" | "search_replace" | "apply_patch"
            ) {
                *edited = true;
                *edited_any = true;
                self.repo_map_stale = true;
                self.tool_cache.clear();
            }

            // Track verification: the model dispatched run_tests or ran a
            // test/typecheck/lint command via run_shell this turn.
            if name == "run_tests" {
                self.verified = true;
            } else if name == "run_shell" {
                if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                    if Sandbox::is_verification_command(cmd) {
                        self.verified = true;
                    }
                }
            }

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
                    let preview: String = cached.chars().take(600).collect();
                    let _ = tx
                        .send(AgentEvent::ToolEnd {
                            name: name.clone(),
                            preview,
                        })
                        .await;
                    self.messages.push(ChatMessage {
                        role: "tool".into(),
                        content: Some(cached.clone()),
                        tool_calls: None,
                        tool_call_id: Some(id),
                    });
                    continue;
                }
            }

            if matches!(
                name.as_str(),
                "write_file" | "search_replace" | "apply_patch"
            ) {
                // Serialize file-mutating tools: dispatch, await, and record
                // the result inline so two edits to the same file apply in
                // call order instead of racing (issue #111).
                let dispatch_name = name.clone();
                let dispatch_result: Result<String, ToolError> =
                    tokio::task::spawn_blocking(move || dispatch(&sandbox, &dispatch_name, &args))
                        .await
                        .unwrap_or_else(|e| {
                            Err(ToolError::Other(format!("Tool error: join failed: {e}")))
                        });
                self.record_tool_result(tx, id, name, cache_key, dispatch_result)
                    .await;
            } else {
                handles.push(tokio::task::spawn_blocking(move || {
                    let result = dispatch(&sandbox, &name, &args);
                    (id, name, result, cache_key)
                }));
            }
        }

        for h in handles {
            let (id, name, dispatch_result, cache_key) = h.await.unwrap_or_else(|e| {
                (
                    String::new(),
                    "unknown".into(),
                    Err(ToolError::Other(format!("Tool error: join failed: {e}"))),
                    String::new(),
                )
            });
            self.record_tool_result(tx, id, name, cache_key, dispatch_result)
                .await;
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
                (lint.contains("exit=") && !lint.contains("exit=0")) || lint.contains("Error");
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
    pub(crate) async fn record_tool_result(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        id: String,
        name: String,
        cache_key: String,
        dispatch_result: Result<String, ToolError>,
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
                if e.is_transient() {
                    tracing::warn!("Transient tool error (retryable): {e}");
                } else {
                    tracing::error!("Tool error: {e}");
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
        if is_read_only && !cache_key.is_empty() {
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
