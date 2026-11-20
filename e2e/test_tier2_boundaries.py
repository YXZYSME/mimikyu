"""Tier 2 E2E boundary, corner, negative, and R3 compliance tests for mimikyu.

Strictly opaque-box against the public CLI (--json), Unix-socket daemon, and
stdio MCP JSON-RPC 2.0 interfaces. Python 3 stdlib + e2e.* helpers only.
"""

from __future__ import annotations

import atexit
import http.server
import json
import os
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.parse
from pathlib import Path
from typing import Any, Callable, Optional

REPO_ROOT: Path = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from e2e.helpers import (  # noqa: E402
    R3_FORBIDDEN_KEYWORDS,
    CommandResult,
    McpClient,
    McpError,
    MimikyuClient,
    assert_r3_compliant,
    check_r3_compliance,
    extract_all_ax_refs,
    extract_ax_ref,
    get_config,
)
from e2e.local_server import (  # noqa: E402
    SAMPLE_WOFF2_BYTES,
    LocalHttpProxy,
    LocalTestServer,
)

# ---------------------------------------------------------------------------
# Lazy Module-Level Local Cluster Management (Standalone + run_e2e.py Safe)
# ---------------------------------------------------------------------------

_CLUSTER_LOCK = threading.Lock()
_MANAGED_SERVERS: list[Any] = []
_SAVED_ENV: Optional[dict[str, Optional[str]]] = None
_ATEXIT_REGISTERED = False


def _teardown_cluster() -> None:
    global _SAVED_ENV
    with _CLUSTER_LOCK:
        servers = list(reversed(_MANAGED_SERVERS))
        _MANAGED_SERVERS.clear()
        saved = _SAVED_ENV
        _SAVED_ENV = None

    for srv in servers:
        try:
            srv.stop()
        except Exception:
            pass

    if saved is not None:
        for key, prev in saved.items():
            if prev is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = prev


def _ensure_cluster() -> tuple[str, str, str]:
    """Ensure local primary, cross-origin, and proxy servers exist without side effects at import time."""
    global _SAVED_ENV, _ATEXIT_REGISTERED
    cfg = get_config()
    if cfg.base_url and cfg.cross_url and cfg.proxy_url:
        return cfg.base_url, cfg.cross_url, cfg.proxy_url

    with _CLUSTER_LOCK:
        cfg = get_config()
        if cfg.base_url and cfg.cross_url and cfg.proxy_url:
            return cfg.base_url, cfg.cross_url, cfg.proxy_url

        if _SAVED_ENV is None:
            _SAVED_ENV = {
                "MIMIKYU_E2E_BASE_URL": os.environ.get("MIMIKYU_E2E_BASE_URL"),
                "MIMIKYU_E2E_CROSS_URL": os.environ.get("MIMIKYU_E2E_CROSS_URL"),
                "MIMIKYU_E2E_PROXY_URL": os.environ.get("MIMIKYU_E2E_PROXY_URL"),
            }

        base_url = cfg.base_url
        cross_url = cfg.cross_url
        proxy_url = cfg.proxy_url

        if not base_url:
            primary = LocalTestServer(name="t2-primary").start()
            _MANAGED_SERVERS.append(primary)
            base_url = primary.base_url
            os.environ["MIMIKYU_E2E_BASE_URL"] = base_url

        if not cross_url:
            cross = LocalTestServer(name="t2-cross").start()
            _MANAGED_SERVERS.append(cross)
            cross_url = cross.base_url
            os.environ["MIMIKYU_E2E_CROSS_URL"] = cross_url

        if not proxy_url:
            proxy = LocalHttpProxy(allow_external=False).start()
            _MANAGED_SERVERS.append(proxy)
            proxy_url = proxy.proxy_url
            os.environ["MIMIKYU_E2E_PROXY_URL"] = proxy_url

        if not _ATEXIT_REGISTERED:
            atexit.register(_teardown_cluster)
            _ATEXIT_REGISTERED = True

        return base_url, cross_url, proxy_url


def tearDownModule() -> None:
    _teardown_cluster()


# ---------------------------------------------------------------------------
# Opaque-Box Binary & Assertion Utilities
# ---------------------------------------------------------------------------


def _resolve_bin() -> str:
    """Resolve the mimikyu binary without silent skips."""
    cfg = get_config()
    candidates: list[Path] = []
    if cfg.bin_path:
        candidates.append(Path(cfg.bin_path))
    candidates.extend(
        [
            REPO_ROOT / "target" / "release" / "mimikyu",
            REPO_ROOT / "target" / "debug" / "mimikyu",
        ]
    )
    which_path = shutil.which("mimikyu")
    if which_path:
        candidates.append(Path(which_path))

    for candidate in candidates:
        if candidate.is_file() and os.access(candidate, os.X_OK):
            return str(candidate)

    raise AssertionError(
        "Executable mimikyu binary not found. Pass --bin <path> to e2e/run_e2e.py "
        "or set MIMIKYU_BIN."
    )


def _allocate_closed_loopback_port() -> int:
    """Bind an ephemeral 127.0.0.1 socket and close it immediately for ECONNREFUSED tests."""
    sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])
    finally:
        sock.close()


def _kill_pgid_safely(proc: subprocess.Popen[Any], pgid: Optional[int] = None) -> None:
    pid = proc.pid
    if pgid is None and pid is not None:
        try:
            pgid = os.getpgid(pid)
        except OSError:
            pgid = pid
    if pgid is not None:
        try:
            os.killpg(pgid, signal.SIGTERM)
        except OSError:
            pass
    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        if pgid is not None:
            try:
                os.killpg(pgid, signal.SIGKILL)
            except OSError:
                pass
        try:
            proc.wait(timeout=2.0)
        except subprocess.TimeoutExpired:
            pass


def _make_tmp_dir(prefix: str = "mk2_") -> Path:
    return Path(tempfile.mkdtemp(dir="/tmp", prefix=prefix))


def _data_url(html: str) -> str:
    return "data:text/html;charset=utf-8," + urllib.parse.quote(html, safe="")


_EVAL_ENVELOPE_KEYS: frozenset[str] = frozenset(
    {"value", "result", "text", "content", "type", "world", "main_world", "duration_ms"}
)


def _maybe_parse_json_container(val: Any) -> Any:
    """Parse serialized JSON objects/arrays while preserving scalar strings like 'null'."""
    if isinstance(val, str) and val.lstrip().startswith(("{", "[")):
        try:
            return json.loads(val)
        except json.JSONDecodeError:
            return val
    return val


def _extract_json_value(res: CommandResult) -> Any:
    """Flexibly extract structured JSON returned by extract/eval CLI commands."""
    if isinstance(res.data, dict):
        if len(res.data) == 1 or set(res.data.keys()) <= _EVAL_ENVELOPE_KEYS:
            for key in ("value", "result", "text", "content"):
                if key in res.data:
                    return _maybe_parse_json_container(res.data[key])
        return res.data
    if isinstance(res.data, (list, int, float, bool)):
        return res.data
    if isinstance(res.data, str):
        return _maybe_parse_json_container(res.data)
    if isinstance(res.payload, dict) and "value" in res.payload:
        return _maybe_parse_json_container(res.payload["value"])
    return res.data


def _is_engine_available_in_doctor(doc_data: Any, engine_name: str) -> bool:
    """Recursively locate the specific engine entry in `doctor` JSON and verify availability."""
    target = engine_name.lower()

    def _entry_available(entry: Any) -> bool:
        if isinstance(entry, bool):
            return entry
        if isinstance(entry, dict):
            if "available" in entry:
                return entry.get("available") is True
            return bool(entry.get("path"))
        return False

    def _walk(node: Any) -> Optional[bool]:
        if isinstance(node, dict):
            for id_key in ("name", "engine", "kind", "browser", "id"):
                val = node.get(id_key)
                if isinstance(val, str) and val.lower() == target:
                    if "available" in node or "path" in node:
                        return _entry_available(node)
            for key, val in node.items():
                if isinstance(key, str) and key.lower() == target:
                    if isinstance(val, (dict, bool)):
                        return _entry_available(val)
            for key, val in node.items():
                if isinstance(key, str) and key.lower() in (
                    "default",
                    "default_engine",
                    "primary",
                    "selected",
                    "skipped_candidates",
                ):
                    continue
                found = _walk(val)
                if found is not None:
                    return found
        elif isinstance(node, list):
            for item in node:
                found = _walk(item)
                if found is not None:
                    return found
        return None

    return _walk(doc_data) is True


def _lingering_state_dir_procs(
    state_dir: Path, *, exclude_daemon_pid: Optional[int] = None
) -> list[str]:
    """Return `ps` lines referencing `state_dir`, optionally excluding the daemon process."""
    ps = subprocess.run(["ps", "-eo", "pid,command"], capture_output=True, text=True, check=True)
    marker = str(state_dir)
    my_pid = os.getpid()
    lingering: list[str] = []
    for line in ps.stdout.splitlines():
        if marker not in line:
            continue
        stripped = line.strip()
        parts = stripped.split(None, 1)
        if not parts or not parts[0].isdigit():
            continue
        pid = int(parts[0])
        cmd = parts[1] if len(parts) > 1 else ""
        if pid == my_pid:
            continue
        if exclude_daemon_pid is not None:
            cmd_tokens = cmd.split()
            if pid == exclude_daemon_pid or "daemon" in cmd_tokens:
                continue
        lingering.append(stripped)
    return lingering


def _extract_text_blob(res: CommandResult) -> str:
    if isinstance(res.data, str):
        return res.data
    if isinstance(res.data, dict):
        for key in ("text", "markdown", "content", "value", "tree", "snapshot", "diff"):
            if key in res.data and isinstance(res.data[key], str):
                return res.data[key]
        return json.dumps(res.data)
    if res.data is not None:
        return json.dumps(res.data)
    return res.stdout


class _EphemeralLoopbackServer(http.server.ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def _start_custom_http_server(
    handler_cls: type[http.server.BaseHTTPRequestHandler],
) -> tuple[_EphemeralLoopbackServer, str, list[str], threading.Lock]:
    log: list[str] = []
    lock = threading.Lock()
    srv = _EphemeralLoopbackServer(("127.0.0.1", 0), handler_cls)
    srv.request_paths = log  # type: ignore[attr-defined]
    srv.request_lock = lock  # type: ignore[attr-defined]
    port = int(srv.server_address[1])
    thread = threading.Thread(target=srv.serve_forever, daemon=True)
    thread.start()
    srv._thread = thread  # type: ignore[attr-defined]
    return srv, f"http://127.0.0.1:{port}", log, lock


def _stop_custom_http_server(srv: _EphemeralLoopbackServer) -> None:
    try:
        srv.shutdown()
    except Exception:
        pass
    try:
        srv.server_close()
    except Exception:
        pass
    t = getattr(srv, "_thread", None)
    if t is not None:
        t.join(timeout=2.0)


class _Tier2TestCase(unittest.TestCase):
    """Shared base class with opaque-box assertions and automatic cluster/binary setup."""

    def _bin(self) -> str:
        return _resolve_bin()

    def _urls(self) -> tuple[str, str, str]:
        return _ensure_cluster()

    def _client(
        self,
        *,
        engine: Optional[str] = None,
        timeout: float = 30.0,
        session: Optional[str] = None,
        state_dir: Optional[Path] = None,
    ) -> MimikyuClient:
        cfg = get_config()
        return MimikyuClient(
            bin_path=self._bin(),
            state_dir=state_dir,
            engine=engine if engine is not None else cfg.engine,
            timeout=timeout,
            session=session,
        )

    def _assert_cli_ok(self, res: CommandResult, *, require_json: bool = True) -> None:
        self.assertNotEqual(res.returncode, -124, msg=f"Command timed out: {res.stderr}")
        combined = f"{res.stdout}\n{res.stderr}".lower()
        self.assertNotIn("panicked at", combined, msg=f"Rust panic observed: {combined[:400]}")
        self.assertTrue(
            res.ok,
            msg=f"Expected success, got rc={res.returncode}, err={res.error}, stdout={res.stdout[:300]}",
        )
        if require_json:
            self.assertIsInstance(
                res.payload,
                dict,
                msg=f"Expected JSON dict envelope, got stdout={res.stdout[:300]}",
            )
            self.assertIs(res.payload.get("ok"), True)

    def _assert_cli_failed(
        self,
        res: CommandResult,
        *,
        allow_timeout: bool = False,
    ) -> None:
        self.assertFalse(
            res.ok,
            msg=f"Expected command failure, got rc={res.returncode}, payload={res.payload}",
        )
        if not allow_timeout:
            self.assertNotEqual(
                res.returncode,
                -124,
                msg=f"Command hung until outer timeout (-124) instead of failing cleanly: {res.stderr}",
            )
        combined = f"{res.stdout}\n{res.stderr}".lower()
        self.assertNotIn("panicked at", combined, msg=f"Rust panic observed: {combined[:400]}")
        has_json_err = (
            isinstance(res.payload, dict)
            and res.payload.get("ok") is False
            and bool(str(res.payload.get("error") or "").strip())
        )
        has_exit_err = res.returncode != 0 and bool(res.error)
        self.assertTrue(
            has_json_err or has_exit_err,
            msg=(
                f"Expected structured JSON error or non-zero exit with diagnostic error; "
                f"got rc={res.returncode}, payload={res.payload}, stderr={res.stderr[:250]}"
            ),
        )

    def _assert_timeout_rejected(self, res: CommandResult) -> None:
        self.assertNotEqual(
            res.returncode,
            -124,
            msg="Wait command exceeded outer subprocess timeout (-124) instead of honoring --timeout-ms",
        )
        self._assert_cli_failed(res, allow_timeout=False)

    def _assert_mcp_failed(
        self,
        mcp: McpClient,
        fn: Callable[..., Any],
        *args: Any,
        **kwargs: Any,
    ) -> None:
        failed = False
        try:
            res = fn(*args, **kwargs)
        except McpError as exc:
            msg = str(exc).lower()
            self.assertNotIn("timed out", msg, msg=f"MCP server timed out: {exc}")
            self.assertNotIn("closed stdout unexpectedly", msg, msg=f"MCP server crashed: {exc}")
            failed = True
        else:
            if isinstance(res, dict):
                if res.get("isError") is True:
                    failed = True
                else:
                    content = res.get("content")
                    if isinstance(content, list):
                        for item in content:
                            if isinstance(item, dict) and isinstance(item.get("text"), str):
                                try:
                                    parsed = json.loads(item["text"])
                                    if isinstance(parsed, dict) and parsed.get("ok") is False:
                                        failed = True
                                except json.JSONDecodeError:
                                    pass
        self.assertTrue(failed, msg="Expected MCP call to return JSON-RPC error or isError=True")
        tools = mcp.tools_list(timeout=5.0)
        self.assertIsInstance(tools, list)

    def _read_env_report(self, client: MimikyuClient, base_url: str, *, session: Optional[str] = None) -> dict[str, Any]:
        self._assert_cli_ok(client.open(f"{base_url}/echo-env", session=session))
        self._assert_cli_ok(
            client.wait(selector='#env-json[data-ready="true"]', timeout_ms=5000, session=session)
        )
        res = client.eval("JSON.stringify(window.__ENV_REPORT)", main_world=True, session=session)
        self._assert_cli_ok(res)
        report = _extract_json_value(res)
        self.assertIsInstance(report, dict, msg=f"Unexpected __ENV_REPORT payload: {res.payload}")
        return report

    def _read_state_report(self, client: MimikyuClient, url: str, *, session: Optional[str] = None) -> dict[str, Any]:
        self._assert_cli_ok(client.open(url, session=session))
        self._assert_cli_ok(
            client.wait(selector='#state-json[data-ready="true"]', timeout_ms=5000, session=session)
        )
        res = client.eval("JSON.stringify(window.__STATE_REPORT)", main_world=True, session=session)
        self._assert_cli_ok(res)
        report = _extract_json_value(res)
        self.assertIsInstance(report, dict, msg=f"Unexpected __STATE_REPORT payload: {res.payload}")
        return report


# ===========================================================================
# F1: Browser Auto-Discovery & Doctor Boundaries (6 tests)
# ===========================================================================


class TestF1DiscoveryDoctorBoundaries(_Tier2TestCase):
    def test_f1_doctor_json_schema_and_no_session_side_effects(self) -> None:
        with self._client() as client:
            res = client.doctor(json_output=True)
            self._assert_cli_ok(res, require_json=True)
            self.assertIsInstance(res.data, dict)
            self.assertGreater(len(res.data), 0)
            listed = client.session_list()
            self._assert_cli_ok(listed, require_json=True)
            sessions = (
                listed.data.get("sessions")
                if isinstance(listed.data, dict) and "sessions" in listed.data
                else listed.data
            )
            self.assertEqual(sessions or [], [])

            # Verify _is_engine_available_in_doctor ignores skipped_candidates regardless of dict key order.
            skipped_only = {
                "skipped_candidates": [
                    {
                        "engine": "chromium",
                        "source": "puppeteer_cache",
                        "path": "/nonexistent/broken/chrome",
                        "available": False,
                        "reason": "probe failed",
                    }
                ],
                "chromium": [],
                "firefox": [],
            }
            self.assertFalse(_is_engine_available_in_doctor(skipped_only, "chromium"))

            skipped_before_healthy = {
                "skipped_candidates": skipped_only["skipped_candidates"],
                "chromium": [
                    {
                        "engine": "chromium",
                        "path": "/usr/bin/chromium",
                    }
                ],
            }
            self.assertTrue(_is_engine_available_in_doctor(skipped_before_healthy, "chromium"))

    def test_f1_doctor_non_json_human_readable_output(self) -> None:
        with self._client() as client:
            res = client.doctor(json_output=False)
            self._assert_cli_ok(res, require_json=False)
            self.assertIsNone(res.payload)
            self.assertGreater(len(res.stdout.strip()), 0)
            low = res.stdout.lower()
            self.assertTrue("chromium" in low or "firefox" in low or "browser" in low)

    def test_f1_doctor_rejects_unknown_flags(self) -> None:
        with self._client() as client:
            res = client.run(
                "doctor",
                "--nonexistent-doctor-flag",
                include_engine=False,
                include_session=False,
                check=False,
            )
            self._assert_cli_failed(res)

    def test_f1_doctor_handles_fresh_and_stale_state_dir(self) -> None:
        tmp_dir = _make_tmp_dir("mk2_doc_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        (tmp_dir / "daemon.sock").write_text("stale_not_a_socket", encoding="utf-8")
        (tmp_dir / "daemon.pid").write_text("99999999", encoding="utf-8")
        with self._client(state_dir=tmp_dir) as client:
            res = client.doctor(json_output=True)
            self._assert_cli_ok(res, require_json=True)
            self.assertIsInstance(res.data, dict)

    def test_f1_doctor_with_poisoned_path_env_no_hang_or_panic(self) -> None:
        with self._client(timeout=10.0) as client:
            res = client.run(
                "doctor",
                include_engine=False,
                include_session=False,
                env_overrides={"PATH": "/nonexistent_dir_e2e"},
                check=False,
            )
            self.assertNotEqual(res.returncode, -124)
            self.assertNotIn("panicked at", (res.stdout + res.stderr).lower())
            self.assertIsInstance(res.payload, dict)

    def test_f1_doctor_idempotent_across_repeated_calls(self) -> None:
        with self._client() as client:
            results = []
            for _ in range(3):
                r = client.doctor(json_output=True)
                self._assert_cli_ok(r, require_json=True)
                data = dict(r.data)
                for volatile in ("timestamp", "duration_ms", "checked_at"):
                    data.pop(volatile, None)
                results.append(data)
            self.assertEqual(results[0], results[1])
            self.assertEqual(results[1], results[2])


# ===========================================================================
# F2: Debloated Launch & Process Lifecycle Boundaries (6 tests)
# ===========================================================================


class TestF2LaunchLifecycleBoundaries(_Tier2TestCase):
    def test_f2_session_close_nonexistent_id_rejected(self) -> None:
        with self._client() as client:
            res = client.session_close("nonexistent-session-xyz-e2e", check=False)
            self._assert_cli_failed(res)

    def test_f2_session_double_close_safe_and_removes_from_list(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="s_dup"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="s_dup"))
            self._assert_cli_ok(client.session_close("s_dup"))
            res2 = client.session_close("s_dup", check=False)
            self._assert_cli_failed(res2)
            listed = client.session_list()
            self._assert_cli_ok(listed)
            self.assertNotIn("s_dup", json.dumps(listed.data))

    def test_f2_commands_on_closed_session_rejected_cleanly(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="s_closed"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="s_closed"))
            self._assert_cli_ok(client.session_close("s_closed"))
            res_open = client.open(f"{base_url}/nav/page2", session="s_closed", check=False)
            self._assert_cli_failed(res_open)
            res_snap = client.snapshot(session="s_closed", check=False)
            self._assert_cli_failed(res_snap)

    def test_f2_invalid_state_dir_regular_file_rejected(self) -> None:
        tmp_dir = _make_tmp_dir("mk2_file_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        bad_file = tmp_dir / "not_a_dir"
        bad_file.write_text("regular file", encoding="utf-8")
        with self._client() as client:
            res = client.run(
                "session",
                "new",
                "--ephemeral",
                "--state-dir",
                str(bad_file),
                include_state_dir=False,
                check=False,
            )
            self._assert_cli_failed(res)

    def test_f2_zero_orphan_pids_after_session_close(self) -> None:
        base_url, _, _ = self._urls()
        state_dir = _make_tmp_dir("mk2_ps_")
        self.addCleanup(shutil.rmtree, state_dir, True)
        proc = subprocess.Popen(
            [
                self._bin(),
                "--state-dir",
                str(state_dir),
                "daemon",
                "--foreground",
                "--idle-timeout",
                "1",
            ],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        if proc.stderr is not None:
            self.addCleanup(proc.stderr.close)
        self.addCleanup(_kill_pgid_safely, proc, proc.pid)

        deadline = time.monotonic() + 3.0
        sock_ready = False
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                err_out = proc.stderr.read() if proc.stderr is not None else ""
                self.fail(f"Foreground daemon exited prematurely rc={proc.returncode}: {err_out}")
            try:
                if any(p.is_socket() for p in state_dir.rglob("*")):
                    sock_ready = True
                    break
            except OSError:
                pass
            time.sleep(0.02)
        self.assertTrue(
            sock_ready,
            msg="Foreground daemon did not create Unix socket in state_dir within 3.0s",
        )

        with self._client(state_dir=state_dir) as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="ps_check"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="ps_check"))
            self._assert_cli_ok(client.session_close("ps_check"))
            lingering_children = _lingering_state_dir_procs(state_dir, exclude_daemon_pid=proc.pid)
            self.assertEqual(
                lingering_children,
                [],
                msg=f"Lingering browser/child processes after session_close: {lingering_children}",
            )

        try:
            rc = proc.wait(timeout=4.0)
        except subprocess.TimeoutExpired as exc:
            self.fail(f"Foreground daemon did not exit within idle-timeout window: {exc}")
        err_out = proc.stderr.read() if proc.stderr is not None else ""
        self.assertEqual(rc, 0, msg=f"Daemon exited with non-zero code {rc}: {err_out}")
        lingering_all = _lingering_state_dir_procs(state_dir)
        self.assertEqual(lingering_all, [], msg=f"Lingering processes found: {lingering_all}")

    def test_f2_back_on_unstarted_session_rejected(self) -> None:
        with self._client() as client:
            res = client.back(session="never-created-session", check=False)
            self._assert_cli_failed(res)


# ===========================================================================
# F3: Dual-Protocol Driver Boundaries (6 tests)
# ===========================================================================


class TestF3DualProtocolDriverBoundaries(_Tier2TestCase):
    def test_f3_invalid_engine_name_rejected_cleanly(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            res1 = client.session_new(engine="webkit_unknown_e2e", ephemeral=True, check=False)
            self._assert_cli_failed(res1)
            res2 = client.run("open", base_url, engine="invalid_engine_e2e", check=False)
            self._assert_cli_failed(res2)

    def test_f3_unreachable_closed_loopback_port_error_and_recovery(self) -> None:
        base_url, _, _ = self._urls()
        closed_port = _allocate_closed_loopback_port()
        with self._client(timeout=15.0) as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="conn_ref"))
            res = client.open(f"http://127.0.0.1:{closed_port}/", session="conn_ref", check=False)
            self._assert_cli_failed(res)
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="conn_ref"))

    def test_f3_malformed_url_and_unsupported_scheme_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="bad_url"))
            for bad in ("not a valid url :::", "http://127.0.0.1:99999/", "ftp://127.0.0.1/file"):
                with self.subTest(url=bad):
                    res = client.open(bad, session="bad_url", check=False)
                    self._assert_cli_failed(res)
            self._assert_cli_ok(client.open(f"{base_url}/health", session="bad_url"))

    def test_f3_http_500_and_redirect_loop_handled_without_wedging_session(self) -> None:
        base_url, _, _ = self._urls()

        class _ErrLoopHandler(http.server.BaseHTTPRequestHandler):
            def log_message(self, fmt: str, *args: Any) -> None:
                return

            def do_GET(self) -> None:
                if self.path.startswith("/err500"):
                    body = b"<!doctype html><html><body><h1>Server Boom 500</h1></body></html>"
                    self.send_response(500)
                    self.send_header("Content-Type", "text/html; charset=utf-8")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                elif self.path.startswith("/loop"):
                    self.send_response(302)
                    self.send_header("Location", "/loop")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                else:
                    self.send_response(404)
                    self.end_headers()

        srv, custom_url, _, _ = _start_custom_http_server(_ErrLoopHandler)
        self.addCleanup(_stop_custom_http_server, srv)
        with self._client(timeout=20.0) as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="err_loop"))
            r500 = client.open(f"{custom_url}/err500", session="err_loop", check=False)
            self.assertNotEqual(r500.returncode, -124)
            self.assertNotIn("panicked at", (r500.stdout + r500.stderr).lower())
            r_loop = client.open(f"{custom_url}/loop", session="err_loop", check=False)
            self._assert_cli_failed(r_loop)
            self._assert_cli_ok(client.open(f"{base_url}/health", session="err_loop"))

    def test_f3_http_404_and_redirect_chain_max_hops(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="r404"))
            r404 = client.open(f"{base_url}/nonexistent-route-404", session="r404", check=False)
            self.assertNotEqual(r404.returncode, -124)
            self.assertNotIn("panicked at", (r404.stdout + r404.stderr).lower())
            txt = client.extract("text", session="r404")
            self._assert_cli_ok(txt)
            self.assertIn("not_found", _extract_text_blob(txt))

            self._assert_cli_ok(client.open(f"{base_url}/redirect-chain?hops=10", session="r404"))
            self._assert_cli_ok(client.wait(selector="#chain-end", timeout_ms=5000, session="r404"))
            chain_txt = client.extract("text", session="r404")
            self._assert_cli_ok(chain_txt)
            self.assertIn("Redirect Chain Complete", _extract_text_blob(chain_txt))

    def test_f3_history_boundaries_and_dual_engine_parity(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="hist"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="hist"))
            r_back = client.back(session="hist", check=False)
            self.assertNotEqual(r_back.returncode, -124)
            self.assertNotIn("panicked at", (r_back.stdout + r_back.stderr).lower())
            self._assert_cli_ok(client.open(f"{base_url}/nav/page2", session="hist"))
            r_fwd = client.forward(session="hist", check=False)
            self.assertNotEqual(r_fwd.returncode, -124)
            self._assert_cli_ok(client.back(session="hist"))
            self._assert_cli_ok(client.forward(session="hist"))
            self._assert_cli_ok(client.reload(session="hist"))
            title_res = client.eval("document.getElementById('page-title').textContent", session="hist")
            self._assert_cli_ok(title_res)
            self.assertIn("Page 2", str(_extract_json_value(title_res)))

            doc = client.doctor(json_output=True)
            self._assert_cli_ok(doc)
            secondary = "firefox" if client.engine == "chromium" else "chromium"
            if _is_engine_available_in_doctor(doc.data, secondary):
                sec_client = MimikyuClient(
                    bin_path=self._bin(),
                    state_dir=client.state_dir,
                    engine=secondary,
                )
                self._assert_cli_ok(
                    sec_client.session_new(ephemeral=True, session="parity_sec")
                )
                self._assert_cli_ok(sec_client.open(f"{base_url}/nav/page1", session="parity_sec"))
                snap = sec_client.snapshot(interactive_only=True, session="parity_sec")
                self._assert_cli_ok(snap)
                ref = extract_ax_ref(snap, role="link", name_substr="Page 2")
                self.assertIsNotNone(ref)
                self._assert_cli_ok(sec_client.click(ref or "#to-page2", session="parity_sec"))


# ===========================================================================
# F4: Persistent & Ephemeral Disk Profile Boundaries (6 tests)
# ===========================================================================


class TestF4DiskProfileBoundaries(_Tier2TestCase):
    def test_f4_ephemeral_isolation_and_profile_ephemeral_conflict(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            r_conf = client.session_new(
                profile="conflict_prof", ephemeral=True, session="conf_s", check=False
            )
            self.assertNotEqual(r_conf.returncode, -124)
            self.assertNotIn("panicked at", (r_conf.stdout + r_conf.stderr).lower())
            if r_conf.ok:
                self._assert_cli_ok(client.session_close("conf_s"))

            self._assert_cli_ok(client.session_new(ephemeral=True, session="ep1"))
            self._read_state_report(
                client,
                f"{base_url}/state?set_cookie=ephem_val&httponly=ephem_ho&set_storage=ephem_ls",
                session="ep1",
            )
            self._assert_cli_ok(client.session_close("ep1"))

            self._assert_cli_ok(client.session_new(ephemeral=True, session="ep2"))
            rep2 = self._read_state_report(client, f"{base_url}/state", session="ep2")
            self.assertNotEqual(rep2.get("serverCookies", {}).get("mimikyu_normal"), "ephem_val")
            self.assertEqual(rep2.get("localStorage", {}), {})

    def test_f4_persistent_profile_persists_cookies_httponly_and_localstorage(self) -> None:
        base_url, _, _ = self._urls()
        prof_dir = _make_tmp_dir("mk2_prof_")
        self.addCleanup(shutil.rmtree, prof_dir, True)
        with self._client() as c1:
            self._assert_cli_ok(c1.session_new(profile=str(prof_dir), session="p1"))
            self._read_state_report(
                c1,
                f"{base_url}/state?set_cookie=tok42&httponly=ho42&set_storage=ls42",
                session="p1",
            )
            self._assert_cli_ok(c1.session_close("p1"))

        with self._client() as c2:
            self._assert_cli_ok(c2.session_new(profile=str(prof_dir), session="p2"))
            rep = self._read_state_report(c2, f"{base_url}/state", session="p2")
            self.assertEqual(rep.get("serverCookies", {}).get("mimikyu_normal"), "tok42")
            self.assertEqual(rep.get("serverCookies", {}).get("mimikyu_httponly"), "ho42")
            self.assertEqual(rep.get("localStorage", {}).get("token"), "ls42")

    def test_f4_flock_contention_across_two_daemons_and_release_after_close(self) -> None:
        prof_dir = _make_tmp_dir("mk2_flock_")
        self.addCleanup(shutil.rmtree, prof_dir, True)
        with self._client(timeout=20.0) as c1:
            self._assert_cli_ok(c1.session_new(profile=str(prof_dir), session="lock_s1"))
            with self._client(timeout=10.0) as c2:
                r_contend = c2.session_new(
                    profile=str(prof_dir), session="lock_s2", timeout=8.0, check=False
                )
                self._assert_cli_failed(r_contend)
            self._assert_cli_ok(c1.session_close("lock_s1"))

        with self._client(timeout=20.0) as c3:
            r_reacquire = c3.session_new(profile=str(prof_dir), session="lock_s3")
            self._assert_cli_ok(r_reacquire)

    def test_f4_profile_path_pointing_to_regular_file_and_empty_rejected(self) -> None:
        tmp_dir = _make_tmp_dir("mk2_pfile_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        reg = tmp_dir / "regular_file_profile"
        reg.write_text("not a directory", encoding="utf-8")
        with self._client() as client:
            r1 = client.session_new(profile=str(reg), session="pf_bad1", check=False)
            self._assert_cli_failed(r1)
            r2 = client.session_new(profile="", session="pf_bad2", check=False)
            self._assert_cli_failed(r2)

    def test_f4_session_export_import_roundtrip_and_invalid_import_rejected(self) -> None:
        base_url, _, _ = self._urls()
        tmp_dir = _make_tmp_dir("mk2_exp_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        export_path = tmp_dir / "state.json"
        corrupt_path = tmp_dir / "corrupt.json"
        corrupt_path.write_text("{not valid json", encoding="utf-8")

        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="src_s"))
            self._read_state_report(
                client,
                f"{base_url}/state?set_cookie=exp1&httponly=expho1&set_storage=expls1",
                session="src_s",
            )
            self._assert_cli_ok(client.session_export(export_path, session="src_s"))
            self.assertTrue(export_path.is_file())
            json.loads(export_path.read_text(encoding="utf-8"))

            self._assert_cli_ok(client.session_new(ephemeral=True, session="dst_s"))
            self._assert_cli_ok(client.session_import(export_path, session="dst_s"))
            rep = self._read_state_report(client, f"{base_url}/state", session="dst_s")
            self.assertEqual(rep.get("serverCookies", {}).get("mimikyu_normal"), "exp1")
            self.assertEqual(rep.get("serverCookies", {}).get("mimikyu_httponly"), "expho1")
            self.assertEqual(rep.get("localStorage", {}).get("token"), "expls1")

            r_missing = client.session_import(tmp_dir / "missing.json", session="dst_s", check=False)
            self._assert_cli_failed(r_missing)
            r_corrupt = client.session_import(corrupt_path, session="dst_s", check=False)
            self._assert_cli_failed(r_corrupt)

    def test_f4_session_export_to_directory_or_missing_parent_rejected(self) -> None:
        base_url, _, _ = self._urls()
        tmp_dir = _make_tmp_dir("mk2_expbad_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        with self._client() as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="exp_s"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="exp_s"))
            r_dir = client.session_export(tmp_dir, session="exp_s", check=False)
            self._assert_cli_failed(r_dir)
            r_noparent = client.session_export(
                tmp_dir / "nonexistent_subdir" / "out.json", session="exp_s", check=False
            )
            self._assert_cli_failed(r_noparent)


# ===========================================================================
# F5: Multi-Context Virtual Isolation Boundaries (6 tests)
# ===========================================================================


class TestF5MultiContextVirtualIsolationBoundaries(_Tier2TestCase):
    def test_f5_two_virtual_contexts_zero_cookie_storage_idb_leakage(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="v1"))
            self._assert_cli_ok(client.session_new(virtual=True, session="v2"))
            self._read_state_report(
                client,
                f"{base_url}/state?set_cookie=v1c&httponly=v1ho&set_storage=v1s",
                session="v1",
            )
            self._read_state_report(
                client,
                f"{base_url}/state?set_cookie=v2c&httponly=v2ho&set_storage=v2s",
                session="v2",
            )
            rep1 = self._read_state_report(client, f"{base_url}/state", session="v1")
            rep2 = self._read_state_report(client, f"{base_url}/state", session="v2")
            self.assertEqual(rep1["serverCookies"]["mimikyu_normal"], "v1c")
            self.assertEqual(rep1["serverCookies"]["mimikyu_httponly"], "v1ho")
            self.assertEqual(rep1["localStorage"]["token"], "v1s")
            self.assertEqual(rep1["sessionStorage"]["token"], "v1s")
            self.assertEqual(rep1["indexedDB"]["entries"]["token"], "v1s")

            self.assertEqual(rep2["serverCookies"]["mimikyu_normal"], "v2c")
            self.assertEqual(rep2["serverCookies"]["mimikyu_httponly"], "v2ho")
            self.assertEqual(rep2["localStorage"]["token"], "v2s")
            self.assertEqual(rep2["sessionStorage"]["token"], "v2s")
            self.assertEqual(rep2["indexedDB"]["entries"]["token"], "v2s")

    def test_f5_closing_one_virtual_context_preserves_sibling(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="sib1"))
            self._assert_cli_ok(client.session_new(virtual=True, session="sib2"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="sib1"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="sib2"))
            self._assert_cli_ok(client.session_close("sib1"))

            self._assert_cli_ok(client.open(f"{base_url}/nav/page2", session="sib2"))
            snap = client.snapshot(session="sib2")
            self._assert_cli_ok(snap)
            listed = client.session_list()
            self._assert_cli_ok(listed)
            blob = json.dumps(listed.data)
            self.assertIn("sib2", blob)
            self.assertNotIn("sib1", blob)

    def test_f5_duplicate_virtual_session_id_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="dup_ctx"))
            r_dup = client.session_new(virtual=True, session="dup_ctx", check=False)
            self._assert_cli_failed(r_dup)
            self._assert_cli_ok(client.open(f"{base_url}/health", session="dup_ctx"))

    def test_f5_virtual_context_etag_cache_not_shared(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="cache1"))
            self._assert_cli_ok(client.session_new(virtual=True, session="cache2"))
            self._read_state_report(client, f"{base_url}/state?set_cookie=cacheA", session="cache1")
            rep1 = self._read_state_report(client, f"{base_url}/state", session="cache1")
            self.assertEqual(rep1.get("serverCookies", {}).get("mimikyu_normal"), "cacheA")
            rep2 = self._read_state_report(client, f"{base_url}/state", session="cache2")
            self.assertNotEqual(rep2.get("serverCookies", {}).get("mimikyu_normal"), "cacheA")

    def test_f5_virtual_context_rapid_churn_cleanup(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            for idx in range(8):
                sid = f"churn_{idx}"
                self._assert_cli_ok(client.session_new(virtual=True, session=sid))
                self._assert_cli_ok(client.open(f"{base_url}/health", session=sid))
                self._assert_cli_ok(client.session_close(sid))
            listed = client.session_list()
            self._assert_cli_ok(listed)
            sessions = (
                listed.data.get("sessions")
                if isinstance(listed.data, dict) and "sessions" in listed.data
                else listed.data
            )
            self.assertEqual(sessions or [], [])

    def test_f5_invalid_and_path_unsafe_session_ids_rejected(self) -> None:
        with self._client() as client:
            for bad_id in ("", "../escape", "bad/id\nchars", "a" * 160):
                with self.subTest(session_id=repr(bad_id)):
                    if bad_id == "":
                        res = client.run(
                            "session",
                            "new",
                            "--virtual",
                            "--session",
                            "",
                            include_session=False,
                            check=False,
                        )
                    else:
                        res = client.session_new(virtual=True, session=bad_id, check=False)
                    self._assert_cli_failed(res)


# ===========================================================================
# F6: Coherent Environment Profiles & Validator Boundaries (6 tests)
# ===========================================================================


class TestF6EnvProfileValidatorBoundaries(_Tier2TestCase):
    def test_f6_invalid_preset_rejected_without_orphan_session(self) -> None:
        with self._client() as client:
            res = client.session_new(
                preset="windows95_invalid_e2e", virtual=True, session="bad_pre", check=False
            )
            self._assert_cli_failed(res)
            listed = client.session_list()
            self._assert_cli_ok(listed)
            self.assertNotIn("bad_pre", json.dumps(listed.data))

    def test_f6_malformed_zero_and_negative_viewports_rejected(self) -> None:
        with self._client() as client:
            for vp in ("invalid", "1280", "0x720", "1280x0", "-800x600", "999999x999999"):
                with self.subTest(viewport=vp):
                    res = client.session_new(
                        viewport=vp, virtual=True, session="bad_vp", check=False
                    )
                    if vp == "999999x999999" and res.ok:
                        self._assert_cli_ok(client.session_close("bad_vp"))
                    else:
                        self._assert_cli_failed(res)

    def test_f6_minimum_and_4k_viewport_bounds_on_echo_env(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            for sid, preset, vp, exp_w, exp_h in (
                ("vp_min", "linux", "320x240", 320, 240),
                ("vp_4k", "win11", "3840x2160", 3840, 2160),
            ):
                with self.subTest(viewport=vp):
                    self._assert_cli_ok(
                        client.session_new(virtual=True, preset=preset, viewport=vp, session=sid)
                    )
                    rep = self._read_env_report(client, base_url, session=sid)
                    screen = rep["screen"]
                    self.assertEqual(screen["viewport"]["innerWidth"], exp_w)
                    self.assertEqual(screen["viewport"]["innerHeight"], exp_h)
                    self.assertGreaterEqual(screen["width"], exp_w)
                    self.assertGreaterEqual(screen["height"], exp_h)
                    self.assertGreaterEqual(screen["devicePixelRatio"], 1)
                    self._assert_cli_ok(client.session_close(sid))

    def test_f6_preset_os_ua_client_hints_platform_coherence(self) -> None:
        base_url, _, _ = self._urls()
        expectations = {
            "win11": ("Win", "Windows", "Windows"),
            "macos": ("Mac", "Macintosh", "macOS"),
            "linux": ("Linux", "Linux", "Linux"),
        }
        with self._client() as client:
            for preset, (nav_plat, ua_sub, ch_plat) in expectations.items():
                with self.subTest(preset=preset):
                    sid = f"pre_{preset}"
                    self._assert_cli_ok(
                        client.session_new(virtual=True, preset=preset, session=sid)
                    )
                    rep = self._read_env_report(client, base_url, session=sid)
                    self.assertIn(nav_plat, rep["navigator"]["platform"])
                    self.assertIn(ua_sub, rep["httpHeaders"]["userAgent"])
                    self.assertIsNot(rep["navigator"].get("webdriver"), True)
                    sec_ch = rep["httpHeaders"].get("secChUaPlatform")
                    if sec_ch:
                        self.assertIn(ch_plat.lower(), sec_ch.lower())
                    self._assert_cli_ok(client.session_close(sid))

    def test_f6_webgl_audio_and_worker_consistency_across_presets(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, preset="macos", session="w_coh"))
            rep = self._read_env_report(client, base_url, session="w_coh")
            self.assertEqual(rep["worker"]["status"], "ok")
            self.assertIs(rep["workerConsistency"]["consistent"], True)
            self.assertEqual(rep["workerConsistency"]["mismatches"], [])
            if rep["audio"].get("status") == "ok":
                self.assertGreaterEqual(rep["audio"]["sampleRate"], 8000)
            if rep["webgl"].get("status") == "ok":
                self.assertTrue(rep["webgl"].get("vendor") or rep["webgl"].get("renderer"))

    def test_f6_http_header_vs_js_and_worker_ua_exact_match(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, preset="win11", session="ua_eq"))
            rep = self._read_env_report(client, base_url, session="ua_eq")
            self.assertEqual(rep["httpHeaders"]["userAgent"], rep["navigator"]["userAgent"])
            self.assertEqual(rep["navigator"]["userAgent"], rep["worker"]["userAgent"])


# ===========================================================================
# F7: Proxy, GeoIP, Locale, Timezone & WebRTC Boundaries (6 tests)
# ===========================================================================


class TestF7ProxyGeoipLocaleTzWebrtcBoundaries(_Tier2TestCase):
    def test_f7_malformed_and_unreachable_closed_port_proxy_rejected(self) -> None:
        base_url, _, _ = self._urls()
        closed_port = _allocate_closed_loopback_port()
        with self._client(timeout=15.0) as client:
            for bad_proxy in ("not-a-valid-proxy", "ftp://127.0.0.1:9999"):
                with self.subTest(proxy=bad_proxy):
                    res = client.session_new(
                        proxy=bad_proxy, virtual=True, session="bad_prx", check=False
                    )
                    self._assert_cli_failed(res)

            r_dead = client.session_new(
                proxy=f"http://127.0.0.1:{closed_port}",
                virtual=True,
                session="dead_prx",
                check=False,
            )
            if r_dead.ok:
                r_open = client.open(f"{base_url}/health", session="dead_prx", check=False)
                self._assert_cli_failed(r_open)
            else:
                self._assert_cli_failed(r_dead)

    def test_f7_invalid_timezone_locale_and_geoip_rejected(self) -> None:
        with self._client() as client:
            for label, kwargs in (
                ("timezone", {"timezone": "Mars/Olympus_Mons"}),
                ("locale", {"locale": "invalid_locale_999"}),
                ("geoip_ip", {"geoip": "999.999.999.999"}),
                ("geoip_str", {"geoip": "not-an-ip"}),
            ):
                with self.subTest(field=label):
                    res = client.session_new(
                        virtual=True, session=f"bad_{label}", check=False, **kwargs
                    )
                    self._assert_cli_failed(res)

    def test_f7_local_proxy_routing_records_traffic_in_proxy_logs(self) -> None:
        srv = LocalTestServer(name="f7-srv").start()
        self.addCleanup(srv.stop)
        proxy = LocalHttpProxy(allow_external=False).start()
        self.addCleanup(proxy.stop)

        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(virtual=True, proxy=proxy.proxy_url, session="prx_ok")
            )
            rep = self._read_env_report(client, srv.base_url, session="prx_ok")
            self.assertGreaterEqual(proxy.request_count, 1)
            self.assertTrue(
                any("/echo-env" in u for u in proxy.seen_urls),
                msg=f"Expected /echo-env in proxy.seen_urls, got {proxy.seen_urls}",
            )
            self.assertIsNotNone(rep.get("httpHeaders"))

    def test_f7_locale_accept_language_tz_and_worker_alignment(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(
                    virtual=True, locale="ja-JP", timezone="Asia/Tokyo", session="loc_jp"
                )
            )
            rep = self._read_env_report(client, base_url, session="loc_jp")
            self.assertTrue(rep["navigator"]["language"].lower().startswith("ja"))
            self.assertIn("ja", (rep["httpHeaders"].get("acceptLanguage") or "").lower())
            self.assertEqual(rep["intl"]["timezone"], "Asia/Tokyo")
            self.assertEqual(rep["intl"]["timezoneOffsetMinutes"], -540)
            self.assertEqual(rep["worker"]["timezone"], "Asia/Tokyo")
            self.assertIs(rep["workerConsistency"]["consistent"], True)

    def test_f7_geoip_loopback_and_regional_ip_alignment(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            r_loop = client.session_new(
                virtual=True, geoip="127.0.0.1", session="geo_loop", check=False
            )
            self.assertNotEqual(r_loop.returncode, -124)
            self.assertNotIn("panicked at", (r_loop.stdout + r_loop.stderr).lower())
            if r_loop.ok:
                self._assert_cli_ok(client.session_close("geo_loop"))

            self._assert_cli_ok(
                client.session_new(virtual=True, geoip="8.8.8.8", session="geo_us")
            )
            rep = self._read_env_report(client, base_url, session="geo_us")
            self.assertTrue(bool(rep["intl"]["timezone"]))
            self.assertIs(rep["workerConsistency"]["consistent"], True)

    def test_f7_webrtc_no_host_candidate_leak_under_proxy(self) -> None:
        base_url, _, proxy_url = self._urls()
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(
                    virtual=True,
                    proxy=proxy_url,
                    preset="macos",
                    locale="en-US",
                    timezone="America/New_York",
                    session="rtc_s",
                )
            )
            rep = self._read_env_report(client, base_url, session="rtc_s")
            webrtc = rep["webrtc"]
            self.assertIs(webrtc.get("hasHostCandidate"), False)
            self.assertNotIn("host", webrtc.get("candidateTypes", []))


# ===========================================================================
# F8: Resource Blocker & Telemetry Ring Buffer Boundaries (6 tests)
# ===========================================================================


class TestF8ResourceBlockerTelemetryBoundaries(_Tier2TestCase):
    def test_f8_block_images_blocks_png_allows_html_and_xhr(self) -> None:
        srv = LocalTestServer(name="f8-img").start()
        self.addCleanup(srv.stop)
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(virtual=True, block_images=True, session="blk_img")
            )
            self._assert_cli_ok(
                client.open(f"{srv.base_url}/workflow/download-verify", session="blk_img")
            )
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="blk_img"))
            paths = [r["path"] for r in srv.request_log]
            self.assertNotIn("/assets/img.png", paths)
            self._assert_cli_ok(client.click("#verify-sha-btn", session="blk_img"))
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=3000, session="blk_img"))
            self.assertIn("/download/sha256", [r["path"] for r in srv.request_log])

    def test_f8_block_media_prevents_mp4_request_and_allows_range_when_unblocked(self) -> None:
        srv = LocalTestServer(name="f8-media").start()
        self.addCleanup(srv.stop)
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(virtual=True, block_media=False, session="media_ctl")
            )
            self._assert_cli_ok(
                client.open(f"{srv.base_url}/workflow/download-verify", session="media_ctl")
            )
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="media_ctl"))
            range_res = client.eval(
                "fetch('/assets/media.mp4', {headers: {Range: 'bytes=0-9'}}).then(r => r.status)",
                session="media_ctl",
            )
            self._assert_cli_ok(range_res)
            self.assertEqual(_extract_json_value(range_res), 206)
            ctl_media = [r["path"] for r in srv.request_log if r["path"] == "/assets/media.mp4"]
            self.assertGreaterEqual(len(ctl_media), 1)
            self._assert_cli_ok(client.session_close("media_ctl"))

            srv.clear_requests()
            self._assert_cli_ok(
                client.session_new(virtual=True, block_media=True, session="media_blk")
            )
            self._assert_cli_ok(
                client.open(f"{srv.base_url}/workflow/download-verify", session="media_blk")
            )
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="media_blk"))
            blk_media = [r["path"] for r in srv.request_log if r["path"] == "/assets/media.mp4"]
            self.assertEqual(blk_media, [])

    def test_f8_block_fonts_prevents_woff2_request_when_font_applied(self) -> None:
        class _FontHandler(http.server.BaseHTTPRequestHandler):
            def log_message(self, fmt: str, *args: Any) -> None:
                return

            def do_GET(self) -> None:
                with self.server.request_lock:  # type: ignore[attr-defined]
                    self.server.request_paths.append(self.path)  # type: ignore[attr-defined]
                if self.path == "/assets/font.woff2":
                    self.send_response(200)
                    self.send_header("Content-Type", "font/woff2")
                    self.send_header("Content-Length", str(len(SAMPLE_WOFF2_BYTES)))
                    self.end_headers()
                    self.wfile.write(SAMPLE_WOFF2_BYTES)
                else:
                    html = (
                        "<!doctype html><html><head><style>"
                        "@font-face{font-family:'T2Font';src:url('/assets/font.woff2') format('woff2');}"
                        "body{font-family:'T2Font',sans-serif;}"
                        "</style></head><body><p id='fp'>Applied Font Probe</p></body></html>"
                    ).encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "text/html; charset=utf-8")
                    self.send_header("Content-Length", str(len(html)))
                    self.end_headers()
                    self.wfile.write(html)

        srv, custom_url, log, lock = _start_custom_http_server(_FontHandler)
        self.addCleanup(_stop_custom_http_server, srv)
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(virtual=True, block_fonts=False, session="fnt_ctl")
            )
            self._assert_cli_ok(client.open(f"{custom_url}/font-page", session="fnt_ctl"))
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="fnt_ctl"))
            with lock:
                ctl_fonts = [p for p in log if p == "/assets/font.woff2"]
                log.clear()
            self.assertGreaterEqual(len(ctl_fonts), 1)
            self._assert_cli_ok(client.session_close("fnt_ctl"))

            self._assert_cli_ok(
                client.session_new(virtual=True, block_fonts=True, session="fnt_blk")
            )
            self._assert_cli_ok(client.open(f"{custom_url}/font-page", session="fnt_blk"))
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="fnt_blk"))
            with lock:
                blk_fonts = [p for p in log if p == "/assets/font.woff2"]
            self.assertEqual(blk_fonts, [])

    def test_f8_console_and_network_telemetry_captures_errors_and_404(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="tel_s"))
            self._assert_cli_ok(
                client.open(f"{base_url}/console-errors?uncaught=1&assets=1", session="tel_s")
            )
            self._assert_cli_ok(
                client.eval("fetch('/definitely-missing-404').catch(() => {})", session="tel_s")
            )
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="tel_s"))
            c_res = client.logs("console", session="tel_s")
            self._assert_cli_ok(c_res)
            c_blob = json.dumps(c_res.data)
            for marker in ("mimikyu-e2e-log", "mimikyu-e2e-warn", "mimikyu-e2e-error", "mimikyu-e2e-uncaught"):
                self.assertIn(marker, c_blob)

            n_res = client.logs("network", session="tel_s")
            self._assert_cli_ok(n_res)
            n_blob = json.dumps(n_res.data)
            self.assertIn("/assets/img.png", n_blob)
            self.assertIn("definitely-missing-404", n_blob)
            self.assertIn("404", n_blob)

    def test_f8_telemetry_ring_buffer_bounded_under_2500_log_burst(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="ring_s"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="ring_s"))
            self._assert_cli_ok(
                client.eval(
                    "for (let i = 0; i < 2500; i++) console.log('burst-msg-' + i); 'done';",
                    session="ring_s",
                )
            )
            res = client.logs("console", session="ring_s")
            self._assert_cli_ok(res)
            entries = res.data
            if isinstance(res.data, dict):
                entries = None
                for key in ("entries", "logs", "items", "events"):
                    if isinstance(res.data.get(key), list):
                        entries = res.data[key]
                        break
                if entries is None:
                    for val in res.data.values():
                        if isinstance(val, list):
                            entries = val
                            break
            self.assertIsInstance(
                entries,
                list,
                msg=f"Expected console logs list in response data, got {type(res.data).__name__}: {res.data!r}",
            )
            self.assertLess(len(entries), 2500)
            self.assertGreater(len(entries), 0)
            self.assertIn("burst-msg-2499", json.dumps(res.data))

    def test_f8_invalid_logs_kind_and_cross_session_telemetry_isolation(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="tel_a"))
            self._assert_cli_ok(client.session_new(virtual=True, session="tel_b"))
            r_bad = client.logs("bogus_telemetry_kind", session="tel_a", check=False)
            self._assert_cli_failed(r_bad)

            self._assert_cli_ok(
                client.open(f"{base_url}/console-errors?uncaught=1", session="tel_a")
            )
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="tel_b"))
            logs_b = client.logs("console", session="tel_b")
            self._assert_cli_ok(logs_b)
            blob_b = json.dumps(logs_b.data)
            self.assertNotIn("mimikyu-e2e-error", blob_b)
            self.assertNotIn("mimikyu-e2e-uncaught", blob_b)


# ===========================================================================
# F9: Isolated-World & Opt-in Main-World Eval Boundaries (6 tests)
# ===========================================================================


class TestF9IsolatedVsMainWorldEvalBoundaries(_Tier2TestCase):
    def test_f9_isolated_world_zero_global_leakage(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="iso1"))
            rep = self._read_env_report(client, base_url, session="iso1")
            self.assertIs(rep["isolatedWorld"]["isolatedWorldClean"], True)
            self.assertEqual(rep["isolatedWorld"]["leakedKeys"], [])

            self._assert_cli_ok(
                client.eval(
                    "window.__mimikyu = 'leaked'; window.__e2e_iso_probe = 123; document.title",
                    main_world=False,
                    session="iso1",
                )
            )
            mw = client.eval(
                "typeof window.__mimikyu + ':' + typeof window.__e2e_iso_probe",
                main_world=True,
                session="iso1",
            )
            self._assert_cli_ok(mw)
            self.assertEqual(_extract_json_value(mw), "undefined:undefined")

    def test_f9_isolated_vs_main_world_expando_visibility(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="iso2"))
            self._assert_cli_ok(client.open(f"{base_url}/frame-same", session="iso2"))
            iso_res = client.eval("typeof window.__FRAME_MARKER", main_world=False, session="iso2")
            self._assert_cli_ok(iso_res)
            self.assertEqual(_extract_json_value(iso_res), "undefined")

            mw_res = client.eval("window.__FRAME_MARKER.frame", main_world=True, session="iso2")
            self._assert_cli_ok(mw_res)
            self.assertEqual(_extract_json_value(mw_res), "same")

    def test_f9_eval_syntax_error_and_runtime_throw_recovery(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="eval_err"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="eval_err"))
            for js, mw in (
                ("function(", False),
                ("(() => { throw new Error('e2e-iso-boom'); })()", False),
                ("(() => { throw new TypeError('e2e-mw-boom'); })()", True),
            ):
                with self.subTest(js=js, main_world=mw):
                    res = client.eval(js, main_world=mw, session="eval_err", check=False)
                    self._assert_cli_failed(res)
            ok_res = client.eval("2 + 3", session="eval_err")
            self._assert_cli_ok(ok_res)
            self.assertEqual(_extract_json_value(ok_res), 5)

    def test_f9_eval_cyclic_object_undefined_and_bigint_handling(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="eval_edge"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="eval_edge"))

            r_cyc = client.eval(
                "(() => { const a = {x: 1}; a.self = a; return a; })()",
                session="eval_edge",
                check=False,
            )
            self._assert_cli_failed(r_cyc)

            r_big = client.eval("BigInt(42)", session="eval_edge", check=False)
            self._assert_cli_failed(r_big)

            r_undef = client.eval("undefined", session="eval_edge", check=False)
            self.assertNotEqual(r_undef.returncode, -124)
            self.assertIsInstance(r_undef.payload, dict)
            self.assertIs(r_undef.ok, True)

            r_ok = client.eval("1 + 1", session="eval_edge")
            self._assert_cli_ok(r_ok)
            self.assertEqual(_extract_json_value(r_ok), 2)

    def test_f9_eval_async_promise_resolve_and_reject(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="eval_async"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="eval_async"))
            r_ok = client.eval("Promise.resolve({answer: 42})", session="eval_async")
            self._assert_cli_ok(r_ok)
            self.assertEqual(_extract_json_value(r_ok), {"answer": 42})

            r_rej = client.eval(
                "Promise.reject(new Error('async-boom-e2e'))", session="eval_async", check=False
            )
            self._assert_cli_failed(r_rej)

    def test_f9_closed_shadow_root_and_cross_origin_frame_isolation(self) -> None:
        base_url, cross_url, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="shd_s"))
            self._assert_cli_ok(
                client.open(
                    f"{base_url}/workflow/cross-frame?cross_origin={urllib.parse.quote(cross_url, safe='')}",
                    session="shd_s",
                )
            )
            r_closed = client.eval(
                "document.getElementById('closed-shadow-host').shadowRoot === null",
                session="shd_s",
            )
            self._assert_cli_ok(r_closed)
            self.assertIs(_extract_json_value(r_closed), True)

            r_xorig = client.eval(
                "(() => { try { return document.getElementById('cross-frame').contentDocument === null ? 'null' : 'exposed'; } catch (e) { return 'blocked:' + e.name; } })()",
                main_world=True,
                session="shd_s",
            )
            self._assert_cli_ok(r_xorig)
            val = str(_extract_json_value(r_xorig))
            self.assertTrue(val == "null" or val.startswith("blocked:"), msg=f"Got {val!r}")


# ===========================================================================
# F10: Compact @eN Semantic Snapshots & Diff Boundaries (6 tests)
# ===========================================================================


class TestF10SnapshotDiffBoundaries(_Tier2TestCase):
    def test_f10_empty_document_snapshot_minimal(self) -> None:
        url = _data_url("<!doctype html><html><head><title>Empty</title></head><body></body></html>")
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="snap_empty"))
            self._assert_cli_ok(client.open(url, session="snap_empty"))
            s_full = client.snapshot(session="snap_empty")
            self._assert_cli_ok(s_full)
            s_inter = client.snapshot(interactive_only=True, session="snap_empty")
            self._assert_cli_ok(s_inter)
            self.assertEqual(extract_all_ax_refs(s_inter), [])

    def test_f10_max_depth_bounds_and_negative_depth_rejected(self) -> None:
        nested = "<div>" * 12 + '<button id="deep-btn">DeepTarget</button>' + "</div>" * 12
        url = _data_url(f"<!doctype html><html><body>{nested}</body></html>")
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="snap_depth"))
            self._assert_cli_ok(client.open(url, session="snap_depth"))
            full = client.snapshot(session="snap_depth")
            self._assert_cli_ok(full)
            self.assertIsNotNone(extract_ax_ref(full, role="button", name_substr="DeepTarget"))

            shallow = client.snapshot(max_depth=1, session="snap_depth")
            self._assert_cli_ok(shallow)
            self.assertIsNone(extract_ax_ref(shallow, role="button", name_substr="DeepTarget"))

            neg = client.snapshot(max_depth=-1, session="snap_depth", check=False)
            self._assert_cli_failed(neg)

    def test_f10_interactive_only_filters_static_nodes(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="snap_inter"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="snap_inter"))
            full = client.snapshot(interactive_only=False, session="snap_inter")
            inter = client.snapshot(interactive_only=True, session="snap_inter")
            self._assert_cli_ok(full)
            self._assert_cli_ok(inter)
            self.assertIsNotNone(extract_ax_ref(inter, role="link", name_substr="Go to Page 2"))
            self.assertNotIn("Primary navigation fixture", _extract_text_blob(inter))
            self.assertLess(len(_extract_text_blob(inter)), len(_extract_text_blob(full)))

    def test_f10_snapshot_diff_unchanged_and_modal_mutation(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="snap_diff"))
            self._assert_cli_ok(client.open(f"{base_url}/spa?delay_ms=10", session="snap_diff"))
            self._assert_cli_ok(client.wait(text="loaded", timeout_ms=5000, session="snap_diff"))

            base_snap = client.snapshot(session="snap_diff")
            self._assert_cli_ok(base_snap)
            same_diff = client.snapshot(diff=True, session="snap_diff")
            self._assert_cli_ok(same_diff)
            self.assertIsNone(extract_ax_ref(same_diff, role="button", name_substr="Close Modal"))

            self._assert_cli_ok(client.click("#open-modal", session="snap_diff"))
            mut_diff = client.snapshot(diff=True, session="snap_diff")
            self._assert_cli_ok(mut_diff)
            self.assertIsNotNone(extract_ax_ref(mut_diff, role="button", name_substr="Close Modal"))

    def test_f10_hidden_and_aria_hidden_elements_excluded(self) -> None:
        url = _data_url(
            '<!doctype html><html><body>'
            '<button style="display:none">HiddenCSS</button>'
            '<button aria-hidden="true">HiddenARIA</button>'
            "<button>VisibleBtn</button>"
            "</body></html>"
        )
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="snap_hid"))
            self._assert_cli_ok(client.open(url, session="snap_hid"))
            snap = client.snapshot(interactive_only=True, session="snap_hid")
            self._assert_cli_ok(snap)
            self.assertIsNotNone(extract_ax_ref(snap, role="button", name_substr="VisibleBtn"))
            self.assertIsNone(extract_ax_ref(snap, role="button", name_substr="HiddenCSS"))
            self.assertIsNone(extract_ax_ref(snap, role="button", name_substr="HiddenARIA"))

    def test_f10_stale_ref_rejected_after_navigation(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="snap_stale"))
            self._assert_cli_ok(client.open(f"{base_url}/workflow/catalog", session="snap_stale"))
            self._assert_cli_ok(client.wait(text="ready", timeout_ms=5000, session="snap_stale"))
            snap = client.snapshot(interactive_only=True, session="snap_stale")
            self._assert_cli_ok(snap)
            refs = extract_all_ax_refs(snap)
            self.assertGreaterEqual(len(refs), 5)
            stale_ref = refs[-1]

            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="snap_stale"))
            r_click = client.click(stale_ref, session="snap_stale", check=False)
            self._assert_cli_failed(r_click)


# ===========================================================================
# F11: Content Extraction, Media & Animations Boundaries (6 tests)
# ===========================================================================


class TestF11ExtractMediaAnimationBoundaries(_Tier2TestCase):
    def test_f11_extract_invalid_mode_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="ext_bad"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="ext_bad"))
            res = client.extract("bogus_format_xyz", session="ext_bad", check=False)
            self._assert_cli_failed(res)

    def test_f11_extract_tables_and_links_empty_vs_populated(self) -> None:
        base_url, _, _ = self._urls()
        plain_url = _data_url("<!doctype html><html><body><p>Plain text only</p></body></html>")
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="ext_emp"))
            self._assert_cli_ok(client.open(plain_url, session="ext_emp"))
            t_empty = client.extract("tables", session="ext_emp")
            l_empty = client.extract("links", session="ext_emp")
            self._assert_cli_ok(t_empty)
            self._assert_cli_ok(l_empty)
            if isinstance(t_empty.data, list):
                self.assertEqual(t_empty.data, [])
            if isinstance(l_empty.data, list):
                self.assertEqual(l_empty.data, [])

            self._assert_cli_ok(client.open(f"{base_url}/workflow/catalog", session="ext_emp"))
            self._assert_cli_ok(client.wait(text="ready", timeout_ms=5000, session="ext_emp"))
            t_pop = client.extract("tables", session="ext_emp")
            self._assert_cli_ok(t_pop)
            self.assertIn("Node Alpha", json.dumps(t_pop.data))

    def test_f11_screenshot_invalid_target_zero_size_and_hidden_rejected(self) -> None:
        tmp_dir = _make_tmp_dir("mk2_shot_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        url = _data_url(
            '<div id="zero" style="width:0;height:0;overflow:hidden"></div>'
            '<div id="hidden" style="display:none">hi</div>'
            '<div id="ok" style="width:40px;height:20px;background:red">ok</div>'
        )
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="shot_bad"))
            self._assert_cli_ok(client.open(url, session="shot_bad"))
            for name, target in (("badref", "@e99999"), ("zero", "#zero"), ("hidden", "#hidden")):
                with self.subTest(target=target):
                    out_png = tmp_dir / f"{name}.png"
                    res = client.screenshot(out_png, target=target, session="shot_bad", check=False)
                    self._assert_cli_failed(res)
                    self.assertFalse(out_png.exists() and out_png.stat().st_size == 0)

    def test_f11_screenshot_full_page_taller_than_viewport_and_non_mutating(self) -> None:
        tmp_dir = _make_tmp_dir("mk2_full_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        url = _data_url(
            '<body style="margin:0"><div style="height:2400px;background:#ccc">Tall</div>'
            '<button id="bottom-btn">Bottom</button></body>'
        )
        vp_png = tmp_dir / "vp.png"
        fp_png = tmp_dir / "fp.png"
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(virtual=True, viewport="800x600", session="shot_fp")
            )
            self._assert_cli_ok(client.open(url, session="shot_fp"))
            self._assert_cli_ok(client.screenshot(vp_png, full_page=False, session="shot_fp"))
            self._assert_cli_ok(client.screenshot(fp_png, full_page=True, session="shot_fp"))
            sy = client.eval("window.scrollY", session="shot_fp")
            self._assert_cli_ok(sy)
            self.assertEqual(_extract_json_value(sy), 0)

        vp_bytes = vp_png.read_bytes()
        fp_bytes = fp_png.read_bytes()
        self.assertTrue(vp_bytes.startswith(b"\x89PNG\r\n\x1a\n"))
        self.assertTrue(fp_bytes.startswith(b"\x89PNG\r\n\x1a\n"))
        _, vp_h = struct.unpack(">II", vp_bytes[16:24])
        _, fp_h = struct.unpack(">II", fp_bytes[16:24])
        self.assertGreater(fp_h, vp_h)
        self.assertGreaterEqual(fp_h, 2000)

    def test_f11_pdf_valid_header_and_unwritable_path_rejected(self) -> None:
        base_url, _, _ = self._urls()
        tmp_dir = _make_tmp_dir("mk2_pdf_")
        self.addCleanup(shutil.rmtree, tmp_dir, True)
        ok_pdf = tmp_dir / "artifact.pdf"
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="pdf_s"))
            self._assert_cli_ok(client.open(f"{base_url}/workflow/download-verify", session="pdf_s"))
            self._assert_cli_ok(client.pdf(ok_pdf, session="pdf_s"))
            data = ok_pdf.read_bytes()
            self.assertTrue(data.startswith(b"%PDF-"))
            self.assertGreater(len(data), 1024)

            bad_pdf = tmp_dir / "nonexistent_parent" / "artifact.pdf"
            r_bad = client.pdf(bad_pdf, session="pdf_s", check=False)
            self._assert_cli_failed(r_bad)

    def test_f11_instant_animations_fast_forwards_long_css_animation(self) -> None:
        url = _data_url(
            "<!doctype html><html><head><style>"
            "@keyframes slide{from{transform:translateX(0px)}to{transform:translateX(200px)}}"
            "#box{width:20px;height:20px;animation:slide 10s linear forwards;}"
            '</style></head><body><div id="box"></div></body></html>'
        )
        with self._client() as client:
            t0 = time.monotonic()
            self._assert_cli_ok(
                client.session_new(virtual=True, instant_animations=True, session="anim_s")
            )
            self._assert_cli_ok(client.open(url, session="anim_s"))
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=3000, session="anim_s"))
            elapsed = time.monotonic() - t0
            self.assertLess(elapsed, 6.0)
            left_res = client.eval(
                "Math.round(document.getElementById('box').getBoundingClientRect().left)",
                session="anim_s",
            )
            self._assert_cli_ok(left_res)
            self.assertGreaterEqual(int(_extract_json_value(left_res)), 200)


# ===========================================================================
# F12: Deterministic Waits & Action Batching Boundaries (6 tests)
# ===========================================================================


class TestF12WaitBatchBoundaries(_Tier2TestCase):
    def test_f12_wait_timeout_negative_text_selector_url(self) -> None:
        base_url, _, _ = self._urls()
        with self._client(timeout=10.0) as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="w_to"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="w_to"))
            for kwargs in (
                {"text": "NEVER_APPEARS_E2E_XYZ", "timeout_ms": 250},
                {"selector": "#nonexistent-el-xyz", "timeout_ms": 250},
                {"url": "/never-matches-xyz", "timeout_ms": 250},
            ):
                with self.subTest(wait=kwargs):
                    res = client.wait(session="w_to", check=False, **kwargs)
                    self._assert_timeout_rejected(res)

    def test_f12_wait_missing_predicate_and_invalid_selector_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client(timeout=10.0) as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="w_bad"))
            self._assert_cli_ok(client.open(f"{base_url}/nav/page1", session="w_bad"))
            r_none = client.wait(session="w_bad", check=False)
            self._assert_cli_failed(r_none)
            r_css = client.wait(selector="div[[[", timeout_ms=500, session="w_bad", check=False)
            self._assert_cli_failed(r_css)

    def test_f12_wait_idle_respects_delayed_spa_xhr_and_timeout_boundary(self) -> None:
        base_url, _, _ = self._urls()
        with self._client(timeout=15.0) as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="w_spa"))
            self._assert_cli_ok(client.open(f"{base_url}/spa?delay_ms=500", session="w_spa"))
            self._assert_cli_ok(client.wait(idle=True, timeout_ms=5000, session="w_spa"))
            st = client.eval("document.getElementById('spa-status').textContent", session="w_spa")
            self._assert_cli_ok(st)
            self.assertEqual(_extract_json_value(st), "loaded")

            self._assert_cli_ok(client.open(f"{base_url}/spa?delay_ms=2500", session="w_spa"))
            r_short = client.wait(idle=True, timeout_ms=150, session="w_spa", check=False)
            self._assert_timeout_rejected(r_short)

    def test_f12_batch_malformed_json_missing_file_and_unknown_op_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="b_bad"))
            self._assert_cli_ok(client.open(f"{base_url}/form", session="b_bad"))
            self._assert_cli_failed(client.batch(ops="{not valid json", session="b_bad", check=False))
            self._assert_cli_failed(
                client.batch(file_path="/nonexistent/e2e_ops.json", session="b_bad", check=False)
            )
            self._assert_cli_failed(
                client.batch(ops=[{"op": "unknown_op_xyz"}], session="b_bad", check=False)
            )
            self._assert_cli_ok(client.eval("document.location.pathname", session="b_bad"))

    def test_f12_batch_empty_ops_handled_cleanly(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="b_emp"))
            self._assert_cli_ok(client.open(f"{base_url}/form", session="b_emp"))
            res = client.batch(ops=[], session="b_emp", check=False)
            self.assertNotEqual(res.returncode, -124)
            self.assertNotIn("panicked at", (res.stdout + res.stderr).lower())
            self.assertIsInstance(res.payload, dict)
            self.assertIsInstance(res.payload.get("ok"), bool)
            if res.payload.get("ok") is False:
                self.assertTrue(bool(res.error))
            path_res = client.eval("document.location.pathname", session="b_emp")
            self._assert_cli_ok(path_res)
            self.assertEqual(_extract_json_value(path_res), "/form")

    def test_f12_batch_mid_step_failure_aborts_subsequent_ops(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="b_mid"))
            self._assert_cli_ok(client.open(f"{base_url}/form", session="b_mid"))
            ops = [
                {"op": "fill", "target": "#name", "value": "StepOneVal"},
                {"op": "click", "target": "#does-not-exist-xyz"},
                {"op": "fill", "target": "#name", "value": "StepThreeShouldNotRun"},
            ]
            res = client.batch(ops=ops, session="b_mid", check=False)
            self._assert_cli_failed(res)
            val_res = client.eval("document.getElementById('name').value", session="b_mid")
            self._assert_cli_ok(val_res)
            self.assertEqual(_extract_json_value(val_res), "StepOneVal")


# ===========================================================================
# F13: Chokepoint Input & Kinematic Humanize Boundaries (6 tests)
# ===========================================================================


class TestF13ChokepointInputHumanizeBoundaries(_Tier2TestCase):
    def test_f13_click_fill_select_missing_target_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="inp_miss"))
            self._assert_cli_ok(client.open(f"{base_url}/form", session="inp_miss"))
            for res in (
                client.click("#missing-xyz", session="inp_miss", check=False),
                client.click("@e99999", session="inp_miss", check=False),
                client.fill("#missing-xyz", "v", session="inp_miss", check=False),
                client.select("#missing-xyz", "agent", session="inp_miss", check=False),
            ):
                self._assert_cli_failed(res)

    def test_f13_click_and_fill_hidden_or_non_editable_element_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="inp_hid"))
            self._assert_cli_ok(client.open(f"{base_url}/workflow/wizard", session="inp_hid"))
            self._assert_cli_failed(client.click("#wiz-submit", session="inp_hid", check=False))
            self._assert_cli_failed(
                client.fill("#wiz-webhook", "http://127.0.0.1/cb", session="inp_hid", check=False)
            )
            self._assert_cli_ok(client.open(f"{base_url}/spa", session="inp_hid"))
            self._assert_cli_failed(
                client.fill("#open-modal", "non-editable", session="inp_hid", check=False)
            )

    def test_f13_select_invalid_option_and_upload_missing_file_rejected(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="inp_sel"))
            self._assert_cli_ok(client.open(f"{base_url}/form", session="inp_sel"))
            self._assert_cli_failed(
                client.select("#role", "nonexistent_role_xyz", session="inp_sel", check=False)
            )
            self._assert_cli_failed(
                client.upload("#upload", "/nonexistent/e2e_missing_file.txt", session="inp_sel", check=False)
            )
            role_res = client.eval("document.getElementById('role').value", session="inp_sel")
            self._assert_cli_ok(role_res)
            self.assertEqual(_extract_json_value(role_res), "admin")

    def test_f13_scroll_negative_zero_and_offscreen_click_clamping(self) -> None:
        url = _data_url(
            '<body style="margin:0"><div style="height:2200px">Spacer</div>'
            '<button id="far-btn" onclick="this.textContent=\'CLICKED\'">Far</button></body>'
        )
        with self._client() as client:
            self._assert_cli_ok(
                client.session_new(virtual=True, viewport="800x600", session="inp_scr")
            )
            self._assert_cli_ok(client.open(url, session="inp_scr"))
            self._assert_cli_ok(client.scroll(0, 0, session="inp_scr"))
            self._assert_cli_ok(client.scroll(0, 1800, session="inp_scr"))
            y1 = client.eval("window.scrollY", session="inp_scr")
            self._assert_cli_ok(y1)
            self.assertGreater(int(_extract_json_value(y1)), 0)

            self._assert_cli_ok(client.scroll(-50, -5000, session="inp_scr"))
            y2 = client.eval("window.scrollY", session="inp_scr")
            self._assert_cli_ok(y2)
            self.assertGreaterEqual(int(_extract_json_value(y2)), 0)

            self._assert_cli_ok(client.click("#far-btn", session="inp_scr"))
            txt = client.eval("document.getElementById('far-btn').textContent", session="inp_scr")
            self._assert_cli_ok(txt)
            self.assertEqual(_extract_json_value(txt), "CLICKED")

    def test_f13_humanize_click_emits_bezier_mousemove_trajectory(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="hum_c"))
            self._assert_cli_ok(client.open(f"{base_url}/humanize-tracker", session="hum_c"))
            self._assert_cli_ok(client.click("#start-btn", humanize=True, session="hum_c"))
            self._assert_cli_ok(client.click("#reset-btn", session="hum_c"))
            self._assert_cli_ok(client.click("#target-btn", humanize=True, session="hum_c"))

            ev_res = client.eval(
                "JSON.stringify(window.__TRACKER_EVENTS)",
                main_world=True,
                session="hum_c",
            )
            self._assert_cli_ok(ev_res)
            events = _extract_json_value(ev_res)
            self.assertIsInstance(events, list)
            moves = [e for e in events if e.get("type") == "mousemove"]
            self.assertGreaterEqual(len(moves), 3)
            coords = {(e.get("x"), e.get("y")) for e in moves}
            self.assertGreaterEqual(len(coords), 2)
            downs = [e for e in events if e.get("type") == "mousedown" and e.get("targetId") == "target-btn"]
            ups = [e for e in events if e.get("type") == "mouseup" and e.get("targetId") == "target-btn"]
            self.assertGreaterEqual(len(downs), 1)
            self.assertGreaterEqual(len(ups), 1)
            times = [e["t"] for e in events if "t" in e]
            self.assertEqual(times, sorted(times))
            self.assertGreater(times[-1] - times[0], 0)

    def test_f13_humanize_type_empty_unicode_and_keydown_keyup_events(self) -> None:
        base_url, _, _ = self._urls()
        with self._client() as client:
            self._assert_cli_ok(client.session_new(virtual=True, session="hum_t"))
            self._assert_cli_ok(client.open(f"{base_url}/humanize-tracker", session="hum_t"))

            r_empty = client.type("#tracker-input", "", session="hum_t", check=False)
            self.assertNotEqual(r_empty.returncode, -124)
            self.assertNotIn("panicked at", (r_empty.stdout + r_empty.stderr).lower())

            self._assert_cli_ok(client.click("#reset-btn", session="hum_t"))
            self._assert_cli_ok(client.type("#tracker-input", "abc!", humanize=True, session="hum_t"))
            val1 = client.eval("document.getElementById('tracker-input').value", session="hum_t")
            self._assert_cli_ok(val1)
            self.assertEqual(_extract_json_value(val1), "abc!")

            ev_res = client.eval(
                "JSON.stringify(window.__TRACKER_EVENTS)",
                main_world=True,
                session="hum_t",
            )
            self._assert_cli_ok(ev_res)
            events = _extract_json_value(ev_res)
            self.assertIsInstance(events, list)
            self.assertGreaterEqual(sum(1 for e in events if e.get("type") == "keydown"), 4)
            self.assertGreaterEqual(sum(1 for e in events if e.get("type") == "keyup"), 4)

            unicode_val = "Alice <e2e@local> — 日本語"
            self._assert_cli_ok(client.fill("#tracker-input", unicode_val, session="hum_t"))
            val2 = client.eval("document.getElementById('tracker-input').value", session="hum_t")
            self._assert_cli_ok(val2)
            self.assertEqual(_extract_json_value(val2), unicode_val)


# ===========================================================================
# F14: Warm Daemon, CLI & Stdio MCP Server Boundaries (7 tests)
# ===========================================================================


class TestF14DaemonCliMcpBoundaries(_Tier2TestCase):
    def test_f14_cli_unknown_subcommand_and_missing_required_args(self) -> None:
        with self._client() as client:
            r1 = client.run(
                "unknown-subcmd-xyz",
                json_output=False,
                include_engine=False,
                include_session=False,
                check=False,
            )
            self._assert_cli_failed(r1)
            r2 = client.run("open", json_output=True, check=False)
            self._assert_cli_failed(r2)

    def test_f14_daemon_stale_socket_and_pid_recovery(self) -> None:
        base_url, _, _ = self._urls()
        state_dir = _make_tmp_dir("mk2_stale_")
        self.addCleanup(shutil.rmtree, state_dir, True)
        sock_path = state_dir / "daemon.sock"
        srv_sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            srv_sock.bind(str(sock_path))
        finally:
            srv_sock.close()
        (state_dir / "daemon.pid").write_text("99999999", encoding="utf-8")

        with self._client(state_dir=state_dir) as client:
            self._assert_cli_ok(client.session_new(ephemeral=True, session="rec_s"))
            self._assert_cli_ok(client.open(f"{base_url}/health", session="rec_s"))

    def test_f14_daemon_concurrent_cli_calls_and_foreground_idle_timeout(self) -> None:
        state_dir_conc = _make_tmp_dir("mk2_conc_")
        self.addCleanup(shutil.rmtree, state_dir_conc, True)
        with self._client(state_dir=state_dir_conc) as client:
            results: list[CommandResult] = []
            lock = threading.Lock()

            def _worker() -> None:
                res = client.session_list(check=False)
                with lock:
                    results.append(res)

            threads = [threading.Thread(target=_worker) for _ in range(3)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=15.0)
            self.assertEqual(len(results), 3)
            for res in results:
                self._assert_cli_ok(res)

        state_dir_fg = _make_tmp_dir("mk2_fg_")
        self.addCleanup(shutil.rmtree, state_dir_fg, True)
        proc = subprocess.Popen(
            [self._bin(), "--state-dir", str(state_dir_fg), "daemon", "--foreground", "--idle-timeout", "1"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        pgid = proc.pid
        try:
            rc = proc.wait(timeout=6.0)
            self.assertEqual(rc, 0)
        finally:
            _kill_pgid_safely(proc, pgid=pgid)

    def test_f14_mcp_initialize_and_tools_list_contract(self) -> None:
        expected_tools = {
            "mimikyu_session_new",
            "mimikyu_session_close",
            "mimikyu_open",
            "mimikyu_snapshot",
            "mimikyu_click",
            "mimikyu_fill",
            "mimikyu_extract",
            "mimikyu_wait",
            "mimikyu_batch",
            "mimikyu_eval",
            "mimikyu_screenshot",
        }
        with McpClient(self._bin(), engine=get_config().engine) as mcp:
            init = mcp.initialize()
            self.assertIsInstance(init, dict)
            self.assertIn("capabilities", init)
            tools = mcp.tools_list()
            names = {t.get("name") for t in tools if isinstance(t, dict)}
            self.assertTrue(expected_tools.issubset(names), msg=f"Missing MCP tools: {expected_tools - names}")
            for tool in tools:
                self.assertIsInstance(tool.get("inputSchema"), dict)

    def test_f14_mcp_malformed_jsonrpc_parse_error_and_recovery(self) -> None:
        state_dir = _make_tmp_dir("mk2_mcpraw_")
        self.addCleanup(shutil.rmtree, state_dir, True)
        proc = subprocess.Popen(
            [self._bin(), "--state-dir", str(state_dir), "mcp"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )
        pgid = proc.pid
        try:
            assert proc.stdin is not None and proc.stdout is not None
            payload = (
                b"{not valid json\n"
                b"\n"
                b"[]\n"
                b'{"jsonrpc":"1.0","id":1,"method":"initialize"}\n'
                b'{"jsonrpc":"2.0","id":99,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t2","version":"0.1"}}}\n'
            )
            out_bytes, err_bytes = proc.communicate(input=payload, timeout=10.0)
            text = out_bytes.decode("utf-8", errors="replace")
            self.assertNotIn("panicked at", (text + err_bytes.decode("utf-8", errors="replace")).lower())
            msgs: list[dict[str, Any]] = []
            for raw_line in text.splitlines():
                line = raw_line.strip()
                if line.startswith("{"):
                    try:
                        msgs.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
            err_codes = {
                m["error"].get("code")
                for m in msgs
                if isinstance(m.get("error"), dict) and "code" in m["error"]
            }
            self.assertTrue(
                bool(err_codes & {-32700, -32600}),
                msg=f"Expected -32700 or -32600 error code, got msgs={msgs}",
            )
            init_ok = [m for m in msgs if m.get("id") == 99 and "result" in m]
            self.assertEqual(len(init_ok), 1, msg=f"Expected id=99 initialize result, got msgs={msgs}")
        finally:
            _kill_pgid_safely(proc, pgid=pgid)

    def test_f14_mcp_unknown_method_unknown_tool_and_missing_args(self) -> None:
        with McpClient(self._bin(), engine=get_config().engine) as mcp:
            mcp.initialize()
            self._assert_mcp_failed(mcp, mcp.request, "nonexistent/method_e2e", {})
            self._assert_mcp_failed(mcp, mcp.tools_call, "mimikyu_nonexistent_tool", {})
            self._assert_mcp_failed(mcp, mcp.tools_call, "mimikyu_open", {})

    def test_f14_mcp_eof_shutdown_tears_down_browser_processes(self) -> None:
        base_url, _, _ = self._urls()
        state_dir = _make_tmp_dir("mk2_mcpeof_")
        self.addCleanup(shutil.rmtree, state_dir, True)
        with McpClient(self._bin(), state_dir=state_dir, engine=get_config().engine) as mcp:
            mcp.initialize()
            res_new = mcp.tools_call("mimikyu_session_new", {"ephemeral": True})
            self.assertIsNot(res_new.get("isError"), True)
            res_open = mcp.tools_call("mimikyu_open", {"url": f"{base_url}/nav/page1"})
            self.assertIsNot(res_open.get("isError"), True)
        time.sleep(0.3)
        lingering = _lingering_state_dir_procs(state_dir)
        self.assertEqual(lingering, [], msg=f"Lingering MCP child processes: {lingering}")


# ===========================================================================
# F15: Local Test & Reproducible Benchmark Suite Boundaries (5 tests)
# ===========================================================================


class TestF15BenchmarkBoundaries(_Tier2TestCase):
    def test_f15_bench_minimal_iterations_zero_warmup_json_schema(self) -> None:
        with self._client(timeout=120.0) as client:
            res = client.bench(iterations=1, warmup=0, json_output=True, timeout=120.0)
            self._assert_cli_ok(res, require_json=True)
            self.assertIsInstance(res.data, dict)
            self.assertGreater(len(res.data), 0)

    def test_f15_bench_zero_and_negative_iterations_warmup_rejected(self) -> None:
        with self._client() as client:
            for args in (
                ("bench", "--iterations", "0"),
                ("bench", "--iterations", "-1"),
                ("bench", "--warmup", "-1"),
            ):
                with self.subTest(args=args):
                    res = client.run(*args, include_session=False, check=False)
                    self._assert_cli_failed(res)

    def test_f15_bench_non_numeric_and_unknown_flags_rejected(self) -> None:
        with self._client() as client:
            for args in (
                ("bench", "--iterations", "abc"),
                ("bench", "--bogus-bench-flag"),
            ):
                with self.subTest(args=args):
                    res = client.run(*args, include_session=False, check=False)
                    self._assert_cli_failed(res)

    def test_f15_bench_non_json_output_and_zero_session_leak(self) -> None:
        with self._client(timeout=120.0) as client:
            res = client.bench(iterations=1, warmup=0, json_output=False, timeout=120.0)
            self._assert_cli_ok(res, require_json=False)
            self.assertGreater(len(res.stdout.strip()), 0)
            listed = client.session_list()
            self._assert_cli_ok(listed)
            sessions = (
                listed.data.get("sessions")
                if isinstance(listed.data, dict) and "sessions" in listed.data
                else listed.data
            )
            self.assertEqual(sessions or [], [])

    def test_f15_bench_repeatability_and_offline_isolation(self) -> None:
        closed_port = _allocate_closed_loopback_port()
        dead_proxy = f"http://127.0.0.1:{closed_port}"
        with self._client(timeout=120.0) as client:
            res = client.run(
                "bench",
                "--iterations",
                "2",
                "--warmup",
                "1",
                include_session=False,
                env_overrides={
                    "HTTP_PROXY": dead_proxy,
                    "HTTPS_PROXY": dead_proxy,
                    "http_proxy": dead_proxy,
                    "https_proxy": dead_proxy,
                },
                timeout=120.0,
                check=False,
            )
            self._assert_cli_ok(res, require_json=True)
            assert_r3_compliant(res.stdout + "\n" + res.stderr)


# ===========================================================================
# R3 Scope Compliance: Static (No Binary) & CLI/MCP Schema (Binary Required)
# ===========================================================================


class TestR3StaticCompliance(unittest.TestCase):
    def test_r3_all_e2e_python_files_compliant(self) -> None:
        e2e_dir = Path(__file__).resolve().parent
        py_files = sorted(e2e_dir.glob("*.py"))
        self.assertGreaterEqual(len(py_files), 6)
        for py_file in py_files:
            with self.subTest(file=py_file.name):
                self.assertEqual(check_r3_compliance(py_file), [])
        assert_r3_compliant(e2e_dir)

    def test_r3_scanner_rejects_all_forbidden_keywords_and_allows_benign(self) -> None:
        self.assertGreater(len(R3_FORBIDDEN_KEYWORDS), 0)
        for kw in R3_FORBIDDEN_KEYWORDS:
            with self.subTest(keyword=kw):
                with self.assertRaises(AssertionError):
                    assert_r3_compliant(f"prefix_{kw}_suffix")
                with self.assertRaises(AssertionError):
                    assert_r3_compliant(f"PREFIX_{kw.upper()}_SUFFIX")
        for benign in ("mimikyu_session_new", "humanize_cursor", "proxy_list", "wait_idle"):
            assert_r3_compliant(benign)


class TestR3SchemaCompliance(_Tier2TestCase):
    def test_r3_cli_top_level_and_subcommand_help_compliant(self) -> None:
        bin_path = self._bin()
        subcommands: list[tuple[str, ...]] = [
            (),
            ("doctor",),
            ("daemon",),
            ("session",),
            ("session", "new"),
            ("session", "list"),
            ("session", "close"),
            ("session", "export"),
            ("session", "import"),
            ("open",),
            ("back",),
            ("forward",),
            ("reload",),
            ("snapshot",),
            ("click",),
            ("fill",),
            ("type",),
            ("select",),
            ("upload",),
            ("scroll",),
            ("press",),
            ("wait",),
            ("batch",),
            ("extract",),
            ("eval",),
            ("screenshot",),
            ("pdf",),
            ("logs",),
            ("mcp",),
            ("bench",),
        ]
        for subcmd in subcommands:
            with self.subTest(subcommand=" ".join(subcmd) or "<root>"):
                proc = subprocess.run(
                    [bin_path, *subcmd, "--help"],
                    capture_output=True,
                    text=True,
                    timeout=5.0,
                    check=False,
                )
                self.assertEqual(proc.returncode, 0, msg=f"--help failed for {subcmd}: {proc.stderr}")
                assert_r3_compliant(proc.stdout + "\n" + proc.stderr)

    def test_r3_cli_rejects_dynamically_constructed_excluded_flags(self) -> None:
        with self._client() as client:
            for kw in R3_FORBIDDEN_KEYWORDS:
                if " " in kw:
                    continue
                flag = "--" + re.sub(r"[^a-z0-9]+", "-", kw.lower()).strip("-")
                with self.subTest(flag=flag):
                    res = client.run("session", "new", flag, check=False)
                    self._assert_cli_failed(res)

    def test_r3_mcp_tools_list_schemas_compliant(self) -> None:
        with McpClient(self._bin(), engine=get_config().engine) as mcp:
            mcp.initialize()
            tools = mcp.tools_list()
            self.assertGreater(len(tools), 0)
            assert_r3_compliant(json.dumps(tools, indent=2))

    def test_r3_doctor_and_bench_outputs_compliant(self) -> None:
        with self._client(timeout=120.0) as client:
            doc = client.doctor(json_output=True)
            self._assert_cli_ok(doc)
            assert_r3_compliant(doc.stdout + "\n" + doc.stderr)

            bench = client.bench(iterations=1, warmup=0, json_output=True, timeout=120.0)
            self._assert_cli_ok(bench)
            assert_r3_compliant(bench.stdout + "\n" + bench.stderr)


if __name__ == "__main__":
    unittest.main()
