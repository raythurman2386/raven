//! System-administration scope (`raven --system`).
//!
//! An opt-in operational scope in which Raven administers the whole operating
//! system instead of editing files inside a single repo workspace. The sandbox
//! is rooted at `/` (write-everywhere at the Landlock layer), the system prompt
//! is an OS-administration frame (see `agent::core::SYSTEM_SCOPE_BASE`), and
//! `confirm_shell` is forced on so destructive/state-changing commands always
//! require user approval.
//!
//! This module owns the headless runner for the scope; it reuses
//! [`crate::runner::spawn_and_drain_logged`] but deliberately does **not**
//! persist to a repo session store, does not run the plan-approval flow, and
//! does not invoke the repo enforced-verify gate.

use anyhow::Result;

use crate::agent::Agent;
use crate::config::Settings;
use crate::runner;

/// Run a single system-administration task headless and print the result.
///
/// The agent is constructed from `settings` (which must have `scope ==
/// `Scope::System`` and `workspace == "/"`), so the sandbox and system prompt
/// already reflect the OS scope. No session store is touched and no plan /
/// approval flow runs: the task is executed directly and the assistant text is
/// streamed to stdout.
pub async fn run(settings: Settings, task: &str) -> Result<()> {
    println!("Raven (system scope)");
    println!("Model:     {}", settings.model);
    println!("Host:      {}", settings.base_url());
    println!(
        "Auth:      {}",
        if settings.api_key().is_some() {
            "provider API key set (Bearer)"
        } else {
            "none (local / unauthenticated)"
        }
    );
    println!("Context:   {} tokens", settings.context_window);
    println!("System root: /\n");

    let agent = Agent::new(settings)?;

    let _messages = runner::spawn_and_drain_logged(agent, task, None).await?;
    Ok(())
}
