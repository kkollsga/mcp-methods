"""Shared utilities for MCP method libraries."""

from __future__ import annotations

import functools
import os
import time
from collections.abc import Callable
from pathlib import Path


def timed(func: Callable) -> Callable:
    """Decorator that appends timing info to string return values.

    Usage::

        @timed
        def my_tool(...) -> str:
            ...
    """

    @functools.wraps(func)
    def wrapper(*args, **kwargs):
        t0 = time.perf_counter()
        result = func(*args, **kwargs)
        ms = (time.perf_counter() - t0) * 1000
        return result + f"\n\n⏱ {ms:.0f}ms"

    return wrapper


def load_env(env_file: str | Path) -> None:
    """Load key=value pairs from a .env file into ``os.environ``.

    - Blank lines and lines starting with ``#`` are skipped.
    - Values may be optionally quoted with single or double quotes.
    - Existing environment variables are **not** overwritten.
    """
    path = Path(env_file)
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, val = line.partition("=")
        key = key.strip()
        val = val.strip().strip("'\"")
        if key and key not in os.environ:
            os.environ[key] = val
