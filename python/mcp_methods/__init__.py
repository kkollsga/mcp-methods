"""mcp-methods: Reusable utility methods for MCP servers."""

from mcp_methods._mcp_methods import (
    ElementCache,
    collapse_code_blocks,
    compact_discussion,
    compact_text,
    detect_git_repo,
    extract_github_refs,
    git_api,
    git_issue,
    grep_files,
    grep_json_fields,
    grep_lines,
    has_git_token,
    read_file,
    validate_repo,
)
from mcp_methods._utils import load_env, timed

__all__ = [
    # Rust-powered
    "grep_files",
    "read_file",
    "validate_repo",
    "extract_github_refs",
    "grep_lines",
    "grep_json_fields",
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
