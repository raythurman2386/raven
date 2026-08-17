//! Shared agent-event consumer and plan-approval flow for the headless runner.
//!
//! Extracts the event-draining loop and plan-approve-revise-execute flow that
//! were previously inlined in `headless_run`, so the headless path stays
//! readable and the logic is unit-testable offline.

use anyhow::Result;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, ChatMessage};
use crate::config::Settings;
use crate::plan;
use crate::session::{Session, SessionStore};

/// Drain agent events from the channel, printing to stdout.
///
/// Returns the accumulated assistant text.
///
/// Stdout is line-buffered when connected to a TTY and fully block-buffered
/// when piped (e.g. `evals/run.py` with `capture_output=True`). Always flush
/// after tool/iteration markers so a killed/timed-out process still leaves
/// parseable progress for the eval runner.
pub async fn drain_events(rx: &mut mpsc::Receiver<AgentEvent>) -> String {
    let mut assistant_text = String::new();
    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => {
                assistant_text.push_str(&t);
                print!("{}", t);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            AgentEvent::ToolStart { name, args } => {
                println!("\n→ {}({})", name, args);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            AgentEvent::ToolEnd { name, preview } => {
                println!(
                    "  [{}] {}",
                    name,
                    preview.chars().take(300).collect::<String>()
                );
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            AgentEvent::Iteration(n) => {
                eprintln!("[iter {}]", n);
                let _ = std::io::Write::flush(&mut std::io::stderr());
            }
            AgentEvent::Compacted {
                before_tokens,
                after_tokens,
            } => {
                eprintln!(
                    "[compacted context: ~{} → ~{} tokens]",
                    before_tokens, after_tokens
                );
            }
            AgentEvent::Retry { attempt, delay_ms } => {
                eprintln!("[retry {}/3 in {}ms]", attempt, delay_ms);
            }
            AgentEvent::VerifyRequired => {
                eprintln!("[verify required: re-running to enforce run_tests]");
            }
            AgentEvent::AskUser { question, reply } => {
                eprintln!("\n── {question} ──");
                let answer = read_line_if_tty()
                    .map(|l| l.trim().to_string())
                    .unwrap_or_default();
                let _ = reply.send(answer);
            }
            AgentEvent::Done => break,
            AgentEvent::Error(e) => {
                eprintln!("\nError: {}", e);
                break;
            }
            AgentEvent::PlanProgress(plan) => {
                eprintln!("\n{}", plan::format_plan(&plan));
            }
        }
    }
    assistant_text
}

/// Spawn an agent task and drain its events.
///
/// Returns the final messages (if the task completed) and the accumulated
/// assistant text.
pub async fn spawn_and_drain(
    mut agent: Agent,
    prompt: &str,
) -> Result<(Option<Vec<ChatMessage>>, String)> {
    let prompt = prompt.to_string();
    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
    let runner = tokio::spawn(async move {
        agent.run(&prompt, tx).await?;
        Ok::<_, anyhow::Error>(agent.messages)
    });
    let assistant_text = drain_events(&mut rx).await;
    let messages = runner.await.ok().and_then(|r| r.ok());
    Ok((messages, assistant_text))
}

/// Run the plan-approve-revise-execute flow.
///
/// This is the shared logic that was duplicated between the headless runner
/// and the TUI's `handle_plan_response`. It parses the plan from the assistant
/// text, prompts for approval, handles revision, and executes the approved
/// plan. Plan mode always requires human approval before execution.
pub async fn run_plan_flow(
    settings: &Settings,
    assistant_text: &str,
    first_messages: Option<Vec<ChatMessage>>,
    store: &SessionStore,
    session: &mut Session,
    task: &str,
) -> Result<()> {
    let plan = plan::parse_plan(assistant_text);
    println!("\n{}", plan::format_plan(&plan));

    println!("── Approve? [Y]es / [n]o / [r]evise ──");
    match resolve_approval("Approving plan")? {
        Approval::Yes => {}
        Approval::No => {
            println!("Aborted.");
            return Ok(());
        }
        Approval::Revise(feedback) => {
            let feedback_msg = format!("Revise the plan based on this feedback:\n{feedback}");
            let agent =
                Agent::with_messages(settings.clone(), first_messages.clone().unwrap_or_default())?
                    .plan_only();

            let rev_msg = ChatMessage {
                role: "user".into(),
                content: Some(feedback_msg.clone()),
                tool_calls: None,
                tool_call_id: None,
            };
            store.append_message(session, &rev_msg)?;

            let (rev_messages, rev_text) = spawn_and_drain(agent, &feedback_msg).await?;

            if let Some(ref msgs) = rev_messages {
                save_session_messages(store, session, msgs, task)?;
            }

            let revised = plan::parse_plan(&rev_text);
            println!("\n{}", plan::format_plan(&revised));
            println!("── Approve? [Y]es / [n]o ──");
            match resolve_approval("Approving revised plan")? {
                Approval::No | Approval::Revise(_) => {
                    println!("Aborted.");
                    return Ok(());
                }
                Approval::Yes => {
                    let exec_msg = ChatMessage {
                        role: "user".into(),
                        content: Some(plan::EXECUTE_PROMPT.into()),
                        tool_calls: None,
                        tool_call_id: None,
                    };
                    store.append_message(session, &exec_msg)?;
                    let exec_messages = rev_messages.unwrap_or_default();
                    let agent =
                        Agent::with_messages(settings.clone(), exec_messages)?.with_plan(revised);
                    let (final_messages, _) = spawn_and_drain(agent, plan::EXECUTE_PROMPT).await?;
                    if let Some(ref msgs) = final_messages {
                        save_session_messages(store, session, msgs, "Plan execution")?;
                    }
                    println!();
                    return Ok(());
                }
            }
        }
    }

    let exec_messages = first_messages.unwrap_or_default();
    let exec_msg = ChatMessage {
        role: "user".into(),
        content: Some(plan::EXECUTE_PROMPT.into()),
        tool_calls: None,
        tool_call_id: None,
    };
    store.append_message(session, &exec_msg)?;

    let agent = Agent::with_messages(settings.clone(), exec_messages)?.with_plan(plan);
    let (final_messages, _) = spawn_and_drain(agent, plan::EXECUTE_PROMPT).await?;
    if let Some(ref msgs) = final_messages {
        save_session_messages(store, session, msgs, "Plan execution")?;
    }
    println!();
    Ok(())
}

/// Save the agent's final messages to the session.
pub fn save_session_messages(
    store: &SessionStore,
    session: &mut Session,
    messages: &[ChatMessage],
    title_hint: &str,
) -> Result<()> {
    store.save_all_messages(session, messages)?;

    let title = if session.summary.title.is_empty() {
        title_hint.chars().take(80).collect()
    } else {
        session.summary.title.clone()
    };
    store.update_summary(session, Some(title))?;
    store.snapshot_patch(session)?;

    session.messages = messages.to_vec();
    Ok(())
}

/// Whether stdin is an interactive terminal.
pub fn stdin_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

/// Read one line of user input, or return `None` immediately when stdin is not
/// interactive (cron / piped / closed). Never blocks on a non-TTY stdin.
pub fn read_line_if_tty() -> Option<String> {
    if !stdin_is_tty() {
        return None;
    }
    let mut line = String::new();
    let n = std::io::stdin().read_line(&mut line).unwrap_or(0);
    if n == 0 {
        None
    } else {
        Some(line)
    }
}

/// The outcome of an interactive approval prompt.
#[derive(Debug)]
pub enum Approval {
    Yes,
    No,
    Revise(String),
}

/// Classify a raw input line into an [`Approval`]. Pure — no I/O.
pub fn classify_approval(line: &str) -> Approval {
    match line.trim().to_lowercase().as_str() {
        "" | "y" | "yes" | "ok" | "approve" => Approval::Yes,
        "n" | "no" | "abort" | "q" | "quit" => Approval::No,
        other => Approval::Revise(other.to_string()),
    }
}

/// Resolve an interactive yes/no/revise answer to an approval decision.
///
/// When stdin is not interactive, defaults to auto-approve so automation
/// never blocks on a human gate.
pub fn resolve_approval(prompt: &str) -> Result<Approval> {
    match read_line_if_tty() {
        None => {
            eprintln!("{prompt} (auto-approved: non-interactive)");
            Ok(Approval::Yes)
        }
        Some(line) => Ok(classify_approval(&line)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_accepts_common_yes_forms() {
        for s in ["", "y", "Y", "yes", "ok", "approve", "  y  ", "YES\n"] {
            assert!(
                matches!(classify_approval(s), Approval::Yes),
                "expected Yes for {s:?}"
            );
        }
    }

    #[test]
    fn classify_accepts_no_forms() {
        for s in ["n", "no", "abort", "q", "quit", "NO"] {
            assert!(
                matches!(classify_approval(s), Approval::No),
                "expected No for {s:?}"
            );
        }
    }

    #[test]
    fn classify_treats_anything_else_as_revise() {
        match classify_approval("  break this into two steps ") {
            Approval::Revise(fb) => assert_eq!(fb, "break this into two steps"),
            other => panic!("expected Revise, got {other:?}"),
        }
    }
}
