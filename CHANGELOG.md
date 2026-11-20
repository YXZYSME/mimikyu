# Changelog

All notable changes to `mimikyu` will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-19

### Added

- **Dual-Engine Native Rust Browser Runtime (`mimikyu-core`)**:
  - Unified `DriverSession` abstraction driving local **Chromium** via native WebSocket DevTools Protocol (`driver::cdp`) and **Firefox / Gecko** via W3C WebDriver BiDi (`driver::bidi`).
  - Automatic local browser discovery across explicit path overrides, `MIMIKYU_CHROMIUM_PATH` / `MIMIKYU_FIREFOX_PATH`, Playwright/Puppeteer caches, system bundles, and `PATH`.
- **Two-Tier Session & Profile Isolation (`isolation`)**:
  - **Tier 1 (Process & Disk Profile)**: Exclusive `fs2`-locked persistent profiles (`--profile`), self-cleaning ephemeral directories (`--ephemeral`), and portable JSON cookie/storage export and import (`session export/import`, `profile export/import`).
  - **Tier 2 (Virtual Browser Contexts)**: Warm pooled lightweight contexts (`--virtual` / `VirtualContextPool`) for low-latency concurrent agent sessions.
- **Environment Consistency (`env_profile`)**:
  - Built-in `win11`, `macos`, and `linux` OS environment presets with cross-checked User-Agent, platform, screen/viewport, locale, and timezone coherence validation.
  - Synchronized window, Worker, and SharedWorker initialization script injection.
  - Offline GeoIP timezone/locale derivation (`--geoip`) and loopback `ProxyRelay` (`127.0.0.1:0`) supporting upstream HTTP and SOCKS5 authentication without exposing credentials on browser command lines.
- **Token-Efficient Agentic Browser Primitives (`snapshot`, `extract`, `wait`, `input`)**:
  - Compact semantic accessibility tree snapshots (`mimikyu snapshot`) with deterministic `@eN` element refs across open Shadow DOM and same-origin/nested/cross-origin iframes, plus `--interactive-only`, `--max-depth`, `--include-bounds`, and structural `--diff`.
  - Structured page extraction (`markdown`, `text`, `links`, `tables`), PNG screenshots (viewport, `--full-page`, `--target`), PDF export (`mimikyu pdf`), and `--instant-animations`.
  - Deterministic readiness waits (`--idle` combining lifecycle + network quiet + layout stability, `--text`, `--url`, and `--selector --state`).
  - Serialized `ChokepointInput` boundary with optional `--humanize` kinematic pointer trajectories, typing cadence variance, split wheel notches, and single-round-trip `ActionBatcher` (`mimikyu batch`).
- **Network Resource Control & Telemetry (`network`)**:
  - Per-session resource blocking (`--block-images`, `--block-media`, `--block-fonts`), request interception/routing, and bounded ring-buffer console/network logs (`mimikyu logs`).
- **Warm Daemon, Unified CLI & 31-Tool MCP Server (`daemon`, `mcp`, `mimikyu-cli`)**:
  - Warm Unix-socket NDJSON background daemon with auto-spawn, configurable idle timeout, and automatic `/tmp/mkd-<sha256>.sock` symlinking when state paths approach `SUN_LEN`.
  - Unified `mimikyu` CLI supporting `--json` structured envelopes across all subcommands and `--no-daemon` direct execution.
  - Stateful stdio JSON-RPC 2.0 MCP server (`protocolVersion = "2024-11-05"`) exposing 11 `mimikyu_*` and 20 `browser_*` tools.
- **Reproducible Offline Benchmark & Verification Suites (`harness`, `bench`, `e2e`)**:
  - Embedded `127.0.0.1:0` dual-origin HTTP benchmark server and `mimikyu bench` CLI measuring cold launch, warm virtual context creation, navigation, `@eN` snapshot latency (~5.0 ms median on Chromium), token compression ratio, live process-tree RSS, and 5 end-to-end agentic workflows across Chromium and Firefox.
  - Standalone 244-test opaque-box Python 3 stdlib E2E verification suite (`e2e/run_e2e.py`) across Tiers 1–4 and `--self-test`.

### Security & Host Reliability

- **Isolated-World Evaluation by Default**: `mimikyu eval` executes JavaScript inside an isolated execution world unless `--main-world` is explicitly requested.
- **macOS Zero-Spawn Firefox Probe & Crash Suppression**: Reads `application.ini` and `Contents/Info.plist` directly from disk (avoiding `firefox --version` exit races on macOS) and launches Firefox with `MOZ_CRASHREPORTER_DISABLE=1`, `MOZ_CRASHREPORTER=0`, and `MOZ_CRASHREPORTER_NO_REPORT=1`.
- **Deterministic Process-Group Reaping**: Isolates every spawned browser in its own POSIX process group (`setpgid(0, 0)`) with graceful protocol close followed by deterministic process-group cleanup on shutdown, signal, or `Drop`.

[Unreleased]: https://github.com/YXZYSME/mimikyu/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/YXZYSME/mimikyu/releases/tag/v0.1.0
