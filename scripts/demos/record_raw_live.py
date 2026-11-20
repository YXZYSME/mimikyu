#!/usr/bin/env python3
"""Record a 100% raw, unstylized browser screencast of `mimikyu` on real public websites.

Zero HUD overlays, zero synthetic cursors, zero mocked pages: every frame is an
unaltered PNG streamed directly from Chromium via `mimikyu record start/stop`
(CDP `Page.startScreencast`) while `mimikyu` executes real AI-agent workflows across:
  1. https://github.com/YXZYSME/mimikyu   (live repo & README inspection + Markdown extract)
  2. https://crates.io/crates/mimikyu     (live Rust package registry verification)
  3. https://pypi.org/project/mimikyu/    (live Python package registry verification)
  4. https://news.ycombinator.com         (live @eN snapshot discovery + real link click)

Outputs (gitignored under `dist/demos/`):
  - `demo-raw-live-browser.mp4`   (1280x720 H.264 High yuv420p, raw unaltered frames)
  - `demo-raw-live-browser.webp`  (looping Animated WebP via img2webp, if available)
  - `demo-raw-poster-github.png`, `demo-raw-poster-crates.png`, `demo-raw-poster-hn.png`
  - `demo-raw-live-telemetry.json` (real measured latencies & extracted stats)
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUT_DIR = REPO_ROOT / "dist" / "demos"


def resolve_mimikyu_bin(override: str | None = None) -> Path:
    candidates: list[Path] = []
    if override:
        candidates.append(Path(override))
    if env_bin := os.environ.get("MIMIKYU_BIN"):
        candidates.append(Path(env_bin))
    candidates.append(REPO_ROOT / "target" / "release" / "mimikyu")
    candidates.append(REPO_ROOT / "target" / "debug" / "mimikyu")
    if which := shutil.which("mimikyu"):
        candidates.append(Path(which))

    for path in candidates:
        if path.is_file() and os.access(path, os.X_OK):
            return path
    raise FileNotFoundError(
        "Could not locate compiled `mimikyu` binary. Run `cargo build --release` or set MIMIKYU_BIN."
    )


def resolve_tool(name: str, fallback: str | None = None) -> str | None:
    if path := shutil.which(name):
        return path
    if fallback and Path(fallback).is_file():
        return fallback
    return None


class MimikyuRunner:
    def __init__(self, binary: Path, state_dir: Path) -> None:
        self.binary = binary
        self.state_dir = state_dir

    def run_json(self, *args: str, timeout: float = 45.0) -> tuple[dict[str, Any], float]:
        cmd = [str(self.binary), "--state-dir", str(self.state_dir), *args, "--json"]
        t0 = time.perf_counter()
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=timeout)
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        if proc.returncode != 0:
            raise RuntimeError(
                f"mimikyu failed (rc={proc.returncode}): {' '.join(args)}\n"
                f"stdout: {proc.stdout}\nstderr: {proc.stderr}"
            )
        payload = json.loads(proc.stdout)
        if not payload.get("ok"):
            raise RuntimeError(f"mimikyu ok=false for {args}: {payload}")
        return payload["data"], elapsed_ms

    def start_daemon(self) -> None:
        self.run_json("daemon", "--idle-timeout", "300")

    def stop_daemon(self) -> None:
        subprocess.run(
            [str(self.binary), "--state-dir", str(self.state_dir), "daemon", "stop"],
            capture_output=True,
            text=True,
            timeout=15.0,
            check=False,
        )


def _find_ref_by_text(snap_data: dict[str, Any], needles: tuple[str, ...]) -> str | None:
    refs = snap_data.get("refs")
    if isinstance(refs, list):
        for entry in refs:
            if not isinstance(entry, dict):
                continue
            ref = entry.get("ref")
            label = f"{entry.get('name', '')} {entry.get('text', '')}".strip().lower()
            role = str(entry.get("role", "")).lower()
            if ref and role in ("link", "button", "a") and any(label == n or n in label for n in needles):
                return str(ref)
    text = str(snap_data.get("text") or snap_data.get("snapshot") or "")
    for line in text.splitlines():
        low = line.lower()
        if "@" in line and any(f'"{n}"' in low or f" {n} " in low for n in needles):
            for tok in line.split():
                if tok.startswith("@e") and tok[2:].isdigit():
                    return tok
    return None


def _encode_raw_mp4(frames: list[Path], out_mp4: Path, ffmpeg: str) -> None:
    if not frames:
        raise RuntimeError("No screencast frames were captured!")

    mtimes = [f.stat().st_mtime for f in frames]
    concat_file = out_mp4.parent / ".raw-concat.txt"
    lines: list[str] = []
    for idx, frame in enumerate(frames):
        if idx + 1 < len(frames):
            dt = mtimes[idx + 1] - mtimes[idx]
            dur = max(0.05, min(dt, 1.25))
        else:
            dur = 1.20
        escaped = str(frame.resolve()).replace("'", "'\\''")
        lines.append(f"file '{escaped}'")
        lines.append(f"duration {dur:.4f}")
    last_escaped = str(frames[-1].resolve()).replace("'", "'\\''")
    lines.append(f"file '{last_escaped}'")
    concat_file.write_text("\n".join(lines) + "\n", encoding="utf-8")

    try:
        subprocess.run(
            [
                ffmpeg,
                "-y",
                "-f",
                "concat",
                "-safe",
                "0",
                "-i",
                str(concat_file),
                "-vf",
                "scale=1280:720:force_original_aspect_ratio=decrease,"
                "pad=1280:720:(ow-iw)/2:(oh-ih)/2:color=0x0d1117,fps=24",
                "-c:v",
                "libx264",
                "-profile:v",
                "high",
                "-level",
                "4.1",
                "-pix_fmt",
                "yuv420p",
                "-crf",
                "20",
                "-preset",
                "medium",
                "-movflags",
                "+faststart",
                str(out_mp4),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
    finally:
        concat_file.unlink(missing_ok=True)


def _encode_raw_webp(mp4_path: Path, out_webp: Path, ffmpeg: str, img2webp: str) -> None:
    with tempfile.TemporaryDirectory(prefix="mimikyu-raw-webp-") as tmp:
        tmp_dir = Path(tmp)
        subprocess.run(
            [
                ffmpeg,
                "-y",
                "-i",
                str(mp4_path),
                "-vf",
                "fps=8,scale=960:540",
                str(tmp_dir / "f_%04d.png"),
            ],
            check=True,
            capture_output=True,
            text=True,
        )
        pngs = sorted(tmp_dir.glob("f_*.png"))
        if not pngs:
            return
        cmd = [img2webp, "-loop", "0", "-d", "125", "-lossy", "-q", "76", "-m", "4"]
        cmd.extend(str(p) for p in pngs)
        cmd.extend(["-o", str(out_webp)])
        subprocess.run(cmd, check=True, capture_output=True, text=True)


def run_capture(binary: Path, out_dir: Path, copy_to: Path | None, headed: bool) -> dict[str, Any]:
    ffmpeg = resolve_tool("ffmpeg", "/opt/homebrew/bin/ffmpeg")
    if not ffmpeg:
        raise FileNotFoundError("ffmpeg is required to encode demo-raw-live-browser.mp4")
    img2webp = resolve_tool("img2webp", "/opt/homebrew/bin/img2webp")

    out_dir.mkdir(parents=True, exist_ok=True)
    telemetry: dict[str, Any] = {"mode": "100% raw CDP Page.startScreencast", "use_cases": []}

    with tempfile.TemporaryDirectory(prefix="mimikyu-raw-live-") as tmp_root:
        tmp = Path(tmp_root)
        state_dir = tmp / "state"
        frames_dir = tmp / "frames"
        frames_dir.mkdir(parents=True, exist_ok=True)

        runner = MimikyuRunner(binary, state_dir)
        runner.start_daemon()
        session = "raw-live"
        try:
            new_args = [
                "session",
                "new",
                "--session",
                session,
                "--engine",
                "chromium",
                "--preset",
                "macos",
                "--viewport",
                "1280x720",
                "--block-trackers",
            ]
            if headed:
                new_args.append("--headed")
            _, launch_ms = runner.run_json(*new_args)
            telemetry["session_launch_ms"] = round(launch_ms, 1)

            runner.run_json(
                "--session",
                session,
                "record",
                "start",
                "--out-dir",
                str(frames_dir),
                "--format",
                "png",
                "--max-width",
                "1280",
                "--max-height",
                "720",
                "--every-nth-frame",
                "1",
            )

            # Use Case 1: Live GitHub repository & README inspection
            open_gh, gh_open_ms = runner.run_json(
                "--session", session, "open", "https://github.com/YXZYSME/mimikyu"
            )
            runner.run_json("--session", session, "wait", "--idle", "--timeout-ms", "12000")
            snap_gh, gh_snap_ms = runner.run_json(
                "--session", session, "snapshot", "--interactive-only"
            )
            md_gh, gh_md_ms = runner.run_json("--session", session, "extract", "markdown")
            runner.run_json(
                "--session", session, "screenshot", str(out_dir / "demo-raw-poster-github.png")
            )
            for _ in range(5):
                runner.run_json("--session", session, "scroll", "0", "320", "--humanize")
                time.sleep(0.18)
            telemetry["use_cases"].append(
                {
                    "name": "GitHub Repo & README Inspection",
                    "url": open_gh.get("url", "https://github.com/YXZYSME/mimikyu"),
                    "open_ms": round(gh_open_ms, 1),
                    "snapshot_ms": round(gh_snap_ms, 1),
                    "extract_markdown_ms": round(gh_md_ms, 1),
                    "markdown_chars": len(str(md_gh.get("markdown") or md_gh.get("content") or "")),
                    "refs_count": len(snap_gh.get("refs", []))
                    if isinstance(snap_gh.get("refs"), list)
                    else None,
                }
            )

            # Use Case 2: Live Rust (crates.io) & Python (PyPI) registry verification
            open_crates, crates_open_ms = runner.run_json(
                "--session", session, "open", "https://crates.io/crates/mimikyu"
            )
            runner.run_json("--session", session, "wait", "--idle", "--timeout-ms", "12000")
            time.sleep(0.5)
            snap_crates, crates_snap_ms = runner.run_json(
                "--session", session, "snapshot", "--interactive-only"
            )
            runner.run_json(
                "--session", session, "screenshot", str(out_dir / "demo-raw-poster-crates.png")
            )
            for _ in range(3):
                runner.run_json("--session", session, "scroll", "0", "280", "--humanize")
                time.sleep(0.18)

            open_pypi, pypi_open_ms = runner.run_json(
                "--session", session, "open", "https://pypi.org/project/mimikyu/"
            )
            runner.run_json("--session", session, "wait", "--idle", "--timeout-ms", "12000")
            md_pypi, pypi_md_ms = runner.run_json("--session", session, "extract", "markdown")
            for _ in range(3):
                runner.run_json("--session", session, "scroll", "0", "280", "--humanize")
                time.sleep(0.18)

            telemetry["use_cases"].append(
                {
                    "name": "Live Multi-Registry Verification (crates.io + PyPI)",
                    "urls": [
                        open_crates.get("url", "https://crates.io/crates/mimikyu"),
                        open_pypi.get("url", "https://pypi.org/project/mimikyu/"),
                    ],
                    "crates_open_ms": round(crates_open_ms, 1),
                    "crates_snapshot_ms": round(crates_snap_ms, 1),
                    "pypi_open_ms": round(pypi_open_ms, 1),
                    "pypi_extract_ms": round(pypi_md_ms, 1),
                    "pypi_markdown_chars": len(
                        str(md_pypi.get("markdown") or md_pypi.get("content") or "")
                    ),
                    "crates_refs_count": len(snap_crates.get("refs", []))
                    if isinstance(snap_crates.get("refs"), list)
                    else None,
                }
            )

            # Use Case 3: Live Hacker News @eN discovery, real link click & Markdown extraction
            open_hn, hn_open_ms = runner.run_json(
                "--session", session, "open", "https://news.ycombinator.com"
            )
            runner.run_json("--session", session, "wait", "--idle", "--timeout-ms", "12000")
            snap_hn, hn_snap_ms = runner.run_json(
                "--session", session, "snapshot", "--interactive-only"
            )
            runner.run_json(
                "--session", session, "screenshot", str(out_dir / "demo-raw-poster-hn.png")
            )
            for _ in range(2):
                runner.run_json("--session", session, "scroll", "0", "240", "--humanize")
                time.sleep(0.15)
            runner.run_json("--session", session, "scroll", "0", "-480")
            time.sleep(0.15)

            clicked_ref = _find_ref_by_text(snap_hn, ("show", "ask", "new"))
            click_ms = None
            if clicked_ref:
                _, click_ms = runner.run_json(
                    "--session", session, "click", clicked_ref, "--humanize"
                )
                runner.run_json("--session", session, "wait", "--idle", "--timeout-ms", "10000")
                for _ in range(3):
                    runner.run_json("--session", session, "scroll", "0", "260", "--humanize")
                    time.sleep(0.15)

            md_hn, hn_md_ms = runner.run_json("--session", session, "extract", "markdown")
            telemetry["use_cases"].append(
                {
                    "name": "Live Hacker News @eN Click & Markdown Extraction",
                    "url": open_hn.get("url", "https://news.ycombinator.com"),
                    "open_ms": round(hn_open_ms, 1),
                    "snapshot_ms": round(hn_snap_ms, 1),
                    "clicked_ref": clicked_ref,
                    "click_ms": round(click_ms, 1) if click_ms is not None else None,
                    "extract_markdown_ms": round(hn_md_ms, 1),
                    "markdown_chars": len(str(md_hn.get("markdown") or md_hn.get("content") or "")),
                }
            )

            stop_res, _ = runner.run_json("--session", session, "record", "stop")
            telemetry["screencast_stop"] = stop_res
            runner.run_json("close", session)
        finally:
            runner.stop_daemon()

        frames = sorted(frames_dir.glob("*.png"))
        telemetry["captured_frames"] = len(frames)

        out_mp4 = out_dir / "demo-raw-live-browser.mp4"
        _encode_raw_mp4(frames, out_mp4, ffmpeg)
        telemetry["mp4_bytes"] = out_mp4.stat().st_size

        out_webp = out_dir / "demo-raw-live-browser.webp"
        if img2webp:
            _encode_raw_webp(out_mp4, out_webp, ffmpeg, img2webp)
            if out_webp.exists():
                telemetry["webp_bytes"] = out_webp.stat().st_size

        (out_dir / "demo-raw-live-telemetry.json").write_text(
            json.dumps(telemetry, indent=2) + "\n", encoding="utf-8"
        )

        if copy_to:
            copy_to.mkdir(parents=True, exist_ok=True)
            for fname in (
                "demo-raw-live-browser.mp4",
                "demo-raw-live-browser.webp",
                "demo-raw-poster-github.png",
                "demo-raw-poster-crates.png",
                "demo-raw-poster-hn.png",
                "demo-raw-live-telemetry.json",
            ):
                src = out_dir / fname
                if src.exists():
                    shutil.copy2(src, copy_to / fname)

    return telemetry


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Capture 100% raw unstylized Chromium screencast of mimikyu on live public sites."
    )
    parser.add_argument("--bin", dest="binary", default=None, help="Path to mimikyu binary")
    parser.add_argument("--out-dir", type=Path, default=DEFAULT_OUT_DIR)
    parser.add_argument("--copy-to", type=Path, default=None)
    parser.add_argument("--headed", action="store_true", help="Launch visible browser window")
    args = parser.parse_args()

    binary = resolve_mimikyu_bin(args.binary)
    telemetry = run_capture(binary, args.out_dir, args.copy_to, headed=args.headed)
    print(json.dumps(telemetry, indent=2))


if __name__ == "__main__":
    main()
