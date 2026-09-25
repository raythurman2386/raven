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
    /// destructive patterns. Output at or under 12 000 characters is inline;
    /// larger output is written to `.raven/tool-output`.
    ///
    /// The denylist is **not a security boundary** — it can always be
    /// bypassed. The `confirm_shell` setting (off with `--yolo`) provides
    /// the real safety net by requiring user approval for each command.
    /// Commands matching the [`safe_command_re`] allowlist skip the prompt.
    pub fn run_shell(&self, command: &str, timeout_secs: u64) -> Result<String> {
        if dangerous_re().is_match(command) {
            return Ok("Error: command blocked by the dangerous-command denylist. \
                 Do not retry this command; pick a narrower one that stays in the workspace."
                .into());
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
        setup_shell_env(&mut cmd, &self.workspace, &self.raven_dir());
        // Explicitly null stdin: the child must never inherit raven's own
        // stdio. In ACP mode raven's fd 0 is the live JSON-RPC pipe — a child
        // that reads stdin would consume ACP frames, and a surviving
        // grandchild holding fd 0 open can corrupt pipe teardown. Both can
        // make the harness see a clean mid-turn EOF (exit 0).
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Sanctioned test runners (vitest/jest/mocha/npm test/cargo test/pytest/
        // tsc/eslint/...) open AF_INET sockets for coverage/worker IPC, which the
        // seccomp network block SIGSYS-kills, and write large linker outputs
        // that RLIMIT_FSIZE would SIGXFSZ-kill. `run_tests` already exempts npm
        // projects; mirror that here so a sanctioned test command run through the
        // shell is not killed. The predicate is the same one the enforced-verify
        // gate uses to credit shell-based verification, so the exemption is
        // limited to user-sanctioned commands, not arbitrary model output.
        let (skip_network_block, skip_rlimits) = confinement_for(command);
        run_confined(
            &mut cmd,
            &self.workspace,
            timeout_secs,
            &self.extra_rw,
            skip_network_block,
            skip_rlimits,
        )
        .map(|out| self.present_output("shell", out))
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

/// Confinement exemptions for a shell command.
///
/// Returns `(skip_network_block, skip_rlimits)`.
///
/// Verification commands (`cargo test`, `pytest`, …) skip both: they open
/// sockets for workers and write large linker outputs. Ordinary toolchain
/// commands (`cargo build`, `git fetch`, `npm install`, `curl` that is not
/// piped into a shell) also need the network and large outputs, so they get
/// the same exemption. The destructive denylist still runs first, so
/// `curl … | sh` never reaches this function.
pub(crate) fn confinement_for(command: &str) -> (bool, bool) {
    if Sandbox::is_verification_command(command) {
        return (true, true);
    }
    let bin = first_executable(command);
    let toolchain = matches!(
        bin.as_str(),
        "cargo"
            | "rustc"
            | "rustup"
            | "rustfmt"
            | "go"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "bun"
            | "uv"
            | "pip"
            | "pip3"
            | "git"
            | "gh"
            | "curl"
            | "wget"
            | "gcc"
            | "clang"
            | "clang++"
            | "g++"
    );
    (toolchain, toolchain)
}

/// First program token after a leading `cd … &&` and `VAR=value` prefixes.
fn first_executable(command: &str) -> String {
    let mut rest = command.trim();
    if let Some((head, tail)) = rest.split_once("&&") {
        let head = head.trim();
        if head == "cd" || head.starts_with("cd ") {
            rest = tail.trim();
        }
    }
    loop {
        let rest_trim = rest.trim_start();
        if rest_trim.is_empty() {
            return String::new();
        }
        let (tok, after) = rest_trim
            .split_once(char::is_whitespace)
            .unwrap_or((rest_trim, ""));
        if let Some((name, _)) = tok.split_once('=') {
            if !name.is_empty() && name.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()) {
                rest = after;
                continue;
            }
        }
        let name = tok.rsplit('/').next().unwrap_or(tok);
        return name
            .trim_matches(|c: char| c == '"' || c == '\'')
            .to_string();
    }
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

/// Build a shell command: `sh -c <command>`.
fn shell_command(command: &str) -> Command {
    let mut cmd = Command::new("sh");
    cmd.arg("-c").arg(command);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolchain_commands_keep_network_and_large_writes() {
        assert_eq!(confinement_for("cargo build --release"), (true, true));
        assert_eq!(confinement_for("git fetch origin"), (true, true));
        assert_eq!(confinement_for("cd crates && cargo check"), (true, true));
        assert_eq!(
            confinement_for("CARGO_TARGET_DIR=/tmp/t cargo build"),
            (true, true)
        );
    }

    #[test]
    fn plain_commands_stay_confined() {
        assert_eq!(confinement_for("ls -la"), (false, false));
        assert_eq!(confinement_for("echo hello"), (false, false));
    }

    #[test]
    fn verification_commands_stay_exempt() {
        assert_eq!(confinement_for("cargo test --lib"), (true, true));
    }

    #[test]
    fn shell_wrapper_does_not_inherit_toolchain_exemption() {
        // First token is the shell, so the network block stays on.
        assert_eq!(confinement_for("bash -c 'cargo build'"), (false, false));
        assert_eq!(confinement_for("npm install"), (true, true));
    }
}
