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
use super::types::{AgentEvent, ChatMessage};

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
        let summary_prompt = "You've reached the maximum number of tool-calling iterations \
            allowed for this turn. Provide a final response summarizing what you've found and \
            accomplished so far, without calling any more tools.";
        self.messages.push(ChatMessage {
            role: "user".into(),
            content: Some(summary_prompt.to_string()),
            tool_calls: None,
            tool_call_id: None,
        });

        // Clamp max_tokens so the summary request fits the context window.
        let prompt_est = history_tokens(&self.messages);
        let margin = 64usize;
        let clamped_max = super::core::clamp_max_tokens(
            self.settings.max_tokens,
            prompt_est,
            self.settings.context_window,
            margin,
        );

        // Toolless request: no `tools`/`tool_choice`, so the model can only
        // produce a final text answer (no tool calls to burn more iterations).
        let body = serde_json::json!({
            "model": self.settings.model,
            "messages": &self.messages,
            "temperature": self.settings.temperature,
            "max_tokens": clamped_max,
            "stream": !self.settings.no_stream,
        });

        let url = format!(
            "{}/chat/completions",
            self.settings.base_url().trim_end_matches('/')
        );

        let resp = match self.send_with_retry(&url, &body, tx).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("summary request failed after budget exhaustion: {e}");
                return self.emit_summary(tx, None).await;
            }
        };

        // Any tool calls parsed here are ignored — with no tools advertised the
        // model cannot legitimately emit one; we only keep the text content.
        let parsed = if self.settings.no_stream {
            self.process_non_stream(resp, tx).await
        } else {
            self.process_stream(resp, tx).await
        };
        if let Some(err) = parsed.error {
            tracing::warn!("summary request returned provider error: {err}");
            return self.emit_summary(tx, None).await;
        }
        let content_buf = parsed.content;

        self.emit_summary(tx, (!content_buf.is_empty()).then_some(content_buf))
            .await
    }

    /// Push a final summary assistant turn and emit `Done`.
    ///
    /// If `content` is `None` (empty/failed model output), a canned summary is
    /// streamed and persisted instead, so the turn always ends with a usable
    /// assistant message.
    pub(crate) async fn emit_summary(
        &mut self,
        tx: &mpsc::Sender<AgentEvent>,
        content: Option<String>,
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
/// - After 3+ consecutive tool-only assistant turns (`iter >= 3`), push a
///   "stop calling tools" reminder to break a tool-calling loop.
/// - At `iter == 5`, push a "reflect on progress" nudge.
pub(crate) fn compute_reminders(messages: &[ChatMessage], iter: usize) -> Vec<String> {
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

/// Summarize a slice of conversation history into a compact paragraph.
///
/// Used by LLM-structured compaction. Makes a single non-streaming chat
/// request asking the model to distill the middle turns. Returns `None` if
/// the request fails, so the caller falls back to the extractive summarizer.
pub(crate) async fn summarize_request(
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
