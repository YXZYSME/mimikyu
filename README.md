# mimikyu

High-speed Rust browser automation, session/profile isolation, and environment consistency control plane for AI agents over local Chromium (CDP) and Firefox (W3C WebDriver BiDi) runtimes.

## Workspace Layout

- `crates/mimikyu-core`: Core library (`discovery`, `process`, `driver::cdp`, `driver::bidi`, plus M2–M4 isolation, environment profile, snapshot, extraction, input, daemon, MCP, harness, and bench modules).
- `crates/mimikyu-cli`: Unified `mimikyu` CLI binary (`doctor`, and M4 daemon/MCP/bench frontends).

## Quick Check

```bash
cargo test --workspace
cargo run --bin mimikyu -- doctor --json
```
