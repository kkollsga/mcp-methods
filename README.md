# mcp-methods

Reusable utility methods for MCP servers. Extracts common patterns from MCP server implementations into a shared, pip-installable library.

## Install

```bash
pip install -e .
```

## Usage

```python
from mcp_methods import git_issue, git_api, grep_files, read_file, ElementCache

# GitHub API
result = git_api("pydata/xarray", "pulls?state=open")

# Fetch issue/PR with smart compaction
cache = ElementCache()
result = git_issue("pydata/xarray", 11124, cache=cache)

# Search files
result = grep_files(["/path/to/source"], "pattern", glob="*.py")

# Read file with path traversal protection
result = read_file("src/main.py", ["/path/to/source"])
```
