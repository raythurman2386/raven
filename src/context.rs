//! Context compaction logic.
//!
//! Token counting is delegated to [`crate::tokenizer`], which implements a
//! BPE-like tokenizer for accurate estimates. This module handles the
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
///   - gemma4 / gemma3 / qwen2.5 / qwen3 / llama3.1 / llama3.2 / deepseek / codestral → 128_000
///   - llama3 / codellama / "32k" in name                          → 32_768
///   - mistral / "8k" in name                                     → 8_192
///   - fallback                                                   → 32_768
pub fn infer_context_window(model: &str) -> usize {
    let m = model.to_lowercase();
    // Cloud glm (via Ollama) has a 1M-token context.
    if m.contains("glm") && m.contains("cloud") {
        1_000_000
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

/// Fetch the actual context window from Ollama's `/api/show` endpoint.
///
/// The response includes `{architecture}.context_length` in `model_info`.
/// Falls back to [`infer_context_window`] if the API call fails (Ollama
/// not running, model not found, or the key is missing).
pub async fn fetch_context_window(base_url: &str, model: &str) -> usize {
    let show_url = format!(
        "{}/api/show",
        base_url.trim_end_matches('/').trim_end_matches("/v1")
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build();

    let client = match client {
        Ok(c) => c,
        Err(_) => return infer_context_window(model),
    };

    let resp = match client
        .post(&show_url)
        .json(&serde_json::json!({ "model": model }))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return infer_context_window(model),
    };

    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return infer_context_window(model),
    };

    let info = match body.get("model_info") {
        Some(i) => i,
        None => return infer_context_window(model),
    };

    let arch = match info.get("general.architecture").and_then(|a| a.as_str()) {
        Some(a) => a,
        None => return infer_context_window(model),
    };

    let key = format!("{arch}.context_length");
    match info.get(&key).and_then(|v| v.as_u64()) {
        Some(n) if n > 0 => n as usize,
        _ => infer_context_window(model),
    }
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

/// Compact a message history in place, returning `(before_tokens, after_tokens)`.
///
/// Strategy:
///   1. Always keep the system message (index 0).
///   2. Compute a token budget for the trailing window (~40% of context_window).
///   3. Find the largest tail that fits in the trailing budget, respecting
///      tool-call/tool-result pair boundaries.
///   4. Summarize everything between the system message and the tail into a
///      short synthetic user + assistant pair.
///   5. Replace `messages` with: `[system, summary_user, summary_assistant, ...tail]`.
///
/// Returns `None` if compaction is not needed (history is under the soft limit).
pub fn compact_if_needed(
    messages: &mut Vec<ChatMessage>,
    context_window: usize,
    compact_threshold: f32,
) -> Option<(usize, usize)> {
    // Reserve for model output.
    let output_reserve = (context_window / 8).max(1024);
    let soft_limit =
        ((context_window.saturating_sub(output_reserve)) as f32 * compact_threshold) as usize;

    let before = history_tokens(messages);
    if before <= soft_limit {
        return None;
    }

    // Step 1: Soft-prune old tool results (keep head 1500 + tail 1500 chars)
    // for tool messages older than the last 3 turns. This is cheaper than
    // full compaction and may bring us under the limit without summarizing.
    prune_tool_results(messages, 3);
    let after_prune = history_tokens(messages);
    if after_prune <= soft_limit {
        return Some((before, after_prune));
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

    // The middle is everything between system (0) and tail_start
    let middle_end = tail_start;
    let middle = &messages[1..middle_end];

    // Build summary
    let summary_user = build_summary_user(middle);
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
    let tail: Vec<ChatMessage> = messages[tail_start..].to_vec();

    messages.clear();
    messages.push(system);
    messages.push(summary_user);
    messages.push(summary_assistant);
    messages.extend(tail);

    let after = history_tokens(messages);
    Some((before, after))
}

/// Build a synthetic user message summarizing the middle turns.
///
/// Captures: key user asks, assistant actions, tool names used.
/// Truncates large tool bodies. Capped at a reasonable summary size.
fn build_summary_user(middle: &[ChatMessage]) -> ChatMessage {
    const MAX_SUMMARY_CHARS: usize = 4000;
    const MAX_TOOL_BODY_CHARS: usize = 200;

    let mut summary = String::from("[Compacted conversation summary]\n");

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

        // Hard cap on summary growth
        if summary.chars().count() > MAX_SUMMARY_CHARS {
            summary.push_str("...[summary truncated]");
            break;
        }
    }

    ChatMessage {
        role: "user".into(),
        content: Some(summary),
        tool_calls: None,
        tool_call_id: None,
    }
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
        assert!(estimate_tokens("hello world") >= 4);
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

    // ── compact_if_needed ────────────────────────────────────────────────

    #[test]
    fn compaction_not_needed_under_threshold() {
        let mut msgs = vec![msg("system", "sys"), msg("user", "short")];
        let result = compact_if_needed(&mut msgs, 128_000, 0.75);
        assert!(result.is_none());
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn compaction_preserves_system_message() {
        let mut msgs = vec![msg("system", "important system prompt")];
        for i in 0..100 {
            msgs.push(msg("user", &format!("message {}", i)));
            msgs.push(msg("assistant", &format!("response {}", i)));
        }
        let result = compact_if_needed(&mut msgs, 8192, 0.1);
        assert!(result.is_some());
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content.as_deref(), Some("important system prompt"));
    }

    #[test]
    fn compaction_reduces_token_count() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..200 {
            msgs.push(msg("user", &format!("message number {}", i)));
            msgs.push(msg("assistant", &format!("response to message {}", i)));
        }
        let (before, after) = compact_if_needed(&mut msgs, 8192, 0.1).unwrap();
        assert!(
            after < before,
            "after ({}) should be < before ({})",
            after,
            before
        );
    }

    #[test]
    fn compaction_produces_summary_messages() {
        let mut msgs = vec![msg("system", "sys")];
        for i in 0..100 {
            msgs.push(msg("user", &format!("task {}", i)));
            msgs.push(msg("assistant", &format!("did {}", i)));
        }
        compact_if_needed(&mut msgs, 8192, 0.1).unwrap();
        // After compaction: [system, summary_user, summary_assistant, ...tail]
        assert!(msgs.len() >= 3);
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[2].role, "assistant");
        assert!(msgs[1].content.as_deref().unwrap().contains("[Compacted"));
    }

    #[test]
    fn compaction_does_not_split_tool_call_result_pair() {
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

        compact_if_needed(&mut msgs, 8192, 0.3).unwrap();

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
}
