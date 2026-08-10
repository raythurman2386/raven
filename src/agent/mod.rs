//! Streaming agent loop + lightweight parallel sub-agents.
//!
//! The [`Agent`] owns the conversation history (`messages`), a [`crate::tools::Sandbox`],
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
//! - Ephemeral reminders are request-only user nudges (`<raven_reminder>`)
//!   and never pollute persisted `self.messages`.
//! - File-mutating tools (`write_file`, `search_replace`, `apply_patch`) run
//!   serially in call order; other tools may run in parallel via
//!   `spawn_blocking`. Tool *results* are always recorded in original
//!   `tool_calls[]` order.
//! - Blank empty turns are stalls (capped retries), not clean finishes.

mod core;
mod loop_control;
mod parallel;
mod stream;
mod tools_exec;
mod types;

#[cfg(test)]
mod tests;

pub use core::Agent;
pub use parallel::{run_parallel, SubAgentReport};
pub use types::{AgentEvent, ChatMessage, FunctionCall, ToolCall};
