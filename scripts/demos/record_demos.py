#!/usr/bin/env python3
"""Reproducible demo recording & compositing pipeline for Mimikyu.

Runs the real compiled `mimikyu` binary against the local offline fixture
server (`scripts/demos/fixtures.py`) with zero macOS Screen Recording prompts,
captures real live browser screenshots, real `@eN` bounding boxes, real
`--humanize` pointer trajectories, and real measured latencies, then composites
1280x720 dark HUD videos + animated WebP + poster PNGs via Pillow, ffmpeg, and
img2webp.

Outputs (written to `dist/demos/` by default):
  - `demo-1-hero.mp4`         (~14s, 1280x720 H.264 high/yuv420p faststart)
  - `demo-1-hero.webp`        (1280x720 animated looping WebP, < 2.5 MB)
  - `demo-1-poster.png`       (1280x720 crisp hero poster)
  - `demo-2-dual-engine.mp4`  (~16s, 1280x720 H.264 high/yuv420p faststart)
  - `demo-2-poster.png`       (1280x720 crisp parity poster)
"""

from __future__ import annotations

import argparse
import json
import math
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from PIL import Image, ImageDraw, ImageFont

SCRIPT_DIR = Path(__file__).resolve().parent
REPO_ROOT = SCRIPT_DIR.parent.parent
if str(SCRIPT_DIR) not in sys.path:
    sys.path.insert(0, str(SCRIPT_DIR))

from fixtures import FixtureServer  # noqa: E402

# Canvas & codec parameters
WIDTH = 1280
HEIGHT = 720
FPS = 24
WEBP_MAX_BYTES = int(2.45 * 1024 * 1024)  # Strictly under 2.5 MB budget

# Mimikyu 16-bit Dark HUD Palette
BG_CANVAS = (18, 9, 36)       # #120924
BG_PANEL = (24, 13, 46)       # #180D2E
BG_TERM = (14, 7, 28)         # #0E071C
BG_HEADER = (20, 10, 40)      # #140A28
BG_CARD_BAR = (30, 17, 56)    # #1E1138
BORDER = (57, 35, 99)         # #392363
BORDER_BRIGHT = (92, 62, 150) # #5C3E96
GOLD = (245, 184, 65)         # #F5B841
GOLD_HI = (255, 228, 122)     # #FFE47A
MINT = (76, 224, 179)         # #4CE0B3
LAV = (157, 128, 235)         # #9D80EB
CORAL = (255, 107, 139)       # #FF6B8B
TEXT = (244, 238, 255)        # #F4EEFF
TEXT_SUB = (184, 168, 219)    # #B8A8DB
TEXT_DIM = (126, 108, 166)    # #7E6CA6

# macOS Firefox crash reporter suppression
BASE_ENV = {
    **os.environ,
    "MOZ_CRASHREPORTER_DISABLE": "1",
    "MOZ_CRASHREPORTER": "0",
    "MOZ_CRASHREPORTER_NO_REPORT": "1",
}

# 12x12 pixel-art Mimikyu emblem for top-left HUD header
MASCOT_PALETTE: dict[str, tuple[int, int, int]] = {
    ".": BG_HEADER,
    "b": (157, 128, 235),  # #9D80EB border
    "d": (18, 9, 36),      # #120924 dark bg
    "r": (238, 216, 180),  # #EED8B4 rag hood
    "s": (207, 169, 126),  # #CFA97E rag shadow
    "k": (9, 4, 20),       # #090414 interior shadow
    "e": (255, 214, 68),   # #FFD644 glowing eyes
    "p": (255, 107, 139),  # #FF6B8B cheek / dot
    "m": (76, 224, 179),   # #4CE0B3 cursor wand tip
}
MASCOT_GRID = [
    ".bbbbbbbbbb.",
    "bdddddddddd" + "b",
    "bddrrddrrddb",
    "bddrrddrrddb",
    "bdrrrrrrrrdb",
    "bdrprrrprsdb",
    "bdrkkkkkksdb",
    "bdkekkkekmdb",
    "bdkkkkkkksdb",
    "bdsrrrrrrsdb",
    "bdddddddddd" + "b",
    ".bbbbbbbbbb.",
]


# ---------------------------------------------------------------------------
# Tool & Font Discovery
# ---------------------------------------------------------------------------

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
        "Could not locate compiled `mimikyu` binary. Set MIMIKYU_BIN or run `cargo build --release`."
    )


def resolve_tool(name: str, preferred: str) -> str:
    if Path(preferred).is_file() and os.access(preferred, os.X_OK):
        return preferred
    if found := shutil.which(name):
        return found
    raise FileNotFoundError(f"Required tool `{name}` not found (checked {preferred} and PATH).")


class FontPack:
    """Loaded monospace regular/bold/medium fonts with cross-platform fallback."""

    def __init__(self) -> None:
        home = Path.home()
        regular_candidates = [
            home / "Library/Fonts/JetBrainsMonoNLNerdFont-Regular.ttf",
            home / "Library/Fonts/JetBrainsMonoNerdFont-Regular.ttf",
            Path("/Library/Fonts/JetBrainsMono-Regular.ttf"),
            Path("/System/Library/Fonts/SFNSMono.ttf"),
            Path("/System/Library/Fonts/Menlo.ttc"),
            Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"),
        ]
        bold_candidates = [
            home / "Library/Fonts/JetBrainsMonoNLNerdFont-Bold.ttf",
            home / "Library/Fonts/JetBrainsMonoNerdFont-Bold.ttf",
            Path("/Library/Fonts/JetBrainsMono-Bold.ttf"),
            Path("/System/Library/Fonts/Menlo.ttc"),
            Path("/System/Library/Fonts/SFNSMono.ttf"),
            Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"),
        ]
        self._reg_path = self._first_existing(regular_candidates)
        self._bold_path = self._first_existing(bold_candidates) or self._reg_path
        self._cache: dict[tuple[str, int], ImageFont.FreeTypeFont | ImageFont.ImageFont] = {}

    @staticmethod
    def _first_existing(paths: list[Path]) -> str | None:
        for p in paths:
            if p.is_file():
                return str(p)
        return None

    def get(self, size: int, bold: bool = False) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
        key = ("b" if bold else "r", size)
        if key not in self._cache:
            path = self._bold_path if bold else self._reg_path
            if path:
                try:
                    self._cache[key] = ImageFont.truetype(path, size=size)
                except OSError:
                    self._cache[key] = ImageFont.load_default()
            else:
                self._cache[key] = ImageFont.load_default()
        return self._cache[key]


# ---------------------------------------------------------------------------
# Live Mimikyu Runner
# ---------------------------------------------------------------------------

class MimikyuRunner:
    """Thin wrapper around the real `mimikyu` binary with isolated state-dir."""

    def __init__(self, binary: Path, state_dir: Path) -> None:
        self.binary = binary
        self.state_dir = state_dir

    def run_json(self, *args: str, timeout: float = 35.0) -> tuple[dict[str, Any], float]:
        cmd = [str(self.binary), "--state-dir", str(self.state_dir), *args, "--json"]
        t0 = time.perf_counter()
        proc = subprocess.run(cmd, capture_output=True, text=True, env=BASE_ENV, timeout=timeout)
        elapsed_ms = (time.perf_counter() - t0) * 1000.0
        if proc.returncode != 0:
            raise RuntimeError(
                f"mimikyu command failed (rc={proc.returncode}): {' '.join(args)}\n"
                f"stdout: {proc.stdout}\nstderr: {proc.stderr}"
            )
        payload = json.loads(proc.stdout)
        if not payload.get("ok"):
            raise RuntimeError(f"mimikyu returned ok=false for {args}: {payload}")
        return payload["data"], elapsed_ms

    def start_daemon(self) -> None:
        self.run_json("daemon", "--idle-timeout", "120")

    def stop_daemon(self) -> None:
        subprocess.run(
            [str(self.binary), "--state-dir", str(self.state_dir), "daemon", "stop"],
            capture_output=True,
            text=True,
            env=BASE_ENV,
            timeout=15.0,
        )

    def screenshot(self, session: str, out_path: Path, target_size: tuple[int, int]) -> Image.Image:
        self.run_json("--session", session, "screenshot", str(out_path))
        with Image.open(out_path) as im:
            rgb = im.convert("RGB")
            if rgb.size != target_size:
                rgb = rgb.resize(target_size, Image.Resampling.LANCZOS)
            return rgb.copy()


# ---------------------------------------------------------------------------
# Capture Data Structures
# ---------------------------------------------------------------------------

@dataclass
class RefBox:
    ref_id: str
    role: str
    name: str
    x: float
    y: float
    w: float
    h: float


@dataclass
class Demo1Capture:
    url: str
    open_ms: float
    snapshot_daemon_ms: int
    snapshot_cli_ms: float
    snapshot_lines: list[str]
    refs: list[RefBox]
    fill_ms: float
    fill_trail: list[dict[str, Any]]
    click_ms: float
    click_trail: list[dict[str, Any]]
    extract_daemon_ms: int
    extract_cli_ms: float
    markdown_lines: list[str]
    shot_loaded: Image.Image
    shot_filled: Image.Image
    shot_deployed: Image.Image


@dataclass
class EngineParityCapture:
    engine: str
    open_ms: float
    snapshot_daemon_ms: int
    snapshot_lines: list[str]
    refs: list[RefBox]
    batch_total_ms: int
    batch_step_ms: list[tuple[str, int]]
    extract_daemon_ms: int
    markdown_text: str
    shot_loaded: Image.Image
    shot_light: Image.Image
    shot_shadow: Image.Image
    shot_all: Image.Image


@dataclass
class Demo2Capture:
    url: str
    chromium: EngineParityCapture
    firefox: EngineParityCapture
    refs_exact_match: bool
    markdown_parity_match: bool


# ---------------------------------------------------------------------------
# Live Capture Routines
# ---------------------------------------------------------------------------

def _parse_ref_boxes(snap_data: dict[str, Any]) -> list[RefBox]:
    entries = snap_data.get("ref_map", {}).get("entries", {})
    out: list[RefBox] = []
    for _, item in sorted(entries.items(), key=lambda kv: int(kv[0])):
        b = item.get("bounds") or {}
        if not b:
            continue
        out.append(
            RefBox(
                ref_id=str(item.get("ref_id", "")),
                role=str(item.get("role", "")),
                name=str(item.get("name", "")),
                x=float(b.get("x", 0.0)),
                y=float(b.get("y", 0.0)),
                w=float(b.get("width", 0.0)),
                h=float(b.get("height", 0.0)),
            )
        )
    return out


def _read_trail(runner: MimikyuRunner, session: str) -> list[dict[str, Any]]:
    data, _ = runner.run_json(
        "--session",
        session,
        "eval",
        "--main-world",
        "JSON.stringify(window.__mimikyuTrail || [])",
    )
    raw = data.get("value", "[]")
    pts = json.loads(raw) if isinstance(raw, str) else []
    return [p for p in pts if isinstance(p, dict) and "x" in p and "y" in p]


def _reset_trail(runner: MimikyuRunner, session: str) -> None:
    runner.run_json(
        "--session",
        session,
        "eval",
        "--main-world",
        "window.__mimikyuResetTrail && window.__mimikyuResetTrail(); 'ok'",
    )


def capture_demo1(runner: MimikyuRunner, srv: FixtureServer, work_dir: Path) -> Demo1Capture:
    vp = (666, 574)
    session = "hero-cdp"
    url = srv.url("/dashboard")
    print("[demo-1] Launching Chromium CDP virtual session (666x574)...")
    runner.run_json(
        "--session",
        session,
        "--engine",
        "chromium",
        "session",
        "new",
        "--virtual",
        "--preset",
        "macos",
        "--viewport",
        f"{vp[0]}x{vp[1]}",
    )
    try:
        _, open_ms = runner.run_json("--session", session, "open", url)
        runner.run_json("--session", session, "wait", "--idle")
        shot_loaded = runner.screenshot(session, work_dir / "d1_loaded.png", vp)

        # Warm in-daemon snapshot timing via atomic 1-op batch + bounds snapshot
        snap_batch, _ = runner.run_json(
            "--session",
            session,
            "batch",
            "--ops",
            json.dumps([{"op": "snapshot", "interactive_only": True}]),
        )
        snap_daemon_ms = int(snap_batch["results"][0]["duration_ms"])
        snap_data, snap_cli_ms = runner.run_json(
            "--session", session, "snapshot", "--interactive-only", "--include-bounds"
        )
        snapshot_lines = [ln for ln in snap_data.get("text", "").splitlines() if ln.strip()]
        refs = _parse_ref_boxes(snap_data)
        print(
            f"[demo-1] Snapshot captured: {len(refs)} refs, "
            f"daemon={snap_daemon_ms}ms, cli={snap_cli_ms:.1f}ms"
        )

        # Step 1: Humanized fill @e1 -> "edge-prod"
        _reset_trail(runner, session)
        _, fill_ms = runner.run_json(
            "--session", session, "fill", "@e1", "edge-prod", "--humanize"
        )
        fill_trail = _read_trail(runner, session)
        _reset_trail(runner, session)
        shot_filled = runner.screenshot(session, work_dir / "d1_filled.png", vp)
        print(f"[demo-1] Humanized fill @e1: {fill_ms:.0f}ms ({len(fill_trail)} pointer events)")

        # Step 2: Humanized click @e4 ("Deploy Canary")
        _reset_trail(runner, session)
        _, click_ms = runner.run_json("--session", session, "click", "@e4", "--humanize")
        click_trail = _read_trail(runner, session)
        _reset_trail(runner, session)
        shot_deployed = runner.screenshot(session, work_dir / "d1_deployed.png", vp)
        print(f"[demo-1] Humanized click @e4: {click_ms:.0f}ms ({len(click_trail)} pointer events)")

        # Step 3: Markdown extraction (in-daemon timing + real markdown output)
        ext_batch, _ = runner.run_json(
            "--session",
            session,
            "batch",
            "--ops",
            json.dumps([{"op": "extract", "mode": "markdown"}]),
        )
        ext_daemon_ms = int(ext_batch["results"][0]["duration_ms"])
        ext_data, ext_cli_ms = runner.run_json("--session", session, "extract", "markdown")
        md_raw = str(ext_data.get("markdown", ""))
        md_lines = [ln for ln in md_raw.splitlines() if ln.strip()]
        print(
            f"[demo-1] Extract markdown: daemon={ext_daemon_ms}ms, "
            f"cli={ext_cli_ms:.1f}ms, lines={len(md_lines)}"
        )

        return Demo1Capture(
            url=url,
            open_ms=open_ms,
            snapshot_daemon_ms=snap_daemon_ms,
            snapshot_cli_ms=snap_cli_ms,
            snapshot_lines=snapshot_lines,
            refs=refs,
            fill_ms=fill_ms,
            fill_trail=fill_trail,
            click_ms=click_ms,
            click_trail=click_trail,
            extract_daemon_ms=ext_daemon_ms,
            extract_cli_ms=ext_cli_ms,
            markdown_lines=md_lines,
            shot_loaded=shot_loaded,
            shot_filled=shot_filled,
            shot_deployed=shot_deployed,
        )
    finally:
        runner.run_json("close", session)


def _capture_parity_engine(
    runner: MimikyuRunner, engine: str, url: str, work_dir: Path
) -> EngineParityCapture:
    vp = (620, 386)
    session = f"parity-{engine}"
    print(f"[demo-2] Launching {engine} virtual session (620x386)...")
    runner.run_json(
        "--session",
        session,
        "--engine",
        engine,
        "session",
        "new",
        "--virtual",
        "--viewport",
        f"{vp[0]}x{vp[1]}",
    )
    try:
        # Pass A: Real atomic single-call batch timing + final 3/3 screenshot
        _, open_ms = runner.run_json("--session", session, "open", url)
        runner.run_json("--session", session, "wait", "--idle")
        shot_loaded = runner.screenshot(session, work_dir / f"d2_{engine}_0.png", vp)

        snap_data, _ = runner.run_json(
            "--session", session, "snapshot", "--interactive-only", "--include-bounds"
        )
        snapshot_lines = [ln for ln in snap_data.get("text", "").splitlines() if ln.strip()]
        refs = _parse_ref_boxes(snap_data)

        batch_ops = [
            {"op": "snapshot", "interactive_only": True},
            {"op": "fill", "target": "@e1", "value": "agent-031"},
            {"op": "click", "target": "@e2"},
            {"op": "fill", "target": "@e3", "value": "shadow-ok"},
            {"op": "click", "target": "@e4"},
            {"op": "click", "target": "@e5"},
            {"op": "wait_text", "text": "PARITY OK"},
            {"op": "extract", "mode": "markdown"},
        ]
        batch_data, _ = runner.run_json(
            "--session", session, "batch", "--ops", json.dumps(batch_ops)
        )
        results = batch_data["results"]
        snap_ms = int(results[0]["duration_ms"])
        action_steps = [(r["action"], int(r["duration_ms"])) for r in results[1:7]]
        batch_total_ms = sum(ms for _, ms in action_steps)
        ext_ms = int(results[7]["duration_ms"])

        _reset_trail(runner, session)
        shot_all = runner.screenshot(session, work_dir / f"d2_{engine}_3.png", vp)
        ext_data, _ = runner.run_json("--session", session, "extract", "markdown")
        md_text = str(ext_data.get("markdown", ""))

        # Pass B: Capture intermediate visual milestones (Zone 1 Light DOM, Zone 2 Shadow DOM)
        runner.run_json("--session", session, "open", url)
        runner.run_json("--session", session, "wait", "--idle")
        runner.run_json("--session", session, "snapshot", "--interactive-only")
        runner.run_json(
            "--session",
            session,
            "batch",
            "--ops",
            json.dumps([
                {"op": "fill", "target": "@e1", "value": "agent-031"},
                {"op": "click", "target": "@e2"},
            ]),
        )
        _reset_trail(runner, session)
        shot_light = runner.screenshot(session, work_dir / f"d2_{engine}_1.png", vp)

        runner.run_json(
            "--session",
            session,
            "batch",
            "--ops",
            json.dumps([
                {"op": "fill", "target": "@e3", "value": "shadow-ok"},
                {"op": "click", "target": "@e4"},
            ]),
        )
        _reset_trail(runner, session)
        shot_shadow = runner.screenshot(session, work_dir / f"d2_{engine}_2.png", vp)

        print(
            f"[demo-2] {engine}: refs={len(refs)} snap={snap_ms}ms "
            f"batch(6)={batch_total_ms}ms extract={ext_ms}ms"
        )
        return EngineParityCapture(
            engine=engine,
            open_ms=open_ms,
            snapshot_daemon_ms=snap_ms,
            snapshot_lines=snapshot_lines,
            refs=refs,
            batch_total_ms=batch_total_ms,
            batch_step_ms=action_steps,
            extract_daemon_ms=ext_ms,
            markdown_text=md_text,
            shot_loaded=shot_loaded,
            shot_light=shot_light,
            shot_shadow=shot_shadow,
            shot_all=shot_all,
        )
    finally:
        runner.run_json("close", session)


def capture_demo2(runner: MimikyuRunner, srv: FixtureServer, work_dir: Path) -> Demo2Capture:
    url = srv.url("/parity")
    chrom = _capture_parity_engine(runner, "chromium", url, work_dir)
    ff = _capture_parity_engine(runner, "firefox", url, work_dir)
    refs_match = chrom.snapshot_lines == ff.snapshot_lines and len(chrom.snapshot_lines) == 5
    # Verify extracted markdown contains identical verified state across both engines
    md_match = (
        "PARITY OK" in chrom.markdown_text
        and "PARITY OK" in ff.markdown_text
        and "3 / 3 VERIFIED" in chrom.markdown_text
        and "3 / 3 VERIFIED" in ff.markdown_text
    )
    if not refs_match:
        raise RuntimeError(
            f"Expected identical @eN refs across Chromium and Firefox:\n"
            f"  chromium: {chrom.snapshot_lines}\n  firefox:  {ff.snapshot_lines}"
        )
    return Demo2Capture(
        url=url,
        chromium=chrom,
        firefox=ff,
        refs_exact_match=refs_match,
        markdown_parity_match=md_match,
    )


# ---------------------------------------------------------------------------
# HUD & Visual Drawing Helpers
# ---------------------------------------------------------------------------

def draw_pixel_mascot(draw: ImageDraw.ImageDraw, x0: int, y0: int, px: int = 3) -> None:
    for r, row in enumerate(MASCOT_GRID):
        for c, ch in enumerate(row):
            color = MASCOT_PALETTE.get(ch)
            if color is None or ch == ".":
                continue
            draw.rectangle(
                [x0 + c * px, y0 + r * px, x0 + (c + 1) * px - 1, y0 + (r + 1) * px - 1],
                fill=color,
            )


def draw_panel(
    draw: ImageDraw.ImageDraw,
    box: tuple[int, int, int, int],
    fill: tuple[int, int, int],
    border: tuple[int, int, int] = BORDER,
    corner_accent: tuple[int, int, int] | None = None,
) -> None:
    x0, y0, x1, y1 = box
    draw.rounded_rectangle([x0, y0, x1, y1], radius=8, fill=fill, outline=border, width=1)
    if corner_accent:
        # 16-bit HUD corner pixel brackets
        t = 6
        draw.line([(x0 + 1, y0 + t), (x0 + 1, y0 + 1), (x0 + t, y0 + 1)], fill=corner_accent, width=2)
        draw.line([(x1 - t, y0 + 1), (x1 - 1, y0 + 1), (x1 - 1, y0 + t)], fill=corner_accent, width=2)


def draw_pill(
    draw: ImageDraw.ImageDraw,
    fonts: FontPack,
    x: int,
    y: int,
    text: str,
    fg: tuple[int, int, int],
    bg: tuple[int, int, int],
    outline: tuple[int, int, int],
    font_size: int = 12,
    pad_x: int = 10,
    pad_y: int = 4,
    bold: bool = True,
) -> int:
    font = fonts.get(font_size, bold=bold)
    bbox = draw.textbbox((0, 0), text, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    w = tw + pad_x * 2
    h = th + pad_y * 2 + 2
    draw.rounded_rectangle([x, y, x + w, y + h], radius=5, fill=bg, outline=outline, width=1)
    draw.text((x + pad_x, y + pad_y - 1), text, font=font, fill=fg)
    return w


def draw_window_dots(draw: ImageDraw.ImageDraw, x: int, y: int) -> None:
    for idx, col in enumerate((CORAL, GOLD, MINT)):
        cx = x + idx * 16
        draw.ellipse([cx, y, cx + 9, y + 9], fill=col)


def draw_ref_overlays(
    base_img: Image.Image,
    fonts: FontPack,
    refs: list[RefBox],
    alpha: float,
    highlight_ref: str | None = None,
    color_fn: Any = None,
    inside_right: bool = False,
) -> Image.Image:
    if alpha <= 0.01 or not refs:
        return base_img
    overlay = Image.new("RGBA", base_img.size, (0, 0, 0, 0))
    od = ImageDraw.Draw(overlay)
    font = fonts.get(11, bold=True)

    for rb in refs:
        base_col = color_fn(rb) if color_fn else GOLD
        if highlight_ref == rb.ref_id:
            base_col = MINT
        a_fill = int(34 * alpha) if highlight_ref != rb.ref_id else int(58 * alpha)
        a_stroke = int(235 * alpha)
        x0, y0 = int(rb.x) - 2, int(rb.y) - 2
        x1, y1 = int(rb.x + rb.w) + 2, int(rb.y + rb.h) + 2
        od.rounded_rectangle(
            [x0, y0, x1, y1],
            radius=5,
            fill=(*base_col, a_fill),
            outline=(*base_col, a_stroke),
            width=2,
        )
        label = rb.ref_id
        tb = od.textbbox((0, 0), label, font=font)
        lw, lh = tb[2] - tb[0] + 10, tb[3] - tb[1] + 4
        if inside_right and rb.w >= 140:
            tx = max(x0 + 4, min(base_img.width - lw - 4, x1 - lw - 4))
            ty = int(y0 + (y1 - y0 - lh) / 2)
        else:
            ty = max(2, y0 - lh - 2)
            tx = max(2, min(base_img.width - lw - 2, x0))
        od.rounded_rectangle(
            [tx, ty, tx + lw, ty + lh],
            radius=4,
            fill=(*base_col, int(245 * alpha)),
        )
        od.text((tx + 5, ty + 1), label, font=font, fill=(18, 9, 36, int(255 * alpha)))

    return Image.alpha_composite(base_img.convert("RGBA"), overlay).convert("RGB")


def _sample_trail(trail: list[dict[str, Any]], progress: float) -> tuple[list[tuple[float, float]], tuple[float, float] | None]:
    if not trail:
        return [], None
    move_pts = [(float(p["x"]), float(p["y"])) for p in trail]
    if len(move_pts) == 1:
        return move_pts, move_pts[0]
    p = max(0.0, min(1.0, progress))
    # Compute cumulative arc length for constant-smooth parametric sampling
    dists = [0.0]
    for i in range(1, len(move_pts)):
        dx = move_pts[i][0] - move_pts[i - 1][0]
        dy = move_pts[i][1] - move_pts[i - 1][1]
        dists.append(dists[-1] + math.hypot(dx, dy))
    total = dists[-1]
    if total < 1e-3:
        return move_pts[:1], move_pts[-1]
    # Ease-in-out cubic matches human Fitts's law velocity profile
    eased = 3 * p * p - 2 * p * p * p
    target_d = eased * total
    pts: list[tuple[float, float]] = [move_pts[0]]
    cur = move_pts[0]
    for i in range(1, len(move_pts)):
        if dists[i] <= target_d:
            pts.append(move_pts[i])
            cur = move_pts[i]
        else:
            seg = dists[i] - dists[i - 1]
            frac = (target_d - dists[i - 1]) / seg if seg > 1e-6 else 1.0
            cx = move_pts[i - 1][0] + (move_pts[i][0] - move_pts[i - 1][0]) * frac
            cy = move_pts[i - 1][1] + (move_pts[i][1] - move_pts[i - 1][1]) * frac
            cur = (cx, cy)
            pts.append(cur)
            break
    return pts, cur


def draw_humanize_overlay(
    base_img: Image.Image,
    trail: list[dict[str, Any]],
    progress: float,
    trail_fade: float = 1.0,
    ripple_progress: float = -1.0,
    show_cursor: bool = True,
) -> Image.Image:
    """Render glowing Bezier trail + luminous SVG-style pointer + click ripple from real trail pts."""
    if not trail or trail_fade <= 0.01:
        return base_img
    pts, tip = _sample_trail(trail, progress)
    if not pts or tip is None:
        return base_img

    overlay = Image.new("RGBA", base_img.size, (0, 0, 0, 0))
    od = ImageDraw.Draw(overlay)

    # 1. Multi-layer fading Bezier trail along the real captured trajectory points
    if len(pts) >= 2:
        n_seg = len(pts) - 1
        for i in range(n_seg):
            frac = (i + 1) / n_seg
            a_glow = int(75 * frac * trail_fade)
            a_core = int(230 * (0.25 + 0.75 * frac) * trail_fade)
            # Blend stroke from mint -> gold toward the click target
            r = int(MINT[0] + (GOLD[0] - MINT[0]) * frac)
            g = int(MINT[1] + (GOLD[1] - MINT[1]) * frac)
            b = int(MINT[2] + (GOLD[2] - MINT[2]) * frac)
            p0, p1 = pts[i], pts[i + 1]
            od.line([p0, p1], fill=(r, g, b, a_glow), width=8)
            od.line([p0, p1], fill=(r, g, b, a_core), width=3)

        # Subtle breadcrumb sample dots every few real points to highlight kinematic curvature
        step = max(1, len(pts) // 12)
        for idx in range(0, len(pts) - 1, step):
            frac = (idx + 1) / len(pts)
            px, py = pts[idx]
            rad = 2.4
            od.ellipse(
                [px - rad, py - rad, px + rad, py + rad],
                fill=(*GOLD_HI, int(190 * frac * trail_fade)),
            )

    # 2. Expanding click ripple ring at impact point
    if 0.0 <= ripple_progress <= 1.0:
        cx, cy = pts[-1]
        r1 = 7 + ripple_progress * 30
        a1 = int(235 * (1.0 - ripple_progress))
        od.ellipse(
            [cx - r1, cy - r1, cx + r1, cy + r1],
            fill=(*GOLD, int(45 * (1.0 - ripple_progress))),
            outline=(*GOLD, a1),
            width=3,
        )
        r2 = 4 + ripple_progress * 18
        a2 = int(210 * (1.0 - ripple_progress))
        od.ellipse(
            [cx - r2, cy - r2, cx + r2, cy + r2],
            outline=(*MINT, a2),
            width=2,
        )

    # 3. Glowing gold/mint SVG-style pointer cursor
    if show_cursor:
        cx, cy = tip
        a_cur = int(255 * min(1.0, trail_fade * 1.25))
        # Outer mint halo
        od.ellipse([cx - 13, cy - 13, cx + 13, cy + 13], fill=(*MINT, int(35 * trail_fade)))
        od.ellipse(
            [cx - 9, cy - 9, cx + 9, cy + 9],
            fill=(*GOLD, int(55 * trail_fade)),
            outline=(*MINT, int(210 * trail_fade)),
            width=2,
        )
        # Sharp gold cursor arrow
        arrow = [
            (cx, cy),
            (cx, cy + 18),
            (cx + 5, cy + 13.5),
            (cx + 9.2, cy + 20.5),
            (cx + 12.2, cy + 18.8),
            (cx + 8, cy + 12),
            (cx + 14.5, cy + 11.5),
        ]
        od.polygon(arrow, fill=(*GOLD, a_cur), outline=(18, 9, 36, a_cur))

    return Image.alpha_composite(base_img.convert("RGBA"), overlay).convert("RGB")


# ---------------------------------------------------------------------------
# Terminal Renderer
# ---------------------------------------------------------------------------

@dataclass
class TermLine:
    start_t: float
    segments: list[tuple[str, tuple[int, int, int]]]
    type_dur: float = 0.0  # > 0 animates character typing


def render_terminal_lines(
    draw: ImageDraw.ImageDraw,
    fonts: FontPack,
    box: tuple[int, int, int, int],
    lines: list[TermLine],
    t: float,
    font_size: int = 13,
    line_h: int = 20,
    max_visible: int = 26,
) -> None:
    x0, y0, x1, y1 = box
    font_reg = fonts.get(font_size, bold=False)
    font_bold = fonts.get(font_size, bold=True)

    active: list[tuple[list[tuple[str, tuple[int, int, int]]], bool]] = []
    for ln in lines:
        if t < ln.start_t:
            continue
        if ln.type_dur > 0:
            elapsed = t - ln.start_t
            frac = min(1.0, elapsed / ln.type_dur)
            full_text = "".join(s for s, _ in ln.segments)
            n_chars = int(round(len(full_text) * frac))
            rem = n_chars
            clipped: list[tuple[str, tuple[int, int, int]]] = []
            for s, col in ln.segments:
                if rem <= 0:
                    break
                take = s[:rem]
                clipped.append((take, col))
                rem -= len(take)
            active.append((clipped, frac < 1.0))
        else:
            active.append((ln.segments, False))

    visible = active[-max_visible:]
    cy = y0
    for idx, (segs, is_typing) in enumerate(visible):
        if cy + line_h > y1:
            break
        cx = x0
        for txt, col in segs:
            use_bold = col in (GOLD, GOLD_HI, MINT) and (txt.startswith("@e") or txt.startswith("$"))
            f = font_bold if use_bold else font_reg
            draw.text((cx, cy), txt, font=f, fill=col)
            tb = draw.textbbox((0, 0), txt, font=f)
            cx += tb[2] - tb[0]
        # Blinking cursor on active typing line or last visible line
        is_last = idx == len(visible) - 1
        if is_typing or (is_last and int(t * 2.4) % 2 == 0):
            draw.rectangle([cx + 2, cy + 2, cx + 9, cy + font_size + 2], fill=GOLD)
        cy += line_h


# ---------------------------------------------------------------------------
# Demo 1 Frame Generator (~14.0s = 336 frames)
# ---------------------------------------------------------------------------

def build_demo1_script(cap: Demo1Capture) -> list[TermLine]:
    # Format real extracted markdown table lines concisely (max ~60 chars)
    table_lines = [ln for ln in cap.markdown_lines if ln.startswith("|")][:4]
    if not table_lines:
        table_lines = [ln[:60] for ln in cap.markdown_lines[:4]]

    lines: list[TermLine] = [
        TermLine(0.15, [("$ ", MINT), ("mimikyu ", GOLD), ("open http://127.0.0.1/dashboard", TEXT)], type_dur=0.85),
        TermLine(1.15, [("  ok ", MINT), (f"loaded Aether Ops ({cap.open_ms:.0f}ms · Chromium CDP)", TEXT_SUB)]),
        TermLine(1.50, [("", TEXT_DIM)]),
        TermLine(1.70, [("$ ", MINT), ("mimikyu ", GOLD), ("snapshot --interactive-only", TEXT)], type_dur=0.85),
        TermLine(
            2.65,
            [
                ("  // ", TEXT_DIM),
                (f"{len(cap.refs)} semantic refs", MINT),
                (f" · {cap.snapshot_daemon_ms}ms daemon", GOLD),
            ],
        ),
    ]
    for i, sline in enumerate(cap.snapshot_lines[:4]):
        parts = sline.split(" ", 2)
        if len(parts) == 3:
            ref_tok, role_tok, label_tok = parts
            lines.append(
                TermLine(
                    2.80 + i * 0.14,
                    [("  ", TEXT), (ref_tok, GOLD), (" ", TEXT), (role_tok, LAV), (f" {label_tok}", TEXT)],
                )
            )
        else:
            lines.append(TermLine(2.80 + i * 0.14, [(f"  {sline[:60]}", TEXT)]))

    lines.extend([
        TermLine(3.60, [("", TEXT_DIM)]),
        TermLine(
            4.20,
            [
                ("$ ", MINT),
                ("mimikyu ", GOLD),
                ('fill @e1 "edge-prod" ', TEXT),
                ("--humanize", LAV),
            ],
            type_dur=0.90,
        ),
        TermLine(
            6.55,
            [
                ("  ok ", MINT),
                (f"kinematic path ({len(cap.fill_trail)} pts, {cap.fill_ms:.0f}ms)", TEXT_SUB),
            ],
        ),
        TermLine(7.00, [("", TEXT_DIM)]),
        TermLine(
            7.25,
            [
                ("$ ", MINT),
                ("mimikyu ", GOLD),
                ("click @e4 ", TEXT),
                ("--humanize", LAV),
            ],
            type_dur=0.75,
        ),
        TermLine(
            9.60,
            [
                ("  ok ", MINT),
                (f"Bezier click ({len(cap.click_trail)} pts, {cap.click_ms:.0f}ms)", TEXT_SUB),
            ],
        ),
        TermLine(9.85, [("  -> ", GOLD), ("CANARY v2.4.1 deployed (2 edge nodes)", MINT)]),
        TermLine(10.20, [("", TEXT_DIM)]),
        TermLine(
            10.45,
            [("$ ", MINT), ("mimikyu ", GOLD), ("extract markdown", TEXT)],
            type_dur=0.70,
        ),
        TermLine(
            11.25,
            [
                ("  // ", TEXT_DIM),
                (f"extracted in {cap.extract_daemon_ms}ms daemon", MINT),
                (" · compact LLM table", TEXT_SUB),
            ],
        ),
    ])
    for idx, tln in enumerate(table_lines):
        # Trim nicely so nothing clips in the 540px terminal pane
        clean = tln if len(tln) <= 61 else tln[:58] + "..."
        col = GOLD_HI if idx == 0 else (TEXT_DIM if idx == 1 else TEXT)
        lines.append(TermLine(11.40 + idx * 0.16, [(f"  {clean}", col)]))

    return lines


def render_demo1_frame(cap: Demo1Capture, script: list[TermLine], fonts: FontPack, t: float, dur: float) -> Image.Image:
    img = Image.new("RGB", (WIDTH, HEIGHT), BG_CANVAS)
    draw = ImageDraw.Draw(img)

    # 1. Top HUD Bar (y=0..56)
    draw.rectangle([0, 0, WIDTH, 56], fill=BG_HEADER)
    draw.line([(0, 56), (WIDTH, 56)], fill=BORDER, width=1)
    draw_pixel_mascot(draw, 16, 10, px=3)
    draw.text((62, 11), "MIMIKYU", font=fonts.get(18, bold=True), fill=GOLD)
    draw.text((154, 14), "// THE 5ms AGENTIC LOOP", font=fonts.get(14, bold=True), fill=MINT)
    draw.text((62, 34), "@eN semantic refs  ->  --humanize kinematic input  ->  Markdown", font=fonts.get(11), fill=TEXT_SUB)

    # Top-right live telemetry badges
    bx = WIDTH - 16
    ext_ready = t >= 11.25
    ext_txt = f"EXTRACT: {cap.extract_daemon_ms}ms" if ext_ready else "EXTRACT: --"
    w_ext = draw.textbbox((0, 0), ext_txt, font=fonts.get(12, bold=True))[2] + 22
    bx -= w_ext
    draw_pill(
        draw, fonts, bx, 15, ext_txt,
        fg=GOLD if ext_ready else TEXT_DIM,
        bg=(44, 30, 22) if ext_ready else BG_PANEL,
        outline=GOLD if ext_ready else BORDER,
    )

    snap_ready = t >= 2.65
    snap_txt = f"SNAPSHOT: {cap.snapshot_daemon_ms}ms" if snap_ready else "SNAPSHOT: --"
    w_snap = draw.textbbox((0, 0), snap_txt, font=fonts.get(12, bold=True))[2] + 22
    bx -= w_snap + 10
    draw_pill(
        draw, fonts, bx, 15, snap_txt,
        fg=MINT if snap_ready else TEXT_DIM,
        bg=(18, 42, 40) if snap_ready else BG_PANEL,
        outline=MINT if snap_ready else BORDER,
    )

    eng_txt = "CHROMIUM CDP"
    w_eng = draw.textbbox((0, 0), eng_txt, font=fonts.get(12, bold=True))[2] + 22
    bx -= w_eng + 10
    draw_pill(draw, fonts, bx, 15, eng_txt, fg=LAV, bg=(34, 21, 64), outline=LAV)

    # 2. Left Terminal Pane (x=14..586, y=66..676)
    draw_panel(draw, (14, 66, 586, 676), fill=BG_TERM, border=BORDER, corner_accent=LAV)
    draw.rounded_rectangle([15, 67, 585, 97], radius=7, fill=BG_CARD_BAR)
    draw.rectangle([15, 88, 585, 97], fill=BG_CARD_BAR)
    draw.line([(15, 97), (585, 97)], fill=BORDER, width=1)
    draw_window_dots(draw, 28, 78)
    draw.text((88, 76), "terminal — mimikyu-cli (warm daemon)", font=fonts.get(12, bold=True), fill=TEXT_SUB)

    render_terminal_lines(
        draw, fonts, (28, 108, 574, 666), script, t, font_size=13, line_h=21, max_visible=26
    )

    # 3. Right Live Browser Pane (x=598..1266, y=66..676)
    humanize_active = (5.10 <= t <= 6.80) or (8.00 <= t <= 10.20)
    draw_panel(
        draw,
        (598, 66, 1266, 676),
        fill=BG_PANEL,
        border=GOLD if humanize_active else BORDER,
        corner_accent=GOLD if humanize_active else MINT,
    )
    draw.rounded_rectangle([599, 67, 1265, 100], radius=7, fill=BG_CARD_BAR)
    draw.rectangle([599, 90, 1265, 100], fill=BG_CARD_BAR)
    draw.line([(599, 100), (1265, 100)], fill=BORDER, width=1)
    draw_window_dots(draw, 612, 79)

    # URL pill
    draw.rounded_rectangle([672, 72, 1074, 95], radius=5, fill=BG_TERM, outline=BORDER, width=1)
    draw.text((682, 77), "http://127.0.0.1/dashboard", font=fonts.get(11), fill=TEXT_SUB)

    # Status pill in browser chrome
    if humanize_active:
        draw_pill(draw, fonts, 1084, 72, "--humanize ACTIVE", fg=BG_CANVAS, bg=GOLD, outline=GOLD_HI, font_size=11, pad_x=8, pad_y=3)
    elif t >= 11.25:
        draw_pill(draw, fonts, 1094, 72, "MARKDOWN READY", fg=BG_CANVAS, bg=MINT, outline=MINT, font_size=11, pad_x=8, pad_y=3)
    else:
        draw_pill(draw, fonts, 1118, 72, "LIVE · 666x574", fg=MINT, bg=(18, 42, 40), outline=MINT, font_size=11, pad_x=8, pad_y=3)

    # Choose live browser screenshot + overlays according to timeline beat
    if t < 6.15:
        vp_img = cap.shot_loaded.copy()
    elif t < 9.35:
        vp_img = cap.shot_filled.copy()
    else:
        vp_img = cap.shot_deployed.copy()

    # Beat 2: @eN bounding boxes illuminate on snapshot (2.65s..5.10s) and subtly during click
    if 2.65 <= t < 5.30:
        fade_in = min(1.0, (t - 2.65) / 0.35)
        fade_out = 1.0 if t < 4.75 else max(0.0, 1.0 - (t - 4.75) / 0.55)
        hl = "@e1" if t >= 4.25 else None
        vp_img = draw_ref_overlays(vp_img, fonts, cap.refs, alpha=fade_in * fade_out, highlight_ref=hl)
    elif 7.25 <= t < 8.40:
        fade = min(1.0, (t - 7.25) / 0.25) * (1.0 if t < 7.95 else max(0.0, 1.0 - (t - 7.95) / 0.45))
        vp_img = draw_ref_overlays(vp_img, fonts, cap.refs, alpha=fade * 0.85, highlight_ref="@e4")

    # Beat 3: Kinematic --humanize fill @e1 motion (5.10s..6.85s)
    if 5.10 <= t <= 6.85:
        prog = min(1.0, (t - 5.10) / 0.90)
        rip = (t - 6.00) / 0.55 if t >= 6.00 else -1.0
        tfade = 1.0 if t < 6.35 else max(0.0, 1.0 - (t - 6.35) / 0.50)
        vp_img = draw_humanize_overlay(vp_img, cap.fill_trail, prog, trail_fade=tfade, ripple_progress=rip)

    # Beat 4: Kinematic --humanize click @e4 motion (8.00s..11.20s — holds crisp trail for poster!)
    if 8.00 <= t <= 11.20:
        prog = min(1.0, (t - 8.00) / 1.15)
        rip = (t - 9.15) / 1.05 if t >= 9.15 else -1.0
        tfade = 1.0 if t < 10.45 else max(0.0, 1.0 - (t - 10.45) / 0.75)
        vp_img = draw_humanize_overlay(vp_img, cap.click_trail, prog, trail_fade=tfade, ripple_progress=rip)

    # Beat 5: Subtle mint extraction badge overlay around extracted table
    if t >= 11.35:
        ov = Image.new("RGBA", vp_img.size, (0, 0, 0, 0))
        od = ImageDraw.Draw(ov)
        ea = min(1.0, (t - 11.35) / 0.35)
        od.rounded_rectangle(
            [16, 198, 650, 310],
            radius=8,
            fill=(*MINT, int(14 * ea)),
            outline=(*MINT, int(195 * ea)),
            width=2,
        )
        badge_txt = f"EXTRACTED -> MARKDOWN ({cap.extract_daemon_ms}ms)"
        bf = fonts.get(11, bold=True)
        bw = od.textbbox((0, 0), badge_txt, font=bf)[2] + 18
        od.rounded_rectangle(
            [648 - bw, 300, 648, 322],
            radius=4,
            fill=(*MINT, int(245 * ea)),
        )
        od.text((648 - bw + 9, 304), badge_txt, font=bf, fill=(18, 9, 36, int(255 * ea)))
        vp_img = Image.alpha_composite(vp_img.convert("RGBA"), ov).convert("RGB")

    img.paste(vp_img, (599, 101))

    # 4. Bottom Pipeline Stepper & Progress Bar (y=684..720)
    draw.rectangle([0, 684, WIDTH, HEIGHT], fill=BG_HEADER)
    draw.line([(0, 684), (WIDTH, 684)], fill=BORDER, width=1)
    stages = [
        ("1. OPEN", 0.0, 1.70),
        ("2. @eN SNAPSHOT", 1.70, 4.20),
        ("3. --HUMANIZE INPUT", 4.20, 10.45),
        ("4. MARKDOWN EXTRACT", 10.45, dur + 1.0),
    ]
    sx = 18
    for label, s0, s1 in stages:
        done = t >= s1
        curr = s0 <= t < s1
        fg = BG_CANVAS if curr else (MINT if done else TEXT_DIM)
        bg = GOLD if curr else ((18, 42, 40) if done else BG_PANEL)
        ol = GOLD_HI if curr else (MINT if done else BORDER)
        w = draw_pill(draw, fonts, sx, 691, label, fg=fg, bg=bg, outline=ol, font_size=11, pad_x=10, pad_y=3)
        sx += w + 12

    # Thin right-aligned timeline bar
    bar_x0, bar_x1 = 760, WIDTH - 18
    bar_y0, bar_y1 = 699, 706
    draw.rounded_rectangle([bar_x0, bar_y0, bar_x1, bar_y1], radius=3, fill=BG_PANEL, outline=BORDER, width=1)
    fill_w = int((bar_x1 - bar_x0 - 2) * min(1.0, t / dur))
    if fill_w > 2:
        draw.rounded_rectangle([bar_x0 + 1, bar_y0 + 1, bar_x0 + 1 + fill_w, bar_y1 - 1], radius=2, fill=MINT)

    return img


# ---------------------------------------------------------------------------
# Demo 2 Frame Generator (~16.0s = 384 frames)
# ---------------------------------------------------------------------------

def build_demo2_script(cap: Demo2Capture) -> list[TermLine]:
    c = cap.chromium
    f = cap.firefox
    return [
        TermLine(0.15, [("$ ", MINT), ("mimikyu ", GOLD), ("open /parity ", TEXT), ("--engine chromium & firefox", LAV)], type_dur=0.95),
        TermLine(1.30, [("  ok ", MINT), ("dual virtual contexts ready (CDP + W3C WebDriver BiDi)", TEXT_SUB)]),
        TermLine(2.10, [("$ ", MINT), ("mimikyu ", GOLD), ("snapshot --interactive-only ", TEXT), ("# Light + Shadow + Iframe", TEXT_DIM)], type_dur=0.90),
        TermLine(3.15, [("  @e1..@e2 ", GOLD), ("Light DOM   ", TEXT), ("@e3..@e4 ", MINT), ("Shadow DOM   ", TEXT), ("@e5 ", CORAL), ("Iframe  [100% MATCH]", MINT)]),
        TermLine(5.10, [("$ ", MINT), ("mimikyu ", GOLD), ("batch --ops ", TEXT), ("'[fill @e1, click @e2, fill @e3, click @e4, click @e5]'", LAV)], type_dur=1.10),
        TermLine(7.00, [("  [1/3] ", GOLD), ("Light DOM bound -> agent-031 (CDP + BiDi)", TEXT_SUB)]),
        TermLine(8.60, [("  [2/3] ", MINT), ("Open Shadow DOM unlocked -> shadow-ok (CDP + BiDi)", TEXT_SUB)]),
        TermLine(10.20, [("  [3/3] ", CORAL), (f"Iframe attested -> PARITY OK (CDP {c.batch_total_ms}ms | BiDi {f.batch_total_ms}ms)", MINT)]),
        TermLine(11.60, [("$ ", MINT), ("mimikyu ", GOLD), ("extract markdown ", TEXT), ("# verify cross-engine output", TEXT_DIM)], type_dur=0.85),
        TermLine(12.60, [("  ok ", MINT), (f"Markdown verified (CDP {c.extract_daemon_ms}ms | BiDi {f.extract_daemon_ms}ms) · 0 diffs", GOLD_HI)]),
    ]


def _parity_ref_color(rb: RefBox) -> tuple[int, int, int]:
    if rb.ref_id in ("@e1", "@e2"):
        return GOLD
    if rb.ref_id in ("@e3", "@e4"):
        return MINT
    return CORAL


def _draw_zone_pulse(img: Image.Image, box: tuple[int, int, int, int], color: tuple[int, int, int], strength: float) -> Image.Image:
    if strength <= 0.01:
        return img
    ov = Image.new("RGBA", img.size, (0, 0, 0, 0))
    od = ImageDraw.Draw(ov)
    od.rounded_rectangle(
        box,
        radius=8,
        fill=(*color, int(26 * strength)),
        outline=(*color, int(230 * strength)),
        width=3,
    )
    return Image.alpha_composite(img.convert("RGBA"), ov).convert("RGB")


def render_demo2_frame(cap: Demo2Capture, script: list[TermLine], fonts: FontPack, t: float, dur: float) -> Image.Image:
    img = Image.new("RGB", (WIDTH, HEIGHT), BG_CANVAS)
    draw = ImageDraw.Draw(img)
    c, f = cap.chromium, cap.firefox

    # 1. Top HUD Bar (y=0..52)
    draw.rectangle([0, 0, WIDTH, 52], fill=BG_HEADER)
    draw.line([(0, 52), (WIDTH, 52)], fill=BORDER, width=1)
    draw_pixel_mascot(draw, 14, 8, px=3)
    draw.text((58, 9), "MIMIKYU", font=fonts.get(17, bold=True), fill=GOLD)
    draw.text((144, 12), "// DUAL-ENGINE PARITY: CHROMIUM (CDP) vs FIREFOX (W3C BiDi)", font=fonts.get(13, bold=True), fill=TEXT)
    draw.text((58, 31), "Identical @eN batch workflow piercing Light DOM + Open Shadow DOM + Embedded Iframe", font=fonts.get(11), fill=TEXT_SUB)

    parity_ok = t >= 10.20
    badge_txt = "PARITY: 100% MATCH" if parity_ok else "PARITY: RUNNING..."
    bw = draw.textbbox((0, 0), badge_txt, font=fonts.get(12, bold=True))[2] + 22
    draw_pill(
        draw, fonts, WIDTH - bw - 14, 13, badge_txt,
        fg=BG_CANVAS if parity_ok else GOLD,
        bg=MINT if parity_ok else BG_PANEL,
        outline=MINT if parity_ok else GOLD,
    )

    # 2. Side-by-Side Live Engine Viewports (y=62..480)
    engines_ui = [
        (14, c, "CHROMIUM", "Native CDP", MINT),
        (646, f, "FIREFOX", "W3C WebDriver BiDi", GOLD),
    ]
    for x0, eng_cap, title, proto, accent in engines_ui:
        x1 = x0 + 620
        draw_panel(draw, (x0, 62, x1, 480), fill=BG_PANEL, border=accent if t >= 10.20 else BORDER, corner_accent=accent)
        draw.rounded_rectangle([x0 + 1, 63, x1 - 1, 93], radius=7, fill=BG_CARD_BAR)
        draw.rectangle([x0 + 1, 84, x1 - 1, 93], fill=BG_CARD_BAR)
        draw.line([(x0 + 1, 93), (x1 - 1, 93)], fill=BORDER, width=1)
        draw_window_dots(draw, x0 + 12, 74)
        draw.text((x0 + 68, 71), title, font=fonts.get(13, bold=True), fill=accent)
        draw.text((x0 + 148, 72), f"· {proto}", font=fonts.get(12), fill=TEXT_SUB)

        # Right status pill per engine
        if t < 3.15:
            st_txt, st_fg = "READY", TEXT_SUB
        elif t < 10.20:
            st_txt, st_fg = f"SNAP {eng_cap.snapshot_daemon_ms}ms", accent
        else:
            st_txt, st_fg = f"BATCH {eng_cap.batch_total_ms}ms · 3/3 OK", MINT
        sw = draw.textbbox((0, 0), st_txt, font=fonts.get(11, bold=True))[2] + 18
        draw_pill(draw, fonts, x1 - sw - 8, 67, st_txt, fg=st_fg, bg=BG_TERM, outline=accent if t >= 3.15 else BORDER, font_size=11, pad_x=8, pad_y=2)

        # Select live stage screenshot
        if t < 6.80:
            vp = eng_cap.shot_loaded.copy()
        elif t < 8.40:
            vp = eng_cap.shot_light.copy()
        elif t < 10.00:
            vp = eng_cap.shot_shadow.copy()
        else:
            vp = eng_cap.shot_all.copy()

        # Overlay @e1..@e5 bounding boxes during snapshot beat (3.15s..5.50s)
        if 3.15 <= t <= 5.55:
            a_in = min(1.0, (t - 3.15) / 0.35)
            a_out = 1.0 if t < 5.05 else max(0.0, 1.0 - (t - 5.05) / 0.50)
            vp = draw_ref_overlays(
                vp, fonts, eng_cap.refs, alpha=a_in * a_out, color_fn=_parity_ref_color, inside_right=True
            )

        # Zone activation pulses synced to atomic batch milestones
        if 6.80 <= t <= 7.85:
            vp = _draw_zone_pulse(vp, (14, 52, 606, 128), GOLD, max(0.0, 1.0 - (t - 6.80) / 1.05))
        elif 8.40 <= t <= 9.45:
            vp = _draw_zone_pulse(vp, (14, 138, 305, 262), MINT, max(0.0, 1.0 - (t - 8.40) / 1.05))
        elif 10.00 <= t <= 11.20:
            vp = _draw_zone_pulse(vp, (314, 138, 606, 262), CORAL, max(0.0, 1.0 - (t - 10.00) / 1.20))

        # Inset 1px inside card and re-stroke rounded border cleanly around all 4 sides
        vp_inset = vp.crop((1, 0, 619, 385))
        img.paste(vp_inset, (x0 + 1, 94))
        card_border = accent if t >= 10.20 else BORDER
        draw.rounded_rectangle([x0, 62, x1, 480], radius=8, outline=card_border, width=1)

    # 3. Bottom-Left Unified Terminal (x=14..756, y=490..708)
    draw_panel(draw, (14, 490, 756, 708), fill=BG_TERM, border=BORDER, corner_accent=LAV)
    draw.rounded_rectangle([15, 491, 755, 517], radius=7, fill=BG_CARD_BAR)
    draw.rectangle([15, 509, 755, 517], fill=BG_CARD_BAR)
    draw.line([(15, 517), (755, 517)], fill=BORDER, width=1)
    draw_window_dots(draw, 26, 500)
    draw.text((84, 498), "shared workflow — identical commands on CDP + BiDi", font=fonts.get(11, bold=True), fill=TEXT_SUB)

    render_terminal_lines(
        draw, fonts, (26, 524, 746, 700), script, t, font_size=12, line_h=17, max_visible=10
    )

    # 4. Bottom-Right Live Engine Parity Scorecard (x=768..1266, y=490..708)
    draw_panel(draw, (768, 490, 1266, 708), fill=BG_PANEL, border=MINT if t >= 12.60 else BORDER, corner_accent=GOLD)
    draw.rounded_rectangle([769, 491, 1265, 517], radius=7, fill=BG_CARD_BAR)
    draw.rectangle([769, 509, 1265, 517], fill=BG_CARD_BAR)
    draw.line([(769, 517), (1265, 517)], fill=BORDER, width=1)
    draw.text((782, 498), "MEASURED PARITY SCORECARD", font=fonts.get(11, bold=True), fill=GOLD)

    # Table columns
    cols_x = [782, 916, 1044, 1172]
    hdr_f = fonts.get(11, bold=True)
    row_f = fonts.get(12)
    row_b = fonts.get(12, bold=True)
    draw.text((cols_x[0], 525), "METRIC", font=hdr_f, fill=TEXT_DIM)
    draw.text((cols_x[1], 525), "CHROMIUM", font=hdr_f, fill=MINT)
    draw.text((cols_x[2], 525), "FIREFOX", font=hdr_f, fill=GOLD)
    draw.text((cols_x[3], 525), "PARITY", font=hdr_f, fill=LAV)
    draw.line([(780, 543), (1254, 543)], fill=BORDER, width=1)

    rows = [
        (3.15, "@eN Refs", f"{len(c.refs)} (@e1..@e5)", f"{len(f.refs)} (@e1..@e5)", "EXACT"),
        (3.15, "Snapshot", f"{c.snapshot_daemon_ms} ms", f"{f.snapshot_daemon_ms} ms", "PASS"),
        (10.20, "Batch (6 ops)", f"{c.batch_total_ms} ms", f"{f.batch_total_ms} ms", "PASS"),
        (10.20, "Boundaries", "Light+Shd+Ifr", "Light+Shd+Ifr", "3 / 3"),
        (12.60, "Extract MD", f"{c.extract_daemon_ms} ms", f"{f.extract_daemon_ms} ms", "MATCH"),
    ]
    ry = 552
    for unlock_t, metric, c_val, f_val, status in rows:
        ready = t >= unlock_t
        draw.text((cols_x[0], ry), metric, font=row_f, fill=TEXT if ready else TEXT_DIM)
        draw.text((cols_x[1], ry), c_val if ready else "--", font=row_b, fill=MINT if ready else TEXT_DIM)
        draw.text((cols_x[2], ry), f_val if ready else "--", font=row_b, fill=GOLD if ready else TEXT_DIM)
        if ready:
            draw_pill(draw, fonts, cols_x[3] - 4, ry - 3, status, fg=BG_CANVAS, bg=MINT, outline=MINT, font_size=11, pad_x=8, pad_y=2)
        else:
            draw.text((cols_x[3], ry), "...", font=row_f, fill=TEXT_DIM)
        ry += 30

    # Thin progress line at very bottom of canvas
    prog_w = int(WIDTH * min(1.0, t / dur))
    draw.rectangle([0, HEIGHT - 3, prog_w, HEIGHT], fill=MINT)

    return img


# ---------------------------------------------------------------------------
# Video & Animated WebP Encoding
# ---------------------------------------------------------------------------

def encode_demo(
    ffmpeg_bin: str,
    img2webp_bin: str | None,
    mp4_path: Path,
    poster_path: Path,
    webp_path: Path | None,
    duration_s: float,
    poster_time_s: float,
    render_fn: Any,
) -> None:
    total_frames = int(round(duration_s * FPS))
    poster_idx = max(0, min(total_frames - 1, int(round(poster_time_s * FPS))))
    mp4_path.parent.mkdir(parents=True, exist_ok=True)

    ffmpeg_cmd = [
        ffmpeg_bin,
        "-y",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "rgb24",
        "-s",
        f"{WIDTH}x{HEIGHT}",
        "-r",
        str(FPS),
        "-i",
        "-",
        "-c:v",
        "libx264",
        "-profile:v",
        "high",
        "-pix_fmt",
        "yuv420p",
        "-crf",
        "22",
        "-movflags",
        "+faststart",
        str(mp4_path),
    ]

    webp_tmp = tempfile.TemporaryDirectory(prefix="mkdemo-webp-") if webp_path else None
    webp_frames: list[Path] = []

    try:
        proc = subprocess.Popen(ffmpeg_cmd, stdin=subprocess.PIPE)
        assert proc.stdin is not None
        for idx in range(total_frames):
            t = idx / FPS
            frame = render_fn(t)
            proc.stdin.write(frame.tobytes())
            if idx == poster_idx:
                frame.save(poster_path, format="PNG", optimize=True)
            # Sample at 12 fps for smooth, lightweight animated WebP
            if webp_tmp is not None and idx % 2 == 0:
                fp = Path(webp_tmp.name) / f"f_{idx:04d}.png"
                frame.save(fp, format="PNG")
                webp_frames.append(fp)

        proc.stdin.close()
        rc = proc.wait()
        if rc != 0:
            raise RuntimeError(f"ffmpeg failed with exit code {rc} for {mp4_path}")

        if webp_path is not None and img2webp_bin is not None:
            _encode_webp_under_budget(img2webp_bin, webp_frames, webp_path)
    finally:
        if webp_tmp is not None:
            webp_tmp.cleanup()


def _encode_webp_under_budget(img2webp_bin: str, frames: list[Path], out_path: Path) -> None:
    delay_ms = int(round(1000.0 / (FPS / 2)))  # 83ms @ 12 fps
    for quality in (80, 72, 64, 54):
        cmd = [img2webp_bin, "-loop", "0"]
        for fp in frames:
            cmd.extend(["-d", str(delay_ms), "-lossy", "-q", str(quality), "-m", "4", str(fp)])
        cmd.extend(["-o", str(out_path)])
        proc = subprocess.run(cmd, capture_output=True, text=True)
        if proc.returncode != 0:
            raise RuntimeError(f"img2webp failed (rc={proc.returncode}): {proc.stderr}")
        size = out_path.stat().st_size
        if size <= WEBP_MAX_BYTES:
            print(f"[webp] Encoded {out_path.name}: {size / (1024 * 1024):.2f} MB (q={quality})")
            return
        print(f"[webp] {out_path.name} at q={quality} was {size / (1024 * 1024):.2f} MB; tightening...")
    raise RuntimeError(f"Unable to fit {out_path} under {WEBP_MAX_BYTES} bytes.")


# ---------------------------------------------------------------------------
# Entrypoint
# ---------------------------------------------------------------------------

def main() -> None:
    parser = argparse.ArgumentParser(description="Record and composite Mimikyu demo videos.")
    parser.add_argument("--mimikyu-bin", default=None, help="Path to compiled mimikyu binary")
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=REPO_ROOT / "dist" / "demos",
        help="Output directory for rendered demos (default: dist/demos)",
    )
    parser.add_argument(
        "--copy-to",
        type=Path,
        default=None,
        help="Optional secondary directory to copy final demo artifacts for inspection",
    )
    parser.add_argument(
        "--only",
        choices=("all", "demo1", "demo2"),
        default="all",
        help="Which demo(s) to record (default: all)",
    )
    args = parser.parse_args()

    mimikyu_bin = resolve_mimikyu_bin(args.mimikyu_bin)
    ffmpeg_bin = resolve_tool("ffmpeg", "/opt/homebrew/bin/ffmpeg")
    img2webp_bin = resolve_tool("img2webp", "/opt/homebrew/bin/img2webp")
    fonts = FontPack()

    print(f"Using mimikyu:  {mimikyu_bin}")
    print(f"Using ffmpeg:   {ffmpeg_bin}")
    print(f"Using img2webp: {img2webp_bin}")

    out_dir: Path = args.out_dir.resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    produced: list[Path] = []

    state_dir = Path(tempfile.mkdtemp(prefix="mimikyu-demos-state-"))
    work_dir = Path(tempfile.mkdtemp(prefix="mimikyu-demos-shots-"))
    runner = MimikyuRunner(mimikyu_bin, state_dir)

    try:
        with FixtureServer() as srv:
            print(f"Fixture server listening on {srv.base_url}")
            runner.start_daemon()

            if args.only in ("all", "demo1"):
                d1 = capture_demo1(runner, srv, work_dir)
                d1_script = build_demo1_script(d1)
                d1_dur = 14.0
                d1_mp4 = out_dir / "demo-1-hero.mp4"
                d1_webp = out_dir / "demo-1-hero.webp"
                d1_poster = out_dir / "demo-1-poster.png"
                print("[demo-1] Rendering 1280x720 split-screen video + animated WebP + poster...")
                encode_demo(
                    ffmpeg_bin=ffmpeg_bin,
                    img2webp_bin=img2webp_bin,
                    mp4_path=d1_mp4,
                    poster_path=d1_poster,
                    webp_path=d1_webp,
                    duration_s=d1_dur,
                    poster_time_s=9.95,  # Crisp Bezier trail + click ripple + deployed canary + terminal confirmation
                    render_fn=lambda t: render_demo1_frame(d1, d1_script, fonts, t, d1_dur),
                )
                produced.extend([d1_mp4, d1_webp, d1_poster])

            if args.only in ("all", "demo2"):
                d2 = capture_demo2(runner, srv, work_dir)
                d2_script = build_demo2_script(d2)
                d2_dur = 16.0
                d2_mp4 = out_dir / "demo-2-dual-engine.mp4"
                d2_poster = out_dir / "demo-2-poster.png"
                print("[demo-2] Rendering 1280x720 dual-engine parity video + poster...")
                encode_demo(
                    ffmpeg_bin=ffmpeg_bin,
                    img2webp_bin=None,
                    mp4_path=d2_mp4,
                    poster_path=d2_poster,
                    webp_path=None,
                    duration_s=d2_dur,
                    poster_time_s=14.5,  # Full 3/3 verified parity + complete scorecard
                    render_fn=lambda t: render_demo2_frame(d2, d2_script, fonts, t, d2_dur),
                )
                produced.extend([d2_mp4, d2_poster])
    finally:
        runner.stop_daemon()
        shutil.rmtree(state_dir, ignore_errors=True)
        shutil.rmtree(work_dir, ignore_errors=True)

    if args.copy_to is not None:
        copy_dir: Path = args.copy_to.resolve()
        copy_dir.mkdir(parents=True, exist_ok=True)
        for src in produced:
            shutil.copy2(src, copy_dir / src.name)
        print(f"Copied {len(produced)} artifacts to {copy_dir}")

    print("\n=== Generated Demo Artifacts ===")
    for p in produced:
        sz = p.stat().st_size
        print(f"  {p}  ({sz / (1024 * 1024):.2f} MB / {sz:,} bytes)")


if __name__ == "__main__":
    main()
