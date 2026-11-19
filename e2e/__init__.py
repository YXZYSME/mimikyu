"""Opaque-box Python 3 stdlib E2E test infrastructure for mimikyu.

Strictly scoped to standard browser automation, isolation, environment
consistency, and agentic workflow verification against local 127.0.0.1:0
fixtures with full R3 scope compliance.
"""

from pathlib import Path

E2E_DIR: Path = Path(__file__).resolve().parent

__all__ = ["E2E_DIR"]
