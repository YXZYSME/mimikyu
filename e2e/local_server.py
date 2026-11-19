"""Local loopback HTTP test servers and proxy for mimikyu E2E testing.

Uses Python 3 stdlib only and binds exclusively to 127.0.0.1 ephemeral ports.
Zero external network traffic is permitted.
"""

from __future__ import annotations

import email.parser
import email.policy
import hashlib
import html
import http.client
import http.cookies
import http.server
import json
import selectors
import socket
import struct
import threading
import time
import urllib.parse
import zlib
from typing import Any, Optional


# ---------------------------------------------------------------------------
# Deterministic Binary & Download Payloads
# ---------------------------------------------------------------------------

def _make_png_1x1() -> bytes:
    """Build a valid 68-byte 1x1 transparent RGBA PNG image."""
    def _chunk(ctype: bytes, data: bytes) -> bytes:
        crc = zlib.crc32(ctype + data) & 0xFFFFFFFF
        return struct.pack(">I", len(data)) + ctype + data + struct.pack(">I", crc)

    sig = b"\x89PNG\r\n\x1a\n"
    ihdr = _chunk(b"IHDR", struct.pack(">IIBBBBB", 1, 1, 8, 6, 0, 0, 0))
    idat = _chunk(b"IDAT", zlib.compress(b"\x00\x00\x00\x00\x00"))
    iend = _chunk(b"IEND", b"")
    return sig + ihdr + idat + iend


def _make_woff2_header() -> bytes:
    """Build a valid 48-byte WOFF2 header payload."""
    return struct.pack(
        ">IIIHHIIHHIIIII",
        0x774F4632,  # 'wOF2'
        0x00010000,  # TrueType flavor
        48,          # total length
        0,           # numTables
        0,           # reserved
        0,           # totalSfntSize
        0,           # totalCompressedSize
        1,           # majorVersion
        0,           # minorVersion
        0,           # metaOffset
        0,           # metaLength
        0,           # metaOrigLength
        0,           # privOffset
        0,           # privLength
    )


def _make_mp4_sample() -> bytes:
    """Build a valid 36-byte ISO BMFF MP4 container (ftyp + mdat)."""
    ftyp_body = b"isom" + struct.pack(">I", 512) + b"isomiso2"
    ftyp = struct.pack(">I", 8 + len(ftyp_body)) + b"ftyp" + ftyp_body
    mdat_body = b"\x00" * 8
    mdat = struct.pack(">I", 8 + len(mdat_body)) + b"mdat" + mdat_body
    return ftyp + mdat


SAMPLE_PNG_BYTES: bytes = _make_png_1x1()
SAMPLE_WOFF2_BYTES: bytes = _make_woff2_header()
SAMPLE_MP4_BYTES: bytes = _make_mp4_sample()
SAMPLE_TEXT_BYTES: bytes = b"mimikyu-e2e-sample-text-v1\nline2: deterministic-payload\n"
SAMPLE_BIN_BYTES: bytes = bytes((i * 31 + 7) % 256 for i in range(4096))

SAMPLE_SHA256_MAP: dict[str, dict[str, Any]] = {
    "sample.txt": {
        "size": len(SAMPLE_TEXT_BYTES),
        "sha256": hashlib.sha256(SAMPLE_TEXT_BYTES).hexdigest(),
    },
    "sample.bin": {
        "size": len(SAMPLE_BIN_BYTES),
        "sha256": hashlib.sha256(SAMPLE_BIN_BYTES).hexdigest(),
    },
}

CATALOG_ITEMS: list[dict[str, Any]] = [
    {"id": 1, "name": "Node Alpha", "category": "compute", "region": "us-east", "price": 12},
    {"id": 2, "name": "Node Beta", "category": "compute", "region": "us-west", "price": 24},
    {"id": 3, "name": "Block Vault", "category": "storage", "region": "eu-central", "price": 18},
    {"id": 4, "name": "Edge Router", "category": "network", "region": "us-east", "price": 30},
    {"id": 5, "name": "GPU Worker", "category": "compute", "region": "ap-east", "price": 65},
    {"id": 6, "name": "Archive Bucket", "category": "storage", "region": "us-west", "price": 9},
    {"id": 7, "name": "Mesh Gateway", "category": "network", "region": "eu-west", "price": 42},
    {"id": 8, "name": "Memory Pool", "category": "compute", "region": "us-central", "price": 28},
    {"id": 9, "name": "Snapshot Store", "category": "storage", "region": "ap-south", "price": 15},
    {"id": 10, "name": "Tunnel Relay", "category": "network", "region": "us-east", "price": 22},
    {"id": 11, "name": "Batch Runner", "category": "compute", "region": "eu-central", "price": 35},
    {"id": 12, "name": "CDN Cache", "category": "network", "region": "us-west", "price": 19},
]

_ALLOWED_LOOPBACK_HOSTS = frozenset({"127.0.0.1", "localhost", "::1"})

_HOP_BY_HOP_HEADERS = frozenset({
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
})


def _safe_json_for_html(obj: Any) -> str:
    return json.dumps(obj, separators=(",", ":")).replace("</", "<\\/")


def _is_safe_local_origin(url_str: str) -> bool:
    if not url_str:
        return False
    parsed = urllib.parse.urlsplit(url_str)
    return parsed.scheme == "http" and (parsed.hostname or "").lower() in _ALLOWED_LOOPBACK_HOSTS


# ---------------------------------------------------------------------------
# Quiet ThreadingHTTPServer
# ---------------------------------------------------------------------------

class _QuietThreadingHTTPServer(http.server.ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True
    block_on_close = False
    request_queue_size = 64

    def handle_error(self, request: Any, client_address: Any) -> None:
        return


# ---------------------------------------------------------------------------
# LocalTestServer
# ---------------------------------------------------------------------------

class _TestRequestHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: "_TestHttpServer"

    def setup(self) -> None:
        super().setup()
        try:
            self.connection.settimeout(5.0)
        except OSError:
            pass

    def log_message(self, format: str, *args: Any) -> None:
        return

    def _read_body(self) -> bytes:
        raw_len = self.headers.get("Content-Length")
        if not raw_len:
            return b""
        try:
            length = max(0, min(int(raw_len), 16 * 1024 * 1024))
        except ValueError:
            return b""
        return self.rfile.read(length) if length > 0 else b""

    def _send_response_bytes(
        self,
        status: int,
        content_type: Optional[str],
        body: bytes,
        extra_headers: Optional[list[tuple[str, str]]] = None,
        send_body: bool = True,
        include_content_length: bool = True,
    ) -> None:
        try:
            self.send_response(status)
            if content_type is not None:
                self.send_header("Content-Type", content_type)
            if include_content_length:
                self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            if extra_headers:
                for k, v in extra_headers:
                    self.send_header(k, v)
            self.end_headers()
            if send_body and body:
                self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError):
            pass

    def _send_html(
        self,
        html_text: str,
        status: int = 200,
        extra_headers: Optional[list[tuple[str, str]]] = None,
        send_body: bool = True,
    ) -> None:
        self._send_response_bytes(
            status,
            "text/html; charset=utf-8",
            html_text.encode("utf-8"),
            extra_headers=extra_headers,
            send_body=send_body,
        )

    def _send_json(
        self,
        payload: Any,
        status: int = 200,
        extra_headers: Optional[list[tuple[str, str]]] = None,
        send_body: bool = True,
    ) -> None:
        body = json.dumps(payload).encode("utf-8")
        self._send_response_bytes(
            status,
            "application/json; charset=utf-8",
            body,
            extra_headers=extra_headers,
            send_body=send_body,
        )

    def do_GET(self) -> None:
        self._handle_request("GET", send_body=True)

    def do_HEAD(self) -> None:
        self._handle_request("HEAD", send_body=False)

    def do_POST(self) -> None:
        self._handle_request("POST", send_body=True)

    def do_PUT(self) -> None:
        self._handle_request("PUT", send_body=True)

    def do_DELETE(self) -> None:
        self._handle_request("DELETE", send_body=True)

    def do_OPTIONS(self) -> None:
        self._handle_request("OPTIONS", send_body=True)

    def do_PATCH(self) -> None:
        self._handle_request("PATCH", send_body=True)

    def _handle_request(self, method: str, send_body: bool) -> None:
        owner = self.server.owner
        header_dict = {k.lower(): v for k, v in self.headers.items()}
        owner._record_request(method, self.path, header_dict)

        parsed = urllib.parse.urlsplit(self.path)
        path = parsed.path
        query = urllib.parse.parse_qs(parsed.query, keep_blank_values=True)

        if method == "OPTIONS":
            self._send_response_bytes(
                204,
                None,
                b"",
                extra_headers=[
                    ("Access-Control-Allow-Origin", "*"),
                    ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
                    ("Access-Control-Allow-Headers", "*"),
                ],
                send_body=False,
            )
            return

        if path == "/":
            self._send_html(self._render_index(), send_body=send_body)
            return

        if path == "/health":
            self._send_json(
                {"status": "ok", "server": owner.name, "port": owner.port},
                send_body=send_body,
            )
            return

        if path == "/nav/page1":
            self._send_html(
                "<!doctype html><html lang='en'><head><meta charset='utf-8'><title>Nav Page 1</title></head>"
                "<body><main><h1 id='page-title'>Page 1</h1>"
                "<p>Primary navigation fixture.</p>"
                "<a id='to-page2' href='/nav/page2'>Go to Page 2</a>"
                "</main></body></html>",
                send_body=send_body,
            )
            return

        if path == "/nav/page2":
            self._send_html(
                "<!doctype html><html lang='en'><head><meta charset='utf-8'><title>Nav Page 2</title></head>"
                "<body><main><h1 id='page-title'>Page 2</h1>"
                "<p>Secondary navigation fixture.</p>"
                "<a id='to-page1' href='/nav/page1'>Back to Page 1</a>"
                "</main></body></html>",
                send_body=send_body,
            )
            return

        if path == "/redirect":
            target = query.get("to", ["/nav/page2"])[0]
            if (
                not target.startswith("/")
                or target.startswith("//")
                or "\\" in target
                or any(ord(ch) < 0x20 or ord(ch) == 0x7F for ch in target)
            ):
                target = "/nav/page2"
            self._send_response_bytes(
                302,
                "text/plain; charset=utf-8",
                f"Redirecting to {target}\n".encode("utf-8"),
                extra_headers=[("Location", target)],
                send_body=send_body,
            )
            return

        if path == "/redirect-chain":
            raw_hops = query.get("hops", ["3"])[0]
            try:
                hops = max(0, min(int(raw_hops), 10))
            except ValueError:
                hops = 3
            if hops > 0:
                next_loc = f"/redirect-chain?hops={hops - 1}"
                self._send_response_bytes(
                    302,
                    "text/plain; charset=utf-8",
                    f"Hop remaining: {hops}\n".encode("utf-8"),
                    extra_headers=[("Location", next_loc)],
                    send_body=send_body,
                )
            else:
                self._send_html(
                    "<!doctype html><html lang='en'><head><meta charset='utf-8'><title>Redirect Chain Complete</title></head>"
                    "<body><main><h1 id='chain-end'>Redirect Chain Complete</h1>"
                    "<a href='/nav/page2'>Continue to Page 2</a></main></body></html>",
                    send_body=send_body,
                )
            return

        if path == "/history":
            self._send_html(self._render_history(), send_body=send_body)
            return

        if path == "/form":
            self._send_html(self._render_form(), send_body=send_body)
            return

        if path == "/submit":
            if method != "POST":
                self._send_json(
                    {"ok": False, "error": "method_not_allowed"},
                    status=405,
                    extra_headers=[("Allow", "POST")],
                    send_body=send_body,
                )
                return
            self._handle_submit(query)
            return

        if path == "/spa":
            self._send_html(self._render_spa(), send_body=send_body)
            return

        if path == "/api/spa-data":
            raw_delay = query.get("delay_ms", ["50"])[0]
            try:
                delay_ms = max(0, min(int(raw_delay), 5000))
            except ValueError:
                delay_ms = 50
            if delay_ms > 0:
                time.sleep(delay_ms / 1000.0)
            self._send_json(
                {
                    "ok": True,
                    "items": [
                        {"id": 1, "name": "Alpha", "category": "core", "price": 10},
                        {"id": 2, "name": "Beta", "category": "addon", "price": 25},
                        {"id": 3, "name": "Gamma", "category": "core", "price": 40},
                    ],
                },
                send_body=send_body,
            )
            return

        if path == "/api/catalog":
            self._handle_api_catalog(query, send_body=send_body)
            return

        if path == "/frames":
            cross_origin = query.get("cross_origin", [""])[0]
            if not _is_safe_local_origin(cross_origin):
                cross_origin = owner.cross_origin_url or ""
            self._send_html(self._render_frames(cross_origin), send_body=send_body)
            return

        if path == "/frame-same":
            self._send_html(self._render_frame_same(), send_body=send_body)
            return

        if path == "/frame-child":
            self._send_html(
                self._render_frame_child(),
                extra_headers=[("Access-Control-Allow-Origin", "*")],
                send_body=send_body,
            )
            return

        if path == "/tabs":
            self._send_html(self._render_tabs(), send_body=send_body)
            return

        if path == "/tab-child":
            token = query.get("token", ["tok_e2e_123"])[0]
            self._send_html(self._render_tab_child(token), send_body=send_body)
            return

        if path == "/download/sample.txt":
            self._send_response_bytes(
                200,
                "text/plain; charset=utf-8",
                SAMPLE_TEXT_BYTES,
                extra_headers=[('Content-Disposition', 'attachment; filename="sample.txt"')],
                send_body=send_body,
            )
            return

        if path == "/download/sample.bin":
            self._send_response_bytes(
                200,
                "application/octet-stream",
                SAMPLE_BIN_BYTES,
                extra_headers=[('Content-Disposition', 'attachment; filename="sample.bin"')],
                send_body=send_body,
            )
            return

        if path == "/download/sha256":
            self._send_json(SAMPLE_SHA256_MAP, send_body=send_body)
            return

        if path == "/echo-headers":
            body = self._read_body() if method == "POST" else b""
            self._send_json(
                {
                    "method": method,
                    "path": self.path,
                    "headers": header_dict,
                    "remote_addr": self.client_address[0],
                    "body_size": len(body),
                },
                send_body=send_body,
            )
            return

        if path == "/assets/img.png":
            self._send_response_bytes(
                200,
                "image/png",
                SAMPLE_PNG_BYTES,
                extra_headers=[("Cache-Control", "public, max-age=60")],
                send_body=send_body,
            )
            return

        if path == "/assets/font.woff2":
            self._send_response_bytes(
                200,
                "font/woff2",
                SAMPLE_WOFF2_BYTES,
                extra_headers=[("Access-Control-Allow-Origin", "*")],
                send_body=send_body,
            )
            return

        if path == "/assets/media.mp4":
            self._handle_media_mp4(send_body=send_body)
            return

        if path == "/api/data":
            raw_body = self._read_body() if method == "POST" else b""
            echo: Any
            if raw_body:
                try:
                    echo = json.loads(raw_body.decode("utf-8"))
                except (UnicodeDecodeError, json.JSONDecodeError):
                    echo = raw_body.decode("utf-8", errors="replace")
            else:
                echo = {k: v[0] if len(v) == 1 else v for k, v in query.items()}
            self._send_json(
                {"ok": True, "source": "local_server", "method": method, "echo": echo},
                extra_headers=[("Access-Control-Allow-Origin", "*")],
                send_body=send_body,
            )
            return

        if path == "/console-errors":
            self._send_html(self._render_console_errors(query), send_body=send_body)
            return

        if path == "/echo-env":
            self._send_html(self._render_echo_env(), send_body=send_body)
            return

        if path == "/worker-env.js":
            self._send_response_bytes(
                200,
                "application/javascript; charset=utf-8",
                self._render_worker_env_js().encode("utf-8"),
                extra_headers=[("Cache-Control", "no-store")],
                send_body=send_body,
            )
            return

        if path == "/humanize-tracker":
            self._send_html(self._render_humanize_tracker(), send_body=send_body)
            return

        if path == "/state":
            self._handle_state(query, send_body=send_body)
            return

        if path == "/workflow/wizard":
            self._send_html(self._render_workflow_wizard(), send_body=send_body)
            return

        if path == "/workflow/catalog":
            self._send_html(self._render_workflow_catalog(), send_body=send_body)
            return

        if path == "/workflow/cross-frame":
            cross_origin = query.get("cross_origin", [""])[0]
            if not _is_safe_local_origin(cross_origin):
                cross_origin = owner.cross_origin_url or ""
            self._send_html(self._render_workflow_cross_frame(cross_origin), send_body=send_body)
            return

        if path == "/workflow/multi-tab":
            token = query.get("token", ["mimikyu-tab-secret-01"])[0]
            self._send_html(self._render_workflow_multi_tab(token), send_body=send_body)
            return

        if path == "/workflow/download-verify":
            self._send_html(self._render_workflow_download_verify(), send_body=send_body)
            return

        self._send_json({"error": "not_found", "path": path}, status=404, send_body=send_body)

    # ------------------------------------------------------------------
    # Route Handlers
    # ------------------------------------------------------------------

    def _handle_submit(self, query: dict[str, list[str]]) -> None:
        raw_body = self._read_body()
        ctype = self.headers.get("Content-Type", "")
        ctype_lower = ctype.split(";")[0].strip().lower()
        fields: dict[str, Any] = {}
        files: list[dict[str, Any]] = []

        if ctype_lower == "application/json":
            try:
                parsed_json = json.loads(raw_body.decode("utf-8") or "{}")
            except (UnicodeDecodeError, json.JSONDecodeError):
                self._send_json({"ok": False, "error": "invalid_json"}, status=400)
                return
            if isinstance(parsed_json, dict):
                fields = parsed_json
            else:
                fields = {"value": parsed_json}
        elif ctype_lower == "multipart/form-data":
            raw_msg = (
                f"Content-Type: {ctype}\r\nMIME-Version: 1.0\r\n\r\n".encode("latin-1")
                + raw_body
            )
            msg = email.parser.BytesParser(policy=email.policy.default).parsebytes(raw_msg)
            if msg.is_multipart():
                for part in msg.iter_parts():
                    name = part.get_param("name", header="content-disposition") or "unknown"
                    filename = part.get_filename()
                    payload = part.get_payload(decode=True)
                    if payload is None:
                        text_payload = part.get_payload()
                        payload = text_payload.encode("utf-8") if isinstance(text_payload, str) else b""
                    if filename is not None:
                        files.append(
                            {
                                "name": name,
                                "filename": filename,
                                "content_type": part.get_content_type(),
                                "size": len(payload),
                                "sha256": hashlib.sha256(payload).hexdigest(),
                            }
                        )
                    else:
                        val = payload.decode("utf-8", errors="replace")
                        if name in fields:
                            if isinstance(fields[name], list):
                                fields[name].append(val)
                            else:
                                fields[name] = [fields[name], val]
                        else:
                            fields[name] = val
        else:
            qs = urllib.parse.parse_qs(raw_body.decode("utf-8", errors="replace"), keep_blank_values=True)
            fields = {k: v[0] if len(v) == 1 else v for k, v in qs.items()}

        result = {"ok": True, "fields": fields, "files": files}
        accept = (self.headers.get("Accept") or "").lower()
        wants_json = (
            "application/json" in accept
            or ctype_lower == "application/json"
            or query.get("format", [""])[0] == "json"
        )
        if wants_json:
            self._send_json(result)
        else:
            escaped = html.escape(json.dumps(result, indent=2))
            self._send_html(
                "<!doctype html><html lang='en'><head><meta charset='utf-8'><title>Submit Result</title></head>"
                "<body><main><h1>Submission Received</h1>"
                f"<pre id='submit-json'>{escaped}</pre>"
                "<a href='/form'>Back to Form</a></main></body></html>"
            )

    def _handle_api_catalog(self, query: dict[str, list[str]], send_body: bool) -> None:
        category = query.get("category", ["all"])[0].strip().lower()
        q = query.get("q", [""])[0].strip().lower()
        try:
            page = max(1, int(query.get("page", ["1"])[0]))
        except ValueError:
            page = 1
        per_page = 6

        filtered = [
            item
            for item in CATALOG_ITEMS
            if (category in ("", "all") or item["category"] == category)
            and (not q or q in item["name"].lower() or q in item["region"].lower())
        ]
        total = len(filtered)
        total_pages = max(1, (total + per_page - 1) // per_page)
        page = min(page, total_pages)
        start = (page - 1) * per_page
        page_items = filtered[start : start + per_page]
        self._send_json(
            {
                "ok": True,
                "items": page_items,
                "total": total,
                "page": page,
                "total_pages": total_pages,
            },
            send_body=send_body,
        )

    def _handle_media_mp4(self, send_body: bool) -> None:
        total = len(SAMPLE_MP4_BYTES)
        range_hdr = self.headers.get("Range")
        if range_hdr and range_hdr.strip().lower().startswith("bytes="):
            spec = range_hdr.split("=", 1)[1].split(",")[0].strip()
            if "-" not in spec:
                self._send_response_bytes(
                    416,
                    "text/plain; charset=utf-8",
                    b"Range Not Satisfiable\n",
                    extra_headers=[
                        ("Accept-Ranges", "bytes"),
                        ("Content-Range", f"bytes */{total}"),
                    ],
                    send_body=send_body,
                )
                return

            start_str, end_str = (part.strip() for part in spec.split("-", 1))
            valid = True
            start = 0
            end = total - 1
            if not start_str and not end_str:
                valid = False
            elif not start_str:
                if not end_str.isdigit():
                    valid = False
                else:
                    suffix_len = int(end_str)
                    if suffix_len <= 0 or total == 0:
                        valid = False
                    else:
                        start = max(0, total - suffix_len)
                        end = total - 1
            else:
                if not start_str.isdigit() or (end_str and not end_str.isdigit()):
                    valid = False
                else:
                    start = int(start_str)
                    end = int(end_str) if end_str else total - 1
                    if start >= total or start > end:
                        valid = False
                    else:
                        end = min(end, total - 1)

            if not valid:
                self._send_response_bytes(
                    416,
                    "text/plain; charset=utf-8",
                    b"Range Not Satisfiable\n",
                    extra_headers=[
                        ("Accept-Ranges", "bytes"),
                        ("Content-Range", f"bytes */{total}"),
                    ],
                    send_body=send_body,
                )
                return

            chunk = SAMPLE_MP4_BYTES[start : end + 1]
            self._send_response_bytes(
                206,
                "video/mp4",
                chunk,
                extra_headers=[
                    ("Accept-Ranges", "bytes"),
                    ("Content-Range", f"bytes {start}-{end}/{total}"),
                ],
                send_body=send_body,
            )
            return

        self._send_response_bytes(
            200,
            "video/mp4",
            SAMPLE_MP4_BYTES,
            extra_headers=[("Accept-Ranges", "bytes")],
            send_body=send_body,
        )

    def _handle_state(self, query: dict[str, list[str]], send_body: bool) -> None:
        raw_cookie = self.headers.get("Cookie", "")
        cookie_jar: http.cookies.SimpleCookie = http.cookies.SimpleCookie()
        if raw_cookie:
            try:
                cookie_jar.load(raw_cookie)
            except http.cookies.CookieError:
                pass
        server_cookies = {k: m.value for k, m in cookie_jar.items()}

        set_val = query.get("set_cookie", query.get("set", [""]))[0]
        httponly_val = query.get("httponly", [f"ho_{set_val}" if set_val else ""])[0]
        clear_flag = query.get("clear", ["0"])[0] == "1"

        extra_headers: list[tuple[str, str]] = [
            ("Cache-Control", "private, max-age=0, must-revalidate"),
        ]
        mutating = bool(set_val) or clear_flag
        if clear_flag:
            extra_headers.append(("Set-Cookie", "mimikyu_normal=; Path=/; Max-Age=0; SameSite=Lax"))
            extra_headers.append(
                ("Set-Cookie", "mimikyu_httponly=; Path=/; Max-Age=0; HttpOnly; SameSite=Lax")
            )
        elif set_val:
            safe_val = set_val.replace(";", "").replace("\r", "").replace("\n", "")
            safe_ho = httponly_val.replace(";", "").replace("\r", "").replace("\n", "")
            extra_headers.append(("Set-Cookie", f"mimikyu_normal={safe_val}; Path=/; SameSite=Lax"))
            extra_headers.append(
                ("Set-Cookie", f"mimikyu_httponly={safe_ho}; Path=/; HttpOnly; SameSite=Lax")
            )

        body_html = self._render_state(server_cookies)
        body_bytes = body_html.encode("utf-8")
        etag = f'"{hashlib.sha256(body_bytes).hexdigest()[:16]}"'
        extra_headers.append(("ETag", etag))

        if_none_match = self.headers.get("If-None-Match", "")
        if not mutating and if_none_match:
            candidates = [
                tok.strip().removeprefix("W/").strip()
                for tok in if_none_match.split(",")
                if tok.strip()
            ]
            if "*" in candidates or etag in candidates:
                self._send_response_bytes(
                    304,
                    None,
                    b"",
                    extra_headers=extra_headers,
                    send_body=False,
                    include_content_length=False,
                )
                return

        self._send_response_bytes(
            200,
            "text/html; charset=utf-8",
            body_bytes,
            extra_headers=extra_headers,
            send_body=send_body,
        )

    # ------------------------------------------------------------------
    # HTML & JS Templates
    # ------------------------------------------------------------------

    def _render_index(self) -> str:
        routes = [
            "/health",
            "/nav/page1",
            "/nav/page2",
            "/redirect",
            "/redirect-chain",
            "/history",
            "/form",
            "/spa",
            "/frames",
            "/frame-same",
            "/frame-child",
            "/tabs",
            "/tab-child",
            "/download/sample.txt",
            "/download/sample.bin",
            "/download/sha256",
            "/echo-headers",
            "/assets/img.png",
            "/assets/font.woff2",
            "/assets/media.mp4",
            "/api/data",
            "/console-errors",
            "/echo-env",
            "/worker-env.js",
            "/humanize-tracker",
            "/state",
            "/workflow/wizard",
            "/workflow/catalog",
            "/workflow/cross-frame",
            "/workflow/multi-tab",
            "/workflow/download-verify",
        ]
        items = "".join(f'<li><a href="{r}">{r}</a></li>' for r in routes)
        return (
            "<!doctype html><html lang='en'><head><meta charset='utf-8'>"
            "<title>mimikyu E2E Test Harness</title></head>"
            f"<body><main><h1>mimikyu E2E Test Harness</h1><ul>{items}</ul></main></body></html>"
        )

    def _render_history(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>History State Fixture</title></head>
<body>
<main>
  <h1>History State</h1>
  <button id="push-step1" type="button">Push Step 1</button>
  <button id="push-step2" type="button">Push Step 2</button>
  <div id="history-state" role="status">initial</div>
</main>
<script>
(function () {
  var stateEl = document.getElementById("history-state");
  function render(st) {
    stateEl.textContent = st && st.step ? ("step:" + st.step) : window.location.search || "initial";
  }
  document.getElementById("push-step1").addEventListener("click", function () {
    var st = { step: 1 };
    history.pushState(st, "", "/history?step=1");
    render(st);
  });
  document.getElementById("push-step2").addEventListener("click", function () {
    var st = { step: 2 };
    history.pushState(st, "", "/history?step=2");
    render(st);
  });
  window.addEventListener("popstate", function (ev) {
    render(ev.state);
  });
})();
</script>
</body>
</html>"""

    def _render_form(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Form Fixture</title></head>
<body>
<main>
  <h1>Test Form</h1>
  <form id="test-form" method="post" action="/submit" enctype="multipart/form-data">
    <div>
      <label for="name">Name</label>
      <input id="name" name="name" type="text">
    </div>
    <div>
      <label for="role">Role</label>
      <select id="role" name="role">
        <option value="admin">Admin</option>
        <option value="agent">Agent</option>
        <option value="viewer">Viewer</option>
      </select>
    </div>
    <div>
      <label for="agree">Agree to terms</label>
      <input id="agree" name="agree" type="checkbox" value="on">
    </div>
    <fieldset>
      <legend>Tier</legend>
      <label><input id="tier-fast" type="radio" name="tier" value="fast" checked> Fast</label>
      <label><input id="tier-accurate" type="radio" name="tier" value="accurate"> Accurate</label>
    </fieldset>
    <div>
      <label for="upload">Upload File</label>
      <input id="upload" name="upload" type="file">
    </div>
    <button id="submit-btn" type="submit">Submit</button>
  </form>
</main>
</body>
</html>"""

    def _render_spa(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Dynamic SPA Fixture</title></head>
<body>
<main>
  <h1>SPA Fixture</h1>
  <div id="spa-status" role="status">loading</div>
  <label for="spa-filter">Category Filter</label>
  <select id="spa-filter">
    <option value="all">All</option>
    <option value="core">Core</option>
    <option value="addon">Addon</option>
  </select>
  <button id="open-modal" type="button" aria-expanded="false">Open Modal</button>
  <div id="spa-modal" role="dialog" aria-modal="true" hidden>
    <p>SPA Dialog Content</p>
    <button id="close-modal" type="button">Close Modal</button>
  </div>
  <ul id="spa-list"></ul>
</main>
<script>
(function () {
  var params = new URLSearchParams(window.location.search);
  var delay = params.get("delay_ms") || "150";
  var allItems = [];
  var listEl = document.getElementById("spa-list");
  var statusEl = document.getElementById("spa-status");
  var filterEl = document.getElementById("spa-filter");
  var openBtn = document.getElementById("open-modal");
  var closeBtn = document.getElementById("close-modal");
  var modalEl = document.getElementById("spa-modal");

  function renderList() {
    var cat = filterEl.value;
    listEl.innerHTML = "";
    allItems.filter(function (it) {
      return cat === "all" || it.category === cat;
    }).forEach(function (it) {
      var li = document.createElement("li");
      li.textContent = it.name + " (" + it.category + ") - $" + it.price;
      listEl.appendChild(li);
    });
  }

  filterEl.addEventListener("change", renderList);
  openBtn.addEventListener("click", function () {
    modalEl.hidden = false;
    openBtn.setAttribute("aria-expanded", "true");
  });
  closeBtn.addEventListener("click", function () {
    modalEl.hidden = true;
    openBtn.setAttribute("aria-expanded", "false");
  });

  fetch("/api/spa-data?delay_ms=" + encodeURIComponent(delay))
    .then(function (r) { return r.json(); })
    .then(function (data) {
      allItems = data.items || [];
      renderList();
      statusEl.textContent = "loaded";
    })
    .catch(function (err) {
      statusEl.textContent = "error: " + String(err);
    });
})();
</script>
</body>
</html>"""

    def _render_frames(self, cross_origin: str) -> str:
        cross_src = f"{cross_origin}/frame-child" if cross_origin else "/frame-child"
        esc_cross = html.escape(cross_src, quote=True)
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Frames Fixture</title></head>
<body>
<main>
  <h1>Frames Host</h1>
  <div id="host-msg" role="status">waiting</div>
  <iframe id="same-frame" title="Same-Origin Frame" src="/frame-same" width="420" height="220"></iframe>
  <iframe id="cross-frame" title="Cross-Origin Frame" src="{esc_cross}" width="420" height="220"></iframe>
</main>
<script>
window.addEventListener("message", function (ev) {{
  var el = document.getElementById("host-msg");
  el.textContent = JSON.stringify(ev.data);
}});
</script>
</body>
</html>"""

    def _render_frame_same(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Same Origin Frame</title></head>
<body>
<main>
  <h2 id="same-frame-heading">Same Origin Frame</h2>
  <label for="same-frame-input">Same Frame Note</label>
  <input id="same-frame-input" type="text">
  <button id="same-frame-btn" type="button">Apply</button>
  <span id="same-frame-out" role="status"></span>
  <hr>
  <label for="frame-input">Unified Frame Message</label>
  <input id="frame-input" type="text">
  <button id="frame-send" type="button">Send To Parent</button>
  <span id="frame-ack" role="status"></span>
</main>
<script>
window.__FRAME_MARKER = {
  frame: "same",
  origin: window.location.origin,
  mimikyuPresent: typeof window.__mimikyu !== "undefined"
};
(function () {
  var sameIn = document.getElementById("same-frame-input");
  var sameOut = document.getElementById("same-frame-out");
  var frameIn = document.getElementById("frame-input");
  var frameAck = document.getElementById("frame-ack");
  function relay(val) {
    sameOut.textContent = val;
    frameAck.textContent = "ACK:" + val;
    if (window.parent && window.parent !== window) {
      window.parent.postMessage({ source: "same-frame", value: val, origin: window.location.origin }, "*");
    }
  }
  document.getElementById("same-frame-btn").addEventListener("click", function () {
    var val = sameIn.value || frameIn.value || "";
    relay(val);
  });
  document.getElementById("frame-send").addEventListener("click", function () {
    var val = frameIn.value || sameIn.value || "";
    relay(val);
  });
})();
</script>
</body>
</html>"""

    def _render_frame_child(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Cross Origin Frame</title></head>
<body>
<main>
  <h2 id="cross-frame-heading">Cross Origin Frame</h2>
  <label for="cross-frame-input">Cross Frame Note</label>
  <input id="cross-frame-input" type="text">
  <button id="cross-frame-btn" type="button">Save</button>
  <span id="cross-frame-out" role="status"></span>
  <hr>
  <label for="frame-input">Unified Frame Message</label>
  <input id="frame-input" type="text">
  <button id="frame-send" type="button">Send To Parent</button>
  <span id="frame-ack" role="status"></span>
</main>
<script>
window.__FRAME_MARKER = {
  frame: "cross",
  origin: window.location.origin,
  mimikyuPresent: typeof window.__mimikyu !== "undefined"
};
(function () {
  var crossIn = document.getElementById("cross-frame-input");
  var crossOut = document.getElementById("cross-frame-out");
  var frameIn = document.getElementById("frame-input");
  var frameAck = document.getElementById("frame-ack");
  function relay(val) {
    crossOut.textContent = val;
    frameAck.textContent = "ACK:" + val;
    if (window.parent && window.parent !== window) {
      window.parent.postMessage({ source: "cross-frame", value: val, origin: window.location.origin }, "*");
    }
  }
  document.getElementById("cross-frame-btn").addEventListener("click", function () {
    var val = crossIn.value || frameIn.value || "";
    relay(val);
  });
  document.getElementById("frame-send").addEventListener("click", function () {
    var val = frameIn.value || crossIn.value || "";
    relay(val);
  });
})();
</script>
</body>
</html>"""

    def _render_tabs(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Multi-Tab Fixture</title></head>
<body>
<main>
  <h1>Multi-Tab Host</h1>
  <button id="open-tab" type="button" onclick="window.open('/tab-child?token=tok_e2e_123', '_blank')">Open Child Tab</button>
  <a id="open-tab-link" target="_blank" rel="opener" href="/tab-child?token=tok_e2e_456">Open Tab Link</a>
  <div id="tab-msg" role="status">none</div>
</main>
<script>
window.addEventListener("message", function (ev) {
  document.getElementById("tab-msg").textContent = JSON.stringify(ev.data);
});
</script>
</body>
</html>"""

    def _render_tab_child(self, token: str) -> str:
        esc_tok = html.escape(token)
        js_tok = _safe_json_for_html(token)
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Child Tab</title></head>
<body>
<main>
  <h1>Child Tab</h1>
  <p>Issued token: <code id="child-token">{esc_tok}</code></p>
  <button id="notify-opener" type="button">Notify Opener</button>
  <div id="copy-ack" role="status">ready</div>
</main>
<script>
(function () {{
  var tok = {js_tok};
  document.getElementById("notify-opener").addEventListener("click", function () {{
    if (window.opener) {{
      window.opener.postMessage({{ type: "tab-token", token: tok }}, "*");
    }}
    document.getElementById("copy-ack").textContent = "sent:" + tok;
  }});
}})();
</script>
</body>
</html>"""

    def _render_console_errors(self, query: dict[str, list[str]]) -> str:
        uncaught = query.get("uncaught", ["0"])[0] == "1"
        assets = query.get("assets", ["0"])[0] == "1"
        extra_assets = ""
        if assets:
            extra_assets = (
                "<img id='probe-img' src='/assets/img.png' alt='probe'>"
                "<script>fetch('/api/data?from=console-errors').catch(function(){});</script>"
            )
        uncaught_js = (
            "setTimeout(function () { throw new Error('mimikyu-e2e-uncaught'); }, 10);"
            if uncaught
            else ""
        )
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Console &amp; Telemetry Fixture</title></head>
<body>
<main>
  <h1>Console &amp; Telemetry Fixture</h1>
  <div id="console-status" role="status">emitted</div>
  {extra_assets}
</main>
<script>
console.log("mimikyu-e2e-log");
console.warn("mimikyu-e2e-warn");
console.error("mimikyu-e2e-error");
{uncaught_js}
</script>
</body>
</html>"""

    def _render_echo_env(self) -> str:
        server_headers = {
            "userAgent": self.headers.get("User-Agent"),
            "acceptLanguage": self.headers.get("Accept-Language"),
            "secChUa": self.headers.get("Sec-CH-UA"),
            "secChUaMobile": self.headers.get("Sec-CH-UA-Mobile"),
            "secChUaPlatform": self.headers.get("Sec-CH-UA-Platform"),
            "secChUaFullVersionList": self.headers.get("Sec-CH-UA-Full-Version-List"),
            "allSecCh": {
                k.lower(): v
                for k, v in self.headers.items()
                if k.lower().startswith("sec-ch-ua")
            },
        }
        embedded = _safe_json_for_html(server_headers)
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Environment Consistency Probe</title></head>
<body>
<main>
  <h1>Environment Probe</h1>
  <div id="env-status" role="status">probing</div>
  <pre id="env-json" data-ready="false" aria-label="Environment Probe JSON">{{"status":"probing"}}</pre>
</main>
<script id="server-env-data" type="application/json">{embedded}</script>
<script>
(async function () {{
  var serverHeaders = {{}};
  try {{
    serverHeaders = JSON.parse(document.getElementById("server-env-data").textContent || "{{}}");
  }} catch (e) {{}}

  var uaData = null;
  if (navigator.userAgentData) {{
    uaData = {{
      brands: navigator.userAgentData.brands || null,
      mobile: navigator.userAgentData.mobile ?? null,
      platform: navigator.userAgentData.platform || null
    }};
  }}

  var intlOpts = Intl.DateTimeFormat().resolvedOptions();
  var leakedKeys = [];
  try {{
    leakedKeys = Object.getOwnPropertyNames(window).filter(function (k) {{
      return k.toLowerCase().indexOf("mimikyu") !== -1;
    }});
  }} catch (e) {{}}

  function probeWebGL() {{
    try {{
      var canvas = document.createElement("canvas");
      var gl = canvas.getContext("webgl2") || canvas.getContext("webgl") || canvas.getContext("experimental-webgl");
      if (!gl) {{
        return {{
          status: "unavailable",
          vendor: null,
          renderer: null,
          version: null,
          unmaskedVendor: null,
          unmaskedRenderer: null
        }};
      }}
      var ext = gl.getExtension("WEBGL_debug_renderer_info");
      return {{
        status: "ok",
        vendor: gl.getParameter(gl.VENDOR),
        renderer: gl.getParameter(gl.RENDERER),
        version: gl.getParameter(gl.VERSION),
        unmaskedVendor: ext ? gl.getParameter(ext.UNMASKED_VENDOR_WEBGL) : null,
        unmaskedRenderer: ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : null
      }};
    }} catch (err) {{
      return {{
        status: "unavailable",
        error: String(err),
        vendor: null,
        renderer: null,
        unmaskedVendor: null,
        unmaskedRenderer: null
      }};
    }}
  }}

  async function probeAudio() {{
    try {{
      var AudioCtx = window.AudioContext || window.webkitAudioContext;
      if (!AudioCtx) {{
        return {{ status: "unavailable", sampleRate: null }};
      }}
      var ctx = new AudioCtx();
      var info = {{
        status: "ok",
        sampleRate: ctx.sampleRate || null,
        channelCount: ctx.destination ? (ctx.destination.channelCount ?? null) : null,
        maxChannelCount: ctx.destination ? (ctx.destination.maxChannelCount ?? null) : null,
        state: ctx.state || null
      }};
      await Promise.race([
        ctx.close().catch(function () {{}}),
        new Promise(function (resolve) {{ setTimeout(resolve, 300); }})
      ]);
      return info;
    }} catch (err) {{
      return {{ status: "unavailable", error: String(err), sampleRate: null }};
    }}
  }}

  async function probeWebRTC() {{
    var PC = window.RTCPeerConnection || window.webkitRTCPeerConnection;
    if (!PC) {{
      return {{ status: "unavailable", candidates: [], candidateTypes: [], hasHostCandidate: false }};
    }}
    var pc = null;
    try {{
      pc = new PC({{ iceServers: [] }});
      pc.createDataChannel("probe");
      var candidates = [];
      var types = [];
      var done = false;
      var result = await new Promise(function (resolve) {{
        var timer = setTimeout(function () {{
          if (!done) {{
            done = true;
            resolve({{
              status: "timeout",
              candidates: candidates,
              candidateTypes: types,
              hasHostCandidate: types.indexOf("host") !== -1
            }});
          }}
        }}, 800);
        pc.onicecandidate = function (ev) {{
          if (ev.candidate && ev.candidate.candidate) {{
            var c = ev.candidate.candidate;
            candidates.push(c);
            var m = /\\btyp\\s+(\\w+)/.exec(c);
            if (m && types.indexOf(m[1]) === -1) {{
              types.push(m[1]);
            }}
          }} else if (!ev.candidate && !done) {{
            done = true;
            clearTimeout(timer);
            resolve({{
              status: "ok",
              candidates: candidates,
              candidateTypes: types,
              hasHostCandidate: types.indexOf("host") !== -1
            }});
          }}
        }};
        pc.createOffer()
          .then(function (offer) {{ return pc.setLocalDescription(offer); }})
          .catch(function (err) {{
            if (!done) {{
              done = true;
              clearTimeout(timer);
              resolve({{ status: "blocked", error: String(err), candidates: [], candidateTypes: [], hasHostCandidate: false }});
            }}
          }});
      }});
      return result;
    }} catch (err) {{
      return {{ status: "blocked", error: String(err), candidates: [], candidateTypes: [], hasHostCandidate: false }};
    }} finally {{
      if (pc) {{
        try {{ pc.close(); }} catch (e) {{}}
      }}
    }}
  }}

  async function probeWorker() {{
    if (typeof Worker === "undefined") {{
      return {{ status: "unavailable" }};
    }}
    var w = null;
    try {{
      w = new Worker("/worker-env.js");
      return await new Promise(function (resolve) {{
        var settled = false;
        var timer = setTimeout(function () {{
          if (!settled) {{
            settled = true;
            resolve({{ status: "timeout" }});
          }}
        }}, 1500);
        w.onmessage = function (ev) {{
          if (!settled) {{
            settled = true;
            clearTimeout(timer);
            resolve(ev.data || {{ status: "ok" }});
          }}
        }};
        w.onerror = function (err) {{
          if (!settled) {{
            settled = true;
            clearTimeout(timer);
            resolve({{ status: "error", error: String(err.message || err) }});
          }}
        }};
        w.postMessage("probe");
      }});
    }} catch (err) {{
      return {{ status: "error", error: String(err) }};
    }} finally {{
      if (w) {{
        try {{ w.terminate(); }} catch (e) {{}}
      }}
    }}
  }}

  var audio = await probeAudio();
  var webrtc = await probeWebRTC();
  var workerReport = await probeWorker();

  var mainNav = {{
    userAgent: navigator.userAgent,
    platform: navigator.platform,
    language: navigator.language,
    languages: Array.from(navigator.languages || []),
    hardwareConcurrency: navigator.hardwareConcurrency ?? null,
    deviceMemory: navigator.deviceMemory ?? null,
    webdriver: navigator.webdriver ?? null,
    userAgentData: uaData
  }};
  var mainIntl = {{
    timezone: intlOpts.timeZone,
    locale: intlOpts.locale,
    timezoneOffsetMinutes: new Date().getTimezoneOffset(),
    numberSample: new Intl.NumberFormat().format(1234567.89)
  }};

  var mismatches = [];
  if (workerReport && workerReport.status === "ok") {{
    if (workerReport.userAgent !== mainNav.userAgent) mismatches.push("userAgent");
    if (workerReport.platform !== mainNav.platform) mismatches.push("platform");
    if (workerReport.language !== mainNav.language) mismatches.push("language");
    if (workerReport.timezone !== mainIntl.timezone) mismatches.push("timezone");
  }}

  var report = {{
    status: "ready",
    httpHeaders: serverHeaders,
    navigator: mainNav,
    screen: {{
      width: screen.width,
      height: screen.height,
      availWidth: screen.availWidth,
      availHeight: screen.availHeight,
      colorDepth: screen.colorDepth,
      pixelDepth: screen.pixelDepth,
      devicePixelRatio: window.devicePixelRatio,
      viewport: {{
        innerWidth: window.innerWidth,
        innerHeight: window.innerHeight,
        outerWidth: window.outerWidth,
        outerHeight: window.outerHeight
      }}
    }},
    intl: mainIntl,
    isolatedWorld: {{
      mimikyuGlobalType: typeof window.__mimikyu,
      isolatedWorldClean: typeof window.__mimikyu === "undefined",
      leakedKeys: leakedKeys
    }},
    webgl: probeWebGL(),
    audio: audio,
    webrtc: webrtc,
    worker: workerReport,
    workerConsistency: {{
      consistent: workerReport && workerReport.status === "ok" && mismatches.length === 0,
      mismatches: mismatches
    }}
  }};

  window.__ENV_REPORT = report;
  var pre = document.getElementById("env-json");
  pre.textContent = JSON.stringify(report, null, 2);
  pre.setAttribute("data-ready", "true");
  document.getElementById("env-status").textContent = "ready";
}})();
</script>
</body>
</html>"""

    def _render_worker_env_js(self) -> str:
        return """self.onmessage = function () {
  try {
    var intlOpts = Intl.DateTimeFormat().resolvedOptions();
    self.postMessage({
      status: "ok",
      userAgent: navigator.userAgent,
      platform: navigator.platform,
      language: navigator.language,
      languages: Array.from(navigator.languages || []),
      hardwareConcurrency: navigator.hardwareConcurrency ?? null,
      deviceMemory: navigator.deviceMemory ?? null,
      webdriver: navigator.webdriver ?? null,
      timezone: intlOpts.timeZone,
      intlLocale: intlOpts.locale,
      mimikyuGlobalType: typeof self.__mimikyu
    });
  } catch (err) {
    self.postMessage({ status: "error", error: String(err) });
  }
};
"""

    def _render_humanize_tracker(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Humanize Input Tracker</title></head>
<body>
<main>
  <h1>Input Event Tracker</h1>
  <div>
    <button id="start-btn" type="button">Start Point</button>
    <button id="target-btn" type="button" style="margin: 120px 0 0 180px; padding: 16px 28px;">Target Button</button>
  </div>
  <div style="margin-top: 24px;">
    <label for="tracker-input">Tracker Input</label>
    <input id="tracker-input" type="text">
    <button id="reset-btn" type="button">Reset Events</button>
  </div>
  <pre id="tracker-json" aria-label="Tracker Events JSON">{"count":0,"counts":{},"events":[]}</pre>
</main>
<script>
(function () {
  window.__TRACKER_EVENTS = [];
  var counts = {};
  var pre = document.getElementById("tracker-json");

  function syncDom() {
    pre.textContent = JSON.stringify({
      count: window.__TRACKER_EVENTS.length,
      counts: counts,
      events: window.__TRACKER_EVENTS
    });
  }

  function record(ev) {
    if (window.__TRACKER_EVENTS.length >= 2000) {
      return;
    }
    var isMouse = ev.type.indexOf("mouse") === 0;
    counts[ev.type] = (counts[ev.type] || 0) + 1;
    window.__TRACKER_EVENTS.push({
      type: ev.type,
      t: Number(performance.now().toFixed(3)),
      epochMs: Date.now(),
      x: isMouse ? ev.clientX : null,
      y: isMouse ? ev.clientY : null,
      buttons: isMouse ? ev.buttons : null,
      key: isMouse ? null : (ev.key || null),
      code: isMouse ? null : (ev.code || null),
      targetId: ev.target && ev.target.id ? ev.target.id : null
    });
    syncDom();
  }

  ["mousemove", "mousedown", "mouseup", "keydown", "keyup"].forEach(function (evName) {
    document.addEventListener(evName, record, { capture: true });
  });

  document.getElementById("reset-btn").addEventListener("click", function () {
    window.__TRACKER_EVENTS = [];
    counts = {};
    syncDom();
  });
})();
</script>
</body>
</html>"""

    def _render_state(self, server_cookies: dict[str, str]) -> str:
        embedded = _safe_json_for_html(server_cookies)
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Tenant State Fixture</title></head>
<body>
<main>
  <h1>Tenant State Isolation</h1>
  <div id="state-status" role="status">loading</div>
  <div>
    <label for="state-key">Storage Key</label>
    <input id="state-key" type="text" value="token">
    <label for="state-val">Storage Value</label>
    <input id="state-val" type="text">
    <button id="save-state-btn" type="button">Save State</button>
  </div>
  <pre id="state-json" data-ready="false" aria-label="State Report JSON">{{}}</pre>
</main>
<script id="server-state-data" type="application/json">{embedded}</script>
<script>
(async function () {{
  var serverCookies = {{}};
  try {{
    serverCookies = JSON.parse(document.getElementById("server-state-data").textContent || "{{}}");
  }} catch (e) {{}}

  var params = new URLSearchParams(window.location.search);
  var lsKey = params.get("ls_key");
  var lsVal = params.get("ls_val");
  var ssKey = params.get("ss_key");
  var ssVal = params.get("ss_val");
  var idbKey = params.get("idb_key");
  var idbVal = params.get("idb_val");
  var sharedStorageVal = params.get("set_storage");

  if (sharedStorageVal !== null) {{
    lsKey = lsKey || "token";
    lsVal = lsVal || sharedStorageVal;
    ssKey = ssKey || "token";
    ssVal = ssVal || sharedStorageVal;
    idbKey = idbKey || "token";
    idbVal = idbVal || sharedStorageVal;
  }}

  if (params.get("clear") === "1") {{
    try {{ localStorage.clear(); }} catch (e) {{}}
    try {{ sessionStorage.clear(); }} catch (e) {{}}
  }} else {{
    if (lsKey && lsVal !== null) {{
      try {{ localStorage.setItem(lsKey, lsVal); }} catch (e) {{}}
    }}
    if (ssKey && ssVal !== null) {{
      try {{ sessionStorage.setItem(ssKey, ssVal); }} catch (e) {{}}
    }}
  }}

  function openIdb() {{
    return new Promise(function (resolve, reject) {{
      if (!window.indexedDB) {{
        reject(new Error("indexedDB unavailable"));
        return;
      }}
      var req = indexedDB.open("mimikyu_state_db", 1);
      req.onupgradeneeded = function () {{
        var db = req.result;
        if (!db.objectStoreNames.contains("kv")) {{
          db.createObjectStore("kv", {{ keyPath: "key" }});
        }}
      }};
      req.onsuccess = function () {{ resolve(req.result); }};
      req.onerror = function () {{ reject(req.error || new Error("idb open failed")); }};
    }});
  }}

  async function syncIdb(writeKey, writeVal, clearAll) {{
    try {{
      var db = await openIdb();
      if (clearAll) {{
        await new Promise(function (resolve) {{
          var tx = db.transaction("kv", "readwrite");
          tx.objectStore("kv").clear();
          tx.oncomplete = resolve;
          tx.onerror = resolve;
        }});
      }} else if (writeKey && writeVal !== null) {{
        await new Promise(function (resolve) {{
          var tx = db.transaction("kv", "readwrite");
          tx.objectStore("kv").put({{ key: writeKey, value: writeVal }});
          tx.oncomplete = resolve;
          tx.onerror = resolve;
        }});
      }}
      var rows = await new Promise(function (resolve) {{
        var tx = db.transaction("kv", "readonly");
        var getReq = tx.objectStore("kv").getAll();
        getReq.onsuccess = function () {{ resolve(getReq.result || []); }};
        getReq.onerror = function () {{ resolve([]); }};
      }});
      db.close();
      var mapped = {{}};
      rows.forEach(function (r) {{ mapped[r.key] = r.value; }});
      return {{ status: "ok", entries: mapped }};
    }} catch (err) {{
      return {{ status: "unavailable", error: String(err), entries: {{}} }};
    }}
  }}

  function dumpStorage(store) {{
    var out = {{}};
    try {{
      for (var i = 0; i < store.length; i++) {{
        var k = store.key(i);
        out[k] = store.getItem(k);
      }}
    }} catch (e) {{}}
    return out;
  }}

  async function refreshReport(writeKey, writeVal, clearAll) {{
    var idbData = await syncIdb(writeKey, writeVal, clearAll);
    var report = {{
      serverCookies: serverCookies,
      documentCookie: document.cookie,
      localStorage: dumpStorage(localStorage),
      sessionStorage: dumpStorage(sessionStorage),
      indexedDB: idbData
    }};
    window.__STATE_REPORT = report;
    var pre = document.getElementById("state-json");
    pre.textContent = JSON.stringify(report, null, 2);
    pre.setAttribute("data-ready", "true");
    document.getElementById("state-status").textContent = "ready";
  }}

  document.getElementById("save-state-btn").addEventListener("click", async function () {{
    var k = document.getElementById("state-key").value.trim() || "token";
    var v = document.getElementById("state-val").value;
    localStorage.setItem(k, v);
    sessionStorage.setItem(k, v);
    await refreshReport(k, v, false);
  }});

  await refreshReport(idbKey, idbVal, params.get("clear") === "1");
}})();
</script>
</body>
</html>"""

    def _render_workflow_wizard(self) -> str:
        return """<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Workflow 1: Onboarding Wizard</title></head>
<body>
<main>
  <h1>Onboarding Wizard</h1>
  <section id="step-1" aria-label="Step 1: Identity">
    <h2>Step 1: Identity</h2>
    <div id="wiz-error-1" role="alert"></div>
    <div>
      <label for="wiz-name">Full Name</label>
      <input id="wiz-name" type="text" required>
    </div>
    <div>
      <label for="wiz-email">Work Email</label>
      <input id="wiz-email" type="email" required>
    </div>
    <div>
      <label for="wiz-role">Role</label>
      <select id="wiz-role">
        <option value="engineer">Engineer</option>
        <option value="researcher">Researcher</option>
        <option value="operator">Operator</option>
      </select>
    </div>
    <button id="wiz-next-1" type="button">Next Step</button>
  </section>

  <section id="step-2" aria-label="Step 2: Preferences" hidden>
    <h2>Step 2: Preferences</h2>
    <div id="wiz-error-2" role="alert"></div>
    <div>
      <label for="wiz-agree">Accept Workspace Policy</label>
      <input id="wiz-agree" type="checkbox">
    </div>
    <div>
      <label for="wiz-notify">Enable Webhook Alerts</label>
      <input id="wiz-notify" type="checkbox">
    </div>
    <div id="wiz-webhook-row" hidden>
      <label for="wiz-webhook">Webhook URL</label>
      <input id="wiz-webhook" type="url">
    </div>
    <fieldset>
      <legend>Execution Tier</legend>
      <label><input id="tier-std" type="radio" name="wiz-tier" value="standard" checked> Standard</label>
      <label><input id="tier-pro" type="radio" name="wiz-tier" value="pro"> Pro</label>
    </fieldset>
    <button id="wiz-back-2" type="button">Back</button>
    <button id="wiz-next-2" type="button">Review</button>
  </section>

  <section id="step-3" aria-label="Step 3: Review and Submit" hidden>
    <h2>Step 3: Review</h2>
    <dl id="wiz-review"></dl>
    <button id="wiz-back-3" type="button">Back to Preferences</button>
    <button id="wiz-submit" type="button">Complete Onboarding</button>
  </section>

  <section id="wiz-complete" aria-label="Onboarding Complete" hidden>
    <h2>Onboarding Complete</h2>
    <div id="wiz-complete-status" role="status">completed</div>
    <pre id="wiz-summary-json"></pre>
  </section>
</main>
<script>
(function () {
  var s1 = document.getElementById("step-1");
  var s2 = document.getElementById("step-2");
  var s3 = document.getElementById("step-3");
  var done = document.getElementById("wiz-complete");
  var err1 = document.getElementById("wiz-error-1");
  var err2 = document.getElementById("wiz-error-2");
  var notifyEl = document.getElementById("wiz-notify");
  var webhookRow = document.getElementById("wiz-webhook-row");

  notifyEl.addEventListener("change", function () {
    webhookRow.hidden = !notifyEl.checked;
  });

  function showStep(target) {
    s1.hidden = target !== s1;
    s2.hidden = target !== s2;
    s3.hidden = target !== s3;
    done.hidden = target !== done;
  }

  document.getElementById("wiz-next-1").addEventListener("click", function () {
    var name = document.getElementById("wiz-name").value.trim();
    var email = document.getElementById("wiz-email").value.trim();
    if (!name || !email) {
      err1.textContent = "Name and email are required.";
      return;
    }
    err1.textContent = "";
    showStep(s2);
  });

  document.getElementById("wiz-back-2").addEventListener("click", function () {
    showStep(s1);
  });

  document.getElementById("wiz-next-2").addEventListener("click", function () {
    if (!document.getElementById("wiz-agree").checked) {
      err2.textContent = "You must accept the workspace policy.";
      return;
    }
    err2.textContent = "";
    var tier = document.querySelector("input[name='wiz-tier']:checked").value;
    var dl = document.getElementById("wiz-review");
    dl.innerHTML =
      "<dt>Name</dt><dd>" + document.getElementById("wiz-name").value + "</dd>" +
      "<dt>Email</dt><dd>" + document.getElementById("wiz-email").value + "</dd>" +
      "<dt>Role</dt><dd>" + document.getElementById("wiz-role").value + "</dd>" +
      "<dt>Tier</dt><dd>" + tier + "</dd>";
    showStep(s3);
  });

  document.getElementById("wiz-back-3").addEventListener("click", function () {
    showStep(s2);
  });

  document.getElementById("wiz-submit").addEventListener("click", function () {
    var payload = {
      name: document.getElementById("wiz-name").value.trim(),
      email: document.getElementById("wiz-email").value.trim(),
      role: document.getElementById("wiz-role").value,
      agree: document.getElementById("wiz-agree").checked,
      notify: notifyEl.checked,
      webhook: document.getElementById("wiz-webhook").value.trim(),
      tier: document.querySelector("input[name='wiz-tier']:checked").value
    };
    fetch("/api/spa-data?delay_ms=50")
      .then(function () {
        return fetch("/submit?format=json", {
          method: "POST",
          headers: { "Content-Type": "application/json", "Accept": "application/json" },
          body: JSON.stringify(payload)
        });
      })
      .then(function (r) { return r.json(); })
      .then(function (res) {
        document.getElementById("wiz-summary-json").textContent = JSON.stringify(res, null, 2);
        showStep(done);
      });
  });
})();
</script>
</body>
</html>"""

    def _render_workflow_catalog(self) -> str:
        return """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Workflow 2: Dynamic Catalog</title>
  <style>
    @font-face { font-family: "E2EProbe"; src: url("/assets/font.woff2") format("woff2"); }
  </style>
</head>
<body>
<main>
  <h1>Dynamic Resource Catalog</h1>
  <img id="catalog-banner" src="/assets/img.png" alt="Catalog Banner" width="16" height="16">
  <div>
    <label for="cat-search">Search Catalog</label>
    <input id="cat-search" type="search" placeholder="Filter by name or region">
    <label for="cat-filter">Category</label>
    <select id="cat-filter">
      <option value="all">All</option>
      <option value="compute">Compute</option>
      <option value="storage">Storage</option>
      <option value="network">Network</option>
    </select>
    <button id="cat-apply" type="button">Apply Filter</button>
    <button id="cat-reset" type="button">Reset Filter</button>
  </div>
  <div id="catalog-status" role="status">loading</div>
  <table id="catalog-table" aria-label="Catalog Items">
    <thead>
      <tr><th>ID</th><th>Name</th><th>Category</th><th>Region</th><th>Price</th><th>Action</th></tr>
    </thead>
    <tbody id="catalog-tbody"></tbody>
  </table>
  <div>
    <button id="page-prev" type="button">Previous Page</button>
    <span id="page-indicator">Page 1 of 1</span>
    <button id="page-next" type="button">Next Page</button>
  </div>
  <div id="selected-item" role="status">none</div>
</main>
<script>
(function () {
  var currentPage = 1;
  var totalPages = 1;
  var tbody = document.getElementById("catalog-tbody");
  var statusEl = document.getElementById("catalog-status");
  var pageInd = document.getElementById("page-indicator");
  var selectedEl = document.getElementById("selected-item");
  var searchEl = document.getElementById("cat-search");
  var filterEl = document.getElementById("cat-filter");

  function loadCatalog(page) {
    statusEl.textContent = "loading";
    var url = "/api/catalog?category=" + encodeURIComponent(filterEl.value) +
      "&q=" + encodeURIComponent(searchEl.value) + "&page=" + encodeURIComponent(page);
    fetch(url)
      .then(function (r) { return r.json(); })
      .then(function (data) {
        currentPage = data.page || 1;
        totalPages = data.total_pages || 1;
        pageInd.textContent = "Page " + currentPage + " of " + totalPages;
        tbody.innerHTML = "";
        (data.items || []).forEach(function (it) {
          var tr = document.createElement("tr");
          tr.innerHTML =
            "<td>" + it.id + "</td>" +
            "<td>" + it.name + "</td>" +
            "<td>" + it.category + "</td>" +
            "<td>" + it.region + "</td>" +
            "<td>" + it.price + "</td>";
          var tdAction = document.createElement("td");
          var btn = document.createElement("button");
          btn.type = "button";
          btn.textContent = "Select Item " + it.id;
          btn.addEventListener("click", function () {
            selectedEl.textContent = "Selected: " + it.name + " (#" + it.id + ")";
          });
          tdAction.appendChild(btn);
          tr.appendChild(tdAction);
          tbody.appendChild(tr);
        });
        statusEl.textContent = "ready";
      });
  }

  document.getElementById("cat-apply").addEventListener("click", function () { loadCatalog(1); });
  document.getElementById("cat-reset").addEventListener("click", function () {
    searchEl.value = "";
    filterEl.value = "all";
    loadCatalog(1);
  });
  document.getElementById("page-prev").addEventListener("click", function () {
    if (currentPage > 1) loadCatalog(currentPage - 1);
  });
  document.getElementById("page-next").addEventListener("click", function () {
    if (currentPage < totalPages) loadCatalog(currentPage + 1);
  });

  loadCatalog(1);
})();
</script>
</body>
</html>"""

    def _render_workflow_cross_frame(self, cross_origin: str) -> str:
        cross_src = f"{cross_origin}/frame-child" if cross_origin else "/frame-child"
        esc_cross = html.escape(cross_src, quote=True)
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Workflow 3: Cross-Frame Workspace</title></head>
<body>
<main>
  <h1>Cross-Frame Workspace</h1>
  <div>
    <label for="parent-note">Host Note</label>
    <input id="parent-note" type="text">
    <button id="parent-ping" type="button">Record Host Note</button>
  </div>
  <div id="cross-frame-status" role="status">idle</div>
  <div id="closed-shadow-host"></div>
  <iframe id="same-frame" title="Same-Origin Frame" src="/frame-same" width="440" height="240"></iframe>
  <iframe id="cross-frame" title="Cross-Origin Frame" src="{esc_cross}" width="440" height="240"></iframe>
  <pre id="frame-messages" aria-label="Frame Messages JSON">[]</pre>
</main>
<script>
(function () {{
  var msgs = [];
  var pre = document.getElementById("frame-messages");
  var statusEl = document.getElementById("cross-frame-status");
  var shadowHost = document.getElementById("closed-shadow-host");
  if (shadowHost && shadowHost.attachShadow) {{
    var closedRoot = shadowHost.attachShadow({{ mode: "closed" }});
    var span = document.createElement("span");
    span.id = "closed-shadow-secret";
    span.textContent = "closed-shadow-ready";
    closedRoot.appendChild(span);
  }}
  function pushMsg(entry) {{
    msgs.push(entry);
    pre.textContent = JSON.stringify(msgs, null, 2);
    statusEl.textContent = "messages:" + msgs.length;
  }}
  document.getElementById("parent-ping").addEventListener("click", function () {{
    pushMsg({{ source: "host", value: document.getElementById("parent-note").value }});
  }});
  window.addEventListener("message", function (ev) {{
    pushMsg(ev.data);
  }});
}})();
</script>
</body>
</html>"""

    def _render_workflow_multi_tab(self, token: str) -> str:
        esc_tok = html.escape(token)
        esc_href = html.escape(f"/tab-child?token={urllib.parse.quote(token)}", quote=True)
        js_tok = _safe_json_for_html(token)
        return f"""<!doctype html>
<html lang="en">
<head><meta charset="utf-8"><title>Workflow 4: Multi-Tab Token Exchange</title></head>
<body>
<main>
  <h1>Multi-Tab Token Exchange</h1>
  <p>Expected workflow token: <code id="expected-token-hint">{esc_tok}</code></p>
  <div>
    <a id="open-tab-link" target="_blank" rel="opener" href="{esc_href}">Open Child Tab Link</a>
    <button id="open-tab-btn" type="button" onclick="window.open('{esc_href}', '_blank')">Open Child Tab Window</button>
  </div>
  <div>
    <label for="redeem-token">Redeem Token</label>
    <input id="redeem-token" type="text">
    <button id="redeem-btn" type="button">Redeem Token</button>
  </div>
  <div id="redeem-result" role="status">pending</div>
</main>
<script>
(function () {{
  var expected = {js_tok};
  var resultEl = document.getElementById("redeem-result");
  document.getElementById("redeem-btn").addEventListener("click", function () {{
    var val = document.getElementById("redeem-token").value.trim();
    if (val && val === expected) {{
      try {{
        localStorage.setItem("redeemed_token", val);
        sessionStorage.setItem("redeemed_token", val);
      }} catch (e) {{}}
      resultEl.textContent = "VERIFIED: " + val;
    }} else {{
      resultEl.textContent = "INVALID";
    }}
  }});
}})();
</script>
</body>
</html>"""

    def _render_workflow_download_verify(self) -> str:
        return """<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <title>Workflow 5: Download &amp; Artifact Verification</title>
  <style>
    @font-face { font-family: "VerifyFont"; src: url("/assets/font.woff2") format("woff2"); }
    #artifact-card { border: 2px solid #111; padding: 16px; background: #f8fafc; color: #0f172a; }
  </style>
</head>
<body>
<main>
  <h1>Download &amp; Artifact Verification</h1>
  <div id="artifact-card">
    <h2>Deterministic Artifact Card</h2>
    <p>Used for screenshot and PDF capture verification.</p>
    <img id="verify-img" src="/assets/img.png" alt="Sample Asset" width="16" height="16">
    <video id="verify-media" src="/assets/media.mp4" width="64" height="36" preload="metadata"></video>
  </div>
  <pre id="asset-status" aria-label="Asset Load Status">{}</pre>
  <ul>
    <li><a id="dl-txt" href="/download/sample.txt" download="sample.txt">Download sample.txt</a></li>
    <li><a id="dl-bin" href="/download/sample.bin" download="sample.bin">Download sample.bin</a></li>
    <li><a id="dl-sha" href="/download/sha256">View SHA-256 Manifest</a></li>
  </ul>
  <div>
    <label for="sha-input">Expected SHA-256 for sample.bin</label>
    <input id="sha-input" type="text">
    <button id="verify-sha-btn" type="button">Verify Checksum</button>
  </div>
  <div id="verify-result" role="status">unverified</div>
</main>
<script>
(function () {
  var assetState = { img: "pending", media: "pending" };
  var statusPre = document.getElementById("asset-status");
  function syncAssets() {
    statusPre.textContent = JSON.stringify(assetState);
  }
  var img = document.getElementById("verify-img");
  img.addEventListener("load", function () { assetState.img = "loaded"; syncAssets(); });
  img.addEventListener("error", function () { assetState.img = "blocked_or_error"; syncAssets(); });
  if (img.complete) {
    assetState.img = img.naturalWidth > 0 ? "loaded" : "blocked_or_error";
  }
  var media = document.getElementById("verify-media");
  media.addEventListener("loadedmetadata", function () { assetState.media = "loaded"; syncAssets(); });
  media.addEventListener("error", function () { assetState.media = "blocked_or_error"; syncAssets(); });
  syncAssets();

  document.getElementById("verify-sha-btn").addEventListener("click", function () {
    var candidate = document.getElementById("sha-input").value.trim().toLowerCase();
    var out = document.getElementById("verify-result");
    fetch("/download/sha256")
      .then(function (r) { return r.json(); })
      .then(function (manifest) {
        var expected = manifest && manifest["sample.bin"] && manifest["sample.bin"].sha256;
        out.textContent = (candidate && candidate === expected) ? "VERIFIED" : "MISMATCH";
      })
      .catch(function () {
        out.textContent = "ERROR";
      });
  });
})();
</script>
</body>
</html>"""


class _TestHttpServer(_QuietThreadingHTTPServer):
    def __init__(
        self,
        server_address: tuple[str, int],
        handler_cls: type[http.server.BaseHTTPRequestHandler],
        owner: "LocalTestServer",
    ) -> None:
        self.owner = owner
        super().__init__(server_address, handler_cls)


class LocalTestServer:
    """Ephemeral 127.0.0.1 HTTP test server serving all E2E fixtures."""

    def __init__(self, host: str = "127.0.0.1", port: int = 0, name: str = "primary") -> None:
        self.host = host
        self._requested_port = port
        self.name = name
        self.cross_origin_url: Optional[str] = None
        self._server: Optional[_TestHttpServer] = None
        self._thread: Optional[threading.Thread] = None
        self._port: int = 0
        self._lock = threading.Lock()
        self._requests: list[dict[str, Any]] = []

    def start(self) -> "LocalTestServer":
        if self._server is not None:
            return self
        srv = _TestHttpServer((self.host, self._requested_port), _TestRequestHandler, self)
        self._server = srv
        self._port = int(srv.server_address[1])
        t = threading.Thread(
            target=srv.serve_forever,
            kwargs={"poll_interval": 0.1},
            name=f"mimikyu-e2e-{self.name}-{self._port}",
            daemon=True,
        )
        self._thread = t
        t.start()
        return self

    def stop(self) -> None:
        srv = self._server
        thread = self._thread
        self._server = None
        self._thread = None
        if srv is not None:
            try:
                srv.shutdown()
            except Exception:
                pass
            try:
                srv.server_close()
            except Exception:
                pass
        if thread is not None:
            thread.join(timeout=3.0)

    def __enter__(self) -> "LocalTestServer":
        return self.start()

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.stop()

    @property
    def port(self) -> int:
        return self._port

    @property
    def base_url(self) -> str:
        return f"http://{self.host}:{self._port}"

    def _record_request(self, method: str, path: str, headers: dict[str, str]) -> None:
        with self._lock:
            self._requests.append(
                {
                    "method": method,
                    "path": path,
                    "headers": headers,
                    "timestamp": time.time(),
                }
            )

    @property
    def request_log(self) -> list[dict[str, Any]]:
        with self._lock:
            return list(self._requests)

    def clear_requests(self) -> None:
        with self._lock:
            self._requests.clear()


# ---------------------------------------------------------------------------
# LocalHttpProxy
# ---------------------------------------------------------------------------

class _ProxyRequestHandler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    rbufsize = 0  # Unbuffered so pipelined bytes after CONNECT are never lost
    server: "_ProxyHttpServer"

    def setup(self) -> None:
        super().setup()
        try:
            self.connection.settimeout(self.server.owner.connect_timeout)
        except OSError:
            pass

    def log_message(self, format: str, *args: Any) -> None:
        return

    def _send_simple(self, status: int, message: str) -> None:
        body = message.encode("utf-8")
        try:
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.end_headers()
            if body:
                self.wfile.write(body)
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError, TimeoutError, OSError):
            pass

    def _is_host_allowed(self, host: Optional[str]) -> bool:
        if self.server.owner.allow_external:
            return bool(host)
        if not host:
            return False
        return host.strip("[]").lower() in _ALLOWED_LOOPBACK_HOSTS

    def do_GET(self) -> None:
        self._relay_http("GET")

    def do_HEAD(self) -> None:
        self._relay_http("HEAD")

    def do_POST(self) -> None:
        self._relay_http("POST")

    def do_PUT(self) -> None:
        self._relay_http("PUT")

    def do_DELETE(self) -> None:
        self._relay_http("DELETE")

    def do_OPTIONS(self) -> None:
        self._relay_http("OPTIONS")

    def do_PATCH(self) -> None:
        self._relay_http("PATCH")

    def _relay_http(self, method: str) -> None:
        owner = self.server.owner
        owner._record(self.path)

        parsed = urllib.parse.urlsplit(self.path)
        if parsed.scheme not in ("http", "") or not parsed.hostname:
            self._send_simple(400, "Bad Request: absolute HTTP URL required\n")
            return
        if not self._is_host_allowed(parsed.hostname):
            self._send_simple(403, "Forbidden: external network blocked by E2E proxy\n")
            return

        target_host = parsed.hostname
        target_port = parsed.port or 80
        req_path = parsed.path or "/"
        if parsed.query:
            req_path = f"{req_path}?{parsed.query}"

        raw_len = self.headers.get("Content-Length")
        req_body: Optional[bytes] = None
        if raw_len:
            try:
                length = max(0, min(int(raw_len), 16 * 1024 * 1024))
            except ValueError:
                length = 0
            req_body = self.rfile.read(length) if length > 0 else b""

        fwd_headers: dict[str, str] = {}
        for k, v in self.headers.items():
            if k.lower() in _HOP_BY_HOP_HEADERS:
                continue
            fwd_headers[k] = v
        fwd_headers["Connection"] = "close"

        conn: Optional[http.client.HTTPConnection] = None
        try:
            conn = http.client.HTTPConnection(
                target_host, target_port, timeout=owner.connect_timeout
            )
            conn.request(method, req_path, body=req_body, headers=fwd_headers)
            resp = conn.getresponse()
            resp_body = b"" if method == "HEAD" else resp.read()

            self.send_response(resp.status, resp.reason)
            for k, v in resp.getheaders():
                kl = k.lower()
                if kl in _HOP_BY_HOP_HEADERS or kl == "content-length":
                    continue
                self.send_header(k, v)
            self.send_header("Via", "1.1 mimikyu-e2e-proxy")
            if resp.status not in (204, 304):
                self.send_header("Content-Length", str(len(resp_body)))
            self.send_header("Connection", "close")
            self.end_headers()
            if method != "HEAD" and resp_body:
                self.wfile.write(resp_body)
            self.wfile.flush()
        except (OSError, http.client.HTTPException) as exc:
            self._send_simple(502, f"Bad Gateway: {exc}\n")
        finally:
            if conn is not None:
                try:
                    conn.close()
                except Exception:
                    pass

    def do_CONNECT(self) -> None:
        owner = self.server.owner
        owner._record(f"CONNECT {self.path}")

        host, sep, port_str = self.path.rpartition(":")
        if not sep:
            self._send_simple(400, "Bad Request: host:port required\n")
            return
        host = host.strip("[]")
        try:
            port = int(port_str)
        except ValueError:
            self._send_simple(400, "Bad Request: invalid port\n")
            return
        if not self._is_host_allowed(host):
            self._send_simple(403, "Forbidden: external CONNECT blocked by E2E proxy\n")
            return

        upstream: Optional[socket.socket] = None
        client_sock = self.connection
        selector: Optional[selectors.BaseSelector] = None
        established = False
        try:
            upstream = socket.create_connection((host, port), timeout=owner.connect_timeout)

            owner._register_socket(client_sock)
            owner._register_socket(upstream)

            self.close_connection = True
            client_sock.settimeout(owner.connect_timeout)
            client_sock.sendall(
                b"HTTP/1.1 200 Connection Established\r\n"
                b"Proxy-Agent: mimikyu-e2e-proxy\r\n\r\n"
            )
            established = True
            upstream.setblocking(False)
            client_sock.setblocking(False)

            selector = selectors.DefaultSelector()
            selector.register(client_sock, selectors.EVENT_READ, data=upstream)
            selector.register(upstream, selectors.EVENT_READ, data=client_sock)
            open_reads: set[socket.socket] = {client_sock, upstream}

            last_activity = time.monotonic()
            while open_reads and not owner._stop_event.is_set():
                events = selector.select(timeout=0.25)
                if not events:
                    if time.monotonic() - last_activity > owner.tunnel_idle_timeout:
                        break
                    continue
                for key, _ in events:
                    src = key.fileobj
                    dst = key.data
                    try:
                        chunk = src.recv(65536)
                    except BlockingIOError:
                        continue
                    except OSError:
                        try:
                            selector.unregister(src)
                        except Exception:
                            pass
                        open_reads.discard(src)
                        try:
                            dst.shutdown(socket.SHUT_WR)
                        except OSError:
                            pass
                        continue
                    if not chunk:
                        try:
                            selector.unregister(src)
                        except Exception:
                            pass
                        open_reads.discard(src)
                        try:
                            dst.shutdown(socket.SHUT_WR)
                        except OSError:
                            pass
                        continue
                    last_activity = time.monotonic()
                    try:
                        dst.settimeout(owner.connect_timeout)
                        dst.sendall(chunk)
                        dst.setblocking(False)
                    except OSError:
                        return
        except OSError as exc:
            if not established:
                self._send_simple(502, f"Bad Gateway: {exc}\n")
        finally:
            if selector is not None:
                try:
                    selector.close()
                except Exception:
                    pass
            owner._unregister_socket(client_sock)
            if upstream is not None:
                owner._unregister_socket(upstream)
                try:
                    upstream.close()
                except OSError:
                    pass


class _ProxyHttpServer(_QuietThreadingHTTPServer):
    def __init__(
        self,
        server_address: tuple[str, int],
        handler_cls: type[http.server.BaseHTTPRequestHandler],
        owner: "LocalHttpProxy",
    ) -> None:
        self.owner = owner
        super().__init__(server_address, handler_cls)


class LocalHttpProxy:
    """Loopback-only HTTP forward + CONNECT tunnel proxy with request telemetry."""

    def __init__(
        self,
        host: str = "127.0.0.1",
        port: int = 0,
        allow_external: bool = False,
        connect_timeout: float = 3.0,
        tunnel_idle_timeout: float = 5.0,
    ) -> None:
        self.host = host
        self._requested_port = port
        self.allow_external = allow_external
        self.connect_timeout = connect_timeout
        self.tunnel_idle_timeout = tunnel_idle_timeout
        self._server: Optional[_ProxyHttpServer] = None
        self._thread: Optional[threading.Thread] = None
        self._port: int = 0
        self._lock = threading.Lock()
        self._request_count: int = 0
        self._seen_urls: list[str] = []
        self._active_sockets: set[socket.socket] = set()
        self._stop_event = threading.Event()

    def start(self) -> "LocalHttpProxy":
        if self._server is not None:
            return self
        self._stop_event.clear()
        srv = _ProxyHttpServer((self.host, self._requested_port), _ProxyRequestHandler, self)
        self._server = srv
        self._port = int(srv.server_address[1])
        t = threading.Thread(
            target=srv.serve_forever,
            kwargs={"poll_interval": 0.1},
            name=f"mimikyu-e2e-proxy-{self._port}",
            daemon=True,
        )
        self._thread = t
        t.start()
        return self

    def stop(self) -> None:
        self._stop_event.set()
        with self._lock:
            socks = list(self._active_sockets)
            self._active_sockets.clear()
        for sock in socks:
            try:
                sock.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            try:
                sock.close()
            except OSError:
                pass

        srv = self._server
        thread = self._thread
        self._server = None
        self._thread = None
        if srv is not None:
            try:
                srv.shutdown()
            except Exception:
                pass
            try:
                srv.server_close()
            except Exception:
                pass
        if thread is not None:
            thread.join(timeout=3.0)

    def __enter__(self) -> "LocalHttpProxy":
        return self.start()

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.stop()

    def _record(self, url_or_target: str) -> None:
        with self._lock:
            self._request_count += 1
            self._seen_urls.append(url_or_target)

    def _register_socket(self, sock: socket.socket) -> None:
        with self._lock:
            self._active_sockets.add(sock)

    def _unregister_socket(self, sock: socket.socket) -> None:
        with self._lock:
            self._active_sockets.discard(sock)

    @property
    def port(self) -> int:
        return self._port

    @property
    def proxy_url(self) -> str:
        return f"http://{self.host}:{self._port}"

    @property
    def request_count(self) -> int:
        with self._lock:
            return self._request_count

    @property
    def seen_urls(self) -> list[str]:
        with self._lock:
            return list(self._seen_urls)

    def reset_stats(self) -> None:
        with self._lock:
            self._request_count = 0
            self._seen_urls.clear()


# ---------------------------------------------------------------------------
# LocalTestCluster
# ---------------------------------------------------------------------------

class LocalTestCluster:
    """Manages primary origin, secondary cross-origin, and loopback HTTP proxy."""

    def __init__(self, host: str = "127.0.0.1") -> None:
        self.host = host
        self.primary = LocalTestServer(host=host, port=0, name="primary")
        self.cross = LocalTestServer(host=host, port=0, name="cross")
        self.proxy = LocalHttpProxy(host=host, port=0, allow_external=False)

    def start(self) -> "LocalTestCluster":
        try:
            self.primary.start()
            self.cross.start()
            self.primary.cross_origin_url = self.cross.base_url
            self.cross.cross_origin_url = self.primary.base_url
            self.proxy.start()
        except Exception:
            self.stop()
            raise
        return self

    def stop(self) -> None:
        for component in (self.proxy, self.cross, self.primary):
            try:
                component.stop()
            except Exception:
                pass

    def __enter__(self) -> "LocalTestCluster":
        return self.start()

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.stop()

    @property
    def base_url(self) -> str:
        return self.primary.base_url

    @property
    def cross_url(self) -> str:
        return self.cross.base_url

    @property
    def proxy_url(self) -> str:
        return self.proxy.proxy_url


__all__ = [
    "CATALOG_ITEMS",
    "LocalHttpProxy",
    "LocalTestCluster",
    "LocalTestServer",
    "SAMPLE_BIN_BYTES",
    "SAMPLE_MP4_BYTES",
    "SAMPLE_PNG_BYTES",
    "SAMPLE_SHA256_MAP",
    "SAMPLE_TEXT_BYTES",
    "SAMPLE_WOFF2_BYTES",
]
