# Python API Reference

The full Python surface exposed by `pip install mcp-methods`. Imported via `from mcp_methods import ...`.

## File + directory navigation

### `list_dir(path, *, depth=1, glob=None, dirs_only=False, relative_to=None, respect_gitignore=True, skip_dirs=None, include_size=False, annotate=None)`

Tree-formatted directory listing.

```python
from mcp_methods import list_dir

tree = list_dir("/project/src", depth=2, glob="*.py", relative_to="/project")

# With annotation callback (e.g. line count from a knowledge graph)
def get_loc(rel_path):
    node = graph.get_file(rel_path)
    return f"({node.loc} loc)" if node else None

tree = list_dir("/project/src", depth=2, annotate=get_loc)
# src/
# ├── main.py        (144 loc)
# └── utils.py       (28 loc)
```

### `read_file(path, allowed_dirs, *, offset=0, limit=0, max_chars=0, transform=None)`

Safe file reading with path-traversal protection.

```python
content = read_file("src/main.py", allowed_dirs=["/project"])
```

## ripgrep-backed search

### `ripgrep(pattern, *, path=".", glob="*", type=None, output_mode="files_with_matches", max_results=None, offset=0, ...)`

Claude Code Grep-compatible interface.

```python
results = ripgrep(r"def \w+", path="/project", type="py", max_results=50)
```

### `ripgrep_files(source_dirs, pattern, *, glob="*", type_filter=None, output_mode="content", max_results=None, offset=0, match_limit=None, relative_to=None, ...)`

Full interface with multi-directory search. `max_results` limits output entries, `match_limit` caps the search engine for early termination.

### `ripgrep_lines(text_lines, pattern, *, context=0)`

Search through text lines with context-window merging.

### `ripgrep_json_fields(text, *, fields=None)`

Extract fields from JSON text.

## GitHub

### `github_discussions(*, repo=None, number=None, kind="all", state="open", sort="created", limit=20, labels=None)`

Fetch a single issue/PR with smart compaction, or list issues/PRs with filters.

```python
issues = github_discussions(repo="owner/repo", kind="issue", state="open")
issue = github_discussions(repo="owner/repo", number=123)
```

### `git_api(repo, path, *, truncate_at=80000)`

GitHub REST API wrapper.

```python
diff = git_api("owner/repo", "compare/main...feature-branch")
```

### `has_git_token() -> bool`

Returns whether a usable `GITHUB_TOKEN` is reachable.

### `extract_github_refs(text) -> list[dict]`

Parse GitHub issue/PR references from text.

## Git

### `detect_git_repo(path) -> dict | None`
### `validate_repo(repo_name) -> bool`

## Text processing

### `html_to_text(html) -> str`

Lightweight HTML → plain-text converter (markdown-flavoured).

### `compact_discussion(text) -> tuple[str, str | None]`
### `compact_text(text) -> str`
### `collapse_code_blocks(text) -> str`

Text compaction utilities used by `github_discussions` internally.

## Caching

### `ElementCache`

Drill-down cache for collapsed elements (code blocks, comments, patches) in GitHub discussions. See [the README ElementCache section](https://github.com/kkollsga/mcp-methods/blob/main/README.md) for the full walkthrough.

```python
cache = ElementCache()
text = cache.fetch_issue("owner/repo", 123)
code = cache.retrieve("owner/repo", 123, "cb_1")
```

## FastMCP helpers

See [Using FastMCP Helpers](../guides/using-fastmcp-helpers.md) for `mcp_methods.fastmcp` (separate sub-module).

## See also

- [Python Bindings](../guides/python-bindings.md) — when to use the wheel vs other surfaces
- [docs.rs/mcp-methods](https://docs.rs/mcp-methods) — Rust API (most Python functions are thin wrappers over Rust ones with the same names)
