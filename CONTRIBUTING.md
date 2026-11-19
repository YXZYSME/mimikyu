# Contributing to Mimikyu

Welcome, traveler, to the **Haunted Rag Workshop**! Whether you are fixing a subtle WebDriver BiDi frame edge case, sharpening `@eN` accessibility snapshots, improving docs, or reporting a bug from the field, we are thrilled to have you here.

`mimikyu` moves fast while keeping `main` rock-solid by pairing **warm, ego-free collaboration** with **crisp architectural boundaries** (see [ARCHITECTURE.md](./ARCHITECTURE.md)). This guide shows you how to equip your dev gear, run the local test gauntlets, and open pull requests that breeze through review on the first pass.

---

## Before You Start: Scope & Community Ground Rules

1. **Be kind and constructive**: All interactions are governed by our [Code of Conduct](./CODE_OF_CONDUCT.md).
2. **Security vulnerabilities**: Never post exploitable security bugs in public issues—use Private Vulnerability Reporting as documented in [SECURITY.md](./SECURITY.md).
3. **Strict R3 Scope Boundary**: `mimikyu` is a legitimate AI-agent browser runtime and isolation toolkit. We **do not** accept issues, discussions, or PRs related to CAPTCHA solvers, human-verification ("verify you are human") bypasses, proof-of-work crackers, or anti-bot gate circumvention.
4. **Right-size your quest**: For bug fixes, docs, or small enhancements (1–2 modules), jump straight to a PR! For new CLI subcommands, new MCP tools, or changes to `DriverSession`, open a `[Feat]` Issue or Discussion first so we can align on the interface before you write hundreds of lines of code.

---

## 1. Equipping Your Dev Gear (Quick Local Setup)

### Prerequisites

- **Rust `1.88+`** (`edition = "2024"`) via [rustup](https://rustup.rs/):
  ```bash
  rustup update stable
  rustup component add clippy rustfmt
  ```
- **Local Browsers**: At least one local **Chromium / Google Chrome** and **Firefox** installation (system install, Playwright/Puppeteer cache, or explicit `MIMIKYU_CHROMIUM_PATH` / `MIMIKYU_FIREFOX_PATH` env vars).
- **Python 3 (`3.9+`)**: Standard library only (zero `pip` packages needed!) for running the opaque-box `e2e/` test suite.

### Clone, Build & Check Your Environment

```bash
git clone https://github.com/<owner>/mimikyu.git
cd mimikyu

# Build release binary and run browser discovery diagnostics
cargo build --release
./target/release/mimikyu doctor
./target/release/mimikyu doctor --json
```

If `mimikyu doctor` finds both Chromium and Firefox, your workstation is ready for full dual-engine testing!

---

## 2. Running the Verification Gauntlet

Every automated test in `mimikyu` runs **100% offline** against ephemeral `127.0.0.1:0` loopback servers (`crates/mimikyu-core/src/harness/` and `e2e/local_server.py`).

### Fast Inner Loop (While Coding)

```bash
# Check formatting and strict zero-warning Clippy lints
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings

# Run a single crate test binary while iterating on a module
cargo test -p mimikyu-core --lib
cargo test -p mimikyu-core --test snapshot_test
cargo test -p mimikyu-cli
```

### Full Pre-PR Verification Suite

Before marking a PR ready for review, run the three verification tiers:

```bash
# Tier 1: Workspace Rust unit, integration, and CLI contract tests
cargo test --workspace

# Tier 2: Build release binary & run E2E harness self-checks
cargo build --release
python3 e2e/run_e2e.py --self-test

# Tier 3: Full Chromium E2E (Tiers 1-4) + Firefox WebDriver BiDi E2E (Tier 4)
python3 e2e/run_e2e.py --tier all --bin ./target/release/mimikyu
python3 e2e/run_e2e.py --tier 4 --engine firefox --bin ./target/release/mimikyu
```

If your change touches performance-critical hot paths (`snapshot/`, `extract/`, `input/`, `isolation/virtual_ctx.rs`, `driver/`), also verify local benchmarks:

```bash
./target/release/mimikyu bench --iterations 3 --warmup 1 --suite all
```

---

## 3. Conventional Commits & Branch Naming

We use **Squash and Merge** on `main` with [Conventional Commits](https://www.conventionalcommits.org/) so `CHANGELOG.md` and SemVer releases stay crisp and auditable.

### Branch Naming

- `feat/<short-slug>` — new capability (e.g., `feat/snapshot-landmark-roles`)
- `fix/<short-slug>` — bug fix (e.g., `fix/bidi-iframe-scroll-bounds`)
- `docs/<short-slug>` — documentation or examples
- `test/<short-slug>` — unit or E2E test coverage
- `refactor/<short-slug>` / `perf/<short-slug>` / `ci/<short-slug>`

### Commit & PR Title Format

```text
<type>(<optional-scope>): <imperative summary in <= 72 chars>
```

Examples:
- `feat(extract): preserve colspan headers in markdown table export`
- `fix(bidi): sanitize lone UTF-16 surrogates in script evaluation frames`
- `perf(snapshot): preallocate AxNode arena for deep Shadow DOM trees`
- `docs(architecture): clarify ChokepointInput ACK timeout guarantees`
- `test(e2e): add cross-origin nested iframe fill regression check`

---

## 4. Five Rules for PRs That Breeze Through Review

Read [ARCHITECTURE.md](./ARCHITECTURE.md) for the full design rationale. In summary, reviewers check every PR against five invariants:

1. **Keep PR slices narrow (2–3 modules max)**:
   - Touch only the modules needed for one coherent vertical slice plus its tests. Don't mix sweeping formatting diffs or unrelated refactors into feature PRs.
2. **Respect `DriverSession` & `ChokepointInput` boundaries**:
   - Keep CDP (`driver/cdp.rs`) and WebDriver BiDi (`driver/bidi.rs`) wire details strictly inside `driver/`.
   - Route all pointer, keyboard, and wheel actions through `ChokepointInput` (`input/chokepoint.rs`)—never call raw `dispatch_*` methods directly from `session.rs`, `mcp/`, or `cli/`.
3. **Preserve Dual-Engine Parity**:
   - Verify behavior on **both** Chromium (CDP) and Firefox (WebDriver BiDi), or gate genuinely unsupported features cleanly via `DriverCapabilities` + `MimikyuError::Unsupported`.
4. **Zero Orphans, Zero Crash Popups, Zero Flaky Sleeps**:
   - Spawn processes exclusively through `ProcessGuard` (`setpgid(0, 0)`), preserve macOS zero-spawn Firefox plist/`application.ini` probing and `MOZ_CRASHREPORTER_DISABLE=1`, and use deterministic waits (`WaitEngine`) instead of arbitrary `sleep()` calls.
5. **Strict Hygiene & Supply-Chain Discipline**:
   - Never hardcode personal machine paths, usernames, or private LAN IPs in code, snapshots, or docs.
   - Avoid adding new crate dependencies unless truly necessary; any new dependency must pass `cargo deny check` against `deny.toml` (`crates.io` only, permissive license allowlist).

---

## Licensing of Contributions

`mimikyu` is dual-licensed under **MIT OR Apache-2.0** ([LICENSE-MIT](./LICENSE-MIT) and [LICENSE-APACHE](./LICENSE-APACHE)). Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this repository by you shall be dual-licensed as above, without any additional terms or conditions.
