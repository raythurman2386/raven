//! Core data types for the agent: chat messages, tool calls, and events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::plan::Plan;

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

/// Events emitted by [`crate::agent::Agent::run`] over an `mpsc` channel.
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
    /// A `delegate_task` sub-agent reported progress (its iteration number
    /// within its own capped turn). Surfaced so invisible sub-agent work
    /// is no longer silent in the TUI.
    Subagent { iter: usize },
    /// Context was compacted; carries before/after token estimates and a
    /// short "what was compacted" note (goal / todos / paths / last verify).
    Compacted {
        before_tokens: usize,
        after_tokens: usize,
        note: String,
    },
    /// A transient error is being retried after a delay.
    Retry { attempt: usize, delay_ms: u64 },
    /// The turn edited files but did not call `run_tests` before finishing;
    /// the harness is re-running with a recovery reminder (enforced verify).
    VerifyRequired,
    /// A recovery patch was written because work could not be merged.
    RecoveryPatch {
        /// Workspace-relative path (e.g. `.raven/recovery-sub-0.patch`).
        path: String,
        /// Why the patch was written (merge conflict, merge error, …).
        reason: String,
    },
    /// The model asked the user a question mid-task. The consumer must render
    /// it and send the answer back over the included oneshot channel (or drop
    /// the sender to signal "no answer / dismissed").
    AskUser {
        question: String,
        reply: tokio::sync::oneshot::Sender<String>,
    },
    /// A queued mid-turn direction (steering) was injected into the running
    /// turn as a `[steer]` user message.
    Steered(String),
    /// The plan's step statuses have been updated during execution.
    PlanProgress(Plan),
    /// The agent finished normally (no more tool calls).
    Done,
    /// An error occurred (HTTP failure, stream error).
    Error(String),
}
