#!/usr/bin/env python3
"""
Run evals against all official latest Ollama Cloud models.

Fetches the list of available models from a local Ollama instance (or cloud endpoint),
filters for official latest models, and runs the full eval suite against each.

For Ollama Cloud endpoints: prioritizes :cloud variants (won't trigger local pulls)
For local Ollama endpoints: includes untagged models, :cloud, and :latest variants

Excludes embedding models, test versions, and models that would trigger downloads.

Note: Individual model failures (e.g., auth required, model not available) are
caught and reported without stopping the full eval run.

Usage:
    # Local Ollama (default)
    python3 evals/run_all_models.py --list-only
    python3 evals/run_all_models.py                          # Run full eval suite
    
    # Specific models
    python3 evals/run_all_models.py --models qwen3.5-coder,deepseek-v4-pro:cloud
    
    # Against Ollama Cloud (requires RAVEN_API_KEY)
    python3 evals/run_all_models.py --host https://api.ollama.ai/api/v1 --api-key sk-...
"""

import argparse
import json
import os
import subprocess
import sys
from datetime import datetime
from pathlib import Path
from typing import Optional

import requests


def get_models(host: str, api_key: Optional[str] = None) -> list[str]:
    """Fetch available models from the endpoint."""
    headers = {}
    if api_key:
        headers["Authorization"] = f"Bearer {api_key}"

    # Try OpenAI-compatible /models endpoint first
    try:
        resp = requests.get(f"{host}/models", headers=headers, timeout=10)
        if resp.status_code == 200:
            data = resp.json()
            if "data" in data:
                models = [m.get("id") for m in data["data"] if m.get("id")]
                if models:
                    return models
    except Exception:
        pass

    # Fall back to local Ollama /api/tags endpoint (not for cloud)
    is_cloud_endpoint = "ollama.ai" in host or "ollama.com" in host
    if not is_cloud_endpoint:
        try:
            ollama_host = host.replace("/v1", "")
            resp = requests.get(f"{ollama_host}/api/tags", headers=headers, timeout=10)
            if resp.status_code == 200:
                data = resp.json()
                if "models" in data:
                    return [m.get("name") for m in data["models"] if m.get("name")]
        except Exception:
            pass

    return []


def is_official_model(model: str, endpoint: str) -> bool:
    """
    Determine if a model is an official release worthy of evaluation.
    
    For Ollama Cloud endpoints: prefer :cloud variants (won't trigger local pulls)
    For local Ollama: include untagged and :latest variants
    
    Excludes test models, dev versions, and embedding models.
    """
    lower = model.lower()
    
    # Exclude embedding and specialized models
    if any(x in lower for x in [
        "embed", "nomic-embed", "mxbai-embed", "all-minilm",
        "test", "dev", "sandbox", "debug"
    ]):
        return False
    
    is_cloud_endpoint = "ollama.ai" in endpoint or "ollama.com" in endpoint
    
    if is_cloud_endpoint:
        # For cloud: only include :cloud variants (don't trigger downloads)
        return model.endswith(":cloud")
    else:
        # For local: include :cloud, :latest, and untagged models
        if model.endswith(":cloud") or model.endswith(":latest"):
            return True
        if ":" not in model:
            return True
    
    return False


def filter_latest_models(models: list[str], endpoint: str) -> list[str]:
    """
    Filter to official latest models, prioritized by endpoint type.
    
    Cloud endpoint: prioritize :cloud variants (won't trigger local pulls)
    Local endpoint: untagged base models, then :cloud, then :latest
    """
    official = [m for m in models if is_official_model(m, endpoint)]
    
    is_cloud_endpoint = "ollama.ai" in endpoint or "ollama.com" in endpoint
    
    # Sort for consistent results
    def sort_key(model):
        if is_cloud_endpoint:
            # Cloud: all are :cloud, just sort alphabetically
            return (0, model)
        else:
            # Local: prefer untagged, then :cloud, then :latest
            if ":" not in model:
                return (0, model)
            elif model.endswith(":cloud"):
                return (1, model)
            elif model.endswith(":latest"):
                return (2, model)
            else:
                return (3, model)
    
    official.sort(key=sort_key)
    return official


def run_eval(model: str, host: str, api_key: Optional[str] = None) -> tuple[bool, str]:
    """
    Run the eval suite for a single model.
    
    Returns: (success: bool, error_message: str or "")
    """
    raven_binary = Path(__file__).parent.parent / "target" / "release" / "raven"
    if not raven_binary.exists():
        return False, f"Raven binary not found: {raven_binary}. Run: cargo build --release"

    cmd = [
        sys.executable,
        str(Path(__file__).parent / "run.py"),
        "--model",
        model,
        "--host",
        host,
    ]

    if api_key:
        cmd.extend(["--api-key", api_key])

    try:
        result = subprocess.run(
            cmd,
            cwd=Path(__file__).parent,
            capture_output=True,
            text=True,
            timeout=300,  # 5 minute timeout per model
        )
        if result.returncode == 0:
            return True, ""
        else:
            # Capture last few lines of stderr for context
            error_lines = result.stderr.split("\n")[-3:]
            error_msg = " | ".join(line.strip() for line in error_lines if line.strip())
            return False, error_msg or result.stderr[:100]
    except subprocess.TimeoutExpired:
        return False, "Eval timeout (>5min)"
    except Exception as e:
        return False, str(e)


def main():
    parser = argparse.ArgumentParser(
        description="Run evals against all official latest Ollama Cloud models",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument(
        "--host",
        default="http://127.0.0.1:11434/v1",
        help="Model endpoint URL (default: local Ollama)",
    )
    parser.add_argument(
        "--api-key",
        help="Bearer token for authentication (or set RAVEN_API_KEY env var)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="List models but don't run evals",
    )
    parser.add_argument(
        "--list-only",
        action="store_true",
        help="Same as --dry-run (just list models)",
    )
    parser.add_argument(
        "--models",
        help="Comma-separated list of specific models to test (overrides fetch)",
    )
    args = parser.parse_args()

    api_key = args.api_key or os.getenv("RAVEN_API_KEY")

    # Get models to test
    if args.models:
        models = [m.strip() for m in args.models.split(",")]
    else:
        print(f"Fetching available models from {args.host}...", file=sys.stderr)
        all_models = get_models(args.host, api_key)
        if not all_models:
            print("❌ No models found", file=sys.stderr)
            return 1

        models = filter_latest_models(all_models, args.host)
        print(f"\n📊 Model Inventory:", file=sys.stderr)
        print(f"   Total available: {len(all_models)}", file=sys.stderr)
        print(f"   Official latest: {len(models)}", file=sys.stderr)
        print(f"\n🧪 Testing ({len(models)} models):", file=sys.stderr)
        for m in models:
            print(f"   - {m}", file=sys.stderr)

    if not models:
        print("❌ No models to test", file=sys.stderr)
        return 1

    if args.dry_run or args.list_only:
        print(f"\n✓ Dry-run complete: {len(models)} official models ready to evaluate")
        return 0

    # Run evals
    start_time = datetime.now()
    results = {}
    for i, model in enumerate(models, 1):
        print(f"\n{'=' * 70}")
        print(f"[{i}/{len(models)}] Testing {model}...")
        print(f"{'=' * 70}")
        success, error = run_eval(model, args.host, api_key)
        results[model] = (success, error)

    # Summary
    passed = sum(1 for v, _ in results.values() if v)
    total = len(results)
    elapsed = datetime.now() - start_time

    print(f"\n{'=' * 70}")
    print(f"✨ EVAL RUN COMPLETE")
    print(f"{'=' * 70}")
    print(f"Total models:  {total}")
    print(f"Passed:        {passed} ✓")
    print(f"Failed:        {total - passed} ✗")
    print(f"Elapsed:       {elapsed}")
    print()

    for model, (success, error) in results.items():
        if success:
            print(f"  ✓ {model}")
        else:
            print(f"  ✗ {model}")
            if error:
                print(f"    └─ {error}")

    print(f"{'=' * 70}\n")

    return 0 if passed == total else 1


if __name__ == "__main__":
    sys.exit(main())
