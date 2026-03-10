"""mcp-methods: Reusable utility methods for MCP servers."""

from __future__ import annotations

from mcp_methods._mcp_methods import (
    ElementCache,
    collapse_code_blocks,
    compact_discussion,
    compact_text,
    detect_git_repo,
    extract_github_refs,
    git_api,
    git_issue,
    has_git_token,
    read_file,
    ripgrep_files,
    ripgrep_json_fields,
    ripgrep_lines,
    validate_repo,
)
from mcp_methods._utils import load_env, timed


def ripgrep(
    pattern: str,
    *,
    path: str = ".",
    glob: str = "*",
    type: str | None = None,
    output_mode: str = "files_with_matches",
    case_insensitive: bool = False,
    multiline: bool = False,
    context_before: int = 0,
    context_after: int = 0,
    context: int = 0,
    line_numbers: bool = True,
    head_limit: int | None = None,
    offset: int = 0,
    relative_to: str | None = None,
) -> str:
    """Ripgrep tool matching the Claude Code Grep interface.

    Drop-in replacement for Claude's built-in Grep tool — same parameter
    names, same defaults, same output format. Powered by ripgrep crates.

    Parameters match the Claude Grep tool schema::

        pattern         Regex pattern to search for (required)
        path            File or directory to search (default: cwd)
        glob            Glob to filter files, e.g. "*.py" (default: "*")
        type            File type: "py", "js", "rust", etc.
        output_mode     "content" | "files_with_matches" (default) | "count"
        case_insensitive  Case-insensitive search
        multiline       Multiline mode (. matches newlines)
        context_before  Lines before each match (-B)
        context_after   Lines after each match (-A)
        context         Lines before and after (-C)
        line_numbers    Show line numbers (default: True)
        head_limit      Limit output entries (None = unlimited)
        offset          Skip first N entries
        relative_to     Base path for relative output paths
    """
    return ripgrep_files(
        [path],
        pattern,
        glob=glob,
        type_filter=type,
        output_mode=output_mode,
        case_insensitive=case_insensitive,
        multiline=multiline,
        context_before=context_before,
        context_after=context_after,
        context=context,
        line_numbers=line_numbers,
        head_limit=head_limit,
        offset=offset,
        relative_to=relative_to,
    )


__all__ = [
    # Rust-powered
    "ripgrep",
    "ripgrep_files",
    "ripgrep_lines",
    "ripgrep_json_fields",
    "read_file",
    "validate_repo",
    "extract_github_refs",
    "collapse_code_blocks",
    "compact_text",
    "compact_discussion",
    "git_api",
    "git_issue",
    "has_git_token",
    "detect_git_repo",
    "ElementCache",
    # Python
    "timed",
    "load_env",
]
