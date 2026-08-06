//! Structured plan mode — data-driven plan with explicit state machine.
//!
//! Plans are extracted from model output, stored as structured data,
//! and presented to the user for explicit approve/revise/abort.

use serde::{Deserialize, Serialize};

/// The message sent to the model once a plan is approved, to start execution.
///
/// Deliberately explicit: it restates that the plan is approved, forbids
/// proposing *another* plan, and directs the model to start editing. Weak or
/// fast models (e.g. a flash cloud model) will otherwise keep narrating or
/// re-planning instead of touching files.
pub const EXECUTE_PROMPT: &str = "\
The plan above is APPROVED. Do NOT propose or restate another plan, do NOT \
re-iterate steps, do NOT summarize what you are about to do, and do NOT end \
with a plain-text summary of intent. Begin EXECUTING the approved plan now: \
read what you need, then make the file edits / run the commands the plan \
calls for, in order. The turn is not finished until every step is done and \
you have actually made the required edits — if the plan is not yet complete, \
your reply MUST include a tool call (write_file, search_replace, run_shell, \
etc.) that progresses it. Work until the plan is done, then reply with a \
brief summary of what changed.";

/// A structured plan with numbered steps.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub title: String,
    pub steps: Vec<PlanStep>,
    pub created_at: String,
}

/// A single step in a plan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub description: String,
    pub status: PlanStepStatus,
}

/// The status of a plan step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PlanStepStatus {
    Pending,
    InProgress,
    Completed,
    Skipped,
}

/// The agent's current state in the plan flow.
#[derive(Debug, Clone, PartialEq)]

pub enum AgentState {
    /// No task active.
    Idle,
    /// Model is producing a plan (first turn in plan mode).
    Planning,
    /// Plan is ready, waiting for user to approve/revise/abort.
    AwaitingApproval,
    /// Model is executing the approved plan.
    Executing,
}

/// Parse a model's plan response into a structured [`Plan`].
///
/// Tries JSON first (if the model returns structured output), then falls
/// back to parsing a numbered list from the text.
pub fn parse_plan(text: &str) -> Plan {
    // Try JSON parse
    if let Ok(plan) = serde_json::from_str::<Plan>(text) {
        return plan;
    }

    // Look for a JSON block within the text (model may wrap in ```json ... ```)
    if let Some(start) = text.find("```json") {
        let content_start = start + 7;
        if let Some(end_rel) = text[content_start..].find("```") {
            let json_str = &text[content_start..content_start + end_rel];
            if let Ok(plan) = serde_json::from_str::<Plan>(json_str.trim()) {
                return plan;
            }
        }
    }

    // Fall back to numbered list parsing
    parse_numbered_list(text)
}

/// Parse a numbered list from text into a [`Plan`].
///
/// Recognizes patterns like:
///   1. First step
/// 2. Second step
///   - Or bullet points
fn parse_numbered_list(text: &str) -> Plan {
    let mut steps = Vec::new();
    let mut title = String::new();

    for line in text.lines() {
        let trimmed = line.trim();

        // Skip empty lines
        if trimmed.is_empty() {
            continue;
        }

        // Try numbered list: "1. Do something" or "1) Do something"
        let step_text = if let Some(rest) = trimmed.strip_prefix(|c: char| c.is_ascii_digit()) {
            let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit());
            let rest = rest.trim_start_matches(['.', ')', ':']).trim();
            if rest.is_empty() {
                None
            } else {
                Some(rest)
            }
        } else if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            Some(trimmed[2..].trim())
        } else {
            None
        };

        if let Some(step) = step_text {
            steps.push(PlanStep {
                description: step.to_string(),
                status: PlanStepStatus::Pending,
            });
        } else if steps.is_empty() && !trimmed.starts_with('#') {
            // First non-list line becomes the title
            if title.is_empty() {
                title = trimmed.to_string();
            }
        }
    }

    if title.is_empty() {
        title = "Plan".to_string();
    }

    // If no steps were found, treat the whole text as a single description
    if steps.is_empty() {
        steps.push(PlanStep {
            description: text.trim().to_string(),
            status: PlanStepStatus::Pending,
        });
    }

    Plan {
        title,
        steps,
        created_at: crate::session::now_iso_public(),
    }
}

/// Advance plan step statuses as the agent executes.
///
/// - Marks the current step `InProgress`.
/// - If `had_tool_calls` is true, marks the current step `Completed` and
///   advances `current_step` to the next step.
/// - If `done` is true, marks all remaining steps (including the current one)
///   `Completed`.
/// - `current_step` is clamped to `plan.steps.len()`.
pub fn advance_step(plan: &mut Plan, current_step: &mut usize, had_tool_calls: bool, done: bool) {
    if plan.steps.is_empty() {
        return;
    }
    *current_step = (*current_step).min(plan.steps.len());
    if done {
        for step in &mut plan.steps {
            step.status = PlanStepStatus::Completed;
        }
        *current_step = plan.steps.len();
        return;
    }
    if *current_step < plan.steps.len() {
        plan.steps[*current_step].status = PlanStepStatus::InProgress;
    }
    if had_tool_calls && *current_step < plan.steps.len() {
        plan.steps[*current_step].status = PlanStepStatus::Completed;
        *current_step += 1;
        if *current_step < plan.steps.len() {
            plan.steps[*current_step].status = PlanStepStatus::InProgress;
        }
    }
}

/// Format a plan for display (headless or TUI).
pub fn format_plan(plan: &Plan) -> String {
    let mut out = format!("── {} ──\n", plan.title);
    for (i, step) in plan.steps.iter().enumerate() {
        let mark = match step.status {
            PlanStepStatus::Completed => "[x]",
            PlanStepStatus::InProgress => "[~]",
            PlanStepStatus::Skipped => "[-]",
            PlanStepStatus::Pending => "[ ]",
        };
        out.push_str(&format!("  {} {}. {}\n", mark, i + 1, step.description));
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_plan_numbered_list() {
        let text = "Implementation Plan\n1. Read the file\n2. Edit the function\n3. Run tests\n";
        let plan = parse_plan(text);
        assert_eq!(plan.title, "Implementation Plan");
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].description, "Read the file");
        assert_eq!(plan.steps[1].description, "Edit the function");
        assert_eq!(plan.steps[2].description, "Run tests");
        assert_eq!(plan.steps[0].status, PlanStepStatus::Pending);
    }

    #[test]
    fn parse_plan_parenthesis_style() {
        let text = "1) First step\n2) Second step\n";
        let plan = parse_plan(text);
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].description, "First step");
    }

    #[test]
    fn parse_plan_bullet_style() {
        let text = "- First\n- Second\n- Third\n";
        let plan = parse_plan(text);
        assert_eq!(plan.steps.len(), 3);
        assert_eq!(plan.steps[0].description, "First");
    }

    #[test]
    fn parse_plan_json_direct() {
        let json = r#"{"title":"Test","steps":[{"description":"Do X","status":"Pending"}],"created_at":"2026-01-01"}"#;
        let plan = parse_plan(json);
        assert_eq!(plan.title, "Test");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].description, "Do X");
    }

    #[test]
    fn parse_plan_json_in_code_block() {
        let text = r#"Here's my plan:
```json
{"title":"My Plan","steps":[{"description":"Step A","status":"Pending"}],"created_at":""}
```
Done."#;
        let plan = parse_plan(text);
        assert_eq!(plan.title, "My Plan");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].description, "Step A");
    }

    #[test]
    fn parse_plan_no_steps_fallback() {
        let text = "Just some text without any list structure.";
        let plan = parse_plan(text);
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(
            plan.steps[0].description,
            "Just some text without any list structure."
        );
    }

    #[test]
    fn parse_plan_empty_title_defaults_to_plan() {
        let text = "1. Do something\n";
        let plan = parse_plan(text);
        assert_eq!(plan.title, "Plan");
    }

    #[test]
    fn advance_step_single_step_completion() {
        let mut plan = Plan {
            title: "Test".into(),
            steps: vec![PlanStep {
                description: "Do X".into(),
                status: PlanStepStatus::Pending,
            }],
            created_at: "".into(),
        };
        let mut current = 0usize;
        advance_step(&mut plan, &mut current, true, false);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Completed);
        assert_eq!(current, 1);
    }

    #[test]
    fn advance_step_multi_step_progression() {
        let mut plan = Plan {
            title: "Test".into(),
            steps: vec![
                PlanStep {
                    description: "A".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "B".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "C".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
            created_at: "".into(),
        };
        let mut current = 0usize;
        advance_step(&mut plan, &mut current, true, false);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Completed);
        assert_eq!(plan.steps[1].status, PlanStepStatus::InProgress);
        assert_eq!(plan.steps[2].status, PlanStepStatus::Pending);
        assert_eq!(current, 1);
        advance_step(&mut plan, &mut current, true, false);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Completed);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Completed);
        assert_eq!(plan.steps[2].status, PlanStepStatus::InProgress);
        assert_eq!(current, 2);
    }

    #[test]
    fn advance_step_done_marks_all_remaining_completed() {
        let mut plan = Plan {
            title: "Test".into(),
            steps: vec![
                PlanStep {
                    description: "A".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "B".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "C".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
            created_at: "".into(),
        };
        let mut current = 1usize;
        advance_step(&mut plan, &mut current, false, true);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Completed);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Completed);
        assert_eq!(plan.steps[2].status, PlanStepStatus::Completed);
        assert_eq!(current, 3);
    }

    #[test]
    fn advance_step_no_tool_calls_does_not_advance() {
        let mut plan = Plan {
            title: "Test".into(),
            steps: vec![
                PlanStep {
                    description: "A".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "B".into(),
                    status: PlanStepStatus::Pending,
                },
            ],
            created_at: "".into(),
        };
        let mut current = 0usize;
        advance_step(&mut plan, &mut current, false, false);
        assert_eq!(plan.steps[0].status, PlanStepStatus::InProgress);
        assert_eq!(plan.steps[1].status, PlanStepStatus::Pending);
        assert_eq!(current, 0);
    }

    #[test]
    fn advance_step_clamps_at_end() {
        let mut plan = Plan {
            title: "Test".into(),
            steps: vec![PlanStep {
                description: "A".into(),
                status: PlanStepStatus::Pending,
            }],
            created_at: "".into(),
        };
        let mut current = 5usize;
        advance_step(&mut plan, &mut current, true, false);
        assert_eq!(current, 1);
        assert_eq!(plan.steps[0].status, PlanStepStatus::Pending);
    }

    #[test]
    fn advance_step_empty_plan_noop() {
        let mut plan = Plan {
            title: "Test".into(),
            steps: vec![],
            created_at: "".into(),
        };
        let mut current = 0usize;
        advance_step(&mut plan, &mut current, true, false);
        assert_eq!(current, 0);
    }

    #[test]
    fn format_plan_shows_steps_with_markers() {
        let plan = Plan {
            title: "Test Plan".into(),
            steps: vec![
                PlanStep {
                    description: "First".into(),
                    status: PlanStepStatus::Completed,
                },
                PlanStep {
                    description: "Second".into(),
                    status: PlanStepStatus::InProgress,
                },
                PlanStep {
                    description: "Third".into(),
                    status: PlanStepStatus::Pending,
                },
                PlanStep {
                    description: "Fourth".into(),
                    status: PlanStepStatus::Skipped,
                },
            ],
            created_at: "2026-01-01".into(),
        };
        let out = format_plan(&plan);
        assert!(out.contains("Test Plan"));
        assert!(out.contains("[x]"));
        assert!(out.contains("[~]"));
        assert!(out.contains("[ ]"));
        assert!(out.contains("[-]"));
        assert!(out.contains("1. First"));
        assert!(out.contains("2. Second"));
        assert!(out.contains("3. Third"));
        assert!(out.contains("4. Fourth"));
    }
}
