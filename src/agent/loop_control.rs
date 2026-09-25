//! Loop control: blank-stall recovery, enforced-verify recovery, ephemeral
//! reminders, and graceful max-iteration wrap-up.
//!
//! These helpers implement the turn-level control flow that decides whether
//! an iteration finishes cleanly, re-runs with a recovery reminder, or wraps
//! up with a summary when the iteration budget is exhausted.

use anyhow::Result;
use tokio::sync::mpsc;

use crate::context::history_tokens;

use super::core::Agent;
use super::tools_exec::IDENTICAL_SUCCESS_LOOP_N;
use super::types::{AgentEvent, ChatMessage};
use crate::tokenizer::TokenUsage;

impl Agent {
    /// Handle the no-tool-calls branch of an iteration.
    ///
    /// Returns `Ok(true)` if the turn should `continue` the loop (a stall or
    /// verify recovery re-run was triggered), or `Ok(false)` if the turn
    /// finished (assistant message pushed, `Done` emitted) and the caller
    /// should return.
    ///
    /// `content_blank` is whether the accumulated content was empty/whitespace;
    /// `edited_any` is the turn-level "edited files" flag gating the verify
    /// gate.
    pub(crate) async fn handle_no_tool_calls(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        assistant: ChatMessage,
        content_blank: bool,
        edited_any: bool,
    ) -> Result<bool> {
        // Blank-response stall: the model returned no tool calls AND no
        // non-whitespace content. Treat this as a stall, not a finish —
        // inject an ephemeral reminder and re-run (capped), so a blank
        // generation can't silently drop the deliverable (issue #110).
        const MAX_BLANK_ATTEMPTS: u32 = 3;
        if content_blank && self.blank_attempts < MAX_BLANK_ATTEMPTS {
            self.blank_attempts += 1;
            self.pending_blank = Some(
                "You returned no content and no tool calls this turn. \
                 Produce the requested deliverable or a summary of your findings \
                 now — do not end with an empty reply."
                    .to_string(),
            );
            return Ok(true);
        }
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
                "You edited files this turn but did not verify your changes. \
                 Run a test/typecheck/lint command (e.g. cargo test, npm test, \
                 cargo clippy, pytest) via run_shell or call run_tests, then \
                 fix any failures before answering."
                    .to_string(),
            );
            let _ = tx.send(AgentEvent::VerifyRequired).await;
            return Ok(true);
        }
        // Blank cap exhausted: still nothing to show. Fall through to
        // emit_summary so the turn ends with a visible canned line
        // rather than an empty assistant message.
        if content_blank {
            self.emit_summary(
                tx,
                Some(
                    "I received several empty replies and could not produce a final answer.".into(),
                ),
                None,
            )
            .await?;
            return Ok(false);
        }
        self.messages.push(assistant);
        if let Some(ref mut plan) = self.plan {
            crate::plan::advance_step(plan, &mut self.current_step, false, true);
            let _ = tx.send(AgentEvent::PlanProgress(plan.clone())).await;
        }
        let _ = tx.send(AgentEvent::Done).await;
        Ok(false)
    }

    /// Gracefully wrap up a turn that exhausted its iteration budget.
    ///
    /// Mirrors Hermes Agent's max-iteration fallback: inject a user message
    /// asking the model to summarize progress without calling any more tools,
    /// then make ONE toolless request. The resulting summary is pushed onto
    /// `self.messages` as a real assistant turn and a [`AgentEvent::Done`] is
    /// emitted, so session persistence and a subsequent "continue" see a
    /// coherent, continuous conversation.
    ///
    /// Fail-open: if the summary request itself errors (or the model returns
    /// nothing), a short canned summary is used instead — the turn always ends
    /// cleanly and is never surfaced as an `Error` event.
    pub(crate) async fn finish_with_summary(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
    ) -> Result<()> {
        self.finish_with_wrap_up(
            tx,
            "You've reached the maximum number of tool-calling iterations             allowed for this turn. Provide a final response summarizing what you've found and             accomplished so far, without calling any more tools.",
        )
        .await
    }

    /// Force-finalize after a verify-style success plateau (circling item #5).
    pub(crate) async fn finish_with_verify_plateau(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        tool_name: &str,
        streak: usize,
    ) -> Result<()> {
        let prompt = format!(
            "VERIFY PLATEAU: `{tool_name}` returned the same successful result {streak} times              and primary work is already met (only non-blocking residue remains).              Provide a final response summarizing what you accomplished. Do not call any more tools."
        );
        self.finish_with_wrap_up(tx, &prompt).await
    }

    /// Shared toolless wrap-up used by max-iteration and verify-plateau stops.
    pub(crate) async fn finish_with_wrap_up(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        summary_prompt: &str,
    ) -> Result<()> {
        self.messages
            .push(ChatMessage::plain("user", Some(summary_prompt.to_string())));

        // Clamp max_tokens so the summary request fits the context window.
        // The estimate is calibrated (when usage samples exist) so the clamp
        // tracks the loaded model's real tokenizer, not cl100k.
        let prompt_est = self.calibration.correct(history_tokens(&self.messages));
        let margin = 64usize;
        let clamped_max = super::core::clamp_max_tokens(
            self.settings.max_tokens,
            prompt_est,
            self.settings.context_window,
            margin,
        );

        // Toolless request: no `tools`/`tool_choice`, so the model can only
        // produce a final text answer (no tool calls to burn more iterations).
        let mut body = serde_json::json!({
            "model": self.settings.model,
            "messages": super::core::request_messages_json(&self.messages),
            "temperature": self.settings.temperature_json(),
            "max_tokens": clamped_max,
            "stream": !self.settings.no_stream,
        });
        if !self.settings.no_stream && self.usage_supported {
            body["stream_options"] = serde_json::json!({"include_usage": true});
        }

        let url = format!(
            "{}/chat/completions",
            self.settings.base_url().trim_end_matches('/')
        );

        let resp = match self.send_with_retry(&url, &mut body, tx).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("summary request failed after budget exhaustion: {e}");
                return self.emit_summary(tx, None, None).await;
            }
        };

        // Any tool calls parsed here are ignored — with no tools advertised the
        // model cannot legitimately emit one; we only keep the text content.
        // Do not observe usage here: this request has no tools schema, so its
        // prompt_tokens would skew the EMA used for tool-bearing clamps. The
        // meter is still persisted on the summary message below.
        let parsed = if self.settings.no_stream {
            self.process_non_stream(resp, tx).await
        } else {
            self.process_stream(resp, tx).await
        };
        if let Some(err) = parsed.error {
            tracing::warn!("summary request returned provider error: {err}");
            return self.emit_summary(tx, None, None).await;
        }
        let content_buf = parsed.content;

        self.emit_summary(
            tx,
            (!content_buf.is_empty()).then_some(content_buf),
            parsed.usage,
        )
        .await
    }

    /// Push a final summary assistant turn and emit `Done`.
    ///
    /// If `content` is `None` (empty/failed model output), a canned summary is
    /// streamed and persisted instead, so the turn always ends with a usable
    /// assistant message. `usage` is the wrap-up request's own meter; the
    /// message is fresh so attaching it cannot double-count.
    pub(crate) async fn emit_summary(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        content: Option<String>,
        usage: Option<TokenUsage>,
    ) -> Result<()> {
        let had_content = content.is_some();
        let text = content.unwrap_or_else(|| {
            format!(
                "I reached the maximum iterations ({}) but could not generate a summary.",
                self.settings.max_iterations
            )
        });
        // Stream the fallback so the user always sees a closing line, even when
        // the model returned nothing.
        if !had_content {
            let _ = tx.send(AgentEvent::TextDelta(text.clone())).await;
        }
        self.messages.push(ChatMessage {
            role: "assistant".into(),
            content: Some(text),
            tool_calls: None,
            tool_call_id: None,
            reasoning_content: None,
            usage,
        });
        let _ = tx.send(AgentEvent::Done).await;
        Ok(())
    }
}

/// Compute ephemeral system reminders to inject into the *next* request.
///
/// These are deliberately kept out of the persisted conversation (`messages`)
/// so the history stays a strict `[system, user, assistant, tool, ...]`
/// alternation. Returns a list of reminder texts (empty on the common path).
///
/// - After 6+ consecutive tool-only assistant turns (`iter >= 6`), push a
///   "stuck in a loop" reminder. The threshold is deliberately high so normal
///   context-gathering (goal → list → grep → read) is never interrupted; only
///   a genuine tool-calling loop triggers it.
/// - After 3 identical successful `(name, args)` tool results in recent turns
///   (with or without assistant text), push a HARD STOP reminder; dispatch
///   also refuses further identical calls (see `tools_exec`).
/// - When a verify-style success has plateaued and primary work is met, push a
///   VERIFY PLATEAU reminder (`run_loop` also force-finalizes — see
///   [`verify_plateau_should_stop`]).
/// - When the latest assistant narration contradicts the latest tool payload
///   (narrow heuristic: clean working tree vs dirty `git_status`), push a
///   trust-the-tool reminder.
pub(crate) fn compute_reminders(
    messages: &[ChatMessage],
    iter: usize,
    goal: Option<&crate::state::Goal>,
    todos: &[crate::state::TodoItem],
) -> Vec<String> {
    let mut reminders = Vec::new();

    if iter >= 6 {
        let tool_only_count = messages
            .iter()
            .rev()
            .filter(|m| m.role == "assistant")
            .take(6)
            .filter(|m| m.content.is_none() && m.tool_calls.is_some())
            .count();
        if tool_only_count >= 6 {
            reminders.push(
                "You have made several tool calls without producing text. \
                 If you are stuck in a loop, try a different approach. \
                 Otherwise, continue working toward the goal and apply your changes."
                    .into(),
            );
        }
    }

    // Identical successful (name, args) loop: N repeats → hard stop reminder.
    // Fires with or without assistant narration (unlike the tool-only streak).
    if let Some((name, n)) = identical_success_loop(messages, IDENTICAL_SUCCESS_LOOP_N) {
        reminders.push(format!(
            "HARD STOP: `{name}` already succeeded {n} times with the same arguments. \
             Further identical calls will be refused. Use the result you have, call ask_user \
             if you need a decision, or finalize your answer now."
        ));
    }

    // Goal-aware reflection: after a long stretch of tool-only iterations,
    // re-anchor the model to its objective and the next pending task (Goose's
    // `next_step` carry-over). The system message already carries goal/todos
    // every turn, so this only earns its tokens when a turn drags on: fire
    // once at iteration 4, then every 8th (12, 20, …) instead of every
    // iteration — a per-iteration anchor makes small models parrot the
    // reminder ("Re-anchoring: …") in every narration instead of working.
    if iter >= 4 && (iter - 4).is_multiple_of(8) {
        let mut anchor = String::new();
        if let Some(goal) = goal {
            if crate::state::normalize_status(&goal.status) != "completed" {
                anchor.push_str(&format!("Your goal: {}\n", goal.description));
            }
        }
        let next_pending = todos
            .iter()
            .find(|t| crate::state::normalize_status(&t.status) != "completed")
            .map(|t| t.content.clone());
        if let Some(next) = next_pending {
            anchor.push_str(&format!("Next pending task: {next}\n"));
        }
        if !anchor.is_empty() {
            reminders.push(format!(
                "Re-anchor on your objective before continuing:\n{anchor}"
            ));
        }
    }

    if let Some((name, n)) = verify_plateau_should_stop(messages, goal, todos) {
        reminders.push(format!(
            "VERIFY PLATEAU: `{name}` returned the same successful result {n} times and              primary work is already met. Stop verifying; finalize your answer now."
        ));
    }

    if let Some(nudge) = narration_contradiction_reminder(messages) {
        reminders.push(nudge);
    }

    reminders
}

/// If the N most recent *successful* tool results share the same
/// `(name, normalized args)`, return that name and N.
fn identical_success_loop(messages: &[ChatMessage], n: usize) -> Option<(String, usize)> {
    if n == 0 {
        return None;
    }
    let recent = recent_successful_tool_keys(messages);
    if recent.len() < n {
        return None;
    }
    let first = &recent[0];
    if recent.iter().take(n).all(|k| k == first) {
        Some((first.0.clone(), n))
    } else {
        None
    }
}

/// Newest-first list of successful `(tool_name, normalized_args)` from history.
fn recent_successful_tool_keys(messages: &[ChatMessage]) -> Vec<(String, String)> {
    use super::tools_exec::normalize_tool_args;
    use std::collections::HashMap;

    let mut call_meta: HashMap<&str, (String, String)> = HashMap::new();
    for m in messages {
        if m.role != "assistant" {
            continue;
        }
        let Some(tcs) = m.tool_calls.as_ref() else {
            continue;
        };
        for tc in tcs {
            call_meta.insert(
                tc.id.as_str(),
                (
                    tc.function.name.clone(),
                    normalize_tool_args(&tc.function.arguments),
                ),
            );
        }
    }

    let mut out = Vec::new();
    for m in messages.iter().rev() {
        if m.role != "tool" {
            continue;
        }
        let Some(id) = m.tool_call_id.as_deref() else {
            continue;
        };
        let Some((name, args)) = call_meta.get(id) else {
            continue;
        };
        let content = m.content.as_deref().unwrap_or("");
        if content.starts_with("Error:") || content.starts_with("Tool error:") {
            continue;
        }
        out.push((name.clone(), args.clone()));
        if out.len() >= 16 {
            break;
        }
    }
    out
}

/// How many consecutive identical verify-style successes trigger a plateau stop.
pub(crate) const VERIFY_PLATEAU_K: usize = 2;

/// Whether open work is only non-blocking residue (or primary work is already met).
fn only_residue_or_primary_met(
    goal: Option<&crate::state::Goal>,
    todos: &[crate::state::TodoItem],
) -> bool {
    if crate::state::primary_work_satisfied(goal, todos) {
        return true;
    }
    if todos.is_empty() {
        return false;
    }
    // Every incomplete todo is residue-style; any live todo is already completed.
    todos.iter().all(|t| {
        crate::state::looks_like_residue_todo(&t.content)
            || crate::state::normalize_status(&t.status) == "completed"
    }) && todos
        .iter()
        .any(|t| crate::state::looks_like_residue_todo(&t.content))
}

/// Detect a verify-style success plateau that should end the turn.
///
/// Returns `(tool_name, streak)` when the same verify tool key **or** the same
/// verify result body succeeded [`VERIFY_PLATEAU_K`] times and only non-blocking
/// residue remains (circling item #5). Extends the #195 identical-success
/// refuse: after the primary metric is met, stop burning iterations.
pub(crate) fn verify_plateau_should_stop(
    messages: &[ChatMessage],
    goal: Option<&crate::state::Goal>,
    todos: &[crate::state::TodoItem],
) -> Option<(String, usize)> {
    if !only_residue_or_primary_met(goal, todos) {
        return None;
    }
    verify_success_plateau(messages, VERIFY_PLATEAU_K)
}

struct VerifyHit {
    name: String,
    args: String,
    content: String,
}

/// Newest-first verify successes that share a key or an unchanged result body.
fn verify_success_plateau(messages: &[ChatMessage], k: usize) -> Option<(String, usize)> {
    if k == 0 {
        return None;
    }
    let recent = recent_successful_verify_hits(messages);
    if recent.len() < k {
        return None;
    }
    let first = &recent[0];
    let plateau = recent.iter().take(k).all(|r| {
        (r.name == first.name && r.args == first.args)
            || (r.name == first.name && r.content == first.content)
    });
    if plateau {
        Some((first.name.clone(), k))
    } else {
        None
    }
}

fn recent_successful_verify_hits(messages: &[ChatMessage]) -> Vec<VerifyHit> {
    use super::tools_exec::{is_mcp_verify_cacheable, normalize_tool_args};
    use std::collections::HashMap;

    let mut call_meta: HashMap<&str, (String, String)> = HashMap::new();
    for m in messages {
        if m.role != "assistant" {
            continue;
        }
        let Some(tcs) = m.tool_calls.as_ref() else {
            continue;
        };
        for tc in tcs {
            call_meta.insert(
                tc.id.as_str(),
                (
                    tc.function.name.clone(),
                    normalize_tool_args(&tc.function.arguments),
                ),
            );
        }
    }

    let mut out = Vec::new();
    for m in messages.iter().rev() {
        if m.role != "tool" {
            continue;
        }
        let Some(id) = m.tool_call_id.as_deref() else {
            continue;
        };
        let Some((name, args)) = call_meta.get(id) else {
            continue;
        };
        if !is_mcp_verify_cacheable(name) {
            continue;
        }
        let content = m.content.as_deref().unwrap_or("");
        if content.starts_with("Error:") || content.starts_with("Tool error:") {
            continue;
        }
        // HARD STOP refusals are sticky for identical-args spam; they still
        // count toward the plateau so force-finalize can fire after refuse.
        out.push(VerifyHit {
            name: name.clone(),
            args: args.clone(),
            content: content.to_string(),
        });
        if out.len() >= 16 {
            break;
        }
    }
    out
}

/// Narrow narration-vs-tool contradiction nudge (circling item #6).
///
/// Currently: assistant claims a clean working tree while the latest
/// `git_status` result is dirty (porcelain lines / not the clean sentinel).
pub(crate) fn narration_contradiction_reminder(messages: &[ChatMessage]) -> Option<String> {
    match detect_narration_contradiction(messages)? {
        NarrationContradiction::CleanVsDirtyGitStatus => Some(
            "Your narration claimed the working tree is clean, but the latest git_status \
             result shows dirty paths. Trust the tool output; correct your summary before continuing."
                .into(),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum NarrationContradiction {
    CleanVsDirtyGitStatus,
}

/// Returns the contradiction kind when detected (unit-tested).
pub(crate) fn detect_narration_contradiction(
    messages: &[ChatMessage],
) -> Option<NarrationContradiction> {
    let latest_status = latest_tool_result(messages, "git_status")?;
    if !git_status_looks_dirty(&latest_status) {
        return None;
    }
    let narration = latest_assistant_narration(messages)?;
    if claims_working_tree_clean(&narration) {
        Some(NarrationContradiction::CleanVsDirtyGitStatus)
    } else {
        None
    }
}

fn latest_tool_result(messages: &[ChatMessage], tool_name: &str) -> Option<String> {
    use std::collections::HashMap;
    let mut call_meta: HashMap<&str, String> = HashMap::new();
    for m in messages {
        if m.role != "assistant" {
            continue;
        }
        let Some(tcs) = m.tool_calls.as_ref() else {
            continue;
        };
        for tc in tcs {
            call_meta.insert(tc.id.as_str(), tc.function.name.clone());
        }
    }
    for m in messages.iter().rev() {
        if m.role != "tool" {
            continue;
        }
        let Some(id) = m.tool_call_id.as_deref() else {
            continue;
        };
        let Some(name) = call_meta.get(id) else {
            continue;
        };
        if name != tool_name {
            continue;
        }
        let content = m.content.as_deref().unwrap_or("");
        if content.starts_with("Error:") || content.starts_with("Tool error:") {
            return None;
        }
        return Some(content.to_string());
    }
    None
}

fn latest_assistant_narration(messages: &[ChatMessage]) -> Option<String> {
    for m in messages.iter().rev() {
        if m.role != "assistant" {
            continue;
        }
        if let Some(c) = m.content.as_deref() {
            let trimmed = c.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn claims_working_tree_clean(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    const PHRASES: &[&str] = &[
        "working tree is clean",
        "working tree clean",
        "clean working tree",
        "nothing to commit",
        "no changes (working tree clean)",
        "tree is clean",
    ];
    PHRASES.iter().any(|p| lower.contains(p))
}

fn git_status_looks_dirty(status: &str) -> bool {
    let trimmed = status.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("No changes (working tree clean)") {
        return false;
    }
    // Porcelain v1: XY PATH lines (e.g. " M src/a.rs", "?? foo", "M  bar").
    trimmed.lines().any(|line| {
        let line = line.trim_end();
        if line.is_empty() {
            return false;
        }
        let b = line.as_bytes();
        if b.len() >= 2 {
            let x = b[0];
            let y = b[1];
            let code = |c: u8| {
                matches!(
                    c,
                    b'M' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!' | b' '
                )
            };
            if code(x) && code(y) && (x != b' ' || y != b' ') {
                return true;
            }
        }
        line.starts_with("??") || line.contains("modified:") || line.contains("Untracked")
    })
}

/// Summarize a slice of conversation history into a compact paragraph.
///
/// Used by LLM-structured compaction. Makes a single non-streaming chat
/// request asking the model to distill the middle turns. Returns `None` if
/// the request fails, so the caller falls back to the extractive summarizer.
/// Options for the compaction summarizer request.
pub(crate) struct SummarizeOptions {
    pub reasoning_effort: Option<String>,
    pub short_prompt: bool,
}

pub(crate) async fn summarize_request(
    client: reqwest::Client,
    base_url: String,
    model: String,
    api_key: Option<String>,
    request_headers: Vec<(String, String)>,
    options: SummarizeOptions,
    middle: Vec<ChatMessage>,
) -> Option<String> {
    let short_prompt = options.short_prompt;
    let reasoning_effort = options.reasoning_effort;
    let transcript = middle
        .iter()
        .map(|m| format!("{}: {}", m.role, m.content.clone().unwrap_or_default()))
        .collect::<Vec<_>>()
        .join("\n");
    let prompt = if short_prompt {
        format!(
            "Summarize this coding-agent transcript. Keep the goal, open tasks, \
             files touched, and the last verification result. Be compact.\n\n\
             {transcript}\n\nSummary:"
        )
    } else {
        format!(
            "Distill the following conversation segment into a compact summary \
             (max ~150 words). Use this layout when the information exists:\n\
             Goal: <current goal>\n\
             Open todos: <pending items>\n\
             Key paths: <files touched>\n\
             Last verification: <run_tests/run_lint result>\n\
             Then a short factual recap of user requests, decisions, and actions. \
             This will replace the original messages in a long-running agent session.\n\n\
             {transcript}\n\nSummary:"
        )
    };

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
    let mut body = body;
    if let Some(effort) = reasoning_effort {
        body["reasoning_effort"] = serde_json::json!(effort);
    }

    let mut req = client.post(&url).json(&body);
    if let Some(key) = &api_key {
        req = req.header("Authorization", format!("Bearer {key}"));
    }
    for (name, value) in &request_headers {
        req = req.header(name.as_str(), value.as_str());
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
