//! Structured plan mode.
//!
//! Re-exported from `huginn-proto`.

pub use huginn_proto::{
    advance_step, classify_approval, format_plan, parse_plan, AgentState, Approval, Plan, PlanStep,
    PlanStepStatus, EXECUTE_PROMPT,
};
