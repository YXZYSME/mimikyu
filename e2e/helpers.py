"""Shared E2E helpers: config, CLI/MCP clients, AX ref parser, and R3 checks.

Python 3 stdlib only. Strictly opaque-box against mimikyu's public CLI
(--json), Unix-socket daemon auto-start, and stdio MCP JSON-RPC 2.0 interfaces.
"""

from __future__ import annotations

import collections
import dataclasses
import json
import os
import queue
import re
import shutil
import signal
import subprocess
import tempfile
import threading
import time
import weakref
from pathlib import Path
from typing import Any, Iterable, Optional, Union

from e2e.local_server import LocalTestCluster


# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

@dataclasses.dataclass(frozen=True)
class E2EConfig:
    bin_path: str
    base_url: str
    cross_url: str
    proxy_url: str
    engine: str


def get_config() -> E2EConfig:
    """Read current E2E environment configuration dynamically."""
    engine = os.environ.get("MIMIKYU_E2E_ENGINE", "chromium").strip() or "chromium"
    return E2EConfig(
        bin_path=os.environ.get("MIMIKYU_BIN", "").strip(),
        base_url=os.environ.get("MIMIKYU_E2E_BASE_URL", "").strip(),
        cross_url=os.environ.get("MIMIKYU_E2E_CROSS_URL", "").strip(),
        proxy_url=os.environ.get("MIMIKYU_E2E_PROXY_URL", "").strip(),
        engine=engine,
    )


def _make_short_temp_dir(prefix: str = "mk_") -> tempfile.TemporaryDirectory[str]:
    """Create a short temp directory respecting macOS 104-byte sun_path limit."""
    base_tmp = tempfile.gettempdir()
    if len(base_tmp) > 70 and Path("/tmp").is_dir() and os.access("/tmp", os.W_OK):
        return tempfile.TemporaryDirectory(prefix=prefix, dir="/tmp")
    return tempfile.TemporaryDirectory(prefix=prefix)


def _kill_process_group(proc: subprocess.Popen[Any], pgid: Optional[int] = None) -> None:
    """Terminate an entire child process group safely, including surviving grandchildren."""
    pid = proc.pid
    if pgid is None and pid is not None:
        try:
            pgid = os.getpgid(pid)
        except OSError:
            pgid = pid

    if pgid is not None:
        try:
            os.killpg(pgid, signal.SIGTERM)
        except (ProcessLookupError, PermissionError, OSError):
            try:
                proc.terminate()
            except OSError:
                pass
    elif pid is not None:
        try:
            proc.terminate()
        except OSError:
            pass

    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        pass

    # Always follow up with SIGKILL on the recorded process group so that
    # grandchildren ignoring SIGTERM or outliving leader exit are killed.
    if pgid is not None:
        try:
            os.killpg(pgid, signal.SIGKILL)
        except (ProcessLookupError, PermissionError, OSError):
            try:
                proc.kill()
            except OSError:
                pass
    elif pid is not None:
        try:
            proc.kill()
        except OSError:
            pass

    try:
        proc.wait(timeout=2.0)
    except subprocess.TimeoutExpired:
        pass


def _read_exact(stream: Any, n: int) -> bytes:
    """Read exactly n bytes from stream across short reads until n bytes or EOF."""
    if n <= 0:
        return b""
    chunks: list[bytes] = []
    remaining = n
    while remaining > 0:
        chunk = stream.read(remaining)
        if not chunk:
            break
        chunks.append(chunk)
        remaining -= len(chunk)
    return b"".join(chunks)


def _cleanup_temp_dir(temp_dir: Optional[tempfile.TemporaryDirectory[str]]) -> None:
    if temp_dir is None:
        return
    name = temp_dir.name
    try:
        temp_dir.cleanup()
    except OSError:
        shutil.rmtree(name, ignore_errors=True)


# ---------------------------------------------------------------------------
# Command Result & CLI Client
# ---------------------------------------------------------------------------

@dataclasses.dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: str
    stderr: str
    payload: Any
    duration_ms: float

    @property
    def ok(self) -> bool:
        if self.returncode != 0:
            return False
        if isinstance(self.payload, dict):
            return self.payload.get("ok", True) is True
        return True

    @property
    def data(self) -> Any:
        if isinstance(self.payload, dict):
            return self.payload.get("data")
        return None

    @property
    def error(self) -> Optional[str]:
        if isinstance(self.payload, dict) and self.payload.get("error") is not None:
            return str(self.payload.get("error"))
        stripped = self.stderr.strip()
        return stripped or None


class CommandError(RuntimeError):
    """Raised when a MimikyuClient command fails with check=True."""

    def __init__(self, result: CommandResult, message: Optional[str] = None) -> None:
        self.result = result
        detail = message or result.error or f"Command exited with code {result.returncode}"
        super().__init__(detail)


class MimikyuClient:
    """Subprocess wrapper for all mimikyu CLI commands with isolated --state-dir."""

    def __init__(
        self,
        bin_path: Union[str, Path, None] = None,
        state_dir: Union[str, Path, None] = None,
        engine: Optional[str] = None,
        timeout: float = 30.0,
        session: Optional[str] = None,
    ) -> None:
        cfg = get_config()
        resolved_bin = str(bin_path) if bin_path is not None else cfg.bin_path
        self.bin_path: str = resolved_bin.strip()
        self.engine: str = (engine if engine is not None else cfg.engine) or "chromium"
        self.timeout: float = float(timeout)
        self.session: Optional[str] = session
        self._has_executed: bool = False
        self._closed: bool = False

        if state_dir is None:
            self._temp_dir: Optional[tempfile.TemporaryDirectory[str]] = _make_short_temp_dir("mk_")
            self.state_dir: Path = Path(self._temp_dir.name)
            self._finalizer: Optional[weakref.finalize] = weakref.finalize(
                self, _cleanup_temp_dir, self._temp_dir
            )
        else:
            self._temp_dir = None
            self.state_dir = Path(state_dir)
            self.state_dir.mkdir(parents=True, exist_ok=True)
            self._finalizer = None

    def __enter__(self) -> "MimikyuClient":
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.close()

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            sock_path = self.state_dir / "daemon.sock"
            pid_path = self.state_dir / "daemon.pid"
            if (
                self._has_executed
                and self.bin_path
                and Path(self.bin_path).is_file()
                and (os.path.lexists(sock_path) or os.path.lexists(pid_path))
            ):
                try:
                    self.run(
                        "daemon",
                        "stop",
                        json_output=True,
                        check=False,
                        include_engine=False,
                        include_session=False,
                        timeout=10.0,
                    )
                except Exception:
                    pass
        finally:
            if self._finalizer is not None:
                self._finalizer()
            elif self._temp_dir is not None:
                _cleanup_temp_dir(self._temp_dir)

    def run(
        self,
        *args: str,
        json_output: bool = True,
        check: bool = False,
        session: Optional[str] = None,
        engine: Optional[str] = None,
        include_engine: bool = True,
        include_state_dir: bool = True,
        include_session: bool = True,
        timeout: Optional[float] = None,
        env_overrides: Optional[dict[str, str]] = None,
    ) -> CommandResult:
        if not self.bin_path:
            raise FileNotFoundError("mimikyu binary path is empty")

        cmd: list[str] = [self.bin_path]
        if include_state_dir:
            cmd.extend(["--state-dir", str(self.state_dir)])
        eff_engine = engine if engine is not None else self.engine
        if include_engine and eff_engine:
            cmd.extend(["--engine", eff_engine])
        eff_session = session if session is not None else self.session
        if include_session and eff_session:
            cmd.extend(["--session", eff_session])

        arg_list = list(args)
        if json_output and "--json" not in arg_list:
            if "--" in arg_list:
                arg_list.insert(arg_list.index("--"), "--json")
            else:
                arg_list.append("--json")
        cmd.extend(arg_list)

        eff_timeout = self.timeout if timeout is None else float(timeout)
        env = os.environ.copy()
        if env_overrides:
            env.update(env_overrides)

        self._has_executed = True
        t0 = time.monotonic()
        proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
            env=env,
        )
        pgid = proc.pid
        try:
            stdout, stderr = proc.communicate(timeout=eff_timeout)
            returncode = proc.returncode
        except subprocess.TimeoutExpired:
            _kill_process_group(proc, pgid=pgid)
            try:
                out_rem, err_rem = proc.communicate(timeout=2.0)
            except (subprocess.TimeoutExpired, OSError):
                out_rem, err_rem = "", ""
            elapsed_ms = (time.monotonic() - t0) * 1000.0
            res = CommandResult(
                returncode=-124,
                stdout=out_rem or "",
                stderr=f"Timed out after {eff_timeout:.2f}s\n{err_rem or ''}".strip(),
                payload=None,
                duration_ms=elapsed_ms,
            )
            if check:
                raise CommandError(res)
            return res

        elapsed_ms = (time.monotonic() - t0) * 1000.0
        payload: Any = None
        if json_output and stdout.strip():
            try:
                payload = json.loads(stdout.strip())
            except json.JSONDecodeError:
                payload = None

        res = CommandResult(
            returncode=returncode,
            stdout=stdout,
            stderr=stderr,
            payload=payload,
            duration_ms=elapsed_ms,
        )
        if check and not res.ok:
            raise CommandError(res)
        return res

    # ------------------------------------------------------------------
    # PROJECT.md § 2 CLI Convenience Methods (27 commands)
    # ------------------------------------------------------------------

    def doctor(
        self,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "doctor",
            json_output=json_output,
            check=check,
            include_engine=False,
            include_session=False,
            timeout=timeout,
        )

    def daemon(
        self,
        foreground: bool = False,
        idle_timeout: Optional[int] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["daemon"]
        if foreground:
            args.append("--foreground")
        if idle_timeout is not None:
            args.extend(["--idle-timeout", str(idle_timeout)])
        return self.run(
            *args,
            json_output=json_output,
            check=check,
            include_engine=False,
            include_session=False,
            timeout=timeout,
        )

    def session_new(
        self,
        *,
        profile: Optional[str] = None,
        ephemeral: bool = False,
        virtual: bool = False,
        engine: Optional[str] = None,
        preset: Optional[str] = None,
        locale: Optional[str] = None,
        timezone: Optional[str] = None,
        proxy: Optional[str] = None,
        geoip: Optional[str] = None,
        viewport: Optional[str] = None,
        block_images: bool = False,
        block_media: bool = False,
        block_fonts: bool = False,
        instant_animations: bool = False,
        humanize: bool = False,
        session: Optional[str] = None,
        extra_args: Optional[list[str]] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args: list[str] = ["session", "new"]
        if profile is not None:
            args.extend(["--profile", profile])
        if ephemeral:
            args.append("--ephemeral")
        if virtual:
            args.append("--virtual")
        if preset is not None:
            args.extend(["--preset", preset])
        if locale is not None:
            args.extend(["--locale", locale])
        if timezone is not None:
            args.extend(["--timezone", timezone])
        if proxy is not None:
            args.extend(["--proxy", proxy])
        if geoip is not None:
            args.extend(["--geoip", geoip])
        if viewport is not None:
            args.extend(["--viewport", viewport])
        if block_images:
            args.append("--block-images")
        if block_media:
            args.append("--block-media")
        if block_fonts:
            args.append("--block-fonts")
        if instant_animations:
            args.append("--instant-animations")
        if humanize:
            args.append("--humanize")
        if extra_args:
            args.extend(extra_args)
        return self.run(
            *args,
            json_output=json_output,
            check=check,
            session=session,
            engine=engine,
            timeout=timeout,
        )

    def session_list(
        self,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "session",
            "list",
            json_output=json_output,
            check=check,
            include_session=False,
            timeout=timeout,
        )

    def session_close(
        self,
        session_id: Optional[str] = None,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["session", "close"]
        if session_id is not None:
            args.append(session_id)
        return self.run(
            *args,
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def session_export(
        self,
        file_path: Union[str, Path],
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "session",
            "export",
            str(file_path),
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def session_import(
        self,
        file_path: Union[str, Path],
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "session",
            "import",
            str(file_path),
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def open(
        self,
        url: str,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run("open", url, json_output=json_output, check=check, session=session, timeout=timeout)

    def back(
        self,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run("back", json_output=json_output, check=check, session=session, timeout=timeout)

    def forward(
        self,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run("forward", json_output=json_output, check=check, session=session, timeout=timeout)

    def reload(
        self,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run("reload", json_output=json_output, check=check, session=session, timeout=timeout)

    def snapshot(
        self,
        *,
        diff: bool = False,
        max_depth: Optional[int] = None,
        interactive_only: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args: list[str] = ["snapshot"]
        if diff:
            args.append("--diff")
        if max_depth is not None:
            args.extend(["--max-depth", str(max_depth)])
        if interactive_only:
            args.append("--interactive-only")
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def click(
        self,
        target: str,
        *,
        humanize: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["click", target]
        if humanize:
            args.append("--humanize")
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def fill(
        self,
        target: str,
        value: str,
        *,
        humanize: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["fill", target, value]
        if humanize:
            args.append("--humanize")
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def type(
        self,
        target: str,
        text: str,
        *,
        humanize: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["type", target, text]
        if humanize:
            args.append("--humanize")
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def select(
        self,
        target: str,
        value: str,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "select",
            target,
            value,
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def upload(
        self,
        target: str,
        path: Union[str, Path],
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "upload",
            target,
            str(path),
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def scroll(
        self,
        dx: int,
        dy: int,
        *,
        humanize: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args: list[str] = ["scroll"]
        if humanize:
            args.append("--humanize")
        if dx < 0 or dy < 0:
            args.append("--")
        args.extend([str(dx), str(dy)])
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def press(
        self,
        key: str,
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run("press", key, json_output=json_output, check=check, session=session, timeout=timeout)

    def wait(
        self,
        *,
        idle: bool = False,
        text: Optional[str] = None,
        url: Optional[str] = None,
        selector: Optional[str] = None,
        timeout_ms: Optional[int] = None,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args: list[str] = ["wait"]
        if idle:
            args.append("--idle")
        if text is not None:
            args.extend(["--text", text])
        if url is not None:
            args.extend(["--url", url])
        if selector is not None:
            args.extend(["--selector", selector])
        if timeout_ms is not None:
            args.extend(["--timeout-ms", str(timeout_ms)])
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def batch(
        self,
        *,
        file_path: Union[str, Path, None] = None,
        ops: Union[str, list[Any], dict[str, Any], None] = None,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args: list[str] = ["batch"]
        if file_path is not None:
            args.extend(["--file", str(file_path)])
        if ops is not None:
            ops_str = ops if isinstance(ops, str) else json.dumps(ops)
            args.extend(["--ops", ops_str])
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def extract(
        self,
        mode: str = "markdown",
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "extract",
            mode,
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def eval(
        self,
        js: str,
        *,
        main_world: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["eval", js]
        if main_world:
            args.append("--main-world")
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def screenshot(
        self,
        out_path: Union[str, Path],
        *,
        target: Optional[str] = None,
        full_page: bool = False,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args = ["screenshot", str(out_path)]
        if target is not None:
            args.extend(["--target", target])
        if full_page:
            args.append("--full-page")
        return self.run(*args, json_output=json_output, check=check, session=session, timeout=timeout)

    def pdf(
        self,
        out_path: Union[str, Path],
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "pdf",
            str(out_path),
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def logs(
        self,
        kind: str = "console",
        *,
        session: Optional[str] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        return self.run(
            "logs",
            kind,
            json_output=json_output,
            check=check,
            session=session,
            timeout=timeout,
        )

    def bench(
        self,
        *,
        iterations: Optional[int] = None,
        warmup: Optional[int] = None,
        json_output: bool = True,
        check: bool = False,
        timeout: Optional[float] = None,
    ) -> CommandResult:
        args: list[str] = ["bench"]
        if iterations is not None:
            args.extend(["--iterations", str(iterations)])
        if warmup is not None:
            args.extend(["--warmup", str(warmup)])
        return self.run(
            *args,
            json_output=json_output,
            check=check,
            include_session=False,
            timeout=timeout,
        )


# ---------------------------------------------------------------------------
# Stdio MCP JSON-RPC 2.0 Client
# ---------------------------------------------------------------------------

class McpError(RuntimeError):
    """Raised when an MCP JSON-RPC call fails, times out, or returns an error."""


class McpClient:
    """Thread-buffered stdio JSON-RPC 2.0 client for `mimikyu mcp`."""

    def __init__(
        self,
        bin_path: Union[str, Path, None] = None,
        state_dir: Union[str, Path, None] = None,
        engine: Optional[str] = None,
        timeout: float = 30.0,
    ) -> None:
        cfg = get_config()
        resolved_bin = str(bin_path) if bin_path is not None else cfg.bin_path
        self.bin_path: str = resolved_bin.strip()
        if not self.bin_path:
            raise FileNotFoundError("mimikyu binary path is empty")
        self.engine: Optional[str] = engine
        self.timeout: float = float(timeout)
        self._next_id: int = 1
        self._closed: bool = False
        self._notifications: list[dict[str, Any]] = []
        self._stderr_lines: collections.deque[str] = collections.deque(maxlen=200)
        self._rx_queue: queue.Queue[Optional[dict[str, Any]]] = queue.Queue()

        if state_dir is None:
            self._temp_dir: Optional[tempfile.TemporaryDirectory[str]] = _make_short_temp_dir("mk_")
            self.state_dir: Path = Path(self._temp_dir.name)
            self._finalizer: Optional[weakref.finalize] = weakref.finalize(
                self, _cleanup_temp_dir, self._temp_dir
            )
        else:
            self._temp_dir = None
            self.state_dir = Path(state_dir)
            self.state_dir.mkdir(parents=True, exist_ok=True)
            self._finalizer = None

        cmd: list[str] = [self.bin_path, "--state-dir", str(self.state_dir)]
        if self.engine:
            cmd.extend(["--engine", self.engine])
        cmd.append("mcp")

        self._proc = subprocess.Popen(
            cmd,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
            start_new_session=True,
        )
        self._pgid: Optional[int] = self._proc.pid
        self._stdout_thread = threading.Thread(
            target=self._read_stdout_loop,
            name="mimikyu-mcp-stdout",
            daemon=True,
        )
        self._stderr_thread = threading.Thread(
            target=self._read_stderr_loop,
            name="mimikyu-mcp-stderr",
            daemon=True,
        )
        self._stdout_thread.start()
        self._stderr_thread.start()

    def __enter__(self) -> "McpClient":
        return self

    def __exit__(self, exc_type: Any, exc_val: Any, exc_tb: Any) -> None:
        self.close()

    def _read_stdout_loop(self) -> None:
        stdout = self._proc.stdout
        if stdout is None:
            self._rx_queue.put(None)
            return
        try:
            while True:
                line = stdout.readline()
                if not line:
                    break
                stripped = line.strip()
                if not stripped:
                    continue
                if stripped.lower().startswith(b"content-length:"):
                    try:
                        content_len = int(stripped.split(b":", 1)[1].strip())
                    except ValueError:
                        continue
                    while True:
                        hdr_line = stdout.readline()
                        if not hdr_line or hdr_line in (b"\r\n", b"\n"):
                            break
                    if content_len < 0:
                        continue
                    payload_bytes = _read_exact(stdout, content_len)
                    if len(payload_bytes) < content_len:
                        break
                    if content_len == 0:
                        continue
                    try:
                        msg = json.loads(payload_bytes.decode("utf-8"))
                        if isinstance(msg, dict):
                            self._rx_queue.put(msg)
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        continue
                else:
                    try:
                        msg = json.loads(stripped.decode("utf-8"))
                        if isinstance(msg, dict):
                            self._rx_queue.put(msg)
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        continue
        finally:
            self._rx_queue.put(None)

    def _read_stderr_loop(self) -> None:
        stderr = self._proc.stderr
        if stderr is None:
            return
        for raw in iter(stderr.readline, b""):
            self._stderr_lines.append(raw.decode("utf-8", errors="replace").rstrip("\r\n"))

    def _recent_stderr(self) -> str:
        return "\n".join(self._stderr_lines)

    def _send_json(self, obj: dict[str, Any]) -> None:
        if self._proc.stdin is None:
            raise McpError("MCP process stdin is closed")
        raw = (json.dumps(obj, separators=(",", ":")) + "\n").encode("utf-8")
        try:
            self._proc.stdin.write(raw)
            self._proc.stdin.flush()
        except OSError as exc:
            raise McpError(f"Failed to write to MCP stdin: {exc}\n{self._recent_stderr()}") from exc

    def notify(self, method: str, params: Optional[dict[str, Any]] = None) -> None:
        msg: dict[str, Any] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._send_json(msg)

    def request(
        self,
        method: str,
        params: Optional[dict[str, Any]] = None,
        timeout: Optional[float] = None,
    ) -> Any:
        req_id = self._next_id
        self._next_id += 1
        msg: dict[str, Any] = {"jsonrpc": "2.0", "id": req_id, "method": method}
        if params is not None:
            msg["params"] = params
        self._send_json(msg)

        eff_timeout = self.timeout if timeout is None else float(timeout)
        deadline = time.monotonic() + eff_timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise McpError(
                    f"MCP request '{method}' (id={req_id}) timed out after {eff_timeout:.2f}s.\n"
                    f"Stderr:\n{self._recent_stderr()}"
                )
            try:
                item = self._rx_queue.get(timeout=remaining)
            except queue.Empty:
                raise McpError(
                    f"MCP request '{method}' (id={req_id}) timed out after {eff_timeout:.2f}s.\n"
                    f"Stderr:\n{self._recent_stderr()}"
                )
            if item is None:
                raise McpError(
                    f"MCP server closed stdout unexpectedly while waiting for '{method}' (id={req_id}).\n"
                    f"Stderr:\n{self._recent_stderr()}"
                )
            if "id" not in item or item.get("id") is None:
                self._notifications.append(item)
                continue
            if item.get("id") != req_id:
                continue
            if "error" in item and item["error"] is not None:
                raise McpError(f"MCP error for '{method}': {item['error']}")
            return item.get("result")

    def initialize(
        self,
        protocol_version: str = "2024-11-05",
        client_name: str = "mimikyu-e2e",
        client_version: str = "0.1.0",
        timeout: Optional[float] = None,
    ) -> dict[str, Any]:
        result = self.request(
            "initialize",
            params={
                "protocolVersion": protocol_version,
                "capabilities": {},
                "clientInfo": {"name": client_name, "version": client_version},
            },
            timeout=timeout,
        )
        self.notify("notifications/initialized")
        return result if isinstance(result, dict) else {"result": result}

    def tools_list(self, timeout: Optional[float] = None) -> list[dict[str, Any]]:
        result = self.request("tools/list", params={}, timeout=timeout)
        if isinstance(result, dict) and isinstance(result.get("tools"), list):
            return list(result["tools"])
        return []

    def tools_call(
        self,
        name: str,
        arguments: Optional[dict[str, Any]] = None,
        timeout: Optional[float] = None,
    ) -> dict[str, Any]:
        result = self.request(
            "tools/call",
            params={"name": name, "arguments": arguments or {}},
            timeout=timeout,
        )
        return result if isinstance(result, dict) else {"result": result}

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            if self._proc.stdin is not None:
                try:
                    self._proc.stdin.close()
                except OSError:
                    pass
            try:
                self._proc.wait(timeout=3.0)
            except subprocess.TimeoutExpired:
                _kill_process_group(self._proc, pgid=self._pgid)
            else:
                # Leader exited; ensure no orphaned grandchildren remain in its process group.
                if self._pgid is not None:
                    try:
                        os.killpg(self._pgid, signal.SIGTERM)
                    except (ProcessLookupError, PermissionError, OSError):
                        pass
                    else:
                        deadline = time.monotonic() + 0.3
                        while time.monotonic() < deadline:
                            try:
                                os.killpg(self._pgid, 0)
                            except (ProcessLookupError, PermissionError, OSError):
                                break
                            time.sleep(0.02)
                    try:
                        os.killpg(self._pgid, signal.SIGKILL)
                    except (ProcessLookupError, PermissionError, OSError):
                        pass
            for pipe in (self._proc.stdout, self._proc.stderr):
                if pipe is not None:
                    try:
                        pipe.close()
                    except OSError:
                        pass
            self._stdout_thread.join(timeout=1.0)
            self._stderr_thread.join(timeout=1.0)
        finally:
            if self._finalizer is not None:
                self._finalizer()
            elif self._temp_dir is not None:
                _cleanup_temp_dir(self._temp_dir)


# ---------------------------------------------------------------------------
# Accessibility Tree @eN Ref Extraction
# ---------------------------------------------------------------------------

_REF_RE = re.compile(r"@e\d+")
_EXACT_REF_RE = re.compile(r"^@e\d+$")
_QUOTED_TEXT_RE = re.compile(r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'')
_BRACKET_TOKEN_RE = re.compile(r"\[([^\]]+)\]")
_AX_CONTAINER_KEYS: tuple[str, ...] = (
    "result",
    "data",
    "snapshot",
    "root",
    "ax_tree",
    "tree",
    "nodes",
    "elements",
    "items",
    "children",
    "refs",
)
_MAX_AX_DEPTH: int = 128


def _extract_refs_from_text(
    text: str,
    role: Optional[str],
    name_substr: Optional[str],
    seen: set[str],
    out: list[str],
) -> None:
    role_pat = re.compile(rf"\b{re.escape(role)}\b", re.IGNORECASE) if role else None
    name_low = name_substr.lower() if name_substr else None
    for line in text.splitlines():
        refs = _REF_RE.findall(line)
        if not refs:
            continue
        if role_pat is not None:
            unquoted = _QUOTED_TEXT_RE.sub(" ", line)
            bracket_tokens = _BRACKET_TOKEN_RE.findall(unquoted)
            if bracket_tokens:
                if not any(role_pat.search(tok) for tok in bracket_tokens):
                    continue
            elif not role_pat.search(unquoted):
                continue
        if name_low is not None and name_low not in line.lower():
            continue
        for ref in refs:
            if ref not in seen:
                seen.add(ref)
                out.append(ref)


def _walk_ax_node(
    node: Any,
    role: Optional[str],
    name_substr: Optional[str],
    seen: set[str],
    out: list[str],
    *,
    _visited_ids: Optional[set[int]] = None,
    _depth: int = 0,
) -> None:
    if _depth > _MAX_AX_DEPTH:
        return

    if isinstance(node, CommandResult):
        if node.payload is not None:
            _walk_ax_node(
                node.payload,
                role,
                name_substr,
                seen,
                out,
                _visited_ids=_visited_ids,
                _depth=_depth + 1,
            )
        elif node.stdout:
            _extract_refs_from_text(node.stdout, role, name_substr, seen, out)
        return

    if isinstance(node, str):
        _extract_refs_from_text(node, role, name_substr, seen, out)
        return

    if not isinstance(node, (list, dict)):
        return

    if _visited_ids is None:
        _visited_ids = set()
    obj_id = id(node)
    if obj_id in _visited_ids:
        return
    _visited_ids.add(obj_id)
    try:
        if isinstance(node, list):
            for item in node:
                _walk_ax_node(
                    item,
                    role,
                    name_substr,
                    seen,
                    out,
                    _visited_ids=_visited_ids,
                    _depth=_depth + 1,
                )
            return

        handled_keys: set[str] = set()

        # Handle MCP tools/call content array: {"content": [{"type": "text", "text": "..."}]}
        content = node.get("content")
        if isinstance(content, list):
            handled_keys.add("content")
            for entry in content:
                if isinstance(entry, dict) and isinstance(entry.get("text"), str):
                    txt = entry["text"]
                    try:
                        parsed = json.loads(txt)
                        _walk_ax_node(
                            parsed,
                            role,
                            name_substr,
                            seen,
                            out,
                            _visited_ids=_visited_ids,
                            _depth=_depth + 1,
                        )
                    except json.JSONDecodeError:
                        _extract_refs_from_text(txt, role, name_substr, seen, out)
                else:
                    _walk_ax_node(
                        entry,
                        role,
                        name_substr,
                        seen,
                        out,
                        _visited_ids=_visited_ids,
                        _depth=_depth + 1,
                    )

        # Handle ref-keyed mappings like {"@e1": {"role": "button", "name": "Submit"}}
        for k, v in node.items():
            if isinstance(k, str) and _EXACT_REF_RE.match(k):
                handled_keys.add(k)
                if isinstance(v, dict):
                    merged = dict(v)
                    merged.setdefault("ref", k)
                    _walk_ax_node(
                        merged,
                        role,
                        name_substr,
                        seen,
                        out,
                        _visited_ids=_visited_ids,
                        _depth=_depth + 1,
                    )
                elif isinstance(v, str):
                    _extract_refs_from_text(f"{k} {v}", role, name_substr, seen, out)

        # Check if this dict itself represents a node with an @eN ref
        ref_val: Optional[str] = None
        for key in ("ref", "id", "ax_ref", "ref_id"):
            candidate = node.get(key)
            if isinstance(candidate, str) and _EXACT_REF_RE.match(candidate.strip()):
                ref_val = candidate.strip()
                break

        if ref_val is not None:
            node_roles = [
                str(node[k]).strip().lower()
                for k in ("role", "ax_role", "aria_role", "tag", "type")
                if node.get(k) is not None
            ]
            node_names = [
                str(node[k]).lower()
                for k in ("name", "label", "text", "title", "placeholder", "value")
                if node.get(k) is not None
            ]
            role_ok = role is None or role.strip().lower() in node_roles
            name_ok = name_substr is None or any(name_substr.lower() in n for n in node_names)
            if role_ok and name_ok and ref_val not in seen:
                seen.add(ref_val)
                out.append(ref_val)

        # Recurse into known container fields first, then any remaining nested dict/list values
        for key in _AX_CONTAINER_KEYS:
            if key in node and key not in handled_keys:
                handled_keys.add(key)
                _walk_ax_node(
                    node[key],
                    role,
                    name_substr,
                    seen,
                    out,
                    _visited_ids=_visited_ids,
                    _depth=_depth + 1,
                )

        if isinstance(node.get("text"), str) and ref_val is None and "text" not in handled_keys:
            handled_keys.add("text")
            _extract_refs_from_text(node["text"], role, name_substr, seen, out)

        for k, v in node.items():
            if k in handled_keys:
                continue
            if isinstance(v, (dict, list)):
                _walk_ax_node(
                    v,
                    role,
                    name_substr,
                    seen,
                    out,
                    _visited_ids=_visited_ids,
                    _depth=_depth + 1,
                )
    finally:
        _visited_ids.discard(obj_id)


def extract_all_ax_refs(
    snapshot_data: Any,
    role: Optional[str] = None,
    name_substr: Optional[str] = None,
) -> list[str]:
    """Return all matching @eN refs from a snapshot payload or text tree."""
    seen: set[str] = set()
    out: list[str] = []
    _walk_ax_node(snapshot_data, role, name_substr, seen, out)
    return out


def extract_ax_ref(
    snapshot_data: Any,
    role: Optional[str] = None,
    name_substr: Optional[str] = None,
) -> Optional[str]:
    """Return the first matching @eN ref from a snapshot payload or text tree."""
    matches = extract_all_ax_refs(snapshot_data, role=role, name_substr=name_substr)
    return matches[0] if matches else None


# ---------------------------------------------------------------------------
# R3 Static & Schema Scope Compliance
# ---------------------------------------------------------------------------

# Constructed via concatenated literals so static raw-text scans of this file
# achieve zero false positives without requiring any file exclusions.
R3_FORBIDDEN_KEYWORDS: tuple[str, ...] = (
    "disable_" + "coop",
    "cap" + "tcha",
    "recap" + "tcha",
    "hcap" + "tcha",
    "turn" + "stile",
    "sun" + "dial",
    "cf_" + "clearance",
    "waf_" + "bypass",
    "waf_" + "evasion",
    "waf " + "evasion",
    "gate_" + "bypass",
    "gate " + "bypass",
    "challenge_" + "bypass",
    "challenge_" + "solver",
    "anti" + "bot",
    "anti-" + "bot",
    "human_" + "verification",
    "human-" + "verification",
)


def _scan_text_lines(source_label: str, text: str) -> list[tuple[str, int, str]]:
    violations: list[tuple[str, int, str]] = []
    for line_no, line in enumerate(text.splitlines(), start=1):
        low = line.lower()
        for kw in R3_FORBIDDEN_KEYWORDS:
            if kw in low:
                violations.append((source_label, line_no, kw))
    return violations


def _scan_path(path: Path) -> list[tuple[str, int, str]]:
    if path.is_dir():
        violations: list[tuple[str, int, str]] = []
        for entry in sorted(path.rglob("*")):
            if not entry.is_file():
                continue
            if "__pycache__" in entry.parts or entry.suffix == ".pyc":
                continue
            try:
                text = entry.read_text(encoding="utf-8")
            except UnicodeDecodeError:
                continue
            violations.extend(_scan_text_lines(str(entry), text))
        return violations
    return _scan_text_lines(str(path), path.read_text(encoding="utf-8"))


def check_r3_compliance(
    target: Union[str, Path, Iterable[Union[str, Path]]],
) -> list[tuple[str, int, str]]:
    """Scan text, a file/directory path, or an iterable of paths for R3 keywords."""
    if isinstance(target, Path):
        return _scan_path(target)
    if isinstance(target, str):
        if "\n" not in target and len(target) < 4096:
            try:
                p = Path(target)
                if p.exists():
                    return _scan_path(p)
            except OSError:
                pass
        return _scan_text_lines("<text>", target)

    violations: list[tuple[str, int, str]] = []
    for item in target:
        violations.extend(check_r3_compliance(item))
    return violations


def assert_r3_compliant(
    target: Union[str, Path, Iterable[Union[str, Path]]],
) -> None:
    """Raise AssertionError if any R3 forbidden keyword appears in target."""
    violations = check_r3_compliance(target)
    if violations:
        formatted = "; ".join(f"{src}:{ln}: '{kw}'" for src, ln, kw in violations[:10])
        extra = f" (+{len(violations) - 10} more)" if len(violations) > 10 else ""
        raise AssertionError(f"R3 compliance violation(s): {formatted}{extra}")


__all__ = [
    "CommandError",
    "CommandResult",
    "E2EConfig",
    "LocalTestCluster",
    "McpClient",
    "McpError",
    "MimikyuClient",
    "R3_FORBIDDEN_KEYWORDS",
    "assert_r3_compliant",
    "check_r3_compliance",
    "extract_all_ax_refs",
    "extract_ax_ref",
    "get_config",
]
