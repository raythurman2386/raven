//! Slash-command autocomplete for the TUI input box.
//!
//! Pure, unit-testable logic: given the current input text and the set of
//! known command names / argument candidates, produce a list of completion
//! candidates and apply a selected one. Kept dependency-free (no terminal, no
//! state) so it can be tested in isolation.
//!
//! Two completion modes:
//! - **Command name**: when the input is `/` followed by a partial command
//!   token (no space yet), match against each command's name + aliases.
//! - **Argument**: when the input is `/cmd <partial>`, complete the argument
//!   from a per-command candidate list (e.g. `/theme` → theme names).

use crate::commands;

/// A set of completion candidates for the current input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// The full candidate strings (e.g. `/theme`, `/theme nord`).
    pub candidates: Vec<String>,
    /// Index of the currently highlighted candidate.
    pub selected: usize,
    /// The byte range in the input that the candidate replaces.
    pub replace_start: usize,
    pub replace_end: usize,
}

impl Completion {
    /// Cycle the highlight forward (wrapping). Returns the new index.
    pub fn next(&mut self) -> usize {
        if self.candidates.is_empty() {
            return 0;
        }
        self.selected = (self.selected + 1) % self.candidates.len();
        self.selected
    }

    /// Cycle the highlight backward (wrapping). Returns the new index.
    pub fn prev(&mut self) -> usize {
        if self.candidates.is_empty() {
            return 0;
        }
        self.selected = (self.selected + self.candidates.len() - 1) % self.candidates.len();
        self.selected
    }
}

/// Compute completion candidates for the given input.
///
/// `arg_candidates` maps a command name to its argument candidates (e.g.
/// `theme` → `["ravenwood", "nord", ...]`). Returns `None` when the input
/// isn't a slash command or nothing matches.
pub fn candidates_for(
    input: &str,
    arg_candidates: &dyn Fn(&str) -> Vec<String>,
) -> Option<Completion> {
    let trimmed = input.trim_start();
    if !trimmed.starts_with('/') {
        return None;
    }
    // The byte offset of the first non-whitespace char (so we can replace the
    // right span even if the user indented).
    let lead = input.len() - trimmed.len();

    // Split into command token + rest.
    let rest = &trimmed[1..];
    let (token, args) = match rest.find(char::is_whitespace) {
        Some(idx) => (&rest[..idx], rest[idx..].trim_start()),
        None => (rest, ""),
    };

    if args.is_empty() {
        // Command-name completion: match the partial token against names+aliases.
        let mut matches: Vec<String> = commands::commands()
            .iter()
            .flat_map(|c| {
                let mut names = vec![c.name.to_string()];
                names.extend(c.aliases.iter().map(|a| a.to_string()));
                names
            })
            .filter(|n| n.starts_with(&token.to_lowercase()))
            .map(|n| format!("/{n}"))
            .collect();
        matches.sort();
        matches.dedup();
        if matches.is_empty() {
            return None;
        }
        // Replace the whole `/token` span.
        let replace_start = lead;
        let replace_end = lead + 1 + token.len();
        return Some(Completion {
            candidates: matches,
            selected: 0,
            replace_start,
            replace_end,
        });
    }

    // Argument completion: resolve the command to its canonical name, then
    // look up candidates.
    let canonical = commands::parse(trimmed)
        .map(|pc| pc.name)
        .unwrap_or_default();
    let mut matches: Vec<String> = arg_candidates(&canonical)
        .into_iter()
        .filter(|c| c.starts_with(&args.to_lowercase()))
        .collect();
    matches.sort();
    matches.dedup();
    if matches.is_empty() {
        return None;
    }
    // Replace the argument token (everything after the command token).
    let cmd_end = lead + 1 + token.len();
    let replace_start = cmd_end + 1; // skip the space
    let replace_end = input.len();
    Some(Completion {
        candidates: matches,
        selected: 0,
        replace_start,
        replace_end,
    })
}

/// Apply a completion candidate to the input, returning the new input and the
/// new cursor position (byte index).
pub fn apply(input: &str, completion: &Completion, candidate: &str) -> (String, usize) {
    let mut out = String::with_capacity(input.len() + candidate.len());
    out.push_str(&input[..completion.replace_start]);
    out.push_str(candidate);
    out.push_str(&input[completion.replace_end..]);
    let cursor = completion.replace_start + candidate.len();
    (out, cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_args(_: &str) -> Vec<String> {
        Vec::new()
    }

    fn theme_args(cmd: &str) -> Vec<String> {
        if cmd == "theme" {
            vec![
                "ravenwood".into(),
                "nord".into(),
                "dracula".into(),
                "solarized-dark".into(),
            ]
        } else {
            Vec::new()
        }
    }

    #[test]
    fn non_command_returns_none() {
        assert!(candidates_for("just a task", &no_args).is_none());
        assert!(candidates_for("", &no_args).is_none());
    }

    #[test]
    fn command_name_prefix_matches() {
        let c = candidates_for("/n", &no_args).unwrap();
        assert!(c.candidates.iter().any(|s| s == "/new"));
        assert!(c.candidates.iter().any(|s| s == "/n"));
    }

    #[test]
    fn command_name_alias_matches() {
        // /q matches the quit alias.
        let c = candidates_for("/q", &no_args).unwrap();
        assert!(c.candidates.iter().any(|s| s == "/quit"));
    }

    #[test]
    fn command_name_no_match_returns_none() {
        assert!(candidates_for("/zzz", &no_args).is_none());
    }

    #[test]
    fn command_name_replace_span_is_whole_token() {
        let c = candidates_for("/n", &no_args).unwrap();
        assert_eq!(c.replace_start, 0);
        assert_eq!(c.replace_end, 2);
    }

    #[test]
    fn argument_completion_uses_candidates() {
        let c = candidates_for("/theme no", &theme_args).unwrap();
        assert_eq!(c.candidates, vec!["nord".to_string()]);
        assert_eq!(c.replace_start, 7); // after "/theme "
        assert_eq!(c.replace_end, 9);
    }

    #[test]
    fn argument_completion_no_match_returns_none() {
        assert!(candidates_for("/theme zzz", &theme_args).is_none());
    }

    #[test]
    fn apply_replaces_command_token() {
        let c = candidates_for("/n", &no_args).unwrap();
        let (out, cursor) = apply("/n", &c, "/new");
        assert_eq!(out, "/new");
        assert_eq!(cursor, 4);
    }

    #[test]
    fn apply_replaces_argument_token() {
        let c = candidates_for("/theme no", &theme_args).unwrap();
        let (out, cursor) = apply("/theme no", &c, "nord");
        assert_eq!(out, "/theme nord");
        assert_eq!(cursor, 11);
    }

    #[test]
    fn cycle_wraps() {
        let mut c = Completion {
            candidates: vec!["a".into(), "b".into()],
            selected: 0,
            replace_start: 0,
            replace_end: 1,
        };
        assert_eq!(c.next(), 1);
        assert_eq!(c.next(), 0);
        assert_eq!(c.prev(), 1);
    }
}
