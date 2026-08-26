//! End-to-end smoke tests for the `raven` CLI binary.
//!
//! These exercise the compiled binary (via `CARGO_BIN_EXE_raven`) as a black
//! box — no `[lib]` target is required. They cover the offline surface that
//! doesn't need a live Ollama: help/version output and session persistence.

use std::process::Command;

/// The compiled `raven` binary, provided by Cargo for integration tests.
fn raven_bin() -> &'static str {
    env!("CARGO_BIN_EXE_raven")
}

/// Run `raven` with the given args and return its stdout + stderr + status.
fn run(args: &[&str]) -> (String, String, std::process::ExitStatus) {
    let out = Command::new(raven_bin())
        .args(args)
        .output()
        .expect("failed to spawn raven");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status,
    )
}

#[test]
fn help_succeeds_and_lists_flags() {
    let (out, _, status) = run(&["--help"]);
    assert!(status.success(), "--help should exit 0");
    assert!(out.contains("raven"), "help should name the binary");
    for flag in [
        "--mode",
        "--provider",
        "--context-window",
        "--no-stream",
        "--resume",
        "--export",
        "--acp",
        "--version",
    ] {
        assert!(out.contains(flag), "help should mention `{flag}`");
    }
    // Regression for issue #126: --help must document that --yolo implies
    // --mode agent (full toolset), so the behavior is discoverable.
    assert!(
        out.contains("imply") || out.contains("implies") || out.contains("agent"),
        "--help should document --yolo's agent-mode implication, got: {out}"
    );
}

#[test]
fn version_reports_package_version() {
    let (out, _, status) = run(&["--version"]);
    assert!(status.success(), "--version should exit 0");
    assert!(
        out.contains(&format!("raven {}", env!("CARGO_PKG_VERSION"))),
        "--version should print the package version, got: {out}"
    );
}

#[test]
fn self_update_help_lists_flags() {
    let (out, _, status) = run(&["self", "update", "--help"]);
    assert!(status.success(), "self update --help should exit 0");
    for flag in ["--version", "--rollback"] {
        assert!(
            out.contains(flag),
            "self update --help should mention `{flag}`, got: {out}"
        );
    }
}

#[test]
fn headless_without_task_errors() {
    // With no task and --headless, the CLI must exit non-zero with a clear message.
    let (out, err, status) = run(&["--headless"]);
    assert!(!status.success(), "headless with no task should fail");
    let combined = format!("{out}{err}");
    assert!(
        combined.contains("No task provided") || combined.contains("Pass a prompt"),
        "should print a no-task message, got: {combined}"
    );
}

#[test]
fn session_persistence_roundtrip() {
    // Run a one-shot task; even though the model is unreachable, the user
    // prompt is appended to a session before the network call. Verify a
    // session directory + summary are created.
    let ws = tempfile::tempdir().unwrap();
    let ws_path = ws.path().to_str().unwrap();

    // Point at an unreachable host so the run fails fast but still persists
    // the session metadata. Force agent mode to keep the run single-turn.
    // The provider's base_url comes from the workspace config (the old
    // --host flag was removed in favor of named providers).
    let cfg_dir = ws.path().join(".raven");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    std::fs::write(
        cfg_dir.join("config.toml"),
        "[providers.unreachable]\nbase_url = \"http://127.0.0.1:1/v1\"\n",
    )
    .unwrap();

    let (out, _, _) = run(&[
        "--headless",
        "--mode",
        "agent",
        "--provider",
        "unreachable",
        "-w",
        ws_path,
        "-p",
        "persistence smoke test",
    ]);

    let sessions_dir = ws.path().join(".raven").join("sessions");
    assert!(
        sessions_dir.is_dir(),
        "sessions dir should be created, got: {sessions_dir:?}\nstdout: {out}"
    );

    // Find the created session directory and read its summary.json.
    let mut found_summary = false;
    for entry in std::fs::read_dir(&sessions_dir).expect("read sessions dir") {
        let entry = entry.unwrap();
        if !entry.file_type().unwrap().is_dir() {
            continue;
        }
        let summary = entry.path().join("summary.json");
        if summary.is_file() {
            found_summary = true;
            let text = std::fs::read_to_string(&summary).unwrap();
            assert!(text.contains("model"), "summary.json should carry a model");
        }
    }
    assert!(
        found_summary,
        "at least one session summary.json should exist"
    );
}
