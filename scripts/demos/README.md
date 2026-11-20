# Mimikyu Demo Recording & Compositing Pipeline

Self-contained, offline, reproducible demo video and animated WebP pipeline driving the real compiled `mimikyu` binary against local `127.0.0.1:0` dark-mode web fixtures — with **zero macOS Screen Recording permission prompts** and **zero external network calls**.

## Files

- [`fixtures.py`](fixtures.py): Local offline `ThreadingHTTPServer` on `127.0.0.1:0` serving:
  - `/dashboard`: Dark-mode SaaS Fleet Catalog with `@eN`-friendly controls and dynamic table filtering/canary state.
  - `/parity` + `/iframe-panel`: Light DOM + Open Shadow DOM (`<mimikyu-vault>`) + embedded `<iframe>` workbench.
  - Both routes include a lightweight demo-only glowing SVG cursor, curved Bézier trail, and click-ripple overlay triggered by real `pointermove` / `pointerdown` events from `mimikyu --humanize`.
- [`record_demos.py`](record_demos.py): Live capture + 1280×720 HUD compositor driving real Chromium (CDP) and Firefox (W3C WebDriver BiDi) sessions via `mimikyu`, rendering synchronized dark `#0E071C` terminal panes + real measured latencies + live browser frames, and encoding H.264 `.mp4`, looping `.webp`, and `.png` posters.
- [`record_raw_live.py`](record_raw_live.py): **100% raw, unstylized browser screencast** using `mimikyu record start/stop` (CDP `Page.startScreencast`) with zero HUD overlays or synthetic cursors across real public websites (`github.com/YXZYSME/mimikyu`, `crates.io/crates/mimikyu`, `pypi.org/project/mimikyu/`, `news.ycombinator.com`).

## Prerequisites

- Compiled `mimikyu` binary (`cargo build --release`, or pass `--mimikyu-bin` / `MIMIKYU_BIN=/path/to/mimikyu`)
- Local Chromium and Firefox binaries discoverable by `mimikyu doctor`
- Python 3 with [Pillow](https://python-pillow.org/) (`python3 -c "import PIL"`)
- `ffmpeg` (with `libx264`) and `img2webp` (`brew install ffmpeg webp`)

## Regenerating All Demos

```bash
# Preview the local offline fixtures interactively in a browser (optional)
python3 scripts/demos/fixtures.py --port 8765

# Record and render all demos into dist/demos/ (gitignored)
python3 scripts/demos/record_demos.py

# Or override the mimikyu binary / output directory / single demo
MIMIKYU_BIN=./target/release/mimikyu \
  python3 scripts/demos/record_demos.py --out-dir dist/demos --only all
```

### Generated Outputs (`dist/demos/`, gitignored)

| Artifact | Spec | Description |
| :--- | :--- | :--- |
| `demo-1-hero.mp4` | `1280×720`, 24 fps, H.264 High (`yuv420p`, CRF 22, `+faststart`), ~14s | Split-screen Hero: *The 5ms Agentic Loop (`@eN` → `--humanize` → Markdown)* |
| `demo-1-hero.webp` | `1280×720`, 12 fps, looping WebP (`< 2.5 MB`) | Lightweight looping animated preview for `README.md` / landing page |
| `demo-1-poster.png` | `1280×720` PNG | High-contrast hero poster frame with `--humanize` Bézier trail & ripple |
| `demo-2-dual-engine.mp4` | `1280×720`, 24 fps, H.264 High (`yuv420p`, CRF 22, `+faststart`), ~16s | Side-by-side Chromium (CDP) vs Firefox (W3C BiDi) `@eN` batch + Shadow DOM + `<iframe>` parity |
| `demo-2-poster.png` | `1280×720` PNG | Final dual-engine parity scorecard poster frame |

> **Git hygiene:** Never commit rendered `.mp4`, `.webp`, or large `.png` demo artifacts to git (`dist/` is already listed in `.gitignore`).

## Publishing Demos to GitHub Releases & `README.md`

### Option A: GitHub Release Assets (Recommended for versioned docs & site)

```bash
gh release upload v0.1.0 \
  dist/demos/demo-1-hero.mp4 \
  dist/demos/demo-1-hero.webp \
  dist/demos/demo-1-poster.png \
  dist/demos/demo-2-dual-engine.mp4 \
  dist/demos/demo-2-poster.png \
  --clobber
```

Reference the uploaded Release URLs from docs or the website, e.g.:
`https://github.com/YXZYSME/mimikyu/releases/download/v0.1.0/demo-1-hero.webp`

### Option B: GitHub README Inline Video (`user-attachments/assets`)

1. Open any GitHub Issue/PR comment box (or edit `README.md` on GitHub.com).
2. Drag-and-drop `dist/demos/demo-1-hero.mp4` and `dist/demos/demo-2-dual-engine.mp4` into the editor to upload them to `https://github.com/user-attachments/assets/<uuid>`.
3. Paste the resulting asset URLs directly on their own lines in `README.md` — GitHub automatically renders native HTML5 `<video>` players with poster previews.
4. For the animated WebP hero banner, upload `dist/demos/demo-1-hero.webp` the same way (or link the Release asset) inside an `<img>` tag so it auto-plays smoothly everywhere.
