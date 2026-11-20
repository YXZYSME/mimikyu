#!/usr/bin/env python3
"""Generate the 1200x630 16-bit Dark HUD OpenGraph social card (website/public/og-card.png)."""

from __future__ import annotations

import os
import pathlib
import re
from xml.etree import ElementTree as ET

from PIL import Image, ImageDraw, ImageFont

ROOT = pathlib.Path(__file__).resolve().parents[2]
MASCOT_SVG = ROOT / "assets" / "mimikyu-mascot.svg"
OUT_PNG = ROOT / "website" / "public" / "og-card.png"

# Exact 16-bit Dark HUD palette from assets/*.svg
C_VOID = "#090414"
C_BG = "#120924"
C_PANEL = "#180D2E"
C_CARD = "#22133F"
C_BORDER = "#563A9E"
C_VIOLET = "#9D80EB"
C_GOLD = "#F5B841"
C_GOLD_BRIGHT = "#FFE47A"
C_MINT = "#4CE0B3"
C_MINT_BRIGHT = "#9EFCE4"
C_CORAL = "#FF6B8B"
C_TEXT = "#F4EEFF"
C_MUTED = "#C9B8F5"


def find_font(candidates: list[str], size: int) -> ImageFont.FreeTypeFont | ImageFont.ImageFont:
    search_dirs = [
        pathlib.Path.home() / "Library" / "Fonts",
        pathlib.Path("/System/Library/Fonts"),
        pathlib.Path("/System/Library/Fonts/Supplemental"),
        pathlib.Path("/Library/Fonts"),
        pathlib.Path("/usr/share/fonts"),
    ]
    for name in candidates:
        for base in search_dirs:
            p = base / name
            if p.is_file():
                try:
                    return ImageFont.truetype(str(p), size=size)
                except OSError:
                    pass
    return ImageFont.load_default()


def render_pixel_svg(svg_path: pathlib.Path, scale: int = 2) -> Image.Image:
    """Parse rectangular M/h/v/h/Z pixel paths from our crispEdges SVG into a sharp RGBA image."""
    tree = ET.parse(svg_path)
    root = tree.getroot()
    w = int(float(root.attrib.get("width", "128")))
    h = int(float(root.attrib.get("height", "128")))
    img = Image.new("RGBA", (w * scale, h * scale), (0, 0, 0, 0))
    draw = ImageDraw.Draw(img)

    rect_re = re.compile(r"M(\d+)\s+(\d+)h(\d+)v(\d+)h-(\d+)Z")
    for elem in root.iter():
        if not elem.tag.endswith("path"):
            continue
        fill = elem.attrib.get("fill")
        d = elem.attrib.get("d", "")
        if not fill or not d:
            continue
        for m in rect_re.finditer(d):
            x, y, rw, rh = int(m.group(1)), int(m.group(2)), int(m.group(3)), int(m.group(4))
            draw.rectangle(
                [x * scale, y * scale, (x + rw) * scale - 1, (y + rh) * scale - 1],
                fill=fill,
            )
    return img


def draw_pixel_border(draw: ImageDraw.ImageDraw, box: tuple[int, int, int, int], outer: str, inner: str, corner: str) -> None:
    x0, y0, x1, y1 = box
    # Outer dark chamfer border
    draw.rectangle([x0, y0, x1, y1], outline=outer, width=4)
    # Inner accent border
    draw.rectangle([x0 + 6, y0 + 6, x1 - 6, y1 - 6], outline=inner, width=3)
    # Corner pixel rivets
    s = 10
    for cx, cy in [
        (x0 + 14, y0 + 14),
        (x1 - 14 - s, y0 + 14),
        (x0 + 14, y1 - 14 - s),
        (x1 - 14 - s, y1 - 14 - s),
    ]:
        draw.rectangle([cx, cy, cx + s, cy + s], fill=corner)


def draw_pill(
    draw: ImageDraw.ImageDraw,
    x: int,
    y: int,
    text: str,
    font: ImageFont.ImageFont,
    fg: str,
    border: str,
    bg: str = C_PANEL,
    pad_x: int = 18,
    pad_y: int = 10,
) -> int:
    bbox = draw.textbbox((0, 0), text, font=font)
    tw, th = bbox[2] - bbox[0], bbox[3] - bbox[1]
    w = tw + pad_x * 2
    h = th + pad_y * 2
    draw.rectangle([x, y, x + w, y + h], fill=bg, outline=border, width=2)
    draw.text((x + pad_x, y + pad_y - bbox[1]), text, font=font, fill=fg)
    return w


def main() -> None:
    W, H = 1200, 630
    img = Image.new("RGBA", (W, H), C_VOID)
    draw = ImageDraw.Draw(img)

    # Subtle 16-bit pixel grid inside main HUD canvas
    draw.rectangle([20, 20, W - 20, H - 20], fill=C_BG)
    for gx in range(36, W - 36, 24):
        for gy in range(36, H - 36, 24):
            draw.rectangle([gx, gy, gx + 1, gy + 1], fill="#1E1136")

    # Outer double HUD frame
    draw_pixel_border(draw, (16, 16, W - 16, H - 16), C_BORDER, C_VIOLET, C_GOLD)

    # Top status header strip
    draw.rectangle([38, 38, W - 38, 92], fill=C_PANEL, outline=C_BORDER, width=2)

    mono_bold = lambda s: find_font(
        [
            "JetBrainsMonoNerdFontMono-ExtraBold.ttf",
            "JetBrainsMonoNerdFont-Bold.ttf",
            "JetBrainsMono-Bold.ttf",
            "Menlo.ttc",
            "Courier New Bold.ttf",
        ],
        s,
    )
    mono_med = lambda s: find_font(
        [
            "JetBrainsMonoNerdFontMono-SemiBold.ttf",
            "JetBrainsMonoNerdFont-Medium.ttf",
            "Menlo.ttc",
            "Monaco.ttf",
        ],
        s,
    )
    mono_reg = lambda s: find_font(
        [
            "JetBrainsMonoNerdFontMono-Regular.ttf",
            "JetBrainsMonoNerdFont-Regular.ttf",
            "Menlo.ttc",
            "Monaco.ttf",
        ],
        s,
    )

    f_kicker = mono_bold(20)
    f_title = mono_bold(76)
    f_brand = mono_bold(24)
    f_sub = mono_med(24)
    f_pill = mono_bold(21)
    f_kpi_val = mono_bold(32)
    f_kpi_lbl = mono_reg(16)
    f_footer = mono_bold(20)

    # Status LEDs & Header Kicker
    draw.rectangle([58, 57, 72, 71], fill=C_GOLD)
    draw.rectangle([80, 57, 94, 71], fill=C_MINT)
    draw.rectangle([102, 57, 116, 71], fill=C_VIOLET)
    draw.text((136, 53), "AGENTDEX // ENTRY NO. 031", font=f_kicker, fill=C_GOLD_BRIGHT)
    draw.text((W - 320, 53), "RUST 1.88+ · MIT/APACHE", font=f_kicker, fill=C_MINT_BRIGHT)

    # Left mascot sprite (256x256 crisp nearest-neighbor from SVG)
    mascot = render_pixel_svg(MASCOT_SVG, scale=2)
    mascot_x, mascot_y = 66, 134
    # Subtle drop shadow plate behind mascot
    draw.rectangle(
        [mascot_x + 8, mascot_y + 8, mascot_x + 256 + 8, mascot_y + 256 + 8],
        fill=C_VOID,
    )
    img.alpha_composite(mascot, (mascot_x, mascot_y))

    # Right side title block
    rx = 368
    # Wordmark with crisp pixel drop shadow
    draw.text((rx + 4, 134 + 4), "MIMIKYU", font=f_title, fill=C_VOID)
    draw.text((rx, 134), "MIMIKYU", font=f_title, fill=C_GOLD_BRIGHT)

    # "by Saengil" pill right beside MIMIKYU
    title_bbox = draw.textbbox((rx, 134), "MIMIKYU", font=f_title)
    draw_pill(
        draw,
        title_bbox[2] + 24,
        158,
        "by Saengil",
        f_brand,
        fg=C_TEXT,
        border=C_VIOLET,
        bg=C_CARD,
        pad_x=20,
        pad_y=10,
    )

    # Tagline
    draw.text(
        (rx, 232),
        "Native Rust Agentic Browser Runtime & Isolation Layer",
        font=f_sub,
        fill=C_TEXT,
    )
    draw.text(
        (rx, 268),
        "Local Chromium (CDP) + Firefox (W3C WebDriver BiDi) · Warm Daemon",
        font=mono_reg(19),
        fill=C_MUTED,
    )

    # Dashed pixel divider on right
    for dx in range(rx, W - 58, 20):
        color = C_GOLD if dx < rx + 240 else C_BORDER
        draw.rectangle([dx, 310, min(dx + 10, W - 58), 313], fill=color)

    # Feature pills row
    px = rx
    py = 332
    pills = [
        ("5.0 ms snapshots", C_GOLD_BRIGHT, C_GOLD),
        ("CDP + BiDi", C_MINT_BRIGHT, C_MINT),
        ("31 MCP tools", C_TEXT, C_VIOLET),
        ("Rust", C_CORAL, C_CORAL),
    ]
    for label, fg, border in pills:
        pw = draw_pill(draw, px, py, label, f_pill, fg=fg, border=border, bg=C_PANEL, pad_x=16, pad_y=10)
        px += pw + 14

    # Bottom 4-column HUD KPI bar
    bar_y0, bar_y1 = 428, 526
    draw.rectangle([48, bar_y0, W - 48, bar_y1], fill=C_PANEL, outline=C_BORDER, width=2)
    kpis = [
        ("5.0 ms", "SEMANTIC @eN SNAPSHOT", C_GOLD_BRIGHT),
        ("2.8x–3.6x", "TOKEN COMPRESSION", C_MINT_BRIGHT),
        ("31 TOOLS", "MCP 2024-11-05 STDIO", C_VIOLET),
        ("581 TESTS", "337 RUST + 244 E2E", C_TEXT),
    ]
    col_w = (W - 96) // 4
    for i, (val, lbl, col) in enumerate(kpis):
        cx = 48 + i * col_w
        if i > 0:
            draw.rectangle([cx, bar_y0 + 16, cx + 2, bar_y1 - 16], fill=C_BORDER)
        draw.text((cx + 24, bar_y0 + 18), val, font=f_kpi_val, fill=col)
        draw.text((cx + 24, bar_y0 + 62), lbl, font=f_kpi_lbl, fill=C_MUTED)

    # Bottom URL bar
    draw.rectangle([48, 544, W - 48, 588], fill=C_CARD, outline=C_VIOLET, width=2)
    draw.text((70, 554), "https://mimikyu.saengil.ai", font=f_footer, fill=C_MINT_BRIGHT)
    draw.text((W - 430, 554), "github.com/YXZYSME/mimikyu", font=f_footer, fill=C_MUTED)

    OUT_PNG.parent.mkdir(parents=True, exist_ok=True)
    img.convert("RGB").save(OUT_PNG, format="PNG", optimize=True)
    print(f"Generated {OUT_PNG} ({W}x{H}, {OUT_PNG.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
