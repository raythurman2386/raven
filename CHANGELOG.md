# Changelog

All notable changes to Raven are documented here. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.5.15] - 2026-08-28

### Added

- **Provider token meters are now persisted** — every response's real
  `usage` (from `stream_options.include_usage` chunks or non-streaming
  bodies) rides on the assistant message it belongs to and lands in
  `messages.jsonl` (`usage` with `promptTokens` / `completionTokens` /
  `totalTokens`). One meter per iteration, including tool-call turns and
  the max-iteration wrap-up request; compaction folds the meters of
  dropped messages onto the summary so totals never shrink. The field is
  stripped from outgoing requests, so replayed history never echoes it to
  the provider, and transcripts from meterless providers are byte-identical
  to before.

### Changed

- **Omarchy agents-panel collector prefers real meters** —
  `omarchy-agent-usage-raven` reads the persisted `usage` and reports
  input/output separately instead of counting everything as a char-per-4
  output estimate. Transcripts without meters (older sessions) keep the
  estimate as a fallback, so history stays visible.

## [0.5.14] - 2026-08-28

### Changed

- **Steering no longer aborts the running turn** — typing while a turn runs
  is now allowed, and pressing Enter queues the message as a mid-turn
  direction that lands at the next iteration boundary as a `[steer]` user
  message (`AgentEvent::Steered` fires so consumers can render the moment it
  reached the model). `/steer` queues into the running turn the same way and
  keeps its re-fire semantics when idle. A direction typed in the turn's
  final moments is replayed as a fresh turn at `Done` so it is never dropped.
  Previously the TUI locked text input while running, `/steer` killed the
  turn (losing all in-flight tool work), and the agent restarted from the
  original prompt.
- **Per-iteration "Re-anchor" reminder removed** — the goal/todo anchor
  reminder now fires once at iteration 4 and then every 8th iteration
  (12, 20, …) instead of on every iteration past 4, and the one-shot
  iteration-5 "reflect" nudge is gone. Small models were parroting the
  injected reminder ("Re-anchoring: …") into every narration and restating
  the task list each turn instead of working.
- **System prompt narration rules** — the output section now asks for
  minimal narration (no "Let me inspect…" play-by-play), no restating of
  `<raven_reminder>` messages, and direct resumption when asked to continue.

### Fixed

- **SIGSYS-killed commands now explain themselves** — the sandbox's seccomp
  network block kills the first outbound TCP connection with SIGSYS (shell
  exit code 159). The tool result now says this is a deterministic sandbox
  policy, not an environment bug, and tells the model not to retry or
  re-diagnose. Previously a package install died silently and the model
  burned many iterations testing proxy env vars, forcing IPv4, and swapping
  `pnpm` for `curl` before asking the user for "high quality logs".

## [0.5.13] - 2026-08-27

### Fixed

- **Config-file keys after a `[table]` header were silently ignored** —
  `max_iterations`, `mode`, and `compact_threshold` written below
  `[providers.ollama]` in `~/.raven/config.toml` parsed as provider-table
  entries and never reached `Settings`, so turns ran with the compiled-in
  default of 60 iterations regardless of the configured budget. The parser
  is unchanged (TOML semantics); document the ordering rule and fix the
  shipped config. Long implementation turns no longer stop at "maximum
  number of tool-calling iterations" when the user configured a higher cap.
- **Sandboxed cargo could not resolve dependencies** — `pin_build_tool_dirs`
  points `CARGO_HOME` at `.raven/cargo-home`, which starts empty, so the
  first sandboxed `cargo test`/`clippy` failed resolution (empty index) or
  spent minutes re-fetching. The pinned home now links the host's
  `registry/index` and `registry/cache` (Unix symlink, Windows junction) so
  cargo resolves and extracts deps offline; `registry/src` extraction stays
  a real workspace directory so Landlock write roots are unchanged.
- **Auto-lint reflection ran after every editing iteration** — each pass is
  a full `cargo clippy --all-targets` compile (tens of seconds on Rust
  workspaces), so a 40-iteration editing turn spent most of its budget
  linting. The linter now runs at most once per turn and the result is
  reused; a stale-but-present lint note beats burning the iteration cap.
- **Long turns were invisible on disk** — session `messages.jsonl` is only
  written at turn end, so a 30-minute turn (slow local/cloud model, many
  iterations) left nothing to inspect while it ran and looked hung.
  `debug-events.jsonl` now records each iteration (and sub-agent iteration)
  as it happens, restoring a live timeline for post-mortems.
- **Update integration tests no longer flake on `ETXTBSY`** — spawning the
  freshly copied test binary could fail with `ExecutableFileBusy` on loaded
  CI runners when another process briefly held a write reference to the
  file. The test spawn helpers now retry with backoff, re-applying env
  overrides on every attempt so retries still hit the local test server.

### Added

- **Iteration budget in the TUI status bar** — the status line now shows
  `thinking… (iter 37/120)` while a turn runs, so a turn approaching its
  iteration cap is visible instead of a surprise wrap-up message.
  `/loop N` keeps the displayed budget in sync.
- **Sub-agent progress is no longer silent** — `delegate_task` sub-agents
  previously swallowed all events, so the parent TUI sat without feedback
  while the sub-agent made many model calls. A new
  `AgentEvent::Subagent { iter }` surfaces sub-agent iterations in the TUI
  status, headless runner, and ACP mapping (no-op payload).
- **Windows junction support for the pinned cargo home** — registry links
  use NTFS junctions via `windows-sys` (`FSCTL_SET_REPARSE_POINT`), which
  need no developer mode or elevation, keeping the seeding fix
  Windows-native. Cross-checked: `cargo check --target
  x86_64-pc-windows-gnu --all-targets` clean.

### Changed

- **Tab/Enter completion behavior in the TUI** — Tab fills the highlighted
  candidate (and cycles on repeat); Enter fills the candidate, but submits
  when the input already holds a complete candidate, so `/n` + Enter no
  longer auto-fires `/new` while `/model q` + Tab + Enter still runs in two
  presses.
- **README refresh** — line-count claim updated, "How I use it" section
  documents daily-driver workflow (ACP + Zed, model choices), and the
  install section notes the signed-manifest checksum verification.

## [0.5.12] - 2026-08-27

### Added

- **Usage-based token calibration** — streaming requests ask for
  `stream_options.include_usage`; real `prompt_tokens` from the provider
  feed an additive EMA that corrects compaction and `max_tokens` clamping.
  Providers that omit usage, or reject `stream_options` with HTTP 400, fall
  back to the uncalibrated estimator (incompatibility cached process-wide
  per base URL so TUI/ACP turns do not re-probe every prompt).
- **Omarchy integration guide** — `docs/omarchy.md` covers wiring Raven as
  the default agent and Agents bar usage collector on Omarchy Linux.
- **Brand mark** — `assets/rvn.svg` monochrome Raven mark (`currentColor`).

### Fixed

- **Tool errors no longer flash over the TUI input bar** — sandbox denials
  such as `Path outside workspace` were logged with `tracing` to stderr on
  the same TTY as the alternate screen, so the line painted at the cursor
  (inside the `❯` chat box) and vanished on the next redraw. When stdout is
  a TTY, tracing appends to `~/.raven/raven.log`; deterministic tool
  failures are `debug` rather than `error` (they already surface in the
  transcript tool block and as the tool result).
- **`stream_options` 400 fallback no longer burns a retry slot** — stripping
  the field after a strict-provider rejection retries immediately without
  consuming the transient-attempt budget.
- **Toolless summary responses no longer skew calibration** — max-iter
  `finish_with_summary` does not observe usage into the tools-schema EMA.

## [0.5.11] - 2026-08-27

### Fixed

- **Loop-breaker interrupted normal context-gathering** — the "stop calling
  tools" reminder fired after only 3 tool-only assistant turns, which is the
  normal exploration pattern (goal → list → grep → read). Reasoning models
  like `glm-5.3-flash:cloud` obeyed the instruction literally and abandoned
  the task before applying edits, failing the multi-file refactor eval. The
  threshold is now 6 consecutive tool-only turns, and the wording nudges
  toward a different approach instead of telling the model to stop.

### Changed

- **Default max iterations** — raised from 30 to 60 per turn, giving
  long-horizon and multi-file tasks more room before the loop budget is
  exhausted.

## [0.5.10] - 2026-08-27

### Changed

- **Ollama default model** — `qwen3.8:latest` → `glm-5.3-flash:cloud`, and
  all `glm-5.2` / `ox-alpha` references replaced with `glm-5.3-flash`.

## [0.5.9] - 2026-08-26

### Added

- **Agent Plugins v1.0.0 (skills-only)** — Raven now loads conformant plugin
  packages from `~/.raven/plugins/` and `.raven/plugins/`. Each plugin's
  `plugin.json` manifest is validated against the closed Agent Plugins schema
  (canonical `$schema`, name constraints, metadata types), and its skills are
  discovered from the fixed `skills/` location and surfaced through the
  existing `skill_search` / `skill_load` tools. Path containment is enforced
  so plugin files cannot resolve outside the plugin root. MCP servers and
  client extensions are ignored per the spec's incremental-adoption rules.

## [0.5.8] - 2026-08-26

### Fixed

- **Sandbox rlimits broke test/lint/format commands** — `RLIMIT_FSIZE` (64 MiB)
  and `RLIMIT_CPU` (30s) were applied to every confined subprocess, including
  the sanctioned verification commands the agent runs. A debug test binary
  larger than 64 MiB (or a clean build exceeding 30s of CPU) was killed by
  SIGXFSZ / SIGXCPU, so `run_tests`, `run_lint`, and `run_shell`-based
  verification (`cargo test`, `cargo clippy`, `cargo fmt --check`, `npm test`,
  `pytest`, `tsc`, `eslint`, …) could not complete. These commands now skip
  rlimits, mirroring the existing seccomp network-block exemption. Landlock
  and seccomp still apply, and the exemption is limited to commands the
  enforced-verify gate would credit — not arbitrary model output.

## [0.5.7] - 2026-08-25

### Added

- **`raven self update`** — an in-binary update path that downloads a release,
  verifies its Ed25519 signature (authenticity) and SHA-256 checksum
  (integrity) in-process, then atomically replaces the running binary while
  keeping a `.old` backup. `--version` pins a specific release;
  `--rollback` restores the previous binary. Honors `RAVEN_RELEASE_BASE_URL`
  (default GitHub releases) and supports `file://`/local paths for offline
  testing. Verification is fail-closed: a missing or invalid checksum or
  signature refuses the update.
- **Signed release packaging** — `scripts/package-release.sh` extracts the
  release layout (raw binaries + `.tar.gz`/`.zip` archives + `checksums.txt`)
  out of the CI workflow, and optionally signs `checksums.txt` via
  `scripts/sign-release.sh` when a secret key is passed.

### Changed

- **Release workflow hardening** — `release.yml` now calls
  `scripts/package-release.sh` instead of inline bash, and all GitHub Actions
  are pinned to commit SHAs (not tags/branches) as a supply-chain hardening
  measure. Release signing stays offline: CI builds and drafts the release;
  the maintainer signs `checksums.txt` locally and uploads
  `checksums.txt.sig` (the secret key never enters CI).

### Dependencies

- Added `ring` 0.17 and `base64` 0.22 (both already in the dependency tree)
  for in-process Ed25519 signature verification and SHA-256 checksums.

## [0.5.6] - 2026-08-25

### Changed

- **Repo map discovery** — prefers `git ls-files --cached --others
  --exclude-standard` (index-fast, respects `.gitignore`), falls back to an
  `ignore`-crate walk when git is unavailable. Hard-coded vendor dirs remain
  as a safety net. Candidates are path-scored before the extract budget so
  entrypoints and shallow `src/` win over deep tests. Docs thresholds fixed
  (15 files / ~3.5K chars).

## [0.5.5] - 2026-08-25

### Fixed

- **Home no longer traps the transcript at the top** — jumping to the top
  used `scroll = u16::MAX` as a sentinel. Rendering clamped correctly, but
  wheel / PgDn subtracted from that value and stayed visually stuck until
  End. Home now sets the real max scroll offset; relative scroll clamps
  through it, and each draw heals any overshoot.

## [0.5.4] - 2026-08-25

### Fixed

- **Windows build** — custom crossterm `Command` helpers
  (`EnableMouseCaptureLite`, `DisableAlternateScroll`) now implement
  `execute_winapi`, which the trait requires on Windows. v0.5.3 failed to
  compile for `x86_64-pc-windows-msvc` (CI Test + Release).

## [0.5.3] - 2026-08-25

### Fixed

- **Mouse wheel scrolls the log again** — on alternate-screen terminals
  (notably Ghostty), the wheel was often delivered as Up/Down *keys*, which
  hit prompt-history recall instead of moving the transcript. Raven now
  disables xterm alternate-scroll (`?1007`), enables a lighter mouse-capture
  set (clicks/drags/wheel without hover-move flood), and drains every pending
  input event each tick so scroll reports are not starved.
- **Up/Down no longer silently no-op** at the ends of prompt history (empty
  history or already on the oldest entry) — they fall through to log scroll.
- **Shift+Tab no longer cycles mode mid-run** — completion prev / mode cycle
  only apply when idle (not while a turn is in flight).

### Changed

- Docs: TUI keybind tables and architecture notes now match the split between
  Up/Down (prompt recall) and wheel / PgUp / PgDn (log scroll).

## [0.5.2] - 2026-08-24

### Removed

- **`git_commit` tool** — the agent no longer has a dedicated commit tool.
  Inspect-only `git_status` / `git_diff` / `git_log` remain. The harness does
  not create commits unless the user explicitly asks via `run_shell`.
- **Auto-checkpoint commits** — budget exhaustion no longer auto-commits a
  dirty tree. Work stays in the working tree for the user to review.
- **`/undo`** — it existed to reverse agent checkpoint commits and would
  otherwise rewind the user's last commit.
- **Secrets gate on `git_commit`** — the scanner existed only to refuse
  harness-created commits.

### Changed

- **Parallel sub-agents** apply each worktree's diff onto the parent working
  tree (no merge commit). Failed applies still write
  `.raven/recovery-sub-N.patch`.
- System prompt: do not commit, amend, or push unless the user explicitly asks.

## [0.5.1] - 2026-08-23

### Added

- **ACP session mode as a config option** — Raven now advertises a `mode`
  select (`plan` / `agent` / `chat`, category `mode`) in `configOptions`
  alongside the model picker. Modern ACP clients ignore the legacy `modes`
  field when `configOptions` is present, so without this the editor had no
  mode selector and every thread stayed on the default (plan).
  `session/set_config_option` with `configId: "mode"` switches the live
  session; `session/set_mode` still works for older clients.

## [0.5.0] - 2026-08-23

### Added

- **ACP provider/model selection** — Raven advertises a `model` session
  config option over ACP listing every configured provider's models as
  provider-qualified ids (`provider/model`), so editors with an ACP model
  selector (Zed, etc.) can switch providers and models without restarting
  the thread. Each provider's list comes from its live `/models` endpoint
  when reachable, else its curated fallback; per-provider capped at 200 so
  a huge catalog (e.g. OpenRouter) can't flood the dropdown.
- **`session/set_config_option`** — the current spec-correct way to change
  a session config option. Selecting a `provider/model` value switches the
  session onto that provider (re-resolving endpoint/key + context window);
  a plain model name stays on the current provider. The legacy
  `session/set_model` is now provider-aware too (both share one helper).
- **`opencode-go` built-in provider preset** — OpenCode Go subscription
  ($5 first month, then $10/mo) serving OpenAI-compatible models from
  `https://opencode.ai/zen/go/v1`. Adds `OPENCODE_GO_API_KEY`, curated
  onboarding fallback models (incl. `qwen3.8-max`, `minimax-m3`), and skips
  the Ollama `/api/show` context probe on `opencode.ai`.
- **`/retry`** — re-run the last user prompt after a failed turn (drops any
  stale partial assistant/tool output from the failed turn; guards against
  retrying with no prior prompt or while a turn is running).
- **`/loop [N]`** — show or set the `max_iterations` budget for new turns.
- **`/steer <message>`** — redirect the running agent by restarting the turn
  with the direction appended (preserving all prior context).
- **`/cleanup <days> [--yes]`** — prune sessions older than N days. Dry-run
  by default (re-run with `--yes` to delete); never deletes the current
  session. Uses Hinnant's civil-date arithmetic (no new dependency).

### Changed

- **Slash-command module restructure** — `src/commands.rs` became a
  `src/commands/` module directory; the dispatcher moved into
  `tui/dispatch.rs` where it can mutate `TuiState` internals without
  widening visibility. No behavior change.
- **TUI input usability** — Up/Down now walk the full prompt-history (not
  just the single most recent entry), and move the completion highlight
  when the autocomplete popup is open (previously only Tab/Shift+Tab).

### Fixed

- **`SessionStore::delete()` path-traversal hardening** — `delete()` now
  rejects non-bare session ids (empty, `.`, `..`, path separators, absolute
  paths) before joining onto the sessions dir, so a crafted id can't make
  `remove_dir_all` escape the sessions directory.

## [0.4.2] - 2026-08-22

### Added

- **Secrets gate on `git_commit`** — staged files are scanned for well-known
  credential prefixes (AWS, GitHub, GitLab, OpenAI, Anthropic, OpenRouter,
  Stripe, PEM private keys, JWTs, …) and the commit is refused on a match.
  The tool result reports path + rule name only; the secret is never echoed.
  Harness checkpoint commits use the same gate. Complements the existing
  `.env` / `.raven/` pathspec exclusions.
- **Tool-argument hygiene** — before dispatch, Raven rejects oversized
  arguments JSON (>1 MiB), non-object payloads, missing/empty required
  fields, and overlong path/command strings.
- **Never-execute shell patterns** even under `--yolo`: `/dev/tcp`, `nc -e`,
  `mkfifo`, encoded PowerShell, `certutil -decode`, `Invoke-Expression` /
  `iex (`, `base64 | sh`, and pipe-to-`pwsh`/`cmd`.
- **Session debug-events for `run_shell`** — commands that actually start
  (allowlisted, confirmed, or `--yolo`) are recorded locally in
  `debug-events.jsonl` as `shell` events.
- **Structured compaction** — extractive and LLM summaries now lead with
  Goal / Open todos / Key paths / Last verification. The `Compacted` event
  carries a one-line "what was compacted" note shown in the TUI and
  headless log.
- **Session export** — `/export [dir]` (alias `/x`) and `raven --export
  [session-id]` write a local Markdown + JSON bundle (plus `last.patch`
  when present) under `.raven/exports/<id>/` by default. Nothing is sent
  off-machine.
- **Visible auto-checkpoints and recovery patches** — budget-exhaustion
  checkpoints emit a `Checkpoint` event (TUI `✓ …`, headless `[auto-checkpoint
  committed — …]`). Parallel merge failures write `.raven/recovery-sub-N.patch`
  plus `.raven/RECOVERY.md` with `git apply` instructions. TUI turns that
  produce a git diff announce `diff snapshot → .raven/sessions/<id>/last.patch`.
- **Eval cases** `14_large_tool_output` (paged `read_file` must see the tail)
  and `15_windows_fs_edge` (file-tool confinement on Windows; skipped on
  other OSes). Layer A covers large-output capping, same-file serial edits,
  and checkpoint-event visibility.

### Changed

- **Stricter shell metacharacter detection** for the direct-exec path:
  braces, `!`, `^`, CR, and NUL now force the shell fallback (no
  `Command::new` of an injected string).
- **Direct-exec allowlist** expanded with common dev tools (`just`, `fd`,
  `jq`, `bun`, `deno`, `uv`, `rustfmt`, …).
- **Windows residual-risk documentation** in `docs/security.md`: Job Objects
  are not a filesystem or network sandbox; file tools still use lexical
  canonicalization (no `openat2`); `%TEMP%` is not pinned.

## [0.4.1] - 2026-08-22

### Added

- **First-run onboarding wizard** — on a fresh interactive install (no config,
  no `--provider`/`--model`/`RAVEN_PROVIDER`, stdin is a TTY) Raven now prompts
  for a provider (local Ollama, Ollama Cloud, OpenRouter, or any custom
  OpenAI-compatible endpoint via `name:base_url`), lists live models from the
  endpoint when reachable (falling back to a curated list otherwise), and
  accepts an optional API key. The choice is persisted so future runs need no
  prompts.
- **Secret-free config + key separation** — the wizard writes `provider` /
  `[providers.<name>]` (base_url + default_model) to `~/.raven/config.toml`
  (0600) and any API key to `~/.raven/.env` (0600), keeping the config file
  free of credentials and safe to share. The `~/.raven` directory is locked to
  0700. Files are created at 0600/0700 from the start (no chmod race).
- **`~/.raven/.env` auto-load** — Raven reads provider API keys from
  `~/.raven/.env` at startup (no-overwrite), so keys persist across runs and
  the key entered during onboarding is picked up in the same session.
- **Custom provider model fallbacks** — curated default models
  (`gpt-4o`, `gpt-4o-mini`) for custom OpenAI-compatible endpoints so the
  wizard never dead-ends.

### Changed

- **Ollama built-in default model** — `gemma4:latest` → `qwen3.8:latest`,
  matching the README's recommended local default.
- **`fetch_live_provider_models` visibility** — promoted to `pub(crate)` so the
  onboarding wizard can reuse the live model listing.

### Fixed

- **TOCTOU permission race on key/config writes** — `~/.raven/.env` and
  `~/.raven/config.toml` are now created at restrictive permissions directly
  (`OpenOptions::mode(0o600)`) instead of write-then-chmod, so an API key is
  never momentarily world-readable.
- **First-run key not applied in-session** — reload `~/.raven/.env` after the
  wizard so keyed providers (OpenRouter, Ollama Cloud) authenticate on the very
  first run, not the second.
- **Install checksum match anchored to exact artifact name** — the release
  workflow now writes raw-binary and archive entries to `checksums.txt`; the
  installers previously matched both lines and every install failed. Anchored
  to the end of the artifact name (from the v0.4.0-era fix).
- **Windows test gating** — the HOME-isolated dotenv test is `#[cfg(not(windows))]`
  (`dirs::home_dir()` reads `USERPROFILE`/`HOMEDRIVE`+`HOMEPATH` on Windows).

### Dependencies

- Bumped `anydoc` 0.1.8 → 0.1.9.

[Unreleased]: https://github.com/raythurman2386/raven/compare/v0.5.15...HEAD
[0.5.15]: https://github.com/raythurman2386/raven/compare/v0.5.14...v0.5.15
[0.5.14]: https://github.com/raythurman2386/raven/compare/v0.5.13...v0.5.14
[0.5.10]: https://github.com/raythurman2386/raven/compare/v0.5.9...v0.5.10
[0.5.9]: https://github.com/raythurman2386/raven/compare/v0.5.8...v0.5.9
[0.5.8]: https://github.com/raythurman2386/raven/compare/v0.5.7...v0.5.8
[0.5.7]: https://github.com/raythurman2386/raven/compare/v0.5.6...v0.5.7
[0.5.6]: https://github.com/raythurman2386/raven/compare/v0.5.5...v0.5.6
[0.5.5]: https://github.com/raythurman2386/raven/compare/v0.5.4...v0.5.5
[0.5.4]: https://github.com/raythurman2386/raven/compare/v0.5.3...v0.5.4
[0.5.3]: https://github.com/raythurman2386/raven/compare/v0.5.2...v0.5.3
[0.5.2]: https://github.com/raythurman2386/raven/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/raythurman2386/raven/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/raythurman2386/raven/compare/v0.4.2...v0.5.0
[0.4.2]: https://github.com/raythurman2386/raven/compare/v0.4.1...v0.4.2

## [0.4.0] - 2026-08-21

### Added

- **ACP registry readiness** — `raven --acp` now advertises a single `agent`-type
  auth method (`agent-auth`) in `initialize`, satisfying the ACP registry's
  `--auth-check` (which requires at least one `agent`/`terminal` method). Raven
  authenticates with provider credentials already resolved in-process
  (env / config / `.env`), so `authenticate` is a no-op acknowledgement that
  validates the `methodId` and rejects unknown ones.
- **`session/set_model`** — ACP clients can now switch the model per-session,
  and `initialize` advertises `sessionCapabilities.set`, matching the surface
  of grok-build / codex-acp.
- **Release archives for the ACP registry** — the release workflow now builds
  `.tar.gz` (unix) / `.zip` (windows) archives containing a stable
  `raven`/`raven.exe`, with archive sha256 appended to `checksums.txt`, so the
  registry's `binary` distribution can pin a version-independent `cmd`. Raw
  binaries are kept for the installers.
- **macOS release builds** — `aarch64-apple-darwin` and `x86_64-apple-darwin`
  are re-enabled in the release matrix for full platform coverage.
- **Registry icon** — a 16×16 monochrome `currentColor` raven mark
  (`assets/icon.svg`) for the ACP registry submission.

### Changed

- **ACP invocation standardized on `raven --acp`** — docs and source comments
  now use the flag form consistently (the registry launches `cmd + args`
  verbatim, so the entry uses `args: ["--acp"]`).

## [0.3.1] - 2026-08-21

### Fixed

- **Temperature no longer serialized with f32→f64 artifacts** — `Settings.temperature`
  is stored as an `f32`, and serializing it directly widened to long values like
  `0.20000000298023224`, which some OpenRouter providers (e.g. Stealth)
  strict-schema-validate and reject with HTTP 400. Temperature is now rounded to
  4 decimal places at all three request-body sites (streaming, non-streaming, and
  retry paths) so it serializes cleanly (`0.2`, `0.7`, `0.33`, …).
- **SKILL.md files without YAML frontmatter are no longer silently skipped** —
  skill discovery previously dropped any skill whose `SKILL.md` lacked a
  frontmatter `name:`, so `skill_search` reported "No skills found" even though
  the file existed. Discovery now falls back to the skill directory name,
  matching how Claude Code / agent skills treat plain-markdown skills.

## [0.3.0] - 2026-08-20

### Added

- **TUI polish program** (`docs/tui-polish.md`) — a grounded, phased plan for
  making the TUI a serious operator's cockpit: dense, local, honest,
  keyboard-first, terminal-native. Every change below is one slice of that
  program, verified with `cargo test` + clippy + fmt + a live look.
- **Tool calls as distinct bordered blocks** — each tool call now renders as a
  dim bordered box with a label (`┌─ read_file` / `│ ⇢ read_file(x)` / `└─`),
  so "working" reads at a glance instead of blending into model prose.
- **Code-block language labels** — fenced code blocks show their language in
  the top border (`┌─ rust` instead of `┌─ code`).
- **Context-sensitive keyhint footer** — the bottom row changes with state:
  approve / answer / interrupt / idle, so a new user finds stop, model switch,
  and plan approve without the README.
- **Provider name in the top bar** — the bar now reads
  `app · model · provider · ctx% · mode`.
- **Empty-state guidance + error recovery** — a "what to try" line on an empty
  transcript, and a recovery action under every error.
- **Prompt history recall** — with an empty input, Up/Down recall previously
  submitted prompts (bounded, resets when typing). Home jumps to the top of
  the transcript, End returns to the live tail.
- **Compact tool-call args** — tool blocks read
  `read_file path=src/main.rs line=1-40` instead of raw JSON braces; long
  values truncate.
- **Markdown table width cap** — wide table cells truncate to a per-cell
  budget with a `…` marker so rows wrap on cell boundaries instead of blowing
  out a line.

### Fixed

- **CJK/emoji no longer break transcript scrolling** — the transcript wrap and
  scroll math now uses display width (`unicode-width`), matching the terminal
  and the input path. Previously a width-2 character (CJK, emoji) made long
  content impossible to scroll to correctly.
- **Transcript no longer freezes while streaming** — the cached scroll-range
  total is now invalidated whenever the log content changes (previously it was
  computed once and never updated as the turn grew, so the tail wouldn't
  track).

### Changed

- **O(viewport) log virtualization** — the per-frame render no longer re-walks
  the whole history to compute the scroll range; the total row count is cached
  and refreshed only when the log changes or the terminal resizes. Long
  sessions render faster.

## [0.2.7] - 2026-08-20

### Fixed

- **Stream interruptions no longer lose your partial reply** — if the model
  produces text and the stream breaks mid-response (connection reset, provider
  hiccup), Raven now keeps what was written and appends a
  `[stream interrupted — retry or use --no-stream]` hint instead of dropping
  the turn. Retry the prompt, or use `--no-stream` for endpoints that don't
  reliably support streaming.

### Docs

- **New troubleshooting guide** (`docs/troubleshooting.md`) — covers the common
  failure modes: Windows `.exe` / "open with?" / PATH, stream decode and
  "stream interrupted" errors, sandbox denies and their escape hatches,
  SearXNG→DuckDuckGo fallback, the ACP one-liner, and first-run provider
  errors.
- **README install snippets corrected** — removed the stale `--provider-url`
  flag (replaced with the `[providers.ollama] base_url` config approach) and
  bumped the version-pin example to the current release.
- **ROADMAP restructured** — the phase-by-phase build log (which described
  shipped features as future work) is now a clean **Done / Next (polish) /
  Non-goals** layout matching the 0.3 release gate.

## [0.2.6] - 2026-08-19

### Added

- **Batch eval runner** — `evals/run_all_models.py` runs the eval suite
  against multiple models (`--list-only`, `--models`, `--host`), saving
  per-model reports to `evals/out/<timestamp>.{json,md}` for analysis.
- **Recommended-models documentation** — `evals/README.md` now lists model
  status (Recommended / Passing / Partial / Flaky) plus daily-driver and
  frontier selection guidance.

### Fixed

- **`run_shell` no longer kills sanctioned test runners** — it hardcoded
  `skip_network_block=false`, so the seccomp network block SIGSYS-killed
  npm/vitest/cargo-test style commands. It now reuses the
  verification-command predicate to exempt user-sanctioned test commands.
- **Checkpoint commits no longer sweep stray temp files** — `git_commit`
  (checkpoint mode) now un-stages scratch files (`err.txt`, `out.txt`,
  `testout.txt`, `_tmp_*`, `*.log`) dropped by model tooling, alongside
  deletions.
- **OpenRouter builtin default model corrected** — was
  `deepseek-v4-flash:cloud` (an Ollama model, not a valid OpenRouter model);
  now `x-ai/grok-4.5`.
- **`/model` autocomplete candidates updated** — stale list replaced with
  current models (`qwen3.8`, `glm-5.2:cloud`, `x-ai/grok-4.5`,
  `x-ai/grok-4.6`).
- **README recommended-model sections updated** — refreshed to the current
  landscape and removed stale suggestions.

## [0.2.5] - 2026-08-17

### Added

- **Local-only session event logging** — each session now maintains a
  `debug-events.jsonl` file recording model changes, summary updates, and
  message saves with timestamps. Purely local; enables reproducible
  debugging without any remote telemetry.
- **Git patch snapshots** — session directories include a `last.patch` file
  containing the full `git diff` after each task completes. Suitable for
  audit trails or rollback decisions; stored locally on-disk.
- **Slash-command completion for `/provider` and `/model`** — the TUI now
  shows live provider names from configuration and fetches available models
  from the active provider's endpoint (e.g., `/api/models` for Ollama,
  `/models` for cloud) with static fallbacks for offline scenarios.

### Changed

- **README modernized for daily-driver use** — Ollama Cloud is now highlighted
  as the recommended endpoint for production work; model recommendations
  updated to reflect current landscape (`qwen3.5-coder`, `deepseek-v4-pro:cloud`,
  `grok-4.5` for frontier tasks). Removed outdated model suggestions.
- **Provider setup documentation** — Ollama Cloud workflow now documented
  first, followed by local Ollama and OpenRouter for specific use cases.

## [0.2.4] - 2026-08-16

### Added

- **Persistent goal + todos** — `.raven/state/` (`goal.json`, `todos.json`)
  survive compaction and resume; injected into the system prompt each turn
  and after `goal_set` / `todo_write`.
- **`goal_set`, `delegate_task`, `think` tools** — long-horizon tracking,
  depth-1 sub-agents (shared workspace, no nested delegate / parent state
  overwrite), and a read-only reasoning scratchpad.
- **Eval case `13_long_horizon`** — multi-step fix that must set a goal and
  track todos.

### Changed

- **Compaction thrashing protection** — pause after 3 no-reduction
  compactions; retry every 4th iteration so a later prune can resume.

## [0.2.3] - 2026-08-14

### Added

- **Eval cases `11_secrets_stay_uncommitted` and `12_verify_before_done`** —
  live graders plus Layer A: `.env` must stay untracked after `git_commit`,
  and an edit that finishes without tests must trip the verify gate.
- **`raven --acp`** — Agent Client Protocol v1 on stdin/stdout so editors can
  attach. Text prompts, `session/load` replay, `session/list` /
  `session/close` / `session/set_mode`, cancel, and
  `session/request_permission` for `ask_user` / shell confirm. No MCP, no
  client-owned FS/terminal. Offline protocol tests cover the wire subset.

### Changed

- **Criterion benches measure the real work again** — `repomap` invalidates
  the cache so a sample is a cold walk (plus a cached-hit contrast);
  `context` reuses one tokio runtime instead of constructing one per iter.
- **Eval `12_verify_before_done` grader reads stdout/stderr files** —
  `EVAL_STDOUT` / `EVAL_STDERR` are paths from `evals/run.py`, not the
  captured text. The previous grep ran against the path strings and
  always failed.

- **Sandbox filesystem policy is now explicit** — OS confinement lives in
  `src/tools/sandbox/confinement.rs` as `FsPolicy`. Write roots are the
  workspace, caller-supplied extras (git worktrees), and `/dev`. The process
  temp dir is never granted; children get `TMPDIR` pinned under
  `.raven/tmp`.

### Fixed

- **`run_shell` can no longer write `/tmp` from a home-directory workspace** —
  Landlock used to grant the whole process temp dir whenever the workspace
  was not nested under it. That was the live `06_sandbox_escape` hole.
- **`git_commit` no longer depends on host git identity or hooks** — commits
  set `user.name`/`user.email`, disable `commit.gpgsign`, and point
  `core.hooksPath` at a null path. The `05_git_commit_clean` eval no longer
  fails because the host has no `user.email` or because `cargo test` left
  `Cargo.lock` dirty.

## [0.2.2] - 2026-08-13

### Fixed

- **Interrupted TUI turns can no longer steal the next turn's handle** —
  the session used one shared event channel, so a leftover `Done` from an
  aborted send could `await` the successor and wipe session history. Each
  turn now owns its own channel; abort drops the old receiver.
- **Enter no longer freezes the TUI before the user bubble paints** — each
  send constructed a new `Agent` on the UI thread, which rebuilt the repo
  map by walking the whole workspace. A typical crate was cheap; opening
  Raven on a parent folder of many repos (e.g. `~/Work`) stalled every
  submit. Construction now runs off-thread, the HTTP client is reused, and
  the repo map is cached (and the walk is capped) until a file edit
  invalidates it.
- **`exit=10` no longer counts as a passing test** — verify/lint used
  `contains("exit=0")`, so exit codes 10/20/30 were treated as success.
- **`save_all_messages` is atomic** — it used in-place truncate; a crash
  mid-write could leave a partial JSONL. It now uses the same temp+rename
  path as `append_message`.
- **Headless plan revise actually executes the revised plan** — after
  `[r]evise` + approve, execution still used the first plan and first
  message list.
- **`/provider` lists known names** and rejects unknown names instead of
  silently switching to builtin ollama.
- **Missing-file tool errors are not retry-nudged**; HTTP error bodies
  truncate on a UTF-8 boundary instead of panicking.
- **Multi-line input caret no longer drifts** — the box used ratatui
  word-wrap while the caret character-wrapped, so a long prompt with
  spaces put the cursor on the wrong cell. Input is now pre-wrapped with
  the same word-break rules used to place the caret.

## [0.2.1] - 2026-08-13

### Added

- **Named providers (Ollama / OpenRouter)** — a `Provider` abstraction bundles
  endpoint + auth + default model so switching between local and cloud is a
  single unit, mirroring Grok Build / Hermes / Opencode. Built-in `ollama` and
  `openrouter` presets, a `[providers.<name>]` config table, a `/provider`
  slash command for runtime switching, and a resolution order of
  `--provider` > `RAVEN_PROVIDER` env > config `provider` > builtin. `model`
  stays a per-session override layered on the provider's default.
- **Per-provider `api_key_env`** — a provider's key env var is declared in
  config (e.g. `api_key_env = "ANTHROPIC_API_KEY"`), so adding a new provider
  is a pure config change with no code edit. Unknown providers fall back to a
  conventional `{NAME}_API_KEY`. Only the var *name* is stored in config — the
  secret stays in the environment.
- **Field-wise provider table merge** — workspace `[providers.<name>]` tables
  overlay global ones per field, so a partial workspace entry (e.g. only
  `base_url`) no longer wipes global `api_key_env` / `default_model`.

### Changed

- **Streaming is now truly incremental** — SSE lines are parsed and emitted
  token-by-token as bytes arrive instead of buffering the whole response and
  bursting at the end. Multi-byte UTF-8 split across TCP chunks is still never
  lossy-decoded.
- **Provider-aware errors** — `OllamaUnreachable` renamed to
  `ProviderUnreachable`, and `ModelNotFound` / `HttpError` now carry the
  provider name. Error text adapts: local Ollama gets an `ollama pull` hint,
  other providers get a check-the-model-id message.

### Fixed

- **Tracing no longer corrupts the TUI on tool error** — `tracing` was writing
  to stdout (which the TUI owns in raw mode + alternate screen), so a `WARN
  Transient tool error` line from a failed tool call overlapped the input bar
  and knocked out the status bar. Logs now go to stderr, out of the alternate
  screen.

## [0.2.0] - 2026-08-12

### Fixed

- **Verify gate no longer credits false green** — the model could assert tests
  passed when the runner was actually blocked (e.g. the sandbox SIGSYS-killed
  the test runner before it ran). The gate is now fail-closed: it only credits
  a pass when the runner genuinely executed and exited 0. (`#136`)
- **`run_tests` network-block exemption was dead code** — the `#137` fix set
  `RAVEN_SANDBOX_NETWORK_BLOCK=0` via `Command::env`, but the seccomp filter
  reads `std::env::var` inside the `pre_exec` closure, which runs after
  `fork()` and before `execve()` — at that point it sees the parent env, not
  the `Command::env` override. So the exemption never took effect and
  `run_tests` still SIGSYS-killed vitest/v8 (signal 31). The flag is now
  threaded through `spawn_confined → apply_os_confinement →
  apply_seccomp_network_block` and captured directly in the `pre_exec`
  closure where it is visible. `run_tests` passes `true` for npm runners;
  `run_shell`/`run_lint`/git pass `false`, preserving the exfiltration
  guarantee for arbitrary model output. (`#137` follow-up)
- **Default sandbox still SIGSYS-killed vitest** — the seccomp network block
  killed vitest/v8 (which opens an AF_INET socket for V8 coverage / worker
  IPC). `run_tests` on npm projects now skips the network-block filter for
  that one test-runner invocation. The npm network-block test was also made
  cross-platform (uses `node -e` instead of Unix shell `$VAR` syntax, which
  broke on Windows `cmd`). (`#137`)
- **vitest hangs inside `run_shell` with network block disabled** — the pipe
  drain waited 2s for stdout then 2s for stderr sequentially (4s total), plus
  the 1s child timeout, so a grandchild holding a pipe open could push a
  timeout past its budget. The total drain is now bounded to a single shared
  2s deadline across both pipes, so a timeout returns promptly regardless of
  shell. (`#138`)
- **Max-iteration exhaustion left verified-but-uncommitted work** — when the
  iteration budget was exhausted with verified changes still uncommitted, the
  work could be lost. The checkpoint path now preserves it. (`#140`)
- **Uncommitted sub-agent work lost in parallel mode** — a sub-agent's
  uncommitted worktree changes could be silently discarded on merge. Parallel
  mode now auto-commits uncommitted worktree changes before merging, writes
  recovery patches to `.raven/recovery-sub-N.patch` on merge conflicts or
  errors, and surfaces the `recovery_patch` in the CLI. (`#139`)
- **Checkpoint auto-commit swept in collateral tracked-file deletions** — the
  harness-internal checkpoint commit used `git add -A`, which staged every
  working-tree change, including a sub-agent's collateral deletion of a
  tracked file (e.g. a failed `npm install` removing `package-lock.json`).
  `git_commit_checkpoint()` now stages additions/modifications but unstages
  tracked-file deletions before committing; the model-facing `git_commit`
  tool keeps full `git add -A` semantics. (Finding 28)
- **Windows build failed on the `skip_network_block` param** — the seccomp
  network-block exemption flag is only consumed inside the `#[cfg(unix)]`
  `pre_exec` block, so on Windows it was an unused variable and `-D warnings`
  turned it into a hard error. Added the same `cfg_attr(not(unix),
  allow(unused_variables))` already used for `workspace`/`extra_rw`.
- **`run_tests`/`run_shell` cargo-verify 'passes' tests gated to Linux** — the
  two tests that assert the verify gate credits a pass create a cargo project
  in a temp workspace and run `cargo test` through the sandbox; on Windows
  that fails at MSVC link time (`link.exe: missing operand after '\377\376'`),
  so cargo exits 101 and the fail-closed gate correctly refuses to credit.
  Gated to `#[cfg(target_os = "linux")]`, matching the existing confinement
  tests. The two 'gates' tests (which expect the gate to refuse) stay ungated.

### Changed

- **Sandbox HOME/proc grants for Node tooling** — when the workspace is under
  `$HOME`, Landlock now grants `$HOME` itself read-only (not just the leaf
  toolchain dirs), because Landlock requires Execute on every path component
  to exec a binary — granting only `~/.rustup`, `~/.cargo`, `~/.config`,
  `~/.local/share/mise` left the intermediate components ungranted and exec of
  mise-managed `node`/`npx` failed with EACCES. `/proc` is also granted
  read-only so node/v8 can read `/proc/self/status` (EACCES otherwise made
  vitest hang after tests passed). Build caches stay pinned into the workspace
  (no EXDEV risk).

## [0.1.10] - 2026-08-11

### Added

- **Optional self-hosted SearXNG backend for `web_search`** — when a base URL
  is configured (`RAVEN_SEARXNG_URL` env var or the `searxng_url` config key),
  `web_search` queries the SearXNG JSON API (`GET {base}/search?q=…&format=json`)
  and returns up to 10 results (title + URL + short snippet). `RAVEN_SEARXNG_ENGINES`
  / `searxng_engines` optionally pins the engine list. The base URL must be
  `http`/`https` only; no API key is required. On any failure (HTTP error,
  empty results, or unparseable JSON) it **falls back to DuckDuckGo**, so a
  down local instance never bricks search. Precedence: env var > config file.

### Changed

- **Sandbox temp-dir scoping** — Landlock now grants the process temp dir RW
  **only when the workspace is not under it**. Previously a workspace nested
  under `/tmp` (e.g. `evals/run.py`'s `raven-eval-…/workspace`) granted RW on
  the whole temp dir, letting a confined child write arbitrary siblings under
  `/tmp` (the `06_sandbox_escape` probe). Build caches/temps are pinned into the
  workspace via `pin_build_tool_dirs`; callers that genuinely need a sibling
  (git worktrees) pass it as an explicit extra RW root instead.
- **Git worktree confinement** — `create_worktree`/`remove_worktree` grant RW
  on just the worktree's parent dir (via a new `run_git_with_extra`), and
  parallel sub-agents get `sandbox_extra_rw` for the shared main repo's `.git`,
  instead of opening up the whole temp dir. `Sandbox` gained a
  `with_extra_rw` constructor and an `extra_rw` field threaded through
  `spawn_confined`/`run_confined`.

### Fixed

- **`git_commit` fails for workspaces under `$HOME`** — the Landlock sandbox
  granted `$HOME` read-only but, in the "workspace under HOME" branch, only
  granted RO to sibling toolchain dirs (`.rustup`, `.cargo`, `.config`), not
  `~/.gitconfig`. Git couldn't read user identity, so `git_commit` failed with
  "unable to access '~/.gitconfig': Permission denied". Now grants RO to
  `~/.gitconfig` and `~/.git-credentials` (read-only, so no write-surface
  widening). Found via stress-testing in `/home/ret/Work/raven-stress`.
- **`install.ps1` closes the window on failure** — when piped into `iex` (the
  documented one-liner), every error path called `exit 1`, which terminated the
  host PowerShell session and closed the window before the user saw the error.
  The whole install is now wrapped in a `try/catch` that prints the failure in
  red and pauses (`Press Enter to close…`, skipped when non-interactive) before
  exiting.

## [0.1.9] - 2026-08-10

### Added

- **Live agent eval suite** — `evals/run.py` runs 10 fixture cases (read-only
  symbol, single edit, multi-file refactor, fix failing test, git commit,
  sandbox escape, memory recall, skill use, plan-then-execute, add test)
  against a built headless `raven`, graded by deterministic `checks.sh`
  (not LLM-as-judge). Layer A (`cargo test eval_suite`) is an offline
  scripted-fake-model harness; Layer B is live fixtures; Layer C is the
  arena. Reports land in `evals/out/`; baseline in `evals/baselines/default.md`.
- **Cloud API support** — a repo-root/CWD `.env` is auto-loaded (without
  overriding exported vars), and API keys resolve via
  `RAVEN_API_KEY` → `OLLAMA_API_KEY` → `OPENROUTER_API_KEY` → `XAI_API_KEY` →
  `OPENAI_API_KEY`. `/model` now fetches the real context window from the
  provider's `/models` endpoint (with bearer auth) for non-Ollama providers,
  falling back to name heuristics.
- **macOS release targets** — `aarch64-apple-darwin` and `x86_64-apple-darwin`
  added to the release matrix (native runners, no cross-compile).
- **Chat toolset** — Chat mode now includes `ask_user` (and all read-only
  tools); the system prompt is mode-aware and `dispatch()` has a runtime
  read-only guard as a defense-in-depth backstop.
- **Ground-truth anchors** — the system prompt includes the current git
  working-tree state, and parallel sub-agent merge results now carry a
  `merge_status` ("merged"/"conflict"/"no changes"/"error") surfaced to the
  model so it can't assume unmerged work landed.
- **Sandbox escape hatches** — `RAVEN_SANDBOX_LANDLOCK=0` and
  `RAVEN_SANDBOX_NETWORK_BLOCK=0` to skip confinement for tests/recovery.

### Changed

- **seccomp network block** — now blocks only `socket()` for `AF_INET`/
  `AF_INET6` with `KillProcess` (immediate kill, not `EPERM`). `AF_UNIX`
  sockets and `socketpair()` are allowed, so esbuild and git ssh helpers
  work without an escape hatch while the exfiltration guarantee holds.
  **Note**: vitest/v8 still opens an AF_INET socket (for V8 coverage /
  worker IPC), so it is killed by the seccomp filter. `run_tests` on npm
  projects now sets `RAVEN_SANDBOX_NETWORK_BLOCK=0` to skip the filter for
  that one test-runner invocation — the test runner is a user-sanctioned
  command, not arbitrary model output, so the exfiltration guarantee is
  preserved.
- **Landlock ABI V3** — `AccessFs::from_all` now includes `REFER`, fixing
  `rustc` `.rmeta` hardlinks into `target/`. `CARGO_HOME`, `CARGO_TARGET_DIR`,
  `TMPDIR`, and the npm cache are pinned under `workspace/.raven/`, and `$HOME`
  is read-only (gitconfig/rustup) instead of read-write.
- **`--yolo` implies `--mode agent`** — full toolset, no plan step, no
  confirmations. An explicit `--mode` still overrides.
- **`replace_all` always writes** — when the match count exceeds the warning
  threshold it still performs the replacement and appends a warning, instead
  of silently returning success without modifying the file.
- **Release profile** — `opt-level="z"` + `panic="abort"` cut the binary from
  13M to 8.3M (36%); tokio narrowed to specific features.
- **TUI polish** — parallel tool calls deactivate the correct named block
  (no stuck spinner), idle redraw is gated on `input_dirty`, cursor alignment
  is grapheme-aware display width, a mode indicator shows in the top bar,
  tool-result previews render dim under each call, and Esc is layered
  (completion → selection → ask_user → quit).

### Fixed

- **`run_shell` hang past timeout** — the reader-thread `join()` blocked on an
  inherited stdout pipe held open by a grandchild, so a green (non-timeout)
  run could hang forever. Pipes are now drained with a bounded deadline and
  reader handles are dropped instead of joined.
- **`search_replace` on `..`-paths** — the raw path was passed to `openat2`
  instead of the lexically normalized path, so `newdir/../f.txt` failed with
  ENOENT even though it was validated as inside the workspace.
- **Iteration-budget dirty tree** — when the budget is exhausted with
  uncommitted changes, raven now injects a commit nudge and gives the model
  one more tooled iteration to checkpoint its work before summarizing.
- **`install.ps1`** — saves the Windows binary with a `.exe` extension
  (removing any old extensionless file) so it no longer triggers the
  "How do you want to open this file" dialog.
- **TUI cursor alignment** — uses grapheme-aware display width (unicode-width
  + unicode-segmentation) instead of char count, fixing CJK/emoji/combining-mark
  drift at the root rather than patching symptoms.

## [0.1.8] - 2026-08-09

### Added

- **Extendable theme system with `/theme` slash command** — the TUI's hardcoded
  `Theme` unit-struct is now a `Copy` data struct with a registry of built-in
  presets (`ravenwood`, `nord`, `dracula`, `solarized-dark`). The active theme
  is threaded through every render path (log, markdown, status strip, plan
  panel, selection highlight) and can be switched mid-session with
  `/theme <name>` (or `/theme` to list), which recolors the whole scrollback
  instantly. The theme is also configurable via `theme` in `config.toml` and a
  `--theme` CLI flag (precedence: CLI > config > default `ravenwood`).
- **Cursor-based input editing** — the chat box now tracks an edit cursor.
  `Left`/`Right` move it, `Home`/`End` jump to the start/end, and text is
  inserted/deleted at the cursor instead of only at the end.
- **Slash-command autocomplete** — typing `/` shows a popup of matching
  commands (names + aliases); `Tab` cycles the highlight (accepting when one
  match) and `Shift+Tab` cycles backward. `/theme <partial>` completes theme
  names. The popup renders between the status strip and the input box.

### Removed

- **Model-driven auto-execution** — the `exit_plan_mode` tool, the
  `AgentEvent::PlanReady` event, and all auto-execute branches (TUI + headless)
  are removed. Plan mode now always requires human approval before execution,
  which also eliminates the auto-execute hang where the plan agent kept running
  after signalling readiness.

### Fixed

- **Mouse hit-testing off by one** — the log block has `LEFT|RIGHT|BOTTOM`
  borders (no top), but both `draw_ui` and `current_display` subtracted 2 rows
  for borders, making the viewport 1 row too short. The last content row was
  never rendered and mouse selection/scroll couldn't reach it. Both now
  subtract 1.
- **Cursor out of position when input wraps** — `input_cursor_position` wrapped
  the prompt+input at `input_content_width` (which subtracts the prompt), but
  ratatui wraps the whole line at the content area width (borders only). Once
  input wrapped to a second line, the cursor landed 2 columns off. It now wraps
  at the content area width.
- **Copy-on-highlight broken when scrolled** — the mouse selection was stored
  in full-log row space (`pos.row + offset`), but `apply_selection_highlight`
  and `selection_text` read the visible window. When the log was scrolled, the
  highlight landed on the wrong rows and copy returned empty. The selection is
  now stored in visible-window coordinates, matching what's rendered. This
  became noticeable after the markdown upgrade made assistant blocks taller
  (more scrolling).
- **Right arrow couldn't reach the end of the line** — the Right-arrow handler
  used `char_indices().nth(1)`, which returns `None` when only one char remains
  after the cursor, so the cursor could never advance past the last character
  to the true end. This also meant Backspace (which deletes the char before the
  cursor) could never delete the final letter. The cursor now advances by the
  next char's byte length and reaches `input.len()`.

## [0.1.7] - 2026-08-09

### Added

- **Offline fake-model agent-loop tests** — a `#[cfg(test)]`-only
  `CompletionSource` seam on `Agent` lets `run()` be driven with scripted
  completions and no HTTP. New tests cover finish-on-content, blank-stall
  recovery + cap, tool round-trip, same-file serial edits, and the
  `max_tokens` clamp (`src/agent/tests/fake_model.rs`).
- **Compaction golden tests** — explicit assertions that history is unchanged
  below the threshold and that tool-call/result pairs are never split at the
  tail boundary (`src/context.rs`).

### Changed

- **Model-oriented repo map** — the flat, lexically-sorted symbol dump is
  replaced with a scored, grouped map that spends the char budget on
  entrypoints and public types. Symbols are ranked (entrypoint/public/type
  bonuses, test-path penalties), rendered grouped by workspace-relative path,
  and capped at 3500 chars. The map now builds when `source_files >= 15` OR
  `symbols >= 80` (was 50 files), skips `.next`/`coverage`/`vendor`/`Pods`/
  `.turbo` and files over 256 KiB, and uses a single `WalkDir` pass with
  regexes compiled once.

### Refactored

- **`src/agent.rs` split into internal submodules** — the 2687-line monolith
  is now `src/agent/` (`core`, `stream`, `tools_exec`, `loop_control`,
  `parallel`, `types`) with no public API change. `run()` is a thin
  orchestrator delegating to `handle_no_tool_calls` and `execute_tool_calls`.

## [0.1.6] - 2026-08-09

### Added

- **Markdown rendering in the TUI** — assistant output now renders as styled
  markdown (headings, bold/italic/strikethrough, inline code, fenced code
  blocks, ordered/unordered lists, blockquotes, links, tables) via a new
  `src/tui/markdown.rs` module built on `pulldown-cmark`. The renderer
  re-parses the accumulated text on each stream delta and degrades unclosed
  tokens to literal text, so streaming never flashes raw markdown.
- **Tool calls shown in the TUI** — every tool call now renders as a live
  line (previously only the newest was shown); only the newest active call
  still glimmers with a spinner. The tool's output is not dumped to the log —
  the call line alone is shown, matching Hermes' lean tool-call display.

### Fixed

- **`/model` left model-derived values stale** — the handler only updated
  `settings.model`, `context_window` (via the name heuristic), and
  `max_tokens`. It now fetches the live Ollama `/api/show` context window
  (falling back to the heuristic when unreachable), recomputes the
  `compact_at` threshold, refreshes the two static header blocks in place, and
  persists the new model to the session so a resume shows it.

## [0.1.5] - 2026-08-09

### Fixed

- **Blank model turn treated as a clean finish** — when the model returned a
  turn with no tool calls AND empty/whitespace-only content, raven treated it
  as a legitimate finish: it pushed an empty assistant message, emitted `Done`,
  and returned, silently dropping the deliverable (the research-report session
  that exposed this did all its work then ended with no report). A blank turn
  is now treated as a STALL: raven injects an ephemeral nudge and re-runs the
  loop, capped at 3 attempts, then falls through to `emit_summary` so the turn
  always ends with a visible line. Mirrors the existing verify / repeated-
  failure loop-breaker plumbing. (`#110`)
- **Parallel same-file edits silently lost** — multiple file-mutating tool
  calls (`write_file`/`search_replace`/`apply_patch`) against the same file in
  one turn were dispatched in parallel on the blocking pool. Because
  `search_replace` is an unlocked read-modify-write, two concurrent same-file
  edits both read the original content, so the last writer won and the earlier
  edit was lost while each call still returned success. File-mutating tools are
  now dispatched serially in call order via a shared `record_tool_result`
  helper; read-only and other tools stay parallel. (`#111`)

## [0.1.4] - 2026-08-09

### Fixed

- **Node tooling in sandbox** — removed the `RLIMIT_AS` (virtual address
  space) and `RLIMIT_NPROC` (per-user thread/process) caps that were applied
  to every confined subprocess. Both are the wrong tool: `RLIMIT_AS` bounds
  virtual, not resident, memory and aborted V8/Node at startup (CodeRange
  OOM); `RLIMIT_NPROC` is a user-global ceiling, not a per-child one, so it
  broke high-thread runtimes (Node) on busy machines and could not isolate a
  child. Kept the per-process `RLIMIT_CPU`, `RLIMIT_FSIZE`, and
  `RLIMIT_NOFILE` caps. This fixes the agent being unable to run `npm`,
  `npx tsc`, `node`, etc. in a workspace.
- **`write_file`/`search_replace` on missing intermediate dirs** — file writes
  no longer fail when an intermediate directory in a `..`-path is missing; the
  parent directory is created first (workspace-confined).
- **Windows worktree paths** — `write_file`/`search_replace` no longer fail
  with "Path outside workspace" on Windows when the workspace is not
  canonicalized. The code now passes the original relative path to
  `open_beneath` instead of re-resolving a canonicalized `\\?\`-prefixed path.

### Changed

- **Sandbox resource limits** — `docs/security.md` updated to document the
  omitted `RLIMIT_AS`/`RLIMIT_NPROC` and the rationale for keeping only the
  per-process limits. Windows Job Object memory caps count *committed* memory,
  not virtual address space, so they remain and are documented as such.

### Security

- Removing `RLIMIT_AS`/`RLIMIT_NPROC` closes a tool-compatibility gap without
  opening a hole: Landlock still confines the filesystem, `RLIMIT_CPU`/
  `RLIMIT_FSIZE` bound runaway execution and writes, and seccomp / Job
  Objects still provide network and process-tree confinement.

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
  seccomp (network syscall block), and `setrlimit` (CPU / file size / file
  descriptors).
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

[0.4.0]: https://github.com/raythurman2386/raven/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/raythurman2386/raven/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/raythurman2386/raven/compare/v0.2.7...v0.3.0
[0.2.7]: https://github.com/raythurman2386/raven/compare/v0.2.6...v0.2.7
[0.2.6]: https://github.com/raythurman2386/raven/compare/v0.2.5...v0.2.6
[0.2.2]: https://github.com/raythurman2386/raven/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/raythurman2386/raven/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/raythurman2386/raven/compare/v0.1.10...v0.2.0
[0.1.10]: https://github.com/raythurman2386/raven/compare/v0.1.9...v0.1.10
[0.1.9]: https://github.com/raythurman2386/raven/compare/v0.1.8...v0.1.9
[0.1.8]: https://github.com/raythurman2386/raven/compare/v0.1.7...v0.1.8
[0.1.7]: https://github.com/raythurman2386/raven/compare/v0.1.6...v0.1.7
[0.1.6]: https://github.com/raythurman2386/raven/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/raythurman2386/raven/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/raythurman2386/raven/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/raythurman2386/raven/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/raythurman2386/raven/compare/v0.1.1...v0.1.2
