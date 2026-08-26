//! Shell execution: `run_shell` plus the command-filtering helpers
//! (metachar detection, argv parsing, direct-exec decision, shell wrapper).

use anyhow::Result;
use std::process::Command;

use super::confinement::run_confined;
use super::{dangerous_re, safe_command_re, setup_shell_env, Sandbox};

impl Sandbox {
    /// Run a shell command in the workspace with a timeout.
    ///
    /// `cwd` is forced to the workspace; the environment is cleared and only
    /// explicitly allowed vars (`PATH`, `HOME`, `PWD`, `LANG`) are passed
    /// through. The best-effort denylist (`dangerous_re`) blocks obviously
    /// destructive patterns. Output is capped at 12 000 chars.
    ///
    /// The denylist is **not a security boundary** — it can always be
    /// bypassed. The `confirm_shell` setting (off with `--yolo`) provides
    /// the real safety net by requiring user approval for each command.
    /// Commands matching the [`safe_command_re`] allowlist skip the prompt.
    pub fn run_shell(&self, command: &str, timeout_secs: u64) -> Result<String> {
        if dangerous_re().is_match(command) {
            return Ok("Error: command blocked by sandbox filter".into());
        }

        // Direct-exec path: if the command is a known-safe single binary with
        // no shell metacharacters, run it without a shell. This removes the
        // shell-injection surface entirely for the common case.
        let mut cmd = if is_direct_exec_command(command) {
            match parse_argv(command).and_then(|argv| {
                let mut it = argv.into_iter();
                let bin = it.next()?;
                let mut c = Command::new(bin);
                c.args(it);
                Some(c)
            }) {
                Some(c) => c,
                None => shell_command(command),
            }
        } else {
            shell_command(command)
        };

        cmd.current_dir(&self.workspace);
        setup_shell_env(&mut cmd, &self.workspace);
        cmd.stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Sanctioned test runners (vitest/jest/mocha/npm test/cargo test/pytest/
        // tsc/eslint/...) open AF_INET sockets for coverage/worker IPC, which the
        // seccomp network block SIGSYS-kills, and write large linker outputs
        // that RLIMIT_FSIZE would SIGXFSZ-kill. `run_tests` already exempts npm
        // projects; mirror that here so a sanctioned test command run through the
        // shell is not killed. The predicate is the same one the enforced-verify
        // gate uses to credit shell-based verification, so the exemption is
        // limited to user-sanctioned commands, not arbitrary model output.
        let skip_network_block = Self::is_verification_command(command);
        run_confined(
            &mut cmd,
            &self.workspace,
            timeout_secs,
            &self.extra_rw,
            skip_network_block,
            skip_network_block,
        )
    }
}

/// Shell metacharacters that indicate a command needs a real shell.
///
/// When a command contains none of these and its first token is on the
/// allowlist, we can run it via direct exec (no shell, no injection surface).
fn has_shell_metachars(command: &str) -> bool {
    command.chars().any(|c| {
        matches!(
            c,
            ';' | '&'
                | '|'
                | '>'
                | '<'
                | '`'
                | '$'
                | '('
                | ')'
                | '{'
                | '}'
                | '!'
                | '^'
                | '\n'
                | '\r'
                | '\0'
        )
    })
}

/// Parse a command into argv via `shlex`. Returns `None` if the command
/// contains shell metacharacters or fails to parse.
fn parse_argv(command: &str) -> Option<Vec<String>> {
    if has_shell_metachars(command) {
        return None;
    }
    shlex::split(command)
}

/// Whether a command can be run via direct exec (no shell).
///
/// The first token must be on the `safe_command_re` allowlist AND the command
/// must contain no shell metacharacters. This flips the model from "denylist
/// dangerous" toward "allowlist safe": known-safe commands run without a
/// shell (no injection surface), everything else falls back to the shell path
/// (still denylist-filtered + confirmation-gated).
pub(crate) fn is_direct_exec_command(command: &str) -> bool {
    if has_shell_metachars(command) {
        return false;
    }
    let Some(argv) = parse_argv(command) else {
        return false;
    };
    let Some(first) = argv.first() else {
        return false;
    };
    safe_command_re().is_match(first)
}

/// Build a platform-aware shell command.
///
/// On Unix: `sh -c <command>`. On Windows: `cmd /C <command>`, falling back
/// to the `COMSPEC` environment variable if set.
fn shell_command(command: &str) -> Command {
    #[cfg(windows)]
    {
        let shell = std::env::var("COMSPEC").unwrap_or_else(|_| "cmd".into());
        let mut cmd = Command::new(&shell);
        cmd.arg("/C").arg(command);
        cmd
    }
    #[cfg(not(windows))]
    {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(command);
        cmd
    }
}

/// Resolve a command name to its platform-appropriate executable.
///
/// On Windows, `npm`, `cargo`, `npx`, `python`, and `pytest` are often
/// `.cmd` or `.exe` shims. This function appends `.cmd` when the bare name
/// is not found but `<name>.cmd` exists on `PATH`. On Unix the name is
/// returned unchanged.
pub(crate) fn resolve_command(name: &str) -> String {
    #[cfg(windows)]
    {
        let cmd_name = format!("{}.cmd", name);
        if std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .any(|p| p.join(&cmd_name).exists())
        {
            return cmd_name;
        }
    }
    let _ = name;
    name.to_string()
}
