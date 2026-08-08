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
The child is granted read/write only under the workspace, temp dirs, the user's
HOME, and `/dev`; read-only for `/usr`, `/bin`, `/lib`, `/lib64`, `/etc`.
Everything else is denied by the kernel.

**Why HOME and /dev are allowed:** Most dev tools (git, npm, cargo) need to
read/write config and caches under HOME, and `/dev/null` is needed by many
tools. Blocking these entirely would break the tool. This is a documented,
pragmatic tradeoff — the primary protection is confining to the workspace, but
HOME is a necessary exception.

**Platforms:** Linux only (Landlock is a Linux LSM, kernel 5.13+).

**Limitation:** Best-effort. If the kernel doesn't support Landlock, Raven logs
a warning and continues without it. The `CompatLevel::BestEffort` mode means
unsupported features are silently downgraded — a security reviewer should
verify the kernel supports Landlock.

### 3. seccomp network block (Linux)

**What it does:** Applies a seccomp-BPF filter to every subprocess that denies
16 network syscalls (`socket`, `connect`, `sendto`, `sendmsg`, `bind`,
`accept`, `listen`, `recvfrom`, `recvmsg`, `socketpair`, `setsockopt`,
`getsockopt`, `shutdown`, `getpeername`, `getsockname`, `accept4`) with
`EPERM`.

**Why:** This closes the exfiltration hole. Even if the model reads a sensitive
file, the subprocess cannot send it anywhere over the network.

**Platforms:** Linux only (seccomp is a Linux feature). Supported arches:
x86_64, aarch64, riscv64.

**Limitation:** Best-effort. If the arch is unsupported, Raven logs and
continues without the filter. Also, the *Raven process itself* (not the
subprocess) still has network access — the model API call goes through the
parent process, which is not seccomp-confined.

### 4. Resource limits (`setrlimit`)

**What it does:** Applies `RLIMIT_CPU` (30s), `RLIMIT_AS` (1 GiB),
`RLIMIT_FSIZE` (64 MiB), `RLIMIT_NOFILE` (1024), `RLIMIT_NPROC` (1024) to every
subprocess.

**Why:** Kills fork bombs (`RLIMIT_NPROC`), runaway memory (`RLIMIT_AS`),
oversized writes (`RLIMIT_FSIZE`), runaway CPU (`RLIMIT_CPU`), and fd exhaustion
(`RLIMIT_NOFILE`).

**Platforms:** Linux + macOS.

**Limitation:** Best-effort — a kernel that doesn't support a limit is ignored.

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
  `$`, parens, newlines), it is run via `Command::new(bin).args(...)` directly
  — **no shell, no injection surface**. This flips the model from "denylist
  dangerous" toward "allowlist safe" for the common case.

**Platforms:** All.

**Limitation:** The shell fallback path (commands with metacharacters) still
goes through `sh -c`, which is inherently injection-prone. The denylist is not
a security boundary. The real safety net is `confirm_shell` (user approval) and
the OS-level layers above.

---

## What Raven does NOT do

- **No OS-level kernel sandbox on Windows.** Windows is best-effort: path
  confinement + shell filtering apply, but there is no Landlock/seccomp
  equivalent. This is the same posture as most tools in this space.
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
```

These tests verify that a confined child cannot read outside the Landlock
allowlist, cannot make network syscalls, cannot write more than `RLIMIT_FSIZE`,
and that `open_beneath` rejects traversal/symlink escapes.
