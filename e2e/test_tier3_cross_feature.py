"""Tier 3 E2E tests: pairwise cross-feature combinations and R3 runtime checks.

Strictly opaque-box against mimikyu's public CLI (--json), Unix-socket daemon,
and stdio MCP JSON-RPC 2.0 interfaces. Uses Python 3 stdlib + e2e.* only and
communicates exclusively over 127.0.0.1 loopback fixtures.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
from typing import Any, Optional
import unittest
import urllib.parse

from e2e.helpers import (
    CommandResult,
    LocalTestCluster,
    McpClient,
    MimikyuClient,
    assert_r3_compliant,
    extract_ax_ref,
    get_config,
)
from e2e.local_server import LocalHttpProxy


REPO_ROOT: Path = Path(__file__).resolve().parent.parent
PNG_MAGIC: bytes = b"\x89PNG\r\n\x1a\n"


def _resolve_binary_for_unittest() -> str:
    cfg = get_config()
    if cfg.bin_path:
        return cfg.bin_path
    for cand in (
        REPO_ROOT / "target" / "release" / "mimikyu",
        REPO_ROOT / "target" / "debug" / "mimikyu",
    ):
        if cand.is_file() and os.access(cand, os.X_OK):
            return str(cand)
    return shutil.which("mimikyu") or ""


def _unwrap_eval_value(res: CommandResult) -> Any:
    """Extract the evaluated JS value from a CommandResult across JSON shapes."""
    data = res.data
    if isinstance(data, dict):
        for key in ("value", "result", "output", "text"):
            if key in data:
                return data[key]
        return data
    if data is not None:
        return data
    if isinstance(res.payload, dict):
        for key in ("value", "result", "output"):
            if key in res.payload:
                return res.payload[key]
    return res.stdout.strip()


def _parse_json_eval(res: CommandResult) -> Any:
    val = _unwrap_eval_value(res)
    if isinstance(val, (dict, list)):
        return val
    if isinstance(val, str):
        text = val.strip()
        if text:
            return json.loads(text)
    raise AssertionError(f"Expected JSON eval result, got {val!r} (stdout={res.stdout!r})")


def _result_text(res: CommandResult) -> str:
    """Flatten CommandResult data/payload/stdout into searchable text."""
    parts: list[str] = []
    if res.data is not None:
        if isinstance(res.data, str):
            parts.append(res.data)
        else:
            parts.append(json.dumps(res.data))
    if res.payload is not None:
        parts.append(json.dumps(res.payload))
    if res.stdout:
        parts.append(res.stdout)
    return "\n".join(parts)


def _mcp_result_text(result: dict[str, Any]) -> str:
    chunks: list[str] = []
    content = result.get("content")
    if isinstance(content, list):
        for entry in content:
            if isinstance(entry, dict) and isinstance(entry.get("text"), str):
                chunks.append(entry["text"])
    chunks.append(json.dumps(result))
    return "\n".join(chunks)


def _pick_mcp_args(tool_schema: dict[str, Any], mapping: dict[str, Any]) -> dict[str, Any]:
    """Select argument keys matching a tool's inputSchema properties when declared."""
    props = (
        tool_schema.get("inputSchema", {}).get("properties", {})
        if isinstance(tool_schema, dict)
        else {}
    )
    if not isinstance(props, dict) or not props:
        out: dict[str, Any] = {}
        for canonical, val in mapping.items():
            if val is not None:
                out[canonical] = val[0] if isinstance(val, tuple) else val
        return out

    out = {}
    for canonical, val in mapping.items():
        if val is None:
            continue
        if isinstance(val, tuple):
            candidates, actual_val = val
        else:
            candidates, actual_val = (canonical,), val
        for key in candidates:
            if key in props:
                out[key] = actual_val
                break
        else:
            out[candidates[0]] = actual_val
    return out


def _doctor_supports_firefox(payload: Any) -> bool:
    """Detect whether `mimikyu doctor --json` reports an available Firefox binary."""
    if not isinstance(payload, dict):
        return False
    data = payload.get("data", payload)
    candidates: list[dict[str, Any]] = []
    if isinstance(data, dict):
        default_ff = data.get("default_firefox")
        if isinstance(default_ff, str) and default_ff.strip():
            return True
        elif isinstance(default_ff, dict):
            candidates.append(default_ff)

        for key in ("firefox", "Firefox"):
            val = data.get(key)
            if isinstance(val, dict):
                candidates.append(val)
            elif isinstance(val, list):
                for item in val:
                    if isinstance(item, dict):
                        candidates.append(item)
                    elif isinstance(item, str) and item.strip():
                        return True
            elif isinstance(val, str) and val.strip():
                return True
            elif isinstance(val, bool) and val:
                return True
        for list_key in ("browsers", "engines", "drivers"):
            items = data.get(list_key)
            if isinstance(items, list):
                for item in items:
                    if isinstance(item, dict):
                        name = str(item.get("engine") or item.get("name") or item.get("kind") or "").lower()
                        if "firefox" in name:
                            candidates.append(item)
            elif isinstance(items, dict):
                ff = items.get("firefox")
                if isinstance(ff, dict):
                    candidates.append(ff)
    for cand in candidates:
        for bool_key in ("available", "found", "installed", "ok", "ready"):
            if bool_key in cand:
                if bool(cand[bool_key]):
                    return True
                break
        else:
            for path_key in ("path", "binary", "binary_path", "executable"):
                val = cand.get(path_key)
                if isinstance(val, str) and val.strip():
                    return True
    return False


def _extract_network_entries(net_logs: CommandResult) -> list[dict[str, Any]]:
    """Normalize network log entries across list or dict container shapes."""
    containers: list[Any] = []
    if net_logs.data is not None:
        containers.append(net_logs.data)
    if isinstance(net_logs.payload, dict):
        containers.append(net_logs.payload)
    for container in containers:
        if isinstance(container, list):
            return [e for e in container if isinstance(e, dict)]
        if isinstance(container, dict):
            for key in ("events", "logs", "entries", "requests", "items"):
                val = container.get(key)
                if isinstance(val, list):
                    return [e for e in val if isinstance(e, dict)]
    return []


def _entry_url(entry: dict[str, Any]) -> str:
    return str(
        entry.get("url")
        or entry.get("request_url")
        or entry.get("uri")
        or entry.get("path")
        or ""
    )


def _ps_matching_lines(needle: str) -> list[str]:
    proc = subprocess.run(
        ["ps", "-eo", "pid=,ppid=,command="],
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode != 0:
        return []
    matches: list[str] = []
    for line in proc.stdout.splitlines():
        if needle in line and "ps -eo" not in line:
            matches.append(line.strip())
    return matches


class _Tier3Base(unittest.TestCase):
    bin_path: str
    engine: str
    base_url: str
    cross_url: str
    proxy_url: str
    _cluster: Optional[LocalTestCluster] = None

    @classmethod
    def setUpClass(cls) -> None:
        cfg = get_config()
        bin_path = _resolve_binary_for_unittest()
        if not bin_path or not Path(bin_path).is_file() or not os.access(bin_path, os.X_OK):
            raise RuntimeError(
                "Compiled mimikyu binary not found or not executable. "
                "Pass --bin <path> via e2e/run_e2e.py or set MIMIKYU_BIN."
            )
        cls.bin_path = bin_path
        cls.engine = cfg.engine

        if cfg.base_url and cfg.cross_url and cfg.proxy_url:
            cls.base_url = cfg.base_url
            cls.cross_url = cfg.cross_url
            cls.proxy_url = cfg.proxy_url
            cls._cluster = None
        else:
            cls._cluster = LocalTestCluster().start()
            cls.base_url = cls._cluster.base_url
            cls.cross_url = cls._cluster.cross_url
            cls.proxy_url = cls._cluster.proxy_url

    @classmethod
    def tearDownClass(cls) -> None:
        if cls._cluster is not None:
            cls._cluster.stop()
            cls._cluster = None

    def make_client(
        self,
        *,
        session: Optional[str] = None,
        engine: Optional[str] = None,
        timeout: float = 30.0,
        state_dir: Optional[Path] = None,
    ) -> MimikyuClient:
        client = MimikyuClient(
            bin_path=self.bin_path,
            state_dir=state_dir,
            engine=engine or self.engine,
            timeout=timeout,
            session=session,
        )
        self.addCleanup(client.close)
        return client

    def make_mcp(
        self,
        *,
        engine: Optional[str] = None,
        timeout: float = 30.0,
    ) -> McpClient:
        mcp = McpClient(
            bin_path=self.bin_path,
            engine=engine or self.engine,
            timeout=timeout,
        )
        self.addCleanup(mcp.close)
        return mcp

    def make_temp_dir(self) -> Path:
        tmp = tempfile.TemporaryDirectory(prefix="mk_t3_")
        self.addCleanup(tmp.cleanup)
        return Path(tmp.name)

    def make_local_proxy(self) -> LocalHttpProxy:
        proxy = LocalHttpProxy(allow_external=False).start()
        self.addCleanup(proxy.stop)
        return proxy

    def require_ref(
        self,
        snap: Any,
        *,
        role: Optional[str] = None,
        name_substr: Optional[str] = None,
    ) -> str:
        ref = extract_ax_ref(snap, role=role, name_substr=name_substr)
        self.assertIsNotNone(
            ref,
            f"Expected @eN ref for role={role!r}, name_substr={name_substr!r} in snapshot",
        )
        assert ref is not None
        return ref

    def assert_no_orphan_processes(self, needle: str, timeout_s: float = 2.0) -> None:
        deadline = time.monotonic() + timeout_s
        remaining: list[str] = []
        while True:
            remaining = _ps_matching_lines(needle)
            if not remaining:
                return
            if time.monotonic() >= deadline:
                break
            time.sleep(0.1)
        self.fail(f"Found orphan process(es) matching {needle!r}: {remaining}")

    def wait_env_ready(self, client: MimikyuClient, *, session: Optional[str] = None) -> dict[str, Any]:
        res = client.wait(
            selector="#env-json[data-ready='true']",
            timeout_ms=8000,
            session=session,
            check=False,
        )
        if not res.ok:
            client.wait(text="ready", timeout_ms=8000, session=session, check=True)
        ev = client.eval(
            "document.getElementById('env-json').textContent",
            session=session,
            check=True,
        )
        report = _parse_json_eval(ev)
        self.assertIsInstance(report, dict)
        self.assertEqual(report.get("status"), "ready")
        return report

    def read_state_report(self, client: MimikyuClient, *, session: Optional[str] = None) -> dict[str, Any]:
        res = client.wait(
            selector="#state-json[data-ready='true']",
            timeout_ms=5000,
            session=session,
            check=False,
        )
        if not res.ok:
            client.wait(text="ready", timeout_ms=5000, session=session, check=True)
        ev = client.eval(
            "document.getElementById('state-json').textContent",
            session=session,
            check=True,
        )
        report = _parse_json_eval(ev)
        self.assertIsInstance(report, dict)
        return report

    def assert_catalog_assets_blocked_and_api_logged(self, client: MimikyuClient) -> None:
        img_blocked = client.eval(
            "(() => { const el = document.getElementById('catalog-banner'); "
            "return Boolean(el && el.complete && el.naturalWidth === 0); })()",
            check=True,
        )
        self.assertIs(
            _unwrap_eval_value(img_blocked),
            True,
            "Expected #catalog-banner to be blocked (complete && naturalWidth === 0)",
        )

        net_logs = client.logs("network", check=True)
        net_blob = _result_text(net_logs)
        self.assertIn("/api/catalog", net_blob)
        entries = _extract_network_entries(net_logs)
        self.assertTrue(
            entries,
            f"Expected non-empty network log entries: {net_logs.payload!r}",
        )
        self.assertTrue(
            any("/api/catalog" in _entry_url(e) for e in entries),
            f"Expected /api/catalog in network log entries: {entries!r}",
        )
        for entry in entries:
            entry_url = _entry_url(entry)
            if "/assets/img.png" in entry_url or "/assets/font.woff2" in entry_url:
                self.assertNotIn(entry.get("status"), (200, "200"))


class TestTier3CrossFeature(_Tier3Base):
    """19 pairwise cross-feature E2E tests covering F1-F15 and R3."""

    def test_f1_f3_doctor_discovery_drives_engine_launch(self) -> None:
        client = self.make_client()
        doc = client.doctor(json_output=True, check=True)
        self.assertTrue(doc.ok)
        doc_blob = json.dumps(doc.payload).lower()
        self.assertTrue(
            "chrom" in doc_blob,
            f"Expected Chromium binary info in doctor output: {doc.payload!r}",
        )

        # Primary Chromium launch & navigation
        c_chr = self.make_client(engine="chromium", session="chr_f1_f3")
        c_chr.session_new(engine="chromium", ephemeral=True, check=True)
        c_chr.open(f"{self.base_url}/nav/page1", check=True)
        ext = c_chr.extract("text", check=True)
        self.assertIn("Page 1", _result_text(ext))
        c_chr.session_close(check=True)

        # Secondary Firefox BiDi path if discovered or requested; else clean structured error
        if _doctor_supports_firefox(doc.payload) or self.engine == "firefox":
            c_ff = self.make_client(engine="firefox", session="ff_f1_f3")
            c_ff.session_new(engine="firefox", ephemeral=True, check=True)
            c_ff.open(f"{self.base_url}/nav/page1", check=True)
            ff_ext = c_ff.extract("text", check=True)
            self.assertIn("Page 1", _result_text(ff_ext))
            c_ff.session_close(check=True)
        else:
            c_ff = self.make_client(engine="firefox", session="ff_missing")
            ff_dir = str(c_ff.state_dir)
            res = c_ff.session_new(engine="firefox", ephemeral=True, check=False)
            self.assertFalse(res.ok)
            self.assertTrue(bool(res.error))
            c_ff.close()
            self.assert_no_orphan_processes(ff_dir)

    def test_f2_f4_debloated_persistent_profile_flock_and_teardown(self) -> None:
        profile_dir = self.make_temp_dir() / "persist_profile"

        c1 = self.make_client(session="s1")
        s1_state = str(c1.state_dir)
        c1.session_new(profile=str(profile_dir), session="s1", check=True)
        c1.open(f"{self.base_url}/state?set_cookie=persist1&set_storage=val1", check=True)
        self.read_state_report(c1)

        # Concurrent flock contention on the same profile directory must fail cleanly
        c2 = self.make_client(session="s2")
        res2 = c2.session_new(profile=str(profile_dir), session="s2", check=False)
        self.assertFalse(res2.ok, "Concurrent persistent profile lock should be rejected")
        c2.close()

        # Original session remains healthy after rejected contention
        alive_title = c1.eval("document.title", check=True)
        self.assertIn("State", str(_unwrap_eval_value(alive_title)))

        c1.close()
        self.assert_no_orphan_processes(str(profile_dir))
        self.assert_no_orphan_processes(s1_state)

        # Reopen persistent profile and confirm durable cookies + storage
        c3 = self.make_client(session="s3")
        c3.session_new(profile=str(profile_dir), session="s3", check=True)
        c3.open(f"{self.base_url}/state", check=True)
        report = self.read_state_report(c3)
        self.assertEqual(report["serverCookies"].get("mimikyu_normal"), "persist1")
        self.assertEqual(report["serverCookies"].get("mimikyu_httponly"), "ho_persist1")
        self.assertEqual(report["localStorage"].get("token"), "val1")
        self.assertEqual(report["indexedDB"]["entries"].get("token"), "val1")

        c3.close()
        self.assert_no_orphan_processes(str(profile_dir))

    def test_f4_f5_export_import_with_concurrent_virtual_isolation(self) -> None:
        art_dir = self.make_temp_dir()
        profile_dir = art_dir / "src_profile"
        export_file = art_dir / "state_export.json"

        c_persist = self.make_client(session="p1")
        c_persist.session_new(profile=str(profile_dir), check=True)
        c_persist.open(f"{self.base_url}/state?set_cookie=exp_tok&set_storage=exp_val", check=True)
        self.read_state_report(c_persist)
        c_persist.session_export(export_file, check=True)
        self.assertTrue(export_file.is_file() and export_file.stat().st_size > 0)
        c_persist.close()

        c_virt = self.make_client()
        c_virt.session_new(virtual=True, session="v1", check=True)
        c_virt.session_new(virtual=True, session="v2", check=True)
        c_virt.session_import(export_file, session="v1", check=True)

        c_virt.open(f"{self.base_url}/state", session="v1", check=True)
        c_virt.open(f"{self.base_url}/state", session="v2", check=True)
        rep_v1 = self.read_state_report(c_virt, session="v1")
        rep_v2 = self.read_state_report(c_virt, session="v2")

        self.assertEqual(rep_v1["serverCookies"].get("mimikyu_normal"), "exp_tok")
        self.assertEqual(rep_v1["serverCookies"].get("mimikyu_httponly"), "ho_exp_tok")
        self.assertEqual(rep_v1["localStorage"].get("token"), "exp_val")

        self.assertEqual(rep_v2["serverCookies"], {})
        self.assertEqual(rep_v2["documentCookie"], "")
        self.assertEqual(rep_v2["localStorage"], {})

        # Mutate v2 and confirm v1 does not observe v2 state
        c_virt.open(
            f"{self.base_url}/state?set_cookie=v2_only&set_storage=v2_val",
            session="v2",
            check=True,
        )
        self.read_state_report(c_virt, session="v2")
        c_virt.reload(session="v1", check=True)
        rep_v1_after = self.read_state_report(c_virt, session="v1")
        self.assertEqual(rep_v1_after["serverCookies"].get("mimikyu_normal"), "exp_tok")
        self.assertNotEqual(rep_v1_after["localStorage"].get("token"), "v2_val")

    def test_f5_f7_concurrent_virtual_distinct_proxy_locale_tz_geoip(self) -> None:
        proxy_a = self.make_local_proxy()
        proxy_b = self.make_local_proxy()
        client = self.make_client()

        client.session_new(
            virtual=True,
            proxy=proxy_a.proxy_url,
            locale="en-US",
            timezone="America/New_York",
            geoip="8.8.8.8",
            session="ctx_a",
            check=True,
        )
        client.session_new(
            virtual=True,
            proxy=proxy_b.proxy_url,
            locale="de-DE",
            timezone="Europe/Berlin",
            geoip="1.1.1.1",
            session="ctx_b",
            check=True,
        )

        client.open(f"{self.base_url}/state?set_cookie=tokA", session="ctx_a", check=True)
        self.read_state_report(client, session="ctx_a")
        client.open(f"{self.base_url}/state?set_cookie=tokB", session="ctx_b", check=True)
        self.read_state_report(client, session="ctx_b")

        client.open(f"{self.base_url}/echo-env", session="ctx_a", check=True)
        env_a = self.wait_env_ready(client, session="ctx_a")
        client.open(f"{self.base_url}/echo-env", session="ctx_b", check=True)
        env_b = self.wait_env_ready(client, session="ctx_b")

        self.assertEqual(env_a["intl"]["timezone"], "America/New_York")
        self.assertEqual(env_a["navigator"]["language"], "en-US")
        self.assertTrue((env_a["httpHeaders"].get("acceptLanguage") or "").lower().startswith("en-us"))

        self.assertEqual(env_b["intl"]["timezone"], "Europe/Berlin")
        self.assertEqual(env_b["navigator"]["language"], "de-DE")
        self.assertTrue((env_b["httpHeaders"].get("acceptLanguage") or "").lower().startswith("de-de"))

        self.assertGreater(proxy_a.request_count, 0)
        self.assertGreater(proxy_b.request_count, 0)

        # Verify cookie isolation across both contexts (re-open /state in ctx_a after ctx_b write)
        client.open(f"{self.base_url}/state", session="ctx_a", check=True)
        rep_a = self.read_state_report(client, session="ctx_a")
        self.assertEqual(rep_a["serverCookies"].get("mimikyu_normal"), "tokA")
        self.assertNotIn("tokB", json.dumps(rep_a))

        client.open(f"{self.base_url}/state", session="ctx_b", check=True)
        rep_b = self.read_state_report(client, session="ctx_b")
        self.assertEqual(rep_b["serverCookies"].get("mimikyu_normal"), "tokB")
        self.assertNotIn("tokA", json.dumps(rep_b))

    def test_f6_f7_coherent_preset_locale_tz_viewport_and_worker_alignment(self) -> None:
        client = self.make_client()
        client.session_new(
            ephemeral=True,
            preset="win11",
            locale="fr-FR",
            timezone="Europe/Paris",
            viewport="1280x720",
            check=True,
        )
        client.open(f"{self.base_url}/echo-env", check=True)
        report = self.wait_env_ready(client)

        self.assertEqual(report["navigator"]["platform"], "Win32")
        self.assertIn("Windows", report["navigator"]["userAgent"])
        sec_plat = (report["httpHeaders"].get("secChUaPlatform") or "").lower()
        if sec_plat:
            self.assertIn("windows", sec_plat)
        self.assertEqual(report["navigator"]["language"], "fr-FR")
        self.assertEqual(report["intl"]["timezone"], "Europe/Paris")
        self.assertEqual(report["screen"]["viewport"]["innerWidth"], 1280)
        self.assertEqual(report["screen"]["viewport"]["innerHeight"], 720)
        self.assertEqual(report["worker"]["status"], "ok")
        self.assertTrue(report["workerConsistency"]["consistent"])
        self.assertEqual(report["workerConsistency"]["mismatches"], [])

    def test_f6_f9_coherent_profile_isolated_vs_main_world_eval(self) -> None:
        client = self.make_client()
        client.session_new(
            ephemeral=True,
            preset="macos",
            locale="en-GB",
            timezone="Europe/London",
            check=True,
        )
        client.open(f"{self.base_url}/echo-env", check=True)
        report = self.wait_env_ready(client)

        self.assertTrue(report["isolatedWorld"]["isolatedWorldClean"])
        self.assertEqual(report["isolatedWorld"]["leakedKeys"], [])

        iso_res = client.eval(
            "window.__ISO_PROBE = 'hidden'; JSON.stringify({"
            "ua: navigator.userAgent, "
            "plat: navigator.platform, "
            "tz: Intl.DateTimeFormat().resolvedOptions().timeZone, "
            "hasReport: typeof window.__ENV_REPORT"
            "})",
            check=True,
        )
        iso_data = _parse_json_eval(iso_res)
        self.assertEqual(iso_data["plat"], "MacIntel")
        self.assertEqual(iso_data["tz"], "Europe/London")
        self.assertEqual(iso_data["hasReport"], "undefined")

        mw_probe = client.eval("typeof window.__ISO_PROBE", main_world=True, check=True)
        self.assertEqual(str(_unwrap_eval_value(mw_probe)).strip('"'), "undefined")

        mw_marker = client.eval("typeof window.__mimikyu", main_world=True, check=True)
        self.assertEqual(str(_unwrap_eval_value(mw_marker)).strip('"'), "undefined")

        mw_status = client.eval("window.__ENV_REPORT.status", main_world=True, check=True)
        self.assertEqual(str(_unwrap_eval_value(mw_status)).strip('"'), "ready")

    def test_f8_f10_resource_blocking_with_spa_snapshot_and_diff(self) -> None:
        client = self.make_client()
        client.session_new(ephemeral=True, block_images=True, block_fonts=True, check=True)

        client.open(f"{self.base_url}/workflow/catalog", check=True)
        client.wait(text="ready", timeout_ms=5000, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)
        self.assert_catalog_assets_blocked_and_api_logged(client)

        client.open(f"{self.base_url}/spa?delay_ms=150", check=True)
        client.wait(text="loaded", timeout_ms=5000, check=True)
        snap1 = client.snapshot(check=True)
        snap1_text = _result_text(snap1)
        self.assertIn("Alpha", snap1_text)
        self.assertIn("Category Filter", snap1_text)

        open_ref = self.require_ref(snap1, role="button", name_substr="Open Modal")
        client.click(open_ref, check=True)
        snap_diff = client.snapshot(diff=True, check=True)
        diff_text = _result_text(snap_diff)
        self.assertTrue(
            "Close Modal" in diff_text or "SPA Dialog Content" in diff_text,
            f"Expected modal transition in snapshot --diff: {diff_text!r}",
        )

    def test_f8_f11_resource_blocking_with_extract_tables_screenshot_logs(self) -> None:
        client = self.make_client()
        client.session_new(
            ephemeral=True,
            block_images=True,
            block_fonts=True,
            block_media=True,
            check=True,
        )
        client.open(f"{self.base_url}/workflow/catalog", check=True)
        client.wait(text="ready", timeout_ms=5000, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        tables = client.extract("tables", check=True)
        tables_text = _result_text(tables)
        for item_name in ("Node Alpha", "Node Beta", "Edge Router", "Archive Bucket"):
            self.assertIn(item_name, tables_text)

        out_png = self.make_temp_dir() / "blocked_catalog.png"
        client.screenshot(out_png, check=True)
        self.assertTrue(out_png.is_file())
        raw_png = out_png.read_bytes()
        self.assertGreater(len(raw_png), 100)
        self.assertTrue(raw_png.startswith(PNG_MAGIC))

        self.assert_catalog_assets_blocked_and_api_logged(client)

    def test_f9_f10_snapshot_refs_isolated_eval_and_cross_origin_frames(self) -> None:
        client = self.make_client()
        client.session_new(ephemeral=True, check=True)
        cross_q = urllib.parse.quote(self.cross_url, safe="")
        client.open(f"{self.base_url}/frames?cross_origin={cross_q}", check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        snap = client.snapshot(check=True)
        same_in = self.require_ref(snap, role="textbox", name_substr="Same Frame Note")
        same_btn = self.require_ref(snap, role="button", name_substr="Apply")
        cross_in = self.require_ref(snap, role="textbox", name_substr="Cross Frame Note")
        cross_btn = self.require_ref(snap, role="button", name_substr="Save")
        self.assertNotEqual(same_in, cross_in)
        self.assertNotEqual(same_btn, cross_btn)

        client.fill(cross_in, "cross-ref-msg", check=True)
        client.click(cross_btn, check=True)
        client.wait(text="cross-ref-msg", timeout_ms=5000, check=True)

        host_msg = _parse_json_eval(
            client.eval("document.getElementById('host-msg').textContent", check=True)
        )
        self.assertEqual(host_msg.get("source"), "cross-frame")
        self.assertEqual(host_msg.get("value"), "cross-ref-msg")

        client.eval("window.__ISO_GUARD = 1", check=True)
        mw_guard = client.eval("typeof window.__ISO_GUARD", main_world=True, check=True)
        self.assertEqual(str(_unwrap_eval_value(mw_guard)).strip('"'), "undefined")

    def test_f10_f12_wait_idle_delayed_spa_snapshot_diff_and_click(self) -> None:
        client = self.make_client()
        client.session_new(ephemeral=True, check=True)
        client.open(f"{self.base_url}/spa?delay_ms=500", check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        status_val = str(
            _unwrap_eval_value(
                client.eval("document.getElementById('spa-status').textContent", check=True)
            )
        ).strip('"')
        self.assertEqual(status_val, "loaded")

        items_text = str(
            _unwrap_eval_value(
                client.eval("document.getElementById('spa-list').textContent", check=True)
            )
        )
        for name in ("Alpha", "Beta", "Gamma"):
            self.assertIn(name, items_text)

        snap1 = client.snapshot(check=True)
        open_ref = self.require_ref(snap1, role="button", name_substr="Open Modal")
        client.click(open_ref, check=True)

        snap_diff = client.snapshot(diff=True, check=True)
        self.assertIn("Close Modal", _result_text(snap_diff))

        snap2 = client.snapshot(check=True)
        close_ref = self.require_ref(snap2, role="button", name_substr="Close Modal")
        client.click(close_ref, check=True)

        hidden_res = client.eval("document.getElementById('spa-modal').hidden === true", check=True)
        self.assertIs(_unwrap_eval_value(hidden_res), True)

    def test_f11_f12_instant_animations_wait_selector_target_screenshot_markdown(self) -> None:
        client = self.make_client()
        client.session_new(ephemeral=True, instant_animations=True, check=True)
        client.open(f"{self.base_url}/workflow/download-verify", check=True)
        client.wait(selector="#artifact-card", timeout_ms=5000, check=True)

        snap = client.snapshot(check=True)
        target_ref = (
            extract_ax_ref(snap, role="button", name_substr="Verify Checksum")
            or extract_ax_ref(snap, role="heading", name_substr="Deterministic Artifact Card")
        )
        self.assertIsNotNone(target_ref)
        assert target_ref is not None

        art_dir = self.make_temp_dir()
        elem_png = art_dir / "elem_target.png"
        full_png = art_dir / "full_page.png"
        client.screenshot(elem_png, target=target_ref, check=True)
        client.screenshot(full_png, full_page=True, check=True)

        elem_bytes = elem_png.read_bytes()
        full_bytes = full_png.read_bytes()
        self.assertTrue(elem_bytes.startswith(PNG_MAGIC))
        self.assertTrue(full_bytes.startswith(PNG_MAGIC))
        self.assertLess(len(elem_bytes), len(full_bytes))

        md = client.extract("markdown", check=True)
        md_text = _result_text(md)
        self.assertIn("Deterministic Artifact Card", md_text)
        self.assertIn("Download & Artifact Verification", md_text)

    def test_f12_f13_batch_atomic_form_fill_select_click_wait(self) -> None:
        client = self.make_client()
        client.session_new(ephemeral=True, check=True)
        client.open(f"{self.base_url}/form", check=True)
        snap = client.snapshot(check=True)

        name_ref = self.require_ref(snap, role="textbox", name_substr="Name")
        role_ref = self.require_ref(snap, role="combobox", name_substr="Role")
        agree_ref = self.require_ref(snap, role="checkbox", name_substr="Agree to terms")
        submit_ref = self.require_ref(snap, role="button", name_substr="Submit")

        ops = [
            {"op": "fill", "target": name_ref, "value": "BatchUser"},
            {"op": "select", "target": role_ref, "value": "agent"},
            {"op": "click", "target": agree_ref},
            {"op": "click", "target": submit_ref},
        ]
        client.batch(ops=ops, check=True)
        client.wait(text="Submission Received", timeout_ms=5000, check=True)

        sub_json = _parse_json_eval(
            client.eval("document.getElementById('submit-json').textContent", check=True)
        )
        self.assertTrue(sub_json.get("ok"))
        fields = sub_json.get("fields", {})
        self.assertEqual(fields.get("name"), "BatchUser")
        self.assertEqual(fields.get("role"), "agent")
        self.assertEqual(fields.get("agree"), "on")

    def test_f13_f6_humanize_viewport_clamped_coordinates_tracker(self) -> None:
        client = self.make_client()
        client.session_new(ephemeral=True, humanize=True, viewport="480x360", check=True)
        client.open(f"{self.base_url}/humanize-tracker", check=True)
        snap = client.snapshot(check=True)

        target_ref = self.require_ref(snap, role="button", name_substr="Target Button")
        input_ref = self.require_ref(snap, role="textbox", name_substr="Tracker Input")

        client.click(target_ref, humanize=True, check=True)
        client.fill(input_ref, "kinematic", humanize=True, check=True)

        tracker = _parse_json_eval(
            client.eval("document.getElementById('tracker-json').textContent", check=True)
        )
        counts = tracker.get("counts", {})
        events = tracker.get("events", [])
        self.assertGreaterEqual(counts.get("mousemove", 0), 2)
        self.assertGreaterEqual(counts.get("mousedown", 0), 1)
        self.assertGreaterEqual(counts.get("mouseup", 0), 1)

        mouse_events = [
            ev for ev in events
            if isinstance(ev, dict) and ev.get("type") in ("mousemove", "mousedown", "mouseup")
        ]
        self.assertTrue(
            any(
                ev.get("targetId") == "target-btn"
                for ev in mouse_events
                if ev.get("type") in ("mousedown", "mouseup")
            )
        )
        for ev in mouse_events:
            x = float(ev["x"])
            y = float(ev["y"])
            self.assertGreaterEqual(x, 0.0)
            self.assertLessEqual(x, 480.0)
            self.assertGreaterEqual(y, 0.0)
            self.assertLessEqual(y, 360.0)

        timestamps = [float(ev["t"]) for ev in events if isinstance(ev, dict) and "t" in ev]
        self.assertEqual(timestamps, sorted(timestamps))

    def test_f14_f5_warm_daemon_concurrent_cli_across_virtual_sessions(self) -> None:
        client = self.make_client()
        client.daemon(idle_timeout=30, check=True)
        client.session_new(virtual=True, session="a", check=True)
        client.session_new(virtual=True, session="b", check=True)

        def _drive(sid: str, cookie_val: str, storage_val: str) -> dict[str, Any]:
            client.open(
                f"{self.base_url}/state?set_cookie={cookie_val}&set_storage={storage_val}",
                session=sid,
                check=True,
            )
            return self.read_state_report(client, session=sid)

        with ThreadPoolExecutor(max_workers=2) as pool:
            fut_a = pool.submit(_drive, "a", "sessA", "valA")
            fut_b = pool.submit(_drive, "b", "sessB", "valB")
            rep_a = fut_a.result(timeout=45.0)
            rep_b = fut_b.result(timeout=45.0)

        listed = client.session_list(check=True)
        listed_blob = json.dumps(listed.payload)
        self.assertIn('"a"', listed_blob)
        self.assertIn('"b"', listed_blob)

        self.assertIn("mimikyu_normal=sessA", rep_a.get("documentCookie", ""))
        self.assertEqual(rep_a["localStorage"].get("token"), "valA")
        self.assertIn("mimikyu_normal=sessB", rep_b.get("documentCookie", ""))
        self.assertEqual(rep_b["localStorage"].get("token"), "valB")

        # Re-open clean /state on both sessions after concurrent writes to prove zero cross-session overwrite
        client.open(f"{self.base_url}/state", session="a", check=True)
        rep_a_after = self.read_state_report(client, session="a")
        client.open(f"{self.base_url}/state", session="b", check=True)
        rep_b_after = self.read_state_report(client, session="b")

        self.assertEqual(rep_a_after["serverCookies"].get("mimikyu_normal"), "sessA")
        self.assertEqual(rep_a_after["localStorage"].get("token"), "valA")
        self.assertNotIn("sessB", json.dumps(rep_a_after))
        self.assertNotIn("valB", json.dumps(rep_a_after))

        self.assertEqual(rep_b_after["serverCookies"].get("mimikyu_normal"), "sessB")
        self.assertEqual(rep_b_after["localStorage"].get("token"), "valB")
        self.assertNotIn("sessA", json.dumps(rep_b_after))
        self.assertNotIn("valA", json.dumps(rep_b_after))

        client.session_close("a", check=True)
        client.session_close("b", check=True)

    def test_f14_f10_f13_mcp_stdio_end_to_end_form_flow(self) -> None:
        mcp = self.make_mcp()
        init_res = mcp.initialize()
        self.assertIsInstance(init_res, dict)

        tools = mcp.tools_list()
        tool_map = {t["name"]: t for t in tools if isinstance(t, dict) and "name" in t}
        for req in (
            "mimikyu_session_new",
            "mimikyu_session_close",
            "mimikyu_open",
            "mimikyu_snapshot",
            "mimikyu_fill",
            "mimikyu_click",
            "mimikyu_wait",
            "mimikyu_extract",
        ):
            self.assertIn(req, tool_map)

        sess_id = "mcp_t3_form"
        mcp.tools_call(
            "mimikyu_session_new",
            _pick_mcp_args(
                tool_map["mimikyu_session_new"],
                {"virtual": True, "session": (("session", "session_id"), sess_id)},
            ),
        )
        mcp.tools_call(
            "mimikyu_open",
            _pick_mcp_args(
                tool_map["mimikyu_open"],
                {"url": f"{self.base_url}/form", "session": (("session", "session_id"), sess_id)},
            ),
        )
        snap = mcp.tools_call(
            "mimikyu_snapshot",
            _pick_mcp_args(
                tool_map["mimikyu_snapshot"],
                {"session": (("session", "session_id"), sess_id)},
            ),
        )

        name_ref = self.require_ref(snap, role="textbox", name_substr="Name")
        submit_ref = self.require_ref(snap, role="button", name_substr="Submit")

        mcp.tools_call(
            "mimikyu_fill",
            _pick_mcp_args(
                tool_map["mimikyu_fill"],
                {
                    "target": (("target", "ref", "selector"), name_ref),
                    "value": (("value", "text"), "McpTier3"),
                    "session": (("session", "session_id"), sess_id),
                },
            ),
        )
        mcp.tools_call(
            "mimikyu_click",
            _pick_mcp_args(
                tool_map["mimikyu_click"],
                {
                    "target": (("target", "ref", "selector"), submit_ref),
                    "session": (("session", "session_id"), sess_id),
                },
            ),
        )
        mcp.tools_call(
            "mimikyu_wait",
            _pick_mcp_args(
                tool_map["mimikyu_wait"],
                {"text": "Submission Received", "session": (("session", "session_id"), sess_id)},
            ),
        )
        ext = mcp.tools_call(
            "mimikyu_extract",
            _pick_mcp_args(
                tool_map["mimikyu_extract"],
                {
                    "mode": (("mode", "format"), "text"),
                    "session": (("session", "session_id"), sess_id),
                },
            ),
        )
        ext_blob = _mcp_result_text(ext)
        self.assertIn("Submission Received", ext_blob)
        self.assertIn("McpTier3", ext_blob)

        mcp.tools_call(
            "mimikyu_session_close",
            _pick_mcp_args(
                tool_map["mimikyu_session_close"],
                {"session": (("session", "session_id", "id"), sess_id)},
            ),
        )

    def test_f15_f2_bench_clean_execution_zero_orphan_processes(self) -> None:
        client = self.make_client()
        tracked_dir = str(client.state_dir)
        res = client.bench(iterations=1, warmup=0, json_output=True, check=True, timeout=90.0)
        self.assertTrue(res.ok)
        self.assertIsInstance(res.payload, dict)
        self.assertTrue(res.payload.get("ok"))
        self.assertIsInstance(res.data, dict)
        self.assertGreater(len(res.data), 0)

        client.close()
        self.assert_no_orphan_processes(tracked_dir, timeout_s=2.0)

    def test_f3_f11_dual_protocol_chromium_firefox_extract_screenshot_parity(self) -> None:
        art_dir = self.make_temp_dir()

        def _run_catalog_capture(engine_name: str, out_name: str) -> tuple[str, str, bytes]:
            c = self.make_client(engine=engine_name, session=f"parity_{engine_name}")
            c.session_new(engine=engine_name, ephemeral=True, check=True)
            c.open(f"{self.base_url}/workflow/catalog", check=True)
            c.wait(text="ready", timeout_ms=5000, check=True)
            tables = _result_text(c.extract("tables", check=True))
            markdown = _result_text(c.extract("markdown", check=True))
            png_path = art_dir / out_name
            c.screenshot(png_path, check=True)
            c.session_close(check=True)
            return tables, markdown, png_path.read_bytes()

        chr_tables, chr_md, chr_png = _run_catalog_capture("chromium", "chr_catalog.png")
        self.assertTrue(chr_png.startswith(PNG_MAGIC))
        self.assertIn("Dynamic Resource Catalog", chr_md)
        self.assertIn("Node Alpha", chr_tables)
        self.assertIn("Edge Router", chr_tables)

        doc = self.make_client().doctor(json_output=True, check=True)
        if _doctor_supports_firefox(doc.payload) or self.engine == "firefox":
            ff_tables, ff_md, ff_png = _run_catalog_capture("firefox", "ff_catalog.png")
            self.assertTrue(ff_png.startswith(PNG_MAGIC))
            self.assertIn("Dynamic Resource Catalog", ff_md)
            self.assertIn("Node Alpha", ff_tables)
            self.assertIn("Edge Router", ff_tables)
        else:
            c_ff = self.make_client(engine="firefox", session="ff_unavail")
            res = c_ff.session_new(engine="firefox", ephemeral=True, check=False)
            self.assertFalse(res.ok)
            self.assertTrue(bool(res.error))

    def test_r3_cross_feature_runtime_guard_cli_session_mcp(self) -> None:
        client = self.make_client()
        help_root = client.run(
            "--help",
            json_output=False,
            include_engine=False,
            include_session=False,
            include_state_dir=False,
            check=True,
        )
        help_sess = client.run(
            "session",
            "new",
            "--help",
            json_output=False,
            include_engine=False,
            include_session=False,
            include_state_dir=False,
            check=True,
        )
        sess_res = client.session_new(virtual=True, json_output=True, check=True)
        client.session_close(check=True)

        mcp = self.make_mcp()
        init_res = mcp.initialize()
        tools = mcp.tools_list()

        assert_r3_compliant([
            help_root.stdout + "\n" + help_root.stderr,
            help_sess.stdout + "\n" + help_sess.stderr,
            json.dumps(sess_res.payload),
            json.dumps(init_res),
            json.dumps(tools),
            Path(__file__),
        ])

    def test_f7_f8_proxy_with_resource_blocking_and_network_logs(self) -> None:
        proxy = self.make_local_proxy()
        client = self.make_client()
        client.session_new(
            ephemeral=True,
            proxy=proxy.proxy_url,
            block_images=True,
            block_fonts=True,
            check=True,
        )
        client.open(f"{self.base_url}/workflow/catalog", check=True)
        client.wait(text="ready", timeout_ms=5000, check=True)
        client.open(f"{self.base_url}/echo-headers", check=True)

        self.assertGreater(proxy.request_count, 0)
        seen = proxy.seen_urls
        self.assertTrue(any("/api/catalog" in u for u in seen), f"Expected /api/catalog in {seen!r}")
        self.assertTrue(any("/echo-headers" in u for u in seen), f"Expected /echo-headers in {seen!r}")
        self.assertFalse(
            any("/assets/img.png" in u or "/assets/font.woff2" in u for u in seen),
            f"Blocked assets should not traverse proxy: {seen!r}",
        )

        net_logs = client.logs("network", check=True)
        net_blob = _result_text(net_logs)
        self.assertIn("/api/catalog", net_blob)
        entries = _extract_network_entries(net_logs)
        for entry in entries:
            entry_url = _entry_url(entry)
            if "/assets/img.png" in entry_url or "/assets/font.woff2" in entry_url:
                self.assertNotIn(entry.get("status"), (200, "200"))


if __name__ == "__main__":
    unittest.main()
