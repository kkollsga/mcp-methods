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
| `git_api` | GitHub REST API wrapper with token auth |
| `ElementCache` | Drill-down cache for collapsed elements (code blocks, comments, patches, thread segments) in GitHub discussions |
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

### `github_discussions(*, repo=None, number=None, kind="all", state="open", sort="created", limit=20, labels=None)`

Fetch a single discussion or list discussions.

```python
from mcp_methods import github_discussions, ElementCache

# List open issues
issues = github_discussions(repo="owner/repo", kind="issue", state="open")

# List pull requests
prs = github_discussions(repo="owner/repo", kind="pr", limit=10)

# Fetch a single issue/PR with smart compaction
issue = github_discussions(repo="owner/repo", number=123)
```

### `ElementCache` — progressive disclosure for GitHub discussions

Cache for drill-down into collapsed elements. Fetches a discussion once, then lets you explore code blocks, comments, and PR diffs without re-fetching.

```python
from mcp_methods import ElementCache

cache = ElementCache()

# First call fetches from GitHub API, compacts, and caches elements
text = cache.fetch_discussion("owner/repo", 123)

# Subsequent calls return cached summary (no network)
summary = cache.fetch_discussion("owner/repo", 123)
# → "Cached owner/repo#123 — 5 elements available: cb_1, comment_2, patch_1, patch_2, patch_3"

# Force re-fetch when discussion has changed
text = cache.fetch_discussion("owner/repo", 123, refresh=True)

# Drill into a collapsed code block
code = cache.retrieve("owner/repo", 123, "cb_1")

# Drill into a PR patch with grep
result = cache.retrieve("owner/repo", 123, "patch_1", grep="error_handler")

# Drill into a patch with line range
result = cache.retrieve("owner/repo", 123, "patch_2", lines="10-30")

# List available elements
ids = cache.available("owner/repo", 123)
```

PR diffs are automatically collapsed into `patch_N` elements in the compact view. Each patch stores the filename, additions/deletions, and full diff text — supporting grep and line-range drill-down.

Large discussions (50+ comments) are automatically digested: first 5 + maintainer highlights + last 5 comments shown inline, with the full middle cached as individual `comment_N` elements and a searchable `comments_middle` segment.

### `git_api(repo, path, *, truncate_at=80000)`

GitHub REST API wrapper. For comparing branches/tags, use `compare`:

```python
from mcp_methods import git_api

# Compare two refs
diff = git_api("owner/repo", "compare/main...feature-branch")

# List commits
commits = git_api("owner/repo", "commits?per_page=10")
```

### `read_file(path, allowed_dirs, *, offset=0, limit=0, max_chars=0, transform=None)`

Safe file reading with path traversal protection.

```python
from mcp_methods import read_file

content = read_file("src/main.py", ["/project"])
```

## Deployment — `mcp-server` binary

This crate also ships an MCP server binary at `crates/mcp-server`. Build
and install it with:

```bash
cargo install --path crates/mcp-server
# → ~/.cargo/bin/mcp-server
```

For deployments that previously pinned a binary path elsewhere (e.g.
`/opt/miniconda3/envs/embeddings/bin/kglite-mcp-server`), drop a
symlink:

```bash
ln -s ~/.cargo/bin/mcp-server /opt/miniconda3/envs/embeddings/bin/mcp-server
```

The binary is domain-agnostic — source tools + GitHub access + a
manifest-driven tool surface. Downstream binaries (e.g. `kglite-mcp-server`)
re-export `mcp_server::McpServer::new(...)` to layer graph-specific
tools on top while reusing the boot sequence, `.env` loading, workspace
mode, watch mode, and embedder lifecycle.

Operating modes (set via CLI flag or the YAML manifest):

| Mode | How to set | When to use |
|---|---|---|
| bare | no flag | testing the protocol layer in isolation |
| source-root | `--source-root DIR` or YAML `source_root:` | fixed local directory; no clone |
| workspace (github) | `--workspace DIR` | clone-and-track GitHub repos |
| workspace (local) | YAML `workspace: { kind: local, root: ..., watch: ... }` | fixed local dir + optional file watcher; alternative to the legacy `code_review` server |
| watch | `--watch DIR` | rebuild downstream artifacts on file changes |

YAML manifest declarations win over CLI flags when both are set
(same precedence rule as `source_root:`).

## Architecture

All heavy lifting is in Rust (PyO3/maturin), compiled to a native Python extension:

- **grep**: Uses `grep-regex`, `grep-searcher`, and `ignore` crates directly (not a ripgrep subprocess). Parallel file walking with per-thread searcher reuse, mmap, SIMD literal optimization, and `.gitignore` support.
- **GitHub**: HTTP via `ureq`, JSON processing via `serde_json`, text compaction in Rust. PR diffs are collapsed into cacheable elements for progressive disclosure.
- **File I/O**: Path validation and traversal protection in Rust.

## License

MIT
