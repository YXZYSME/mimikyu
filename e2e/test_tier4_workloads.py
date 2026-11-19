"""Tier 4 E2E tests: 7 realistic multi-step agentic application workloads.

Strictly opaque-box against mimikyu's public CLI (--json), Unix-socket daemon,
and stdio MCP JSON-RPC 2.0 interfaces. Uses Python 3 stdlib + e2e.* only and
communicates exclusively over 127.0.0.1 loopback fixtures.
"""

from __future__ import annotations

import base64
import hashlib
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
from e2e.local_server import LocalHttpProxy, SAMPLE_SHA256_MAP


REPO_ROOT: Path = Path(__file__).resolve().parent.parent
PNG_MAGIC: bytes = b"\x89PNG\r\n\x1a\n"
PDF_MAGIC: bytes = b"%PDF-"


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


class _Tier4Base(unittest.TestCase):
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
        tmp = tempfile.TemporaryDirectory(prefix="mk_t4_")
        self.addCleanup(tmp.cleanup)
        return Path(tmp.name)

    def make_local_cluster(self) -> LocalTestCluster:
        cluster = LocalTestCluster().start()
        self.addCleanup(cluster.stop)
        return cluster

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


class TestTier4Workloads(_Tier4Base):
    """7 end-to-end real-world agentic application workloads."""

    def test_workload_01_onboarding_wizard(self) -> None:
        client = self.make_client(session="wiz", timeout=60.0)
        client.session_new(ephemeral=True, session="wiz", check=True)
        client.open(f"{self.base_url}/workflow/wizard", check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        # Step 1 negative validation
        snap1 = client.snapshot(check=True)
        next1_ref = self.require_ref(snap1, role="button", name_substr="Next Step")
        client.click(next1_ref, check=True)
        err1_text = _result_text(client.extract("text", check=True))
        self.assertIn("Name and email are required.", err1_text)
        step2_hidden = client.eval("document.getElementById('step-2').hidden === true", check=True)
        self.assertIs(_unwrap_eval_value(step2_hidden), True)

        # Step 1 valid entry + <select> role
        name_ref = self.require_ref(snap1, role="textbox", name_substr="Full Name")
        email_ref = self.require_ref(snap1, role="textbox", name_substr="Work Email")
        role_ref = self.require_ref(snap1, role="combobox", name_substr="Role")
        client.fill(name_ref, "Ada Lovelace", check=True)
        client.fill(email_ref, "ada@local.test", check=True)
        client.select(role_ref, "researcher", check=True)
        client.click(next1_ref, check=True)
        client.wait(text="Accept Workspace Policy", timeout_ms=5000, check=True)

        # Step 2 negative validation
        snap2 = client.snapshot(check=True)
        review_ref = self.require_ref(snap2, role="button", name_substr="Review")
        agree_ref = self.require_ref(snap2, role="checkbox", name_substr="Accept Workspace Policy")
        notify_ref = self.require_ref(snap2, role="checkbox", name_substr="Enable Webhook Alerts")
        pro_ref = self.require_ref(snap2, role="radio", name_substr="Pro")

        client.click(review_ref, check=True)
        err2_text = _result_text(client.extract("text", check=True))
        self.assertIn("You must accept the workspace policy.", err2_text)

        # Step 2 conditional reveal + selections (re-resolve refs after DOM unhiding)
        client.click(notify_ref, check=True)
        snap2b = client.snapshot(check=True)
        webhook_ref = self.require_ref(snap2b, role="textbox", name_substr="Webhook URL")
        agree_ref = self.require_ref(snap2b, role="checkbox", name_substr="Accept Workspace Policy")
        pro_ref = self.require_ref(snap2b, role="radio", name_substr="Pro")
        review_ref = self.require_ref(snap2b, role="button", name_substr="Review")
        client.fill(webhook_ref, "http://127.0.0.1/hook", check=True)
        client.click(agree_ref, check=True)
        client.click(pro_ref, check=True)
        client.click(review_ref, check=True)

        # Step 3 review & submission
        client.wait(text="Step 3: Review", timeout_ms=5000, check=True)
        review_text = _result_text(client.extract("text", check=True))
        for expected in ("Ada Lovelace", "ada@local.test", "researcher", "pro"):
            self.assertIn(expected, review_text)

        snap3 = client.snapshot(check=True)
        submit_ref = self.require_ref(snap3, role="button", name_substr="Complete Onboarding")
        client.click(submit_ref, check=True)

        # Wait on unhidden completion section (not pre-existing Review text or hidden status)
        res_complete = client.wait(
            selector="#wiz-complete:not([hidden])",
            timeout_ms=5000,
            check=False,
        )
        if not res_complete.ok:
            client.wait(text="Onboarding Complete", timeout_ms=5000, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)
        complete_visible = client.eval(
            "document.getElementById('wiz-complete').hidden === false",
            check=True,
        )
        self.assertIs(_unwrap_eval_value(complete_visible), True)

        final_text = _result_text(client.extract("text", check=True))
        self.assertIn("Onboarding Complete", final_text)

        summary = _parse_json_eval(
            client.eval("document.getElementById('wiz-summary-json').textContent", check=True)
        )
        self.assertTrue(summary.get("ok"))
        fields = summary.get("fields", {})
        self.assertEqual(fields.get("name"), "Ada Lovelace")
        self.assertEqual(fields.get("email"), "ada@local.test")
        self.assertEqual(fields.get("role"), "researcher")
        self.assertIs(fields.get("agree"), True)
        self.assertIs(fields.get("notify"), True)
        self.assertEqual(fields.get("webhook"), "http://127.0.0.1/hook")
        self.assertEqual(fields.get("tier"), "pro")

    def test_workload_02_catalog_filter_tables_and_diff(self) -> None:
        client = self.make_client(session="cat", timeout=60.0)
        client.session_new(ephemeral=True, session="cat", check=True)
        client.open(f"{self.base_url}/workflow/catalog", check=True)
        client.wait(text="ready", timeout_ms=5000, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        tables_init = _result_text(client.extract("tables", check=True))
        for header in ("ID", "Name", "Category", "Region", "Price", "Action"):
            self.assertIn(header, tables_init)
        for item_name in (
            "Node Alpha",
            "Node Beta",
            "Block Vault",
            "Edge Router",
            "GPU Worker",
            "Archive Bucket",
        ):
            self.assertIn(item_name, tables_init)

        snap_init = client.snapshot(check=True)
        snap_init_text = _result_text(snap_init)
        self.assertIn("Page 1 of 2", snap_init_text)
        self.assertIn("Select Item 3", snap_init_text)

        next_ref = self.require_ref(snap_init, role="button", name_substr="Next Page")
        client.click(next_ref, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        tables_p2 = _result_text(client.extract("tables", check=True))
        self.assertIn("Mesh Gateway", tables_p2)
        self.assertIn("CDN Cache", tables_p2)
        self.assertIn("Page 2 of 2", _result_text(client.extract("text", check=True)))

        snap_p2 = client.snapshot(check=True)
        prev_ref = self.require_ref(snap_p2, role="button", name_substr="Previous Page")
        client.click(prev_ref, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        # Apply category=compute filter & inspect snapshot --diff
        snap_p1 = client.snapshot(check=True)
        cat_ref = self.require_ref(snap_p1, role="combobox", name_substr="Category")
        apply_ref = self.require_ref(snap_p1, role="button", name_substr="Apply Filter")
        client.select(cat_ref, "compute", check=True)
        client.click(apply_ref, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        tables_compute = _result_text(client.extract("tables", check=True))
        for compute_name in ("Node Alpha", "Node Beta", "GPU Worker", "Memory Pool", "Batch Runner"):
            self.assertIn(compute_name, tables_compute)
        self.assertNotIn("Block Vault", tables_compute)
        self.assertNotIn("Edge Router", tables_compute)

        diff_res = client.snapshot(diff=True, check=True)
        diff_text = _result_text(diff_res)
        self.assertTrue(
            "Page 1 of 1" in diff_text
            or "Select Item 8" in diff_text
            or "Select Item 11" in diff_text
            or "Select Item 3" in diff_text,
            f"Expected catalog filter delta in snapshot --diff: {diff_text!r}",
        )

        # Refine with search="ap-east" -> single row GPU Worker (#5)
        snap_comp = client.snapshot(check=True)
        search_ref = (
            extract_ax_ref(snap_comp, role="searchbox", name_substr="Search Catalog")
            or self.require_ref(snap_comp, role="textbox", name_substr="Search Catalog")
        )
        apply_ref2 = self.require_ref(snap_comp, role="button", name_substr="Apply Filter")
        client.fill(search_ref, "ap-east", check=True)
        client.click(apply_ref2, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        tables_gpu = _result_text(client.extract("tables", check=True))
        self.assertIn("GPU Worker", tables_gpu)
        self.assertIn("65", tables_gpu)
        self.assertNotIn("Node Alpha", tables_gpu)

        snap_gpu = client.snapshot(check=True)
        select_gpu_ref = self.require_ref(snap_gpu, role="button", name_substr="Select Item 5")
        client.click(select_gpu_ref, check=True)
        self.assertIn("Selected: GPU Worker (#5)", _result_text(client.extract("text", check=True)))

        reset_ref = self.require_ref(snap_gpu, role="button", name_substr="Reset Filter")
        client.click(reset_ref, check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)
        self.assertIn("Page 1 of 2", _result_text(client.extract("text", check=True)))

    def test_workload_03_cross_frame_and_isolated_world(self) -> None:
        client = self.make_client(session="frames", timeout=60.0)
        client.session_new(ephemeral=True, session="frames", check=True)
        cross_q = urllib.parse.quote(self.cross_url, safe="")
        client.open(f"{self.base_url}/workflow/cross-frame?cross_origin={cross_q}", check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        snap = client.snapshot(check=True)
        host_in = self.require_ref(snap, role="textbox", name_substr="Host Note")
        host_btn = self.require_ref(snap, role="button", name_substr="Record Host Note")
        same_in = self.require_ref(snap, role="textbox", name_substr="Same Frame Note")
        same_btn = self.require_ref(snap, role="button", name_substr="Apply")
        cross_in = self.require_ref(snap, role="textbox", name_substr="Cross Frame Note")
        cross_btn = self.require_ref(snap, role="button", name_substr="Save")

        client.fill(host_in, "host-msg-1", check=True)
        client.click(host_btn, check=True)
        client.fill(same_in, "same-msg-2", check=True)
        client.click(same_btn, check=True)
        client.fill(cross_in, "cross-msg-3", check=True)
        client.click(cross_btn, check=True)

        client.wait(text="messages:3", timeout_ms=5000, check=True)
        messages = _parse_json_eval(
            client.eval("document.getElementById('frame-messages').textContent", check=True)
        )
        self.assertIsInstance(messages, list)
        self.assertEqual(len(messages), 3)

        by_src = {m.get("source"): m for m in messages if isinstance(m, dict)}
        self.assertEqual(by_src["host"].get("value"), "host-msg-1")
        self.assertEqual(by_src["same-frame"].get("value"), "same-msg-2")
        self.assertEqual(by_src["same-frame"].get("origin"), self.base_url)
        self.assertEqual(by_src["cross-frame"].get("value"), "cross-msg-3")
        self.assertEqual(by_src["cross-frame"].get("origin"), self.cross_url)
        self.assertNotEqual(self.cross_url, self.base_url)

        # Isolated-world vs --main-world verification
        iso_res = client.eval(
            "window.__mimikyu_iso_probe = 'secret_iso'; typeof window.__mimikyu",
            check=True,
        )
        self.assertEqual(str(_unwrap_eval_value(iso_res)).strip('"'), "undefined")

        mw_res = client.eval(
            "JSON.stringify({"
            "hasMimikyu: typeof window.__mimikyu !== 'undefined', "
            "isoLeaked: typeof window.__mimikyu_iso_probe !== 'undefined'"
            "})",
            main_world=True,
            check=True,
        )
        mw_data = _parse_json_eval(mw_res)
        self.assertIs(mw_data.get("hasMimikyu"), False)
        self.assertIs(mw_data.get("isoLeaked"), False)

    def test_workload_04_multi_tab_and_virtual_isolation(self) -> None:
        local_cluster = self.make_local_cluster()
        base = local_cluster.base_url

        client = self.make_client(timeout=90.0)
        client.session_new(
            virtual=True,
            locale="en-US",
            timezone="America/New_York",
            session="s1",
            check=True,
        )
        client.session_new(
            virtual=True,
            locale="fr-FR",
            timezone="Europe/Paris",
            session="s2",
            check=True,
        )

        listed_blob = json.dumps(client.session_list(check=True).payload)
        self.assertIn('"s1"', listed_blob)
        self.assertIn('"s2"', listed_blob)

        # Multi-tab window.open() + token redemption in s1
        tok = "mimikyu-s1-secret-99"
        multi_url = f"{base}/workflow/multi-tab?token={urllib.parse.quote(tok, safe='')}"
        client.open(multi_url, session="s1", check=True)
        client.wait(idle=True, timeout_ms=5000, session="s1", check=True)

        snap_mt = client.snapshot(session="s1", check=True)
        open_tab_btn = self.require_ref(snap_mt, role="button", name_substr="Open Child Tab Window")
        client.click(open_tab_btn, session="s1", check=True)
        client.wait(idle=True, timeout_ms=5000, session="s1", check=True)

        loc_path = str(
            _unwrap_eval_value(client.eval("window.location.pathname", session="s1", check=True))
        ).strip('"')
        if loc_path == "/tab-child":
            child_tok = str(
                _unwrap_eval_value(
                    client.eval(
                        "document.getElementById('child-token').textContent",
                        session="s1",
                        check=True,
                    )
                )
            ).strip('"')
            self.assertEqual(child_tok, tok)
            client.open(multi_url, session="s1", check=True)
            client.wait(idle=True, timeout_ms=5000, session="s1", check=True)
        else:
            deadline = time.monotonic() + 1.5
            seen_child = False
            while time.monotonic() < deadline:
                if any("/tab-child" in req.get("path", "") for req in local_cluster.primary.request_log):
                    seen_child = True
                    break
                time.sleep(0.05)
            self.assertTrue(seen_child, "Expected /tab-child request after window.open()")
            client.open(
                f"{base}/tab-child?token={urllib.parse.quote(tok, safe='')}",
                session="s1",
                check=True,
            )
            child_tok = str(
                _unwrap_eval_value(
                    client.eval(
                        "document.getElementById('child-token').textContent",
                        session="s1",
                        check=True,
                    )
                )
            ).strip('"')
            self.assertEqual(child_tok, tok)
            client.open(multi_url, session="s1", check=True)
            client.wait(idle=True, timeout_ms=5000, session="s1", check=True)

        snap_redeem = client.snapshot(session="s1", check=True)
        redeem_in = self.require_ref(snap_redeem, role="textbox", name_substr="Redeem Token")
        redeem_btn = self.require_ref(snap_redeem, role="button", name_substr="Redeem Token")
        client.fill(redeem_in, tok, session="s1", check=True)
        client.click(redeem_btn, session="s1", check=True)
        client.wait(text=f"VERIFIED: {tok}", timeout_ms=5000, session="s1", check=True)
        self.assertIn(f"VERIFIED: {tok}", _result_text(client.extract("text", session="s1", check=True)))

        # Full cookie / storage isolation across s1 and s2
        client.open(
            f"{base}/state?set_cookie=s1_norm&httponly=s1_ho&set_storage=s1_store",
            session="s1",
            check=True,
        )
        self.read_state_report(client, session="s1")

        client.open(f"{base}/state", session="s1", check=True)
        rep_s1 = self.read_state_report(client, session="s1")
        self.assertEqual(rep_s1["serverCookies"].get("mimikyu_normal"), "s1_norm")
        self.assertEqual(rep_s1["serverCookies"].get("mimikyu_httponly"), "s1_ho")
        self.assertIn("mimikyu_normal=s1_norm", rep_s1.get("documentCookie", ""))
        self.assertNotIn("s1_ho", rep_s1.get("documentCookie", ""))
        self.assertEqual(rep_s1["localStorage"].get("token"), "s1_store")
        self.assertEqual(rep_s1["sessionStorage"].get("token"), "s1_store")
        self.assertEqual(rep_s1["indexedDB"]["entries"].get("token"), "s1_store")

        client.open(f"{base}/state", session="s2", check=True)
        rep_s2 = self.read_state_report(client, session="s2")
        self.assertEqual(rep_s2["serverCookies"], {})
        self.assertEqual(rep_s2["documentCookie"], "")
        self.assertEqual(rep_s2["localStorage"], {})
        self.assertEqual(rep_s2["sessionStorage"], {})
        self.assertEqual(rep_s2["indexedDB"]["entries"], {})

        # Distinct locale & timezone consistency in concurrent contexts
        client.open(f"{base}/echo-env", session="s1", check=True)
        env_s1 = self.wait_env_ready(client, session="s1")
        client.open(f"{base}/echo-env", session="s2", check=True)
        env_s2 = self.wait_env_ready(client, session="s2")

        self.assertEqual(env_s1["intl"]["timezone"], "America/New_York")
        self.assertTrue(env_s1["navigator"]["language"].lower().startswith("en"))
        self.assertEqual(env_s2["intl"]["timezone"], "Europe/Paris")
        self.assertTrue(env_s2["navigator"]["language"].lower().startswith("fr"))

        client.session_close("s1", check=True)
        client.session_close("s2", check=True)

    def test_workload_05_download_verify_and_artifacts(self) -> None:
        local_cluster = self.make_local_cluster()
        base = local_cluster.base_url

        client = self.make_client(session="dl", timeout=60.0)
        client.session_new(
            ephemeral=True,
            block_images=True,
            block_media=True,
            session="dl",
            check=True,
        )
        client.open(f"{base}/workflow/download-verify", check=True)
        client.wait(idle=True, timeout_ms=5000, check=True)

        asset_status = _parse_json_eval(
            client.eval("document.getElementById('asset-status').textContent", check=True)
        )
        self.assertEqual(asset_status.get("img"), "blocked_or_error")
        self.assertIn(asset_status.get("media"), ("blocked_or_error", "pending"))

        req_paths = [req.get("path", "") for req in local_cluster.primary.request_log]
        self.assertFalse(any("/assets/img.png" in p for p in req_paths))
        self.assertFalse(any("/assets/media.mp4" in p for p in req_paths))

        # Trigger sample.bin download & verify SHA-256 via UI + local file if saved
        snap = client.snapshot(check=True)
        dl_bin_ref = self.require_ref(snap, role="link", name_substr="sample.bin")
        client.click(dl_bin_ref, check=True)

        expected_sha = SAMPLE_SHA256_MAP["sample.bin"]["sha256"]
        expected_size = SAMPLE_SHA256_MAP["sample.bin"]["size"]

        deadline = time.monotonic() + 3.0
        while time.monotonic() < deadline:
            if any("/download/sample.bin" in r.get("path", "") for r in local_cluster.primary.request_log):
                break
            time.sleep(0.05)

        self.assertTrue(
            any("/download/sample.bin" in r.get("path", "") for r in local_cluster.primary.request_log),
            "Expected /download/sample.bin request after clicking sample.bin download link",
        )

        saved_bins = list(client.state_dir.rglob("sample.bin"))
        if saved_bins:
            raw_dl = saved_bins[0].read_bytes()
            self.assertEqual(len(raw_dl), expected_size)
            self.assertEqual(hashlib.sha256(raw_dl).hexdigest(), expected_sha)

        sha_in = self.require_ref(snap, role="textbox", name_substr="Expected SHA-256")
        verify_btn = self.require_ref(snap, role="button", name_substr="Verify Checksum")
        client.fill(sha_in, expected_sha, check=True)
        client.click(verify_btn, check=True)
        client.wait(text="VERIFIED", timeout_ms=5000, check=True)

        verify_val = str(
            _unwrap_eval_value(
                client.eval("document.getElementById('verify-result').textContent", check=True)
            )
        ).strip('"')
        self.assertEqual(verify_val, "VERIFIED")

        # Screenshot & PDF artifacts
        art_dir = self.make_temp_dir()
        png_path = art_dir / "verify.png"
        card_path = art_dir / "card.png"
        client.screenshot(png_path, check=True)
        client.screenshot(card_path, target="#artifact-card", check=True)

        for p in (png_path, card_path):
            self.assertTrue(p.is_file())
            data = p.read_bytes()
            self.assertGreater(len(data), 100)
            self.assertTrue(data.startswith(PNG_MAGIC))

        pdf_path = art_dir / "verify.pdf"
        if self.engine == "chromium":
            client.pdf(pdf_path, check=True)
            self.assertTrue(pdf_path.is_file())
            pdf_bytes = pdf_path.read_bytes()
            self.assertGreater(len(pdf_bytes), 100)
            self.assertTrue(pdf_bytes.startswith(PDF_MAGIC))
        else:
            pdf_res = client.pdf(pdf_path, check=False)
            if pdf_res.ok:
                pdf_bytes = pdf_path.read_bytes()
                self.assertGreater(len(pdf_bytes), 100)
                self.assertTrue(pdf_bytes.startswith(PDF_MAGIC))
            else:
                self.assertTrue(bool(pdf_res.error))

    def test_workload_06_mcp_full_agentic_lifecycle(self) -> None:
        art_dir = self.make_temp_dir()
        mcp = self.make_mcp(timeout=60.0)
        tracked_state_dir = str(mcp.state_dir)

        init_res = mcp.initialize()
        self.assertIsInstance(init_res, dict)
        self.assertTrue("capabilities" in init_res or "serverInfo" in init_res)

        tools = mcp.tools_list()
        tool_map = {t["name"]: t for t in tools if isinstance(t, dict) and "name" in t}
        required_tools = (
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
        )
        for name in required_tools:
            self.assertIn(name, tool_map)

        assert_r3_compliant(json.dumps(tools))

        sess_id = "mcp_s1"
        mcp.tools_call(
            "mimikyu_session_new",
            _pick_mcp_args(
                tool_map["mimikyu_session_new"],
                {"ephemeral": True, "session": (("session", "session_id"), sess_id)},
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
        agree_ref = self.require_ref(snap, role="checkbox", name_substr="Agree to terms")
        tier_ref = self.require_ref(snap, role="radio", name_substr="Accurate")
        submit_ref = self.require_ref(snap, role="button", name_substr="Submit")

        mcp.tools_call(
            "mimikyu_fill",
            _pick_mcp_args(
                tool_map["mimikyu_fill"],
                {
                    "target": (("target", "ref", "selector"), name_ref),
                    "value": (("value", "text"), "MCP Agent 06"),
                    "session": (("session", "session_id"), sess_id),
                },
            ),
        )
        for ref in (agree_ref, tier_ref, submit_ref):
            mcp.tools_call(
                "mimikyu_click",
                _pick_mcp_args(
                    tool_map["mimikyu_click"],
                    {
                        "target": (("target", "ref", "selector"), ref),
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
        for token in ("Submission Received", "MCP Agent 06", "agree", "accurate"):
            self.assertIn(token, ext_blob)

        shot_path = art_dir / "mcp_submit.png"
        shot_res = mcp.tools_call(
            "mimikyu_screenshot",
            _pick_mcp_args(
                tool_map["mimikyu_screenshot"],
                {
                    "out_path": (("out_path", "path", "output"), str(shot_path)),
                    "session": (("session", "session_id"), sess_id),
                },
            ),
        )
        if shot_path.is_file():
            png_bytes = shot_path.read_bytes()
        else:
            # Fallback if screenshot tool returns base64 image block in MCP content
            png_bytes = b""
            for item in shot_res.get("content", []):
                if isinstance(item, dict) and isinstance(item.get("data"), str):
                    png_bytes = base64.b64decode(item["data"])
                    break
        self.assertTrue(png_bytes.startswith(PNG_MAGIC), "Expected valid PNG from MCP screenshot")

        mcp.tools_call(
            "mimikyu_session_close",
            _pick_mcp_args(
                tool_map["mimikyu_session_close"],
                {"session": (("session", "session_id", "id"), sess_id)},
            ),
        )
        mcp.close()
        self.assert_no_orphan_processes(tracked_state_dir, timeout_s=2.0)

    def test_workload_07_coherent_profile_proxy_bench_and_teardown(self) -> None:
        proxy = self.make_local_proxy()
        art_dir = self.make_temp_dir()
        client = self.make_client(timeout=120.0)
        tracked_state_dir = str(client.state_dir)

        client.session_new(
            profile="tenant_a",
            preset="win11",
            proxy=proxy.proxy_url,
            locale="de-DE",
            timezone="Europe/Berlin",
            session="tenant_a",
            check=True,
        )
        client.open(f"{self.base_url}/echo-env", session="tenant_a", check=True)
        env_rep = self.wait_env_ready(client, session="tenant_a")

        self.assertGreater(proxy.request_count, 0)
        self.assertTrue(
            any("/echo-env" in u for u in proxy.seen_urls),
            f"Expected /echo-env in proxy.seen_urls: {proxy.seen_urls!r}",
        )

        self.assertEqual(env_rep["httpHeaders"]["userAgent"], env_rep["navigator"]["userAgent"])
        self.assertIn("Windows", env_rep["navigator"]["userAgent"])
        self.assertEqual(env_rep["navigator"]["platform"], "Win32")
        self.assertTrue(env_rep["navigator"]["language"].lower().startswith("de"))
        self.assertIn("de", (env_rep["httpHeaders"].get("acceptLanguage") or "").lower())
        self.assertEqual(env_rep["intl"]["timezone"], "Europe/Berlin")
        self.assertTrue(env_rep["workerConsistency"]["consistent"])
        self.assertEqual(env_rep["workerConsistency"]["mismatches"], [])
        self.assertTrue(env_rep["isolatedWorld"]["isolatedWorldClean"])
        self.assertEqual(env_rep["isolatedWorld"]["leakedKeys"], [])
        webrtc = env_rep.get("webrtc", {})
        if webrtc.get("status") == "ok" or webrtc.get("candidates"):
            self.assertIs(webrtc.get("hasHostCandidate"), False)

        # Export tenant_a state -> import into fresh tenant_b
        client.open(
            f"{self.base_url}/state?set_cookie=t7_cookie&httponly=t7_ho&set_storage=t7_ls",
            session="tenant_a",
            check=True,
        )
        self.read_state_report(client, session="tenant_a")

        export_file = art_dir / "tenant_a_export.json"
        client.session_export(export_file, session="tenant_a", check=True)
        self.assertTrue(export_file.is_file() and export_file.stat().st_size > 0)
        client.session_close("tenant_a", check=True)

        client.session_new(ephemeral=True, session="tenant_b", check=True)
        client.session_import(export_file, session="tenant_b", check=True)
        client.open(f"{self.base_url}/state", session="tenant_b", check=True)
        rep_b = self.read_state_report(client, session="tenant_b")
        self.assertEqual(rep_b["serverCookies"].get("mimikyu_normal"), "t7_cookie")
        self.assertEqual(rep_b["serverCookies"].get("mimikyu_httponly"), "t7_ho")
        self.assertEqual(rep_b["localStorage"].get("token"), "t7_ls")
        client.session_close("tenant_b", check=True)

        # Reproducible local benchmark
        bench_res = client.bench(iterations=2, warmup=1, json_output=True, check=True, timeout=90.0)
        self.assertTrue(bench_res.ok)
        self.assertIsInstance(bench_res.data, dict)
        self.assertGreater(len(bench_res.data), 0)

        client.close()
        self.assert_no_orphan_processes(tracked_state_dir, timeout_s=2.0)


if __name__ == "__main__":
    unittest.main()
