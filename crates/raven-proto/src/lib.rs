//! Shared protocol types for the Raven harness.
//!
//! Chat messages, tool calls, agent events, errors, plans, goals, and
//! todos. Higher crates depend on this one; it depends on no other
//! Raven crate.

mod chat;
mod error;
mod plan;
mod scope;
mod state;
mod time;
mod usage;

pub use chat::{AgentEvent, ChatMessage, FunctionCall, ToolCall};
pub use error::{cap_http_body, AgentError, ToolError};
pub use plan::{
    advance_step, classify_approval, format_plan, parse_plan, AgentState, Approval, Plan, PlanStep,
    PlanStepStatus, EXECUTE_PROMPT,
};
pub use scope::Scope;
pub use state::{Goal, TodoItem, WorkStatus};
pub use time::now_iso_public;
pub use usage::TokenUsage;
