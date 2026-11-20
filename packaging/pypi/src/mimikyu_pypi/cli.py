"""Downloads, SHA-256-verifies, caches, and executes the native mimikyu binary."""

from __future__ import annotations

import hashlib
import io
import os
import platform
import shutil
import stat
import sys
import tarfile
import tempfile
import urllib.request
from pathlib import Path

from . import __version__

REPO = os.environ.get("MIMIKYU_REPO", "YXZYSME/mimikyu")


def _resolve_target() -> str:
    sys_name = platform.system().lower()
    machine = platform.machine().lower()

    if machine in ("x86_64", "amd64"):
        arch = "x86_64"
    elif machine in ("arm64", "aarch64"):
        arch = "aarch64"
    else:
        raise RuntimeError(f"Unsupported CPU architecture: {machine!r}")

    if sys_name == "darwin":
        return f"{arch}-apple-darwin"
    if sys_name == "linux":
        return f"{arch}-unknown-linux-gnu"
    raise RuntimeError(
        f"Unsupported operating system: {sys_name!r} (supported: macOS and Linux)"
    )


def _resolve_cache_root() -> Path:
    override = os.environ.get("MIMIKYU_CACHE_DIR")
    if override:
        return Path(override)
    if platform.system().lower() == "darwin":
        return Path.home() / "Library" / "Caches" / "mimikyu" / "pypi"
    xdg = os.environ.get("XDG_CACHE_HOME")
    base = Path(xdg) if xdg else (Path.home() / ".cache")
    return base / "mimikyu" / "pypi"


def _fetch_bytes(url: str) -> bytes:
    req = urllib.request.Request(
        url,
        headers={"User-Agent": f"mimikyu-pypi/{__version__}"},
    )
    with urllib.request.urlopen(req, timeout=60) as resp:
        return resp.read()


def _parse_expected_sha256(sums_text: str, archive_name: str) -> str:
    for raw_line in sums_text.splitlines():
        parts = raw_line.strip().split()
        if len(parts) >= 2:
            digest = parts[0].lower()
            fname = parts[1].lstrip("*")
            if fname == archive_name and len(digest) == 64:
                return digest
    raise RuntimeError(f"Checksum entry for {archive_name!r} not found in SHA256SUMS")


def ensure_binary() -> Path:
    override_bin = os.environ.get("MIMIKYU_BIN")
    if override_bin and Path(override_bin).is_file():
        return Path(override_bin)

    target = _resolve_target()
    install_dir = _resolve_cache_root() / f"v{__version__}-{target}"
    bin_path = install_dir / "mimikyu"
    if bin_path.is_file():
        return bin_path

    archive_name = f"mimikyu-{target}.tar.gz"
    base_url = f"https://github.com/{REPO}/releases/download/v{__version__}"
    sys.stderr.write(
        f"[mimikyu] Downloading and verifying mimikyu v{__version__} ({target})...\n"
    )
    sys.stderr.flush()

    sums_bytes = _fetch_bytes(f"{base_url}/SHA256SUMS")
    tar_bytes = _fetch_bytes(f"{base_url}/{archive_name}")

    expected_sha = _parse_expected_sha256(sums_bytes.decode("utf-8"), archive_name)
    actual_sha = hashlib.sha256(tar_bytes).hexdigest()
    if actual_sha != expected_sha:
        raise RuntimeError(
            f"SHA-256 mismatch for {archive_name}: expected {expected_sha}, got {actual_sha}"
        )

    install_dir.mkdir(parents=True, exist_ok=True)
    tmp_dir = Path(tempfile.mkdtemp(prefix=".stage-", dir=str(install_dir)))
    try:
        with tarfile.open(fileobj=io.BytesIO(tar_bytes), mode="r:gz") as tf:
            member = tf.getmember("mimikyu")
            extracted = tf.extractfile(member)
            if extracted is None:
                raise RuntimeError(f"Archive {archive_name} missing 'mimikyu' file payload")
            staged_bin = tmp_dir / "mimikyu"
            with open(staged_bin, "wb") as out_f:
                shutil.copyfileobj(extracted, out_f)
        os.chmod(staged_bin, stat.S_IRWXU | stat.S_IRGRP | stat.S_IXGRP | stat.S_IROTH | stat.S_IXOTH)
        os.replace(staged_bin, bin_path)
    finally:
        shutil.rmtree(tmp_dir, ignore_errors=True)

    return bin_path


def main() -> None:
    try:
        bin_path = ensure_binary()
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"[mimikyu] Error: {exc}\n")
        sys.exit(1)
    os.execv(str(bin_path), [str(bin_path), *sys.argv[1:]])


def main_mcp() -> None:
    try:
        bin_path = ensure_binary()
    except Exception as exc:  # noqa: BLE001
        sys.stderr.write(f"[mimikyu-mcp] Error: {exc}\n")
        sys.exit(1)
    args = sys.argv[1:] if len(sys.argv) > 1 else ["mcp"]
    os.execv(str(bin_path), [str(bin_path), *args])


if __name__ == "__main__":
    main()
