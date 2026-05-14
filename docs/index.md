# mcp-methods

Shared Rust-powered utilities for [Model Context Protocol](https://modelcontextprotocol.io) servers. Pip-installable Python library, native Rust crate, and a generic CLI binary — three distribution shapes, one set of primitives.

## What it is

| Layer | What you get |
|---|---|
| **Primitives** | `ripgrep_files`, `list_dir`, `read_file`, `github_discussions`, `git_api`, `html_to_text`, `compact_text`, `ElementCache` — fast Rust implementations of the common building blocks an MCP tool server needs. |
| **Framework** | `McpServer`, `ServerOptions`, `Manifest`, `Workspace`, `watch_dir`, `load_env_walk` — an rmcp-backed boot sequence with YAML manifest, operating modes (bare / source-root / workspace / watch), advisory trust gates, and `.env` resolution. |
| **CLI** | `mcp-server` — generic binary that loads a YAML manifest and serves the MCP protocol over stdio. Ships in the pip wheel. |

## Pick a path

```{toctree}
:caption: Tutorials
:maxdepth: 2

getting-started
core-concepts
```

```{toctree}
:caption: How-to Guides
:maxdepth: 2

guides/writing-a-manifest
guides/operating-modes
guides/trust-gates
guides/downstream-binary
guides/python-bindings
guides/using-fastmcp-helpers
guides/watch-and-workspace
guides/skills-aware-manifests
guides/authoring-skills
```

```{toctree}
:caption: Explanation
:maxdepth: 2

explanation/architecture
explanation/trust-pattern
explanation/distribution-shape
explanation/three-layer-composition
```

```{toctree}
:caption: Reference
:maxdepth: 2

reference/manifest-schema
reference/python-api
reference/rust-api
```

```{toctree}
:caption: Examples
:maxdepth: 1

examples/minimal-manifest
examples/workspace-github
examples/workspace-local-watch
examples/tools-and-trust
```

```{toctree}
:caption: Project
:maxdepth: 1

contributing
changelog
```

## Install

```bash
pip install mcp-methods       # Python library + mcp-server CLI on PATH
cargo add mcp-methods         # Pure Rust library, zero pyo3
```

The `mcp-server` CLI bundled in the wheel is the same binary you'd get
from `cargo build --release -p mcp-server` in the repo — built once
per OS, packaged into the abi3 wheel, no Rust toolchain required for
operators.

## Where to start

- **New to MCP servers** → [Getting Started](getting-started.md) — 15-minute walkthrough from `pip install` to a running server.
- **Want to write your own server** → [Downstream Binary](guides/downstream-binary.md).
- **Just need a YAML manifest** → [Writing a Manifest](guides/writing-a-manifest.md).
- **Rust API reference** → [docs.rs/mcp-methods](https://docs.rs/mcp-methods).
