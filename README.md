# mcp-methods

Shared Rust-powered utilities for [Model Context Protocol](https://modelcontextprotocol.io) servers. Pip-installable Python library AND a native Rust crate AND a generic CLI binary — three distribution shapes, one set of primitives.

Fast file search (ripgrep-backed), GitHub integration with smart compaction, text utilities, and an rmcp-backed MCP server framework with YAML-manifest configuration. The common building blocks needed when writing MCP tool servers.

The Rust library is the source of truth; the Python wheel is a thin pyo3 binding over it. Rust consumers see zero pyo3 in their dep tree.

## Install

### Python — `pip install`

```bash
pip install mcp-methods
```

```python
from mcp_methods import ElementCache, ripgrep, list_dir, github_discussions, read_file
```

The wheel ships the `mcp-server` CLI on PATH alongside the library — `which mcp-server` should resolve immediately. Single abi3 wheel per OS; works on Python 3.10 through 3.13 without reinstall.

### Rust library — `cargo add`

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

For pre-release coordination — pinning against a specific commit while the framework is iterating quickly — depend on a git rev:

```toml
mcp-methods = { git = "https://github.com/kkollsga/mcp-methods.git", rev = "<short SHA>", features = ["server"] }
```

The downstream [`kglite-mcp-server`](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server) uses this pattern to stay locked to the exact framework rev its integration tests pass against.

### CLI — `cargo install` (alternative path)

The CLI ships in the pip wheel by default. If you'd rather skip Python:

```bash
cargo install --git https://github.com/kkollsga/mcp-methods mcp-server
```

Not on crates.io — `mcp-server` is unpublished on purpose. The Rust library on crates.io is the published surface; the binary ships via the wheel and via git.

## Documentation

**Full docs at [mcp-methods.readthedocs.io](https://mcp-methods.readthedocs.io)** (Sphinx + Read the Docs, structured along Diátaxis lines):

- **[Getting Started](https://mcp-methods.readthedocs.io/en/latest/getting-started.html)** — 15-minute walkthrough from pip install to a running server
- **[Core Concepts](https://mcp-methods.readthedocs.io/en/latest/core-concepts.html)** — manifest, operating modes, trust gates, downstream-binary pattern
- **[Guides](https://mcp-methods.readthedocs.io/en/latest/index.html#how-to-guides)** — writing a manifest, downstream binaries, trust gates, python bindings, watch + workspace modes
- **[Reference](https://mcp-methods.readthedocs.io/en/latest/index.html#reference)** — manifest schema, Python API, Rust API
- **[Explanation](https://mcp-methods.readthedocs.io/en/latest/index.html#explanation)** — three-crate architecture, advisory-trust pattern, distribution shape

Rust API rustdoc: **[docs.rs/mcp-methods](https://docs.rs/mcp-methods)**.

## What's included

| Primitive | Purpose |
|---|---|
| `list_dir` | Tree-formatted directory listing with depth, glob, `.gitignore`, annotation |
| `ripgrep_files`, `ripgrep`, `ripgrep_lines` | Ripgrep-powered search (in-process, no subprocess) |
| `read_file` | Safe file reading with path traversal protection |
| `github_discussions`, `git_api` | GitHub issue/PR fetching with smart compaction, REST wrapper |
| `ElementCache` | Drill-down cache for collapsed elements in GitHub discussions |
| `html_to_text`, `compact_text`, `collapse_code_blocks` | Text utilities |
| `extract_github_refs`, `detect_git_repo`, `validate_repo` | Git/GitHub helpers |
| `mcp_methods.fastmcp` | Composable tool registrations for FastMCP servers |
| `SkillRegistry`, `Skill`, `register_skills_as_prompts` | Operator-authored methodology as MCP prompts (three-layer: project → domain pack → bundled defaults). See `crates/mcp-methods/dev-documentation/skills-aware-mcp.md` for the design doc. |

Plus the **server framework** in `mcp_methods::server::*`: `McpServer`, `ServerOptions`, `Manifest`, `Workspace`, `watch_dir`, `load_env_walk`, `serve_prompts`. See the [Rust API reference](https://mcp-methods.readthedocs.io/en/latest/reference/rust-api.html).

## License

MIT.
