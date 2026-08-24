#!/usr/bin/env python3
"""Raven live eval runner (Layer B).

Copies each case repo to a temp dir, runs headless `raven`, then executes
`checks.sh`. Writes JSON + Markdown reports under evals/out/.

Usage:
  python3 evals/run.py --smoke
  python3 evals/run.py --case 02_single_edit
  python3 evals/run.py --model MODEL --host URL
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

ROOT = Path(__file__).resolve().parent
REPO_ROOT = ROOT.parent
CASES_DIR = ROOT / "cases"
OUT_DIR = ROOT / "out"
DEFAULT_BIN_CANDIDATES = [
    REPO_ROOT / "target" / "release" / "raven",
    REPO_ROOT / "target" / "debug" / "raven",
    Path(shutil.which("raven") or ""),
]


def load_dotenv(path: Path) -> None:
    """Load KEY=VALUE pairs from a .env file into os.environ (no overwrite)."""
    if not path.is_file():
        return
    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        if not key or key in os.environ:
            continue
        val = val.strip()
        if len(val) >= 2 and val[0] == val[-1] and val[0] in "\"'":
            val = val[1:-1]
        os.environ[key] = val


def looks_authenticated_host(host: str) -> bool:
    h = host.lower()
    return any(
        s in h
        for s in (
            "openrouter.ai",
            "api.openai.com",
            "api.x.ai",
            "api.anthropic.com",
            "ollama.com",
            "together.xyz",
            "groq.com",
            "fireworks.ai",
        )
    )


@dataclass
class CaseMeta:
    id: str
    tags: list[str] = field(default_factory=list)
    timeout_secs: int = 300
    mode: str = "agent"
    yolo: bool = True
    flaky: bool = False
    requires_git: bool = False
    expect_raven_fail: bool = False
    skip_live: bool = False
    stdin_approve: bool = False
    # Skip unless sys.platform matches: "windows" (win32), "linux", "macos" (darwin).
    requires_os: str = ""


@dataclass
class CaseResult:
    id: str
    status: str  # pass | fail | skip | timeout | error
    seconds: float
    raven_exit: int | None = None
    checks_exit: int | None = None
    tool_calls: int = 0
    iterations: int = 0
    dirty_tree: bool = False
    flaky: bool = False
    message: str = ""
    tags: list[str] = field(default_factory=list)


def parse_simple_toml(text: str) -> dict[str, Any]:
    """Minimal TOML subset parser for case meta (no external deps)."""
    out: dict[str, Any] = {}
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, val = line.split("=", 1)
        key = key.strip()
        val = val.strip()
        if val.startswith("[") and val.endswith("]"):
            inner = val[1:-1].strip()
            if not inner:
                out[key] = []
            else:
                items = []
                for part in inner.split(","):
                    p = part.strip().strip('"').strip("'")
                    if p:
                        items.append(p)
                out[key] = items
        elif val in ("true", "false"):
            out[key] = val == "true"
        elif val.isdigit() or (val.startswith("-") and val[1:].isdigit()):
            out[key] = int(val)
        else:
            out[key] = val.strip('"').strip("'")
    return out


def load_meta(case_dir: Path) -> CaseMeta:
    meta_path = case_dir / "meta.toml"
    data: dict[str, Any] = {"id": case_dir.name}
    if meta_path.exists():
        data.update(parse_simple_toml(meta_path.read_text(encoding="utf-8")))
    known = {f.name for f in CaseMeta.__dataclass_fields__.values()}  # type: ignore[attr-defined]
    filtered = {k: v for k, v in data.items() if k in known}
    if "id" not in filtered:
        filtered["id"] = case_dir.name
    return CaseMeta(**filtered)


def find_raven(explicit: str | None) -> Path:
    if explicit:
        p = Path(explicit)
        if not p.is_file():
            sys.exit(f"raven binary not found: {p}")
        return p
    env = os.environ.get("RAVEN_BIN")
    if env:
        p = Path(env)
        if p.is_file():
            return p
    for c in DEFAULT_BIN_CANDIDATES:
        if c and c.is_file():
            return c
    sys.exit(
        "raven binary not found. Build with `cargo build --release` "
        "or pass --bin / set RAVEN_BIN."
    )


def list_cases(only: str | None, smoke: bool, tag: str | None) -> list[Path]:
    dirs = sorted(p for p in CASES_DIR.iterdir() if p.is_dir())
    selected: list[Path] = []
    for d in dirs:
        meta = load_meta(d)
        if only and meta.id != only and d.name != only:
            continue
        if smoke and "smoke" not in meta.tags:
            continue
        if tag and tag not in meta.tags:
            continue
        selected.append(d)
    return selected


def copy_repo(src: Path, dst: Path) -> None:
    if dst.exists():
        shutil.rmtree(dst)
    shutil.copytree(
        src,
        dst,
        ignore=shutil.ignore_patterns("target", ".git", "*.pyc", "__pycache__"),
    )


def init_git(repo: Path) -> None:
    env = os.environ.copy()
    env.setdefault("GIT_AUTHOR_NAME", "raven-eval")
    env.setdefault("GIT_AUTHOR_EMAIL", "eval@raven.local")
    env.setdefault("GIT_COMMITTER_NAME", "raven-eval")
    env.setdefault("GIT_COMMITTER_EMAIL", "eval@raven.local")
    subprocess.run(["git", "init"], cwd=repo, check=True, capture_output=True)
    # Keep planted `.env` / `.raven/` / `data/` untracked in the seed.
    # `git add -A` would otherwise bake them into the initial commit.
    subprocess.run(
        ["git", "add", "-A", "--", ".", ":!.env", ":!.env.*", ":!.raven/", ":!data/"],
        cwd=repo,
        check=True,
        capture_output=True,
    )
    subprocess.run(
        ["git", "commit", "-m", "eval seed", "--allow-empty"],
        cwd=repo,
        check=True,
        capture_output=True,
        env=env,
    )


def parse_metrics(stdout: str, stderr: str = "") -> tuple[int, int]:
    """Parse headless raven output (see src/runner.rs::drain_events)."""
    blob = f"{stdout}\n{stderr}"
    # ToolStart lines: "→ name({...})"
    tool_calls = len(re.findall(r"^→\s+\S+\(", blob, re.M))
    # Iteration markers go to stderr: "[iter N]"
    iterations = len(re.findall(r"\[iter\s+\d+\]", blob, re.I))
    return tool_calls, iterations


def is_dirty(repo: Path) -> bool:
    if not (repo / ".git").exists():
        return False
    r = subprocess.run(
        ["git", "status", "--porcelain"],
        cwd=repo,
        capture_output=True,
        text=True,
    )
    lines = []
    for line in r.stdout.splitlines():
        # ignore session store
        if ".raven/" in line or line.endswith(".raven"):
            continue
        if line.strip():
            lines.append(line)
    return bool(lines)


def run_case(
    case_dir: Path,
    raven: Path,
    model: str,
    host: str,
    api_key: str | None,
) -> CaseResult:
    meta = load_meta(case_dir)
    if meta.skip_live:
        return CaseResult(
            id=meta.id,
            status="skip",
            seconds=0.0,
            message="skip_live",
            flaky=meta.flaky,
            tags=list(meta.tags),
        )
    if meta.requires_os:
        plat = sys.platform
        want = meta.requires_os.lower()
        ok = (
            (want in ("windows", "win32") and plat == "win32")
            or (want == "linux" and plat.startswith("linux"))
            or (want in ("macos", "darwin") and plat == "darwin")
        )
        if not ok:
            return CaseResult(
                id=meta.id,
                status="skip",
                seconds=0.0,
                message=f"requires_os={meta.requires_os} (this host is {plat})",
                flaky=meta.flaky,
                tags=list(meta.tags),
            )

    task_path = case_dir / "task.md"
    checks_path = case_dir / "checks.sh"
    repo_src = case_dir / "repo"
    if not task_path.exists() or not repo_src.is_dir():
        return CaseResult(
            id=meta.id,
            status="error",
            seconds=0.0,
            message="missing task.md or repo/",
            flaky=meta.flaky,
            tags=list(meta.tags),
        )

    task = task_path.read_text(encoding="utf-8").strip()
    started = time.monotonic()

    with tempfile.TemporaryDirectory(prefix=f"raven-eval-{meta.id}-") as tmp:
        work = Path(tmp) / "workspace"
        copy_repo(repo_src, work)
        if meta.requires_git:
            try:
                init_git(work)
            except subprocess.CalledProcessError as e:
                return CaseResult(
                    id=meta.id,
                    status="error",
                    seconds=time.monotonic() - started,
                    message=f"git init failed: {e}",
                    flaky=meta.flaky,
                    tags=list(meta.tags),
                )

        stdout_path = Path(tmp) / "stdout.txt"
        stderr_path = Path(tmp) / "stderr.txt"

        # The raven CLI no longer takes --host/--api-key. Declare the eval
        # endpoint as a named provider in the workspace config and select it
        # with --provider eval. The API key is passed via env (RAVEN_API_KEY).
        cfg_dir = work / ".raven"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        cfg_lines = [
            'provider = "eval"',
            "",
            "[providers.eval]",
            f'base_url = "{host}"',
        ]
        if api_key:
            cfg_lines.append(f'api_key = "{api_key}"')
        (cfg_dir / "config.toml").write_text("\n".join(cfg_lines) + "\n", encoding="utf-8")

        cmd = [
            str(raven),
            "--headless",
            "--workspace",
            str(work),
            "--model",
            model,
            "--provider",
            "eval",
            "--mode",
            meta.mode,
            "-p",
            task,
        ]
        if meta.yolo:
            cmd.append("--yolo")

        env = os.environ.copy()
        if api_key:
            env["RAVEN_API_KEY"] = api_key

        stdin_data = "y\n" if meta.stdin_approve else None
        try:
            proc = subprocess.run(
                cmd,
                cwd=work,
                capture_output=True,
                text=True,
                timeout=meta.timeout_secs,
                env=env,
                input=stdin_data,
            )
            stdout_path.write_text(proc.stdout, encoding="utf-8")
            stderr_path.write_text(proc.stderr, encoding="utf-8")
            raven_exit = proc.returncode
        except subprocess.TimeoutExpired as e:
            out = e.stdout or ""
            err = e.stderr or ""
            if isinstance(out, bytes):
                out = out.decode("utf-8", "replace")
            if isinstance(err, bytes):
                err = err.decode("utf-8", "replace")
            stdout_path.write_text(out, encoding="utf-8")
            stderr_path.write_text(err, encoding="utf-8")
            # Capture partial progress so timeouts are diagnosable (tools/iters).
            tool_calls, iterations = parse_metrics(out, err)
            return CaseResult(
                id=meta.id,
                status="timeout",
                seconds=time.monotonic() - started,
                raven_exit=None,
                tool_calls=tool_calls,
                iterations=iterations,
                message=f"exceeded {meta.timeout_secs}s",
                flaky=meta.flaky,
                tags=list(meta.tags),
            )

        stdout = stdout_path.read_text(encoding="utf-8")
        stderr = stderr_path.read_text(encoding="utf-8")
        tool_calls, iterations = parse_metrics(stdout, stderr)
        dirty = is_dirty(work)

        if raven_exit != 0 and not meta.expect_raven_fail:
            # Still run checks — some cases grade artifacts only.
            pass

        checks_exit: int | None = None
        if checks_path.exists():
            # Ensure executable bit is not required: run via bash
            cenv = env.copy()
            cenv.update(
                {
                    "EVAL_CASE": meta.id,
                    "EVAL_STDOUT": str(stdout_path),
                    "EVAL_STDERR": str(stderr_path),
                    "EVAL_EXIT": str(raven_exit),
                    "EVAL_REPO": str(work),
                }
            )
            try:
                cproc = subprocess.run(
                    ["bash", str(checks_path)],
                    cwd=work,
                    capture_output=True,
                    text=True,
                    timeout=max(60, meta.timeout_secs // 2),
                    env=cenv,
                )
                checks_exit = cproc.returncode
                if cproc.stdout:
                    stdout_path.write_text(
                        stdout + "\n--- checks stdout ---\n" + cproc.stdout,
                        encoding="utf-8",
                    )
                if cproc.stderr:
                    stderr_path.write_text(
                        stderr_path.read_text(encoding="utf-8")
                        + "\n--- checks stderr ---\n"
                        + cproc.stderr,
                        encoding="utf-8",
                    )
            except subprocess.TimeoutExpired:
                return CaseResult(
                    id=meta.id,
                    status="timeout",
                    seconds=time.monotonic() - started,
                    raven_exit=raven_exit,
                    tool_calls=tool_calls,
                    iterations=iterations,
                    dirty_tree=dirty,
                    message="checks.sh timed out",
                    flaky=meta.flaky,
                    tags=list(meta.tags),
                )
        else:
            checks_exit = 0

        elapsed = time.monotonic() - started
        ok_checks = checks_exit == 0
        ok_raven = raven_exit == 0 or meta.expect_raven_fail
        status = "pass" if ok_checks and ok_raven else "fail"
        msg = ""
        if not ok_raven:
            msg = f"raven exit {raven_exit}"
        if not ok_checks:
            msg = (msg + "; " if msg else "") + f"checks exit {checks_exit}"

        return CaseResult(
            id=meta.id,
            status=status,
            seconds=round(elapsed, 2),
            raven_exit=raven_exit,
            checks_exit=checks_exit,
            tool_calls=tool_calls,
            iterations=iterations,
            dirty_tree=dirty,
            flaky=meta.flaky,
            message=msg,
            tags=list(meta.tags),
        )


def write_reports(run_id: str, results: list[CaseResult], meta: dict[str, Any]) -> None:
    OUT_DIR.mkdir(parents=True, exist_ok=True)
    payload = {
        "run_id": run_id,
        "meta": meta,
        "results": [asdict(r) for r in results],
        "summary": summarize(results),
    }
    json_path = OUT_DIR / f"{run_id}.json"
    md_path = OUT_DIR / f"{run_id}.md"
    json_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    md_path.write_text(render_markdown(payload), encoding="utf-8")
    print(f"\nWrote {json_path}")
    print(f"Wrote {md_path}")


def summarize(results: list[CaseResult]) -> dict[str, Any]:
    total = len(results)
    counts = {"pass": 0, "fail": 0, "skip": 0, "timeout": 0, "error": 0}
    hard_fails = 0
    for r in results:
        counts[r.status] = counts.get(r.status, 0) + 1
        if r.status in ("fail", "timeout", "error") and not r.flaky:
            hard_fails += 1
    return {
        "total": total,
        "counts": counts,
        "hard_fails": hard_fails,
        "pass_rate": (counts["pass"] / total) if total else 0.0,
    }


def render_markdown(payload: dict[str, Any]) -> str:
    s = payload["summary"]
    lines = [
        f"# Eval run `{payload['run_id']}`",
        "",
        f"- model: `{payload['meta'].get('model')}`",
        f"- host: `{payload['meta'].get('host')}`",
        f"- pass rate: {s['counts']['pass']}/{s['total']} "
        f"({s['pass_rate']*100:.0f}%)",
        f"- hard fails: {s['hard_fails']}",
        "",
        "| case | status | secs | tools | dirty | notes |",
        "|------|--------|------|-------|-------|-------|",
    ]
    for r in payload["results"]:
        lines.append(
            f"| {r['id']} | {r['status']} | {r['seconds']} | {r['tool_calls']} | "
            f"{'yes' if r['dirty_tree'] else ''} | {r.get('message','')} |"
        )
    lines.append("")
    return "\n".join(lines)


def main() -> int:
    # Repo-root .env is convenient for OpenRouter / cloud keys; do not override
    # already-exported shell vars.
    load_dotenv(REPO_ROOT / ".env")

    ap = argparse.ArgumentParser(description="Raven live agent eval runner")
    ap.add_argument("--smoke", action="store_true", help="only cases tagged smoke")
    ap.add_argument("--case", type=str, default=None, help="run a single case id")
    ap.add_argument("--tag", type=str, default=None, help="filter by tag")
    ap.add_argument("--bin", type=str, default=None, help="path to raven binary")
    ap.add_argument(
        "--model",
        type=str,
        default=None,
        help="model id (default: RAVEN_MODEL / OLLAMA_MODEL / llama3.2)",
    )
    ap.add_argument(
        "--host",
        type=str,
        default=None,
        help="OpenAI-compatible base URL (default: RAVEN_HOST / OLLAMA_HOST / local Ollama)",
    )
    ap.add_argument(
        "--api-key",
        type=str,
        default=None,
        help="API key (default: RAVEN_API_KEY / OLLAMA_API_KEY / …)",
    )
    args = ap.parse_args()

    # Resolve after dotenv. Explicit CLI flags win; otherwise env/.env; else
    # built-in defaults. Never replace an explicit --host localhost with a
    # cloud URL from .env just because it matches the built-in fallback.
    if args.api_key is None:
        args.api_key = os.environ.get("RAVEN_API_KEY") or os.environ.get("OLLAMA_API_KEY")
    if args.model is None:
        args.model = (
            os.environ.get("RAVEN_MODEL")
            or os.environ.get("OLLAMA_MODEL")
            or "llama3.2"
        )
    if args.host is None:
        args.host = (
            os.environ.get("RAVEN_HOST")
            or os.environ.get("OLLAMA_HOST")
            or "http://127.0.0.1:11434/v1"
        )

    # OpenRouter requires HTTPS; plain HTTP returns a fast 404 and zero tools.
    if "openrouter.ai" in args.host.lower() and args.host.lower().startswith("http://"):
        fixed = "https://" + args.host[len("http://") :]
        print(
            f"warning: rewriting host {args.host!r} -> {fixed!r} (OpenRouter needs HTTPS)",
            file=sys.stderr,
        )
        args.host = fixed

    if looks_authenticated_host(args.host) and not args.api_key:
        print(
            "error: host looks like a cloud API but RAVEN_API_KEY is unset.\n"
            "  export RAVEN_API_KEY=...   # or put it in repo-root .env\n"
            "  python3 evals/run.py --api-key ... --host https://openrouter.ai/api/v1",
            file=sys.stderr,
        )
        return 2

    raven = find_raven(args.bin)
    cases = list_cases(args.case, args.smoke, args.tag)
    if not cases:
        print("No cases selected.", file=sys.stderr)
        return 2

    run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    print(f"raven:  {raven}")
    print(f"model:  {args.model}")
    print(f"host:   {args.host}")
    print(f"auth:   {'yes' if args.api_key else 'no'}")
    print(f"cases:  {', '.join(c.name for c in cases)}")
    print()

    results: list[CaseResult] = []
    for case_dir in cases:
        print(f"→ {case_dir.name} ...", flush=True)
        result = run_case(case_dir, raven, args.model, args.host, args.api_key)
        results.append(result)
        mark = {"pass": "OK", "fail": "FAIL", "skip": "SKIP", "timeout": "TIME", "error": "ERR"}.get(
            result.status, result.status
        )
        print(
            f"  [{mark}] {result.id} {result.seconds}s "
            f"tools={result.tool_calls} {result.message}".rstrip(),
            flush=True,
        )

    write_reports(
        run_id,
        results,
        {
            "model": args.model,
            "host": args.host,
            "bin": str(raven),
            "smoke": args.smoke,
            "utc": run_id,
        },
    )

    summary = summarize(results)
    # Exit 1 only on non-flaky failures
    return 1 if summary["hard_fails"] else 0


if __name__ == "__main__":
    sys.exit(main())
