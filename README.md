# mimikyu

High-speed Rust agentic browser runtime, session/profile isolation layer, and CLI / Daemon / MCP control plane driving local **Chromium (CDP)** and **Firefox (W3C WebDriver BiDi)** browsers.

Inspired by the documented base architecture of [Camoufox](https://camoufox.com/development/) — consistent environment profiles, two-tier browser/context isolation, proxy/GeoIP alignment, kinematic input modeling, and resource control — `mimikyu` pairs those primitives with token-efficient AI-agent tooling (`@eN` accessibility tree snapshots, structural diffs, structured extraction, atomic action batching, a warm Unix-socket daemon, and a stdio MCP server) in a zero-dependency native Rust binary.

---

## Architecture

`mimikyu` is organized as a two-crate Rust workspace (Edition 2024, MSRV `1.88`) plus an opaque-box Python 3 stdlib end-to-end test suite:

```text
crates/
├── mimikyu-core/src/
│   ├── discovery.rs      # Local Chromium & Firefox discovery + macOS zero-spawn Firefox metadata probe
│   ├── process.rs        # POSIX process-group isolation (setpgid), deterministic reaping, crash suppression
│   ├── driver/           # Native WebSocket CDP (Chromium) & W3C WebDriver BiDi (Firefox) drivers
│   ├── isolation/        # Two-tier isolation: persistent/ephemeral profiles (fs2 locks) & virtual contexts
│   ├── env_profile/      # OS presets (win11|macos|linux), offline GeoIP, coherence checks, ProxyRelay, init scripts
│   ├── network/          # Resource blocking, request interception/routing, ring-buffer telemetry
│   ├── snapshot/         # Compact semantic accessibility tree, deterministic @eN ref map, structural diffs
│   ├── extract/          # Markdown, plain text, links, HTML tables, PNG screenshots, PDF, instant animations
│   ├── input/            # Serialized ChokepointInput, optional kinematic humanize motion, ActionBatcher
│   ├── wait/             # Page idle (lifecycle + network quiet + layout stability), text, URL, selector waits
│   ├── session.rs        # Unified SessionManager orchestrating engines, isolation, state, and telemetry
│   ├── daemon/           # Warm Unix-socket NDJSON daemon with auto-spawn, idle timeout, and socket symlinking
│   ├── mcp/              # Stdio JSON-RPC 2.0 MCP server (protocol 2024-11-05, 31 tools)
│   ├── harness/          # Embedded 127.0.0.1:0 dual-origin (primary + cross-origin) local HTTP test server
│   └── bench/            # Reproducible local speed & 5-workflow agentic benchmark suite + live RSS tracking
└── mimikyu-cli/src/      # Unified `mimikyu` CLI binary (--json envelope, daemon client, MCP & bench frontends)
e2e/                      # Opaque-box Python 3 stdlib E2E test suite (Tiers 1–4 + --self-test) on 127.0.0.1:0
```

### Runtime Flow

1. **CLI / MCP Frontend**:
   - Interactive and scripted CLI invocations connect over a local Unix domain socket (`<state-dir>/daemon.sock`, automatically symlinking to `/tmp/mkd-<sha256>.sock` when paths approach `SUN_LEN`), auto-starting a warm background daemon unless `--no-daemon` is passed.
   - `mimikyu mcp` runs a stateful stdio JSON-RPC 2.0 server backed directly by `SessionManager`.
2. **Two-Tier Isolation**:
   - **Tier 1 (Process & Disk Profile)**: Dedicated browser process with either a locked named/path persistent profile (`--profile`, exclusive `fs2` lock, JSON cookie/storage export & import) or a self-cleaning temporary directory (`--ephemeral`).
   - **Tier 2 (Virtual Browser Context)**: Lightweight isolated browser contexts (`--virtual` / `--virtual-ctx`) allocated inside a warm shared `VirtualContextPool` for low-latency multi-session concurrency.
3. **Dual-Engine Drivers**:
   - `driver::cdp` speaks native Chromium DevTools Protocol over WebSocket.
   - `driver::bidi` speaks W3C WebDriver BiDi over WebSocket for Firefox/Gecko.
   - Both implement `DriverSession` and shut down gracefully via `Browser.close` / `browser.close` before process-group cleanup.

---

## Features

### Camoufox-Inspired Environment & Runtime Controls
- **Consistent Environment Profiles**: Built-in `win11`, `macos`, and `linux` presets with cross-checked UA/platform/screen/locale/timezone coherence validation and synchronized window + worker init scripts (including SharedWorker alignment).
- **Offline GeoIP & Proxy Alignment**: Local GeoIP lookup (`--geoip`) for timezone/locale derivation and an embedded local `ProxyRelay` (`--proxy`) supporting authenticated HTTP and SOCKS5 upstreams without leaking credentials in browser flags.
- **Two-Tier Session & Profile Isolation**: Persistent locked profiles, ephemeral temp profiles, storage state export/import (`session export/import`, `profile export/import`), and warm pooled virtual contexts.
- **Network & Resource Control**: Per-session blocking for images (`--block-images`), media (`--block-media`), and fonts (`--block-fonts`), plus request interception and bounded console/network telemetry logs (`mimikyu logs`).
- **Kinematic Humanized Input**: Opt-in `--humanize` mode on `session new`, `click`, `fill`, `type`, and `scroll` with viewport-clamped pointer trajectories, typing cadence variance, and split wheel notches, serialized through `ChokepointInput` with bounded ACK timeouts.

### Agentic Browser Primitives
- **Compact `@eN` Accessibility Snapshots**: `mimikyu snapshot` distills pages (including open Shadow DOM and same-origin/nested/cross-origin iframes) into a compact semantic tree with deterministic `@eN` element refs, supporting `--interactive-only`, `--max-depth`, `--include-bounds`, and `--diff`.
- **Structured Extraction & Media**: `mimikyu extract <markdown|text|links|tables>`, `mimikyu screenshot` (viewport, `--full-page`, or `--target`), `mimikyu pdf`, and `--instant-animations`.
- **Deterministic Waits & Atomic Batching**: `mimikyu wait` (`--idle`, `--text`, `--url`, `--selector --state <attached|visible|hidden|detached>`) and `mimikyu batch` (`--ops` or `--file`) to execute multi-step action sequences in a single round trip.
- **Sandboxed Evaluation**: `mimikyu eval` executes in an isolated world by default, with explicit opt-in `--main-world`.

### Host & macOS Reliability Hardening
- **Zero-Spawn macOS Firefox Discovery**: Reads `application.ini` and `Contents/Info.plist` directly from disk rather than repeatedly spawning `firefox --version` (preventing macOS `RegisterFonts` exit races and desktop crash dialogs).
- **Crash-Reporter Suppression & Zero Orphans**: Launches Firefox with `MOZ_CRASHREPORTER_DISABLE=1`, `MOZ_CRASHREPORTER=0`, and `MOZ_CRASHREPORTER_NO_REPORT=1`, isolates every browser process in its own POSIX process group (`setpgid(0, 0)`), and guarantees teardown on shutdown, signal, or `Drop`.
- **Local Discovery Precedence**: Explicit override path → `MIMIKYU_CHROMIUM_PATH` / `MIMIKYU_FIREFOX_PATH` → Playwright & Puppeteer local caches → system installs & `PATH`.

---

## Build & Quickstart

### Prerequisites
- Rust toolchain `1.88+` (`cargo`, `rustc`)
- Local Chromium/Chrome and/or Firefox installation (Playwright/Puppeteer cache, system install, or `MIMIKYU_CHROMIUM_PATH` / `MIMIKYU_FIREFOX_PATH`)
- Python 3 (stdlib only, for running the `e2e/` suite)

### Build

```bash
cargo build --release
./target/release/mimikyu doctor
./target/release/mimikyu doctor --json
```

### CLI Examples

All commands support global flags:
`--json` (machine-readable `{"ok": true, "data": ...}` / `{"ok": false, "error": "..."}`), `--session <ID>` (alias `--context`, default `default`), `--engine <chromium|firefox>`, `--state-dir <DIR>`, and `--no-daemon`.

```bash
# Start a lightweight virtual session with a consistent preset and resource blocking
./target/release/mimikyu --session demo session new \
  --virtual --preset macos --locale en-US --timezone America/Chicago \
  --viewport 1280x720 --block-media --json

# Navigate, wait for idle, and capture an interactive @eN snapshot
./target/release/mimikyu --session demo open https://example.com
./target/release/mimikyu --session demo wait --idle
./target/release/mimikyu --session demo snapshot --interactive-only

# Interact via @eN refs or CSS selectors, extract markdown, and screenshot
./target/release/mimikyu --session demo click @e1 --humanize
./target/release/mimikyu --session demo extract markdown --json
./target/release/mimikyu --session demo screenshot ./page.png --full-page

# Execute an ordered batch of actions in one call
./target/release/mimikyu --session demo batch \
  --ops '[{"op":"fill","target":"@e2","value":"hello"},{"op":"click","target":"@e3"},{"op":"wait","idle":true}]'

# Close session
./target/release/mimikyu close demo
```

### Background Daemon

CLI commands auto-start the daemon for `--state-dir` (defaulting to the platform local data dir, e.g. `~/Library/Application Support/mimikyu` on macOS, with a 300s default idle timeout). You can also manage it explicitly:

```bash
./target/release/mimikyu daemon --idle-timeout 300   # start in background (or --foreground)
./target/release/mimikyu daemon status --json
./target/release/mimikyu daemon stop
```

### MCP Server Configuration

`mimikyu mcp` speaks JSON-RPC 2.0 over stdio (MCP protocol `2024-11-05`) and exposes **31 tools**:
- **11 `mimikyu_*` tools**: `mimikyu_session_new`, `mimikyu_session_close`, `mimikyu_open`, `mimikyu_snapshot`, `mimikyu_click`, `mimikyu_fill`, `mimikyu_extract`, `mimikyu_wait`, `mimikyu_batch`, `mimikyu_eval`, `mimikyu_screenshot`
- **20 `browser_*` tools**: `browser_open`, `browser_close`, `browser_snapshot`, `browser_click`, `browser_fill`, `browser_type`, `browser_press`, `browser_select`, `browser_upload`, `browser_scroll`, `browser_eval`, `browser_extract`, `browser_screenshot`, `browser_wait`, `browser_batch`, `browser_context_list`, `browser_context_close`, `browser_profile_export`, `browser_profile_import`, `browser_env_report`

Example MCP client entry:

```json
{
  "mcpServers": {
    "mimikyu": {
      "command": "/path/to/mimikyu/target/release/mimikyu",
      "args": ["mcp", "--engine", "chromium"]
    }
  }
}
```

### Reproducible Local Benchmark Suite

`mimikyu bench` runs entirely offline against the embedded `127.0.0.1:0` dual-origin harness, measuring cold launch, warm virtual context creation, navigation, snapshot, extraction, batch latency, live process-tree RSS, snapshot token compression ratio, and 5 end-to-end agentic workflows (multi-step wizard, form echo, catalog filter + diff + table extraction, dynamic DOM hydration + batch, and Shadow DOM + nested/cross-origin iframes):

```bash
./target/release/mimikyu bench --iterations 3 --warmup 1 --suite all
./target/release/mimikyu bench --engine firefox --suite all --json
```

#### Measured Release Benchmarks (`--iterations 3 --warmup 1 --suite all`)

| Metric (median) | Chromium (CDP) | Firefox (WebDriver BiDi) |
| --- | ---: | ---: |
| Cold ephemeral launch | `203.0 ms` | `1179.3 ms` |
| Warm virtual context | `67.1 ms` | `124.2 ms` |
| Navigation + idle | `276.9 ms` | `286.7 ms` |
| Semantic `@eN` snapshot | `5.0 ms` | `19.0 ms` |
| Markdown extraction | `5.4 ms` | `5.5 ms` |
| Atomic 3-step batch | `204.2 ms` | `203.5 ms` |
| Process-tree RSS | `278.3 MB` | `727.8 MB` |
| Snapshot token compression | `2.79x` (`967` → `347` est. tokens) | `3.57x` (`967` → `271` est. tokens) |
| Agentic workflows passed | **5 / 5 (100%)** | **5 / 5 (100%)** |

---

## Testing & Verification

### Rust Workspace Lint & Tests

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Verified workspace results:
- `cargo clippy --workspace --all-targets -- -D warnings`: **0 warnings**
- `cargo test --workspace`: **337 passed, 0 failed, 0 ignored** (`mimikyu-core`: 314/314 across unit & integration suites; `mimikyu-cli`: 23/23 across contract, doctor, and bench CLI suites)
- Process & crash hygiene: **0 orphan browser/mimikyu processes**, **0 macOS CrashReporter logs**

### Opaque-Box Python E2E Suite (`e2e/`)

Runs strictly against local `127.0.0.1:0` loopback servers and proxies (`e2e/local_server.py`) using Python 3 stdlib only:

```bash
# Harness self-check (no compiled binary required)
python3 e2e/run_e2e.py --self-test

# Full E2E suite (Tier 1: Features F1–F15, Tier 2: Boundaries/Negative, Tier 3: Cross-Feature, Tier 4: 7 Agentic Workloads)
python3 e2e/run_e2e.py --tier all --bin ./target/release/mimikyu

# Firefox WebDriver BiDi workload verification
python3 e2e/run_e2e.py --tier 4 --engine firefox --bin ./target/release/mimikyu
```

Verified E2E results against `./target/release/mimikyu` (**244/244 passed**, 0 failed, 0 errors, 0 skipped):
- `--self-test`: **29/29 passed**
- `--tier all` (Chromium, Tiers 1–4): **208/208 passed** (Tier 1: 86, Tier 2: 96, Tier 3: 19, Tier 4: 7)
- `--tier 4 --engine firefox`: **7/7 passed**
- Scope compliance & hygiene: **0 R3 violations**, **0 leaked processes**, **0 macOS CrashReporter logs**

---

## Scope & Compliance Boundary

`mimikyu` is strictly a general-purpose AI-agent browser automation, session/profile isolation, and environment consistency runtime. It intentionally **excludes** any human-verification challenge, CAPTCHA solver, proof-of-work circumvention, or anti-bot gate bypass functionality across both its runtime (`crates/`) and test suites (`e2e/`).

---

## License

Dual-licensed under `MIT OR Apache-2.0`.
