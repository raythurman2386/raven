//! Tests for `run_tests` / `run_lint` and the verification-gate matcher
//! (`Sandbox::is_verification_command`).

use crate::tools::Sandbox;

#[test]
fn run_lint_no_project_returns_message() {
    let tmp = tempfile::tempdir().unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.run_lint().unwrap();
    assert!(out.contains("No linter detected"), "{out}");
}

#[test]
#[cfg(target_os = "linux")]
fn run_tests_cargo_project_compiles_under_confinement() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[package]
name = "eval_add_test"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "pub fn is_even(n: i32) -> bool { n % 2 == 0 }\n\
         #[cfg(test)]\n\
         mod tests {\n\
             use super::*;\n\
             #[test]\n\
             fn even() { assert!(is_even(2)); }\n\
             #[test]\n\
             fn odd() { assert!(!is_even(3)); }\n\
             #[test]\n\
             fn zero() { assert!(is_even(0)); }\n\
         }\n",
    )
    .unwrap();
    let sb = Sandbox::new(tmp.path().to_path_buf());
    let out = sb.run_tests().expect("run_tests");
    assert!(
        out.contains("exit=0"),
        "cargo test under confinement must succeed: {out}"
    );
    assert!(
        !out.to_lowercase().contains("cross-device"),
        "must not hit EXDEV: {out}"
    );
    assert!(
        tmp.path().join(".raven/cargo-home").is_dir(),
        "CARGO_HOME should be pinned under workspace/.raven"
    );
}

#[test]
fn run_lint_cargo_project_runs_clippy() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname=\"x\"\nversion=\"0.1.0\"\n",
    )
    .unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.run_lint().unwrap();
    assert!(
        out.contains("--- run_lint (cargo)"),
        "should invoke cargo: {out}"
    );
}

#[test]
fn run_lint_python_project_runs_compileall() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname=\"x\"\n").unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.run_lint().unwrap();
    assert!(
        out.contains("python"),
        "should run python compileall: {out}"
    );
}

#[test]
fn is_verification_command_matches_test_commands() {
    let tests = [
        "cargo test",
        "cargo test --lib",
        "cargo clippy",
        "cargo clippy --all-targets -- -D warnings",
        "cargo fmt --check",
        "npm test",
        "npm run test",
        "npm run typecheck",
        "npm run lint",
        "npx jest",
        "npx vitest",
        "npx mocha",
        "npx tsc",
        "yarn test",
        "yarn typecheck",
        "yarn lint",
        "pnpm test",
        "pnpm typecheck",
        "pnpm lint",
        "pytest",
        "pytest -v",
        "python -m pytest",
        "python3 -m pytest",
        "tsc",
        "tsc --noEmit",
        "eslint .",
        "prettier --check .",
        "ruff check",
        "mypy src/",
        "flake8 .",
        "go test",
        "go test ./...",
        "make test",
        "dotnet test",
        "zig build test",
        "deno test",
        "bun test",
    ];
    for cmd in tests {
        assert!(
            Sandbox::is_verification_command(cmd),
            "should be a verification command: {cmd}"
        );
    }
}

#[test]
fn is_verification_command_rejects_non_test_commands() {
    let non_tests = [
        "cargo build",
        "cargo run",
        "npm install",
        "npm start",
        "ls -la",
        "echo hello",
        "git status",
        "git commit -m 'msg'",
        "curl http://example.com",
        "node server.js",
        "python script.py",
        "mkdir foo",
        "rm file.txt",
    ];
    for cmd in non_tests {
        assert!(
            !Sandbox::is_verification_command(cmd),
            "should not be a verification command: {cmd}"
        );
    }
}

#[test]
#[cfg(target_os = "linux")]
fn run_tests_skips_rlimits_for_large_linker_outputs() {
    // Regression: RLIMIT_FSIZE (64 MiB) was applied to `run_tests`, so a
    // debug test binary larger than 64 MiB (or a test that writes a large
    // file) was SIGXFSZ-killed. Sanctioned test runners must skip rlimits
    // the same way they skip the seccomp network block.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        r#"[package]
name = "eval_big_write"
version = "0.1.0"
edition = "2021"
"#,
    )
    .unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("src/lib.rs"),
        "pub fn ok() -> bool { true }\n\
         #[cfg(test)]\n\
         mod tests {\n\
             use super::*;\n\
             #[test]\n\
             fn writes_large_file() {\n\
                 let mut f = std::fs::File::create(\"big.bin\").unwrap();\n\
                 use std::io::Write;\n\
                 let chunk = vec![0u8; 1 << 20];\n\
                 for _ in 0..70 { f.write_all(&chunk).unwrap(); }\n\
                 assert!(ok());\n\
             }\n\
         }\n",
    )
    .unwrap();
    let sb = Sandbox::new(tmp.path().to_path_buf());
    let out = sb.run_tests().expect("run_tests");
    assert!(
        out.contains("exit=0"),
        "cargo test under confinement must not be capped by RLIMIT_FSIZE: {out}"
    );
    assert!(
        !out.contains("killed by signal"),
        "test runner must not be SIGXFSZ-killed: {out}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn run_tests_npm_project_skips_seccomp_network_block() {
    // Regression for Finding 26: the previous #137 fix set the exemption via
    // `command.env(...)`, which a `pre_exec` closure cannot see (it reads the
    // parent env before execve). So `run_tests` still SIGSYS-killed vitest/v8,
    // which opens an AF_INET socket for coverage + worker IPC.
    //
    // This test genuinely exercises the seccomp path: an npm `test` script
    // that binds an AF_INET socket (127.0.0.1) must SUCCEED under `run_tests`.
    // With the block active the child is killed by SIGSYS (exit 159 / signal 31)
    // and the output would contain "killed by signal"; with the exemption it
    // prints OK and exits 0.
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join("package.json"),
        r#"{"scripts": {"test": "node -e \"require('net').createServer(()=>{}).listen(0,'127.0.0.1',()=>{console.log('BIND_OK');process.exit(0)})\""}}"#,
    )
    .unwrap();
    let sb = Sandbox::new(tmp.path().canonicalize().unwrap());
    let out = sb.run_tests().unwrap();
    assert!(
        !out.contains("killed by signal"),
        "npm run_tests must skip the seccomp network block so vitest/v8 can bind an AF_INET socket, got: {out}"
    );
    assert!(
        out.contains("BIND_OK"),
        "npm run_tests must let the child bind an AF_INET socket, got: {out}"
    );
}
