"""Tier 1 isolated happy-path E2E tests for mimikyu (Features F1-F15).

Opaque-box Python 3 stdlib test suite exercising mimikyu via its public CLI
(--json), Unix-socket daemon auto-start, and stdio MCP JSON-RPC 2.0 interfaces
strictly against local 127.0.0.1:0 fixtures.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import shutil
import struct
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any, Optional

REPO_ROOT: Path = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from e2e.helpers import (  # noqa: E402
    CommandResult,
    McpClient,
    MimikyuClient,
    assert_r3_compliant,
    extract_all_ax_refs,
    extract_ax_ref,
    get_config,
)
from e2e.local_server import LocalHttpProxy  # noqa: E402


# ---------------------------------------------------------------------------
# Shared Normalization & Probe Helpers
# ---------------------------------------------------------------------------


def _unwrap_eval(res: CommandResult) -> Any:
    """Normalize mimikyu eval --json output across scalar and wrapped shapes."""
    if not res.ok:
        raise AssertionError(f"eval failed (rc={res.returncode}): {res.error or res.stdout}")
    if not isinstance(res.payload, dict) or res.payload.get("ok") is not True:
        raise AssertionError(f"eval JSON envelope invalid: {res.payload!r}")
    data = res.data
    if isinstance(data, dict):
        for key in ("value", "result", "output", "eval"):
            if key in data:
                return data[key]
    return data


def _unwrap_json_eval(res: CommandResult) -> Any:
    """Unwrap eval output and parse JSON if returned as a string."""
    val = _unwrap_eval(res)
    if isinstance(val, str):
        return json.loads(val)
    return val


def _unwrap_sessions(res: CommandResult) -> list[str]:
    """Normalize session list --json output into a list of session ID strings."""
    if not res.ok:
        raise AssertionError(f"session list failed (rc={res.returncode}): {res.error or res.stdout}")
    data = res.data
    raw_items: Any = None
    if isinstance(data, list):
        raw_items = data
    elif isinstance(data, dict):
        for key in ("sessions", "items", "list"):
            if isinstance(data.get(key), list):
                raw_items = data[key]
                break
    if raw_items is None:
        return []
    out: list[str] = []
    for entry in raw_items:
        if isinstance(entry, str):
            out.append(entry)
        elif isinstance(entry, dict):
            for key in ("id", "session", "session_id", "name"):
                val = entry.get(key)
                if val is not None:
                    out.append(str(val))
                    break
    return out


def _unwrap_extract(res: CommandResult) -> str:
    """Normalize extract --json output into a text representation."""
    if not res.ok:
        raise AssertionError(f"extract failed (rc={res.returncode}): {res.error or res.stdout}")
    data = res.data
    if isinstance(data, str):
        return data
    if isinstance(data, dict):
        for key in ("text", "markdown", "content", "output"):
            val = data.get(key)
            if isinstance(val, str):
                return val
    return json.dumps(data)


def _unwrap_log_entries(res: CommandResult) -> list[Any]:
    """Normalize logs console/network --json output into a list of entries."""
    if not res.ok:
        raise AssertionError(f"logs failed (rc={res.returncode}): {res.error or res.stdout}")
    data = res.data
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        for key in ("entries", "logs", "events", "items", "requests"):
            if isinstance(data.get(key), list):
                return data[key]
    return []


def _read_state_report(
    client: MimikyuClient,
    session: Optional[str] = None,
) -> dict[str, Any]:
    """Wait for /state readiness and return parsed __STATE_REPORT."""
    client.wait(selector='#state-json[data-ready="true"]', timeout_ms=5000, session=session, check=True)
    res = client.eval(
        "JSON.stringify(window.__STATE_REPORT || JSON.parse(document.getElementById('state-json').textContent))",
        session=session,
        check=True,
    )
    report = _unwrap_json_eval(res)
    if not isinstance(report, dict):
        raise AssertionError(f"Expected dict state report, got: {report!r}")
    for key in ("serverCookies", "localStorage", "sessionStorage", "indexedDB"):
        if key not in report:
            raise AssertionError(f"Missing key {key!r} in state report: {report!r}")
    return report


def _open_and_read_env_report(
    client: MimikyuClient,
    base_url: str,
    session: Optional[str] = None,
) -> dict[str, Any]:
    """Open /echo-env, wait for async probes, and return parsed #env-json."""
    client.open(f"{base_url}/echo-env", session=session, check=True)
    client.wait(selector='#env-json[data-ready="true"]', timeout_ms=5000, session=session, check=True)
    res = client.eval(
        "document.getElementById('env-json').textContent",
        session=session,
        check=True,
    )
    report = _unwrap_json_eval(res)
    if not isinstance(report, dict):
        raise AssertionError(f"Expected dict env report, got: {report!r}")
    return report


def _assert_pid_reaped_if_reported(
    testcase: unittest.TestCase,
    data: Any,
    timeout: float = 2.0,
) -> None:
    """If a PID is reported in session_new data, verify it exits cleanly."""
    if not isinstance(data, dict):
        return
    pid = data.get("pid")
    if not isinstance(pid, int) or isinstance(pid, bool):
        return
    testcase.assertGreater(pid, 0)
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return
        except PermissionError:
            return
        time.sleep(0.05)
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return
    except PermissionError:
        return
    testcase.fail(f"Reported browser PID {pid} is still alive after session close")


def _collect_doctor_paths(obj: Any, out: list[str]) -> None:
    """Recursively collect executable path values from doctor JSON data."""
    if isinstance(obj, list):
        for item in obj:
            _collect_doctor_paths(item, out)
        return
    if not isinstance(obj, dict):
        return
    if obj.get("available") is False or obj.get("found") is False:
        return
    for key in ("path", "binary", "binary_path", "executable"):
        val = obj.get(key)
        if isinstance(val, str):
            out.append(val)
    for key, val in obj.items():
        if isinstance(key, str) and key.lower() == "skipped_candidates":
            continue
        if isinstance(val, (dict, list)):
            _collect_doctor_paths(val, out)


def _has_available_engine(obj: Any) -> bool:
    """Return True if doctor JSON data reports at least one available browser."""
    if isinstance(obj, list):
        return any(_has_available_engine(item) for item in obj)
    if not isinstance(obj, dict):
        return False
    if obj.get("available") is False or obj.get("found") is False:
        return False
    if obj.get("available") is True or obj.get("found") is True:
        return True
    for key in ("path", "binary", "binary_path", "executable"):
        val = obj.get(key)
        if isinstance(val, str) and val.strip():
            return True
    return any(
        _has_available_engine(v)
        for k, v in obj.items()
        if not (isinstance(k, str) and k.lower() == "skipped_candidates")
    )


def _extract_engine_names(obj: Any) -> set[str]:
    """Extract canonical engine names mentioned in doctor JSON data."""
    blob = json.dumps(obj).lower()
    found: set[str] = set()
    if "chromium" in blob or "chrome" in blob:
        found.add("chromium")
    if "firefox" in blob:
        found.add("firefox")
    return found


def _collect_numbers_for_keys(obj: Any, key_substrings: tuple[str, ...]) -> list[float]:
    """Recursively gather finite numeric values under keys matching substrings."""
    results: list[float] = []
    if isinstance(obj, list):
        for item in obj:
            results.extend(_collect_numbers_for_keys(item, key_substrings))
        return results
    if not isinstance(obj, dict):
        return results
    for k, v in obj.items():
        k_low = str(k).lower()
        if any(sub in k_low for sub in key_substrings):
            if isinstance(v, (int, float)) and not isinstance(v, bool) and math.isfinite(float(v)):
                results.append(float(v))
            elif isinstance(v, dict):
                for sub_v in v.values():
                    if isinstance(sub_v, (int, float)) and not isinstance(sub_v, bool) and math.isfinite(float(sub_v)):
                        results.append(float(sub_v))
        if isinstance(v, (dict, list)):
            results.extend(_collect_numbers_for_keys(v, key_substrings))
    return results


def _iter_network_entries_for_url(obj: Any, url_substr: str) -> list[dict[str, Any]]:
    """Recursively collect network log entry dicts matching url_substr."""
    matches: list[dict[str, Any]] = []
    if isinstance(obj, list):
        for item in obj:
            matches.extend(_iter_network_entries_for_url(item, url_substr))
        return matches
    if not isinstance(obj, dict):
        return matches
    top_url = obj.get("url")
    req = obj.get("request")
    req_url = req.get("url") if isinstance(req, dict) else None
    matched_req = isinstance(req_url, str) and url_substr in req_url
    if (isinstance(top_url, str) and url_substr in top_url) or matched_req:
        matches.append(obj)
    for k, val in obj.items():
        if k == "request" and matched_req:
            continue
        if isinstance(val, (dict, list)):
            matches.extend(_iter_network_entries_for_url(val, url_substr))
    return matches


def _find_key_values(obj: Any, target_key: str) -> list[Any]:
    """Recursively collect all values for exact dictionary key `target_key`."""
    found: list[Any] = []
    if isinstance(obj, list):
        for item in obj:
            found.extend(_find_key_values(item, target_key))
        return found
    if not isinstance(obj, dict):
        return found
    if target_key in obj:
        found.append(obj[target_key])
    for val in obj.values():
        if isinstance(val, (dict, list)):
            found.extend(_find_key_values(val, target_key))
    return found


def _unwrap_mcp_text(res: dict[str, Any]) -> str:
    """Extract concatenated text from an MCP tools/call response."""
    if res.get("isError") is True:
        raise AssertionError(f"MCP tool returned isError=True: {res!r}")
    content = res.get("content")
    if isinstance(content, list):
        parts = [
            str(entry.get("text"))
            for entry in content
            if isinstance(entry, dict) and entry.get("text") is not None
        ]
        if parts:
            return "\n".join(parts)
    return json.dumps(res)


# ---------------------------------------------------------------------------
# F1: Browser Auto-Discovery & Doctor (5 tests)
# ---------------------------------------------------------------------------


class TestF1BrowserDiscoveryAndDoctor(unittest.TestCase):
    def test_f1_01_doctor_json_envelope_and_structure(self) -> None:
        with MimikyuClient() as client:
            res = client.doctor(json_output=True)
            self.assertEqual(res.returncode, 0)
            self.assertTrue(res.ok)
            self.assertIsInstance(res.payload, dict)
            self.assertIs(res.payload.get("ok"), True)
            self.assertIsNotNone(res.data)
            self.assertIsInstance(res.data, (dict, list))
            self.assertGreater(len(res.data), 0)
            assert_r3_compliant(res.stdout)

    def test_f1_02_doctor_discovers_at_least_one_supported_engine(self) -> None:
        with MimikyuClient() as client:
            res = client.doctor(json_output=True)
            self.assertTrue(res.ok)
            serialized = json.dumps(res.data).lower()
            self.assertTrue(
                any(tok in serialized for tok in ("chromium", "chrome", "firefox")),
                f"Expected chromium/chrome/firefox in doctor output: {serialized}",
            )
            self.assertTrue(_has_available_engine(res.data))

    def test_f1_03_doctor_reported_binary_paths_are_executable_files(self) -> None:
        with MimikyuClient() as client:
            res = client.doctor(json_output=True)
            self.assertTrue(res.ok)
            paths: list[str] = []
            _collect_doctor_paths(res.data, paths)
            self.assertGreater(len(paths), 0)
            for p in paths:
                stripped = p.strip()
                self.assertTrue(len(stripped) > 0)
                has_sep = os.sep in stripped or (os.altsep is not None and os.altsep in stripped)
                resolved = stripped if has_sep else (shutil.which(stripped) or "")
                self.assertTrue(
                    os.path.exists(resolved),
                    f"Reported binary path does not exist: {stripped}",
                )
                self.assertTrue(os.path.isfile(resolved), f"Not a regular file: {resolved}")
                self.assertTrue(os.access(resolved, os.X_OK), f"Not executable: {resolved}")

            # Verify recursive doctor walkers ignore skipped_candidates even when zero healthy browsers exist.
            only_skipped = {
                "chromium": [],
                "firefox": [],
                "skipped_candidates": [
                    {
                        "engine": "chromium",
                        "source": "puppeteer_cache",
                        "path": "/nonexistent/broken/chrome",
                        "available": False,
                        "reason": "probe failed",
                    }
                ],
            }
            skipped_paths: list[str] = []
            _collect_doctor_paths(only_skipped, skipped_paths)
            self.assertEqual(skipped_paths, [])
            self.assertFalse(_has_available_engine(only_skipped))

    def test_f1_04_doctor_is_idempotent_and_creates_no_sessions(self) -> None:
        with MimikyuClient() as client:
            r1 = client.doctor(json_output=True)
            r2 = client.doctor(json_output=True)
            r_list = client.session_list(json_output=True)
            self.assertTrue(r1.ok)
            self.assertTrue(r2.ok)
            self.assertTrue(r_list.ok)
            engines1 = _extract_engine_names(r1.data)
            engines2 = _extract_engine_names(r2.data)
            self.assertGreater(len(engines1), 0)
            self.assertEqual(engines1, engines2)
            self.assertEqual(_unwrap_sessions(r_list), [])

    def test_f1_05_doctor_human_readable_non_json_output(self) -> None:
        with MimikyuClient() as client:
            res = client.doctor(json_output=False)
            self.assertEqual(res.returncode, 0)
            self.assertTrue(res.ok)
            self.assertGreater(len(res.stdout.strip()), 0)
            low = res.stdout.lower()
            self.assertTrue(
                any(tok in low for tok in ("chromium", "chrome", "firefox", "browser", "doctor")),
                f"Unexpected doctor text output: {res.stdout!r}",
            )


# ---------------------------------------------------------------------------
# F2: Debloated Launch & Process Lifecycle (5 tests)
# ---------------------------------------------------------------------------


class TestF2LaunchAndProcessLifecycle(unittest.TestCase):
    def test_f2_01_session_new_open_and_explicit_close_lifecycle(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            r_new = client.session_new(session="f2_life")
            self.assertTrue(r_new.ok)
            r_open = client.open(f"{cfg.base_url}/nav/page1", session="f2_life")
            self.assertTrue(r_open.ok)
            r_before = client.session_list()
            self.assertTrue(r_before.ok)
            self.assertIn("f2_life", _unwrap_sessions(r_before))
            r_close = client.session_close("f2_life")
            self.assertTrue(r_close.ok)
            r_after = client.session_list()
            self.assertTrue(r_after.ok)
            self.assertNotIn("f2_life", _unwrap_sessions(r_after))
            _assert_pid_reaped_if_reported(self, r_new.data)

    def test_f2_02_client_context_manager_teardown_cleans_sessions_and_state_dir(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            state_dir = client.state_dir
            r_new = client.session_new(session="f2_ctx")
            self.assertTrue(r_new.ok)
            r_open = client.open(f"{cfg.base_url}/health", session="f2_ctx")
            self.assertTrue(r_open.ok)
        self.assertFalse(state_dir.exists())
        _assert_pid_reaped_if_reported(self, r_new.data)

    def test_f2_03_debloated_launch_clean_window_no_mimikyu_global_leak(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(session="f2_env", check=True)
            report = _open_and_read_env_report(client, cfg.base_url, session="f2_env")
            iso = report["isolatedWorld"]
            self.assertIs(iso["isolatedWorldClean"], True)
            self.assertEqual(iso["mimikyuGlobalType"], "undefined")
            self.assertEqual(iso["leakedKeys"], [])
            client.session_close("f2_env", check=True)

    def test_f2_04_debloated_launch_clean_page_zero_console_errors_and_no_external_urls(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(session="f2_quiet", check=True)
            client.open(f"{cfg.base_url}/nav/page1", session="f2_quiet", check=True)
            r_con = client.logs("console", session="f2_quiet")
            r_net = client.logs("network", session="f2_quiet")
            self.assertTrue(r_con.ok)
            self.assertTrue(r_net.ok)
            for entry in _unwrap_log_entries(r_con):
                if isinstance(entry, dict):
                    lvl = str(entry.get("level") or entry.get("type") or "").lower()
                    self.assertNotIn(lvl, ("error", "exception"))
            for entry in _unwrap_log_entries(r_net):
                if isinstance(entry, dict) and isinstance(entry.get("url"), str):
                    url = entry["url"]
                    if url.startswith(("http://", "https://")):
                        self.assertTrue(
                            url.startswith(("http://127.0.0.1", "http://localhost", "https://127.0.0.1", "https://localhost")),
                            f"Unexpected external request during clean launch: {url}",
                        )
            client.session_close("f2_quiet", check=True)

    def test_f2_05_repeated_launch_and_close_cycles_leave_zero_orphan_sessions(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            for i in range(3):
                sid = f"f2_cyc_{i}"
                r_new = client.session_new(session=sid, check=True)
                client.open(f"{cfg.base_url}/nav/page{(i % 2) + 1}", session=sid, check=True)
                client.session_close(sid, check=True)
                _assert_pid_reaped_if_reported(self, r_new.data)
            r_list = client.session_list(check=True)
            self.assertEqual(_unwrap_sessions(r_list), [])


# ---------------------------------------------------------------------------
# F3: Dual-Protocol Driver — CDP + BiDi Parity (6 tests)
# ---------------------------------------------------------------------------


class TestF3DualProtocolDriverParity(unittest.TestCase):
    ENGINES: tuple[str, ...] = ("chromium", "firefox")

    def test_f3_01_chromium_and_firefox_session_launch_open_and_extract_parity(self) -> None:
        cfg = get_config()
        for engine in self.ENGINES:
            with self.subTest(engine=engine):
                with MimikyuClient(engine=engine) as client:
                    sid = f"f3_open_{engine}"
                    self.assertTrue(client.session_new(session=sid, engine=engine).ok)
                    self.assertTrue(client.open(f"{cfg.base_url}/nav/page1", session=sid).ok)
                    r_title = client.eval("document.title", session=sid)
                    r_ext = client.extract("text", session=sid)
                    self.assertEqual(_unwrap_eval(r_title), "Nav Page 1")
                    self.assertIn("Page 1", _unwrap_extract(r_ext))
                    self.assertTrue(client.session_close(sid).ok)

    def test_f3_02_history_back_forward_reload_parity(self) -> None:
        cfg = get_config()
        for engine in self.ENGINES:
            with self.subTest(engine=engine):
                with MimikyuClient(engine=engine) as client:
                    sid = f"f3_hist_{engine}"
                    client.session_new(session=sid, engine=engine, check=True)
                    client.open(f"{cfg.base_url}/nav/page1", session=sid, check=True)
                    client.open(f"{cfg.base_url}/nav/page2", session=sid, check=True)
                    client.back(session=sid, check=True)
                    t_back = client.eval("document.title", session=sid)
                    self.assertEqual(_unwrap_eval(t_back), "Nav Page 1")
                    client.forward(session=sid, check=True)
                    t_fwd = client.eval("document.title", session=sid)
                    self.assertEqual(_unwrap_eval(t_fwd), "Nav Page 2")
                    client.reload(session=sid, check=True)
                    p_rel = client.eval("window.location.pathname", session=sid)
                    self.assertEqual(_unwrap_eval(p_rel), "/nav/page2")
                    client.session_close(sid, check=True)

    def test_f3_03_http_302_redirect_resolution_parity(self) -> None:
        cfg = get_config()
        for engine in self.ENGINES:
            with self.subTest(engine=engine):
                with MimikyuClient(engine=engine) as client:
                    sid = f"f3_redir_{engine}"
                    client.session_new(session=sid, engine=engine, check=True)
                    r_open = client.open(f"{cfg.base_url}/redirect", session=sid)
                    self.assertTrue(r_open.ok)
                    r_path = client.eval("window.location.pathname", session=sid)
                    r_title = client.eval("document.title", session=sid)
                    self.assertEqual(_unwrap_eval(r_path), "/nav/page2")
                    self.assertEqual(_unwrap_eval(r_title), "Nav Page 2")
                    client.session_close(sid, check=True)

    def test_f3_04_snapshot_ax_ref_and_click_navigation_parity(self) -> None:
        cfg = get_config()
        for engine in self.ENGINES:
            with self.subTest(engine=engine):
                with MimikyuClient(engine=engine) as client:
                    sid = f"f3_snap_{engine}"
                    client.session_new(session=sid, engine=engine, check=True)
                    client.open(f"{cfg.base_url}/nav/page1", session=sid, check=True)
                    r_snap = client.snapshot(session=sid)
                    self.assertTrue(r_snap.ok)
                    ref = extract_ax_ref(r_snap, role="link", name_substr="Page 2")
                    self.assertIsNotNone(ref)
                    assert ref is not None
                    self.assertTrue(ref.startswith("@e"))
                    r_click = client.click(ref, session=sid)
                    self.assertTrue(r_click.ok)
                    r_wait = client.wait(url="/nav/page2", timeout_ms=5000, session=sid)
                    self.assertTrue(r_wait.ok)
                    r_title = client.eval("document.title", session=sid)
                    self.assertEqual(_unwrap_eval(r_title), "Nav Page 2")
                    client.session_close(sid, check=True)

    def test_f3_05_form_fill_and_submit_parity(self) -> None:
        cfg = get_config()
        for engine in self.ENGINES:
            with self.subTest(engine=engine):
                with MimikyuClient(engine=engine) as client:
                    sid = f"f3_form_{engine}"
                    client.session_new(session=sid, engine=engine, check=True)
                    client.open(f"{cfg.base_url}/form", session=sid, check=True)
                    r_snap = client.snapshot(session=sid)
                    self.assertTrue(r_snap.ok)
                    name_ref = extract_ax_ref(r_snap, role="textbox", name_substr="Name") or "#name"
                    submit_ref = extract_ax_ref(r_snap, role="button", name_substr="Submit") or "#submit-btn"
                    self.assertTrue(client.fill(name_ref, "parity_alice", session=sid).ok)
                    self.assertTrue(client.click(submit_ref, session=sid).ok)
                    self.assertTrue(client.wait(url="/submit", timeout_ms=5000, session=sid).ok)
                    r_ext = client.extract("text", session=sid)
                    self.assertIn("parity_alice", _unwrap_extract(r_ext))
                    client.session_close(sid, check=True)

    def test_f3_06_cookie_and_storage_parity(self) -> None:
        cfg = get_config()
        for engine in self.ENGINES:
            with self.subTest(engine=engine):
                with MimikyuClient(engine=engine) as client:
                    sid = f"f3_state_{engine}"
                    client.session_new(session=sid, engine=engine, check=True)
                    client.open(
                        f"{cfg.base_url}/state?set_cookie=f3_ck&set_storage=f3_st",
                        session=sid,
                        check=True,
                    )
                    _ = _read_state_report(client, session=sid)
                    client.open(f"{cfg.base_url}/state", session=sid, check=True)
                    rep = _read_state_report(client, session=sid)
                    self.assertEqual(rep["serverCookies"].get("mimikyu_normal"), "f3_ck")
                    self.assertEqual(rep["serverCookies"].get("mimikyu_httponly"), "ho_f3_ck")
                    self.assertEqual(rep["localStorage"].get("token"), "f3_st")
                    self.assertEqual(rep["sessionStorage"].get("token"), "f3_st")
                    client.session_close(sid, check=True)


# ---------------------------------------------------------------------------
# F4: Persistent & Ephemeral Disk Profiles (6 tests)
# ---------------------------------------------------------------------------


class TestF4DiskProfilesAndStatePortability(unittest.TestCase):
    def test_f4_01_named_profile_persists_cookies_and_localstorage_across_close(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(profile="f4_named", session="w1", check=True)
            client.open(
                f"{cfg.base_url}/state?set_cookie=named_ck&ls_key=acct&ls_val=u42",
                session="w1",
                check=True,
            )
            _ = _read_state_report(client, session="w1")
            client.session_close("w1", check=True)

            client.session_new(profile="f4_named", session="r1", check=True)
            client.open(f"{cfg.base_url}/state", session="r1", check=True)
            rep = _read_state_report(client, session="r1")
            self.assertEqual(rep["serverCookies"].get("mimikyu_normal"), "named_ck")
            self.assertEqual(rep["serverCookies"].get("mimikyu_httponly"), "ho_named_ck")
            self.assertEqual(rep["localStorage"].get("acct"), "u42")
            client.session_close("r1", check=True)

    def test_f4_02_explicit_profile_path_persists_across_independent_clients(self) -> None:
        cfg = get_config()
        with tempfile.TemporaryDirectory() as tmp:
            profile_dir = Path(tmp) / "disk_prof"
            with MimikyuClient() as c1:
                c1.session_new(profile=str(profile_dir), session="p1", check=True)
                c1.open(
                    f"{cfg.base_url}/state?set_cookie=disk_ck&ls_key=org&ls_val=acme&idb_key=doc&idb_val=v1",
                    session="p1",
                    check=True,
                )
                _ = _read_state_report(c1, session="p1")
                c1.session_close("p1", check=True)

            self.assertTrue(profile_dir.exists())

            with MimikyuClient() as c2:
                c2.session_new(profile=str(profile_dir), session="p2", check=True)
                c2.open(f"{cfg.base_url}/state", session="p2", check=True)
                rep = _read_state_report(c2, session="p2")
                self.assertEqual(rep["serverCookies"].get("mimikyu_normal"), "disk_ck")
                self.assertEqual(rep["serverCookies"].get("mimikyu_httponly"), "ho_disk_ck")
                self.assertEqual(rep["localStorage"].get("org"), "acme")
                if rep["indexedDB"].get("status") == "ok":
                    self.assertEqual(rep["indexedDB"]["entries"].get("doc"), "v1")
                c2.session_close("p2", check=True)

    def test_f4_03_ephemeral_profile_clears_all_state_after_close(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(ephemeral=True, session="e1", check=True)
            client.open(f"{cfg.base_url}/state?set_cookie=eph_ck&set_storage=eph_st", session="e1", check=True)
            rep1 = _read_state_report(client, session="e1")
            self.assertEqual(rep1["localStorage"].get("token"), "eph_st")
            client.session_close("e1", check=True)

            client.session_new(ephemeral=True, session="e2", check=True)
            client.open(f"{cfg.base_url}/state", session="e2", check=True)
            rep2 = _read_state_report(client, session="e2")
            self.assertEqual(rep2["serverCookies"], {})
            self.assertEqual(rep2["documentCookie"], "")
            self.assertEqual(rep2["localStorage"], {})
            self.assertEqual(rep2["sessionStorage"], {})
            self.assertEqual(rep2["indexedDB"].get("entries", {}), {})
            client.session_close("e2", check=True)

    def test_f4_04_distinct_named_profiles_do_not_cross_contaminate(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(profile="prof_a", session="sa", check=True)
            client.open(
                f"{cfg.base_url}/state?set_cookie=ck_a&ls_key=tenant&ls_val=alpha",
                session="sa",
                check=True,
            )
            _ = _read_state_report(client, session="sa")
            client.session_close("sa", check=True)

            client.session_new(profile="prof_b", session="sb", check=True)
            client.open(f"{cfg.base_url}/state", session="sb", check=True)
            rep_b = _read_state_report(client, session="sb")
            self.assertNotIn("mimikyu_normal", rep_b["serverCookies"])
            self.assertNotIn("tenant", rep_b["localStorage"])
            client.session_close("sb", check=True)

    def test_f4_05_session_export_produces_valid_non_empty_json_file(self) -> None:
        cfg = get_config()
        with tempfile.TemporaryDirectory() as tmp:
            export_path = Path(tmp) / "state_export.json"
            with MimikyuClient() as client:
                client.session_new(ephemeral=True, session="exp_s", check=True)
                client.open(
                    f"{cfg.base_url}/state?set_cookie=exp_ck&ls_key=exp_k&ls_val=exp_v",
                    session="exp_s",
                    check=True,
                )
                _ = _read_state_report(client, session="exp_s")
                r_exp = client.session_export(export_path, session="exp_s")
                self.assertTrue(r_exp.ok)
                self.assertTrue(export_path.is_file())
                raw = export_path.read_text(encoding="utf-8")
                parsed = json.loads(raw)
                self.assertIsInstance(parsed, (dict, list))
                self.assertGreater(len(parsed), 0)
                self.assertTrue("exp_ck" in raw or "exp_k" in raw)

    def test_f4_06_session_export_and_import_restores_cookies_and_localstorage(self) -> None:
        cfg = get_config()
        with tempfile.TemporaryDirectory() as tmp:
            export_path = Path(tmp) / "xfer.json"
            with MimikyuClient() as client:
                client.session_new(ephemeral=True, session="src", check=True)
                client.open(
                    f"{cfg.base_url}/state?set_cookie=xfer_ck&ls_key=xfer_k&ls_val=xfer_v",
                    session="src",
                    check=True,
                )
                _ = _read_state_report(client, session="src")
                client.session_export(export_path, session="src", check=True)
                client.session_close("src", check=True)

                client.session_new(ephemeral=True, session="dst", check=True)
                r_imp = client.session_import(export_path, session="dst")
                self.assertTrue(r_imp.ok)
                client.open(f"{cfg.base_url}/state", session="dst", check=True)
                rep = _read_state_report(client, session="dst")
                self.assertEqual(rep["serverCookies"].get("mimikyu_normal"), "xfer_ck")
                self.assertEqual(rep["serverCookies"].get("mimikyu_httponly"), "ho_xfer_ck")
                self.assertEqual(rep["localStorage"].get("xfer_k"), "xfer_v")
                client.session_close("dst", check=True)


# ---------------------------------------------------------------------------
# F5: Multi-Context Virtual Isolation (5 tests)
# ---------------------------------------------------------------------------


class TestF5MultiContextVirtualIsolation(unittest.TestCase):
    def test_f5_01_concurrent_virtual_sessions_listed_and_independently_navigable(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            self.assertTrue(client.session_new(virtual=True, session="s1").ok)
            self.assertTrue(client.session_new(virtual=True, session="s2").ok)
            r_list = client.session_list()
            self.assertTrue(r_list.ok)
            self.assertTrue({"s1", "s2"}.issubset(set(_unwrap_sessions(r_list))))

            client.open(f"{cfg.base_url}/nav/page1", session="s1", check=True)
            client.open(f"{cfg.base_url}/nav/page2", session="s2", check=True)
            t1 = client.eval("document.title", session="s1")
            t2 = client.eval("document.title", session="s2")
            self.assertEqual(_unwrap_eval(t1), "Nav Page 1")
            self.assertEqual(_unwrap_eval(t2), "Nav Page 2")

    def test_f5_02_virtual_contexts_isolate_normal_and_httponly_cookies(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(virtual=True, session="s1", check=True)
            client.session_new(virtual=True, session="s2", check=True)
            client.open(f"{cfg.base_url}/state?set_cookie=ck_one&httponly=ho_one", session="s1", check=True)
            _ = _read_state_report(client, session="s1")
            client.open(f"{cfg.base_url}/state?set_cookie=ck_two&httponly=ho_two", session="s2", check=True)
            _ = _read_state_report(client, session="s2")

            client.open(f"{cfg.base_url}/state", session="s1", check=True)
            client.open(f"{cfg.base_url}/state", session="s2", check=True)
            rep1 = _read_state_report(client, session="s1")
            rep2 = _read_state_report(client, session="s2")

            self.assertEqual(rep1["serverCookies"].get("mimikyu_normal"), "ck_one")
            self.assertEqual(rep1["serverCookies"].get("mimikyu_httponly"), "ho_one")
            self.assertEqual(rep2["serverCookies"].get("mimikyu_normal"), "ck_two")
            self.assertEqual(rep2["serverCookies"].get("mimikyu_httponly"), "ho_two")
            self.assertNotIn("ck_two", rep1["documentCookie"])
            self.assertNotIn("ck_one", rep2["documentCookie"])

    def test_f5_03_virtual_contexts_isolate_localstorage_and_sessionstorage(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(virtual=True, session="s1", check=True)
            client.session_new(virtual=True, session="s2", check=True)
            client.open(
                f"{cfg.base_url}/state?ls_key=secret&ls_val=ls_s1&ss_key=tab&ss_val=ss_s1",
                session="s1",
                check=True,
            )
            client.open(
                f"{cfg.base_url}/state?ls_key=secret&ls_val=ls_s2&ss_key=tab&ss_val=ss_s2",
                session="s2",
                check=True,
            )
            rep1 = _read_state_report(client, session="s1")
            rep2 = _read_state_report(client, session="s2")
            self.assertEqual(rep1["localStorage"].get("secret"), "ls_s1")
            self.assertEqual(rep1["sessionStorage"].get("tab"), "ss_s1")
            self.assertEqual(rep2["localStorage"].get("secret"), "ls_s2")
            self.assertEqual(rep2["sessionStorage"].get("tab"), "ss_s2")

            client.open(f"{cfg.base_url}/state", session="s1", check=True)
            rep1_reload = _read_state_report(client, session="s1")
            self.assertEqual(rep1_reload["localStorage"].get("secret"), "ls_s1")

    def test_f5_04_virtual_contexts_isolate_indexeddb(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(virtual=True, session="s1", check=True)
            client.session_new(virtual=True, session="s2", check=True)
            client.open(f"{cfg.base_url}/state?idb_key=vault&idb_val=idb_s1", session="s1", check=True)
            client.open(f"{cfg.base_url}/state?idb_key=vault&idb_val=idb_s2", session="s2", check=True)
            rep1 = _read_state_report(client, session="s1")
            rep2 = _read_state_report(client, session="s2")
            self.assertEqual(rep1["indexedDB"].get("status"), "ok")
            self.assertEqual(rep1["indexedDB"]["entries"].get("vault"), "idb_s1")
            self.assertEqual(rep2["indexedDB"].get("status"), "ok")
            self.assertEqual(rep2["indexedDB"]["entries"].get("vault"), "idb_s2")

            client.session_new(virtual=True, session="s3", check=True)
            client.open(f"{cfg.base_url}/state", session="s3", check=True)
            rep3 = _read_state_report(client, session="s3")
            self.assertEqual(rep3["indexedDB"].get("entries", {}), {})

    def test_f5_05_independent_virtual_session_close_preserves_sibling(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(virtual=True, session="s1", check=True)
            client.session_new(virtual=True, session="s2", check=True)
            client.open(f"{cfg.base_url}/state?set_cookie=c_s1&set_storage=st_s1", session="s1", check=True)
            client.open(f"{cfg.base_url}/state?set_cookie=c_s2&set_storage=st_s2", session="s2", check=True)
            _ = _read_state_report(client, session="s1")
            _ = _read_state_report(client, session="s2")

            r_close1 = client.session_close("s1")
            self.assertTrue(r_close1.ok)
            r_mid = client.session_list(check=True)
            mid_sessions = _unwrap_sessions(r_mid)
            self.assertNotIn("s1", mid_sessions)
            self.assertIn("s2", mid_sessions)

            client.open(f"{cfg.base_url}/state", session="s2", check=True)
            rep2 = _read_state_report(client, session="s2")
            self.assertEqual(rep2["serverCookies"].get("mimikyu_normal"), "c_s2")
            self.assertEqual(rep2["localStorage"].get("token"), "st_s2")

            r_close2 = client.session_close("s2")
            self.assertTrue(r_close2.ok)
            r_end = client.session_list(check=True)
            self.assertEqual(_unwrap_sessions(r_end), [])


# ---------------------------------------------------------------------------
# F6: Coherent Environment Profiles & Validator (6 tests)
# ---------------------------------------------------------------------------


class TestF6CoherentEnvironmentProfiles(unittest.TestCase):
    def test_f6_01_preset_win11_http_and_navigator_coherence(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="win11", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            hdrs = report["httpHeaders"]
            nav = report["navigator"]
            self.assertIn("Windows NT", hdrs["userAgent"] or "")
            self.assertEqual(nav["userAgent"], hdrs["userAgent"])
            self.assertEqual(nav["platform"], "Win32")
            self.assertFalse(nav.get("webdriver"))
            if hdrs.get("secChUaPlatform"):
                self.assertIn("windows", hdrs["secChUaPlatform"].lower())
            if nav.get("userAgentData") and nav["userAgentData"].get("platform"):
                self.assertEqual(nav["userAgentData"]["platform"].lower(), "windows")
                self.assertIs(nav["userAgentData"].get("mobile"), False)

    def test_f6_02_preset_macos_and_linux_profiles(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="macos", session="mac_s", check=True)
            mac_rep = _open_and_read_env_report(client, cfg.base_url, session="mac_s")
            mac_hdrs = mac_rep["httpHeaders"]
            mac_nav = mac_rep["navigator"]
            self.assertIn("Macintosh", mac_hdrs["userAgent"] or "")
            self.assertEqual(mac_nav["userAgent"], mac_hdrs["userAgent"])
            self.assertIn("Mac", mac_nav["platform"] or "")
            if mac_hdrs.get("secChUaPlatform"):
                self.assertIn("mac", mac_hdrs["secChUaPlatform"].lower())
            client.session_close("mac_s", check=True)

            client.session_new(preset="linux", session="lin_s", check=True)
            lin_rep = _open_and_read_env_report(client, cfg.base_url, session="lin_s")
            lin_hdrs = lin_rep["httpHeaders"]
            lin_nav = lin_rep["navigator"]
            self.assertIn("Linux", lin_hdrs["userAgent"] or "")
            self.assertEqual(lin_nav["userAgent"], lin_hdrs["userAgent"])
            self.assertTrue((lin_nav["platform"] or "").startswith("Linux"))
            if lin_hdrs.get("secChUaPlatform"):
                self.assertIn("linux", lin_hdrs["secChUaPlatform"].lower())
            client.session_close("lin_s", check=True)

    def test_f6_03_viewport_and_screen_dpr_coherence(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="macos", viewport="1440x900", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            scr = report["screen"]
            vp = scr["viewport"]
            self.assertEqual(vp["innerWidth"], 1440)
            self.assertEqual(vp["innerHeight"], 900)
            self.assertGreaterEqual(scr["width"], 1440)
            self.assertGreaterEqual(scr["height"], 900)
            self.assertLessEqual(scr["availWidth"], scr["width"])
            self.assertLessEqual(scr["availHeight"], scr["height"])
            self.assertGreaterEqual(scr["colorDepth"], 24)
            self.assertGreaterEqual(scr["devicePixelRatio"], 1)

    def test_f6_04_webgl_and_audiocontext_coherence(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="win11", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            webgl = report["webgl"]
            audio = report["audio"]
            self.assertEqual(webgl.get("status"), "ok")
            self.assertTrue(isinstance(webgl.get("vendor"), str) and len(webgl["vendor"].strip()) > 0)
            self.assertTrue(isinstance(webgl.get("renderer"), str) and len(webgl["renderer"].strip()) > 0)
            self.assertTrue(isinstance(webgl.get("version"), str) and len(webgl["version"].strip()) > 0)
            if webgl.get("unmaskedVendor") is not None:
                self.assertGreater(len(str(webgl["unmaskedVendor"]).strip()), 0)
            if webgl.get("unmaskedRenderer") is not None:
                self.assertGreater(len(str(webgl["unmaskedRenderer"]).strip()), 0)

            self.assertEqual(audio.get("status"), "ok")
            self.assertIn(audio.get("sampleRate"), (44100, 48000))
            self.assertGreaterEqual(audio.get("channelCount") or 0, 2)

    def test_f6_05_worker_navigator_consistency(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="win11", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            nav = report["navigator"]
            worker = report["worker"]
            wc = report["workerConsistency"]
            self.assertEqual(worker.get("status"), "ok")
            self.assertEqual(worker.get("userAgent"), nav["userAgent"])
            self.assertEqual(worker.get("platform"), nav["platform"])
            self.assertEqual(worker.get("language"), nav["language"])
            self.assertIs(wc.get("consistent"), True)
            self.assertEqual(wc.get("mismatches"), [])

    def test_f6_06_echo_headers_matches_navigator_ua(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="linux", check=True)
            client.open(f"{cfg.base_url}/echo-headers", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            payload = _unwrap_json_eval(client.eval("document.body.innerText", check=True))
            nav_ua = _unwrap_eval(client.eval("navigator.userAgent", check=True))
            self.assertEqual(payload["method"], "GET")
            ua_hdr = payload["headers"].get("user-agent", "")
            self.assertIn("Linux", ua_hdr)
            self.assertEqual(ua_hdr, nav_ua)


# ---------------------------------------------------------------------------
# F7: Proxy, GeoIP/Region, Locale, TZ & WebRTC (6 tests)
# ---------------------------------------------------------------------------


class TestF7ProxyGeoipLocaleTimezoneWebrtc(unittest.TestCase):
    def test_f7_01_proxy_routing_observes_traffic(self) -> None:
        cfg = get_config()
        with LocalHttpProxy(allow_external=False) as proxy:
            with MimikyuClient() as client:
                client.session_new(proxy=proxy.proxy_url, check=True)
                res = client.open(f"{cfg.base_url}/echo-headers", check=True)
                self.assertTrue(res.ok)
                client.wait(idle=True, timeout_ms=5000, check=True)
                payload = _unwrap_json_eval(client.eval("document.body.innerText", check=True))
                self.assertEqual(payload["method"], "GET")
                self.assertGreaterEqual(proxy.request_count, 1)
                self.assertTrue(
                    any("/echo-headers" in u or "127.0.0.1:" in u for u in proxy.seen_urls),
                    f"Expected loopback URL in proxy.seen_urls, got: {proxy.seen_urls}",
                )

    def test_f7_02_locale_aligns_accept_language_and_navigator(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(locale="fr-FR", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            nav = report["navigator"]
            hdrs = report["httpHeaders"]
            worker = report["worker"]
            self.assertTrue(
                nav["language"] == "fr-FR" or str(nav["language"]).lower().startswith("fr"),
                f"Unexpected navigator.language: {nav['language']!r}",
            )
            self.assertGreater(len(nav["languages"]), 0)
            self.assertTrue(str(nav["languages"][0]).lower().startswith("fr"))
            self.assertIn("fr", (hdrs.get("acceptLanguage") or "").lower())
            self.assertEqual(worker.get("status"), "ok")
            self.assertTrue(str(worker.get("language", "")).lower().startswith("fr"))

    def test_f7_03_timezone_aligns_intl_and_worker(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(timezone="Asia/Tokyo", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            intl = report["intl"]
            worker = report["worker"]
            wc = report["workerConsistency"]
            self.assertEqual(intl["timezone"], "Asia/Tokyo")
            self.assertEqual(intl["timezoneOffsetMinutes"], -540)
            self.assertEqual(worker.get("status"), "ok")
            self.assertEqual(worker.get("timezone"), "Asia/Tokyo")
            self.assertIs(wc.get("consistent"), True)

    def test_f7_04_geoip_region_mapping_aligns_env(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            res = client.session_new(geoip="8.8.8.8")
            self.assertTrue(res.ok)
            report = _open_and_read_env_report(client, cfg.base_url)
            intl = report["intl"]
            nav = report["navigator"]
            hdrs = report["httpHeaders"]
            worker = report["worker"]
            wc = report["workerConsistency"]
            self.assertIn("/", intl["timezone"])
            self.assertTrue(isinstance(nav["language"], str) and len(nav["language"]) >= 2)
            lang_prefix = nav["language"].split("-")[0].lower()
            self.assertIn(lang_prefix, (hdrs.get("acceptLanguage") or "").lower())
            self.assertEqual(worker.get("status"), "ok")
            self.assertIs(wc.get("consistent"), True)

    def test_f7_05_webrtc_host_candidate_policy(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(preset="macos", check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            webrtc = report["webrtc"]
            self.assertIn(webrtc.get("status"), ("ok", "blocked", "timeout", "unavailable"))
            self.assertIs(webrtc.get("hasHostCandidate"), False)
            self.assertNotIn("host", webrtc.get("candidateTypes", []))

    def test_f7_06_cluster_proxy_forwards_dual_origin(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(proxy=cfg.proxy_url, check=True)
            r1 = client.open(f"{cfg.base_url}/echo-headers")
            self.assertTrue(r1.ok)
            p1 = _unwrap_json_eval(client.eval("document.body.innerText", check=True))
            self.assertEqual(p1["method"], "GET")

            r2 = client.open(f"{cfg.cross_url}/echo-headers")
            self.assertTrue(r2.ok)
            p2 = _unwrap_json_eval(client.eval("document.body.innerText", check=True))
            self.assertEqual(p2["method"], "GET")


# ---------------------------------------------------------------------------
# F8: Resource Blocker & Telemetry Buffers (6 tests)
# ---------------------------------------------------------------------------


class TestF8ResourceBlockerAndTelemetry(unittest.TestCase):
    def test_f8_01_block_images_prevents_image_load(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(block_images=True, check=True)
            client.open(f"{cfg.base_url}/workflow/download-verify", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            asset_state = _unwrap_json_eval(
                client.eval("document.getElementById('asset-status').textContent", check=True)
            )
            nat_w = _unwrap_eval(client.eval("document.getElementById('verify-img').naturalWidth", check=True))
            self.assertEqual(asset_state.get("img"), "blocked_or_error")
            self.assertEqual(nat_w, 0)

    def test_f8_02_block_media_prevents_video_load(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(block_media=True, check=True)
            client.open(f"{cfg.base_url}/workflow/download-verify", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            asset_state = _unwrap_json_eval(
                client.eval("document.getElementById('asset-status').textContent", check=True)
            )
            ready_state = _unwrap_eval(
                client.eval("document.getElementById('verify-media').readyState", check=True)
            )
            self.assertNotEqual(asset_state.get("media"), "loaded")
            self.assertEqual(ready_state, 0)

    def test_f8_03_block_fonts_prevents_woff2_load(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(block_fonts=True, check=True)
            client.open(f"{cfg.base_url}/workflow/download-verify", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            r_font = client.eval(
                "(() => {"
                " let el = document.getElementById('font-probe-status');"
                " if (!el) {"
                " el = document.createElement('div');"
                " el.id = 'font-probe-status';"
                " document.body.appendChild(el);"
                " }"
                " el.removeAttribute('data-done');"
                " const finish = (count) => {"
                " const loaded = Array.from(document.fonts).filter("
                " f => String(f.family || '').includes('VerifyFont') && f.status === 'loaded'"
                " ).length;"
                " el.dataset.count = String(count);"
                " el.dataset.loaded = String(loaded);"
                " el.dataset.done = '1';"
                " return count;"
                " };"
                " return document.fonts.load('16px VerifyFont').then("
                " faces => finish(Array.isArray(faces) ? faces.length : 0),"
                " () => finish(-1)"
                " );"
                "})()",
                check=True,
            )
            loaded_faces = _unwrap_eval(r_font)
            if isinstance(loaded_faces, (int, float)) and not isinstance(loaded_faces, bool):
                self.assertTrue(
                    math.isfinite(float(loaded_faces)),
                    f"Non-finite font face count from eval: {loaded_faces!r}",
                )
                self.assertLessEqual(loaded_faces, 0)
            else:
                client.wait(selector="#font-probe-status[data-done='1']", timeout_ms=5000, check=True)
                r_probe = client.eval(
                    "(() => {"
                    " const el = document.getElementById('font-probe-status');"
                    " const loadedFonts = Array.from(document.fonts).filter("
                    " f => String(f.family || '').includes('VerifyFont') && f.status === 'loaded'"
                    " ).length;"
                    " return JSON.stringify({"
                    " done: el ? el.dataset.done : null,"
                    " count: el ? Number(el.dataset.count) : null,"
                    " loaded: el ? Number(el.dataset.loaded) : null,"
                    " loadedFonts: loadedFonts"
                    " });"
                    "})()",
                    check=True,
                )
                probe = _unwrap_json_eval(r_probe)
                if not isinstance(probe, dict) or probe.get("done") != "1":
                    self.fail(f"Font load probe did not complete with verifiable status: {probe!r}")
                face_count = probe.get("count")
                loaded_attr = probe.get("loaded")
                loaded_count = probe.get("loadedFonts")
                if (
                    not isinstance(face_count, (int, float))
                    or isinstance(face_count, bool)
                    or not math.isfinite(float(face_count))
                    or not isinstance(loaded_attr, (int, float))
                    or isinstance(loaded_attr, bool)
                    or not math.isfinite(float(loaded_attr))
                    or not isinstance(loaded_count, (int, float))
                    or isinstance(loaded_count, bool)
                    or not math.isfinite(float(loaded_count))
                ):
                    self.fail(f"Font load probe missing numeric verification fields: {probe!r}")
                self.assertLessEqual(face_count, 0)
                self.assertEqual(loaded_attr, 0)
                self.assertEqual(loaded_count, 0)

            r_net = client.logs("network", check=True)
            for entry in _iter_network_entries_for_url(r_net.data, "/assets/font.woff2"):
                self.assertNotEqual(entry.get("status"), 200)
                self.assertNotEqual(entry.get("status_code"), 200)
                resp = entry.get("response")
                if isinstance(resp, dict):
                    self.assertNotEqual(resp.get("status"), 200)

    def test_f8_04_logs_console_captures_log_warn_error(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/console-errors", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            r_con = client.logs("console", check=True)
            serialized = json.dumps(r_con.data)
            self.assertIn("mimikyu-e2e-log", serialized)
            self.assertIn("mimikyu-e2e-warn", serialized)
            self.assertIn("mimikyu-e2e-error", serialized)

    def test_f8_05_logs_network_captures_api_data_requests(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/console-errors?assets=1", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            r_net = client.logs("network", check=True)
            serialized = json.dumps(r_net.data)
            self.assertIn("/console-errors", serialized)
            self.assertIn("/assets/img.png", serialized)
            self.assertIn("/api/data", serialized)

    def test_f8_06_telemetry_buffers_bounded_and_retrievable(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/console-errors", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            client.eval(
                "(() => { for (let i = 0; i < 150; i++) { console.log('burst-' + i); } "
                "return Promise.all(Array.from({length: 10}, (_, i) => "
                "fetch('/api/data?i=' + i).then(r => r.status))).then(s => s.length); })()",
                check=True,
            )
            client.wait(idle=True, timeout_ms=5000, check=True)
            r_con = client.logs("console")
            r_net = client.logs("network")
            self.assertTrue(r_con.ok)
            self.assertTrue(r_net.ok)
            con_entries = _unwrap_log_entries(r_con)
            net_entries = _unwrap_log_entries(r_net)
            self.assertGreater(len(con_entries), 0)
            self.assertGreater(len(net_entries), 0)
            self.assertIn("burst-149", json.dumps(r_con.data))
            self.assertIn("/api/data", json.dumps(r_net.data))


# ---------------------------------------------------------------------------
# F9: Isolated-World & Opt-in Main-World Eval (6 tests)
# ---------------------------------------------------------------------------


class TestF9IsolatedAndMainWorldEval(unittest.TestCase):
    def test_f9_01_isolated_eval_default_zero_page_leakage(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            report = _open_and_read_env_report(client, cfg.base_url)
            iso = report["isolatedWorld"]
            self.assertIs(iso["isolatedWorldClean"], True)
            self.assertEqual(iso["mimikyuGlobalType"], "undefined")
            self.assertEqual(iso["leakedKeys"], [])

            r_iso = client.eval("window.__ISOLATED_PROBE = 99; typeof window.__ENV_REPORT", check=True)
            self.assertEqual(_unwrap_eval(r_iso), "undefined")

            r_mw = client.eval("typeof window.__ISOLATED_PROBE", main_world=True, check=True)
            self.assertEqual(_unwrap_eval(r_mw), "undefined")

    def test_f9_02_isolated_eval_reads_and_mutates_dom(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)

            initial_h1 = _unwrap_eval(client.eval("document.querySelector('h1').textContent", check=True))
            self.assertEqual(initial_h1, "Test Form")

            mutated_val = _unwrap_eval(
                client.eval(
                    "(() => { document.getElementById('name').value = 'Ada'; "
                    "document.querySelector('h1').textContent = 'Updated Form'; "
                    "return document.getElementById('name').value; })()",
                    check=True,
                )
            )
            self.assertEqual(mutated_val, "Ada")
            self.assertIn("Updated Form", _unwrap_extract(client.extract("text", check=True)))

    def test_f9_03_main_world_eval_reads_page_js_globals(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            _ = _open_and_read_env_report(client, cfg.base_url)
            status = _unwrap_eval(
                client.eval("window.__ENV_REPORT && window.__ENV_REPORT.status", main_world=True, check=True)
            )
            clean = _unwrap_eval(
                client.eval(
                    "window.__ENV_REPORT.isolatedWorld.isolatedWorldClean",
                    main_world=True,
                    check=True,
                )
            )
            self.assertEqual(status, "ready")
            self.assertIs(clean, True)

    def test_f9_04_same_origin_iframe_dom_and_global_eval(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/frames?cross_origin={cfg.cross_url}", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            same_out = _unwrap_eval(
                client.eval(
                    "(() => { var f = document.getElementById('same-frame').contentDocument; "
                    "f.getElementById('same-frame-input').value = 'from-host'; "
                    "f.getElementById('same-frame-btn').click(); "
                    "return f.getElementById('same-frame-out').textContent; })()",
                    check=True,
                )
            )
            self.assertEqual(same_out, "from-host")
            client.wait(text="from-host", timeout_ms=3000, check=True)
            host_msg = _unwrap_eval(client.eval("document.getElementById('host-msg').textContent", check=True))
            self.assertIn("same-frame", str(host_msg))
            self.assertIn("from-host", str(host_msg))

            frame_marker = _unwrap_eval(
                client.eval(
                    "document.getElementById('same-frame').contentWindow.__FRAME_MARKER.frame",
                    main_world=True,
                    check=True,
                )
            )
            self.assertEqual(frame_marker, "same")

    def test_f9_05_closed_shadow_root_and_nested_frame_traversal(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/workflow/cross-frame", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            shadow_null = _unwrap_eval(
                client.eval(
                    "document.getElementById('closed-shadow-host').shadowRoot === null",
                    main_world=True,
                    check=True,
                )
            )
            self.assertIs(shadow_null, True)
            snap = client.snapshot(check=True)
            snap_blob = json.dumps(snap.payload if snap.payload is not None else snap.stdout)
            self.assertIn("closed-shadow-ready", snap_blob)
            self.assertTrue(
                "Same Frame Note" in snap_blob or "Cross Frame Note" in snap_blob,
                f"Expected nested frame controls in snapshot: {snap_blob}",
            )

    def test_f9_06_spa_main_world_vs_isolated_world_isolation(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/spa?delay_ms=10", check=True)
            client.wait(text="loaded", timeout_ms=5000, check=True)
            client.eval("window.__SPA_SECRET = 'mw_ok'", main_world=True, check=True)

            iso_type = _unwrap_eval(client.eval("typeof window.__SPA_SECRET", check=True))
            mw_val = _unwrap_eval(client.eval("window.__SPA_SECRET", main_world=True, check=True))
            li_count = _unwrap_eval(
                client.eval("document.querySelectorAll('#spa-list li').length", check=True)
            )
            self.assertEqual(iso_type, "undefined")
            self.assertEqual(mw_val, "mw_ok")
            self.assertEqual(li_count, 3)


# ---------------------------------------------------------------------------
# F10: Compact @eN Semantic Snapshots & Diff (6 tests)
# ---------------------------------------------------------------------------


class TestF10CompactSnapshotsAndDiff(unittest.TestCase):
    def test_f10_01_snapshot_form_compact_refs_and_aria_roles(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            snap = client.snapshot(check=True)
            all_refs = extract_all_ax_refs(snap)
            self.assertGreaterEqual(len(all_refs), 5)
            self.assertIsNotNone(extract_ax_ref(snap, role="textbox", name_substr="Name"))
            self.assertIsNotNone(extract_ax_ref(snap, name_substr="Role"))
            self.assertIsNotNone(extract_ax_ref(snap, role="checkbox", name_substr="Agree"))
            self.assertIsNotNone(extract_ax_ref(snap, role="radio", name_substr="Fast"))
            self.assertIsNotNone(extract_ax_ref(snap, role="button", name_substr="Submit"))

    def test_f10_02_snapshot_spa_hydrated_roles_and_names(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/spa?delay_ms=30", check=True)
            client.wait(text="loaded", timeout_ms=5000, check=True)
            snap = client.snapshot(check=True)
            self.assertIsNotNone(extract_ax_ref(snap, role="button", name_substr="Open Modal"))
            self.assertIsNotNone(extract_ax_ref(snap, name_substr="Category Filter"))
            blob = json.dumps(snap.payload if snap.payload is not None else snap.stdout)
            for item in ("Alpha (core)", "Beta (addon)", "Gamma (core)"):
                self.assertIn(item, blob)

    def test_f10_03_snapshot_interactive_only_filters_static_nodes(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            full_snap = client.snapshot(interactive_only=False, check=True)
            inter_snap = client.snapshot(interactive_only=True, check=True)
            inter_refs = extract_all_ax_refs(inter_snap)
            full_refs = extract_all_ax_refs(full_snap)
            self.assertGreaterEqual(len(inter_refs), 5)
            self.assertLessEqual(len(inter_refs), len(full_refs))
            self.assertIsNotNone(extract_ax_ref(inter_snap, role="button", name_substr="Submit"))
            self.assertIsNotNone(extract_ax_ref(inter_snap, role="textbox", name_substr="Name"))
            self.assertIsNone(extract_ax_ref(inter_snap, role="heading", name_substr="Test Form"))

    def test_f10_04_snapshot_max_depth_bounds_tree(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            shallow = client.snapshot(max_depth=1, check=True)
            deep = client.snapshot(max_depth=10, check=True)
            self.assertLess(len(extract_all_ax_refs(shallow)), len(extract_all_ax_refs(deep)))

    def test_f10_05_snapshot_diff_detects_spa_modal_change(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/spa?delay_ms=20", check=True)
            client.wait(text="loaded", timeout_ms=5000, check=True)
            snap1 = client.snapshot(check=True)
            open_btn = extract_ax_ref(snap1, role="button", name_substr="Open Modal")
            self.assertIsNotNone(open_btn)
            assert open_btn is not None
            client.click(open_btn, check=True)
            diff_res = client.snapshot(diff=True, check=True)
            self.assertTrue(diff_res.ok)
            diff_blob = json.dumps(diff_res.data)
            self.assertTrue(
                "Close Modal" in diff_blob or "SPA Dialog Content" in diff_blob,
                f"Expected modal change in snapshot diff: {diff_blob}",
            )

    def test_f10_06_snapshot_refs_drive_form_submission(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            snap = client.snapshot(interactive_only=True, check=True)
            name_ref = extract_ax_ref(snap, role="textbox", name_substr="Name")
            submit_ref = extract_ax_ref(snap, role="button", name_substr="Submit")
            self.assertIsNotNone(name_ref)
            self.assertIsNotNone(submit_ref)
            assert name_ref is not None and submit_ref is not None
            client.fill(name_ref, "Mimikyu Agent", check=True)
            client.click(submit_ref, check=True)
            client.wait(url="/submit", timeout_ms=5000, check=True)
            self.assertIn("Mimikyu Agent", _unwrap_extract(client.extract("text", check=True)))


# ---------------------------------------------------------------------------
# F11: Content Extraction, Media & Animations (7 tests)
# ---------------------------------------------------------------------------


class TestF11ContentExtractionMediaAnimations(unittest.TestCase):
    def test_f11_01_extract_markdown_catalog(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/workflow/catalog", check=True)
            client.wait(text="Node Alpha", timeout_ms=5000, check=True)
            res = client.extract("markdown", check=True)
            self.assertTrue(res.ok)
            md = _unwrap_extract(res)
            for token in ("Dynamic Resource Catalog", "Node Alpha", "Block Vault", "Edge Router"):
                self.assertIn(token, md)

    def test_f11_02_extract_text_catalog(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/workflow/catalog", check=True)
            client.wait(text="ready", timeout_ms=5000, check=True)
            res = client.extract("text", check=True)
            self.assertTrue(res.ok)
            txt = _unwrap_extract(res)
            for token in ("Dynamic Resource Catalog", "Node Alpha", "GPU Worker", "Page 1 of 2"):
                self.assertIn(token, txt)

    def test_f11_03_extract_tables_catalog(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/workflow/catalog", check=True)
            client.wait(text="Node Alpha", timeout_ms=5000, check=True)
            res = client.extract("tables", check=True)
            self.assertTrue(res.ok)
            blob = json.dumps(res.data)
            for token in ("ID", "Name", "Category", "Region", "Price", "Node Alpha", "compute", "us-east", "GPU Worker", "65"):
                self.assertIn(token, blob)

    def test_f11_04_extract_links_download_verify(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/workflow/download-verify", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            res = client.extract("links", check=True)
            self.assertTrue(res.ok)
            blob = json.dumps(res.data)
            for token in ("/download/sample.txt", "/download/sample.bin", "/download/sha256", "Download sample.txt"):
                self.assertIn(token, blob)

    def test_f11_05_screenshot_viewport_target_and_full_page(self) -> None:
        cfg = get_config()
        with tempfile.TemporaryDirectory() as tmp:
            tmp_dir = Path(tmp)
            vp_png = tmp_dir / "viewport.png"
            el_png = tmp_dir / "element.png"
            fp_png = tmp_dir / "fullpage.png"
            with MimikyuClient() as client:
                client.session_new(check=True)
                client.open(f"{cfg.base_url}/workflow/download-verify", check=True)
                client.wait(idle=True, timeout_ms=5000, check=True)
                snap = client.snapshot(check=True)
                ref = extract_ax_ref(snap, name_substr="Download sample.txt") or extract_ax_ref(snap)
                self.assertIsNotNone(ref)
                assert ref is not None
                self.assertTrue(ref.startswith("@e"))

                client.screenshot(vp_png, check=True)
                client.screenshot(el_png, target=ref, check=True)
                client.screenshot(fp_png, full_page=True, check=True)

            dims: list[tuple[int, int]] = []
            for p in (vp_png, el_png, fp_png):
                self.assertTrue(p.is_file())
                raw = p.read_bytes()
                self.assertGreater(len(raw), 100)
                self.assertTrue(raw.startswith(b"\x89PNG\r\n\x1a\n"))
                self.assertEqual(raw[12:16], b"IHDR")
                w, h = struct.unpack(">II", raw[16:24])
                self.assertGreater(w, 0)
                self.assertGreater(h, 0)
                dims.append((w, h))

            (w_vp, h_vp), (w_el, h_el), _ = dims
            self.assertLessEqual(w_el, w_vp)
            self.assertLessEqual(h_el, h_vp)

    def test_f11_06_pdf_export_valid_header(self) -> None:
        cfg = get_config()
        with tempfile.TemporaryDirectory() as tmp:
            out_pdf = Path(tmp) / "artifact.pdf"
            with MimikyuClient() as client:
                client.session_new(check=True)
                client.open(f"{cfg.base_url}/workflow/download-verify", check=True)
                client.wait(idle=True, timeout_ms=5000, check=True)
                client.pdf(out_pdf, check=True)
            self.assertTrue(out_pdf.is_file())
            raw = out_pdf.read_bytes()
            self.assertGreater(len(raw), 200)
            self.assertTrue(raw.startswith(b"%PDF-"))
            self.assertIn(b"%%EOF", raw[-128:])

    def test_f11_07_instant_animations_completes_fast(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(instant_animations=True, check=True)
            client.open(f"{cfg.base_url}/nav/page1", check=True)
            t0 = time.monotonic()
            client.eval(
                "(() => {"
                " const st = document.createElement('style');"
                " st.textContent = '@keyframes mk_fade { from { opacity: 0; } to { opacity: 1; } } "
                "#anim-kf { opacity: 0; animation: mk_fade 30s linear 1 forwards; } "
                "#anim-tr { width: 10px; transition: width 30s linear; } "
                "#anim-tr.go { width: 210px; }';"
                " document.head.appendChild(st);"
                " const kf = document.createElement('div'); kf.id = 'anim-kf'; kf.textContent = 'kf';"
                " const tr = document.createElement('div'); tr.id = 'anim-tr'; tr.textContent = 'tr';"
                " document.body.appendChild(kf); document.body.appendChild(tr);"
                " void tr.offsetWidth;"
                " tr.classList.add('go');"
                " return true;"
                "})()",
                check=True,
            )
            time.sleep(0.2)
            state = _unwrap_json_eval(
                client.eval(
                    "JSON.stringify({"
                    " opacity: getComputedStyle(document.getElementById('anim-kf')).opacity,"
                    " width: getComputedStyle(document.getElementById('anim-tr')).width,"
                    " states: document.getAnimations().map(a => a.playState)"
                    "})",
                    check=True,
                )
            )
            elapsed = time.monotonic() - t0
            self.assertLess(elapsed, 3.0)
            self.assertGreaterEqual(float(state["opacity"]), 0.99)
            self.assertEqual(state["width"], "210px")


# ---------------------------------------------------------------------------
# F12: Deterministic Waits & Action Batching (6 tests)
# ---------------------------------------------------------------------------


class TestF12DeterministicWaitsAndBatching(unittest.TestCase):
    def test_f12_01_wait_idle_after_delayed_spa_fetch(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/spa?delay_ms=300", check=True)
            res = client.wait(idle=True, timeout_ms=5000, check=True)
            self.assertTrue(res.ok)
            txt = _unwrap_extract(client.extract("text", check=True))
            self.assertIn("loaded", txt)
            self.assertIn("Alpha (core) - $10", txt)
            self.assertIn("Gamma (core) - $40", txt)

    def test_f12_02_wait_text_delayed_hydration(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/spa?delay_ms=350", check=True)
            res = client.wait(text="Gamma", timeout_ms=5000, check=True)
            self.assertTrue(res.ok)
            status = _unwrap_eval(client.eval("document.getElementById('spa-status').textContent", check=True))
            self.assertEqual(status, "loaded")

    def test_f12_03_wait_selector_dynamic_list_item(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/spa?delay_ms=250", check=True)
            res = client.wait(selector="#spa-list li", timeout_ms=5000, check=True)
            self.assertTrue(res.ok)
            count = _unwrap_eval(client.eval("document.querySelectorAll('#spa-list li').length", check=True))
            self.assertEqual(count, 3)

    def test_f12_04_wait_url_pushstate_and_redirect(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/history", check=True)
            snap = client.snapshot(check=True)
            btn = extract_ax_ref(snap, role="button", name_substr="Push Step 1")
            self.assertIsNotNone(btn)
            assert btn is not None
            client.click(btn, check=True)
            res = client.wait(url="step=1", timeout_ms=3000, check=True)
            self.assertTrue(res.ok)
            self.assertEqual(_unwrap_eval(client.eval("window.location.search", check=True)), "?step=1")

    def test_f12_05_batch_ops_inline_ordered_form_submit(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            snap = client.snapshot(check=True)
            name_ref = extract_ax_ref(snap, role="textbox", name_substr="Name") or "#name"
            role_ref = extract_ax_ref(snap, name_substr="Role") or "#role"
            agree_ref = extract_ax_ref(snap, role="checkbox", name_substr="Agree") or "#agree"
            submit_ref = extract_ax_ref(snap, role="button", name_substr="Submit") or "#submit-btn"

            ops = [
                {"op": "fill", "target": name_ref, "value": "BatchInline"},
                {"op": "select", "target": role_ref, "value": "agent"},
                {"op": "click", "target": agree_ref},
                {"op": "click", "target": submit_ref},
            ]
            client.batch(ops=ops, check=True)
            client.wait(url="/submit", timeout_ms=5000, check=True)
            submitted = _unwrap_json_eval(
                client.eval("document.getElementById('submit-json').textContent", check=True)
            )
            self.assertEqual(submitted["fields"].get("name"), "BatchInline")
            self.assertEqual(submitted["fields"].get("role"), "agent")
            self.assertEqual(submitted["fields"].get("agree"), "on")

    def test_f12_06_batch_file_ordered_execution(self) -> None:
        cfg = get_config()
        with tempfile.TemporaryDirectory() as tmp:
            ops_path = Path(tmp) / "ops.json"
            with MimikyuClient() as client:
                client.session_new(check=True)
                client.open(f"{cfg.base_url}/form", check=True)
                snap = client.snapshot(check=True)
                name_ref = extract_ax_ref(snap, role="textbox", name_substr="Name") or "#name"
                role_ref = extract_ax_ref(snap, name_substr="Role") or "#role"
                accurate_ref = extract_ax_ref(snap, role="radio", name_substr="Accurate") or "#tier-accurate"
                submit_ref = extract_ax_ref(snap, role="button", name_substr="Submit") or "#submit-btn"

                ops = [
                    {"op": "fill", "target": name_ref, "value": "BatchFile"},
                    {"op": "select", "target": role_ref, "value": "viewer"},
                    {"op": "click", "target": accurate_ref},
                    {"op": "click", "target": submit_ref},
                ]
                ops_path.write_text(json.dumps(ops), encoding="utf-8")
                client.batch(file_path=ops_path, check=True)
                client.wait(url="/submit", timeout_ms=5000, check=True)
                submitted = _unwrap_json_eval(
                    client.eval("document.getElementById('submit-json').textContent", check=True)
                )
                self.assertEqual(submitted["fields"].get("name"), "BatchFile")
                self.assertEqual(submitted["fields"].get("role"), "viewer")
                self.assertEqual(submitted["fields"].get("tier"), "accurate")


# ---------------------------------------------------------------------------
# F13: Chokepoint Input & Kinematic humanize (6 tests)
# ---------------------------------------------------------------------------


class TestF13ChokepointInputAndHumanize(unittest.TestCase):
    def test_f13_01_click_fill_select_form_submission(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/form", check=True)
            snap = client.snapshot(check=True)
            name_ref = extract_ax_ref(snap, role="textbox", name_substr="Name") or "#name"
            role_ref = extract_ax_ref(snap, name_substr="Role") or "#role"
            agree_ref = extract_ax_ref(snap, role="checkbox", name_substr="Agree") or "#agree"
            accurate_ref = extract_ax_ref(snap, role="radio", name_substr="Accurate") or "#tier-accurate"
            submit_ref = extract_ax_ref(snap, role="button", name_substr="Submit") or "#submit-btn"

            client.fill(name_ref, "Alice Chokepoint", check=True)
            client.select(role_ref, "admin", check=True)
            client.click(agree_ref, check=True)
            client.click(accurate_ref, check=True)
            client.click(submit_ref, check=True)
            client.wait(url="/submit", timeout_ms=5000, check=True)

            submitted = _unwrap_json_eval(
                client.eval("document.getElementById('submit-json').textContent", check=True)
            )
            self.assertEqual(submitted["fields"].get("name"), "Alice Chokepoint")
            self.assertEqual(submitted["fields"].get("role"), "admin")
            self.assertEqual(submitted["fields"].get("agree"), "on")
            self.assertEqual(submitted["fields"].get("tier"), "accurate")

    def test_f13_02_type_and_press_keys_recorded(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/humanize-tracker", check=True)
            snap = client.snapshot(check=True)
            input_ref = extract_ax_ref(snap, role="textbox", name_substr="Tracker Input") or "#tracker-input"
            client.type(input_ref, "abc", check=True)
            client.press("Enter", check=True)

            tracker = _unwrap_json_eval(
                client.eval("document.getElementById('tracker-json').textContent", check=True)
            )
            input_val = _unwrap_eval(client.eval("document.getElementById('tracker-input').value", check=True))
            self.assertEqual(input_val, "abc")
            keys_down = [ev.get("key") for ev in tracker["events"] if ev.get("type") == "keydown"]
            for ch in ("a", "b", "c", "Enter"):
                self.assertIn(ch, keys_down)
            self.assertGreaterEqual(tracker["counts"].get("keyup", 0), 4)

    def test_f13_03_upload_file_multipart_sha256(self) -> None:
        cfg = get_config()
        payload = b"mimikyu-tier1-upload-v1\n"
        expected_sha256 = hashlib.sha256(payload).hexdigest()
        with tempfile.TemporaryDirectory() as tmp:
            upload_path = Path(tmp) / "upload_payload.txt"
            upload_path.write_bytes(payload)
            with MimikyuClient() as client:
                client.session_new(check=True)
                client.open(f"{cfg.base_url}/form", check=True)
                snap = client.snapshot(check=True)
                upload_ref = extract_ax_ref(snap, name_substr="Upload File") or "#upload"
                name_ref = extract_ax_ref(snap, role="textbox", name_substr="Name") or "#name"
                submit_ref = extract_ax_ref(snap, role="button", name_substr="Submit") or "#submit-btn"

                client.upload(upload_ref, upload_path, check=True)
                client.fill(name_ref, "FileSender", check=True)
                client.click(submit_ref, check=True)
                client.wait(url="/submit", timeout_ms=5000, check=True)

                submitted = _unwrap_json_eval(
                    client.eval("document.getElementById('submit-json').textContent", check=True)
                )
                self.assertEqual(len(submitted["files"]), 1)
                self.assertEqual(submitted["files"][0]["filename"], "upload_payload.txt")
                self.assertEqual(submitted["files"][0]["size"], len(payload))
                self.assertEqual(submitted["files"][0]["sha256"], expected_sha256)

    def test_f13_04_scroll_positive_and_negative_offsets(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(viewport="640x300", check=True)
            client.open(f"{cfg.base_url}/", check=True)
            client.wait(idle=True, timeout_ms=5000, check=True)
            y0 = _unwrap_eval(client.eval("window.scrollY", check=True))
            self.assertEqual(y0, 0)

            client.scroll(0, 260, check=True)
            y_down = _unwrap_eval(client.eval("window.scrollY", check=True))
            self.assertGreater(y_down, 0)

            client.scroll(0, -140, check=True)
            y_up = _unwrap_eval(client.eval("window.scrollY", check=True))
            self.assertLess(y_up, y_down)

    def test_f13_05_humanize_click_multi_step_mouse_trajectory(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/humanize-tracker", check=True)
            snap = client.snapshot(check=True)
            start_ref = extract_ax_ref(snap, role="button", name_substr="Start Point") or "#start-btn"
            reset_ref = extract_ax_ref(snap, role="button", name_substr="Reset Events") or "#reset-btn"
            target_ref = extract_ax_ref(snap, role="button", name_substr="Target Button") or "#target-btn"

            client.click(start_ref, check=True)
            client.click(reset_ref, check=True)
            client.click(target_ref, humanize=True, check=True)

            tracker = _unwrap_json_eval(
                client.eval("document.getElementById('tracker-json').textContent", check=True)
            )
            counts = tracker["counts"]
            events = tracker["events"]
            self.assertGreaterEqual(counts.get("mousemove", 0), 2)
            self.assertGreaterEqual(counts.get("mousedown", 0), 1)
            self.assertGreaterEqual(counts.get("mouseup", 0), 1)

            moves = [ev for ev in events if ev.get("type") == "mousemove"]
            coords = {(ev.get("x"), ev.get("y")) for ev in moves}
            self.assertGreaterEqual(len(coords), 2)
            times = [ev["t"] for ev in events]
            self.assertEqual(times, sorted(times))
            clicks = [ev for ev in events if ev.get("type") in ("mousedown", "mouseup")]
            self.assertTrue(any(ev.get("targetId") == "target-btn" for ev in clicks))

    def test_f13_06_humanize_type_multi_step_key_events(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/humanize-tracker", check=True)
            snap = client.snapshot(check=True)
            input_ref = extract_ax_ref(snap, role="textbox", name_substr="Tracker Input") or "#tracker-input"
            client.type(input_ref, "kinematic", humanize=True, check=True)

            tracker = _unwrap_json_eval(
                client.eval("document.getElementById('tracker-json').textContent", check=True)
            )
            input_val = _unwrap_eval(client.eval("document.getElementById('tracker-input').value", check=True))
            self.assertEqual(input_val, "kinematic")
            self.assertGreaterEqual(tracker["counts"].get("keydown", 0), 9)
            self.assertGreaterEqual(tracker["counts"].get("keyup", 0), 9)
            key_times = [ev["t"] for ev in tracker["events"] if ev.get("type") in ("keydown", "keyup")]
            self.assertGreater(max(key_times), min(key_times))


# ---------------------------------------------------------------------------
# F14: Warm Daemon, CLI & Stdio MCP Server (5 tests)
# ---------------------------------------------------------------------------


class TestF14DaemonCliAndMcpServer(unittest.TestCase):
    def test_f14_01_daemon_start_and_reuse_across_cli_calls(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            d_res = client.daemon(idle_timeout=30, check=True)
            self.assertTrue(d_res.ok)
            client.session_new(session="daemon_s1", check=True)
            client.open(f"{cfg.base_url}/nav/page1", session="daemon_s1", check=True)

            r_list = client.session_list(check=True)
            self.assertIn("daemon_s1", _unwrap_sessions(r_list))
            path = _unwrap_eval(client.eval("window.location.pathname", session="daemon_s1", check=True))
            self.assertEqual(path, "/nav/page1")

    def test_f14_02_cli_json_envelope_contract(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            results = [
                client.doctor(json_output=True),
                client.session_new(session="env_s1", json_output=True),
                client.open(f"{cfg.base_url}/nav/page1", session="env_s1", json_output=True),
                client.snapshot(session="env_s1", json_output=True),
                client.extract("text", session="env_s1", json_output=True),
                client.session_close("env_s1", json_output=True),
            ]
            for res in results:
                self.assertEqual(res.returncode, 0)
                self.assertIsInstance(res.payload, dict)
                self.assertIs(res.payload.get("ok"), True)
                self.assertIn("data", res.payload)

    def test_f14_03_open_redirect_back_forward_reload(self) -> None:
        cfg = get_config()
        with MimikyuClient() as client:
            client.session_new(check=True)
            client.open(f"{cfg.base_url}/nav/page1", check=True)
            client.open(f"{cfg.base_url}/redirect", check=True)
            self.assertEqual(_unwrap_eval(client.eval("window.location.pathname", check=True)), "/nav/page2")

            client.back(check=True)
            self.assertEqual(_unwrap_eval(client.eval("window.location.pathname", check=True)), "/nav/page1")

            client.forward(check=True)
            self.assertEqual(_unwrap_eval(client.eval("window.location.pathname", check=True)), "/nav/page2")

            client.eval("window.__MK_RELOAD_CHECK = 99", main_world=True, check=True)
            client.reload(check=True)
            self.assertEqual(
                _unwrap_eval(client.eval("typeof window.__MK_RELOAD_CHECK", main_world=True, check=True)),
                "undefined",
            )
            self.assertEqual(_unwrap_eval(client.eval("window.location.pathname", check=True)), "/nav/page2")

            # Also verify SPA pushState history navigation
            client.open(f"{cfg.base_url}/history", check=True)
            snap = client.snapshot(check=True)
            btn1 = extract_ax_ref(snap, role="button", name_substr="Push Step 1") or "#push-step1"
            btn2 = extract_ax_ref(snap, role="button", name_substr="Push Step 2") or "#push-step2"
            client.click(btn1, check=True)
            client.click(btn2, check=True)
            client.back(check=True)
            self.assertEqual(
                _unwrap_eval(client.eval("document.getElementById('history-state').textContent", check=True)),
                "step:1",
            )
            client.forward(check=True)
            self.assertEqual(
                _unwrap_eval(client.eval("document.getElementById('history-state').textContent", check=True)),
                "step:2",
            )

    def test_f14_04_mcp_initialize_and_tools_list_schema(self) -> None:
        cfg = get_config()
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
        with McpClient(cfg.bin_path, engine=cfg.engine) as mcp:
            init_res = mcp.initialize()
            self.assertIsInstance(init_res, dict)
            self.assertTrue("capabilities" in init_res or "protocolVersion" in init_res)
            tools = mcp.tools_list()
            tool_map = {t["name"]: t for t in tools if isinstance(t, dict) and "name" in t}
            self.assertTrue(expected_tools.issubset(set(tool_map.keys())))
            for name in expected_tools:
                self.assertIsInstance(tool_map[name].get("inputSchema"), dict)

    def test_f14_05_mcp_tools_call_agentic_loop(self) -> None:
        cfg = get_config()
        with McpClient(cfg.bin_path, engine=cfg.engine) as mcp:
            mcp.initialize()
            tools = {t["name"]: t for t in mcp.tools_list() if isinstance(t, dict) and "name" in t}

            def _props(tool_name: str) -> dict[str, Any]:
                schema = tools.get(tool_name, {}).get("inputSchema", {})
                props = schema.get("properties", {}) if isinstance(schema, dict) else {}
                return props if isinstance(props, dict) else {}

            def _pick_key(props: dict[str, Any], candidates: tuple[str, ...], default: str) -> str:
                for cand in candidates:
                    if cand in props:
                        return cand
                return default

            sid = "mcp_loop_1"
            new_props = _props("mimikyu_session_new")
            new_args: dict[str, Any] = {}
            if "session" in new_props:
                new_args["session"] = sid
            elif "session_id" in new_props:
                new_args["session_id"] = sid
            _unwrap_mcp_text(mcp.tools_call("mimikyu_session_new", new_args))

            open_props = _props("mimikyu_open")
            open_args: dict[str, Any] = {_pick_key(open_props, ("url",), "url"): f"{cfg.base_url}/nav/page1"}
            if "session" in open_props:
                open_args["session"] = sid
            elif "session_id" in open_props:
                open_args["session_id"] = sid
            _unwrap_mcp_text(mcp.tools_call("mimikyu_open", open_args))

            snap_props = _props("mimikyu_snapshot")
            snap_args: dict[str, Any] = {}
            if "session" in snap_props:
                snap_args["session"] = sid
            elif "session_id" in snap_props:
                snap_args["session_id"] = sid
            snap_res = mcp.tools_call("mimikyu_snapshot", snap_args)
            ref = extract_ax_ref(snap_res, role="link", name_substr="Go to Page 2")
            self.assertIsNotNone(ref)
            assert ref is not None

            click_props = _props("mimikyu_click")
            click_args: dict[str, Any] = {_pick_key(click_props, ("target", "selector", "ref"), "target"): ref}
            if "session" in click_props:
                click_args["session"] = sid
            elif "session_id" in click_props:
                click_args["session_id"] = sid
            _unwrap_mcp_text(mcp.tools_call("mimikyu_click", click_args))

            wait_props = _props("mimikyu_wait")
            wait_args: dict[str, Any] = {_pick_key(wait_props, ("text",), "text"): "Page 2"}
            if "session" in wait_props:
                wait_args["session"] = sid
            elif "session_id" in wait_props:
                wait_args["session_id"] = sid
            _unwrap_mcp_text(mcp.tools_call("mimikyu_wait", wait_args))

            ext_props = _props("mimikyu_extract")
            ext_args: dict[str, Any] = {_pick_key(ext_props, ("mode", "format"), "mode"): "text"}
            if "session" in ext_props:
                ext_args["session"] = sid
            elif "session_id" in ext_props:
                ext_args["session_id"] = sid
            ext_text = _unwrap_mcp_text(mcp.tools_call("mimikyu_extract", ext_args))
            self.assertIn("Page 2", ext_text)

            eval_props = _props("mimikyu_eval")
            eval_args: dict[str, Any] = {_pick_key(eval_props, ("js", "expression", "script", "code"), "js"): "document.title"}
            if "session" in eval_props:
                eval_args["session"] = sid
            elif "session_id" in eval_props:
                eval_args["session_id"] = sid
            eval_text = _unwrap_mcp_text(mcp.tools_call("mimikyu_eval", eval_args))
            self.assertIn("Nav Page 2", eval_text)

            close_props = _props("mimikyu_session_close")
            close_args: dict[str, Any] = {}
            if "session" in close_props:
                close_args["session"] = sid
            elif "session_id" in close_props:
                close_args["session_id"] = sid
            _unwrap_mcp_text(mcp.tools_call("mimikyu_session_close", close_args))


# ---------------------------------------------------------------------------
# F15: Local Test & Reproducible Benchmark Suite (5 tests)
# ---------------------------------------------------------------------------


class TestF15LocalBenchmarkSuite(unittest.TestCase):
    def test_f15_01_bench_json_envelope_and_config_echo(self) -> None:
        with MimikyuClient() as client:
            res = client.bench(iterations=2, warmup=1, check=True)
            self.assertTrue(res.ok)
            self.assertIsInstance(res.payload, dict)
            self.assertIs(res.payload.get("ok"), True)
            self.assertIsInstance(res.data, dict)
            iter_vals = _find_key_values(res.data, "iterations")
            warmup_vals = _find_key_values(res.data, "warmup")
            self.assertGreater(
                len(iter_vals),
                0,
                f"Expected 'iterations' key in bench JSON data: {res.data!r}",
            )
            self.assertGreater(
                len(warmup_vals),
                0,
                f"Expected 'warmup' key in bench JSON data: {res.data!r}",
            )
            if "iterations" in res.data:
                self.assertEqual(res.data["iterations"], 2)
                self.assertNotIsInstance(res.data["iterations"], bool)
            if "warmup" in res.data:
                self.assertEqual(res.data["warmup"], 1)
                self.assertNotIsInstance(res.data["warmup"], bool)
            self.assertTrue(
                any(v == 2 and not isinstance(v, bool) for v in iter_vals),
                f"Expected iterations == 2 in bench data, got {iter_vals!r}",
            )
            self.assertTrue(
                any(v == 1 and not isinstance(v, bool) for v in warmup_vals),
                f"Expected warmup == 1 in bench data, got {warmup_vals!r}",
            )

    def test_f15_02_bench_speed_metrics_positive_numeric(self) -> None:
        with MimikyuClient() as client:
            res = client.bench(iterations=2, warmup=1, check=True)
            self.assertIsInstance(res.data, dict)
            speed_vals = _collect_numbers_for_keys(
                res.data,
                ("ms", "speed", "timing", "latency", "duration", "elapsed"),
            )
            self.assertGreater(len(speed_vals), 0, f"No speed/timing metrics in bench data: {res.data!r}")
            self.assertTrue(any(v > 0.0 for v in speed_vals), f"Expected positive timing metric: {speed_vals!r}")

    def test_f15_03_bench_rss_memory_metrics_positive_numeric(self) -> None:
        with MimikyuClient() as client:
            res = client.bench(iterations=2, warmup=1, check=True)
            self.assertIsInstance(res.data, dict)
            rss_vals = _collect_numbers_for_keys(res.data, ("rss", "memory", "mem"))
            self.assertGreater(len(rss_vals), 0, f"No RSS/memory metrics in bench data: {res.data!r}")
            self.assertTrue(any(v > 0.0 for v in rss_vals), f"Expected positive memory metric: {rss_vals!r}")

    def test_f15_04_bench_five_agentic_tasks_complete(self) -> None:
        with MimikyuClient() as client:
            res = client.bench(iterations=2, warmup=1, check=True)
            data = res.data
            self.assertIsInstance(data, dict)
            tasks: Optional[list[Any]] = None
            for key in ("tasks", "workflows", "agentic_tasks"):
                if isinstance(data.get(key), list):
                    tasks = data[key]
                    break
            self.assertIsNotNone(tasks, f"Missing agentic tasks list in bench data: {data!r}")
            assert tasks is not None
            self.assertEqual(len(tasks), 5)
            for t in tasks:
                if isinstance(t, dict):
                    passed = t.get("ok") if "ok" in t else t.get("passed")
                    self.assertIs(passed, True, f"Bench task failed: {t!r}")

    def test_f15_05_bench_reproducibility_across_runs(self) -> None:
        with MimikyuClient() as c1:
            r1 = c1.bench(iterations=2, warmup=1, check=True)
        with MimikyuClient() as c2:
            r2 = c2.bench(iterations=2, warmup=1, check=True)

        self.assertIsInstance(r1.data, dict)
        self.assertIsInstance(r2.data, dict)
        self.assertEqual(set(r1.data.keys()), set(r2.data.keys()))

        def _extract_task_ids(d: dict[str, Any]) -> list[str]:
            for key in ("tasks", "workflows", "agentic_tasks"):
                if isinstance(d.get(key), list):
                    ids: list[str] = []
                    for idx, item in enumerate(d[key]):
                        if isinstance(item, dict):
                            ids.append(str(item.get("id") or item.get("name") or item.get("task") or idx))
                        else:
                            ids.append(str(item))
                    return ids
            return []

        ids1 = _extract_task_ids(r1.data)
        ids2 = _extract_task_ids(r2.data)
        self.assertEqual(len(ids1), 5)
        self.assertEqual(ids1, ids2)


if __name__ == "__main__":
    unittest.main()
