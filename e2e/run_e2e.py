#!/usr/bin/env python3
"""Standalone E2E test runner for mimikyu (Python 3 stdlib only)."""

from __future__ import annotations

import argparse
import io
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import unittest
from pathlib import Path
from typing import Any, NoReturn, Optional

REPO_ROOT: Path = Path(__file__).resolve().parent.parent
E2E_DIR: Path = Path(__file__).resolve().parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from e2e.local_server import LocalTestCluster  # noqa: E402


TIER_MODULES: dict[str, tuple[str, Path]] = {
    "1": ("e2e.test_tier1_features", E2E_DIR / "test_tier1_features.py"),
    "2": ("e2e.test_tier2_boundaries", E2E_DIR / "test_tier2_boundaries.py"),
    "3": ("e2e.test_tier3_cross_feature", E2E_DIR / "test_tier3_cross_feature.py"),
    "4": ("e2e.test_tier4_workloads", E2E_DIR / "test_tier4_workloads.py"),
}

ENV_KEYS: tuple[str, ...] = (
    "MIMIKYU_BIN",
    "MIMIKYU_E2E_BASE_URL",
    "MIMIKYU_E2E_CROSS_URL",
    "MIMIKYU_E2E_PROXY_URL",
    "MIMIKYU_E2E_ENGINE",
)


class _ArgumentParseError(Exception):
    """Raised cleanly on invalid CLI flags."""


class _CleanArgumentParser(argparse.ArgumentParser):
    def error(self, message: str) -> NoReturn:
        raise _ArgumentParseError(message)


def _emit_error(message: str, json_mode: bool, code: int = 2, **extra: Any) -> int:
    if json_mode:
        payload: dict[str, Any] = {"ok": False, "error": message}
        payload.update(extra)
        sys.stdout.write(json.dumps(payload, indent=2) + "\n")
        sys.stdout.flush()
    else:
        sys.stderr.write(f"error: {message}\n")
        sys.stderr.flush()
    return code


def resolve_binary(explicit_bin: Optional[str]) -> Optional[str]:
    """Resolve the mimikyu binary path; never fall back if --bin was given."""
    if explicit_bin is not None:
        return explicit_bin.strip()
    env_bin = os.environ.get("MIMIKYU_BIN", "").strip()
    if env_bin:
        return env_bin
    for candidate in (
        REPO_ROOT / "target" / "release" / "mimikyu",
        REPO_ROOT / "target" / "debug" / "mimikyu",
    ):
        if candidate.exists():
            return str(candidate)
    which_bin = shutil.which("mimikyu")
    return which_bin


def validate_binary(bin_path: Optional[str]) -> tuple[bool, str]:
    """Verify the target binary exists, is executable, and responds to `doctor --help`."""
    if not bin_path:
        return (
            False,
            "No mimikyu binary specified or found. Pass --bin <path> or set MIMIKYU_BIN.",
        )

    p = Path(bin_path)
    if not p.exists():
        return False, f"mimikyu binary does not exist: {bin_path}"
    if p.is_dir():
        return False, f"mimikyu binary path is a directory, not an executable file: {bin_path}"
    if not p.is_file():
        return False, f"mimikyu binary path is not a regular file: {bin_path}"
    if not os.access(p, os.X_OK):
        return False, f"mimikyu binary is not executable (chmod +x required): {bin_path}"

    proc: Optional[subprocess.Popen[str]] = None
    try:
        proc = subprocess.Popen(
            [str(p), "doctor", "--help"],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        stdout, stderr = proc.communicate(timeout=5.0)
    except subprocess.TimeoutExpired:
        if proc is not None:
            try:
                os.killpg(os.getpgid(proc.pid), signal.SIGKILL)
            except OSError:
                try:
                    proc.kill()
                except OSError:
                    pass
            try:
                proc.communicate(timeout=1.0)
            except (subprocess.TimeoutExpired, OSError):
                pass
        return False, f"mimikyu binary timed out running `doctor --help`: {bin_path}"
    except OSError as exc:
        return False, f"mimikyu binary failed to execute ({exc}): {bin_path}"

    if proc.returncode != 0:
        detail = (stderr or stdout or "").strip()
        suffix = f" ({detail})" if detail else ""
        return (
            False,
            f"mimikyu binary exited with code {proc.returncode} on `doctor --help`: {bin_path}{suffix}",
        )

    combined = f"{stdout}\n{stderr}".strip().lower()
    if not combined or not any(tok in combined for tok in ("doctor", "mimikyu", "usage")):
        return (
            False,
            f"mimikyu binary failed validation: `doctor --help` produced no recognizable CLI help output: {bin_path}",
        )

    return True, str(p)


def _build_parser() -> _CleanArgumentParser:
    default_engine = os.environ.get("MIMIKYU_E2E_ENGINE", "chromium").strip() or "chromium"
    if default_engine not in ("chromium", "firefox"):
        default_engine = "chromium"
    parser = _CleanArgumentParser(
        prog="run_e2e.py",
        description="Run mimikyu opaque-box E2E test suites against local 127.0.0.1:0 fixtures.",
    )
    parser.add_argument(
        "--tier",
        choices=("1", "2", "3", "4", "all"),
        default="all",
        help="Select test tier (1, 2, 3, 4, or all).",
    )
    parser.add_argument(
        "--bin",
        dest="bin_path",
        default=None,
        help="Path to compiled mimikyu binary.",
    )
    parser.add_argument(
        "--engine",
        choices=("chromium", "firefox"),
        default=default_engine,
        help="Target browser engine (chromium or firefox).",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run e2e/test_self_check.py without requiring a compiled mimikyu binary.",
    )
    parser.add_argument(
        "--json",
        dest="json_mode",
        action="store_true",
        help="Emit a machine-readable JSON summary to stdout.",
    )
    parser.add_argument(
        "-v",
        "--verbose",
        action="store_true",
        help="Verbose unittest output.",
    )
    return parser


def _run_suite(
    module_names: list[str],
    *,
    mode: str,
    tier: str,
    engine: str,
    bin_path: str,
    json_mode: bool,
    verbose: bool,
) -> int:
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for mod_name in module_names:
        suite.addTests(loader.loadTestsFromName(mod_name))

    capture = io.StringIO() if json_mode else None
    stream = capture if capture is not None else sys.stdout
    verbosity = 2 if verbose else 1
    runner = unittest.TextTestRunner(stream=stream, verbosity=verbosity)

    t0 = time.monotonic()
    result = runner.run(suite)
    duration_ms = round((time.monotonic() - t0) * 1000.0, 2)

    passed = result.wasSuccessful() and result.testsRun > 0
    if json_mode:
        summary = {
            "ok": passed,
            "mode": mode,
            "tier": tier,
            "engine": engine,
            "bin": bin_path,
            "tests_run": result.testsRun,
            "failures": len(result.failures),
            "errors": len(result.errors),
            "skipped": len(result.skipped),
            "duration_ms": duration_ms,
            "output": capture.getvalue() if capture is not None else "",
        }
        sys.stdout.write(json.dumps(summary, indent=2) + "\n")
        sys.stdout.flush()

    return 0 if passed else 1


def main(argv: Optional[list[str]] = None) -> int:
    raw_argv = list(sys.argv[1:] if argv is None else argv)
    json_hint = "--json" in raw_argv
    parser = _build_parser()

    try:
        args = parser.parse_args(raw_argv)
    except _ArgumentParseError as exc:
        return _emit_error(str(exc), json_hint, code=2)
    except SystemExit as exc:
        return int(exc.code) if isinstance(exc.code, int) else 0

    if args.self_test:
        self_test_path = E2E_DIR / "test_self_check.py"
        if not self_test_path.is_file():
            return _emit_error(
                f"Self-check module not found: {self_test_path}",
                args.json_mode,
                code=2,
            )
        return _run_suite(
            ["e2e.test_self_check"],
            mode="self-test",
            tier="self-test",
            engine=args.engine,
            bin_path="",
            json_mode=args.json_mode,
            verbose=args.verbose,
        )

    candidate_bin = resolve_binary(args.bin_path)
    valid, detail = validate_binary(candidate_bin)
    if not valid:
        return _emit_error(detail, args.json_mode, code=2, tier=args.tier)

    selected_keys = ["1", "2", "3", "4"] if args.tier == "all" else [args.tier]
    selected_modules: list[str] = []
    for key in selected_keys:
        mod_name, mod_path = TIER_MODULES[key]
        if not mod_path.is_file():
            return _emit_error(
                f"Requested tier module not found: {mod_path.relative_to(REPO_ROOT)}",
                args.json_mode,
                code=2,
                tier=args.tier,
            )
        selected_modules.append(mod_name)

    prev_env = {k: os.environ.get(k) for k in ENV_KEYS}
    with LocalTestCluster() as cluster:
        try:
            os.environ["MIMIKYU_BIN"] = detail
            os.environ["MIMIKYU_E2E_BASE_URL"] = cluster.base_url
            os.environ["MIMIKYU_E2E_CROSS_URL"] = cluster.cross_url
            os.environ["MIMIKYU_E2E_PROXY_URL"] = cluster.proxy_url
            os.environ["MIMIKYU_E2E_ENGINE"] = args.engine

            return _run_suite(
                selected_modules,
                mode="e2e",
                tier=args.tier,
                engine=args.engine,
                bin_path=detail,
                json_mode=args.json_mode,
                verbose=args.verbose,
            )
        finally:
            for key, old_val in prev_env.items():
                if old_val is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = old_val


if __name__ == "__main__":
    try:
        sys.exit(main())
    except KeyboardInterrupt:
        sys.exit(130)
    except Exception as exc:
        json_flag = "--json" in sys.argv[1:]
        sys.exit(_emit_error(f"Unexpected runner failure: {exc}", json_flag, code=1))
