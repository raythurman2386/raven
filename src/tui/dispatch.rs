//! Slash-command dispatcher for the TUI.
//!
//! [`dispatch_slash_command`] routes a parsed slash command to a TUI action.
//! It lives here — inside the `tui` module tree — because it deeply mutates
//! [`TuiState`] internals and calls the private turn helpers (`begin_agent_turn`,
//! `abort_current_turn`, `reset_session`). Keeping it in this module avoids
//! widening `TuiState` and its fields to `pub(crate)` just for the dispatcher.
//!
//! The command registry (names, aliases, help) stays in `crate::commands`;
//! this module is purely the per-command match.

use anyhow::Result;

use crate::commands::ParsedCommand;
use crate::config::Settings;
use crate::session::{Session, SessionStore};

use super::blocks::BlockKind;
use super::{begin_agent_turn, theme_name, AgentState, Theme, TuiState};

/// Dispatch a parsed slash command, mutating TUI state as needed.
///
/// Returns `Ok(true)` if the command was handled (the input should not be
/// treated as a task or plan response). All user-visible feedback is pushed
/// to the log.
pub(super) async fn dispatch_slash_command(
    state: &mut TuiState,
    pc: &ParsedCommand,
    settings: &mut Settings,
    store: &SessionStore,
    session: &mut Session,
    compact_at: &mut usize,
    config_file: &crate::config::ConfigFile,
) -> Result<bool> {
    match pc.name.as_str() {
        "help" => {
            let text = if pc.args.is_empty() {
                crate::commands::help_text()
            } else {
                crate::commands::command_help(&pc.args)
                    .unwrap_or_else(|| format!("Unknown command: /{}", pc.args))
            };
            state.push_system(text);
            state.log_dirty = true;
        }
        "new" => {
            super::reset_session(state, session, store, settings, "raven")?;
        }
        "clear" => {
            state.blocks.clear();
            state.log_dirty = true;
        }
        "stop" => {
            if state.task_handle.is_some() {
                super::abort_current_turn(state);
                let _ = store.save_all_messages(session, &state.session_messages);
                let _ = store.update_summary(session, None);
                state.push_system("⏹ stopped (partial turn saved)");
                state.log_dirty = true;
            } else {
                state.push_system("nothing running to stop");
                state.log_dirty = true;
            }
            // Drop ask_user oneshot so the agent (if still winding down) sees cancel.
            state.pending_question = None;
            state.pending_question_text = None;
            state.running = false;
            state.agent_state = AgentState::Idle;
            state.status = "ready".into();
            state.assistant_text.clear();
            state.live_tool = None;
            state.turn_tool_count = 0;
        }
        "model" => {
            let name = pc.args.trim();
            if name.is_empty() {
                state.push_system(format!(
                    "current model: {}  (try /model <name>)",
                    settings.model
                ));
                state.log_dirty = true;
            } else {
                settings.model = name.to_string();
                // Match startup behaviour: prefer the live Ollama `/api/show`
                // value, falling back to the name heuristic when unreachable.
                settings.context_window =
                    crate::context::fetch_context_window(&settings.provider, &settings.model).await;
                settings.max_tokens = Settings::derived_max_tokens(settings.context_window);
                *compact_at = ((settings.context_window - settings.context_window / 8) as f32
                    * settings.compact_threshold) as usize;

                // Persist the new model on the session so a resume shows it.
                let _ = store.update_model(session, &settings.model);

                // Refresh the static header blocks (model + context/compact).
                if let Some(BlockKind::System(b)) = state.blocks.get_mut(0) {
                    b.set_text(format!(
                        "raven · {} · {}",
                        settings.model,
                        settings.base_url()
                    ));
                }
                if let Some(BlockKind::System(b)) = state.blocks.get_mut(2) {
                    b.set_text(format!(
                        "context {} · compact ~{}",
                        super::fmt_tokens(settings.context_window as u64),
                        super::fmt_tokens(*compact_at as u64),
                    ));
                }

                state.push_system(format!(
                    "model → {} · context {} · max_tokens {}",
                    settings.model, settings.context_window, settings.max_tokens
                ));
                state.log_dirty = true;
            }
        }
        "provider" => {
            let name = pc.args.trim();
            if name.is_empty() {
                let names = crate::config::known_provider_names(config_file);
                state.push_system(format!(
                    "current provider: {}\navailable: {}",
                    settings.provider.name,
                    names.join(", ")
                ));
                state.log_dirty = true;
            } else if !crate::config::is_known_provider(config_file, name) {
                state.push_system(format!("unknown provider {name:?} — try /provider to list"));
                state.log_dirty = true;
            } else {
                // Re-resolve the provider from config + env. If the current
                // model is the old provider's default (not an explicit
                // /model override), adopt the new provider's default model.
                let old_default = settings.provider.default_model.clone();
                let new_provider =
                    crate::config::resolve_provider(config_file, Some(name.to_string()));
                if settings.model == old_default {
                    settings.model = new_provider.default_model.clone();
                }
                settings.provider = new_provider;
                // Match startup behaviour: prefer the live provider API value,
                // falling back to the name heuristic when unreachable.
                settings.context_window =
                    crate::context::fetch_context_window(&settings.provider, &settings.model).await;
                settings.max_tokens = Settings::derived_max_tokens(settings.context_window);
                *compact_at = ((settings.context_window - settings.context_window / 8) as f32
                    * settings.compact_threshold) as usize;

                // Persist the new model on the session so a resume shows it.
                let _ = store.update_model(session, &settings.model);

                // Refresh the static header blocks (model + context/compact).
                if let Some(BlockKind::System(b)) = state.blocks.get_mut(0) {
                    b.set_text(format!(
                        "raven · {} · {}",
                        settings.model,
                        settings.base_url()
                    ));
                }
                if let Some(BlockKind::System(b)) = state.blocks.get_mut(2) {
                    b.set_text(format!(
                        "context {} · compact ~{}",
                        super::fmt_tokens(settings.context_window as u64),
                        super::fmt_tokens(*compact_at as u64),
                    ));
                }

                state.push_system(format!(
                    "provider → {} · model {} · context {} · max_tokens {}",
                    settings.provider.name,
                    settings.model,
                    settings.context_window,
                    settings.max_tokens
                ));
                state.log_dirty = true;
            }
        }
        "quit" => {
            state.quit = true;
        }
        "export" => {
            let dest = if pc.args.trim().is_empty() {
                store.default_export_dir(session)
            } else {
                let p = std::path::PathBuf::from(pc.args.trim());
                if p.is_absolute() {
                    p
                } else {
                    settings.workspace.join(p)
                }
            };
            let mut snapshot = session.clone();
            if !state.session_messages.is_empty() {
                snapshot.messages = state.session_messages.clone();
            }
            match store.export_bundle(&snapshot, &dest) {
                Ok(path) => state.push_system(format!(
                    "exported session {} → {}",
                    session.summary.id,
                    path.display()
                )),
                Err(e) => state.push_system(format!("export failed: {e}")),
            }
            state.log_dirty = true;
        }
        "retry" => {
            let Some((preload, prompt, read_only)) = state.last_turn.clone() else {
                state.push_system("nothing to retry — send a task first");
                state.log_dirty = true;
                return Ok(true);
            };
            if state.running {
                state.push_system("a turn is already running — /stop it first");
                state.log_dirty = true;
                return Ok(true);
            }
            // Truncate session history back to the last user message
            // (inclusive) so no stale partial assistant/tool output from the
            // failed turn leaks into the retry.
            if let Some(pos) = state
                .session_messages
                .iter()
                .rposition(|m| m.role == "user")
            {
                state.session_messages.truncate(pos + 1);
            }
            // Mirror `start_task`'s running-state setup (the stored preload is
            // history *before* the user message, so Agent::run re-appends it).
            state.running = true;
            state.status = "running…".into();
            state.messages_dirty = true;
            state.assistant_text.clear();
            begin_agent_turn(state, settings.clone(), preload, prompt, move |agent| {
                if read_only {
                    agent.plan_only()
                } else {
                    agent
                }
            });
        }
        "loop" => {
            let n = pc.args.trim();
            if n.is_empty() {
                state.push_system(format!(
                    "max iterations: {}  (try /loop <N>)",
                    settings.max_iterations
                ));
                state.log_dirty = true;
            } else {
                match n.parse::<usize>() {
                    Ok(v) if v >= 1 => {
                        settings.max_iterations = v;
                        state.iterations_max = v;
                        state.push_system(format!("max iterations → {v} (applies to new turns)"));
                        state.log_dirty = true;
                    }
                    _ => {
                        state.push_system(format!(
                            "invalid iteration count: {n:?}  (expected a positive integer)"
                        ));
                        state.log_dirty = true;
                    }
                }
            }
        }
        "steer" => {
            let msg = pc.args.trim();
            if msg.is_empty() {
                state.push_system("/steer <message>");
                state.log_dirty = true;
                return Ok(true);
            }
            // Running turn: queue the direction into the agent's steering
            // channel. It lands at the next iteration boundary as a `[steer]`
            // user message — no abort, no restart, no lost in-flight work.
            if state.running {
                let Some(tx) = state.steer_tx.clone() else {
                    state.push_system("no steerable turn — send a task first");
                    state.log_dirty = true;
                    return Ok(true);
                };
                let _ = tx.send(msg.to_string());
                state.push_user(msg.to_string());
                state.push_system(format!("→ steered: {msg}"));
                state.log_dirty = true;
                return Ok(true);
            }
            // Idle: re-fire the last turn with the direction appended,
            // preserving prior context so the model picks up mid-task.
            let Some((preload, prompt, read_only)) = state.last_turn.clone() else {
                state.push_system("nothing to steer — send a task first");
                state.log_dirty = true;
                return Ok(true);
            };
            let steer_prompt = format!("{prompt}\n\n[steer] {msg}");
            state.running = true;
            state.status = "running…".into();
            state.messages_dirty = true;
            state.assistant_text.clear();
            begin_agent_turn(
                state,
                settings.clone(),
                preload,
                steer_prompt,
                move |agent| {
                    if read_only {
                        agent.plan_only()
                    } else {
                        agent
                    }
                },
            );
        }
        "cleanup" => {
            let confirm = pc.args.split_whitespace().any(|t| t == "--yes");
            let days_str = pc
                .args
                .split_whitespace()
                .find(|t| *t != "--yes")
                .unwrap_or("");
            let days: usize = match days_str.parse() {
                Ok(v) if v >= 1 => v,
                _ => {
                    state.push_system(
                        "usage: /cleanup <days> [--yes]  (days must be a positive integer)",
                    );
                    state.log_dirty = true;
                    return Ok(true);
                }
            };
            let cutoff = date_minus_days(crate::session::now_iso_public().as_str(), days);
            let current_id = session.summary.id.clone();
            let mut stale = Vec::new();
            match store.list() {
                Ok(metas) => {
                    for m in metas {
                        if m.id == current_id {
                            continue;
                        }
                        let mdate = &m.updated_at[..m.updated_at.len().min(10)];
                        if mdate.as_bytes() < cutoff.as_bytes() {
                            stale.push(m);
                        }
                    }
                }
                Err(e) => {
                    state.push_system(format!("cleanup failed: {e}"));
                    state.log_dirty = true;
                    return Ok(true);
                }
            }
            if stale.is_empty() {
                state.push_system(format!("no sessions older than {days}d"));
                state.log_dirty = true;
                return Ok(true);
            }
            if !confirm {
                state.push_system(format!(
                    "{} session(s) older than {days}d (cutoff {cutoff}):\n{}\nRe-run with --yes to delete.",
                    stale.len(),
                    stale
                        .iter()
                        .map(|m| format!("  {}  (updated {})", m.id, m.updated_at))
                        .collect::<Vec<_>>()
                        .join("\n"),
                ));
                state.log_dirty = true;
                return Ok(true);
            }
            let mut deleted = 0usize;
            let mut failed = Vec::new();
            for m in &stale {
                match store.delete(&m.id) {
                    Ok(()) => deleted += 1,
                    Err(e) => failed.push(format!("{}: {e}", m.id)),
                }
            }
            let mut msg = format!("deleted {deleted} session(s)");
            if !failed.is_empty() {
                msg.push_str(&format!("\nfailed: {}", failed.join("; ")));
            }
            state.push_system(msg);
            state.log_dirty = true;
        }
        "theme" => {
            let name = pc.args.trim();
            if name.is_empty() {
                // List available themes.
                let names: Vec<&str> = Theme::all().iter().map(|(n, _)| *n).collect();
                state.push_system(format!(
                    "themes: {}  (current: {})  ·  /theme <name>",
                    names.join(", "),
                    theme_name(state.theme)
                ));
                state.log_dirty = true;
            } else if let Some(t) = Theme::by_name(name) {
                state.theme = t;
                // Force a full re-render so the whole scrollback recolors.
                state.push_system(format!("theme → {}", theme_name(t)));
                state.log_dirty = true;
            } else {
                state.push_system(format!("unknown theme: {name}  (try /theme to list)"));
                state.log_dirty = true;
            }
        }
        _ => {
            state.push_system(format!("Unknown command: /{}  (try /help)", pc.name));
            state.log_dirty = true;
        }
    }
    Ok(true)
}

/// Return the ISO date prefix `YYYY-MM-DD` for `days` days before the given
/// `now_iso` timestamp (UTC). `now_iso` is `YYYY-MM-DDTHH:MM:SS`; only the
/// date part is used and returned. Handles month/year rollover without a
/// date library.
pub(super) fn date_minus_days(now_iso: &str, days: usize) -> String {
    // Parse the leading date components (day-granularity; ignore time).
    let date = &now_iso[..now_iso.len().min(10)];
    let (y, m, d) = (
        date[0..4].parse::<i64>().unwrap_or(1970),
        date[5..7].parse::<i64>().unwrap_or(1),
        date[8..10].parse::<i64>().unwrap_or(1),
    );

    // Convert to a serial day number and subtract.
    let serial = days_from_civil(y, m, d) - days as i64;
    let (ny, nm, nd) = civil_from_days(serial);

    format!("{ny:04}-{nm:02}-{nd:02}")
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
