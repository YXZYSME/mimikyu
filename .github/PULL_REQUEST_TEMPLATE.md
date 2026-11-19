## Summary

<!-- Briefly describe what this PR changes and why. Link any related issue (e.g., `Closes #123`). -->

## Modules Touched

<!-- Keep PRs narrow (max 2-3 modules per vertical slice; see ARCHITECTURE.md). List touched modules below: -->
- `crates/...`

## Architectural Boundary Check

- [ ] **Narrow vertical slice**: Touches at most 2–3 modules (plus corresponding tests/docs) with no unrelated refactors bundled in.
- [ ] **`DriverSession` boundary (`ARCHITECTURE.md` #1)**: Zero raw CDP or WebDriver BiDi wire types/methods leaked into `session.rs`, `snapshot/`, `input/`, `extract/`, `wait/`, `daemon/`, `mcp/`, or `mimikyu-cli`.
- [ ] **`ChokepointInput` boundary (`ARCHITECTURE.md` #2)**: All pointer, keyboard, and wheel dispatches flow through `ChokepointInput` with bounded ACK timeouts.
- [ ] **Process & macOS hygiene (`ARCHITECTURE.md` #3–#4)**: Preserves `ProcessGuard` (`setpgid(0, 0)`) zero-orphan teardown, macOS zero-spawn Firefox `application.ini` / `Info.plist` probing, and `MOZ_CRASHREPORTER_DISABLE=1`.

## Engine Parity Check (Chromium CDP & Firefox WebDriver BiDi)

- [ ] Verified on **Chromium (CDP)**
- [ ] Verified on **Firefox (W3C WebDriver BiDi)** *(or explicitly gated via `DriverCapabilities` / `MimikyuError::Unsupported` — explain below if N/A)*

## Security & Scope Boundary Check

- [ ] **Zero secrets / PII / host paths**: No API keys, credentials, private LAN IPs, or local user home paths committed.
- [ ] **Security invariants (`SECURITY.md`)**: Preserves isolated-world `eval` default (`--main-world` opt-in), `ProxyRelay` `127.0.0.1:0` loopback binding & credential redaction, and structured `Command::new` spawning.
- [ ] **R3 Scope Compliance**: Introduces zero CAPTCHA solvers, human-verification challenge bypasses, or proof-of-work circumvention code or tests.

## Test Evidence

<!-- Paste exact pass/fail counts from your local offline test run (`127.0.0.1:0` only): -->

- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] `python3 e2e/run_e2e.py --self-test`
- [ ] `python3 e2e/run_e2e.py --tier all --bin ./target/release/mimikyu`
- [ ] `python3 e2e/run_e2e.py --tier 4 --engine firefox --bin ./target/release/mimikyu`

```text
# Paste summary output here
```
