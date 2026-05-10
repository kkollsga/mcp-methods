"""Source-tool registrations for FastMCP servers.

Each helper registers one MCP tool that mirrors the YAML+CLI surface:
`read_source`, `grep`, `list_source`. The implementations are thin
wrappers over the existing `mcp_methods` PyO3 surface (`read_file`,
`ripgrep_files`, `list_dir`) plus a path-sandbox check so reads stay
inside the configured `source_roots`.

The sandbox check resolves both the requested path and the root via
`os.path.realpath` and rejects anything that doesn't share the resolved
root as a prefix — same shape as the Rust `source::resolve_under_roots`.
"""

from __future__ import annotations

import os
from collections.abc import Sequence

from mcp_methods import list_dir, read_file, ripgrep_files


def register_source_tools(app, *, source_roots: Sequence[str]) -> None:
    """Register `read_source`, `grep`, `list_source` on a FastMCP app.

    `source_roots` is a list of directories (absolute or relative —
    they're canonicalised at registration time). Path arguments passed
    to the tools are resolved against the *first* root in the list;
    `grep` searches across all of them.
    """
    if not source_roots:
        raise ValueError("register_source_tools needs at least one source_root")
    resolved_roots = [os.path.realpath(r) for r in source_roots]
    for r in resolved_roots:
        if not os.path.isdir(r):
            raise ValueError(f"source_root is not a directory: {r}")
    primary = resolved_roots[0]

    @app.tool(
        description=(
            "Read a file from the configured source root(s). Pass "
            "`start_line`/`end_line` to slice, `grep` to filter to matching "
            "lines, `max_chars` to cap output. Path traversal attempts are "
            "rejected."
        )
    )
    def read_source(
        file_path: str,
        start_line: int | None = None,
        end_line: int | None = None,
        grep: str | None = None,
        grep_context: int | None = None,
        max_matches: int | None = None,
        max_chars: int | None = None,
    ) -> str:
        resolved = _resolve_under(file_path, primary, resolved_roots)
        if resolved is None:
            return f"Error: path '{file_path}' resolves outside the configured source roots."
        return read_file(
            resolved,
            resolved_roots,
            offset=(start_line - 1) if start_line else 0,
            limit=(end_line - start_line + 1)
            if (start_line and end_line)
            else (end_line if end_line else 0),
            max_chars=max_chars or 0,
        )

    @app.tool(
        description=(
            "Search source files using ripgrep. `pattern` is a regex. "
            "`glob` filters file paths. `context` adds N surrounding lines "
            "per match. `max_results` caps total matches (default 50)."
        )
    )
    def grep(
        pattern: str,
        glob: str = "*",
        context: int = 0,
        max_results: int | None = 50,
        case_insensitive: bool = False,
    ) -> str:
        return ripgrep_files(
            resolved_roots,
            pattern,
            glob=glob,
            context=context,
            max_results=max_results,
            case_insensitive=case_insensitive,
            output_mode="content",
        )

    @app.tool(
        description=(
            "List directory contents under the configured source root. "
            "`path` is resolved against the first source root (`.` lists "
            "the root). `depth` controls recursion. `glob` filters entry "
            "names. `dirs_only=true` shows only directories."
        )
    )
    def list_source(
        path: str = ".",
        depth: int = 1,
        glob: str | None = None,
        dirs_only: bool = False,
    ) -> str:
        resolved = _resolve_dir_under(path, primary, resolved_roots)
        if resolved is None:
            return f"Error: path '{path}' resolves outside the configured source roots."
        return list_dir(
            resolved,
            depth=depth,
            glob=glob,
            dirs_only=dirs_only,
            relative_to=primary,
        )


def _resolve_under(requested: str, primary_root: str, all_roots: Sequence[str]) -> str | None:
    """Resolve a file path under the source roots. Returns the canonical
    absolute path or None if it escapes."""
    base = primary_root if not os.path.isabs(requested) else ""
    candidate = (
        os.path.realpath(os.path.join(base, requested)) if base else os.path.realpath(requested)
    )
    for root in all_roots:
        if candidate == root or candidate.startswith(root + os.sep):
            return candidate
    return None


def _resolve_dir_under(requested: str, primary_root: str, all_roots: Sequence[str]) -> str | None:
    """Same as `_resolve_under` but also requires the target to be a
    directory (matches the Rust `resolve_dir_under_roots` behaviour)."""
    resolved = _resolve_under(requested, primary_root, all_roots)
    if resolved is None or not os.path.isdir(resolved):
        return None
    return resolved
