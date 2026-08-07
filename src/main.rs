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

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;

use raven::agent::{run_parallel, Agent, ChatMessage};
use raven::config::{
    default_api_key, default_base_url, default_max_iter, default_model, env_compact_threshold,
    env_context_window, load_config_file, Mode, Settings,
};
use raven::context::{fetch_context_window, infer_context_window};
use raven::runner;
use raven::session::{Session, SessionStore};

/// CLI value for the `--mode` flag.
#[derive(clap::ValueEnum, Clone, Copy, Debug)]
enum ModeArg {
    Plan,
    Agent,
    Chat,
}

impl From<ModeArg> for Mode {
    fn from(m: ModeArg) -> Mode {
        match m {
            ModeArg::Plan => Mode::Plan,
            ModeArg::Agent => Mode::Agent,
            ModeArg::Chat => Mode::Chat,
        }
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "raven",
    version,
    about = "Privacy-first local coding-agent harness for Ollama / OpenAI-compatible endpoints"
)]
struct Cli {
    /// Task description (positional args, joined with spaces).
    ///
    /// Only the first word is captured when unquoted — `raven add a test`
    /// captures just `add` (the rest are parsed as flags/unknown args).
    /// Use quotes (`raven "add a test"`) or `-p` (`raven -p "add a test"`)
    /// for multi-word tasks.
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

    /// Interaction mode: plan (default), agent, or chat
    #[arg(long, value_enum)]
    mode: Option<ModeArg>,

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

    /// Disable the enforced verification gate (agent must run tests after edits).
    #[arg(long)]
    no_verify: bool,

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
    let mode = cli.mode.map(Mode::from).or(cfg.mode).unwrap_or(Mode::Plan);
    let temperature = cfg.temperature.unwrap_or(0.2);

    let settings = Settings {
        model,
        base_url,
        api_key,
        workspace,
        max_iterations,
        mode,
        yolo: cli.yolo,
        temperature,
        max_tokens,
        rules: cli.rules,
        context_window,
        compact_threshold,
        no_stream: cli.no_stream || cfg.no_stream.unwrap_or(false),
        verify: !cli.no_verify && cfg.verify.unwrap_or(true),
        confirm_shell: !cli.yolo,
    };

    if let Some(tasks) = cli.parallel {
        println!("Running {} parallel sub-agents…", tasks.len());
        let reports = run_parallel(&settings, tasks).await?;
        for r in &reports {
            println!(
                "\n══ Sub-agent {} ══ ({:.1}s)\n{}\n",
                r.index,
                r.elapsed.as_secs_f64(),
                r.text
            );
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
        return raven::tui::run_tui(settings, resume_session).await;
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
/// When `settings.mode` is [`Mode::Plan`] (and not `--yolo`), the first run
/// produces a plan, then prompts for `[Y/n]` approval on stdin before
/// executing it. Approval is accepted for empty input, `y`, `yes`, or `ok`.
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

    let mut session = if let Some(s) = resume_session {
        s
    } else {
        store.create(&settings.model)?
    };

    let user_msg = ChatMessage {
        role: "user".into(),
        content: Some(task.to_string()),
        tool_calls: None,
        tool_call_id: None,
    };
    store.append_message(&session, &user_msg)?;

    // Plan mode runs the plan-approval flow; Chat mode uses the read-only
    // toolset but skips the plan step. `--yolo` disables the plan flow.
    let plan_first = settings.mode.plans_first() && !settings.yolo;
    let read_only = settings.mode.read_only();

    let mut agent = if session.messages.is_empty() {
        Agent::new(settings.clone())?
    } else {
        Agent::with_messages(settings.clone(), session.messages.clone())?
    };
    if read_only {
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

    let (first_messages, assistant_text, plan_ready) =
        runner::spawn_and_drain(agent, &prompt, plan_first).await?;

    if let Some(ref final_messages) = first_messages {
        runner::save_session_messages(&store, &mut session, final_messages, task)?;
    }

    if plan_first {
        runner::run_plan_flow(
            &settings,
            &assistant_text,
            plan_ready,
            first_messages,
            &store,
            &mut session,
            task,
        )
        .await?;
    }

    Ok(())
}
