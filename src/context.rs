//! Context compaction logic.
//!
//! Token counting is delegated to [`crate::tokenizer`], which implements a
//! fast BPE-like estimator for context management. This module handles the
//! compaction strategy: when to compact, what to keep, and how to summarize.
//!
//! Also provides [`fetch_context_window`] — queries Ollama's `/api/show`
//! endpoint for the model's actual `context_length` from its metadata,
//! falling back to a name-based heuristic when the API is unreachable.
//!
//! Key invariants:
//! - System message (index 0) is never dropped.
//! - Tool-call / tool-result pairs are never split.

use crate::agent::ChatMessage;
pub use crate::tokenizer::{history_tokens, message_tokens};

/// Infer a model's context window from its name (fallback when API is unreachable).
///
/// Heuristics:
///   - glm:cloud, deepseek-v4:cloud (flash and pro)          → 1_000_000
///   - qwen3.5                                              → 262_144
///   - gemma4 / gemma3 / qwen2.5 / qwen3 / deepseek / llama3.1 / llama3.2 / codestral → 128_000
///   - llama3 / codellama / "32k" in name                   → 32_768
///   - mistral / "8k" in name                               → 8_192
///   - fallback                                             → 32_768
pub fn infer_context_window(model: &str) -> usize {
    let m = model.to_lowercase();
    // Cloud glm (via Ollama) has a 1M-token context.
    if m.contains("glm") && m.contains("cloud") {
        1_000_000
    } else if m.contains("deepseek-v4") && m.contains("cloud") {
        // deepseek-v4-flash:cloud and deepseek-v4-pro:cloud → 1M (verified via /api/show).
        1_000_000
    } else if m.contains("qwen3.5") {
        // Qwen 3.5 family is 256K; check before the broader `qwen3` match.
        262_144
    } else if m.contains("gemma4")
        || m.contains("gemma3")
        || m.contains("qwen2.5")
        || m.contains("qwen3")
        || m.contains("llama3.1")
        || m.contains("llama3.2")
        || m.contains("deepseek")
        || m.contains("codestral")
        || m.contains("glm")
    {
        128_000
    } else if m.contains("llama3") || m.contains("codellama") || m.contains("32k") {
        32_768
    } else if m.contains("mistral") || m.contains("8k") {
        8_192
    } else {
        32_768
    }
}

/// Fetch the actual context window from the provider's API.
///
/// For Ollama, uses `/api/show`. For OpenAI-compatible providers (OpenRouter,
/// etc.), uses `/models` and reads the `context_length` field. Falls back to
/// [`infer_context_window`] if the API call fails or the field is missing.
pub async fn fetch_context_window(provider: &crate::config::Provider, model: &str) -> usize {
    let base_url = &provider.base_url;
    let trimmed = base_url.trim_end_matches('/').trim_end_matches("/v1");
    let host = base_url.to_ascii_lowercase();
    let looks_cloud = [
        "openrouter.ai",
        "opencode.ai",
        "api.openai.com",
        "api.x.ai",
        "api.anthropic.com",
        "together.xyz",
        "groq.com",
        "fireworks.ai",
    ]
    .iter()
    .any(|s| host.contains(s));

    // Skip the Ollama /api/show probe on known cloud hosts (extra failed RTT).
    if !looks_cloud {
        if let Some(ctx) = fetch_ollama_context(trimmed, model).await {
            return ctx;
        }
    }

    // Try the OpenAI-compatible /models endpoint (OpenRouter, etc.).
    if let Some(ctx) = fetch_openai_context(provider, model).await {
        return ctx;
    }

    // Fall back to name heuristics.
    infer_context_window(model)
}

/// Query Ollama's `/api/show` for the model's context length.
async fn fetch_ollama_context(base_url: &str, model: &str) -> Option<usize> {
    let show_url = format!("{}/api/show", base_url);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .ok()?;

    let resp = client
        .post(&show_url)
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
        .ok()?;

    let body: serde_json::Value = resp.json().await.ok()?;
    let info = body.get("model_info")?;
    let arch = info.get("general.architecture")?.as_str()?;
    let key = format!("{arch}.context_length");
    let n = info.get(&key)?.as_u64()?;
    if n > 0 {
        Some(n as usize)
    } else {
        None
    }
}

/// Query the OpenAI-compatible `/models` endpoint for the model's context
/// length. Works with OpenRouter and other providers that expose
/// `context_length` in the model metadata.
async fn fetch_openai_context(provider: &crate::config::Provider, model: &str) -> Option<usize> {
    let models_url = format!("{}/models", provider.base_url.trim_end_matches('/'));

    let mut req = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()?
        .get(&models_url);

    // Pass the provider's API key if available.
    if let Some(key) = &provider.api_key {
        req = req.bearer_auth(key);
    }

    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }

    let body: serde_json::Value = resp.json().await.ok()?;
    let models = body.get("data")?.as_array()?;

    // Find the model by ID and read its context_length.
    for m in models {
        let id = m.get("id")?.as_str()?;
        if id == model {
            let ctx = m.get("context_length")?.as_u64()?;
            if ctx > 0 {
                return Some(ctx as usize);
            }
        }
    }
    None
}

/// A boundary that keeps tool-call / tool-result pairs together.
///
/// Returns the index in `messages` such that everything from that index onward
/// has no orphaned tool results (every tool result's matching assistant call
/// is also included). The system message (index 0) is always kept separately.
fn find_safe_tail_start(messages: &[ChatMessage], desired_start: usize) -> usize {
    if messages.is_empty() {
        return 0;
    }

    let mut start = desired_start.max(1); // never cut into system at index 0

    // Walk backward: if the message at `start` is a tool result, we need to
    // include the assistant message that issued the call. Keep walking back
    // until we land on a non-tool message.
    while start < messages.len() {
        let role = messages[start].role.as_str();
        if role == "tool" {
            // Need to step back to find the assistant message with tool_calls
            if start > 1 {
                start -= 1;
                continue;
            } else {
                // Can't go further back; just start at 1 (after system)
                return 1;
            }
        }
        // If this is an assistant message with tool_calls, we must include
        // all the tool results that follow it. That's fine — they're after `start`.
        break;
    }

    start
}

/// The result of a compaction: the new token estimate and how it changed.
pub type CompactionResult = (usize, usize);

/// Outcome of a compaction pass, including a short user-visible note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionReport {
    /// Token estimate before this pass.
    pub before_tokens: usize,
    /// Token estimate after this pass (never greater than `before_tokens`).
    pub after_tokens: usize,
    /// One-line "what was compacted" for the TUI / headless log.
    pub note: String,
}

/// Structured facts lifted out of compacted turns so the model keeps its
/// bearings after the middle is replaced by a summary.
#[derive(Debug, Default, Clone)]
struct CompactFacts {
    goal: Option<String>,
    open_todos: Vec<String>,
    key_paths: Vec<String>,
    last_verification: Option<String>,
    middle_len: usize,
}

/// The outcome of [`prepare_compaction`].
enum CompactionOutcome {
    /// History is under the soft limit — no compaction performed.
    None,
    /// Soft-pruning alone brought us under the limit; no summarization needed.
    /// Carries `(before, after)` token estimates.
    PrunedOnly(usize, usize),
    /// The middle region needs summarizing before assembly.
    NeedsSummary(CompactionPlan),
}

/// Prepare compaction: prune old tool results, check the threshold, and if
/// over it, choose the tail boundary.
struct CompactionPlan {
    /// Messages between system (0) and tail_start, to be summarized.
    middle: Vec<ChatMessage>,
    /// Index into `messages` where the kept tail begins.
    tail_start: usize,
    /// Token estimate before compaction.
    before: usize,
}

fn prepare_compaction(
    messages: &mut [ChatMessage],
    context_window: usize,
    compact_threshold: f32,
) -> CompactionOutcome {
    // Reserve for model output.
    let output_reserve = (context_window / 8).max(1024);
    let soft_limit =
        ((context_window.saturating_sub(output_reserve)) as f32 * compact_threshold) as usize;

    let before = history_tokens(messages);
    if before <= soft_limit {
        return CompactionOutcome::None;
    }

    // Step 1: Soft-prune old tool results (keep head 1500 + tail 1500 chars)
    // for tool messages older than the last 3 turns. This is cheaper than
    // full compaction and may bring us under the limit without summarizing.
    prune_tool_results(messages, 3);
    let after_prune = history_tokens(messages);
    if after_prune <= soft_limit {
        return CompactionOutcome::PrunedOnly(before, after_prune);
    }

    // Trailing budget: ~40% of (context_window - output_reserve)
    let trailing_budget = ((context_window.saturating_sub(output_reserve)) as f32 * 0.40) as usize;

    // Find the tail: start from the end and accumulate messages until we exceed
    // the trailing budget. Then adjust for tool-call/tool-result pair safety.
    let mut tail_cost = 0usize;
    let mut tail_start = messages.len();

    for i in (1..messages.len()).rev() {
        let cost = message_tokens(&messages[i]);
        if tail_cost + cost > trailing_budget && tail_start < messages.len() {
            break;
        }
        tail_cost += cost;
        tail_start = i;
    }

    // Adjust so we don't split a tool-call/tool-result pair
    tail_start = find_safe_tail_start(messages, tail_start);

    CompactionOutcome::NeedsSummary(CompactionPlan {
        middle: messages[1..tail_start].to_vec(),
        tail_start,
        before,
    })
}

fn json_str_field(raw: &str, field: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    v.get(field)?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn json_todos(raw: &str) -> Vec<String> {
    let v: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let Some(arr) = v.get("todos").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|t| {
            let status = t
                .get("status")
                .and_then(|s| s.as_str())
                .unwrap_or("pending");
            if status == "completed" || status == "complete" || status == "done" {
                return None;
            }
            t.get("content")
                .and_then(|c| c.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .collect()
}

fn section_after<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let rest = text.split_once(heading)?.1;
    let rest = rest.trim_start_matches('\n');
    let end = rest.find("\n--- ").unwrap_or(rest.len());
    let body = rest[..end].trim();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

fn push_unique_path(paths: &mut Vec<String>, path: String) {
    if path.is_empty() || path.contains("..") {
        return;
    }
    if !paths.iter().any(|p| p == &path) {
        paths.push(path);
    }
}

fn first_exit_token(output: &str) -> Option<&str> {
    output.split_whitespace().find(|t| t.starts_with("exit="))
}

fn extract_facts(system: Option<&ChatMessage>, middle: &[ChatMessage]) -> CompactFacts {
    let mut facts = CompactFacts {
        middle_len: middle.len(),
        ..CompactFacts::default()
    };

    if let Some(sys) = system.and_then(|m| m.content.as_deref()) {
        if let Some(g) = section_after(sys, "--- Current goal ---") {
            facts.goal = Some(g.lines().next().unwrap_or(g).to_string());
        }
        if let Some(block) = section_after(sys, "--- Task list ---") {
            facts.open_todos = block
                .lines()
                .filter(|l| l.starts_with("[pending]") || l.starts_with("[in_progress]"))
                .map(|l| {
                    l.trim_start_matches("[pending]")
                        .trim_start_matches("[in_progress]")
                        .trim()
                        .to_string()
                })
                .filter(|s| !s.is_empty())
                .collect();
        }
    }

    let mut pending_verify: Vec<Option<String>> = Vec::new();
    for msg in middle {
        if let Some(tcs) = &msg.tool_calls {
            for tc in tcs {
                match tc.function.name.as_str() {
                    "goal_set" => {
                        if let Some(d) = json_str_field(&tc.function.arguments, "description") {
                            facts.goal = Some(d);
                        }
                    }
                    "todo_write" => {
                        let open = json_todos(&tc.function.arguments);
                        if !open.is_empty() {
                            facts.open_todos = open;
                        }
                    }
                    "read_file" | "write_file" | "search_replace" | "list_dir" | "grep" => {
                        if let Some(p) = json_str_field(&tc.function.arguments, "path") {
                            push_unique_path(&mut facts.key_paths, p);
                        }
                    }
                    _ => {}
                }
                let verify_label = match tc.function.name.as_str() {
                    "run_tests" | "run_lint" => Some(tc.function.name.clone()),
                    _ => None,
                };
                pending_verify.push(verify_label);
            }
        }
        if msg.role == "tool" {
            let label = if pending_verify.is_empty() {
                None
            } else {
                pending_verify.remove(0)
            };
            if let Some(name) = label {
                let body = msg.content.as_deref().unwrap_or("");
                let exit = first_exit_token(body).unwrap_or("exit=?");
                facts.last_verification = Some(format!("{name} {exit}"));
            }
        }
    }
    if facts.key_paths.len() > 8 {
        facts.key_paths.truncate(8);
    }
    if facts.open_todos.len() > 8 {
        facts.open_todos.truncate(8);
    }
    facts
}

fn format_facts(facts: &CompactFacts) -> String {
    let mut out = String::new();
    if let Some(g) = &facts.goal {
        out.push_str(&format!("Goal: {}\n", truncate(g, 200)));
    }
    if !facts.open_todos.is_empty() {
        let todos: Vec<String> = facts.open_todos.iter().map(|t| truncate(t, 80)).collect();
        out.push_str(&format!("Open todos: {}\n", todos.join("; ")));
    }
    if !facts.key_paths.is_empty() {
        out.push_str(&format!("Key paths: {}\n", facts.key_paths.join(", ")));
    }
    if let Some(v) = &facts.last_verification {
        out.push_str(&format!("Last verification: {v}\n"));
    }
    out
}

fn compaction_note(facts: &CompactFacts, kind: &str) -> String {
    let mut parts = vec![kind.to_string()];
    if facts.goal.is_some() {
        parts.push("goal kept".into());
    }
    if !facts.open_todos.is_empty() {
        parts.push(format!("{} open todos", facts.open_todos.len()));
    }
    if !facts.key_paths.is_empty() {
        parts.push(format!("{} paths", facts.key_paths.len()));
    }
    if let Some(v) = &facts.last_verification {
        parts.push(format!("last verify: {v}"));
    }
    parts.join(" · ")
}

/// Assemble the compacted history from a plan, replacing `messages` in place.
///
/// `llm_summary` is an optional pre-computed LLM summary of the middle. When
/// `None`, the extractive [`extractive_body`] fallback is used. A structured
/// facts block (goal, open todos, key paths, last verification) is always
/// prepended so those anchors survive even if the LLM summary drops them.
fn assemble_compaction(
    messages: &mut Vec<ChatMessage>,
    plan: CompactionPlan,
    llm_summary: Option<String>,
) -> CompactionReport {
    let facts = extract_facts(messages.first(), &plan.middle);
    let facts_block = format_facts(&facts);
    let body = match llm_summary {
        Some(text) => text,
        None => extractive_body(&plan.middle),
    };
    let mut content = String::from("[Compacted conversation summary]\n");
    if !facts_block.is_empty() {
        content.push_str(&facts_block);
        content.push('\n');
    }
    content.push_str(&body);
    // Hard cap the assembled summary (facts + body).
    const MAX_SUMMARY_CHARS: usize = 4000;
    if content.chars().count() > MAX_SUMMARY_CHARS {
        content = content.chars().take(MAX_SUMMARY_CHARS).collect();
        content.push_str("...[summary truncated]");
    }
    let summary_user = ChatMessage {
        role: "user".into(),
        content: Some(content),
        tool_calls: None,
        tool_call_id: None,
    };
    let summary_assistant = ChatMessage {
        role: "assistant".into(),
        content: Some(
            "[Context compacted — prior conversation summarized above. \
             Continue from the recent messages below.]"
                .into(),
        ),
        tool_calls: None,
        tool_call_id: None,
    };

    // Assemble new history
    let system = messages[0].clone();
    let tail: Vec<ChatMessage> = messages[plan.tail_start..].to_vec();

    let mut compacted = Vec::with_capacity(tail.len() + 3);
    compacted.push(system);
    compacted.push(summary_user);
    compacted.push(summary_assistant);
    compacted.extend(tail);

    let after = history_tokens(&compacted);
    // Compaction must never grow the history. For degenerate histories — many
    // tiny, near-identical messages where the extractive summary's per-line
    // prefixes and newlines cost more than the verbatim middle they replace —
    // the compacted form can be larger than the original. In that case keep the
    // original unchanged rather than make things worse.
    if after >= plan.before {
        return CompactionReport {
            before_tokens: plan.before,
            after_tokens: plan.before,
            note: "compaction skipped (would not shrink)".into(),
        };
    }

    *messages = compacted;
    CompactionReport {
        before_tokens: plan.before,
        after_tokens: after,
        note: compaction_note(&facts, &format!("summarized {} messages", facts.middle_len)),
    }
}

/// LLM-structured compaction: summarize the middle with the model, falling
/// back to the extractive summarizer if the model call fails.
///
/// `summarizer` is an async callable that takes ownership of the middle
/// messages and returns a summary string (or `None` to fall back). Taking the
/// messages by value with a `'static` future avoids tying the closure to the
/// caller's borrows.
pub async fn compact_if_needed_llm(
    messages: &mut Vec<ChatMessage>,
    context_window: usize,
    compact_threshold: f32,
    summarizer: impl FnOnce(
        Vec<ChatMessage>,
    ) -> futures_util::future::BoxFuture<'static, Option<String>>,
) -> Option<CompactionReport> {
    match prepare_compaction(messages, context_window, compact_threshold) {
        CompactionOutcome::None => None,
        // Soft-pruning alone sufficed — no LLM round-trip needed.
        CompactionOutcome::PrunedOnly(before, after) => Some(CompactionReport {
            before_tokens: before,
            after_tokens: after,
            note: "pruned old tool results".into(),
        }),
        CompactionOutcome::NeedsSummary(plan) => {
            // Clone the middle so the extractive fallback still has it if the
            // LLM summarizer returns `None`.
            let summary = summarizer(plan.middle.clone()).await;
            Some(assemble_compaction(messages, plan, summary))
        }
    }
}

/// Extractive fallback body: user asks, assistant actions, tool names.
/// Truncates large tool bodies. The facts header is added by the assembler.
fn extractive_body(middle: &[ChatMessage]) -> String {
    const MAX_BODY_CHARS: usize = 3200;
    const MAX_TOOL_BODY_CHARS: usize = 200;
    let mut summary = String::new();

    for msg in middle {
        match msg.role.as_str() {
            "user" => {
                if let Some(content) = &msg.content {
                    let snippet = truncate(content, 300);
                    summary.push_str(&format!("User asked: {}\n", snippet));
                }
            }
            "assistant" => {
                if let Some(content) = &msg.content {
                    if !content.is_empty() {
                        let snippet = truncate(content, 200);
                        summary.push_str(&format!("Assistant said: {}\n", snippet));
                    }
                }
                if let Some(tool_calls) = &msg.tool_calls {
                    for tc in tool_calls {
                        let args_snippet = truncate(&tc.function.arguments, 150);
                        summary.push_str(&format!(
                            "Assistant called: {}({})\n",
                            tc.function.name, args_snippet
                        ));
                    }
                }
            }
            "tool" => {
                if let Some(content) = &msg.content {
                    let snippet = truncate(content, MAX_TOOL_BODY_CHARS);
                    summary.push_str(&format!("Tool result: {}\n", snippet));
                }
            }
            _ => {}
        }

        if summary.chars().count() > MAX_BODY_CHARS {
            summary.push_str("...[summary truncated]");
            break;
        }
    }
    summary
}

fn truncate(s: &str, max_chars: usize) -> String {
    let chars: String = s.chars().take(max_chars).collect();
    if s.chars().count() > max_chars {
        format!("{}…", chars)
    } else {
        chars
    }
}

/// Soft-prune old tool result messages.
///
/// For `role: "tool"` messages that are older than the last `keep_turns`
/// assistant+tool round-trips, trim the content to head + tail with a
/// truncation marker. This is cheaper than full compaction and preserves
/// the tool-call/result pairing.
fn prune_tool_results(messages: &mut [ChatMessage], keep_turns: usize) {
    const SOFT_TRIM_THRESHOLD: usize = 4000; // only trim if > this many chars
    const HEAD: usize = 1500;
    const TAIL: usize = 1500;

    // Find the boundary: keep the last `keep_turns` tool messages untouched.
    // Count tool messages from the end.
    let mut tool_count = 0;
    let mut boundary = messages.len();
    for i in (0..messages.len()).rev() {
        if messages[i].role == "tool" {
            tool_count += 1;
            if tool_count >= keep_turns {
                boundary = i;
                break;
            }
        }
    }

    // Soft-trim tool results before the boundary
    for msg in messages.iter_mut().take(boundary).skip(1) {
        if msg.role == "tool" {
            if let Some(content) = msg.content.as_mut() {
                let char_count = content.chars().count();
                if char_count > SOFT_TRIM_THRESHOLD {
                    let chars_vec: Vec<char> = content.chars().collect();
                    let head: String = chars_vec.iter().take(HEAD).collect();
                    let tail: String = chars_vec
                        .iter()
                        .rev()
                        .take(TAIL)
                        .copied()
                        .collect::<Vec<char>>()
                        .iter()
                        .rev()
                        .copied()
                        .collect();
                    *content = format!(
                        "{}\n...[tool output trimmed: {} → {} chars]\n{}",
                        head,
                        char_count,
                        HEAD + TAIL,
                        tail
                    );
                }
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{ChatMessage, FunctionCall, ToolCall};
    use crate::tokenizer::{count_tokens as estimate_tokens, MSG_OVERHEAD};

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    fn msg_with_tools(role: &str, content: &str, tool_calls: Vec<ToolCall>) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: if content.is_empty() {
                None
            } else {
                Some(content.into())
            },
            tool_calls: Some(tool_calls),
            tool_call_id: None,
        }
    }

    fn tool_msg(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".into(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(id.into()),
        }
    }

    fn tool_call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            type_: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    // ── estimate_tokens ─────────────────────────────────────────────────

    #[test]
    fn estimate_tokens_empty_is_one() {
        assert_eq!(estimate_tokens(""), 1);
    }

    #[test]
    fn estimate_tokens_nonzero() {
        // "hello world" is 2 tokens in tiktoken (cl100k_base); the estimator
        // is conservative and should never under-count it.
        assert!(estimate_tokens("hello world") >= 2);
    }

    #[test]
    fn estimate_tokens_monotonic() {
        let short = estimate_tokens("a");
        let medium = estimate_tokens("a".repeat(100).as_str());
        let long = estimate_tokens("a".repeat(1000).as_str());
        assert!(short < medium);
        assert!(medium < long);
    }

    // ── message_tokens ──────────────────────────────────────────────────

    #[test]
    fn message_tokens_includes_overhead() {
        let m = msg("user", "hi");
        let tokens = message_tokens(&m);
        assert!(tokens > MSG_OVERHEAD);
    }

    #[test]
    fn message_tokens_includes_tool_calls() {
        let no_tools = msg("assistant", "hello");
        let with_tools = msg_with_tools(
            "assistant",
            "hello",
            vec![tool_call("call_0", "read_file", r#"{"path":"test.rs"}"#)],
        );
        assert!(message_tokens(&with_tools) > message_tokens(&no_tools));
    }

    // ── history_tokens ──────────────────────────────────────────────────

    #[test]
    fn history_tokens_sums_messages() {
        let msgs = vec![
            msg("system", "system prompt"),
            msg("user", "hello"),
            msg("assistant", "hi there"),
        ];
        let total = history_tokens(&msgs);
        let sum: usize = msgs.iter().map(message_tokens).sum();
        assert_eq!(total, sum);
    }

    // ── compact_if_needed_llm ──────────────────────────────────────────

    /// Exercise compaction with the extractive fallback (summarizer returns
    /// `None`), matching the old sync entrypoint's behavior.
    async fn compact_extractive(
        msgs: &mut Vec<ChatMessage>,
        context_window: usize,
        threshold: f32,
    ) -> Option<(usize, usize)> {
        compact_if_needed_llm(msgs, context_window, threshold, |_middle| {
            Box::pin(async { None })
        })
        .await
        .map(|r| (r.before_tokens, r.after_tokens))
    }

    #[tokio::test]
    async fn compaction_not_needed_under_threshold() {
        let mut msgs = vec![msg("system", "sys"), msg("user", "short")];
        let result = compact_extractive(&mut msgs, 128_000, 0.75).await;
        assert!(result.is_none());
        assert_eq!(msgs.len(), 2);
    }

    #[tokio::test]
    async fn golden_below_threshold_history_unchanged() {
        // Golden: when under the soft limit, compaction must leave the history
        // byte-for-byte identical (no pruning, no summary, no reordering).
        let mut msgs = vec![
            msg("system", "sys"),
            msg("user", "hello"),
            msg("assistant", "hi there"),
            msg("user", "how are you"),
            msg("assistant", "fine, thanks"),
        ];
        let before = serde_json::to_string(&msgs).unwrap();
        let result = compact_extractive(&mut msgs, 128_000, 0.75).await;
        assert!(result.is_none(), "no compaction below threshold");
        let after = serde_json::to_string(&msgs).unwrap();
        assert_eq!(after, before, "history must be unchanged below threshold");
    }

    #[tokio::test]
    async fn golden_tool_pair_not_split_at_tail_boundary() {
        // Golden: the tail boundary must never split an assistant tool-call
        // message from its following tool-result message. Construct a history
        // where the naive trailing-budget boundary would land between a tool
        // call and its result, and assert the kept tail keeps them together.
        let mut msgs = vec![msg("system", "sys")];
        // Fill with enough content to force a tail boundary deep in the middle.
        for i in 0..60 {
            msgs.push(msg(
                "user",
                &format!("user message number {i} with some padding"),
            ));
            msgs.push(msg(
                "assistant",
                &format!("assistant response number {i} with padding"),
            ));
        }
        // A tool call + result pair near the end.
        msgs.push(msg_with_tools(
            "assistant",
            "",
            vec![tool_call("call_99", "read_file", r#"{"path":"x.rs"}"#)],
        ));
        msgs.push(tool_msg("call_99", "file contents"));

        compact_extractive(&mut msgs, 8192, 0.1).await.unwrap();

        // Walk the compacted history: every tool result must be immediately
        // preceded by its assistant tool_calls sibling.
        for (i, m) in msgs.iter().enumerate() {
            if m.role == "tool" {
                assert!(
                    i > 0,
                    "tool result at index {i} must have a preceding assistant call"
                );
                let prev = &msgs[i - 1];
                assert_eq!(
                    prev.role, "assistant",
                    "tool result at {i} must follow an assistant message"
                );
                assert!(
                    prev.tool_calls.is_some(),
                    "tool result at {i} must follow an assistant with tool_calls"
                );
            }
        }
    }

    #[tokio::test]
    async fn compaction_preserves_system_message() {
        let mut msgs = vec![msg("system", "important system prompt")];
        for i in 0..100 {
            msgs.push(msg("user", &format!("message {i}")));
            msgs.push(msg("assistant", &format!("response {i}")));
        }
        let result = compact_extractive(&mut msgs, 8192, 0.1).await;
        assert!(result.is_some());
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content.as_deref(), Some("important system prompt"));
    }

    #[tokio::test]
    async fn compaction_reduces_token_count() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..120 {
            msgs.push(msg(
                "user",
                &format!("Refactor the auth module to add scoped permissions for user {i}. Ensure the middleware checks each route."),
            ));
            msgs.push(msg(
                "assistant",
                &format!("Added scoped permissions for user {i} in the auth middleware. Updated the route guards to enforce them."),
            ));
        }
        let (before, after) = compact_extractive(&mut msgs, 8192, 0.1).await.unwrap();
        assert!(
            after < before,
            "after ({after}) should be < before ({before})"
        );
    }

    #[tokio::test]
    async fn compaction_produces_summary_messages() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..120 {
            msgs.push(msg(
                "user",
                &format!("Add unit tests for the payment handler and cover the edge case where balance is {i}."),
            ));
            msgs.push(msg(
                "assistant",
                &format!("Wrote tests for the payment handler covering the zero-balance edge case for account {i}."),
            ));
        }
        compact_extractive(&mut msgs, 8192, 0.1).await.unwrap();
        // After compaction: [system, summary_user, summary_assistant, ...tail]
        assert!(msgs.len() >= 3);
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
        assert!(msgs[1].content.as_deref().unwrap().contains("[Compacted"));
    }

    #[tokio::test]
    async fn compaction_never_grows_history() {
        // Degenerate case: many tiny, near-identical messages where the
        // extractive summary costs as much as (or more than) the verbatim
        // middle it replaces. Compaction must not make the history larger.
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..200 {
            msgs.push(msg("user", &format!("message number {i}")));
            msgs.push(msg("assistant", &format!("response to message {i}")));
        }
        let before_total = history_tokens(&msgs);
        let result = compact_extractive(&mut msgs, 8192, 0.1).await;
        assert!(result.is_some(), "compaction should still be attempted");
        let (before, after) = result.unwrap();
        assert_eq!(before, before_total);
        // The guard must keep the original history when compaction would grow it.
        assert!(
            after <= before,
            "compaction must not grow history: {after} > {before}"
        );
    }

    #[tokio::test]
    async fn compaction_does_not_split_tool_call_result_pair() {
        let mut msgs = vec![msg("system", "sys")];
        // Fill with enough content to trigger compaction
        for _i in 0..40 {
            msgs.push(msg("user", &"x".repeat(200)));
            msgs.push(msg("assistant", &"y".repeat(200)));
        }
        // Add a tool call + result at the end
        msgs.push(msg_with_tools(
            "assistant",
            "",
            vec![tool_call("call_42", "read_file", r#"{"path":"test.rs"}"#)],
        ));
        msgs.push(tool_msg("call_42", "file contents here"));

        compact_extractive(&mut msgs, 8192, 0.3).await.unwrap();

        // Find the tool result — its matching assistant call must precede it
        let tool_idx = msgs.iter().position(|m| m.role == "tool");
        if let Some(idx) = tool_idx {
            // The message before it must be an assistant with tool_calls
            assert!(idx > 0);
            assert_eq!(msgs[idx - 1].role, "assistant");
            assert!(msgs[idx - 1].tool_calls.is_some());
        }
    }

    // ── prune_tool_results ─────────────────────────────────────────────

    #[test]
    fn prune_trims_large_old_tool_results() {
        let big_content = "x".repeat(10_000);
        let mut msgs = vec![
            msg("system", "sys"),
            msg_with_tools("assistant", "", vec![tool_call("c1", "run_shell", "{}")]),
            tool_msg("c1", &big_content),
            msg_with_tools("assistant", "", vec![tool_call("c2", "run_shell", "{}")]),
            tool_msg("c2", &big_content),
            msg_with_tools("assistant", "", vec![tool_call("c3", "run_shell", "{}")]),
            tool_msg("c3", &big_content),
            msg_with_tools("assistant", "", vec![tool_call("c4", "run_shell", "{}")]),
            tool_msg("c4", "recent result"),
        ];
        prune_tool_results(&mut msgs, 3);

        // The first tool result (c1) should be trimmed (it's old)
        let c1 = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c1"))
            .unwrap();
        let c1_content = c1.content.as_ref().unwrap();
        assert!(
            c1_content.contains("[tool output trimmed"),
            "c1 should be trimmed: {}",
            &c1_content[..50]
        );

        // The last tool result (c4) should NOT be trimmed (it's recent)
        let c4 = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c4"))
            .unwrap();
        assert_eq!(c4.content.as_deref(), Some("recent result"));
    }

    #[test]
    fn prune_does_not_trim_small_results() {
        let mut msgs = vec![
            msg("system", "sys"),
            msg_with_tools("assistant", "", vec![tool_call("c1", "read_file", "{}")]),
            tool_msg("c1", "small output"),
            msg_with_tools("assistant", "", vec![tool_call("c2", "read_file", "{}")]),
            tool_msg("c2", "small output 2"),
        ];
        prune_tool_results(&mut msgs, 1);
        // Both should be unchanged (under 4000 char threshold)
        let c1 = msgs
            .iter()
            .find(|m| m.tool_call_id.as_deref() == Some("c1"))
            .unwrap();
        assert_eq!(c1.content.as_deref(), Some("small output"));
    }

    #[tokio::test]
    async fn llm_compaction_uses_provided_summary_and_reduces_tokens() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..200 {
            msgs.push(msg("user", &format!("message number {i}")));
            msgs.push(msg("assistant", &format!("response to message {i}")));
        }
        let report = compact_if_needed_llm(&mut msgs, 8192, 0.1, |_middle| {
            Box::pin(async { Some("LLM distilled: 200 turns of task work.".to_string()) })
        })
        .await
        .unwrap();
        let (before, after) = (report.before_tokens, report.after_tokens);
        assert!(
            after < before,
            "after ({after}) should be < before ({before})"
        );
        // The LLM summary text is present in the assembled history.
        let summary_present = msgs.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("LLM distilled"))
        });
        assert!(summary_present, "LLM summary should be in the history");
        // System message preserved.
        assert_eq!(msgs[0].role, "system");
    }

    #[tokio::test]
    async fn llm_compaction_falls_back_when_summarizer_returns_none() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..120 {
            msgs.push(msg(
                "user",
                &format!("Refactor the auth module to add scoped permissions for user {i}. Ensure the middleware checks each route."),
            ));
            msgs.push(msg(
                "assistant",
                &format!("Added scoped permissions for user {i} in the auth middleware. Updated the route guards to enforce them."),
            ));
        }
        let report = compact_if_needed_llm(&mut msgs, 8192, 0.1, |_middle| {
            Box::pin(async { None }) // summarizer failed → extractive fallback
        })
        .await
        .unwrap();
        let (before, after) = (report.before_tokens, report.after_tokens);
        assert!(
            after < before,
            "fallback should still reduce tokens ({after} < {before})"
        );
        // The extractive summary marker is present.
        let fallback_present = msgs.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.contains("[Compacted conversation summary]"))
        });
        assert!(
            fallback_present,
            "extractive fallback summary should be present"
        );
    }

    #[test]
    fn extract_facts_from_system_goal_and_todos() {
        let sys = msg(
            "system",
            "sys\n--- Current goal ---\n[in_progress] fix the parser\n\n\
             --- Task list ---\n[pending] 1: write tests\n[in_progress] 2: run clippy\n\
             [completed] 3: read the file\n",
        );
        let facts = extract_facts(Some(&sys), &[]);
        assert_eq!(facts.goal.as_deref(), Some("[in_progress] fix the parser"));
        assert_eq!(facts.open_todos.len(), 2);
        assert!(facts.open_todos[0].contains("write tests"));
        assert!(facts.open_todos[1].contains("run clippy"));
    }

    #[test]
    fn extract_facts_from_tool_calls() {
        let middle = vec![
            msg_with_tools(
                "assistant",
                "",
                vec![
                    tool_call("g1", "goal_set", r#"{"description":"ship the feature"}"#),
                    tool_call(
                        "t1",
                        "todo_write",
                        r#"{"todos":[{"content":"edit parser","status":"pending"},{"content":"done bit","status":"completed"}]}"#,
                    ),
                    tool_call("r1", "read_file", r#"{"path":"src/parser.rs"}"#),
                    tool_call(
                        "w1",
                        "write_file",
                        r#"{"path":"src/parser.rs","content":"x"}"#,
                    ),
                    tool_call("v1", "run_tests", "{}"),
                ],
            ),
            tool_msg("g1", "goal set"),
            tool_msg("t1", "todos written"),
            tool_msg("r1", "fn parse() {}"),
            tool_msg("w1", "Wrote src/parser.rs"),
            tool_msg("v1", "--- run_tests (cargo) exit=0 ---\nok"),
        ];
        let facts = extract_facts(None, &middle);
        assert_eq!(facts.goal.as_deref(), Some("ship the feature"));
        assert_eq!(facts.open_todos, vec!["edit parser".to_string()]);
        assert_eq!(facts.key_paths, vec!["src/parser.rs".to_string()]);
        assert_eq!(facts.last_verification.as_deref(), Some("run_tests exit=0"));
    }

    #[tokio::test]
    async fn compaction_summary_includes_structured_facts() {
        let mut msgs = vec![msg(
            "system",
            "sys\n--- Current goal ---\n[in_progress] keep the goal\n",
        )];
        for i in 0..80 {
            msgs.push(msg(
                "user",
                &format!("Refactor module {i} with enough padding to force compaction now."),
            ));
            msgs.push(msg_with_tools(
                "assistant",
                &format!("Working on module {i} with enough padding here too."),
                vec![tool_call(
                    &format!("c{i}"),
                    "read_file",
                    r#"{"path":"src/lib.rs"}"#,
                )],
            ));
            msgs.push(tool_msg(&format!("c{i}"), "pub fn f() {}"));
        }
        compact_extractive(&mut msgs, 8192, 0.1).await.unwrap();
        let summary = msgs[1].content.as_deref().unwrap();
        assert!(
            summary.contains("Goal: [in_progress] keep the goal"),
            "goal should be in summary: {summary}"
        );
        assert!(
            summary.contains("Key paths: src/lib.rs"),
            "key paths should be in summary: {summary}"
        );
    }

    #[tokio::test]
    async fn compaction_report_note_mentions_what_was_kept() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..80 {
            msgs.push(msg(
                "user",
                &format!("Do work item {i} with extra padding so tokens climb quickly here."),
            ));
            msgs.push(msg_with_tools(
                "assistant",
                "",
                vec![
                    tool_call(
                        &format!("w{i}"),
                        "write_file",
                        r#"{"path":"src/main.rs","content":"fn main(){}"}"#,
                    ),
                    tool_call(&format!("v{i}"), "run_tests", "{}"),
                ],
            ));
            msgs.push(tool_msg(&format!("w{i}"), "Wrote"));
            msgs.push(tool_msg(
                &format!("v{i}"),
                "--- run_tests (cargo) exit=0 ---\nok",
            ));
        }
        let report =
            compact_if_needed_llm(&mut msgs, 8192, 0.1, |_middle| Box::pin(async { None }))
                .await
                .unwrap();
        assert!(
            report.note.contains("paths") || report.note.contains("last verify"),
            "note should surface what was kept: {}",
            report.note
        );
    }
}
