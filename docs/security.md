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

**What it does:** Applies `RLIMIT_CPU` (30s), `RLIMIT_FSIZE` (64 MiB), and
`RLIMIT_NOFILE` (1024) to every subprocess.

**Why:** Caps oversized writes (`RLIMIT_FSIZE`), runaway CPU (`RLIMIT_CPU`),
and fd exhaustion (`RLIMIT_NOFILE`).

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
filesystem, `RLIMIT_CPU`/`RLIMIT_FSIZE` stop runaway execution and writes, and
Windows Job Objects bound committed memory separately (see §5).

### 5. Windows Job Objects (resource limits + process-tree confinement)

**What it does:** On Windows, every subprocess Raven spawns is assigned to a
fresh Job Object configured with:

- `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — all processes in the job are killed
  when the job handle closes, so a runaway child (and its entire process tree)
  cannot outlive the parent Raven process.
- `JOB_OBJECT_LIMIT_ACTIVE_PROCESS` (max 256) — caps the process tree size.
- `JOB_OBJECT_LIMIT_PROCESS_MEMORY` / `JOB_OBJECT_LIMIT_JOB_MEMORY` (1 GiB
  each) — caps per-process and per-job committed memory. These count *resident*
  (committed) memory, not virtual address space, so unlike the Unix `RLIMIT_AS`
  they do not break runtimes like V8/Node that reserve large virtual regions.

The child is opened with the minimal access rights (`PROCESS_SET_QUOTA |
PROCESS_QUERY_INFORMATION | PROCESS_TERMINATE`) — deliberately *not*
`PROCESS_ALL_ACCESS`.

**Why:** Job Objects are the Windows-native equivalent of the Unix
Landlock/seccomp/rlimits model. A process in a job cannot escape it, so a
misbehaving child's grandchildren are confined to the same limits, and a
fork-bomb/runaway process is killed when Raven exits.

**Platforms:** Windows only.

**Limitation:** Best-effort. If creating or assigning the Job Object fails
(kernel policy, handle exhaustion), Raven logs a warning and the child runs
without job confinement — the same posture as the Unix layers. Job Objects do
not provide filesystem confinement (that is Landlock's role, which has no
Windows equivalent) or a network block (seccomp, also Linux-only).

### 6. Shell safety (denylist + allowlist + direct exec)

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

- **No Windows filesystem confinement.** Windows gets Job Objects (resource
  limits + process-tree confinement + kill-on-close) and shell filtering, but
  there is no Landlock/seccomp equivalent — a subprocess on Windows can still
  read/write any file the user can, and can make network calls. This is the
  same posture as most tools in this space.
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
