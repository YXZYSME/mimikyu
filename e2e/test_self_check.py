"""Self-check unittest suite for e2e/ infrastructure (no compiled binary needed)."""

from __future__ import annotations

import hashlib
import http.client
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.error
import urllib.request
from pathlib import Path

REPO_ROOT: Path = Path(__file__).resolve().parent.parent
if str(REPO_ROOT) not in sys.path:
    sys.path.insert(0, str(REPO_ROOT))

from e2e.helpers import (  # noqa: E402
    R3_FORBIDDEN_KEYWORDS,
    CommandError,
    LocalTestCluster,
    McpClient,
    McpError,
    MimikyuClient,
    assert_r3_compliant,
    check_r3_compliance,
    extract_all_ax_refs,
    extract_ax_ref,
    get_config,
)
from e2e.local_server import LocalHttpProxy  # noqa: E402


def _http_get(
    base_url: str,
    path: str,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict[str, list[str]], bytes]:
    parsed = urllib.request.urlparse(base_url)
    conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=5.0)
    try:
        conn.request("GET", path, headers=headers or {})
        resp = conn.getresponse()
        body = resp.read()
        hdrs: dict[str, list[str]] = {}
        for k, v in resp.getheaders():
            hdrs.setdefault(k.lower(), []).append(v)
        return resp.status, hdrs, body
    finally:
        conn.close()


def _http_request(
    base_url: str,
    method: str,
    path: str,
    body: bytes | None = None,
    headers: dict[str, str] | None = None,
) -> tuple[int, dict[str, list[str]], bytes]:
    parsed = urllib.request.urlparse(base_url)
    conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=5.0)
    try:
        conn.request(method, path, body=body, headers=headers or {})
        resp = conn.getresponse()
        data = resp.read()
        hdrs: dict[str, list[str]] = {}
        for k, v in resp.getheaders():
            hdrs.setdefault(k.lower(), []).append(v)
        return resp.status, hdrs, data
    finally:
        conn.close()


def _write_executable_script(directory: Path, name: str, body: str) -> Path:
    script = directory / name
    script.write_text(body, encoding="utf-8")
    script.chmod(0o755)
    return script


class TestLocalTestServerEndpoints(unittest.TestCase):
    cluster: LocalTestCluster

    @classmethod
    def setUpClass(cls) -> None:
        cls.cluster = LocalTestCluster().start()

    @classmethod
    def tearDownClass(cls) -> None:
        cls.cluster.stop()

    def test_index_and_health(self) -> None:
        status, hdrs, body = _http_get(self.cluster.base_url, "/")
        self.assertEqual(status, 200)
        self.assertIn("text/html", hdrs["content-type"][0])
        text = body.decode("utf-8")
        self.assertIn("mimikyu E2E Test Harness", text)
        self.assertIn("/workflow/wizard", text)

        status, _, body = _http_get(self.cluster.base_url, "/health")
        self.assertEqual(status, 200)
        data = json.loads(body.decode("utf-8"))
        self.assertEqual(data["status"], "ok")
        self.assertEqual(data["port"], self.cluster.primary.port)

    def test_navigation_redirects_and_history(self) -> None:
        st1, _, b1 = _http_get(self.cluster.base_url, "/nav/page1")
        st2, _, b2 = _http_get(self.cluster.base_url, "/nav/page2")
        self.assertEqual(st1, 200)
        self.assertEqual(st2, 200)
        self.assertIn("Page 1", b1.decode("utf-8"))
        self.assertIn("Page 2", b2.decode("utf-8"))

        st_r, hdrs_r, _ = _http_get(self.cluster.base_url, "/redirect")
        self.assertEqual(st_r, 302)
        self.assertEqual(hdrs_r["location"][0], "/nav/page2")

        # Guard against backslash open-redirect and CRLF header injection
        for malicious in ("/\\evil.example/", "/nav/page1\r\nX-Injected: 1"):
            quoted = urllib.request.quote(malicious, safe="/")
            st_m, hdrs_m, _ = _http_get(self.cluster.base_url, f"/redirect?to={quoted}")
            self.assertEqual(st_m, 302)
            self.assertEqual(hdrs_m["location"][0], "/nav/page2")
            self.assertNotIn("x-injected", hdrs_m)

        st_c, hdrs_c, _ = _http_get(self.cluster.base_url, "/redirect-chain?hops=2")
        self.assertEqual(st_c, 302)
        self.assertEqual(hdrs_c["location"][0], "/redirect-chain?hops=1")

        with urllib.request.urlopen(f"{self.cluster.base_url}/redirect-chain?hops=2", timeout=5.0) as resp:
            final_html = resp.read().decode("utf-8")
            self.assertEqual(resp.status, 200)
            self.assertIn("Redirect Chain Complete", final_html)

        st_h, _, bh = _http_get(self.cluster.base_url, "/history")
        self.assertEqual(st_h, 200)
        self.assertIn("history.pushState", bh.decode("utf-8"))

    def test_form_and_submit_urlencoded_json_multipart(self) -> None:
        st_f, _, bf = _http_get(self.cluster.base_url, "/form")
        self.assertEqual(st_f, 200)
        form_html = bf.decode("utf-8")
        for needle in ('id="name"', 'id="role"', 'id="agree"', 'type="radio"', 'type="file"'):
            self.assertIn(needle, form_html)

        st_get, _, _ = _http_get(self.cluster.base_url, "/submit")
        self.assertEqual(st_get, 405)

        # application/x-www-form-urlencoded
        st_u, _, bu = _http_request(
            self.cluster.base_url,
            "POST",
            "/submit",
            body=b"name=alice&role=agent&agree=on",
            headers={
                "Content-Type": "application/x-www-form-urlencoded",
                "Accept": "application/json",
            },
        )
        self.assertEqual(st_u, 200)
        ju = json.loads(bu.decode("utf-8"))
        self.assertTrue(ju["ok"])
        self.assertEqual(ju["fields"]["name"], "alice")
        self.assertEqual(ju["fields"]["role"], "agent")

        # application/json
        st_j, _, bj = _http_request(
            self.cluster.base_url,
            "POST",
            "/submit",
            body=json.dumps({"name": "bob", "tier": "accurate"}).encode("utf-8"),
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(st_j, 200)
        jj = json.loads(bj.decode("utf-8"))
        self.assertEqual(jj["fields"]["name"], "bob")

        # multipart/form-data with uploaded file
        boundary = "----MimikyuBoundary123"
        file_bytes = b"hello-upload-payload\n"
        mp_body = (
            f"--{boundary}\r\n"
            f'Content-Disposition: form-data; name="name"\r\n\r\ncharlie\r\n'
            f"--{boundary}\r\n"
            f'Content-Disposition: form-data; name="upload"; filename="hello.txt"\r\n'
            f"Content-Type: text/plain\r\n\r\n"
        ).encode("utf-8") + file_bytes + f"\r\n--{boundary}--\r\n".encode("utf-8")
        st_m, _, bm = _http_request(
            self.cluster.base_url,
            "POST",
            "/submit",
            body=mp_body,
            headers={
                "Content-Type": f"multipart/form-data; boundary={boundary}",
                "Accept": "application/json",
            },
        )
        self.assertEqual(st_m, 200)
        jm = json.loads(bm.decode("utf-8"))
        self.assertEqual(jm["fields"]["name"], "charlie")
        self.assertEqual(len(jm["files"]), 1)
        self.assertEqual(jm["files"][0]["filename"], "hello.txt")
        self.assertEqual(jm["files"][0]["size"], len(file_bytes))
        self.assertEqual(jm["files"][0]["sha256"], hashlib.sha256(file_bytes).hexdigest())

        # Malformed JSON and invalid UTF-8 return 400 Bad Request
        for bad_body in (b"{malformed json", b"\xff\xfe\xfd"):
            st_bad, _, b_bad = _http_request(
                self.cluster.base_url,
                "POST",
                "/submit",
                body=bad_body,
                headers={"Content-Type": "application/json"},
            )
            self.assertEqual(st_bad, 400)
            j_bad = json.loads(b_bad.decode("utf-8"))
            self.assertFalse(j_bad["ok"])
            self.assertEqual(j_bad["error"], "invalid_json")

    def test_spa_catalog_and_console_errors(self) -> None:
        st_spa, _, b_spa = _http_get(self.cluster.base_url, "/spa")
        self.assertEqual(st_spa, 200)
        self.assertIn("spa-modal", b_spa.decode("utf-8"))

        t0 = time.monotonic()
        st_api, _, b_api = _http_get(self.cluster.base_url, "/api/spa-data?delay_ms=50")
        elapsed = time.monotonic() - t0
        self.assertEqual(st_api, 200)
        self.assertGreaterEqual(elapsed, 0.04)
        self.assertEqual(len(json.loads(b_api.decode("utf-8"))["items"]), 3)

        st_cat, _, b_cat = _http_get(self.cluster.base_url, "/api/catalog?category=compute&page=1")
        self.assertEqual(st_cat, 200)
        cat_data = json.loads(b_cat.decode("utf-8"))
        self.assertTrue(all(it["category"] == "compute" for it in cat_data["items"]))

        st_d, _, bd = _http_request(
            self.cluster.base_url,
            "POST",
            "/api/data",
            body=b'{"probe":1}',
            headers={"Content-Type": "application/json"},
        )
        self.assertEqual(st_d, 200)
        self.assertEqual(json.loads(bd.decode("utf-8"))["echo"], {"probe": 1})

        st_ce, _, b_ce = _http_get(self.cluster.base_url, "/console-errors?uncaught=1&assets=1")
        self.assertEqual(st_ce, 200)
        ce_text = b_ce.decode("utf-8")
        self.assertIn("mimikyu-e2e-error", ce_text)
        self.assertIn("mimikyu-e2e-uncaught", ce_text)

    def test_frames_and_tabs_routes(self) -> None:
        st_fr, _, b_fr = _http_get(
            self.cluster.base_url,
            f"/frames?cross_origin={urllib.request.quote(self.cluster.cross_url, safe='')}",
        )
        self.assertEqual(st_fr, 200)
        self.assertIn(f"{self.cluster.cross_url}/frame-child", b_fr.decode("utf-8"))

        st_same, _, b_same = _http_get(self.cluster.base_url, "/frame-same")
        self.assertEqual(st_same, 200)
        same_txt = b_same.decode("utf-8")
        for marker in ("same-frame-input", "frame-input", "frame-send", "frame-ack", "__FRAME_MARKER"):
            self.assertIn(marker, same_txt)

        st_child, hdrs_child, b_child = _http_get(self.cluster.cross_url, "/frame-child")
        self.assertEqual(st_child, 200)
        self.assertEqual(hdrs_child["access-control-allow-origin"][0], "*")
        child_txt = b_child.decode("utf-8")
        for marker in ("cross-frame-input", "frame-input", "frame-send", "frame-ack", "__FRAME_MARKER"):
            self.assertIn(marker, child_txt)

        st_t, _, bt = _http_get(self.cluster.base_url, "/tabs")
        self.assertEqual(st_t, 200)
        self.assertIn("window.open", bt.decode("utf-8"))

        st_tc, _, btc = _http_get(self.cluster.base_url, "/tab-child?token=tok42")
        self.assertEqual(st_tc, 200)
        self.assertIn("tok42", btc.decode("utf-8"))

    def test_downloads_and_sha256_integrity(self) -> None:
        st_t, hdrs_t, bt = _http_get(self.cluster.base_url, "/download/sample.txt")
        self.assertEqual(st_t, 200)
        self.assertIn("attachment", hdrs_t["content-disposition"][0])

        st_b, hdrs_b, bb = _http_get(self.cluster.base_url, "/download/sample.bin")
        self.assertEqual(st_b, 200)
        self.assertIn("attachment", hdrs_b["content-disposition"][0])
        self.assertEqual(len(bb), 4096)

        st_s, _, bs = _http_get(self.cluster.base_url, "/download/sha256")
        self.assertEqual(st_s, 200)
        manifest = json.loads(bs.decode("utf-8"))
        self.assertEqual(manifest["sample.txt"]["sha256"], hashlib.sha256(bt).hexdigest())
        self.assertEqual(manifest["sample.bin"]["sha256"], hashlib.sha256(bb).hexdigest())

    def test_echo_headers_and_binary_assets(self) -> None:
        st_h, _, bh = _http_get(
            self.cluster.base_url,
            "/echo-headers",
            headers={"X-Mimikyu-Probe": "ok"},
        )
        self.assertEqual(st_h, 200)
        self.assertEqual(json.loads(bh.decode("utf-8"))["headers"]["x-mimikyu-probe"], "ok")

        st_p, hp, bp = _http_get(self.cluster.base_url, "/assets/img.png")
        self.assertEqual(st_p, 200)
        self.assertEqual(hp["content-type"][0], "image/png")
        self.assertTrue(bp.startswith(b"\x89PNG\r\n\x1a\n"))
        self.assertEqual(bp[12:16], b"IHDR")
        self.assertIn(b"IEND", bp[-12:])

        st_w, hw, bw = _http_get(self.cluster.base_url, "/assets/font.woff2")
        self.assertEqual(st_w, 200)
        self.assertEqual(hw["content-type"][0], "font/woff2")
        self.assertTrue(bw.startswith(b"wOF2"))

        st_m, hm, bm = _http_get(self.cluster.base_url, "/assets/media.mp4")
        self.assertEqual(st_m, 200)
        self.assertEqual(hm["content-type"][0], "video/mp4")
        self.assertIn(b"ftyp", bm[:16])

        st_m2, hm2, bm2 = _http_get(
            self.cluster.base_url,
            "/assets/media.mp4",
            headers={"Range": "bytes=0-15"},
        )
        self.assertEqual(st_m2, 206)
        self.assertEqual(len(bm2), 16)
        self.assertIn("content-range", hm2)

        # Suffix range bytes=-10 returns the last 10 bytes
        st_suf, hsuf, bsuf = _http_get(
            self.cluster.base_url,
            "/assets/media.mp4",
            headers={"Range": "bytes=-10"},
        )
        self.assertEqual(st_suf, 206)
        self.assertEqual(len(bsuf), 10)
        self.assertEqual(bsuf, bm[-10:])
        self.assertEqual(hsuf["content-range"][0], f"bytes {len(bm) - 10}-{len(bm) - 1}/{len(bm)}")

        # Out-of-bounds, inverted, zero-suffix, and malformed ranges return 416
        for bad_range in ("bytes=999-1000", "bytes=20-5", "bytes=-0", "bytes=abc-def", "bytes=-", "bytes=10"):
            st_416, h416, _ = _http_get(
                self.cluster.base_url,
                "/assets/media.mp4",
                headers={"Range": bad_range},
            )
            self.assertEqual(st_416, 416, msg=f"expected 416 for {bad_range}")
            self.assertEqual(h416["content-range"][0], f"bytes */{len(bm)}")

    def test_echo_env_and_worker_env(self) -> None:
        st, _, body = _http_get(
            self.cluster.base_url,
            "/echo-env",
            headers={
                "User-Agent": "MimikyuProbe/1.0",
                "Accept-Language": "fr-FR,fr;q=0.9",
                "Sec-CH-UA": '"Chromium";v="143"',
                "Sec-CH-UA-Platform": '"macOS"',
            },
        )
        self.assertEqual(st, 200)
        txt = body.decode("utf-8")
        self.assertIn("MimikyuProbe/1.0", txt)
        self.assertIn("fr-FR,fr;q=0.9", txt)
        for marker in (
            "server-env-data",
            "env-json",
            "__ENV_REPORT",
            "WEBGL_debug_renderer_info",
            "AudioContext",
            "RTCPeerConnection",
            "__mimikyu",
            "/worker-env.js",
        ):
            self.assertIn(marker, txt)

        stw, hw, bw = _http_get(self.cluster.base_url, "/worker-env.js")
        self.assertEqual(stw, 200)
        self.assertIn("javascript", hw["content-type"][0])
        wtxt = bw.decode("utf-8")
        for marker in ("navigator.userAgent", "navigator.platform", "Intl.DateTimeFormat", "__mimikyu"):
            self.assertIn(marker, wtxt)

    def test_humanize_tracker_route(self) -> None:
        st, _, body = _http_get(self.cluster.base_url, "/humanize-tracker")
        self.assertEqual(st, 200)
        txt = body.decode("utf-8")
        for marker in (
            "tracker-json",
            "__TRACKER_EVENTS",
            "mousemove",
            "mousedown",
            "mouseup",
            "keydown",
            "keyup",
        ):
            self.assertIn(marker, txt)

    def test_state_cookies_and_etag_304(self) -> None:
        st1, hdrs1, _ = _http_get(
            self.cluster.base_url,
            "/state?set_cookie=alpha&httponly=secret1",
        )
        self.assertEqual(st1, 200)
        set_cookies = hdrs1.get("set-cookie", [])
        self.assertEqual(len(set_cookies), 2)
        normal = [c for c in set_cookies if "mimikyu_normal=alpha" in c]
        httponly = [c for c in set_cookies if "mimikyu_httponly=secret1" in c]
        self.assertEqual(len(normal), 1)
        self.assertNotIn("HttpOnly", normal[0])
        self.assertEqual(len(httponly), 1)
        self.assertIn("HttpOnly", httponly[0])

        cookie_hdr = "mimikyu_normal=alpha; mimikyu_httponly=secret1"
        st2, hdrs2, b2 = _http_get(
            self.cluster.base_url,
            "/state",
            headers={"Cookie": cookie_hdr},
        )
        self.assertEqual(st2, 200)
        self.assertIn("secret1", b2.decode("utf-8"))
        etag = hdrs2["etag"][0]

        st3, hdrs3, b3 = _http_get(
            self.cluster.base_url,
            "/state",
            headers={"Cookie": cookie_hdr, "If-None-Match": etag},
        )
        self.assertEqual(st3, 304)
        self.assertEqual(hdrs3["etag"][0], etag)
        self.assertEqual(b3, b"")

        st4, _, b4 = _http_get(
            self.cluster.base_url,
            "/state",
            headers={"Cookie": cookie_hdr, "If-None-Match": '"mismatch"'},
        )
        self.assertEqual(st4, 200)
        self.assertNotEqual(b4, b"")

    def test_five_workflow_pages(self) -> None:
        routes = (
            "/workflow/wizard",
            "/workflow/catalog",
            "/workflow/cross-frame",
            "/workflow/multi-tab",
            "/workflow/download-verify",
        )
        for route in routes:
            with self.subTest(route=route):
                st, hdrs, body = _http_get(self.cluster.base_url, route)
                self.assertEqual(st, 200)
                self.assertIn("text/html", hdrs["content-type"][0])
                text = body.decode("utf-8")
                self.assertIn("role=", text)
                self.assertTrue("<label" in text or "<a " in text)

    def test_unknown_route_returns_404(self) -> None:
        st, _, body = _http_get(self.cluster.base_url, "/definitely-missing")
        self.assertEqual(st, 404)
        self.assertEqual(json.loads(body.decode("utf-8"))["error"], "not_found")


class TestLocalHttpProxyAndDualOriginCluster(unittest.TestCase):
    def test_dual_origin_and_proxy_forwarding(self) -> None:
        with LocalTestCluster() as cluster:
            self.assertNotEqual(cluster.base_url, cluster.cross_url)
            self.assertNotEqual(cluster.base_url, cluster.proxy_url)
            self.assertTrue(cluster.base_url.startswith("http://127.0.0.1:"))
            self.assertTrue(cluster.cross_url.startswith("http://127.0.0.1:"))
            self.assertTrue(cluster.proxy_url.startswith("http://127.0.0.1:"))

            # Preserve 302 through HTTP proxy via raw HTTPConnection
            proxy_parsed = urllib.request.urlparse(cluster.proxy_url)
            pconn = http.client.HTTPConnection(proxy_parsed.hostname, proxy_parsed.port, timeout=5.0)
            try:
                pconn.request("GET", f"{cluster.base_url}/redirect")
                presp = pconn.getresponse()
                presp.read()
                self.assertEqual(presp.status, 302)
                self.assertEqual(presp.getheader("Location"), "/nav/page2")
            finally:
                pconn.close()

            # ProxyHandler GET through proxy
            opener = urllib.request.build_opener(
                urllib.request.ProxyHandler({"http": cluster.proxy_url})
            )
            with opener.open(f"{cluster.base_url}/echo-headers", timeout=5.0) as resp:
                self.assertEqual(resp.status, 200)
                payload = json.loads(resp.read().decode("utf-8"))
                self.assertEqual(payload["method"], "GET")

            self.assertEqual(cluster.proxy.request_count, 2)
            self.assertIn(f"{cluster.base_url}/redirect", cluster.proxy.seen_urls)
            self.assertIn(f"{cluster.base_url}/echo-headers", cluster.proxy.seen_urls)

            # CONNECT tunnel through proxy
            with socket.create_connection(
                (proxy_parsed.hostname, proxy_parsed.port), timeout=5.0
            ) as sock:
                target = f"127.0.0.1:{cluster.primary.port}"
                sock.sendall(
                    f"CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n".encode("ascii")
                )
                buf = b""
                while b"\r\n\r\n" not in buf:
                    chunk = sock.recv(4096)
                    self.assertTrue(chunk)
                    buf += chunk
                self.assertIn(b"200 Connection Established", buf)

                sock.sendall(
                    f"GET /health HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n".encode("ascii")
                )
                tunneled = b""
                while True:
                    part = sock.recv(4096)
                    if not part:
                        break
                    tunneled += part
                self.assertIn(b"HTTP/1.1 200", tunneled)
                self.assertIn(b'"status": "ok"', tunneled)

            self.assertIn(f"CONNECT 127.0.0.1:{cluster.primary.port}", cluster.proxy.seen_urls)
            cluster.proxy.reset_stats()
            self.assertEqual(cluster.proxy.request_count, 0)
            self.assertEqual(cluster.proxy.seen_urls, [])

    def test_proxy_blocks_external_hosts(self) -> None:
        with LocalHttpProxy(allow_external=False) as proxy:
            conn = http.client.HTTPConnection("127.0.0.1", proxy.port, timeout=3.0)
            try:
                conn.request("GET", "http://example.com/")
                resp = conn.getresponse()
                resp.read()
                self.assertEqual(resp.status, 403)
            finally:
                conn.close()

            with socket.create_connection(("127.0.0.1", proxy.port), timeout=3.0) as sock:
                sock.sendall(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
                reply = sock.recv(4096)
                self.assertIn(b"403", reply)

    def test_proxy_connect_half_close_preserves_upstream_response(self) -> None:
        with LocalTestCluster() as cluster:
            proxy_parsed = urllib.request.urlparse(cluster.proxy_url)
            with socket.create_connection(
                (proxy_parsed.hostname, proxy_parsed.port), timeout=5.0
            ) as sock:
                target = f"127.0.0.1:{cluster.primary.port}"
                sock.sendall(
                    f"CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n".encode("ascii")
                )
                buf = b""
                while b"\r\n\r\n" not in buf:
                    chunk = sock.recv(4096)
                    self.assertTrue(chunk)
                    buf += chunk
                self.assertIn(b"200 Connection Established", buf)

                sock.sendall(
                    f"GET /health HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n".encode("ascii")
                )
                sock.shutdown(socket.SHUT_WR)
                tunneled = b""
                while True:
                    part = sock.recv(4096)
                    if not part:
                        break
                    tunneled += part
                self.assertIn(b"HTTP/1.1 200", tunneled)
                self.assertIn(b'"status": "ok"', tunneled)


class TestRunE2eCliAndCleanFailure(unittest.TestCase):
    def _run_runner(self, *args: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(REPO_ROOT / "e2e" / "run_e2e.py"), *args],
            cwd=str(REPO_ROOT),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=15.0,
        )

    def test_cli_help_flags(self) -> None:
        proc = self._run_runner("--help")
        self.assertEqual(proc.returncode, 0)
        for flag in ("--tier", "--bin", "--engine", "--self-test", "--json", "-v"):
            self.assertIn(flag, proc.stdout)

    def test_missing_binary_clean_failure(self) -> None:
        proc = self._run_runner("--tier", "1", "--bin", "/nonexistent/mimikyu")
        self.assertNotEqual(proc.returncode, 0)
        self.assertNotIn("Traceback", proc.stdout + proc.stderr)
        self.assertIn("does not exist", proc.stderr)

        proc_json = self._run_runner("--tier", "1", "--bin", "/nonexistent/mimikyu", "--json")
        self.assertNotEqual(proc_json.returncode, 0)
        self.assertNotIn("Traceback", proc_json.stdout + proc_json.stderr)
        parsed = json.loads(proc_json.stdout)
        self.assertFalse(parsed["ok"])
        self.assertIn("does not exist", parsed["error"])

    def test_directory_and_non_executable_binary_clean_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            proc_dir = self._run_runner("--tier", "1", "--bin", tmp)
            self.assertNotEqual(proc_dir.returncode, 0)
            self.assertNotIn("Traceback", proc_dir.stdout + proc_dir.stderr)

            non_exec = Path(tmp) / "mimikyu_nonexec"
            non_exec.write_text("#!/bin/sh\necho ok\n", encoding="utf-8")
            non_exec.chmod(0o644)
            proc_nx = self._run_runner("--tier", "1", "--bin", str(non_exec))
            self.assertNotEqual(proc_nx.returncode, 0)
            self.assertNotIn("Traceback", proc_nx.stdout + proc_nx.stderr)

    def test_failing_and_silent_stub_binary_clean_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            fail_stub = _write_executable_script(
                Path(tmp),
                "fail_stub",
                "#!/bin/sh\necho boom >&2\nexit 1\n",
            )
            proc_fail = self._run_runner("--tier", "1", "--bin", str(fail_stub))
            self.assertNotEqual(proc_fail.returncode, 0)
            self.assertNotIn("Traceback", proc_fail.stdout + proc_fail.stderr)

            silent_stub = _write_executable_script(
                Path(tmp),
                "silent_stub",
                "#!/bin/sh\nexit 0\n",
            )
            proc_silent = self._run_runner("--tier", "1", "--bin", str(silent_stub))
            self.assertNotEqual(proc_silent.returncode, 0)
            self.assertNotIn("Traceback", proc_silent.stdout + proc_silent.stderr)

    def test_invalid_tier_and_bogus_flag_clean_failure(self) -> None:
        for bad_args in (("--tier", "99"), ("--bogus-flag",), ("--bogus-flag", "--json")):
            proc = self._run_runner(*bad_args)
            self.assertNotEqual(proc.returncode, 0)
            self.assertNotIn("Traceback", proc.stdout + proc.stderr)
            if "--json" in bad_args:
                parsed = json.loads(proc.stdout)
                self.assertFalse(parsed["ok"])


class TestMimikyuClientAndMcpClientWithStubs(unittest.TestCase):
    def test_get_config_reads_env_dynamically(self) -> None:
        keys = (
            "MIMIKYU_BIN",
            "MIMIKYU_E2E_BASE_URL",
            "MIMIKYU_E2E_CROSS_URL",
            "MIMIKYU_E2E_PROXY_URL",
            "MIMIKYU_E2E_ENGINE",
        )
        prev = {k: os.environ.get(k) for k in keys}
        try:
            os.environ["MIMIKYU_BIN"] = "/tmp/fake-mimikyu"
            os.environ["MIMIKYU_E2E_BASE_URL"] = "http://127.0.0.1:10001"
            os.environ["MIMIKYU_E2E_CROSS_URL"] = "http://127.0.0.1:10002"
            os.environ["MIMIKYU_E2E_PROXY_URL"] = "http://127.0.0.1:10003"
            os.environ["MIMIKYU_E2E_ENGINE"] = "firefox"
            cfg = get_config()
            self.assertEqual(cfg.bin_path, "/tmp/fake-mimikyu")
            self.assertEqual(cfg.base_url, "http://127.0.0.1:10001")
            self.assertEqual(cfg.cross_url, "http://127.0.0.1:10002")
            self.assertEqual(cfg.proxy_url, "http://127.0.0.1:10003")
            self.assertEqual(cfg.engine, "firefox")
        finally:
            for k, v in prev.items():
                if v is None:
                    os.environ.pop(k, None)
                else:
                    os.environ[k] = v

    def test_mimikyu_client_cli_methods_and_cleanup(self) -> None:
        stub_src = f"""#!{sys.executable}
import json, sys
args = sys.argv[1:]
if "session" in args and "list" in args:
    print(json.dumps({{"ok": True, "data": [{{"id": "s1"}}]}}))
else:
    print(json.dumps({{"ok": True, "data": {{"args": args}}}}))
"""
        with tempfile.TemporaryDirectory() as tmp:
            stub = _write_executable_script(Path(tmp), "mimikyu_stub", stub_src)
            with MimikyuClient(stub, engine="chromium", session="s1") as client:
                state_dir = client.state_dir
                self.assertTrue(state_dir.exists())

                r_doc = client.doctor()
                self.assertTrue(r_doc.ok)
                self.assertIn("doctor", r_doc.data["args"])
                self.assertNotIn("--engine", r_doc.data["args"])

                r_dae = client.daemon(idle_timeout=15)
                self.assertIn("daemon", r_dae.data["args"])
                self.assertIn("--idle-timeout", r_dae.data["args"])

                r_new = client.session_new(
                    virtual=True,
                    preset="macos",
                    locale="en-US",
                    timezone="America/New_York",
                    block_images=True,
                    humanize=True,
                )
                self.assertIn("new", r_new.data["args"])
                self.assertIn("--virtual", r_new.data["args"])
                self.assertIn("--humanize", r_new.data["args"])

                self.assertTrue(client.session_export(Path(tmp) / "exp.json").ok)
                self.assertTrue(client.session_import(Path(tmp) / "exp.json").ok)
                self.assertTrue(client.open("http://127.0.0.1:1234/").ok)
                self.assertTrue(client.back().ok)
                self.assertTrue(client.forward().ok)
                self.assertTrue(client.reload().ok)
                self.assertIn("--diff", client.snapshot(diff=True, max_depth=3, interactive_only=True).data["args"])
                self.assertIn("--humanize", client.click("@e1", humanize=True).data["args"])
                self.assertTrue(client.fill("@e2", "val").ok)
                self.assertTrue(client.type("@e2", "abc").ok)
                self.assertTrue(client.select("@e3", "opt").ok)
                self.assertTrue(client.upload("@e4", Path(tmp) / "f.txt").ok)

                r_scroll = client.scroll(-50, -120, humanize=True)
                self.assertIn("--", r_scroll.data["args"])
                self.assertIn("-50", r_scroll.data["args"])

                self.assertTrue(client.press("Enter").ok)
                self.assertIn("--idle", client.wait(idle=True, timeout_ms=500).data["args"])
                self.assertIn("--ops", client.batch(ops=[{"op": "click", "target": "@e1"}]).data["args"])
                self.assertTrue(client.extract("tables").ok)
                self.assertIn("--main-world", client.eval("1+1", main_world=True).data["args"])
                self.assertIn("--full-page", client.screenshot(Path(tmp) / "a.png", full_page=True).data["args"])
                self.assertTrue(client.pdf(Path(tmp) / "a.pdf").ok)
                self.assertTrue(client.logs("network").ok)
                self.assertTrue(client.bench(iterations=2, warmup=1).ok)

            self.assertFalse(state_dir.exists())

    def test_mimikyu_client_error_and_timeout_cleanup(self) -> None:
        err_stub_src = f"""#!{sys.executable}
import json, sys, time
args = sys.argv[1:]
if "--sleep" in args:
    time.sleep(5)
print(json.dumps({{"ok": False, "error": "boom"}}))
sys.exit(1)
"""
        with tempfile.TemporaryDirectory() as tmp:
            stub = _write_executable_script(Path(tmp), "err_stub", err_stub_src)
            with MimikyuClient(stub, timeout=0.3) as client:
                res = client.run("doctor", check=False)
                self.assertFalse(res.ok)
                self.assertEqual(res.error, "boom")
                with self.assertRaises(CommandError):
                    client.run("doctor", check=True)

                t0 = time.monotonic()
                timed = client.run("--sleep", check=False, timeout=0.25)
                elapsed = time.monotonic() - t0
                self.assertEqual(timed.returncode, -124)
                self.assertLess(elapsed, 2.0)

    def test_extract_ax_ref_and_all_refs(self) -> None:
        structured = {
            "ok": True,
            "data": {
                "nodes": [
                    {"ref": "@e1", "role": "textbox", "name": "Full Name"},
                    {
                        "ref": "@e2",
                        "role": "button",
                        "name": "Submit Order",
                        "children": [{"ref": "@e3", "role": "link", "name": "Cancel"}],
                    },
                ]
            },
        }
        self.assertEqual(extract_ax_ref(structured, role="button", name_substr="submit"), "@e2")
        self.assertEqual(extract_ax_ref(structured, role="link", name_substr="cancel"), "@e3")
        self.assertIsNone(extract_ax_ref(structured, role="checkbox", name_substr="missing"))

        compact_text = '@e1 [textbox] "Email"\n@e2 [button] "Continue"\n@e3 [button] "Back"'
        self.assertEqual(extract_ax_ref(compact_text, role="button", name_substr="continue"), "@e2")
        self.assertEqual(extract_all_ax_refs(compact_text, role="button"), ["@e2", "@e3"])

        mcp_res = {"content": [{"type": "text", "text": compact_text}]}
        self.assertEqual(extract_ax_ref(mcp_res, role="textbox", name_substr="email"), "@e1")

        ref_map = {"@e7": {"role": "checkbox", "name": "Accept Policy"}}
        self.assertEqual(extract_ax_ref(ref_map, role="checkbox", name_substr="policy"), "@e7")

        # Wrapper keys ("result", "root", "ax_tree", custom nested containers) & cycle safety
        wrapped_root = {"result": {"root": {"ref": "@e21", "role": "button", "name": "Nested"}}}
        self.assertEqual(extract_ax_ref(wrapped_root, role="button", name_substr="nested"), "@e21")

        wrapped_ax = {
            "content": [
                {
                    "type": "text",
                    "text": json.dumps({"ax_tree": [{"ref": "@e22", "role": "button", "name": "AXTree"}]}),
                }
            ]
        }
        self.assertEqual(extract_ax_ref(wrapped_ax, role="button", name_substr="axtree"), "@e22")

        wrapped_custom = {"envelope": {"panel": [{"ref": "@e23", "role": "link", "name": "Details"}]}}
        self.assertEqual(extract_ax_ref(wrapped_custom, role="link", name_substr="details"), "@e23")

        cyclic: dict[str, object] = {}
        cyclic["self"] = cyclic
        self.assertIsNone(extract_ax_ref(cyclic, role="button"))

        # Bracketed [role] precision: must not false-match role substring inside quoted element name
        ambiguous_text = '@e15 [link] "Submit Button"\n@e16 [button] "Submit"'
        self.assertIsNone(extract_ax_ref('@e15 [link] "Submit Button"', role="button", name_substr="submit"))
        self.assertEqual(extract_ax_ref(ambiguous_text, role="button", name_substr="submit"), "@e16")

    def test_scroll_negative_places_json_before_double_dash(self) -> None:
        argparse_stub_src = f"""#!{sys.executable}
import argparse, json, sys
p = argparse.ArgumentParser()
p.add_argument("--state-dir", required=True)
p.add_argument("--engine", default="chromium")
p.add_argument("--session", default="default")
sub = p.add_subparsers(dest="cmd", required=True)
sc = sub.add_parser("scroll")
sc.add_argument("--json", action="store_true")
sc.add_argument("--humanize", action="store_true")
sc.add_argument("dx", type=int)
sc.add_argument("dy", type=int)
ns = p.parse_args()
print(json.dumps({{"ok": True, "data": {{"dx": ns.dx, "dy": ns.dy, "json": ns.json, "argv": sys.argv[1:]}}}}))
"""
        with tempfile.TemporaryDirectory() as tmp:
            stub = _write_executable_script(Path(tmp), "mimikyu_scroll_stub", argparse_stub_src)
            with MimikyuClient(stub, engine="chromium", session="s1") as client:
                res = client.scroll(-50, -120, humanize=True)
                self.assertTrue(res.ok)
                self.assertEqual(res.data["dx"], -50)
                self.assertEqual(res.data["dy"], -120)
                self.assertTrue(res.data["json"])
                argv = res.data["argv"]
                self.assertLess(argv.index("--json"), argv.index("--"))

    def test_process_group_grandchild_cleanup_on_timeout_and_mcp_close(self) -> None:
        cli_fork_stub_src = f"""#!{sys.executable}
import os, signal, subprocess, sys, time
if "wait" not in sys.argv:
    print('{{"ok": true, "data": []}}')
    sys.exit(0)
pid_file = os.environ["TEST_CLI_PID_FILE"]
child = subprocess.Popen(
    [sys.executable, "-c", "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)"]
)
with open(pid_file, "w", encoding="utf-8") as fh:
    fh.write(str(child.pid))
signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
time.sleep(30)
"""
        mcp_fork_stub_src = f"""#!{sys.executable}
import json, os, subprocess, sys
pid_file = os.environ["TEST_CHILD_PID_FILE"]
child = subprocess.Popen(
    [sys.executable, "-c", "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(30)"]
)
with open(pid_file, "w", encoding="utf-8") as fh:
    fh.write(str(child.pid))
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    if msg.get("id") is None:
        continue
    sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": msg["id"], "result": {{"ok": True}}}}) + "\\n")
    sys.stdout.flush()
"""
        with tempfile.TemporaryDirectory() as tmp:
            cli_pid_file = Path(tmp) / "cli_child.pid"
            cli_stub = _write_executable_script(Path(tmp), "cli_fork_stub", cli_fork_stub_src)
            with MimikyuClient(cli_stub, timeout=0.6) as client:
                res = client.run(
                    "wait",
                    timeout=0.6,
                    env_overrides={"TEST_CLI_PID_FILE": str(cli_pid_file)},
                )
                self.assertEqual(res.returncode, -124)
            cli_child_pid = int(cli_pid_file.read_text(encoding="utf-8").strip())
            try:
                with self.assertRaises(ProcessLookupError):
                    os.kill(cli_child_pid, 0)
            finally:
                try:
                    os.kill(cli_child_pid, 9)
                except OSError:
                    pass

            mcp_pid_file = Path(tmp) / "mcp_child.pid"
            mcp_stub = _write_executable_script(Path(tmp), "mcp_fork_stub", mcp_fork_stub_src)
            prev_env = os.environ.get("TEST_CHILD_PID_FILE")
            os.environ["TEST_CHILD_PID_FILE"] = str(mcp_pid_file)
            try:
                with McpClient(mcp_stub, timeout=5.0) as mcp:
                    self.assertTrue(mcp.initialize()["ok"])
                mcp_child_pid = int(mcp_pid_file.read_text(encoding="utf-8").strip())
                try:
                    with self.assertRaises(ProcessLookupError):
                        os.kill(mcp_child_pid, 0)
                finally:
                    try:
                        os.kill(mcp_child_pid, 9)
                    except OSError:
                        pass
            finally:
                if prev_env is None:
                    os.environ.pop("TEST_CHILD_PID_FILE", None)
                else:
                    os.environ["TEST_CHILD_PID_FILE"] = prev_env

    def test_mcp_client_newline_and_content_length_framing(self) -> None:
        nl_stub_src = f"""#!{sys.executable}
import json, sys
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    req_id = msg.get("id")
    if req_id is None:
        continue
    if method == "initialize":
        sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "method": "notifications/progress", "params": {{"step": 1}}}}) + "\\n")
        sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": req_id, "result": {{"serverInfo": {{"name": "stub"}}}}}}) + "\\n")
    elif method == "tools/list":
        sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": req_id, "result": {{"tools": [{{"name": "mimikyu_open"}}]}}}}) + "\\n")
    elif method == "tools/call":
        name = msg["params"]["name"]
        if name == "fail_tool":
            sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": req_id, "error": {{"code": -32000, "message": "tool failed"}}}}) + "\\n")
        else:
            sys.stdout.write(json.dumps({{"jsonrpc": "2.0", "id": req_id, "result": {{"content": [{{"type": "text", "text": "@e1 [button] \\"OK\\""}}]}}}}) + "\\n")
    sys.stdout.flush()
"""
        cl_stub_src = f"""#!{sys.executable}
import json, sys
def write_cl(obj):
    raw = json.dumps(obj).encode("utf-8")
    sys.stdout.buffer.write(f"Content-Length: {{len(raw)}}\\r\\n\\r\\n".encode("ascii") + raw)
    sys.stdout.buffer.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    req_id = msg.get("id")
    if req_id is None:
        continue
    write_cl({{"jsonrpc": "2.0", "id": req_id, "result": {{"protocolVersion": "2024-11-05"}}}})
"""
        with tempfile.TemporaryDirectory() as tmp:
            nl_stub = _write_executable_script(Path(tmp), "mcp_nl", nl_stub_src)
            with McpClient(nl_stub, timeout=5.0) as mcp:
                init_res = mcp.initialize()
                self.assertEqual(init_res["serverInfo"]["name"], "stub")
                tools = mcp.tools_list()
                self.assertEqual(tools[0]["name"], "mimikyu_open")
                call_res = mcp.tools_call("mimikyu_snapshot", {})
                self.assertEqual(extract_ax_ref(call_res, role="button", name_substr="OK"), "@e1")
                with self.assertRaises(McpError):
                    mcp.tools_call("fail_tool", {})

            cl_stub = _write_executable_script(Path(tmp), "mcp_cl", cl_stub_src)
            with McpClient(cl_stub, timeout=5.0) as mcp_cl:
                res = mcp_cl.initialize()
                self.assertEqual(res["protocolVersion"], "2024-11-05")

            # Large (>100 KB) single-write and split/chunked Content-Length responses
            large_cl_stub_src = f"""#!{sys.executable}
import json, sys, time
def write_cl(obj, chunked=False):
    raw = json.dumps(obj).encode("utf-8")
    hdr = f"Content-Length: {{len(raw)}}\\r\\n\\r\\n".encode("ascii")
    if not chunked:
        sys.stdout.buffer.write(hdr + raw)
        sys.stdout.buffer.flush()
    else:
        sys.stdout.buffer.write(hdr + raw[:40000])
        sys.stdout.buffer.flush()
        time.sleep(0.03)
        sys.stdout.buffer.write(raw[40000:90000])
        sys.stdout.buffer.flush()
        time.sleep(0.03)
        sys.stdout.buffer.write(raw[90000:])
        sys.stdout.buffer.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    req_id = msg.get("id")
    if req_id is None:
        continue
    if msg.get("method") == "initialize":
        blob = "A" * 150000
        write_cl({{"jsonrpc": "2.0", "id": req_id, "result": {{"protocolVersion": "2024-11-05", "blob": blob}}}}, chunked=False)
    else:
        blob = "B" * 120000
        write_cl({{"jsonrpc": "2.0", "id": req_id, "result": {{"content": [{{"type": "text", "text": "@e9 [button] \\"Go\\" " + blob}}]}}}}, chunked=True)
"""
            large_stub = _write_executable_script(Path(tmp), "mcp_large_cl", large_cl_stub_src)
            with McpClient(large_stub, timeout=5.0) as mcp_big:
                init_big = mcp_big.initialize()
                self.assertEqual(len(init_big["blob"]), 150000)
                call_big = mcp_big.tools_call("mimikyu_snapshot", {})
                self.assertEqual(extract_ax_ref(call_big, role="button", name_substr="Go"), "@e9")
                self.assertEqual(len(call_big["content"][0]["text"]), 120000 + len('@e9 [button] "Go" '))


class TestR3StaticCompliance(unittest.TestCase):
    def test_all_e2e_python_files_r3_compliant(self) -> None:
        e2e_dir = Path(__file__).resolve().parent
        py_files = sorted(e2e_dir.glob("*.py"))
        self.assertGreaterEqual(len(py_files), 5)
        for py_file in py_files:
            with self.subTest(file=py_file.name):
                self.assertEqual(check_r3_compliance(py_file), [])
                assert_r3_compliant(py_file.read_text(encoding="utf-8"))
        assert_r3_compliant(e2e_dir)

    def test_r3_rejects_keywords_and_temp_files_while_allowing_benign(self) -> None:
        self.assertGreater(len(R3_FORBIDDEN_KEYWORDS), 0)
        for kw in R3_FORBIDDEN_KEYWORDS:
            with self.subTest(keyword=kw):
                with self.assertRaises(AssertionError):
                    assert_r3_compliant(f"mock_{kw}_handler")
                with self.assertRaises(AssertionError):
                    assert_r3_compliant(f"MOCK_{kw.upper()}_HANDLER")

        with tempfile.TemporaryDirectory() as tmp:
            bad_file = Path(tmp) / "bad_fixture.py"
            bad_kw = R3_FORBIDDEN_KEYWORDS[1]
            bad_file.write_text(f"# test {bad_kw} probe\n", encoding="utf-8")
            violations = check_r3_compliance(Path(tmp))
            self.assertEqual(len(violations), 1)
            self.assertEqual(violations[0][2], bad_kw)

        for benign in ("mimikyu_session_new", "humanize_cursor", "proxy_bypass_list", "wait_idle"):
            assert_r3_compliant(benign)


if __name__ == "__main__":
    unittest.main()
