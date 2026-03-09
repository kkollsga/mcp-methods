"""File search and reading methods for MCP servers."""

from __future__ import annotations

import re
from pathlib import Path
from typing import Callable

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

DEFAULT_SKIP_DIRS: set[str] = {
    ".git",
    "node_modules",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".eggs",
    "venv",
    ".venv",
    "target",
    ".cargo",
    ".ruff_cache",
}

# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def grep_files(
    source_dirs: list[str | Path],
    pattern: str,
    *,
    glob: str = "*",
    case_insensitive: bool = False,
    max_results: int = 50,
    skip_dirs: set[str] | None = None,
    relative_to: str | Path | None = None,
    transform: Callable[[str], str] | None = None,
) -> str:
    """Search for a text pattern across files in *source_dirs*.

    *source_dirs*: directories to search (recursively).
    *pattern*: plain text or regex pattern to search for.
    *glob*: file glob to filter (default ``"*"`` for all files).
    *case_insensitive*: set True for case-insensitive matching.
    *max_results*: maximum number of matching lines to return.
    *skip_dirs*: directory names to skip (defaults to :data:`DEFAULT_SKIP_DIRS`).
    *relative_to*: if set, display paths relative to this directory.
    *transform*: optional function to transform file content before searching
        (e.g. HTML-to-text converter).
    """
    try:
        flags = re.IGNORECASE if case_insensitive else 0
        regex = re.compile(pattern, flags)
    except re.error as e:
        return f"Invalid regex pattern: {e}"

    if skip_dirs is None:
        skip_dirs = DEFAULT_SKIP_DIRS

    rel_base = Path(relative_to) if relative_to else None

    matches: list[str] = []
    for source_dir in source_dirs:
        source_dir = Path(source_dir)
        if not source_dir.is_dir():
            continue
        for path in sorted(source_dir.rglob(glob)):
            if not path.is_file():
                continue
            if any(part in skip_dirs for part in path.parts):
                continue
            try:
                if rel_base:
                    rel = str(path.relative_to(rel_base))
                else:
                    rel = str(path.relative_to(source_dir))
                text = path.read_text(encoding="utf-8")
                if transform:
                    text = transform(text)
                for i, line in enumerate(text.splitlines(), 1):
                    if regex.search(line):
                        matches.append(f"  {rel}:{i}  {line.rstrip()}")
                        if len(matches) >= max_results:
                            break
            except (UnicodeDecodeError, PermissionError):
                continue
            if len(matches) >= max_results:
                break
        if len(matches) >= max_results:
            break

    if not matches:
        return f"No matches for '{pattern}' in {glob} files."
    header = f"Found {len(matches)} match(es) for '{pattern}'"
    if len(matches) >= max_results:
        header += f" (capped at {max_results})"
    return header + ":\n" + "\n".join(matches)


def read_file(
    file_path: str,
    allowed_dirs: list[str | Path],
    *,
    start_line: int | None = None,
    end_line: int | None = None,
    max_chars: int | None = None,
    transform: Callable[[str], str] | None = None,
) -> str:
    """Read a file with path-traversal protection.

    *file_path*: relative path resolved against each directory in *allowed_dirs*.
    *allowed_dirs*: directories in which the file must reside (path traversal guard).
    *start_line*: first line to include (1-indexed).
    *end_line*: last line to include (1-indexed, inclusive).
    *max_chars*: truncate the result to this many characters.
    *transform*: optional function to transform file content before returning
        (e.g. HTML-to-text converter).
    """
    # Resolve file against allowed directories
    resolved: Path | None = None
    base_dir: Path | None = None
    for d in allowed_dirs:
        d = Path(d)
        candidate = (d / file_path).resolve()
        if candidate.is_relative_to(d.resolve()) and candidate.exists():
            resolved = candidate
            base_dir = d
            break

    if resolved is None:
        # Check if it's an absolute path inside an allowed dir
        abs_path = Path(file_path).resolve()
        for d in allowed_dirs:
            d = Path(d)
            if abs_path.is_relative_to(d.resolve()) and abs_path.exists():
                resolved = abs_path
                base_dir = d
                break

    if resolved is None:
        return f"Error: file not found or access denied: {file_path}"

    try:
        raw = resolved.read_text(encoding="utf-8")
    except Exception as e:
        return f"Error reading file: {e}"

    # Apply transform (e.g. HTML to text)
    if transform:
        raw = transform(raw)

    all_lines = raw.splitlines()
    total = len(all_lines)

    if start_line is not None or end_line is not None:
        s = max(1, start_line or 1)
        e = min(total, end_line or total)
        selected = all_lines[s - 1 : e]
        numbered = [f"{i:>5}  {line}" for i, line in enumerate(selected, start=s)]
        header = f"{file_path}:{s}-{e}  ({e - s + 1} of {total} lines)"
    else:
        selected = all_lines
        numbered = [f"{i:>5}  {line}" for i, line in enumerate(selected, start=1)]
        header = f"{file_path}  ({total} lines)"

    text = header + "\n" + "\n".join(numbered)

    if max_chars and len(text) > max_chars:
        text = text[:max_chars] + f"\n\n[... truncated at {max_chars} chars — {len(raw)} total]"

    return text
