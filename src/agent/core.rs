//! The [`Agent`] struct, its constructors, and the `run()` orchestration loop.
//!
//! The heavy lifting is split across sibling modules:
//! - [`super::stream`] — streaming/non-streaming response processing.
//! - [`super::tools_exec`] — tool dispatch and result bookkeeping.
//! - [`super::loop_control`] — stall/verify recovery, reminders, max-iter wrap-up.
//! - [`super::parallel`] — parallel sub-agents.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::OnceLock;
use tokio::sync::mpsc;

use crate::config::{load_agents_md, Mode, Settings};
use crate::context::{compact_if_needed_llm, history_tokens};
use crate::error::{cap_http_body, AgentError};
use crate::memory;
use crate::plan::Plan;
use crate::tools::{tool_definitions, Sandbox};

use super::loop_control::{compute_reminders, summarize_request};
use super::stream::ParsedCompletion;
#[cfg(test)]
use super::stream::{process_non_stream_json, process_stream_text};
use super::types::{AgentEvent, ChatMessage, FunctionCall, ToolCall};

enum IterationOutcome {
    Continue,
    Finished,
}

static TOOL_DEFS: OnceLock<serde_json::Value> = OnceLock::new();
static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

/// Shared HTTP client so each TUI send does not re-init TLS.
fn shared_http_client() -> Result<reqwest::Client> {
    if let Some(client) = HTTP_CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("build HTTP client")?;
    Ok(HTTP_CLIENT.get_or_init(|| client.clone()).clone())
}

/// Test-only completion source: a closure that returns the raw completion
/// body (SSE text, or JSON when `no_stream`) for a given outgoing request
/// body. Used to drive the agent loop offline without HTTP.
#[cfg(test)]
pub(crate) type CompletionSource = Box<dyn FnMut(&Value) -> String + Send + Sync + 'static>;

fn cached_tool_definitions() -> &'static serde_json::Value {
    TOOL_DEFS.get_or_init(tool_definitions)
}

/// Clamp `max_tokens` so `prompt_tokens + max_tokens + margin <= context_window`.
///
/// Pure helper (no I/O) so the clamp math is unit-testable offline. A floor of
/// 256 tokens is always reserved so the model can still produce a reply even
/// when the context is nearly full.
pub(crate) fn clamp_max_tokens(
    max_tokens: u32,
    prompt_tokens: usize,
    context_window: usize,
    margin: usize,
) -> u32 {
    let remaining = context_window
        .saturating_sub(prompt_tokens)
        .saturating_sub(margin);
    max_tokens.min(remaining.max(256) as u32)
}

const SYSTEM_BASE: &str = r#"You are an efficient coding agent. You help with software engineering tasks in the user's workspace.

<tool_calling>
- You have tools for reading files, searching code, editing files, and running shell commands.
- Prefer the dedicated tool over shell equivalents: read_file (not cat), grep (not rg), list_dir (not ls), search_replace (not sed).
- Use run_shell only for commands with no dedicated tool (build, test, git).
- You can call multiple tools in a single response.
- Do NOT call the same tool with the same arguments twice. If you already have the information, use it.
- Use think to reason through long chains of tool calls or costly sequential decisions before acting.
- Use goal_set at the start of a multi-step task and todo_write to track 3+ steps; keep them updated as you work.
- Use delegate_task to offload a self-contained sub-task to a fresh context window; it returns a summary you can rely on.
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

<accuracy>
- Never invent or guess file contents, function signatures, test results, or command output. If you have not read it or run it, you do not know it.
- Base every claim about the code on what a tool actually returned. If a tool result is missing, truncated, or errored, say so instead of fabricating.
- Do not claim a change is done or verified unless you actually ran the test/lint command and saw a passing result.
- If you are unsure whether a file exists or what it contains, read it or list the directory rather than assuming.
- When you finish, only report what you genuinely did and observed.
</accuracy>

<output>
- When you have enough information, answer the user's question directly with text.
- You do NOT need to call a tool for every response. Sometimes just text is the right answer.
- If you're stuck or a tool returns an error, explain what happened and suggest a fix.
- After reading a file you have its contents for the requested line range. read_file only returns up to 400 lines by default; if the output ends with "... [truncated]", you have NOT seen the whole file — call read_file again with a larger max_lines or a start_line to read the rest before concluding.
- Do not call list_dir again — you already know the structure.
</output>
"#;

/// Rebuild the system message (goal/todos, repo map, memory) from disk.
pub(crate) fn rebuild_system_message(settings: &Settings) -> ChatMessage {
    build_system_message(settings)
}

/// Build the system message from settings, including the repo map if applicable.
fn build_system_message(settings: &Settings) -> ChatMessage {
    let mut system = SYSTEM_BASE.to_string();

    // Mode awareness: tell the model what it can and cannot do in this mode.
    let mode_desc = match settings.mode {
        Mode::Plan => {
            "You are in PLAN mode. You can read files and inspect the workspace \
            but CANNOT edit files or run shell commands. Propose a concise step-by-step \
            plan first; the user will approve it before you can execute."
        }
        Mode::Agent => {
            "You are in AGENT mode. You have full access to read/write files, \
            run shell commands, and commit changes."
        }
        Mode::Chat => {
            "You are in CHAT mode. You can read files and inspect the workspace \
            but CANNOT edit files or run shell commands. Answer questions, explore the \
            codebase, and ask clarifying questions with the ask_user tool. If the user \
            wants changes made, suggest they switch to agent mode."
        }
    };
    system.push_str("\n--- Mode ---\n");
    system.push_str(mode_desc);
    system.push('\n');

    system.push_str(&format!(
        "\n\nWorkspace root: {}\n",
        settings.workspace.display()
    ));

    // Workspace state: give the model a ground-truth anchor so it can
    // verify its mental model of what has changed against reality.
    let sandbox = Sandbox::new(settings.workspace.clone());
    if sandbox.is_git_repo().unwrap_or(false) {
        match sandbox.git_status() {
            Ok(status) if status.contains("No changes") => {
                system.push_str("Working tree: clean\n");
            }
            Ok(status) => {
                let lines: Vec<&str> = status.lines().take(10).collect();
                system.push_str(&format!(
                    "Working tree: dirty ({} changed)\n{}\n",
                    status.lines().count(),
                    lines.join("\n")
                ));
            }
            Err(_) => {}
        }
    }

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
    if let Some(goal) = crate::state::load_goal(&settings.workspace) {
        system.push_str("\n--- Current goal ---\n");
        system.push_str(&crate::state::format_goal(&goal));
        system.push('\n');
    }
    let todos = crate::state::load_todos(&settings.workspace);
    if !todos.is_empty() {
        system.push_str("\n--- Task list ---\n");
        system.push_str(&crate::state::format_todos(&todos));
        system.push('\n');
    }
    if let Some(rules) = &settings.rules {
        system.push_str("\n--- Session rules ---\n");
        system.push_str(rules);
        system.push('\n');
    }
    ChatMessage {
        role: "system".into(),
        content: Some(system),
        tool_calls: None,
        tool_call_id: None,
    }
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
    pub(crate) tool_cache: HashMap<String, String>,
    /// When true, the request advertises only read-only tools so the model
    /// can gather context but physically cannot write files or run shell.
    /// Set for the plan-proposal turn; cleared for execution.
    pub(crate) plan_only: bool,
    client: reqwest::Client,
    /// Holds lint feedback to surface as a reminder on the *next* request,
    /// set after a turn that edited files (write_file/search_replace/apply_patch).
    /// Kept out of `self.messages` so the persisted conversation stays a clean
    /// `[system, user, assistant, tool, ...]` alternation.
    pub(crate) pending_lint: Option<String>,
    /// Holds a verify-required reminder to surface on the *next* request when
    /// the model edited files but did not call `run_tests` before finishing.
    /// Follows the same ephemeral pattern as `pending_lint`.
    pub(crate) pending_verify: Option<String>,
    /// Set when the model dispatched `run_tests` this turn (verification done).
    /// Turn-level, persists across iterations.
    pub(crate) verified: bool,
    /// Number of times the enforced-verify gate has re-run this turn (capped at 3).
    pub(crate) verify_attempts: u32,
    /// Set to true when a file-editing tool (write_file/search_replace/apply_patch)
    /// runs, signalling that the repo map in the system message may be stale.
    pub(crate) repo_map_stale: bool,
    /// Tracks consecutive identical failing tool calls to detect degenerate
    /// loops where the model retries the same failing call without adapting.
    pub(crate) consecutive_failure_key: Option<(String, String)>,
    pub(crate) consecutive_failure_count: usize,
    /// Holds a repeated-failure reminder to surface on the *next* request when
    /// the model has made 3+ identical failing tool calls in a row.
    pub(crate) pending_repeated_failure: Option<String>,
    /// Number of consecutive blank model turns (no content, no tool calls)
    /// handled this run. Capped: after this many, the turn falls through to
    /// `emit_summary` so it always ends visibly.
    pub(crate) blank_attempts: u32,
    /// Holds a blank-response reminder to surface on the *next* request when
    /// the model returned nothing (no content, no tool calls). Follows the
    /// same ephemeral pattern as `pending_lint`.
    pub(crate) pending_blank: Option<String>,
    /// Optional plan being executed; step statuses are updated as the agent
    /// progresses through tool calls.
    pub(crate) plan: Option<Plan>,
    /// Index into `plan.steps` of the step currently being executed.
    pub(crate) current_step: usize,
    /// Consecutive compactions that failed to bring the history under the
    /// soft limit (context refilled immediately). After a cap, auto-compaction
    /// is paused to avoid thrashing (Claude Code's thrashing protection).
    pub(crate) compact_thrash_count: u32,
    /// Test-only completion source. When set, `run()` bypasses HTTP entirely
    /// and pulls each completion body from this closure instead, so the agent
    /// loop can be driven offline with scripted responses. The closure
    /// receives the outgoing request body (so tests can inspect the messages
    /// sent) and returns the raw SSE body string (or JSON when `no_stream`).
    #[cfg(test)]
    pub(crate) completion_source: Option<CompletionSource>,
}

impl Agent {
    /// Create a new agent, seeding the system message (index 0).
    ///
    /// The system prompt is `SYSTEM_BASE` + workspace root + optional
    /// `AGENTS.md` content + optional `--rules`. The workspace must exist.
    pub fn new(settings: Settings) -> Result<Self> {
        settings.ensure_workspace()?;
        let sandbox = Sandbox::with_extra_rw(
            settings.workspace.clone(),
            settings.sandbox_extra_rw.clone(),
        );
        let messages = vec![build_system_message(&settings)];
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
            repo_map_stale: false,
            consecutive_failure_key: None,
            consecutive_failure_count: 0,
            pending_repeated_failure: None,
            blank_attempts: 0,
            pending_blank: None,
            plan: None,
            current_step: 0,
            compact_thrash_count: 0,
            #[cfg(test)]
            completion_source: None,
            client: shared_http_client()?,
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

    /// Attach a plan to this agent so step statuses are updated during
    /// execution and emitted as [`AgentEvent::PlanProgress`] events.
    pub fn with_plan(mut self, plan: Plan) -> Self {
        self.plan = Some(plan);
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

    /// Test-only: install a scripted completion source so `run()` drives the
    /// loop offline without HTTP. The closure receives the outgoing request
    /// body and returns the raw completion body (SSE text, or JSON when
    /// `no_stream`).
    #[cfg(test)]
    pub(crate) fn with_completion_source(mut self, source: CompletionSource) -> Self {
        self.completion_source = Some(source);
        self
    }

    /// The tool definitions to advertise in the next request.
    ///
    /// Returns the full static set (no clone) during execution, or the
    /// read-only subset (a fresh filtered array) during a plan-proposal turn
    /// so the model can gather context but physically cannot write files or
    /// run shell.
    fn tools_value(&self) -> serde_json::Value {
        let tools = if self.plan_only {
            match self.settings.mode {
                Mode::Chat => crate::tools::chat_tool_definitions(),
                _ => crate::tools::plan_tool_definitions(),
            }
        } else {
            cached_tool_definitions().clone()
        };
        if self.settings.allow_delegate {
            return tools;
        }
        let Some(arr) = tools.as_array() else {
            return tools;
        };
        let filtered: Vec<serde_json::Value> = arr
            .iter()
            .filter(|tool| {
                !matches!(
                    tool.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|n| n.as_str()),
                    Some("delegate_task" | "goal_set" | "todo_write")
                )
            })
            .cloned()
            .collect();
        serde_json::Value::Array(filtered)
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
        self.blank_attempts = 0;
        let mut edited_any = false;

        // Always refresh the system message so persisted goal/todos and
        // working-tree status stay current. Invalidate the repo map first
        // when a previous turn edited files.
        if self.repo_map_stale {
            crate::repomap::invalidate(&self.settings.workspace);
            self.repo_map_stale = false;
        }
        self.messages[0] = build_system_message(&self.settings);

        for iter in 0..self.settings.max_iterations {
            match self
                .run_single_iteration(&tx, iter, &mut edited_any)
                .await?
            {
                IterationOutcome::Continue => continue,
                IterationOutcome::Finished => return Ok(()),
            }
        }

        // The iteration budget is exhausted without a final answer. Before
        // wrapping up with a toolless summary, check whether the working tree
        // is dirty. If it is, force a direct git_commit so verified-but-
        // uncommitted work is never left in a dirty tree — a silent correctness
        // hazard for unattended loops (issue #127, #140).
        if !self.sandbox.is_working_tree_clean() {
            let _ = self
                .sandbox
                .git_commit_checkpoint("checkpoint: auto-commit before budget exhaustion");
        }

        self.finish_with_summary(&tx).await?;
        Ok(())
    }

    /// Run one iteration of the agent loop.
    ///
    /// Returns `IterationOutcome::Continue` if the loop should continue
    /// (tool calls were dispatched, or a stall/verify recovery re-run was
    /// triggered), or `IterationOutcome::Finished` if the turn ended
    /// (assistant message pushed, `Done` emitted).
    async fn run_single_iteration(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        iter: usize,
        edited_any: &mut bool,
    ) -> Result<IterationOutcome> {
        let _ = tx.send(AgentEvent::Iteration(iter + 1)).await;
        let t_iter = std::time::Instant::now();
        let mut edited = false;

        if let Some(ref mut plan) = self.plan {
            crate::plan::advance_step(plan, &mut self.current_step, false, false);
            let _ = tx.send(AgentEvent::PlanProgress(plan.clone())).await;
        }

        let mut reminders = compute_reminders(
            &self.messages,
            iter,
            crate::state::load_goal(&self.settings.workspace).as_ref(),
            &crate::state::load_todos(&self.settings.workspace),
        );
        if let Some(lint) = self.pending_lint.take() {
            reminders.push(lint);
        }
        if let Some(v) = self.pending_verify.take() {
            reminders.push(v);
        }
        if let Some(f) = self.pending_repeated_failure.take() {
            reminders.push(f);
        }
        if let Some(b) = self.pending_blank.take() {
            reminders.push(b);
        }

        let client = self.client.clone();
        let base_url = self.settings.base_url().to_string();
        let model = self.settings.model.clone();
        let api_key = self.settings.api_key().map(str::to_string);
        // Thrashing protection: if compaction keeps failing to bring the
        // history under the soft limit (a single huge file/tool output refills
        // context immediately), pause auto-compaction after a few attempts so
        // the loop doesn't spin on repeated summarize calls (Claude Code).
        const MAX_COMPACT_THRASH: u32 = 3;
        // Retry compaction every 4th iteration even after the pause so a
        // later prune can resume shrinking instead of staying stuck forever.
        let compact_paused =
            self.compact_thrash_count >= MAX_COMPACT_THRASH && !iter.is_multiple_of(4);
        if !compact_paused {
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
                // If compaction didn't actually reduce the history (context
                // refilled immediately), count it toward the thrash cap.
                if after >= before {
                    self.compact_thrash_count += 1;
                } else {
                    self.compact_thrash_count = 0;
                }
                let _ = tx
                    .send(AgentEvent::Compacted {
                        before_tokens: before,
                        after_tokens: after,
                    })
                    .await;
            }
        }

        let prompt_est = history_tokens(&self.messages);
        let margin = 64usize;
        let remaining = self
            .settings
            .context_window
            .saturating_sub(prompt_est)
            .saturating_sub(margin);
        let clamped_max = self.settings.max_tokens.min(remaining.max(256) as u32);

        // Ephemeral reminders go out as user nudges (not extra system
        // messages) so providers that only honor a single leading system
        // message still see them. They are request-only and never persisted.
        let body = if reminders.is_empty() {
            json!({
                "model": self.settings.model,
                "messages": request_messages_json(&self.messages),
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
                    role: "user".into(),
                    content: Some(format!("<raven_reminder>\n{text}\n</raven_reminder>")),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
            json!({
                "model": self.settings.model,
                "messages": request_messages_json(&request_messages),
                "tools": self.tools_value(),
                "tool_choice": "auto",
                "temperature": self.settings.temperature,
                "max_tokens": clamped_max,
                "stream": !self.settings.no_stream,
            })
        };

        let url = format!(
            "{}/chat/completions",
            self.settings.base_url().trim_end_matches('/')
        );

        tracing::info!(
            "iter={} pre_http_ms={} history_msgs={}",
            iter + 1,
            t_iter.elapsed().as_millis(),
            self.messages.len()
        );

        let t_send = std::time::Instant::now();
        #[cfg(test)]
        let parsed: ParsedCompletion = if let Some(source) = self.completion_source.as_mut() {
            let raw = source(&body);
            if self.settings.no_stream {
                let v: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
                process_non_stream_json(&v, tx).await
            } else {
                process_stream_text(&raw, tx).await
            }
        } else {
            let resp = match self.send_with_retry(&url, &body, tx).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                    return Ok(IterationOutcome::Finished);
                }
            };
            if self.settings.no_stream {
                self.process_non_stream(resp, tx).await
            } else {
                self.process_stream(resp, tx).await
            }
        };
        #[cfg(not(test))]
        let parsed: ParsedCompletion = {
            let resp = match self.send_with_retry(&url, &body, tx).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(AgentEvent::Error(e.to_string())).await;
                    return Ok(IterationOutcome::Finished);
                }
            };
            if self.settings.no_stream {
                self.process_non_stream(resp, tx).await
            } else {
                self.process_stream(resp, tx).await
            }
        };
        tracing::info!(
            "iter={} send_http_ms={} (model={})",
            iter + 1,
            t_send.elapsed().as_millis(),
            self.settings.model
        );

        let t_stream = std::time::Instant::now();
        tracing::info!(
            "iter={} stream_ms={} content_chars={} tool_calls={} finish_reason={:?}",
            iter + 1,
            t_stream.elapsed().as_millis(),
            parsed.content.chars().count(),
            parsed.tool_acc.len(),
            parsed.finish_reason
        );

        if let Some(err) = parsed.error {
            let msg = if parsed.finish_reason.as_deref() == Some("length") {
                format!("{err} (finish_reason=length; response may be truncated)")
            } else {
                err
            };
            let _ = tx.send(AgentEvent::Error(msg)).await;
            return Ok(IterationOutcome::Finished);
        }

        // Truncated tool JSON with finish_reason=length: surface rather than
        // dispatching half-formed arguments.
        if parsed.finish_reason.as_deref() == Some("length") && !parsed.tool_acc.is_empty() {
            let incomplete = parsed.tool_acc.values().any(|(_, _, args)| {
                if args.trim().is_empty() {
                    return true;
                }
                serde_json::from_str::<Value>(args).is_err()
            });
            if incomplete {
                let _ = tx
                    .send(AgentEvent::Error(
                        "Completion truncated (finish_reason=length) with incomplete tool call arguments"
                            .into(),
                    ))
                    .await;
                return Ok(IterationOutcome::Finished);
            }
        }

        let content_buf = parsed.content;
        let tool_acc = parsed.tool_acc;
        let content_blank = content_buf.trim().is_empty();

        let assistant = ChatMessage {
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
            if self
                .handle_no_tool_calls(tx, assistant, content_blank, *edited_any)
                .await?
            {
                return Ok(IterationOutcome::Continue);
            }
            return Ok(IterationOutcome::Finished);
        }

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

        self.execute_tool_calls(tx, tcs, assistant, &mut edited, edited_any)
            .await?;
        Ok(IterationOutcome::Continue)
    }

    // ── HTTP helpers ───────────────────────────────────────────────────

    /// Build and send the request with auth headers, applying retry logic
    /// for transient failures (connection errors, 5xx, 429).
    pub(crate) async fn send_with_retry(
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
            if let Some(key) = self.settings.api_key() {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
            // OpenRouter optional ranking headers (harmless elsewhere).
            if url.contains("openrouter.ai") {
                req = req
                    .header("HTTP-Referer", "https://github.com/raven-agent/raven")
                    .header("X-Title", "Raven");
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
                                provider: self.settings.provider.name.clone(),
                                model: self.settings.model.clone(),
                            });
                        }
                        return Err(AgentError::HttpError {
                            provider: self.settings.provider.name.clone(),
                            status,
                            body: cap_http_body(text),
                        });
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
                    return Err(AgentError::HttpError {
                        provider: self.settings.provider.name.clone(),
                        status,
                        body: cap_http_body(text),
                    });
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
                    return Err(AgentError::ProviderUnreachable {
                        provider: self.settings.provider.name.clone(),
                        url: url.to_string(),
                        source: e,
                    });
                }
                Err(e) => {
                    return Err(AgentError::ProviderUnreachable {
                        provider: self.settings.provider.name.clone(),
                        url: url.to_string(),
                        source: e,
                    });
                }
            }
        }
        // All retries exhausted without a definitive success or error.
        Err(AgentError::HttpError {
            provider: self.settings.provider.name.clone(),
            status: 503,
            body: "retries exhausted — all attempts failed with transient errors".into(),
        })
    }
}

/// Serialize chat messages for the wire format.
///
/// Assistant messages that only carry `tool_calls` omit `content` in our
/// in-memory type (`None` + `skip_serializing_if`). Some OpenAI-compatible
/// validators want an explicit `"content": null` instead — emit that here
/// without changing the persisted `ChatMessage` shape.
fn request_messages_json(messages: &[ChatMessage]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|m| {
                let mut v = serde_json::to_value(m).unwrap_or_else(|_| json!({}));
                if m.tool_calls.is_some() {
                    if let Some(obj) = v.as_object_mut() {
                        if !obj.contains_key("content") {
                            obj.insert("content".into(), Value::Null);
                        }
                    }
                }
                v
            })
            .collect(),
    )
}

#[cfg(test)]
mod wire_format_tests {
    use super::super::types::{ChatMessage, FunctionCall, ToolCall};
    use super::request_messages_json;
    use serde_json::json;

    #[test]
    fn request_messages_json_sets_null_content_for_tool_only_assistant() {
        let msgs = vec![ChatMessage {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c1".into(),
                type_: "function".into(),
                function: FunctionCall {
                    name: "read_file".into(),
                    arguments: r#"{"path":"a"}"#.into(),
                },
            }]),
            tool_call_id: None,
        }];
        let v = request_messages_json(&msgs);
        let obj = v.as_array().unwrap()[0].as_object().unwrap();
        assert!(obj.get("content").unwrap().is_null());
        assert!(obj.get("tool_calls").is_some());
    }

    #[test]
    fn request_messages_json_keeps_text_content() {
        let msgs = vec![ChatMessage {
            role: "assistant".into(),
            content: Some("hi".into()),
            tool_calls: None,
            tool_call_id: None,
        }];
        let v = request_messages_json(&msgs);
        assert_eq!(v.as_array().unwrap()[0]["content"], json!("hi"));
    }
}
