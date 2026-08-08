# Changelog

All notable changes to Raven are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.3] - 2026-08-08

### Added

- **Windows support** — the sandbox now compiles and runs on Windows via a
  Job Object confinement layer (`src/tools/windows.rs`). Every subprocess is
  assigned to a fresh Job Object with `KILL_ON_JOB_CLOSE`, active-process,
  per-process, and per-job memory limits, and process-tree confinement. The
  Job Object handle is held for the child's full lifetime via a RAII guard so
  a runaway child cannot outlive the parent.
- **Cross-platform `OpenFlags`** — replaces the Linux-only `rustix::fs::OFlags`
  in `open_beneath`'s signature/fallback, so the sandbox builds on Windows.
- **Shared confined-spawn path** — `spawn_confined`/`run_confined` is now the
  single spawn path used by `run_shell`, `run_tests`, `run_lint`, and the git
  tools, so every subprocess inherits OS-level confinement.
- `docs/security.md` — new threat model and defense-layer documentation.

### Changed

- **Linux sandbox hardening** — subprocesses are now confined with `openat2`
  (`RESOLVE_BENEATH | NO_MAGICLINKS`), Landlock (workspace + temp + HOME + /dev),
  seccomp (network syscall block), and `setrlimit` (CPU / address space / file
  size / file descriptors / processes).
- **Direct exec** — allowlisted single-binary commands with no shell
  metacharacters are executed directly (no `sh -c`), removing the shell
  injection surface for the common case.
- **TUI fixes** — cursor now tracks correctly when input auto-wraps (shared
  width helper); the input box cap was raised to 12 rows with the cursor
  clamped to the last visible row; bracketed paste is enabled and `Event::Paste`
  appends the whole block at once; the plan panel scrolls with the mouse wheel.
- **Installer** — the `install.sh` EXIT trap now guards `$tmp_dir` with
  `${tmp_dir:-}` to avoid an "unbound variable" error under `set -u`.

### Fixed

- Test `HOME` pollution in `config::tests` that Landlock confinement exposed.
- Input buffer is now length-capped so a multi-MB paste cannot grow memory or
  slow per-frame re-wrap without bound.

### Security

- Confinement now covers all subprocess paths (`run_shell`, `run_tests`,
  `run_lint`, and git), not just `run_shell`.
- `unsafe` is limited to platform-required sites (Linux `pre_exec`, Windows
  Job Object FFI), each with a sound SAFETY comment. The ROADMAP's "zero
  unsafe" principle was updated to reflect this.

## [0.1.2] - 2026-08-08

### Added

- TUI wrapped multi-line input, ravenwood theme, streaming tail-patch.
- Plan mode with model-driven auto-exit + read-only plan toolset.
- Session persistence (atomic `messages.jsonl` + `summary.json`).
- Linear token estimator, phase-timing telemetry.

### Changed

- Ratatui 0.30, crossterm 0.29.

[Unreleased]: https://github.com/ret/raven/compare/v0.1.3...HEAD
[0.1.3]: https://github.com/ret/raven/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/ret/raven/compare/v0.1.1...v0.1.2
