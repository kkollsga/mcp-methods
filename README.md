# mcp-methods

Shared Rust-powered utilities for MCP servers. Pip-installable Python library AND a native Rust crate — they're the same set of primitives reachable through whichever interface fits your project. Fast file search, GitHub integration, text compaction, and an rmcp-backed MCP server framework. The common building blocks needed when writing MCP tool servers.

The Rust library is the source of truth; the Python wheel is a thin PyO3 binding over it. Rust consumers see zero Python in their dep tree.

## Install — Python

```bash
pip install mcp-methods
```

```python
from mcp_methods import ElementCache, ripgrep, list_dir, github_issues, read_file, html_to_text
```

Single abi3 wheel per OS — works on Python 3.10 through 3.13 without reinstall. The wheel also bundles the `mcp-server` CLI on PATH — see [Deployment — `mcp-server` CLI](#deployment--mcp-server-cli) below.

## Install — Rust library

```toml
[dependencies]
mcp-methods = "0.3"
```

```rust
use mcp_methods::cache::ElementCache;
use mcp_methods::{github, files, grep, list_dir, compact, html};
use mcp_methods::server::{McpServer, ServerOptions, Manifest}; // with default `server` feature
```

Zero pyo3 in the dep tree. The `server` feature (default-on) adds the rmcp-backed framework; disable with `default-features = false` for the bare primitives.

For pre-release coordination — pinning against a specific commit while the framework is iterating quickly — depend on a git rev directly:

```toml
mcp-methods = {
    git = "https://github.com/kkollsga/mcp-methods.git",
    rev = "<short SHA>",
    default-features = false,
    features = ["server"],
}
```

The downstream `kglite-mcp-server` uses this pattern to stay locked to the exact framework rev its integration tests pass against; switch to a published `version = "0.3"` once API churn settles.

## Local development

```bash
make dev           # build + install editable wheel
make test          # python tests
make test-rust     # rust library tests
make test-rust-all # all workspace tests
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
| `has_git_token` | Returns whether a usable `GITHUB_TOKEN` is reachable (used for honest tool listing) |
| `ElementCache` | Drill-down cache for collapsed elements (code blocks, comments, patches, thread segments) in GitHub discussions |
| `html_to_text` | Lightweight HTML → plain-text converter (markdown-flavoured) |
| `ripgrep_lines` | Search through text lines with context window merging |
| `ripgrep_json_fields` | Extract fields from JSON text |
| `compact_discussion` / `compact_text` / `collapse_code_blocks` | Text compaction utilities |
| `extract_github_refs` | Parse GitHub issue/PR references from text |
| `detect_git_repo` / `validate_repo` | Git repository detection and validation |
| `mcp_methods.fastmcp` | Composable tool registrations for FastMCP servers — see below |

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
text = cache.fetch_issue("owner/repo", 123)

# Subsequent calls return cached summary (no network)
summary = cache.fetch_issue("owner/repo", 123)
# → "Cached owner/repo#123 — 5 elements available: cb_1, comment_2, patch_1, patch_2, patch_3"

# Force re-fetch when the issue has changed upstream
text = cache.fetch_issue("owner/repo", 123, refresh=True)

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

## `mcp_methods.fastmcp` — drop-in tools for FastMCP servers

If you're running your own [FastMCP](https://github.com/modelcontextprotocol/python-sdk) server but want the same tool surface the bundled `mcp-server` binary ships (source navigation, graph overview, Cypher with CSV export, save_graph), import these helpers and register them on your `app`:

```python
from mcp.server.fastmcp import FastMCP
from mcp_methods.fastmcp import (
    register_overview,
    register_cypher_query,
    register_source_tools,
    register_save_graph,
    serve_csv_via_http,
)

app = FastMCP("My Server")
register_overview(app, graph, overview_prefix="My custom guidance")
register_cypher_query(app, graph, csv_dir="temp/")
register_source_tools(app, source_roots=["./source"])
register_save_graph(app, graph)
_server, base_url = serve_csv_via_http("temp/")  # optional CORS-enabled HTTP server
app.run(transport="stdio")
```

Each helper is a thin (~10-line) wrapper over the existing Rust PyO3 surface — there's no logic duplication between the YAML-driven binary and these helpers, so agent behaviour is identical regardless of which path booted the server. `graph` is any object exposing `describe()` / `cypher()` / `save()`; kglite's `KnowledgeGraph` satisfies it. A runnable end-to-end stub lives at `examples/fastmcp_demo.py`.

## Deployment — `mcp-server` CLI

`pip install mcp-methods` puts the `mcp-server` CLI on PATH automatically:

```bash
pip install mcp-methods
which mcp-server
# → /opt/miniconda3/bin/mcp-server (or your env's bin dir)
mcp-server --help
```

The wheel bundles the native Rust binary at `mcp_methods/_bin/mcp-server`; the Python entry point (`mcp_methods._cli:main`) execs it. Pure-Rust binary, zero libpython link — same single abi3 wheel per OS that the Python library ships in (3 wheels total across macOS / Linux / Windows).

Generic MCP server, domain-agnostic: source tools + GitHub access + a manifest-driven tool surface. Reads YAML manifests and serves the MCP protocol over stdio.

If you'd rather build the binary from source (no Python in the loop), the crate lives at `crates/mcp-server` in this repo and builds with `cargo build --release -p mcp-server`. Not published to crates.io — Rust toolchain users typically use `cargo install --git https://github.com/kkollsga/mcp-methods mcp-server` or vendor the crate.

Downstream Rust crates (e.g. `kglite-mcp-server`) depend on `mcp-methods` directly and re-use `mcp_methods::server::McpServer::new(...)` to layer domain-specific tools on top while reusing the boot sequence, `.env` loading, workspace mode, and watch mode.

### Cargo features

| Feature | What it enables | Default |
|---|---|---|
| `server` | The MCP server framework: rmcp + tokio + clap + manifest + tool routing + the `mcp_methods::server` module tree. | on |

PyO3 bindings live in a **separate crate** (`mcp-methods-py`) and are only built by maturin for the Python wheel — they don't live in `mcp-methods`'s source or dep tree. `cargo add mcp-methods` is zero-Python:

```toml
# Pure-Rust framework (default):
mcp-methods = "0.3"

# Just the primitives — no rmcp / tokio:
mcp-methods = { version = "0.3", default-features = false }
```

### Operating modes (set via CLI flag or the YAML manifest)

| Mode | How to set | When to use |
|---|---|---|
| bare | no flag | testing the protocol layer in isolation |
| source-root | `--source-root DIR` or YAML `source_root:` | fixed local directory; no clone |
| workspace (github) | `--workspace DIR` | clone-and-track GitHub repos |
| workspace (local) | YAML `workspace: { kind: local, root: ..., watch: ... }` | fixed local dir + optional file watcher; alternative to the legacy `code_review` server |
| watch | `--watch DIR` | rebuild downstream artifacts on file changes |

YAML manifest declarations win over CLI flags when both are set
(same precedence rule as `source_root:`).

### Trust gates (advisory metadata)

The framework parses the `trust:` block but doesn't enforce its flags — downstream binaries enforce them when loading the corresponding extension. Operators reviewing a manifest for security audit can read `trust:` to see every dynamic-code hook the manifest enables in one place.

| Flag | Gates |
|---|---|
| `allow_python_tools` | `tools[].python:` factories (consumer-enforced; mcp-methods 0.3.26+ removed framework-level loading). |
| `allow_embedder` | `extensions.embedder` loaders in downstream binaries (e.g. kglite's bge-m3 wrapper). |
| `allow_query_preprocessor` | `extensions.cypher_preprocessor` query-rewriter hooks (kglite 0.9.25+). |

Each defaults to `false`. Downstream binaries should refuse to load the corresponding extension when the flag is unset, mirroring the embedder pattern. New trust keys are added as advisory metadata in patch releases.

## Rust API reference

`mcp_methods::server::*` is the Rust-consumer surface for the framework. Downstream binaries (e.g. [`kglite-mcp-server`](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server) — ~500 LOC, readable end-to-end) build domain-specific MCP servers by composing these primitives.

| Type / function | Purpose |
|---|---|
| `Manifest`, `load_manifest(path)` | Parse + validate YAML manifests with strict unknown-key checking. `Manifest::to_json()` returns a stable JSON shape for FFI/RPC bridging. |
| `find_sibling_manifest`, `find_workspace_manifest` | Auto-detect manifest files next to a graph file or workspace directory. |
| `Workspace::open(...)`, `Workspace::open_local(...)` | GitHub clone-tracker or local-directory bind. `set_root_dir` swaps the active root atomically (RwLock-protected); `repo_management` is the operator-facing tool dispatcher. |
| `PostActivateHook = Arc<dyn Fn(&Path, &str) -> Result<()> + Send + Sync>` | Caller-supplied callback fired after each successful activate / `set_root_dir`. |
| `watch_dir(...)`, `ChangeHandler`, `WatchHandle` | Filesystem watcher with `notify-debouncer-mini`. Drop the handle to stop. |
| `load_env_walk(start)`, `load_env_explicit(path)` | `.env` loading; mirrors the Python `_utils.load_env` semantics (no-overwrite, quoted-value support). |
| `McpServer`, `ServerOptions` | rmcp-backed framework boot — what `mcp-server` and downstream binaries wrap. |
| `TrustConfig` (parsed; framework records, consumer enforces) | `allow_python_tools`, `allow_embedder`, `allow_query_preprocessor`. See [Trust gates](#trust-gates-advisory-metadata) above. |

For full signatures and per-field docs, run `cargo doc -p mcp-methods --open` or browse [docs.rs](https://docs.rs/mcp-methods).

### Serialising a `Manifest`

`Manifest::to_json() -> serde_json::Value` returns a stable JSON representation of the parsed manifest. Useful when bridging to Python / RPC / JavaScript without per-field FFI getters.

```rust
use mcp_methods::server::manifest::load as load_manifest;

let m = load_manifest(yaml_path)?;
let json = m.to_json();
// Shape:
// {
//   "yaml_path", "name", "instructions", "overview_prefix",
//   "source_roots": [...],
//   "trust": { "allow_python_tools", "allow_embedder", "allow_query_preprocessor" },
//   "tools":   [ { "kind": "cypher"|"python", ... } ],
//   "embedder", "builtins", "env_file", "workspace", "extensions"
// }
```

Field additions are non-breaking patch releases; renames or removals require a minor version bump. See `to_json_shape_is_stable` in `crates/mcp-methods/src/server/manifest.rs` for the snapshot test that locks the canonical shape.

## Architecture

All heavy lifting is in Rust (PyO3/maturin), compiled to a native Python extension:

- **grep**: Uses `grep-regex`, `grep-searcher`, and `ignore` crates directly (not a ripgrep subprocess). Parallel file walking with per-thread searcher reuse, mmap, SIMD literal optimization, and `.gitignore` support.
- **GitHub**: HTTP via `ureq`, JSON processing via `serde_json`, text compaction in Rust. PR diffs are collapsed into cacheable elements for progressive disclosure.
- **File I/O**: Path validation and traversal protection in Rust.

## License

MIT
