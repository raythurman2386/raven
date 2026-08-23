//! Slash-command framework for the TUI.
//!
//! Slash commands (`/new`, `/help`, ...) are the primary way to drive
//! the interactive UI — unlike Ctrl+letter shortcuts they work identically in
//! an editor-like terminal and are self-discoverable via `/help`.
//!
//! Design: a [`commands()`] registry is the single source of truth for command
//! names, aliases, and help text. [`parse`] extracts a command + raw args from
//! an input line. The TUI dispatches the parsed command by name; [`help_text`]
//! renders the full listing (or one command's detail) from the registry, so
//! adding a command is one registry entry plus one dispatch arm.

/// Metadata for a single slash command.
pub struct CommandSpec {
    /// Canonical name, without the leading `/`.
    pub name: &'static str,
    /// Aliases (also without `/`), e.g. `q` for `quit`.
    pub aliases: &'static [&'static str],
    /// One-line description shown in `/help`.
    pub summary: &'static str,
    /// Argument help, or `None` if the command takes no arguments.
    pub arg_help: Option<&'static str>,
}

impl CommandSpec {
    /// Whether this command matches the given name or one of its aliases.
    fn matches(&self, name: &str) -> bool {
        self.name == name || self.aliases.contains(&name)
    }
}

/// A parsed slash command from an input line.
pub struct ParsedCommand {
    /// The canonical command name (e.g. `plan`), or the raw token if unknown.
    pub name: String,
    /// Everything after the command token, trimmed. Empty if none.
    pub args: String,
}

/// The registry of all available slash commands.
pub fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            name: "help",
            aliases: &["h", "?"],
            summary: "Show this help, or details for one command: /help <cmd>",
            arg_help: Some("[command]"),
        },
        CommandSpec {
            name: "new",
            aliases: &["n"],
            summary: "Save the current session and start a fresh one",
            arg_help: None,
        },
        CommandSpec {
            name: "clear",
            aliases: &["c"],
            summary: "Clear the on-screen log (history is preserved)",
            arg_help: None,
        },
        CommandSpec {
            name: "stop",
            aliases: &["s"],
            summary: "Interrupt the running task",
            arg_help: None,
        },
        CommandSpec {
            name: "model",
            aliases: &["m"],
            summary: "Switch the model for subsequent turns: /model <name>",
            arg_help: Some("<model>"),
        },
        CommandSpec {
            name: "provider",
            aliases: &["p"],
            summary:
                "Switch the provider for subsequent turns: /provider <name> (or /provider to list)",
            arg_help: Some("[name]"),
        },
        CommandSpec {
            name: "quit",
            aliases: &["q", "exit"],
            summary: "Quit Raven",
            arg_help: None,
        },
        CommandSpec {
            name: "undo",
            aliases: &["u"],
            summary: "Undo the last commit, keeping changes in the working tree",
            arg_help: None,
        },
        CommandSpec {
            name: "theme",
            aliases: &["t"],
            summary: "Switch the color theme: /theme <name> (or /theme to list)",
            arg_help: Some("[name]"),
        },
        CommandSpec {
            name: "export",
            aliases: &["x"],
            summary: "Export this session as a local Markdown/JSON bundle",
            arg_help: Some("[dir]"),
        },
    ]
}

/// Parse a raw input line into a [`ParsedCommand`].
///
/// Returns `None` if the line is not a slash command (doesn't start with `/`).
/// The line `/name arg1 arg2` parses to name `name`, args `arg1 arg2`.
pub fn parse(input: &str) -> Option<ParsedCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    // Split off the first whitespace-delimited token as the command name.
    let rest = &trimmed[1..];
    let (token, args) = match rest.find(char::is_whitespace) {
        Some(idx) => (rest[..idx].trim(), rest[idx..].trim()),
        None => (rest.trim(), ""),
    };
    let canonical = commands()
        .iter()
        .find(|c| c.matches(token))
        .map(|c| c.name.to_string());
    let name = canonical.unwrap_or_else(|| token.to_string());
    Some(ParsedCommand {
        name,
        args: args.to_string(),
    })
}

/// Render the full `/help` listing from the registry.
pub fn help_text() -> String {
    let mut out = String::from("Slash commands\n\n");
    for c in commands() {
        let args = c.arg_help.unwrap_or("");
        out.push_str(&format!("/{:<8} {}\n", c.name, c.summary));
        if !c.aliases.is_empty() {
            out.push_str(&format!(
                "           (aliases: {})\n",
                c.aliases
                    .iter()
                    .map(|a| format!("/{a}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !args.is_empty() {
            out.push_str(&format!("           usage: /{} {}\n", c.name, args));
        }
    }
    out
}

/// Render help for a single command, or `None` if unknown.
pub fn command_help(name: &str) -> Option<String> {
    let c = commands().into_iter().find(|c| c.matches(name))?;
    let args = c.arg_help.unwrap_or("");
    let aliases = if c.aliases.is_empty() {
        String::new()
    } else {
        format!(
            "Aliases: {}\n",
            c.aliases
                .iter()
                .map(|a| format!("/{a}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    Some(format!(
        "/{}\n{}\n{}{}",
        c.name,
        c.summary,
        aliases,
        if args.is_empty() {
            String::new()
        } else {
            format!("Usage: /{} {}\n", c.name, args)
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_non_command_returns_none() {
        assert!(parse("just a task").is_none());
        assert!(parse("  spaced text").is_none());
    }

    #[test]
    fn parse_known_command() {
        let pc = parse("/new").unwrap();
        assert_eq!(pc.name, "new");
        assert!(pc.args.is_empty());
    }

    #[test]
    fn parse_command_with_args() {
        let pc = parse("/help  new").unwrap();
        assert_eq!(pc.name, "help");
        assert_eq!(pc.args, "new");
    }

    #[test]
    fn parse_alias_resolves_to_canonical() {
        let pc = parse("/q").unwrap();
        assert_eq!(pc.name, "quit");
    }

    #[test]
    fn model_command_registered_with_alias() {
        let pc = parse("/model deepseek-v4-pro:cloud").unwrap();
        assert_eq!(pc.name, "model");
        assert_eq!(pc.args, "deepseek-v4-pro:cloud");
        // /m is an alias for /model
        assert_eq!(parse("/m").unwrap().name, "model");
        // /help lists it
        assert!(help_text().contains("/model"), "help should list /model");
        // /help model works
        let h = command_help("model").expect("/model has help");
        assert!(h.contains("Switch the model"), "help text: {h}");
    }

    #[test]
    fn stop_command_registered() {
        assert_eq!(parse("/stop").unwrap().name, "stop");
        assert_eq!(parse("/s").unwrap().name, "stop");
        assert!(help_text().contains("/stop"), "help should list /stop");
    }

    #[test]
    fn provider_command_registered() {
        let pc = parse("/provider openrouter").unwrap();
        assert_eq!(pc.name, "provider");
        assert_eq!(pc.args, "openrouter");
        // /p is an alias for /provider
        assert_eq!(parse("/p").unwrap().name, "provider");
        assert!(
            help_text().contains("/provider"),
            "help should list /provider"
        );
        let h = command_help("provider").expect("/provider has help");
        assert!(h.contains("Switch the provider"), "help text: {h}");
    }

    #[test]
    fn undo_command_registered() {
        assert_eq!(parse("/undo").unwrap().name, "undo");
        assert_eq!(parse("/u").unwrap().name, "undo");
        assert!(help_text().contains("/undo"), "help should list /undo");
    }

    #[test]
    fn theme_command_registered() {
        assert_eq!(parse("/theme").unwrap().name, "theme");
        assert_eq!(parse("/t").unwrap().name, "theme");
        assert_eq!(parse("/theme nord").unwrap().args, "nord");
        assert!(help_text().contains("/theme"), "help should list /theme");
    }

    #[test]
    fn export_command_registered() {
        assert_eq!(parse("/export").unwrap().name, "export");
        assert_eq!(parse("/x").unwrap().name, "export");
        assert_eq!(parse("/export /tmp/out").unwrap().args, "/tmp/out");
        assert!(help_text().contains("/export"), "help should list /export");
    }

    #[test]
    fn parse_unknown_command() {
        let pc = parse("/nope arg").unwrap();
        assert_eq!(pc.name, "nope");
        assert_eq!(pc.args, "arg");
    }

    #[test]
    fn parse_just_slash() {
        let pc = parse("/").unwrap();
        assert_eq!(pc.name, "");
    }

    #[test]
    fn registry_has_unique_names_and_no_duplicate_aliases() {
        let cmds = commands();
        // Names are unique.
        for (i, a) in cmds.iter().enumerate() {
            for b in cmds.iter().skip(i + 1) {
                assert_ne!(a.name, b.name, "duplicate command name {}", a.name);
            }
        }
        // Aliases don't collide with names or other aliases.
        let all_names: Vec<&str> = cmds.iter().map(|c| c.name).collect();
        for c in &cmds {
            for alias in c.aliases {
                assert!(
                    !all_names.contains(alias),
                    "alias /{alias} collides with a command name"
                );
            }
        }
    }

    #[test]
    fn help_lists_all_commands() {
        let text = help_text();
        for c in commands() {
            assert!(
                text.contains(&format!("/{}", c.name)),
                "help should list /{}",
                c.name
            );
        }
    }

    #[test]
    fn command_help_known_and_unknown() {
        assert!(command_help("new").is_some());
        assert!(command_help("q").is_some(), "alias should resolve");
        assert!(command_help("does_not_exist").is_none());
    }
}
