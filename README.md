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
| `list_dir` | Tree-formatted directory listing with depth control, glob filtering, `.gitignore` support, dir summaries, and annotation callback |
| `ripgrep_files` | Ripgrep-powered file search with parallel walking, early termination, context lines, and multiple output modes |
| `ripgrep` | Drop-in replacement for the Claude Code Grep tool interface |
| `read_file` | Safe file reading with path traversal protection and line range support |
| `github_discussions` | Fetch a single issue/PR with smart compaction, or list issues/PRs with filters |
| `git_diff` | Compare two commits/branches — local git with GitHub API fallback for shallow clones |
| `git_api` | GitHub REST API wrapper with token auth |
| `ElementCache` | Drill-down cache for collapsed elements in GitHub discussions |
| `ripgrep_lines` | Search through text lines with context window merging |
| `ripgrep_json_fields` | Extract fields from JSON text |
| `compact_discussion` / `compact_text` / `collapse_code_blocks` | Text compaction utilities |
| `extract_github_refs` | Parse GitHub issue/PR references from text |
| `detect_git_repo` / `validate_repo` | Git repository detection and validation |

## Python API

### `list_dir(path, *, depth=1, glob=None, dirs_only=False, relative_to=None, respect_gitignore=True, skip_dirs=None, include_size=False, annotate=None)`

Tree-formatted directory listing.

```python
from mcp_methods import list_dir

# Basic tree
tree = list_dir("/project/src", depth=2, glob="*.py", relative_to="/project")

# With annotation callback (e.g. loc from knowledge graph)
def get_loc(rel_path):
    node = graph.get_file(rel_path)
    return f"({node.loc} loc)" if node else None

tree = list_dir("/project/src", depth=2, annotate=get_loc)
# src/
# ├── main.py        (144 loc)
# ├── utils.py       (28 loc)
# └── models/
#     ├── user.py    (89 loc)
#     └── post.py    (112 loc)
```

### `ripgrep(pattern, *, path=".", glob="*", type=None, output_mode="files_with_matches", max_results=None, offset=0, ...)`

Claude Code Grep-compatible interface.

```python
from mcp_methods import ripgrep

results = ripgrep(r"def \w+", path="/project", type="py", max_results=50)
```

### `ripgrep_files(source_dirs, pattern, *, glob="*", type_filter=None, output_mode="content", max_results=None, offset=0, match_limit=None, relative_to=None, ...)`

Full interface with multi-directory search. `max_results` limits output entries, `match_limit` caps the search engine for early termination.

```python
from mcp_methods import ripgrep_files

results = ripgrep_files(
    ["/project"],
    r"def \w+",
    type_filter="py",
    relative_to="/project",
    match_limit=500,
    max_results=100,
)
```

### `github_discussions(*, repo=None, number=None, kind="all", state="open", sort="created", limit=20, labels=None, expand=None)`

Fetch a single discussion or list discussions.

```python
from mcp_methods import github_discussions, ElementCache

# List open issues
issues = github_discussions(repo="owner/repo", kind="issue", state="open")

# List pull requests
prs = github_discussions(repo="owner/repo", kind="pr", limit=10)

# Fetch a single issue/PR with smart compaction
issue = github_discussions(repo="owner/repo", number=123)

# With caching for drill-down into collapsed elements
cache = ElementCache()
issue = cache.fetch_discussion("owner/repo", 123)
element = cache.retrieve("owner/repo", 123, "cb_1")
```

### `git_diff(base, head, *, repo_path=".", repo=None, stat_only=False, path_filter=None, context=3)`

Compare two commits or branches. Tries local `git diff` first; falls back to GitHub compare API when refs are missing (common in shallow clones).

```python
from mcp_methods import git_diff

# Full diff (local git)
diff = git_diff("main", "feature-branch")

# Stat summary only
stat = git_diff("main", "feature-branch", stat_only=True)

# Filter to specific files
diff = git_diff("v1.0", "v2.0", path_filter="*.py", context=5)

# Shallow clone: local diff fails → auto-detects repo and uses GitHub API
diff = git_diff("v1.0", "v2.0")

# Explicit repo for API fallback
diff = git_diff("v1.0", "v2.0", repo="owner/repo")
```

### `read_file(path, allowed_dirs, *, offset=0, limit=0, max_chars=0, transform=None)`

Safe file reading with path traversal protection.

```python
from mcp_methods import read_file

content = read_file("src/main.py", ["/project"])
```

## Architecture

All heavy lifting is in Rust (PyO3/maturin), compiled to a native Python extension:

- **grep**: Uses `grep-regex`, `grep-searcher`, and `ignore` crates directly (not a ripgrep subprocess). Parallel file walking with per-thread searcher reuse, mmap, SIMD literal optimization, and `.gitignore` support.
- **GitHub**: HTTP via `ureq`, JSON processing via `serde_json`, text compaction in Rust.
- **File I/O**: Path validation and traversal protection in Rust.

## License

MIT
