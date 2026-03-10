# mcp-methods

Shared Rust-powered utilities for MCP servers. Pip-installable library that provides fast file search, GitHub integration, and text processing — the common building blocks needed when writing MCP tool servers.

## Install

```bash
pip install mcp-methods
```

For development (requires Rust toolchain + maturin):

```bash
pip install -e ".[dev]"
```

## What's included

| Function | Purpose |
|---|---|
| `ripgrep_files` | Ripgrep-powered file search with parallel walking, early termination, context lines, and multiple output modes |
| `ripgrep` | Drop-in replacement for the Claude Code Grep tool interface |
| `read_file` | Safe file reading with path traversal protection and line range support |
| `git_issue` | Fetch GitHub issues/PRs with smart compaction (collapses code blocks, filters bots, truncates) |
| `git_api` | GitHub REST API wrapper with token auth |
| `ElementCache` | Drill-down cache for collapsed elements in GitHub discussions |
| `ripgrep_lines` | Search through text lines with context window merging |
| `ripgrep_json_fields` | Extract fields from JSON text |
| `compact_discussion` / `compact_text` / `collapse_code_blocks` | Text compaction utilities |
| `extract_github_refs` | Parse GitHub issue/PR references from text |
| `detect_git_repo` / `validate_repo` | Git repository detection and validation |

## Usage in an MCP server

```python
from mcp_methods import ripgrep, ripgrep_files, read_file, git_issue, ElementCache

PROJECT = "/path/to/project"

# ripgrep() — Claude Code Grep-compatible interface
# Returns all matches by default (head_limit=None means no cap)
results = ripgrep(r"def \w+", path=PROJECT, type="py")

# ripgrep_files() — full interface with multi-dir, max_results, transform
results = ripgrep_files(
    [PROJECT],
    r"def \w+",
    type_filter="py",
    relative_to=PROJECT,  # project-relative paths in output
    max_results=500,      # early termination at engine level
)

# Safe file reading with allowed directory enforcement
content = read_file("src/main.py", [PROJECT])

# GitHub issue with compaction for context windows
cache = ElementCache()
issue = cache.fetch_issue("owner/repo", 123)
# Drill into collapsed elements
element = cache.retrieve("owner/repo", 123, "cb_1")
```

## Architecture

All heavy lifting is in Rust (PyO3/maturin), compiled to a native Python extension:

- **grep**: Uses `grep-regex`, `grep-searcher`, and `ignore` crates directly (not a ripgrep subprocess). Parallel file walking with per-thread searcher reuse, mmap, SIMD literal optimization, and `.gitignore` support.
- **GitHub**: HTTP via `ureq`, JSON processing via `serde_json`, text compaction in Rust.
- **File I/O**: Path validation and traversal protection in Rust.

## License

MIT
