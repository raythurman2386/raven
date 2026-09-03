# Security model

This document describes what **Raven** protects against, what it does **not**
protect against, and why each boundary is what it is. It is written to be
reviewable — the goal is an honest, auditable threat model, not a marketing
claim.

Raven is a local coding-agent harness. It runs an LLM agent loop that can read,
write, and execute commands in a workspace. The security question is: **what
can the agent do, and what stops it from doing more?**

---

## Threat model

Raven's primary threat is a **compromised or misbehaving model** — the LLM
producing tool calls that read, write, or execute something it shouldn't. This
could be:

- **Path traversal** — the model asks to read/write a file outside the workspace.
- **Symlink escape** — a symlink inside the workspace points outside it.
- **Shell injection** — a command string smuggles extra commands.
- **Exfiltration** — the model reads a sensitive file and sends it somewhere.
- **Resource exhaustion** — a runaway command (fork bomb, huge write, infinite loop).
- **Destructive commands** — `rm -rf /`, `mkfs`, etc.

Raven is **not** a defense against a malicious *user* who already has shell
access to the machine. If the user can run `raven`, they can run anything.
Raven confines the *agent*, not the user.

---

## Defense layers

Raven applies several independent layers. Each is best-effort on some
platforms; none is a complete boundary on its own. Defense-in-depth is the
point.

### 1. Path confinement (`openat2` / `RESOLVE_BENEATH`)

**What it does:** On Linux, file opens go through `openat2` with
`RESOLVE_BENEATH | NO_MAGICLINKS`, rooted at the workspace directory. The
kernel refuses to resolve any path that escapes the workspace — **atomically**,
with no TOCTOU race. A symlink cannot be swapped in between the check and the
open.

**Platforms:** Linux only. On other platforms, falls back to lexical `..`
rejection + canonicalization of the nearest existing ancestor (see
`safe_resolve`), which is still a real check but has a small TOCTOU window.

**Limitation:** `openat2` confines *path resolution*, not the process. A
confined child could still open a file it already has an fd to, or use a
different syscall. This is why Landlock (below) is the stronger boundary for
subprocesses.

### 2. Landlock filesystem confinement (Linux)

**What it does:** Applies a Landlock ruleset to every subprocess before `exec`.
The child is granted **read/write** under the workspace, any explicit extra
roots (git worktree siblings), and `/dev`; **read-only** for `/usr`, `/bin`,
`/lib`, `/lib64`, `/etc`, `/proc`, and `$HOME`. Everything else is denied by
the kernel. The process temp dir is **not** granted — children get `TMPDIR`
pinned under `workspace/.raven/tmp`. Granting `/tmp` was a real escape:
`run_shell` could write `/tmp/raven_eval_escape_probe.txt` from a normal
home-directory workspace.

**Why ABI V3 + pinned caches:** Landlock ABI V1 does not include `REFER`.
Without it, `rename`/`link` across directories fails with `EXDEV` even under
the same allowed tree — which is how `rustc` stages `.rmeta` into `target/`.
Raven therefore:

1. Uses Landlock **ABI V3** so `AccessFs::from_all` includes `REFER`.
2. Pins `CARGO_HOME`, `CARGO_TARGET_DIR`, `TMPDIR`, and the npm cache under
   `workspace/.raven/` so package caches do not hardlink from `$HOME` into
   `target/` across separate hierarchies.
3. Grants `$HOME` **read-only** (not RW) so git can read `~/.gitconfig` and
   rustup can read toolchains, without letting builds write caches into HOME.

**Why /dev is allowed:** git and many tools need `/dev/null` open for
read+write.

**Escape hatch:** `RAVEN_SANDBOX_LANDLOCK=0` skips Landlock (tests / recovery).

**Platforms:** Linux only (Landlock is a Linux LSM, kernel 5.13+; REFER needs
5.19+ / ABI V2+).

**Limitation:** Best-effort. If the kernel doesn't support Landlock, Raven logs
a warning and continues without it. The `CompatLevel::BestEffort` mode means
unsupported features are silently downgraded — a security reviewer should
verify the kernel supports Landlock.

### 3. seccomp network block (Linux)

**What it does:** Applies a seccomp-BPF filter to every subprocess that denies
`socket()` when the domain is `AF_INET` or `AF_INET6` with `KillProcess`
(immediate kill, not `EPERM`). `AF_UNIX` sockets are allowed (needed by
esbuild, vitest, git ssh helpers, etc.), and `socketpair()` is not blocked
(it only supports `AF_UNIX` on Linux). All other network syscalls (`connect`,
`sendto`, etc.) are allowed because no internet-facing socket can be created.

**Why:** This closes the exfiltration hole. Even if the model reads a sensitive
file, the subprocess cannot create an internet-facing socket to send it
anywhere over the network.

**Platforms:** Linux only (seccomp is a Linux feature). Supported arches:
x86_64, aarch64, riscv64.

**Limitation:** Best-effort. If the arch is unsupported, Raven logs and
continues without the filter. Also, the *Raven process itself* (not the
subprocess) still has network access — the model API call goes through the
parent process, which is not seccomp-confined.

**Escape hatch:** Set `RAVEN_SANDBOX_NETWORK_BLOCK=0` to skip the filter
entirely (e.g. if a legitimate tool needs network access).

### 4. Resource limits (`setrlimit`)

**What it does:** Applies `RLIMIT_CPU` (30s), `RLIMIT_FSIZE` (248 MiB), and
`RLIMIT_NOFILE` (1024) to every subprocess.

**Why:** Caps oversized writes (`RLIMIT_FSIZE`), runaway CPU (`RLIMIT_CPU`),
and fd exhaustion (`RLIMIT_NOFILE`). The 248 MiB write cap stays above real
toolchain outputs (raven's own debug test binary is ~280 MiB with full
debuginfo) while bounding a runaway write.

**Exemption for sanctioned verification commands:** `run_tests`, `run_lint`,
and `run_shell` commands that match the verification-gate predicate
(`cargo test`, `cargo clippy`, `cargo fmt --check`, `npm test`, `pytest`,
`tsc`, `eslint`, …) skip rlimits entirely. These commands legitimately need
to write linker outputs larger than the cap (a debug test binary with full
debuginfo can exceed it, which would SIGXFSZ-kill the linker) and to burn more
than 30s of CPU on a clean build. The matcher tolerates inert prefixes — a
single leading `cd <dir> &&`, leading `VAR=value` assignments — and a single
trailing pipe into a read-only output consumer (`tail`, `head`, `grep`,
`sed -n`, `sort`, `uniq`, `wc`); anything executable chained after the
sanctioned command (`&&`, `;`, a non-reader pipe) does not match. The
exemption mirrors the seccomp network-block exemption already granted to the
same sanctioned commands, and is limited to commands the enforced-verify gate
would credit — not arbitrary model output. Landlock and seccomp still apply.

**Platforms:** Linux + macOS.

**Limitation:** Best-effort — a kernel that doesn't support a limit is ignored.

**Note on omitted limits:** Memory and process counts are deliberately *not*
capped via `setrlimit`:
- `RLIMIT_AS` (virtual address space) is omitted because runtimes like V8/Node
  reserve large regions up front — a 1 GiB cap made Node abort at startup. It
  bounds virtual, not resident, memory, so it is the wrong tool for limiting
  memory use anyway.
- `RLIMIT_NPROC` (processes/threads per user) is omitted because it is a
  *user-global* ceiling, not a per-child one. Imposing it on a child cannot
  isolate that child — a fork bomb would instead kill the entire user session
  — and on a busy machine it silently breaks high-thread runtimes (Node, etc.)
  because the ambient thread count is already near the cap.

The remaining layers bound the practical damage: Landlock confines the
filesystem, and `RLIMIT_CPU`/`RLIMIT_FSIZE` stop runaway execution and writes.

### 5. Shell safety (denylist + allowlist + direct exec)

**What it does:**
- **Denylist** (`dangerous_re`): blocks obviously destructive patterns
  (`rm -rf /`, `mkfs`, fork bombs, `curl | sh`, `dd` to block devices, etc.).
  This is a **best-effort guard, not a security boundary** — a denylist is
  inherently incomplete.
- **Allowlist** (`safe_command_re`): matches known-safe development commands.
  When `confirm_shell` is enabled (the default, non-`--yolo` path), commands
  matching the allowlist run without a confirmation prompt. Anything outside
  requires explicit user approval.
- **Direct exec**: when a command's first token is on the allowlist AND the
  command contains no shell metacharacters (`;`, `&`, `|`, `>`, `<`, backticks,
  `$`, parens, braces, `!`, `^`, CR/LF/NUL), it is run via
  `Command::new(bin).args(...)` directly — **no shell, no injection surface**.
  This flips the model from "denylist dangerous" toward "allowlist safe" for
  the common case.
- **Never-execute denylist even under `--yolo`**: pipe-to-shell, reverse-shell
  primitives (`/dev/tcp`, `nc -e`, `mkfifo`), encoded PowerShell, `certutil
  -decode`, and `Invoke-Expression` / `iex (` are blocked before spawn. This
  is still a denylist, not a boundary.
- **Invocation log**: every `run_shell` that actually starts (allowlisted,
  confirmed, or `--yolo`) is recorded in the session `debug-events.jsonl` as
  a local-only `shell` event. Declined confirmations are not logged as
  executed commands.

**Platforms:** All.

**Limitation:** The shell fallback path (commands with metacharacters) still
goes through `sh -c`, which is inherently injection-prone. The denylist is not
a security boundary. The real safety net is `confirm_shell` (user approval) and
the OS-level layers above.

### 6. Tool-argument hygiene

**What it does:** Before dispatch, Raven rejects tool calls whose arguments
JSON exceeds 1 MiB, are not a JSON object, or omit/mis-type required fields
(empty `run_shell.command`, missing `read_file.path`, …). Path arguments are
capped at 4096 characters; shell commands at 32 KiB.

**Limitation:** This is schema hygiene, not an allowlist of values. A
well-formed `run_shell` still has to pass the denylist / confirm_shell /
OS layers.

---

## What Raven does NOT do

- **No container/VM isolation.** Raven does not require Docker or any
  container runtime. If you need stronger isolation, run Raven inside a
  container or VM yourself.
- **No network sandbox on macOS.** seccomp is Linux-only; macOS gets rlimits
  but not the network block.
- **No protection against a malicious user.** If the user can run `raven`,
  they can run anything.

---

## Recommended deployment for high-security environments

For a locked-down environment (e.g. a government agency), the recommended
deployment is:

1. **Run on Linux** (kernel 5.13+) so Landlock + seccomp are active.
2. **Verify Landlock is enforced** — check the log for
   `Landlock not enforced` warnings at startup.
3. **Keep `confirm_shell` enabled** (the default). Do not use `--yolo` unless
   you fully trust the model and the workspace.
4. **For the strongest isolation**, run Raven inside a container or VM. Raven
   does not require this, but it adds a boundary that Raven itself does not
   provide.

---

## Verification

Run the test suite to verify the confinement layers are active:

```bash
cargo test --lib tools::tests::confined_child
cargo test --lib tools::tests::open_beneath
cargo test --lib tools::tests::is_direct_exec_command
cargo test --lib tools::tests::confined_child_oversized_write_capped_by_fsize
cargo test --lib tools::validate
```

These tests verify that a confined child cannot read outside the Landlock
allowlist, cannot make network syscalls, cannot write more than `RLIMIT_FSIZE`,
and that `open_beneath` rejects traversal/symlink escapes.

---

## System scope (`--system`)

Raven also ships an opt-in `--system` scope for OS administration
(`raven --system "…"`). This scope is **fundamentally different** from the
repo default and must be understood as a separate trust posture:

- **The sandbox is rooted at `/`.** Because the entire sandbox (`Sandbox.workspace`,
  the Landlock RW roots, `safe_resolve`, `open_beneath`) anchors on the workspace
  root, setting that root to `/` in system scope grants the agent write access
  to *every path on the machine*. `safe_resolve`'s lexical/symlink checks and
  `openat2/RESOLVE_BENEATH` become effectively no-ops, because every path is
  under `/`. The Landlock `/` RW grant unions over the usual read-only subpaths
  (`/etc`, `/usr`, …), so those are writable too. This is intentional: it is
  what makes `raven --system` able to manage system configuration, packages,
  and services.
- **The real safety net is `confirm_shell`, not the sandbox.** Every `run_shell`
  command must be user-approved unless it matches an allowlist: the general
  dev allowlist (`safe_command_re` — `cargo`, `git`, `ls`, …) everywhere, plus
  a **system-scope allowlist** (`system_safe_command_re`) that auto-runs
  read-only diagnostics — `pacman` query/search/info, `systemctl`
  status/list/show/cat, `journalctl`, `coredumpctl`, `hyprctl` reads, hardware
  readers, `omarchy` informational commands — so the system agent can inspect
  freely. State-changing operations (package install/remove/upgrade, service
  start/stop/restart, `omarchy install/refresh/theme set/pkg`, `sudo`, kills,
  power ops) always prompt unless the user passes `--yolo`. **`--system
  --yolo` is a supported explicit opt-in** to a fully autonomous system agent
  on a trusted single-user machine; without it, a system-scope agent is never
  fully autonomous.
- **The `dangerous_re` denylist still applies** as a last-resort filter for
  obviously destructive commands (recursive root deletes, `mkfs`, fork bombs,
  `curl | sh`, …) — **including under `--yolo`**.
- **`--system` suppresses repo behaviors**: no enforced-verify gate (no
  workspace test runner), no `delegate_task`, no goal/todo persistence, and a
  separate system prompt (`SYSTEM_SCOPE_BASE`) that instructs the agent to
  prefer `omarchy`/`systemctl`/`pacman` commands, never edit
  `/usr/share/omarchy/`, back up configs before writing, and confirm
  destructive work.
- **Network access in shell commands is blocked by default** (seccomp
  AF_INET/AF_INET6 kill) in *both* scopes. Package installs, downloads, and
  DNS-touching diagnostics fail with a deterministic SIGSYS kill unless the
  user opts out explicitly with `RAVEN_SANDBOX_NETWORK_BLOCK=0` (e.g. in
  `~/.raven/.env`). The opt-out is environment-level, deliberate, and
  survives `--yolo`: the network block is a separate layer from the
  confirmation gate.
- **Sessions persist as an audit trail.** System-scope sessions (TUI or
  headless one-shot) are written to `~/.raven/system/sessions/` — deliberately
  outside `/`-rooted paths and matching the system-memory convention
  (`~/.raven/system/MEMORY.md`). Transcripts therefore record exactly which
  commands the privileged agent ran, with the user's approval decisions
  implicit in what executed. Treat this directory as sensitive: it can contain
  the output of privileged commands.
- **Scope is a first-class axis through the standard harness**: the TUI,
  headless one-shot runner, plan flow, and session tooling (`/new`,
  `/cleanup`, `/export`, `--resume`) all work in system scope against the
  system store; `raven --system` with no prompt opens the same TUI as the
  repo default, with the OS-administration prompt and write-anywhere sandbox.

**Recommendation:** use `--system` only on a trusted, single-user machine. It
is not a hardening boundary — it is a convenience scope whose default
protection is the tiered confirmation gate (diagnostics auto-run, mutations
prompt) and the command denylist. `--system --yolo` removes the per-command
gate entirely (the denylist and network block remain); treat it as appropriate
only where the user is the sole operator of the machine and accepts the blast
radius. For stronger isolation of system-administration work, run it inside a
container or VM, or audit the session transcripts under
`~/.raven/system/sessions/` afterward.

See `docs/omarchy.md` for how the system scope complements the repo default on
an Omarchy machine.
