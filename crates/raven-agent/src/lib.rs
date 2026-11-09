//! Streaming agent loop + lightweight parallel sub-agents.
//!
//! The [`Agent`] owns the conversation history (`messages`), a [`raven_tools::Sandbox`],
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

pub mod context;

mod core;
mod loop_control;
mod parallel;
mod stream;
#[cfg(test)]
mod tests;
mod title;
mod tools_exec;

pub use core::Agent;
pub use parallel::{delegate_task, run_parallel, SubAgentReport};
pub use raven_proto::{AgentEvent, ChatMessage, FunctionCall, ToolCall};
pub use title::{generate_session_title, is_title_prompt, sanitize_title};

// The shared helpers also serve crates that do not call every function.
#[cfg(test)]
#[allow(dead_code)]
#[path = "../../../src/testutil.rs"]
mod testutil;
