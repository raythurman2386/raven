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
use super::{theme_name, AgentState, Theme, TuiState};

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
        "undo" => {
            let sandbox = crate::tools::Sandbox::new(settings.workspace.clone());
            match sandbox.git_undo() {
                Ok(out) => state.push_system(out),
                Err(e) => state.push_system(format!("undo failed: {e}")),
            }
            state.log_dirty = true;
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
