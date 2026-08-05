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
use crate::context::{compact_if_needed, history_tokens};
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

        for iter in 0..self.settings.max_iterations {
            let _ = tx.send(AgentEvent::Iteration(iter + 1)).await;
            let t_iter = std::time::Instant::now();

            // Reminders for the *next* request only. These are appended to the
            // outgoing request body, NOT to `self.messages`, so the persisted
            // conversation stays a strict `[system, user, assistant, tool, ...]`
            // alternation that compaction and session persistence can rely on.
            let reminders = compute_reminders(&self.messages, iter);

            // Compaction: if estimated history tokens exceed the soft limit,
            // summarize the middle turns and keep a recent tail.
            if let Some((before, after)) = compact_if_needed(
                &mut self.messages,
                self.settings.context_window,
                self.settings.compact_threshold,
            ) {
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
                        crate::web::search(&query).await
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
}
