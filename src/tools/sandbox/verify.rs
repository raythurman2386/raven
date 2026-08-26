//! Test and lint running: `run_tests`, `run_lint`, test-runner detection, and
//! the verification-gate matcher (`is_verification_command`).

use anyhow::{Context, Result};
use regex::Regex;
use std::process::Command;
use std::sync::OnceLock;

use super::shell::resolve_command;
use super::{cap_output, setup_shell_env, spawn_confined, wait_for_child, Sandbox};

enum TestRunner {
    Cargo,
    Npm,
    Pytest,
}

impl Sandbox {
    /// Auto-detect and run the project's test suite.
    pub fn run_tests(&self) -> Result<String> {
        let runner = if self.workspace.join("Cargo.toml").exists() {
            TestRunner::Cargo
        } else if self.workspace.join("package.json").exists() {
            TestRunner::Npm
        } else if self.workspace.join("pytest.ini").exists()
            || self.workspace.join("pyproject.toml").exists()
            || self.workspace.join("setup.py").exists()
        {
            TestRunner::Pytest
        } else {
            return Ok(
                "No test runner detected (no Cargo.toml, package.json, or pytest config found)"
                    .into(),
            );
        };

        let (cmd, args): (&str, Vec<&str>) = match runner {
            TestRunner::Cargo => ("cargo", vec!["test"]),
            TestRunner::Npm => {
                if self.uses_vitest() {
                    (
                        "npx",
                        vec![
                            "vitest",
                            "--run",
                            "--pool=threads",
                            "--poolOptions.threads.singleThread",
                        ],
                    )
                } else {
                    ("npm", vec!["test"])
                }
            }
            TestRunner::Pytest => ("pytest", vec![]),
        };

        let mut command = Command::new(resolve_command(cmd));
        command
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        setup_shell_env(&mut command, &self.workspace);
        command.env("CI", "true");
        // Sanctioned test runner: skip the seccomp network block for npm
        // projects. vitest/v8 opens an AF_INET socket for V8 coverage + worker
        // IPC, which the block SIGSYS-kills. This is a user-sanctioned command
        // (not arbitrary model output), so the exemption does not weaken the
        // exfiltration guarantee. The flag is threaded into the pre_exec
        // closure — setting it via `command.env` alone is dead code because
        // pre_exec reads the parent env, not the Command::env override.
        //
        // rlimits are also skipped: a debug test binary can exceed the 64 MiB
        // RLIMIT_FSIZE cap (SIGXFSZ), and a clean build can exceed the 30s
        // RLIMIT_CPU cap. The test runner is user-sanctioned, so the exemption
        // is limited to commands the enforced-verify gate would credit.
        let skip_network_block = matches!(runner, TestRunner::Npm);
        let mut confined = spawn_confined(
            &mut command,
            &self.workspace,
            &self.extra_rw,
            skip_network_block,
            true,
        )
        .context("spawn test runner")?;
        match wait_for_child(&mut confined.child, 600) {
            Some((status, stdout, stderr)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(signal) = status.signal() {
                        let mut out =
                            format!("--- run_tests ({cmd}) killed by signal {signal} ---\n",);
                        out.push_str(&String::from_utf8_lossy(&stdout));
                        if !stderr.is_empty() {
                            out.push_str(&String::from_utf8_lossy(&stderr));
                        }
                        return Ok(cap_output(out));
                    }
                }
                let mut out = format!(
                    "--- run_tests ({}) exit={} ---\n",
                    cmd,
                    status.code().unwrap_or(-1)
                );
                out.push_str(&String::from_utf8_lossy(&stdout));
                if !stderr.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&stderr));
                }
                Ok(cap_output(out))
            }
            None => Ok("Error: test runner timed out".into()),
        }
    }

    /// Whether the workspace has a detectable test runner (Cargo, npm, or
    /// pytest). Mirrors the detection in [`Self::run_tests`]. Used by the
    /// enforced-verify gate to skip when there is nothing to run.
    ///
    /// For npm projects, also requires `node_modules` to exist so the gate
    /// doesn't loop on scaffolding tasks where deps aren't installed yet.
    /// For Python projects, checks that `pytest` is on PATH.
    pub fn has_test_runner(&self) -> bool {
        if self.workspace.join("Cargo.toml").exists() {
            return true;
        }
        if self.workspace.join("package.json").exists() {
            return self.workspace.join("node_modules").is_dir();
        }
        if self.workspace.join("pytest.ini").exists()
            || self.workspace.join("pyproject.toml").exists()
            || self.workspace.join("setup.py").exists()
        {
            return std::process::Command::new(resolve_command("pytest"))
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok();
        }
        false
    }

    fn uses_vitest(&self) -> bool {
        let pkg = self.workspace.join("package.json");
        if !pkg.exists() {
            return false;
        }
        std::fs::read_to_string(&pkg)
            .ok()
            .map(|s| s.contains("\"vitest\""))
            .unwrap_or(false)
    }

    /// Whether a shell command is a test, typecheck, or lint invocation.
    ///
    /// Used by the enforced-verify gate to credit `run_shell`-based
    /// verification (e.g. `npm test`, `cargo clippy`, `pytest`) the same
    /// way it credits the `run_tests` tool.
    pub fn is_verification_command(command: &str) -> bool {
        static RE: OnceLock<Regex> = OnceLock::new();
        let re = RE.get_or_init(|| {
            Regex::new(
                r"(?i)^\s*(cargo\s+(test|clippy|fmt\s+--\s*check)|npm\s+(test|run\s+(test|typecheck|lint))|npx\s+(jest|vitest|mocha|tsc)|yarn\s+(test|typecheck|lint)|pnpm\s+(test|typecheck|lint)|pytest|python3?\s+-m\s+pytest|tsc(\s|$)|eslint(\s|$)|prettier\s+--\s*check|ruff\s+check|mypy(\s|$)|flake8(\s|$)|go\s+test|make\s+test|dotnet\s+test|zig\s+build\s+test|deno\s+test|bun\s+test)"
            )
            .expect("valid regex")
        });
        re.is_match(command)
    }

    /// Auto-detect and run the project's linter / type checker.
    ///
    /// Non-mutating: reports problems without fixing them. Prefers the fastest
    /// check per ecosystem: `cargo clippy` for Rust, `tsc --noEmit` for
    /// TypeScript, `eslint` for plain JS, `pytest --collect-only` is avoided
    /// (that's a test, not a lint) — plain Python uses `python -m py_compile`.
    pub fn run_lint(&self) -> Result<String> {
        let has = |name: &str| self.workspace.join(name).exists();
        let (cmd, args): (&str, Vec<&str>) = if has("Cargo.toml") {
            (
                "cargo",
                vec!["clippy", "--all-targets", "--", "-D", "warnings"],
            )
        } else if has("tsconfig.json") {
            ("npx", vec!["tsc", "--noEmit", "-p", "tsconfig.json"])
        } else if has("package.json") {
            ("npx", vec!["eslint", "."])
        } else if has("pyproject.toml") || has("pytest.ini") || has("setup.py") {
            ("python", vec!["-m", "compileall", "-q", "."])
        } else {
            return Ok("No linter detected (no Cargo.toml, tsconfig.json, package.json, or Python config found)".into());
        };

        let mut command = Command::new(resolve_command(cmd));
        command
            .args(&args)
            .current_dir(&self.workspace)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        setup_shell_env(&mut command, &self.workspace);
        // Linters (clippy/tsc/eslint) compile the project, so they need the
        // same rlimits exemption as `run_tests` (large linker outputs, long
        // clean builds). The network block stays on: linters don't need it.
        let mut confined =
            spawn_confined(&mut command, &self.workspace, &self.extra_rw, false, true)
                .context("spawn linter")?;
        match wait_for_child(&mut confined.child, 600) {
            Some((status, stdout, stderr)) => {
                #[cfg(unix)]
                {
                    use std::os::unix::process::ExitStatusExt;
                    if let Some(signal) = status.signal() {
                        let mut out =
                            format!("--- run_lint ({cmd}) killed by signal {signal} ---\n",);
                        out.push_str(&String::from_utf8_lossy(&stdout));
                        if !stderr.is_empty() {
                            out.push_str(&String::from_utf8_lossy(&stderr));
                        }
                        return Ok(cap_output(out));
                    }
                }
                let mut out = format!(
                    "--- run_lint ({}) exit={} ---\n",
                    cmd,
                    status.code().unwrap_or(-1)
                );
                out.push_str(&String::from_utf8_lossy(&stdout));
                if !stderr.is_empty() {
                    out.push_str(&String::from_utf8_lossy(&stderr));
                }
                Ok(cap_output(out))
            }
            None => Ok("Error: linter timed out".into()),
        }
    }
}
