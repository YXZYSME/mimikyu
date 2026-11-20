# Mimikyu v0.3.0 (0.10 Program) — Product Triage & Parallel-Team Execution Index

This document is the repository-side index for the **Mimikyu `0.3.0`** product roadmap ([Milestone `0.3.0`](https://github.com/YXZYSME/mimikyu/milestone/1) · [Roadmap Tracker `#37`](https://github.com/YXZYSME/mimikyu/issues/37)), synthesized from real-world `mimikyu 0.2.0` production workloads and aligned with [`ARCHITECTURE.md`](../ARCHITECTURE.md), [`CONTRIBUTING.md`](../CONTRIBUTING.md), and [`SECURITY.md`](../SECURITY.md).

---

## 1. Production v0.2.0 Feedback Context & Mandatory Regression Guardrails

### Production Workload Signal (`v0.2.0`)

Roadmap priorities were distilled from an async Python 3.12 production orchestrator driving `mimikyu 0.2.0` (Homebrew, macOS ARM64) across:

- **Scale**: ~750 live SMB websites, ~1,000+ target records, and ~700+ rotated authenticated proxies across 3–6 concurrent workers.
- **Engine topology**: Chromium `--virtual --preset macos --block-media --block-fonts --block-trackers --instant-animations --proxy ...` primary pool with Firefox `--ephemeral --engine firefox` fallback.
- **Core loop**: `session new` -> `open` -> `wait --idle` -> `extract links` -> `eval` -> occasional `snapshot`/`click` -> `close`.

### Mandatory v0.2.0 Regression Guardrails

Every PR landing under [Milestone `0.3.0`](https://github.com/YXZYSME/mimikyu/milestone/1) **must** preserve the four strengths validated in production:

1. **Zero-friction install & `doctor --json` auto-discovery**: Preserve `discovery.rs` and `browser_install.rs` auto-discovery precedence and macOS zero-spawn Firefox metadata probing (`ARCHITECTURE.md` Invariant 4).
2. **Zero browser crashes, port conflicts, or orphan processes**: Preserve `ProcessGuard` POSIX process-group (`setpgid(0, 0)`) containment and deterministic teardown across 700+ concurrent session open/close cycles (`process.rs`, `ARCHITECTURE.md` Invariant 3).
3. **Loopback `ProxyRelay` (`127.0.0.1:0`) + `--virtual` context isolation with zero cross-talk**: Preserve strict per-context isolation across concurrent virtual contexts (`env_profile/proxy_relay.rs`, `isolation/virtual_ctx.rs`).
4. **Uniform `--json` envelope & dual-engine parity**: Preserve `{"ok": true, "data": ...}` / `{"ok": false, "error": "..."}` backwards compatibility and strict `DriverSession` Chromium CDP / Firefox WebDriver BiDi parity (`ARCHITECTURE.md` Invariant 1).

---

## 2. Locked Product Decisions (`0.3.0`)

The following design decisions are locked for implementation across this milestone:

- **Milestone & SemVer track**: **`0.3.0`** ([Milestone `0.3.0`](https://github.com/YXZYSME/mimikyu/milestone/1)), continuing strict SemVer from `Cargo.toml` `0.2.0` across `crates.io`, `npm`, `PyPI`, Homebrew, and `website/scripts/sync_site_data.mjs`.
- **Triage & Priority labels**: `triage` (`#fbca04`), `P0` (`#b60205`), `P1` (`#d93f0b`), `P2` (`#0e8a16`).
- **TypeScript SDK npm package home ([`#33`](https://github.com/YXZYSME/mimikyu/issues/33))**: Expose ESM/CJS library entrypoints (`"main"`, `"exports"`, `"types"`, and built `dist/` files with zero runtime dependencies) directly from existing `packaging/npm/saengil-mimikyu/package.json`, reusing the existing npm OIDC Trusted Publisher workflow in `.github/workflows/release.yml` and `scripts/check-version-sync.sh` (keeping `mimikyu-cli` and `mimikyu-mcp` lean bin-only launchers).
- **MCP asset-fetch namespace symmetry ([`#34`](https://github.com/YXZYSME/mimikyu/issues/34))**: Expose **both** `mimikyu_fetch` and `browser_fetch`, expanding the MCP server catalog from **`33` -> `35` tools** (`14` `mimikyu_*` + `21` `browser_*`), synchronized in lockstep across `crates/mimikyu-core/src/mcp/tools.rs`, `crates/mimikyu-core/tests/mcp_test.rs`, `crates/mimikyu-cli/tests/cli_contract_test.rs`, `website/src/data/mcp-tools.json`, derived site artifacts, and package/README tool counts.

---

## 3. Live GitHub Triage Matrix (`0.3.0`)

**Roadmap Tracker**: [`#37`](https://github.com/YXZYSME/mimikyu/issues/37) — `[Roadmap]: Mimikyu 0.10 — Production Ergonomics & High-Volume Extraction Tracking` (`enhancement`, `triage`, `P0`)

| Live Issue | Priority | Wave | Labels | Title | Narrow PR Slices (`ARCHITECTURE.md` Invariant 5) & Dependencies |
| :---: | :---: | :---: | --- | --- | --- |
| [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) | `P0` | Wave 1 | `enhancement`, `triage`, `P0` | `[Feat]: Add --wait-until, --timeout-ms, and partial-load readiness controls to mimikyu open` | **PR 1A** (`driver/{mod,cdp,bidi}.rs`) + **PR 1B** (`wait/{mod,idle}.rs`, `session.rs`, `daemon/{protocol,pool}.rs`) + **PR 1C** (`mcp/tools.rs`, `mimikyu-cli`, `mcp_test.rs`, `website/src/data/mcp-tools.json`); rebase on PR #28 |
| [`#30`](https://github.com/YXZYSME/mimikyu/issues/30) | `P0` | Wave 1 | `bug`, `triage`, `P0` | `[Bug]: Transient CDP -32000 "Cannot find context with specified id" race during mid-eval/extract redirects` | **Part A** (`driver/cdp.rs`, zero PR #28 overlap) + **Part B** (`driver/bidi.rs`, rebase on PR #28) |
| [`#36`](https://github.com/YXZYSME/mimikyu/issues/36) | `P2` | Wave 1 | `enhancement`, `triage`, `P2` | `[Feat]: Separate non-fatal skipped_candidates from actionable warnings in mimikyu doctor --json` | Single narrow PR (`discovery.rs`, `mimikyu-cli/src/output.rs`, `doctor_cli_test.rs`, `e2e/test_tier1_features.py`, `e2e/test_tier2_boundaries.py`); **independent, zero PR #28 overlap** |
| [`#31`](https://github.com/YXZYSME/mimikyu/issues/31) | `P1` | Wave 2 | `enhancement`, `triage`, `P1` | `[Feat]: Add structured machine-readable error taxonomy (error_code, category, retryable) across CLI, Daemon, Batch, and MCP` | Benefits from [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) BiDi error-flattening fix; **PR 3A** (`error.rs`, `daemon/protocol.rs`, `input/batch.rs`, zero PR #28 overlap) + **PR 3B** (CLI + `mcp/tools.rs` + `mcp_test.rs`, rebase on PR #28) |
| [`#32`](https://github.com/YXZYSME/mimikyu/issues/32) | `P1` | Wave 2 | `enhancement`, `triage`, `P1` | `[Feat]: Expand mimikyu batch for single-round-trip read/extract workflows` | Depends on [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) `Open` options; integrates with [`#31`](https://github.com/YXZYSME/mimikyu/issues/31) errors; **Slice 4A** (`input/batch.rs`, `session.rs` unification) + **Slice 4B** (daemon, CLI, `mcp/tools.rs`, `mcp-tools.json`); rebase on PR #28 |
| [`#33`](https://github.com/YXZYSME/mimikyu/issues/33) | `P1` | Wave 2 | `enhancement`, `documentation`, `triage`, `P1` | `[Feat]: Document warm Unix-socket NDJSON daemon protocol and ship thin Python/TypeScript SDKs` | Benefits from [`#31`](https://github.com/YXZYSME/mimikyu/issues/31) & [`#32`](https://github.com/YXZYSME/mimikyu/issues/32); **Slice 5A** (`protocol_version: 1` + `docs/DAEMON_PROTOCOL.md`) + **Slice 5B** (`packaging/pypi/src/mimikyu/`) + **Slice 5C** (`packaging/npm/saengil-mimikyu/` exports/types); rebase on PR #28 |
| [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) | `P1` | Wave 3 | `enhancement`, `triage`, `P1` | `[Feat]: Add native in-session asset fetch primitive (mimikyu fetch <url> --out <path>)` | Pre-implementation dual-engine spike -> **Slice 6A** (`driver/{mod,cdp}.rs`, `network/{blocker,intercept}.rs`) + **Slice 6B** (`driver/bidi.rs`) + **Slice 6C** (`session.rs`, daemon, CLI, `mimikyu_fetch` + `browser_fetch` 35-tool MCP & site sync); rebase on PR #28 |
| [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) | `P2` | Wave 3 | `enhancement`, `triage`, `P2` | `[Feat]: Support mid-session proxy rotation via live-swappable ProxyRelay` | Pre-implementation active `CONNECT` tunnel-flush spike -> **Slice 7A** (`env_profile/proxy_relay.rs`, zero PR #28 overlap) + **Slice 7B** (`driver/{mod,cdp,bidi}.rs`, `process.rs`, strict `is_root` dual-slot ownership) + **Slice 7C** (`session.rs`, daemon, CLI, optional MCP slice); rebase Slices 7B–7C on PR #28 |

---

## 4. Wave Execution Plan

- **Wave 1 — Reliability & Quick Win ([`#29`](https://github.com/YXZYSME/mimikyu/issues/29), [`#30`](https://github.com/YXZYSME/mimikyu/issues/30), [`#36`](https://github.com/YXZYSME/mimikyu/issues/36))**:
  - Eliminates ~88% of observed live navigation timeouts (`#29`), resolves transient mid-redirect execution-context errors (`#30`), and separates non-fatal skipped browser cache candidates from actionable `doctor --json` warnings (`#36`).
  - *Immediate zero-overlap branches*: `feat/doctor-skipped-candidates` (`#36`), `fix/cdp-context-race` (`#30` Part A).
  - *Post-PR #28 rebase branches*: `feat/open-wait-until-driver` (`#29` PR 1A), `feat/open-wait-until-core` (`#29` PR 1B), `feat/open-wait-until-surfaces` (`#29` PR 1C), `fix/bidi-context-race` (`#30` Part B).
- **Wave 2 — Machine Ergonomics & Low-Overhead Clients ([`#31`](https://github.com/YXZYSME/mimikyu/issues/31), [`#32`](https://github.com/YXZYSME/mimikyu/issues/32), [`#33`](https://github.com/YXZYSME/mimikyu/issues/33))**:
  - Adds structured error classification across CLI/Daemon/Batch/MCP (`#31`), collapses multi-step read/extract loops into a single `batch` round-trip (`#32`), and publishes the versioned Unix-socket NDJSON protocol spec alongside thin zero-dependency Python (`packaging/pypi/src/mimikyu/`) and TypeScript (`packaging/npm/saengil-mimikyu/`) SDKs (`#33`).
  - *Branches*: `feat/structured-errors-core` (`#31` PR 3A — zero PR #28 overlap), `feat/structured-errors-surfaces` (`#31` PR 3B), `feat/batch-read-extract` (`#32`), `feat/daemon-protocol-sdks` (`#33`).
- **Wave 3 — In-Session Asset Fetch & Live Proxy Rotation ([`#34`](https://github.com/YXZYSME/mimikyu/issues/34), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35))**:
  - Executes timeboxed dual-engine spikes before landing streaming in-session `mimikyu fetch` with `mimikyu_fetch` + `browser_fetch` (`#34`) and live-swappable `ProxyRelay` rotation (`#35`).
  - *Branches*: `feat/proxy-relay-core` (`#35` Slice 7A — zero PR #28 overlap), `feat/session-asset-fetch` (`#34`), `feat/proxy-relay-hot-swap` (`#35` Slices 7B–7C).

---

## 5. Parallel-Team Lane & PR #28 (`sec/wave1-hardening`) Coordination

All product feature work follows **`CONTRIBUTING.md` Section 5 (Parallel-Team Workflow Standard)**:

1. **Branch & Worktree Isolation**: Never commit or push directly to `main`. Develop every vertical PR slice in an isolated `git worktree` (`git worktree add ../mimikyu-<slug> -b <type>/<slug> origin/main`) and open an early Draft PR targeting `main`.
2. **Exclusive Lane & File Ownership**: Active **PR #28 (`sec/wave1-hardening`, `dc949be`, 30 files)** owns the parallel security and infrastructure hardening lane. Product feature branches touching any overlapping file below **must** wait for PR #28 to merge (or rebase cleanly onto `origin/main` post-merge) before requesting review.
3. **Derived Website Artifact Regeneration**: Never hand-merge generated site artifacts (`website/src/data/release.json`, `website/public/llms.txt`, `website/public/assets/mimikyu-banner.svg`, `website/public/og-card.png`). After rebasing and editing canonical sources (such as `website/src/data/mcp-tools.json`), regenerate derived files via `node website/scripts/sync_site_data.mjs` and `python3 website/scripts/generate_og.py`, then verify with `npm run build` inside `website/`.

### Four Zero-Overlap Fast-Track Slices (Safe While PR #28 Is Open)

These four vertical slices touch **zero** files modified by PR #28 (`dc949be`) and can proceed immediately in isolated worktrees off `origin/main`:

| Fast-Track Slice | Issue | Exclusive Files Owned |
| --- | :---: | --- |
| `feat/doctor-skipped-candidates` | [`#36`](https://github.com/YXZYSME/mimikyu/issues/36) | `crates/mimikyu-core/src/discovery.rs`, `crates/mimikyu-cli/src/output.rs`, `crates/mimikyu-cli/tests/doctor_cli_test.rs`, `e2e/test_tier1_features.py`, `e2e/test_tier2_boundaries.py` |
| `fix/cdp-context-race` (Part A) | [`#30`](https://github.com/YXZYSME/mimikyu/issues/30) | `crates/mimikyu-core/src/driver/cdp.rs` (+ dedicated tests) |
| `feat/structured-errors-core` (PR 3A) | [`#31`](https://github.com/YXZYSME/mimikyu/issues/31) | `crates/mimikyu-core/src/error.rs`, `crates/mimikyu-core/src/daemon/protocol.rs`, `crates/mimikyu-core/src/input/batch.rs` |
| `feat/proxy-relay-core` (Slice 7A) | [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) | `crates/mimikyu-core/src/env_profile/proxy_relay.rs` |

### PR #28 (`sec/wave1-hardening`) Shared-File Rebase Matrix

| PR #28 Overlapping File | Affected `0.3.0` Issues | Rebase & Preservation Requirement |
| --- | --- | --- |
| `crates/mimikyu-core/src/driver/mod.rs` | [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) (1A), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6A), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) (7B) | Rebase on PR #28; preserve `LaunchOptions.ignore_https_errors` default `false` |
| `crates/mimikyu-core/src/driver/bidi.rs` | [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) (1A), [`#30`](https://github.com/YXZYSME/mimikyu/issues/30) (2B), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6B), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) (7B) | Rebase on PR #28; preserve Firefox BiDi TLS verification defaults |
| `crates/mimikyu-core/src/session.rs` | [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) (1B), [`#32`](https://github.com/YXZYSME/mimikyu/issues/32) (4A), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6C), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) (7C) | Rebase on PR #28; preserve `ensure_not_paused()` and `resolve_path_if_confined` / `resolve_confined_path` guards |
| `crates/mimikyu-core/src/daemon/mod.rs` | [`#33`](https://github.com/YXZYSME/mimikyu/issues/33) (5A docs/SDK alignment) | Rebase on PR #28; match `0700`/`0600` socket permissions, `peer_cred` UID checks, and `/tmp/mimikyu-<uid>/mkd-<hex16>.sock` fallback |
| `crates/mimikyu-core/src/mcp/tools.rs` | [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) (1C), [`#31`](https://github.com/YXZYSME/mimikyu/issues/31) (3B), [`#32`](https://github.com/YXZYSME/mimikyu/issues/32) (4B), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6C), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) (opt. MCP) | Rebase on PR #28; preserve `confine_mcp_path`, HITL confirmation params, and `-32602` (`INVALID_PARAMS`) |
| `crates/mimikyu-core/tests/mcp_test.rs` | [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) (1C), [`#31`](https://github.com/YXZYSME/mimikyu/issues/31) (3B), [`#32`](https://github.com/YXZYSME/mimikyu/issues/32) (4B), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6C), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) (opt. MCP) | Rebase on PR #28; keep confinement, HITL, and tool-count assertions (`33` -> `35` in [`#34`](https://github.com/YXZYSME/mimikyu/issues/34)) green |
| `packaging/pypi/{pyproject.toml,README.md}` | [`#33`](https://github.com/YXZYSME/mimikyu/issues/33) (5B), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6C tool count) | Rebase on PR #28; sync `scripts/check-version-sync.sh` when adding `src/mimikyu/` |
| `packaging/npm/saengil-mimikyu/README.md` | [`#33`](https://github.com/YXZYSME/mimikyu/issues/33) (5C) | Rebase on PR #28; add library exports/types cleanly to `saengil-mimikyu` |
| `packaging/npm/mimikyu-mcp/{package.json,README.md}` | [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6C tool count) | Rebase on PR #28; update tool count (`35`) in lockstep |
| `README.md` | [`#33`](https://github.com/YXZYSME/mimikyu/issues/33), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) | Rebase on PR #28 |
| `website/src/data/mcp-tools.json` + derived outputs | [`#29`](https://github.com/YXZYSME/mimikyu/issues/29) (1C), [`#32`](https://github.com/YXZYSME/mimikyu/issues/32) (4B), [`#34`](https://github.com/YXZYSME/mimikyu/issues/34) (6C), [`#35`](https://github.com/YXZYSME/mimikyu/issues/35) (opt. MCP) | Rebase on PR #28; edit `mcp-tools.json`, regenerate via `sync_site_data.mjs` + `generate_og.py`, verify `npm run build` |

---

## 6. Verification & Scope Invariants

- **Three-Tier Offline Verification (`ARCHITECTURE.md` Invariant 5)**: Every implementation PR must pass `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`, `python3 e2e/run_e2e.py --self-test`, `python3 e2e/run_e2e.py --tier all --bin ./target/release/mimikyu`, and `python3 e2e/run_e2e.py --tier 4 --engine firefox --bin ./target/release/mimikyu` with 100% offline `127.0.0.1:0` fixtures and zero orphan processes.
- **Requirement R3 & Security Boundary**: All issues and PRs tracked in this index strictly comply with `SECURITY.md` Requirement R3. Suspected security issues are handled privately via GitHub Private Vulnerability Reporting rather than public roadmap issues.
