# Security Policy

`mimikyu` is an AI-agent browser runtime, isolation layer, and CLI / Daemon / MCP control plane driving real local Chromium and Firefox processes. Because browser automation bridges local host credentials, network proxies, and untrusted web content, security boundaries are treated as first-class architectural invariants.

---

## Supported Versions

Only the latest minor release track receives security patches and advisories:

| Version | Supported | Notes |
| --- | :---: | --- |
| `0.1.x` | Yes | Active release line (Rust 2024 edition, MSRV `1.88`) |
| `< 0.1.0` | No | Pre-release / development snapshots |

---

## Reporting a Vulnerability (Private Coordinated Disclosure)

> [!CAUTION]
> **Do NOT open a public GitHub Issue, Discussion, or Pull Request for suspected security vulnerabilities.**

Please report security vulnerabilities privately through **GitHub Private Vulnerability Reporting**:

1. Navigate to the [`YXZYSME/mimikyu` Security tab](https://github.com/YXZYSME/mimikyu/security) on GitHub.
2. Click **Advisories** -> [**Report a vulnerability**](https://github.com/YXZYSME/mimikyu/security/advisories/new).
3. Include as much of the following detail as possible:
   - **Affected version & target** (`mimikyu --version` and `mimikyu doctor --json`, redacting any personal host paths).
   - **Affected subsystem** (e.g., `daemon/`, `env_profile/proxy_relay.rs`, `driver/cdp.rs`, `driver/bidi.rs`, `mcp/`, `isolation/`, `install.sh`).
   - **Minimal offline proof-of-concept** reproducible against `127.0.0.1` or the embedded local harness (`crates/mimikyu-core/src/harness/`).
   - **Impact & attack surface** (e.g., local privilege escalation across OS users, credential leakage, arbitrary command execution, sandbox escape from isolated `eval` world).

### Response SLA Targets

| Stage | Target SLA |
| --- | --- |
| Initial acknowledgment | Within **48 hours** |
| Triage, reproduction & severity rating (CVSS v3.1/v4.0) | Within **5 business days** |
| Patch & coordinated advisory (Critical / High) | Within **14 calendar days** |
| Patch & coordinated advisory (Medium / Low) | Within **30 calendar days** |

We practice coordinated disclosure and gladly credit reporters in GitHub Security Advisories and `CHANGELOG.md` (unless anonymity is requested).

---

## Mimikyu Security Model

`mimikyu` enforces five defense-in-depth boundaries across its core library (`crates/mimikyu-core`) and CLI/MCP surfaces (`crates/mimikyu-cli`).

### 1. Unix-Socket Daemon Access Control & State-Dir Isolation

- **No TCP control listener**: The warm background daemon (`crates/mimikyu-core/src/daemon/`) never binds a TCP port. All RPC traffic travels over a local Unix domain socket (`<state-dir>/daemon.sock`) using newline-delimited JSON (`NDJSON`) bounded to `MAX_LINE_BYTES` (`16 MiB`) per frame to prevent unbounded memory allocation.
- **Exclusive `fs2` locking**: Startup, active lifecycle, and shutdown coordinate through OS advisory file locks (`daemon.lock`, `daemon-active.lock`) and PID liveness checks before unlinking stale sockets.
- **`SUN_LEN` short-socket fallback**: When `<state-dir>/daemon.sock` approaches the POSIX `sockaddr_un.sun_path` limit (`>= 100` bytes), `resolve_socket_paths` binds a deterministic hashed socket path (`/tmp/mkd-<sha256-prefix>.sock`) symlinked from `<state-dir>/daemon.sock` and cleans up both files on graceful exit, `SIGTERM`/`SIGINT`, idle timeout, or `Drop`.
- **Multi-user host guidance**: The daemon trusts local OS processes running under the same user account that can connect to its Unix socket. On shared multi-tenant POSIX machines, always configure `--state-dir` inside a mode-`0700` user-owned directory with a short path (`< 80` bytes, such as `$XDG_RUNTIME_DIR/mimikyu`) so the socket remains strictly inside a `0700` directory.

### 2. Local `ProxyRelay` Loopback-Only Binding & Credential Redaction

- **Loopback-only listener (`127.0.0.1:0`)**: When a session specifies an authenticated upstream HTTP or SOCKS5 proxy (`--proxy http://user:pass@host:port` or `socks5://user:pass@host:port`), `ProxyConfig::needs_relay()` launches an in-process `ProxyRelay` (`crates/mimikyu-core/src/env_profile/proxy_relay.rs`) bound strictly to `127.0.0.1:0`. It is never exposed on `0.0.0.0`, `::`, or LAN interfaces.
- **Zero credential leakage to browser flags or profiles**: Browser child processes receive only the unauthenticated local relay endpoint (`http://127.0.0.1:<ephemeral-port>` via `browser_proxy_config()`). Upstream proxy usernames and passwords never appear in `ps` process arguments, Firefox `user.js` / `prefs.js`, DevTools protocol logs, or crash dumps.
- **Deterministic lifecycle**: `ProxyRelay` aborts its forwarding tasks immediately when the owning session drops.

### 3. Isolated-World JavaScript `eval` Default (`--main-world` Explicit Opt-In)

- **Isolated execution world by default**: Both `mimikyu eval <EXPR>` on the CLI and `mimikyu_eval` / `browser_eval` on the MCP server route to `DriverSession::eval_isolated` (`Page.createIsolatedWorld` on Chromium CDP; isolated sandbox realm on Firefox WebDriver BiDi).
- **Protection against page prototype tampering**: Evaluating inside an isolated world prevents untrusted page JavaScript from monkey-patching `Array.prototype`, `JSON.stringify`, or global bindings relied upon by agent extraction and inspection expressions, and keeps agent evaluation state invisible to `window` scripts.
- **Explicit `--main-world` opt-in**: Mutating or reading page-defined `window.*` globals requires passing `--main-world` (`"main_world": true` in MCP/JSON). Agents navigating untrusted pages should treat main-world return values as untrusted input.

### 4. Structured Process Spawning, Process-Group Containment & Storage Secrets

- **Zero shell interpolation**: Browser binaries and version probes (`crates/mimikyu-core/src/process.rs`, `discovery.rs`) are spawned exclusively via `Command::new(binary_path).args(...)` with `Stdio::null()` stdin—never via `sh -c`—eliminating command-injection vectors across URLs, profile names, and proxy strings.
- **POSIX process-group containment (`setpgid`)**: Every browser child process enters its own process group (`setpgid(0, 0)`) before `exec` and is torn down via protocol `close` -> `-pgid` `SIGTERM` -> `-pgid` `SIGKILL`, preventing zombie/orphan renderer or helper processes.
- **Profile locking & sensitive state exports**: Persistent profiles acquire an exclusive `fs2` `ProfileLock` to prevent cross-session corruption, while `--ephemeral` profiles self-delete on `Drop`. Exported storage state files (`session export`, `profile export`) contain plaintext session cookies and `localStorage` tokens—store them with `0600` permissions and never commit them to version control.
- **Prompt-injection boundary**: `mimikyu` isolates browser profiles and JS worlds, **not** the LLM reasoning loop consuming page text. When connecting `mimikyu mcp` to an autonomous agent browsing untrusted third-party sites, run inside an unprivileged user/container sandbox and restrict filesystem access outside the intended workspace.

### 5. Supply-Chain & Release Integrity

- **Dependency & license gate**: `deny.toml` + `.github/workflows/security.yml` enforce `crates.io`-only registry sources, forbid wildcard version specs and yanked crates, audit against `rustsec/advisory-db`, and enforce a strict permissive license allowlist.
- **Verified binary releases**: Official release tarballs publish `SHA256SUMS` and GitHub Artifact Attestations (`actions/attest-build-provenance`). `install.sh` verifies SHA-256 digests before extraction and supports `--verify-attestation`.

---

## Strict R3 Scope & Abuse Boundary (Out of Scope)

`mimikyu` is built strictly for legitimate AI-agent browser workflows, automated QA, reproducible benchmarking, and session/profile isolation.

Per **Project Requirement R3**, the following topics are **permanently out of scope** and **strictly prohibited** across code, tests, issues, discussions, and pull requests:

- CAPTCHA solvers or automated challenge-response services (e.g., reCAPTCHA, hCaptcha, Cloudflare Turnstile, Altcha, FunCaptcha).
- Human-verification ("verify you are human") or proof-of-work gate crackers/circumvention.
- Anti-bot evasion modules targeting specific third-party access controls, paywall bypasses, or credential-stuffing / account-takeover automation.

Issues or pull requests introducing, requesting, or benchmarking R3-prohibited functionality will be closed immediately without review.
