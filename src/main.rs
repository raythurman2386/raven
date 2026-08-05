//! # Raven
//!
//! Privacy-first local coding-agent harness for [Ollama](https://ollama.com)
//! and any OpenAI-compatible `/v1/chat/completions` endpoint.
//!
//! Inspired by the agent-harness ideas in xAI's [Grok Build](https://github.com/xai-org/grok-build);
//! not affiliated.
//!
//! One binary, no cloud auth, no MCP marketplace, no kernel sandbox — just a
//! streaming agent loop with tools, plan mode, context compaction, and parallel
//! sub-agents, talking to any OpenAI-compatible `/v1/chat/completions` endpoint
//! (local Ollama by default, Ollama Cloud with a Bearer token optionally).
//!
//! ## Modules
//!
//! | Module        | Responsibility                                      |
//! |---------------|-----------------------------------------------------|
//! | [`config`]    | `Settings`, env defaults, config file loading       |
//! | [`commands`]  | Slash-command registry + parsing for the TUI        |
//! | [`agent`]     | Streaming loop, `AgentEvent`, parallel sub-agents   |
//! | [`context`]   | Token estimation, compaction, context-window query  |
//! | [`tools`]     | `Sandbox`, tool implementations, `dispatch`         |
//! | [`tui`]       | ratatui interactive UI                              |
//! | [`error`]     | Typed error enum with retry classification          |
//! | [`memory`]    | Project memory (MEMORY.md) loading + update tool     |
//! | [`plan`]      | Structured plan data model + parsing                |
//! | [`session`]   | Session persistence (JSONL)                          |
//! | [`tokenizer`] | BPE-like token counting                              |
//!
//! See the repository `README.md` for user-facing documentation and
//! `docs/architecture.md` for design details.

mod agent;
mod commands;
mod config;
mod context;
mod error;
mod memory;
mod plan;
mod session;
mod tokenizer;
mod tools;
mod tui;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::mpsc;

use agent::{run_parallel, Agent, AgentEvent, ChatMessage};
use config::{
    default_api_key, default_base_url, default_max_iter, default_model, env_compact_threshold,
    env_context_window, load_config_file, Settings,
};
use context::{fetch_context_window, infer_context_window};
use session::{Session, SessionStore};

#[derive(Parser, Debug)]
#[command(
    name = "raven",
    version,
    about = "Privacy-first local coding-agent harness for Ollama / OpenAI-compatible endpoints"
)]
struct Cli {
    /// Task description
    task: Vec<String>,

    /// Task prompt (alternative to positional)
    #[arg(short = 'p', long)]
    prompt: Option<String>,

    /// Ollama model name
    #[arg(short, long, default_value_t = default_model())]
    model: String,

    /// Ollama OpenAI-compatible base URL
    #[arg(long, default_value_t = default_base_url())]
    host: String,

    /// Ollama API key (prefer RAVEN_API_KEY or OLLAMA_API_KEY env var). Used for Ollama Cloud / authenticated hosts.
    #[arg(long)]
    api_key: Option<String>,

    /// Working directory
    #[arg(short, long)]
    workspace: Option<PathBuf>,

    /// Skip plan-first mode
    #[arg(long)]
    no_plan: bool,

    /// Skip all confirmations
    #[arg(long)]
    yolo: bool,

    /// Force TUI
    #[arg(long)]
    tui: bool,

    /// Force headless even with no task
    #[arg(long)]
    headless: bool,

    /// Run multiple focused sub-agents in parallel
    #[arg(long, num_args = 1..)]
    parallel: Option<Vec<String>>,

    /// Append extra rules to the system prompt for this session
    #[arg(long)]
    rules: Option<String>,

    /// Override the model's context window size (tokens). Auto-inferred from model name when absent.
    /// Env: RAVEN_CONTEXT_WINDOW (or legacy OG_CONTEXT_WINDOW).
    #[arg(long, env = "RAVEN_CONTEXT_WINDOW")]
    context_window: Option<usize>,

    /// Fraction of the context window at which compaction triggers (0.0–1.0, default 0.75).
    /// Env: RAVEN_COMPACT_THRESHOLD (or legacy OG_COMPACT_THRESHOLD).
    #[arg(long, env = "RAVEN_COMPACT_THRESHOLD")]
    compact_threshold: Option<f32>,

    /// Disable streaming and use a single non-streaming request instead.
    #[arg(long)]
    no_stream: bool,

    /// Resume the most recent session (or a specific session by ID).
    #[arg(long, num_args = 0..=1)]
    resume: Option<Option<String>>,

    /// List saved sessions for this workspace.
    #[arg(long)]
    list_sessions: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Default to `warn` when RUST_LOG is unset. EnvFilter::from_default_env()
    // yields an EMPTY filter (suppressing even WARN/ERROR) when the var is
    // missing, which silently dropped config-parse and session warnings.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    let workspace = cli
        .workspace
        .map(|p| p.canonicalize().unwrap_or(p))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    // Load config file (workspace .raven/config.toml overrides ~/.raven/config.toml)
    let cfg = load_config_file(&workspace);

    let api_key = cli
        .api_key
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(default_api_key);

    // Model: CLI > env (handled by clap default) > config file > built-in default
    let model = if cli.model != default_model() {
        cli.model
    } else if let Some(m) = cfg.model {
        m
    } else {
        cli.model
    };

    // Host: CLI > env (handled by clap) > config file > built-in default
    let base_url = if cli.host != default_base_url() {
        cli.host
    } else if let Some(h) = cfg.host {
        h
    } else {
        cli.host
    };

    let context_window = cli
        .context_window
        .or_else(env_context_window)
        .or(cfg.context_window)
        .unwrap_or_else(|| infer_context_window(&model));

    // If no explicit override, try the live Ollama API for the real value
    let context_window = if cli.context_window.is_none()
        && env_context_window().is_none()
        && cfg.context_window.is_none()
    {
        fetch_context_window(&base_url, &model).await
    } else {
        context_window
    };
    let compact_threshold = cli
        .compact_threshold
        .or_else(env_compact_threshold)
        .or(cfg.compact_threshold)
        .unwrap_or(0.75);
    let max_tokens = Settings::derived_max_tokens(context_window);

    let max_iterations = cfg.max_iterations.unwrap_or_else(default_max_iter);
    let plan_first = cfg.plan_first.unwrap_or(true) && !cli.no_plan;
    let temperature = cfg.temperature.unwrap_or(0.2);

    let settings = Settings {
        model,
        base_url,
        api_key,
        workspace,
        max_iterations,
        plan_first,
        yolo: cli.yolo,
        temperature,
        max_tokens,
        rules: cli.rules,
        context_window,
        compact_threshold,
        no_stream: cli.no_stream || cfg.no_stream.unwrap_or(false),
    };

    if let Some(tasks) = cli.parallel {
        println!("Running {} parallel sub-agents…", tasks.len());
        let reports = run_parallel(&settings, tasks).await?;
        for (i, r) in reports.iter().enumerate() {
            println!("\n══ Sub-agent {} ══\n{}\n", i, r);
        }
        return Ok(());
    }

    // ── Session management ─────────────────────────────────────────────

    let store = SessionStore::for_workspace(&settings.workspace)?;

    // --list-sessions: print sessions and exit
    if cli.list_sessions {
        let sessions = store.list()?;
        if sessions.is_empty() {
            println!("No saved sessions in this workspace.");
        } else {
            println!("Sessions in {}:\n", settings.workspace.display());
            for m in &sessions {
                println!("  {}  {}  [{}]", m.updated_at, m.id, m.model);
                if !m.title.is_empty() {
                    println!("    {}", m.title);
                }
            }
        }
        return Ok(());
    }

    // --resume [id]: load a session
    let resume_session: Option<Session> = if let Some(maybe_id) = &cli.resume {
        let session = if let Some(id) = maybe_id {
            match store.load(id) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to load session {}: {}", id, e);
                    return Ok(());
                }
            }
        } else {
            match store.latest()? {
                Some(s) => s,
                None => {
                    eprintln!("No sessions to resume in this workspace.");
                    return Ok(());
                }
            }
        };
        println!(
            "Resumed session {} ({} messages)\n",
            session.summary.id,
            session.messages.len()
        );
        Some(session)
    } else {
        None
    };

    let task = cli.prompt.unwrap_or_else(|| cli.task.join(" "));

    // Default to TUI when no task given
    if (task.is_empty() && !cli.headless) || cli.tui {
        return tui::run_tui(settings).await;
    }

    if task.is_empty() {
        eprintln!("No task provided. Pass a prompt or run without args for TUI.");
        std::process::exit(1);
    }

    headless_run(settings, &task, resume_session, store).await
}

/// Run a single task in headless (non-interactive) mode.
///
/// Prints agent text deltas, tool calls, and iteration markers to stdout.
/// When `settings.plan_first` is on (and not `--yolo`), the first run produces
/// a plan, then prompts for `[Y/n]` approval on stdin before executing it.
/// Approval is accepted for empty input, `y`, `yes`, or `ok`.
async fn headless_run(
    settings: Settings,
    task: &str,
    resume_session: Option<Session>,
    store: SessionStore,
) -> Result<()> {
    let compact_at = ((settings.context_window - settings.context_window / 8) as f32
        * settings.compact_threshold) as usize;

    println!("Raven (headless)");
    println!("Model:     {}", settings.model);
    println!("Host:      {}", settings.base_url);
    println!(
        "Auth:      {}",
        if settings.api_key.is_some() {
            "RAVEN_API_KEY / OLLAMA_API_KEY set (Bearer)"
        } else {
            "none (local / unauthenticated)"
        }
    );
    println!(
        "Context:   {} tokens (compact @ ~{})",
        settings.context_window, compact_at
    );
    println!("Workspace: {}\n", settings.workspace.display());

    // Create or resume session
    let mut session = if let Some(s) = resume_session {
        s
    } else {
        store.create(&settings.model)?
    };

    // Save the user's prompt as a message to the session
    let user_msg = ChatMessage {
        role: "user".into(),
        content: Some(task.to_string()),
        tool_calls: None,
        tool_call_id: None,
    };
    store.append_message(&session, &user_msg)?;

    let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);

    let plan_first = settings.plan_first && !settings.yolo;

    // Create agent (with preloaded messages if resuming). When this turn is a
    // plan proposal, restrict it to the read-only toolset so it can gather
    // context but cannot modify the workspace before approval.
    let mut agent = if session.messages.is_empty() {
        Agent::new(settings.clone())?
    } else {
        Agent::with_messages(settings.clone(), session.messages.clone())?
    };
    if plan_first {
        agent = agent.plan_only();
    }

    let prompt = if plan_first {
        format!(
            "{}\n\nFirst propose a concise step-by-step plan. You may use read-only tools (list_dir, read_file, grep, search_code, git_status, git_diff, git_log) to inspect the workspace, but you CANNOT edit files or run shell until the plan is approved. Just list the numbered steps.",
            task
        )
    } else {
        task.to_string()
    };

    // Collect assistant text for plan parsing
    let mut assistant_text = String::new();
    // True when the plan-proposal turn ended via exit_plan_mode (PlanReady),
    // meaning the model signalled completion and we should auto-execute.
    let mut plan_ready = false;

    let runner = tokio::spawn(async move {
        agent.run(&prompt, tx).await?;
        Ok::<_, anyhow::Error>(agent.messages)
    });

    while let Some(ev) = rx.recv().await {
        match ev {
            AgentEvent::TextDelta(t) => {
                assistant_text.push_str(&t);
                print!("{}", t);
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            AgentEvent::ToolStart { name, args } => {
                println!("\n→ {}({})", name, args);
            }
            AgentEvent::ToolEnd { name, preview } => {
                println!(
                    "  [{}] {}",
                    name,
                    preview.chars().take(300).collect::<String>()
                );
            }
            AgentEvent::Iteration(n) => {
                eprintln!("[iter {}]", n);
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
            AgentEvent::PlanReady => {
                plan_ready = true;
                break;
            }
            AgentEvent::AskUser { question, reply } => {
                // Headless: print the question and read a line from stdin,
                // then send the answer back so the agent can continue. If the
                // channel is closed on our side (user hit EOF), the agent
                // treats it as "no answer".
                eprintln!("\n── {question} ──");
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                let answer = line.trim().to_string();
                let _ = reply.send(answer);
            }
            AgentEvent::Done => break,
            AgentEvent::Error(e) => {
                eprintln!("\nError: {}", e);
                break;
            }
        }
    }
    let first_messages = runner.await.ok().and_then(|r| r.ok());
    if let Some(ref final_messages) = first_messages {
        save_session_messages(&store, &mut session, final_messages, task)?;
    }

    // ── Plan approval flow ─────────────────────────────────────────────
    if plan_first {
        let plan = plan::parse_plan(&assistant_text);
        println!("\n{}", plan::format_plan(&plan));

        // Model-driven: if the plan turn ended via exit_plan_mode, auto-proceed
        // to execution without a human gate. Otherwise prompt.
        if !plan_ready {
            println!("── Approve? [Y]es / [n]o / [r]evise ──");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            let low = line.trim().to_lowercase();
            match low.as_str() {
                "" | "y" | "yes" | "ok" | "approve" => {
                    // Approved — fall through to execution.
                }
                "n" | "no" | "abort" | "q" | "quit" => {
                    println!("Aborted.");
                    return Ok(());
                }
                _ => {
                    // Revise — send feedback as a new user message
                    let feedback =
                        format!("Revise the plan based on this feedback:\n{}", line.trim());
                    let mut agent = Agent::with_messages(
                        settings.clone(),
                        first_messages.clone().unwrap_or_default(),
                    )?
                    .plan_only();

                    // Save the revision prompt
                    let rev_msg = ChatMessage {
                        role: "user".into(),
                        content: Some(feedback.clone()),
                        tool_calls: None,
                        tool_call_id: None,
                    };
                    store.append_message(&session, &rev_msg)?;

                    let (tx2, mut rx2) = mpsc::channel::<AgentEvent>(64);
                    let mut rev_text = String::new();
                    let mut rev_ready = false;
                    let runner2 = tokio::spawn(async move {
                        agent.run(&feedback, tx2).await?;
                        Ok::<_, anyhow::Error>(agent.messages)
                    });
                    while let Some(ev) = rx2.recv().await {
                        match ev {
                            AgentEvent::TextDelta(t) => {
                                rev_text.push_str(&t);
                                print!("{}", t);
                                let _ = std::io::Write::flush(&mut std::io::stdout());
                            }
                            AgentEvent::PlanReady => {
                                rev_ready = true;
                                break;
                            }
                            AgentEvent::Done => break,
                            AgentEvent::Error(e) => {
                                eprintln!("\nError: {}", e);
                                break;
                            }
                            _ => {}
                        }
                    }
                    let rev_messages = runner2.await.ok().and_then(|r| r.ok());
                    if let Some(ref msgs) = rev_messages {
                        save_session_messages(&store, &mut session, msgs, task)?;
                    }
                    // Show the revised plan; auto-proceed if the model signalled
                    // completion via exit_plan_mode, else prompt once more.
                    let revised = plan::parse_plan(&rev_text);
                    println!("\n{}", plan::format_plan(&revised));
                    if !rev_ready {
                        println!("── Approve? [Y]es / [n]o ──");
                        let mut line2 = String::new();
                        std::io::stdin().read_line(&mut line2)?;
                        let low2 = line2.trim().to_lowercase();
                        if !(low2.is_empty() || low2 == "y" || low2 == "yes" || low2 == "ok") {
                            println!("Aborted.");
                            return Ok(());
                        }
                    }
                }
            }
        }

        // Execute the plan — use messages already in memory
        let exec_messages = first_messages.clone().unwrap_or_default();
        let exec_msg = ChatMessage {
            role: "user".into(),
            content: Some(plan::EXECUTE_PROMPT.into()),
            tool_calls: None,
            tool_call_id: None,
        };
        store.append_message(&session, &exec_msg)?;

        let mut agent = Agent::with_messages(settings, exec_messages)?;
        let (tx, mut rx) = mpsc::channel::<AgentEvent>(64);
        let runner = tokio::spawn(async move {
            agent.run(plan::EXECUTE_PROMPT, tx).await?;
            Ok::<_, anyhow::Error>(agent.messages)
        });
        while let Some(ev) = rx.recv().await {
            match ev {
                AgentEvent::TextDelta(t) => {
                    print!("{}", t);
                    let _ = std::io::Write::flush(&mut std::io::stdout());
                }
                AgentEvent::ToolStart { name, args } => {
                    println!("\n→ {}({})", name, args);
                }
                AgentEvent::ToolEnd { name, preview } => {
                    println!(
                        "  [{}] {}",
                        name,
                        preview.chars().take(300).collect::<String>()
                    );
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
                AgentEvent::Done | AgentEvent::Error(_) => break,
                _ => {}
            }
        }
        if let Ok(Ok(final_messages)) = runner.await {
            save_session_messages(&store, &mut session, &final_messages, "Plan execution")?;
        }
        println!();
    }

    Ok(())
}

/// Save the agent's final messages to the session.
fn save_session_messages(
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

    // Update in-memory state
    session.messages = messages.to_vec();
    Ok(())
}
