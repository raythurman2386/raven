//! Streaming agent loop + lightweight parallel sub-agents.
//!
//! The [`Agent`] owns the conversation history (`messages`), a [`Sandbox`],
//! and a `reqwest::Client`. [`Agent::run`] appends a user message, then loops:
//!
//! 1. Estimate history tokens and compact if over the soft limit.
//! 2. Clamp `max_tokens` so the request fits the context window.
//! 3. Stream a completion from the OpenAI-compatible endpoint.
//! 4. Accumulate any tool calls from the stream.
//! 5. If no tool calls: append the assistant message and finish.
//! 6. Otherwise: execute all tool calls in parallel (`spawn_blocking`),
//!    append their results, and loop back to step 1.
//!
//! Progress is reported via an `mpsc` channel of [`AgentEvent`]s.
//!
//! # Invariants
//!
//! - `messages[0]` is always the system message; compaction never drops it.
//! - Tool-call / tool-result pairs are kept together during compaction.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use tokio::sync::mpsc;

use crate::config::{load_agents_md, Settings};
use crate::context::{compact_if_needed_llm, history_tokens};
use crate::error::AgentError;
use crate::memory;
use crate::tools::{dispatch, tool_definitions, Sandbox};
use std::collections::HashMap;
use std::sync::OnceLock;

static TOOL_DEFS: OnceLock<serde_json::Value> = OnceLock::new();

fn cached_tool_definitions() -> &'static serde_json::Value {
    TOOL_DEFS.get_or_init(tool_definitions)
}

const SYSTEM_BASE: &str = r#"You are an efficient coding agent. You help with software engineering tasks in the user's workspace.

<tool_calling>
- You have tools for reading files, searching code, editing files, and running shell commands.
- Prefer the dedicated tool over shell equivalents: read_file (not cat), grep (not rg), list_dir (not ls), search_replace (not sed).
- Use run_shell only for commands with no dedicated tool (build, test, git).
- You can call multiple tools in a single response.
- Do NOT call the same tool with the same arguments twice. If you already have the information, use it.
</tool_calling>

<edit_discipline>
- Always read a file before editing it.
- Use search_replace for targeted edits; use write_file for new files or full rewrites.
- Prefer small, focused changes.
</edit_discipline>

<workspace>
- All paths are relative to the workspace root. Do NOT use absolute paths starting with /.
- Stay strictly inside the workspace.
</workspace>

<output>
- When you have enough information, answer the user's question directly with text.
- You do NOT need to call a tool for every response. Sometimes just text is the right answer.
- If you're stuck or a tool returns an error, explain what happened and suggest a fix.
- After reading a file you have its contents for the requested line range. read_file only returns up to 400 lines by default; if the output ends with "... [truncated]", you have NOT seen the whole file — call read_file again with a larger max_lines or a start_line to read the rest before concluding.
- Do not call list_dir again — you already know the structure.
</output>
"#;

/// A single chat message in the OpenAI conversation format.
///
/// `content` is `None` for assistant messages that only carry tool calls.
/// `tool_calls` is `None` unless this is an assistant message requesting tools.
/// `tool_call_id` is `None` unless this is a `tool`-role result message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

/// A tool call requested by the assistant (OpenAI function-calling format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub type_: String,
    pub function: FunctionCall,
}

/// The function name + JSON-string arguments for a [`ToolCall`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// Events emitted by [`Agent::run`] over an `mpsc` channel.
///
/// Consumers (headless runner, TUI) match on these to render progress.
pub enum AgentEvent {
    /// A streamed text delta from the assistant.
    TextDelta(String),
    /// A tool call is about to execute (name + parsed args).
    ToolStart { name: String, args: Value },
    /// A tool call finished (name + first 600 chars of result).
    ToolEnd { name: String, preview: String },
    /// A new agent iteration is starting (1-based).
    Iteration(usize),
    /// Context was compacted; carries before/after token estimates.
    Compacted {
        before_tokens: usize,
        after_tokens: usize,
    },
    /// A transient error is being retried after a delay.
    Retry { attempt: usize, delay_ms: u64 },
    /// The turn edited files but did not call `run_tests` before finishing;
    /// the harness is re-running with a recovery reminder (enforced verify).
    VerifyRequired,
    /// The plan-only turn produced a plan and signalled readiness to execute.
    /// Consumers should auto-proceed to execution (model-driven, no human gate).
    PlanReady,
    /// The model asked the user a question mid-task. The consumer must render
    /// it and send the answer back over the included oneshot channel (or drop
    /// the sender to signal "no answer / dismissed").
    AskUser {
        question: String,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// The agent finished normally (no more tool calls).
    Done,
    /// An error occurred (HTTP failure, stream error, max iterations).
    Error(String),
}

/// A streaming coding agent backed by an OpenAI-compatible endpoint.
///
/// Owns the conversation history, a workspace [`Sandbox`], and an HTTP client.
/// Construct via [`Agent::new`]; drive via [`Agent::run`].
pub struct Agent {
    pub settings: Settings,
    pub sandbox: Sandbox,
    pub messages: Vec<ChatMessage>,
    /// Cache of tool results keyed by `name:args` to avoid redundant calls.
    tool_cache: HashMap<String, String>,
    /// When true, the request advertises only read-only tools so the model
    /// can gather context but physically cannot write files or run shell.
    /// Set for the plan-proposal turn; cleared for execution.
    plan_only: bool,
    client: reqwest::Client,
    /// Holds lint feedback to surface as a reminder on the *next* request,
    /// set after a turn that edited files (write_file/search_replace/apply_patch).
    /// Kept out of `self.messages` so the persisted conversation stays a clean
    /// `[system, user, assistant, tool, ...]` alternation.
    pending_lint: Option<String>,
    /// Holds a verify-required reminder to surface on the *next* request when
    /// the model edited files but did not call `run_tests` before finishing.
    /// Follows the same ephemeral pattern as `pending_lint`.
    pending_verify: Option<String>,
    /// Set when the model dispatched `run_tests` this turn (verification done).
    /// Turn-level, persists across iterations.
    verified: bool,
    /// Number of times the enforced-verify gate has re-run this turn (capped at 3).
    verify_attempts: u32,
}

impl Agent {
    /// Create a new agent, seeding the system message (index 0).
    ///
    /// The system prompt is `SYSTEM_BASE` + workspace root + optional
    /// `AGENTS.md` content + optional `--rules`. The workspace must exist.
    pub fn new(settings: Settings) -> Result<Self> {
        settings.ensure_workspace()?;
        let sandbox = Sandbox::new(settings.workspace.clone());
        let mut messages = Vec::new();
        let mut system = SYSTEM_BASE.to_string();
        system.push_str(&format!(
            "\n\nWorkspace root: {}\n",
            settings.workspace.display()
        ));
        if let Some(map) = crate::repomap::build_map(&settings.workspace) {
            system.push('\n');
            system.push_str(&map);
            system.push('\n');
        }
        let agents = load_agents_md(&settings.workspace);
        if !agents.is_empty() {
            system.push_str("\n--- Project instructions (AGENTS.md) ---\n");
            system.push_str(&agents);
            system.push('\n');
        }
        let mem = memory::load_memory(&settings.workspace);
        if !mem.is_empty() {
            system.push_str("\n--- Project memory ---\n");
            system.push_str(&mem);
            system.push('\n');
        }
        if let Some(rules) = &settings.rules {
            system.push_str("\n--- Session rules ---\n");
            system.push_str(rules);
            system.push('\n');
        }
        messages.push(ChatMessage {
            role: "system".into(),
            content: Some(system),
            tool_calls: None,
            tool_call_id: None,
        });
        Ok(Self {
            settings,
            sandbox,
            messages,
            tool_cache: HashMap::new(),
            plan_only: false,
            pending_lint: None,
            pending_verify: None,
            verified: false,
            verify_attempts: 0,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .context("build HTTP client")?,
        })
    }

    /// Restrict this agent to the read-only toolset for a plan-proposal turn.
    ///
    /// The model can still list/read/search/git-inspect to gather context for
    /// a good plan, but the request advertises no write or shell tools, so it
    /// physically cannot modify the workspace during planning.
    pub fn plan_only(mut self) -> Self {
        self.plan_only = true;
        self
    }

    /// Create an agent with preloaded messages (for session resume).
    ///
    /// Rebuilds the system message from settings, then appends the preloaded
    /// messages. The first message in `preload` should NOT be a system message
    /// (it's rebuilt fresh).
    pub fn with_messages(settings: Settings, preload: Vec<ChatMessage>) -> Result<Self> {
        let mut agent = Agent::new(settings)?;
        // Skip any system messages in preload (index 0 is rebuilt by new())
        for msg in preload {
            if msg.role != "system" {
                agent.messages.push(msg);
            }
        }
        Ok(agent)
    }

    /// The tool definitions to advertise in the next request.
    ///
    /// Returns the full static set (no clone) during execution, or the
    /// read-only subset (a fresh filtered array) during a plan-proposal turn
    /// so the model can gather context but physically cannot write files or
    /// run shell.
    fn tools_value(&self) -> serde_json::Value {
        if self.plan_only {
            crate::tools::plan_tool_definitions()
        } else {
            cached_tool_definitions().clone()
        }
    }

    /// Run one full agent turn (may include multiple tool rounds). Yields events.
    ///
    /// Appends `user_text` as a user message, then loops up to
    /// `settings.max_iterations` times. Emits [`AgentEvent`]s to `tx`.
    /// Returns `Ok(())` on normal completion or error event; the caller
    /// should drain `tx` to observe the outcome.
    pub async fn run(&mut self, user_text: &str, tx: mpsc::Sender<AgentEvent>) -> Result<()> {
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: Some(user_text.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });

        // Turn-level state (persists across iterations within this turn).
        // `verified` is set when the model dispatches `run_tests`; the
        // enforced-verify gate checks it at the finish branch so an edit in
        // iter 1 still gates a finish in iter 2 unless run_tests was called.
        self.verified = false;
        self.verify_attempts = 0;
        let mut edited_any = false;

        for iter in 0..self.settings.max_iterations {
            let _ = tx.send(AgentEvent::Iteration(iter + 1)).await;
            let t_iter = std::time::Instant::now();
            // True if this turn's tools edited files, triggering a lint pass.
            let mut edited = false;

            // Reminders for the *next* request only. These are appended to the
            // outgoing request body, NOT to `self.messages`, so the persisted
            // conversation stays a strict `[system, user, assistant, tool, ...]`
            // alternation that compaction and session persistence can rely on.
            let mut reminders = compute_reminders(&self.messages, iter);
            if let Some(lint) = self.pending_lint.take() {
                reminders.push(lint);
            }
            if let Some(v) = self.pending_verify.take() {
                reminders.push(v);
            }

            // Compaction: if estimated history tokens exceed the soft limit,
            // summarize the middle turns with the model (falling back to the
            // extractive summarizer if the summarization request fails) and
            // keep a recent tail.
            //
            // Capture the fields the summarizer needs as owned values (client
            // is Arc-backed and cheap to clone) so the closure doesn't borrow
            // `self` — which would conflict with the mutable borrow of
            // `self.messages` below.
            let client = self.client.clone();
            let base_url = self.settings.base_url.clone();
            let model = self.settings.model.clone();
            let api_key = self.settings.api_key.clone();
            if let Some((before, after)) = compact_if_needed_llm(
                &mut self.messages,
                self.settings.context_window,
                self.settings.compact_threshold,
                move |middle| {
                    Box::pin(summarize_request(
                        client.clone(),
                        base_url.clone(),
                        model.clone(),
                        api_key.clone(),
                        middle,
                    ))
                },
            )
            .await
            {
                let _ = tx
                    .send(AgentEvent::Compacted {
                        before_tokens: before,
                        after_tokens: after,
                    })
                    .await;
            }

            // Clamp max_tokens so prompt_tokens + max_tokens + margin <= context_window
            let prompt_est = history_tokens(&self.messages);
            let margin = 64usize;
            let remaining = self
                .settings
                .context_window
                .saturating_sub(prompt_est)
                .saturating_sub(margin);
            let clamped_max = self.settings.max_tokens.min(remaining.max(256) as u32);

            // The outgoing `messages` array is the persisted conversation plus
            // any ephemeral system reminders for this iteration. `self.messages`
            // is left untouched so session persistence and compaction see a clean
            // `[system, user, assistant, tool, ...]` alternation. To avoid
            // cloning the whole history on the common (no-reminder) path, we
            // serialize `self.messages` directly and only clone when a reminder
            // must be appended.
            let body = if reminders.is_empty() {
                json!({
                    "model": self.settings.model,
                    "messages": &self.messages,
                    "tools": self.tools_value(),
                    "tool_choice": "auto",
                    "temperature": self.settings.temperature,
                    "max_tokens": clamped_max,
                    "stream": !self.settings.no_stream,
                })
            } else {
                let mut request_messages: Vec<ChatMessage> = self.messages.clone();
                for text in &reminders {
                    request_messages.push(ChatMessage {
                        role: "system".into(),
                        content: Some(text.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                }
                json!({
                    "model": self.settings.model,
                    "messages": request_messages,
                    "tools": self.tools_value(),
                    "tool_choice": "auto",
                    "temperature": self.settings.temperature,
                    "max_tokens": clamped_max,
                    "stream": !self.settings.no_stream,
                })
            };

            let url = format!(
                "{}/chat/completions",
                self.settings.base_url.trim_end_matches('/')
            );

            // Phase timing: pre-request work (tokenization, compaction, body
            // build, serialization) vs HTTP round-trip vs stream processing.
            tracing::info!(
                "iter={} pre_http_ms={} history_msgs={}",
                iter + 1,
                t_iter.elapsed().as_millis(),
                self.messages.len()
            );

            // Send with retry for transient failures
            let t_send = std::time::Instant::now();
            let resp = match self.send_with_retry(&url, &body, &tx).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                    return Ok(());
                }
            };
            tracing::info!(
                "iter={} send_http_ms={} (model={})",
                iter + 1,
                t_send.elapsed().as_millis(),
                self.settings.model
            );

            // Process the response (streaming or non-streaming)
            let t_stream = std::time::Instant::now();
            let (content_buf, tool_acc) = if self.settings.no_stream {
                self.process_non_stream(resp, &tx).await
            } else {
                self.process_stream(resp, &tx).await
            };
            tracing::info!(
                "iter={} stream_ms={} content_chars={} tool_calls={}",
                iter + 1,
                t_stream.elapsed().as_millis(),
                content_buf.chars().count(),
                tool_acc.len()
            );

            // Build assistant message
            let mut assistant = ChatMessage {
                role: "assistant".into(),
                content: if content_buf.is_empty() {
                    None
                } else {
                    Some(content_buf)
                },
                tool_calls: None,
                tool_call_id: None,
            };

            if tool_acc.is_empty() {
                // Enforced verification: if the turn edited files and verify is on and
                // the model hasn't called run_tests, don't finish — inject a recovery
                // reminder and re-run (capped at 3 attempts).
                if self.settings.verify
                    && edited_any
                    && !self.verified
                    && self.verify_attempts < 3
                    && self.sandbox.has_test_runner()
                {
                    self.verify_attempts += 1;
                    self.pending_verify = Some(
                        "You edited files this turn but did not call run_tests to verify \
                         your changes. Call run_tests now and fix any failures before \
                         answering."
                            .to_string(),
                    );
                    let _ = tx.send(AgentEvent::VerifyRequired).await;
                    continue;
                }
                self.messages.push(assistant);
                let _ = tx.send(AgentEvent::Done).await;
                return Ok(());
            }

            // Convert accumulated tool calls
            let mut tcs = Vec::new();
            for (_idx, (id, name, arguments)) in tool_acc {
                tcs.push(ToolCall {
                    id: if id.is_empty() {
                        format!("call_{}", tcs.len())
                    } else {
                        id
                    },
                    type_: "function".into(),
                    function: FunctionCall { name, arguments },
                });
            }

            // If this is a plan-proposal turn and the model signalled it is
            // done planning via the exit_plan_mode tool, don't dispatch it as a
            // real tool — emit PlanReady so the consumer auto-proceeds to
            // execution (Grok Build-style model-driven transition).
            if self.plan_only && tcs.iter().any(|tc| tc.function.name == "exit_plan_mode") {
                self.messages.push(assistant);
                let _ = tx.send(AgentEvent::PlanReady).await;
                return Ok(());
            }

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
                // reusing the ask_user oneshot path. If the user declines, the
                // command is replaced with a no-op explanation the model can see.
                if self.settings.confirm_shell && tc.function.name == "run_shell" {
                    let command = args
                        .get("command")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
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
                    edited = true;
                    edited_any = true;
                }

                // Track verification: the model dispatched run_tests this turn.
                if name == "run_tests" {
                    self.verified = true;
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

                handles.push(tokio::task::spawn_blocking(move || {
                    let result = dispatch(&sandbox, &name, &args);
                    (id, name, result, cache_key)
                }));
            }

            for h in handles {
                let (id, name, result, cache_key) = h.await.unwrap_or_else(|e| {
                    (
                        String::new(),
                        "unknown".into(),
                        format!("Tool error: join failed: {}", e),
                        String::new(),
                    )
                });
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

            // Auto-lint reflection: if this turn edited files, run the
            // project's linter (off the blocking pool) and stash the feedback
            // so the *next* request gets it as an ephemeral reminder — the
            // model can then self-correct before the user sees the damage.
            if edited && !self.plan_only {
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
            // Loop continues so the model can react to tool results
        }

        let _ = tx
            .send(AgentEvent::Error(
                AgentError::MaxIterations(self.settings.max_iterations).to_string(),
            ))
            .await;
        Ok(())
    }

    // ── HTTP helpers ───────────────────────────────────────────────────

    /// Build and send the request with auth headers, applying retry logic
    /// for transient failures (connection errors, 5xx, 429).
    async fn send_with_retry(
        &self,
        url: &str,
        body: &Value,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<reqwest::Response, AgentError> {
        let max_retries = 3;
        let mut delay = std::time::Duration::from_secs(1);

        for attempt in 0..max_retries {
            let mut req = self
                .client
                .post(url)
                .header("Content-Type", "application/json");
            if let Some(key) = &self.settings.api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }

            match req.json(body).send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),

                Ok(resp) => {
                    let status = resp.status().as_u16();
                    // 404 = model not found — don't retry
                    if status == 404 {
                        let text = resp.text().await.unwrap_or_default();
                        // Check if this looks like a model-not-found error
                        if text.contains("model") && text.to_lowercase().contains("not found") {
                            return Err(AgentError::ModelNotFound {
                                model: self.settings.model.clone(),
                            });
                        }
                        return Err(AgentError::HttpError { status, body: text });
                    }
                    // 5xx and 429 = transient — retry
                    if ((500..600).contains(&status) || status == 429) && attempt + 1 < max_retries
                    {
                        let _ = tx
                            .send(AgentEvent::Retry {
                                attempt: attempt + 1,
                                delay_ms: delay.as_millis() as u64,
                            })
                            .await;
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    // Other 4xx = don't retry
                    let text = resp.text().await.unwrap_or_default();
                    return Err(AgentError::HttpError { status, body: text });
                }
                Err(e) if e.is_connect() || e.is_timeout() => {
                    if attempt + 1 < max_retries {
                        let _ = tx
                            .send(AgentEvent::Retry {
                                attempt: attempt + 1,
                                delay_ms: delay.as_millis() as u64,
                            })
                            .await;
                        tokio::time::sleep(delay).await;
                        delay *= 2;
                        continue;
                    }
                    return Err(AgentError::OllamaUnreachable {
                        url: url.to_string(),
                        source: e,
                    });
                }
                Err(e) => {
                    return Err(AgentError::OllamaUnreachable {
                        url: url.to_string(),
                        source: e,
                    });
                }
            }
        }
        // All retries exhausted without a definitive success or error.
        Err(AgentError::HttpError {
            status: 503,
            body: "retries exhausted — all attempts failed with transient errors".into(),
        })
    }

    // ── Response processing ─────────────────────────────────────────────

    /// Process a streaming SSE response, accumulating content and tool calls.
    ///
    /// Returns (accumulated_content, accumulated_tool_calls).
    async fn process_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> (String, BTreeMap<u32, (String, String, String)>) {
        let mut stream = resp.bytes_stream();
        let mut content_buf = String::new();
        let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();

        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(c) => c,
                Err(e) => {
                    let _ = tx
                        .send(AgentEvent::Error(format!("Stream error: {}", e)))
                        .await;
                    break;
                }
            };
            let text = String::from_utf8_lossy(&chunk);
            for line in text.lines() {
                let line = line.trim();
                if !line.starts_with("data: ") {
                    continue;
                }
                let data = &line[6..];
                if data == "[DONE]" {
                    continue;
                }
                let Ok(v) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
                    continue;
                };
                let Some(choice) = choices.first() else {
                    continue;
                };
                let delta = choice.get("delta").cloned().unwrap_or(json!({}));

                if let Some(c) = delta.get("content").and_then(|c| c.as_str()) {
                    content_buf.push_str(c);
                    let _ = tx.send(AgentEvent::TextDelta(c.to_string())).await;
                }

                if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                    for tc in tcs {
                        let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as u32;
                        let entry = tool_acc
                            .entry(idx)
                            .or_insert_with(|| (String::new(), String::new(), String::new()));
                        if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
                            if !id.is_empty() {
                                entry.0 = id.to_string();
                            }
                        }
                        if let Some(func) = tc.get("function") {
                            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                                if !name.is_empty() {
                                    entry.1 = name.to_string();
                                }
                            }
                            if let Some(args) = func.get("arguments") {
                                entry.2.push_str(&args_to_string(args));
                            }
                        }
                    }
                }
            }
        }

        (content_buf, tool_acc)
    }

    /// Process a non-streaming JSON response (fallback for weak SSE hosts).
    ///
    /// Returns (accumulated_content, accumulated_tool_calls).
    async fn process_non_stream(
        &self,
        resp: reqwest::Response,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> (String, BTreeMap<u32, (String, String, String)>) {
        let mut content_buf = String::new();
        let mut tool_acc: BTreeMap<u32, (String, String, String)> = BTreeMap::new();

        let Ok(v) = resp.json::<Value>().await else {
            return (content_buf, tool_acc);
        };

        let Some(choices) = v.get("choices").and_then(|c| c.as_array()) else {
            return (content_buf, tool_acc);
        };
        let Some(choice) = choices.first() else {
            return (content_buf, tool_acc);
        };

        // Non-streaming uses "message" instead of "delta"
        let msg = choice.get("message").cloned().unwrap_or(json!({}));

        if let Some(c) = msg.get("content").and_then(|c| c.as_str()) {
            content_buf.push_str(c);
            let _ = tx.send(AgentEvent::TextDelta(c.to_string())).await;
        }

        if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
            for (i, tc) in tcs.iter().enumerate() {
                let idx = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(i as u64) as u32;
                let id = tc
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let func = tc.get("function").cloned().unwrap_or(json!({}));
                let name = func
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let args = func
                    .get("arguments")
                    .map(args_to_string)
                    .unwrap_or_default();
                tool_acc.insert(idx, (id, name, args));
            }
        }

        (content_buf, tool_acc)
    }
}

/// Summarize a slice of conversation history into a compact paragraph.
///
/// Used by LLM-structured compaction. Makes a single non-streaming chat
/// request asking the model to distill the middle turns. Returns `None` if
/// the request fails, so the caller falls back to the extractive summarizer.
async fn summarize_request(
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    middle: Vec<ChatMessage>,
) -> Option<String> {
    let prompt = format!(
        "Distill the following conversation segment into a compact summary \
         (max ~150 words) preserving the key user requests, decisions, and \
         actions taken. This will replace the original messages in a \
         long-running agent session.\n\n{}\n\nSummary:",
        middle
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "messages": [
            {"role": "system", "content": "You are a context-compaction assistant for a coding agent. Be concise and factual."},
            {"role": "user", "content": prompt}
        ],
        "max_tokens": 512,
        "stream": false
    });

    let mut req = client.post(&url).json(&body);
    if let Some(key) = &api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let json: serde_json::Value = resp.json().await.ok()?;
    let text = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(str::to_string)?;
    let trimmed = text.trim().to_string();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Run several focused sub-agents in parallel and return their final text.
///
/// Each task gets a fresh [`Agent`] with a clean conversation. Results are
/// returned in the same order as the input tasks. Tool events are consumed
/// silently; only the accumulated text deltas are returned.
pub async fn run_parallel(settings: &Settings, tasks: Vec<String>) -> Result<Vec<String>> {
    let mut handles = Vec::new();
    for (i, task) in tasks.into_iter().enumerate() {
        let s = settings.clone();
        // Each sub-agent gets a clean conversation
        let handle = tokio::spawn(async move {
            let mut agent = Agent::new(s)?;
            let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
            let runner = tokio::spawn(async move {
                if let Err(e) = agent.run(&task, tx).await {
                    tracing::warn!("sub-agent {} failed: {}", i, e);
                }
            });
            let mut out = String::new();
            while let Some(ev) = rx.recv().await {
                match ev {
                    AgentEvent::TextDelta(t) => out.push_str(&t),
                    AgentEvent::Done | AgentEvent::Error(_) => break,
                    _ => {}
                }
            }
            let _ = runner.await;
            Ok::<_, anyhow::Error>(out)
        });
        handles.push((i, handle));
    }

    let mut results = vec![String::new(); handles.len()];
    for (i, h) in handles {
        results[i] = h.await??;
    }
    Ok(results)
}

/// Compute ephemeral system reminders to inject into the *next* request.
///
/// These are deliberately kept out of the persisted conversation (`messages`)
/// so the history stays a strict `[system, user, assistant, tool, ...]`
/// alternation. Returns a list of reminder texts (empty on the common path).
///
/// - After 3+ consecutive tool-only assistant turns (`iter >= 3`), push a
///   "stop calling tools" reminder to break a tool-calling loop.
/// - At `iter == 5`, push a "reflect on progress" nudge.
fn compute_reminders(messages: &[ChatMessage], iter: usize) -> Vec<String> {
    let mut reminders = Vec::new();

    if iter >= 3 {
        let tool_only_count = messages
            .iter()
            .rev()
            .filter(|m| m.role == "assistant")
            .take(3)
            .filter(|m| m.content.is_none() && m.tool_calls.is_some())
            .count();
        if tool_only_count >= 3 {
            reminders.push(
                "You have been calling tools repeatedly without producing text. \
                 Stop calling tools now. Answer the user's question directly with text. \
                 If you need information you already have it. Just respond."
                    .into(),
            );
        }
    }

    if iter == 5 {
        reminders.push(
            "You've made several iterations. Reflect: are you making progress? \
             If you're stuck in a loop, try a different approach."
                .into(),
        );
    }

    reminders
}

/// Convert a tool-call `arguments` JSON value into the string form the
/// dispatch layer expects.
///
/// OpenAI-compatible endpoints vary: most stream `arguments` as a JSON *string*
/// fragment (accumulated across chunks), but some return a fully-formed JSON
/// *object*. Normalize both to a string so tool arguments are never silently
/// dropped (which would produce a malformed call with empty args). A JSON
/// `null`/missing value becomes an empty string.
fn args_to_string(args: &Value) -> String {
    match args {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => serde_json::to_string(other).unwrap_or_else(|_| String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener as StdTcpListener;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Read the raw HTTP request headers (everything up to and including the
    /// `\r\n\r\n` header terminator), waiting until the full body (per
    /// Content-Length) has arrived. Returns the header block as a String
    /// (headers only, NOT including body).
    #[allow(dead_code)]
    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await.expect("read from stream");
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            // Check for header terminator.
            if let Some(header_end) = find_subslice(b"\r\n\r\n", &buf) {
                let headers = String::from_utf8_lossy(&buf[..header_end + 4]).to_string();
                // Wait for full body per Content-Length so the client can send it.
                let content_length = extract_content_length(&headers).unwrap_or(0);
                let body_start = header_end + 4;
                while buf.len() < body_start + content_length {
                    let n = stream.read(&mut tmp).await.expect("read body from stream");
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&tmp[..n]);
                }
                return headers;
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    fn find_subslice(needle: &[u8], haystack: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn extract_content_length(headers: &str) -> Option<usize> {
        for line in headers.split("\r\n") {
            if let Some((name, value)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    return value.trim().parse::<usize>().ok();
                }
            }
        }
        None
    }

    /// Serve the given responses (status, reason, body) over the listener,
    /// keep-alive aware.
    ///
    /// The agent uses a shared `reqwest::Client` that reuses one TCP
    /// connection across requests. A naive one-connection-per-response mock
    /// races: after the mock writes a response and drops the stream, the
    /// client may already have sent its next request on that same connection,
    /// where it sits unread while the mock blocks on `accept()` for a new
    /// connection that never comes — hanging the agent until it times out and
    /// ends the run with `Error` instead of `Done`. To avoid that, read
    /// multiple requests per connection, serving the next scripted response
    /// each time, until the connection closes. Once the scripted responses are
    /// exhausted, serve a benign empty fallback so an extra request (a verify
    /// retry, a timing shift) never hits a connection-refused → retry →
    /// `OllamaUnreachable` path.
    async fn serve_mock(
        listener: &tokio::net::TcpListener,
        responses: Vec<(u16, &'static str, &'static str)>,
    ) {
        let mut next = 0usize;
        loop {
            let (mut stream, _) = listener.accept().await.expect("accept");
            loop {
                // Read one request. If the connection closed (client done),
                // break to accept the next connection.
                if read_http_request(&mut stream).await.is_empty() {
                    break;
                }
                let (status, reason, body) = match responses.get(next) {
                    Some(&(s, r, b)) => {
                        next += 1;
                        (s, r, b)
                    }
                    None => (200, "OK", ""),
                };
                let response = format!(
                    "HTTP/1.1 {} {}\r\n\
                     Content-Type: text/event-stream\r\n\
                     Content-Length: {}\r\n\
                     \r\n\
                     {}",
                    status,
                    reason,
                    body.len(),
                    body,
                );
                if stream.write_all(response.as_bytes()).await.is_err() {
                    break;
                }
                if stream.flush().await.is_err() {
                    break;
                }
            }
        }
    }

    /// Spawn a mock HTTP server that serves the given SSE bodies in order.
    /// Returns `(base_url_string, handle)`.
    #[allow(dead_code)]
    async fn spawn_mock(responses: Vec<&'static str>) -> (String, tokio::task::JoinHandle<()>) {
        let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = std_listener.local_addr().expect("local addr");
        std_listener.set_nonblocking(true).expect("set_nonblocking");
        let listener =
            tokio::net::TcpListener::from_std(std_listener).expect("convert to tokio listener");

        let scripted: Vec<(u16, &'static str, &'static str)> =
            responses.into_iter().map(|b| (200, "OK", b)).collect();
        let handle = tokio::spawn(async move {
            serve_mock(&listener, scripted).await;
        });

        (format!("http://{addr}"), handle)
    }

    /// Same as `spawn_mock` but each entry carries a status code. For status
    /// 503 use reason `Service Unavailable`, else `OK`. Body string is still
    /// served after headers.
    #[allow(dead_code)]
    async fn spawn_mock_status(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let std_listener = StdTcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = std_listener.local_addr().expect("local addr");
        std_listener.set_nonblocking(true).expect("set_nonblocking");
        let listener =
            tokio::net::TcpListener::from_std(std_listener).expect("convert to tokio listener");

        let scripted: Vec<(u16, &'static str, &'static str)> = responses
            .into_iter()
            .map(|(status, body)| {
                let reason = match status {
                    503 => "Service Unavailable",
                    429 => "Too Many Requests",
                    _ => "OK",
                };
                (status, reason, body)
            })
            .collect();
        let handle = tokio::spawn(async move {
            serve_mock(&listener, scripted).await;
        });

        (format!("http://{addr}"), handle)
    }

    /// Returns a `Settings` configured for tests against a mock server.
    #[allow(dead_code)]
    fn settings_for(workspace: &std::path::Path, base_url: &str) -> Settings {
        Settings {
            model: "mock-model".into(),
            base_url: base_url.into(),
            api_key: None,
            workspace: workspace.to_path_buf(),
            max_iterations: 5,
            plan_first: false,
            yolo: true,
            temperature: 0.0,
            max_tokens: 4096,
            rules: None,
            context_window: 128_000,
            compact_threshold: 0.75,
            no_stream: false,
            verify: false,
            confirm_shell: false,
        }
    }

    fn plain(role: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Some("x".into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn tool_only() -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                type_: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"a.rs"}"#.into(),
                },
            }]),
            tool_call_id: None,
        }
    }

    #[test]
    fn no_reminders_early_and_clean() {
        let msgs = vec![plain("system"), plain("user"), plain("assistant")];
        assert!(compute_reminders(&msgs, 0).is_empty());
        assert!(compute_reminders(&msgs, 2).is_empty());
    }

    #[test]
    fn loop_breaker_fires_after_3_tool_only_turns() {
        let mut msgs = vec![plain("system")];
        for _ in 0..3 {
            msgs.push(tool_only());
        }
        let r = compute_reminders(&msgs, 3);
        assert!(
            r.iter().any(|t| t.contains("Stop calling tools")),
            "loop breaker should fire, got {r:?}"
        );
    }

    #[test]
    fn loop_breaker_ignores_recent_text_output() {
        // Only 2 tool-only turns; the third has content, so no loop breaker.
        let msgs = vec![
            plain("system"),
            tool_only(),
            tool_only(),
            plain("assistant"),
        ];
        let r = compute_reminders(&msgs, 3);
        assert!(
            !r.iter().any(|t| t.contains("Stop calling tools")),
            "should not fire with text output, got {r:?}"
        );
    }

    #[test]
    fn iteration_5_adds_reflect_nudge() {
        let msgs = vec![plain("system")];
        let r = compute_reminders(&msgs, 5);
        assert!(
            r.iter().any(|t| t.contains("Reflect")),
            "iteration-5 nudge should fire, got {r:?}"
        );
    }

    #[test]
    fn iteration_5_does_not_fire_elsewhere() {
        let msgs = vec![plain("system")];
        assert!(!compute_reminders(&msgs, 4)
            .iter()
            .any(|t| t.contains("Reflect")));
        assert!(!compute_reminders(&msgs, 6)
            .iter()
            .any(|t| t.contains("Reflect")));
    }

    #[test]
    fn args_string_passes_through() {
        assert_eq!(
            args_to_string(&json!("{\"path\":\"a.rs\"}")),
            "{\"path\":\"a.rs\"}"
        );
    }

    #[test]
    fn args_object_serialized() {
        let s = args_to_string(&json!({"path": "a.rs", "n": 3}));
        assert!(
            s.contains("\"path\":\"a.rs\"") || s.contains("\"path\": \"a.rs\""),
            "args object should serialize path: {s}"
        );
        assert!(s.contains("\"n\":3"), "args object should serialize n: {s}");
    }

    #[test]
    fn args_null_is_empty() {
        assert_eq!(args_to_string(&Value::Null), "");
    }

    #[test]
    fn args_number_serialized() {
        assert_eq!(args_to_string(&json!(42)), "42");
    }

    #[tokio::test]
    async fn stream_text_only_reaches_done() {
        let tmp = tempfile::tempdir().unwrap();
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![body]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("hello", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Hel")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "lo")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
        // assistant message appended with the full concatenated content
        let last = agent.messages.last().unwrap();
        assert_eq!(last.role, "assistant");
        assert_eq!(last.content.as_deref(), Some("Hello"));
    }

    #[tokio::test]
    async fn stream_tool_call_then_answer() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
        // Round 1: SSE with a read_file tool call (no content delta).
        let tool_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        // Round 2: SSE with the final text answer.
        let final_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Done reading.\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![tool_round, final_round]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("read a.rs", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read_file")));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolEnd { name, preview } if name == "read_file" && preview.contains("fn main"))));
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "Done reading.")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
        // The tool result was injected into conversation history.
        assert!(agent
            .messages
            .iter()
            .any(|m| m.role == "tool" && m.content.as_deref().unwrap_or("").contains("fn main")));
    }

    #[tokio::test]
    async fn retries_on_5xx_then_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let ok_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock_status(vec![(503, "oops"), (200, ok_body)]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("ping", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Retry { attempt: 1, .. })));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn non_stream_text_message() {
        let tmp = tempfile::tempdir().unwrap();
        let body =
            r#"{"choices":[{"message":{"role":"assistant","content":"plain json answer"}}]}"#;
        let (base, _h) = spawn_mock(vec![body]).await;
        let mut s = settings_for(tmp.path(), &base);
        s.no_stream = true;
        let mut agent = Agent::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("go", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "plain json answer")));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn model_not_found_no_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let err_body = r#"{"error":"model 'nope' not found"}"#;
        let (base, _h) = spawn_mock_status(vec![(404, err_body)]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("go", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Error(msg) if msg.contains("not found"))));
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::Retry { .. })));
    }

    #[tokio::test]
    async fn verify_requires_run_tests() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
        let text_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![
            edit_round, text_round, text_round, text_round, text_round,
        ])
        .await;
        let mut s = settings_for(tmp.path(), &base);
        s.verify = true;
        let mut agent = Agent::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("edit a.rs", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::VerifyRequired)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn verify_passes_when_run_tests_called() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
        let test_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_2\",\"type\":\"function\",\"function\":{\"name\":\"run_tests\",\"arguments\":\"{}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
        let text_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"all good\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![edit_round, test_round, text_round]).await;
        let mut s = settings_for(tmp.path(), &base);
        s.verify = true;
        let mut agent = Agent::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("edit and verify", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::VerifyRequired)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn verify_off_does_not_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
        let text_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![edit_round, text_round]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("edit a.rs", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::VerifyRequired)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn verify_caps_at_max_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
        let text_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![
            edit_round, text_round, text_round, text_round, text_round,
        ])
        .await;
        let mut s = settings_for(tmp.path(), &base);
        s.verify = true;
        let mut agent = Agent::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("edit a.rs", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        let verify_count = events
            .iter()
            .filter(|e| matches!(e, AgentEvent::VerifyRequired))
            .count();
        assert_eq!(verify_count, 3);
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn verify_skips_when_no_test_runner() {
        let tmp = tempfile::tempdir().unwrap();
        let edit_round = concat!(
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"write_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\",\\\"content\\\":\\\"fn main() {}\\\"}\"}}]}}]}\n\n",
        "data: [DONE]\n\n",
    );
        let text_round = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        // No Cargo.toml in the tempdir → no test runner → gate skipped.
        let (base, _h) = spawn_mock(vec![edit_round, text_round]).await;
        let mut s = settings_for(tmp.path(), &base);
        s.verify = true;
        let mut agent = Agent::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("edit a.rs", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(!events
            .iter()
            .any(|e| matches!(e, AgentEvent::VerifyRequired)));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn retries_on_429_then_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let ok_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock_status(vec![(429, "rate limited"), (200, ok_body)]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("ping", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Retry { attempt: 1, .. })));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn compaction_triggers_when_context_exceeds_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let summarizer_body =
            r#"{"choices":[{"message":{"role":"assistant","content":"summary"}}]}"#;
        let agent_body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![summarizer_body, agent_body]).await;
        let mut s = settings_for(tmp.path(), &base);
        s.context_window = 500;
        s.compact_threshold = 0.5;
        let mut agent = Agent::new(s).unwrap();
        let (tx, mut rx) = mpsc::channel(256);
        agent.run("hello", tx).await.unwrap();
        let mut events = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events.push(ev);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, AgentEvent::Compacted { .. })));
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Done)));
    }

    #[tokio::test]
    async fn multi_turn_conversation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}\n").unwrap();
        let turn1 = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let turn2 = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"File looks good.\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let turn3 = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"All done.\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let (base, _h) = spawn_mock(vec![turn1, turn2, turn3]).await;
        let mut agent = Agent::new(settings_for(tmp.path(), &base)).unwrap();

        let (tx, mut rx) = mpsc::channel(256);
        agent.run("read a.rs", tx).await.unwrap();
        let mut events1 = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events1.push(ev);
        }
        assert!(events1
            .iter()
            .any(|e| matches!(e, AgentEvent::ToolStart { name, .. } if name == "read_file")));
        assert!(events1.iter().any(|e| matches!(e, AgentEvent::Done)));

        let (tx, mut rx) = mpsc::channel(256);
        agent.run("what do you think?", tx).await.unwrap();
        let mut events2 = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            events2.push(ev);
        }
        assert!(events2
            .iter()
            .any(|e| matches!(e, AgentEvent::TextDelta(s) if s == "All done.")));
        assert!(events2.iter().any(|e| matches!(e, AgentEvent::Done)));

        let user_msgs: Vec<_> = agent.messages.iter().filter(|m| m.role == "user").collect();
        assert_eq!(user_msgs.len(), 2);
    }
}
