# Core Concepts

`mcp-methods` is the framework half of an MCP server. The mental model is:

> **You write a YAML manifest. The framework boots a server, exposes a configurable set of tools, and serves the MCP protocol over stdio.** Domain-specific tools (your Cypher queries, your database operations, your custom logic) are added by you — either inline in the manifest (for the simple case) or in a downstream Rust binary that wraps `McpServer::new` (for the rich case).

## The four building blocks

### 1. The Manifest

A YAML file (`workspace_mcp.yaml` by convention, or `--mcp-config path/to/file.yaml`) declares:

- The **server's identity** (`name:`, `instructions:`, `overview_prefix:`)
- The **operating mode** (`source_roots:`, `workspace: { kind, root, watch }`)
- The **trust gates** (`trust: { allow_python_tools, allow_embedder, allow_query_preprocessor }`)
- The **builtin behaviour** (`builtins: { save_graph, temp_cleanup }`)
- Optional **manifest-declared tools** (`tools: [{ kind: cypher | python, ... }]`)
- An **opaque passthrough** (`extensions: { ... }`) for downstream-binary-specific config

The framework parses + validates the schema (strict unknown-key checking) and exposes the result as `Manifest` in Rust or `manifest.to_dict()` from a pyo3 wrapper. See [Writing a Manifest](guides/writing-a-manifest.md) for the field-by-field walkthrough.

### 2. Operating modes

The framework's modes determine how source navigation and watch hooks behave:

| Mode | When |
|---|---|
| **bare** | No `--source-root` / `--workspace` / `--watch` flag. Framework + manifest tools only. Useful for testing the protocol layer or building stateless tool-only servers. |
| **`--source-root DIR`** | Bind the source tools (`read_source`, `grep`, `list_source`) to a fixed directory. |
| **`--workspace DIR`** (GitHub) | Clone-and-track GitHub repos under DIR. `repo_management` tool handles activate/update/delete. |
| **`workspace.kind: local`** (manifest) | Bind a fixed local directory + optional file watcher. `set_root_dir(path)` swaps the active root at runtime. |
| **`--watch DIR`** | Watch DIR for filesystem changes. Downstream binaries register a rebuild callback. |

The manifest's `workspace:` block wins over CLI flags (same precedence as `source_root:`). See [Operating Modes](guides/operating-modes.md).

### 3. Trust gates (advisory metadata)

Three boolean flags under `trust:` declare what dynamic-code hooks the manifest permits:

| Flag | Gates |
|---|---|
| `allow_python_tools` | `tools[].python:` factories |
| `allow_embedder` | `extensions.embedder` loaders in downstream binaries |
| `allow_query_preprocessor` | `extensions.cypher_preprocessor` hooks |

**The framework records these flags but doesn't enforce them.** Enforcement lives in downstream binaries (the consumer pattern). The flag answers the question "the operator approved this hook," and the consumer is responsible for refusing to boot the hook when the answer is false. See [Trust Gates](guides/trust-gates.md) and the [Trust Pattern](explanation/trust-pattern.md) explanation for why it works this way.

### 4. The downstream-binary pattern

For anything beyond source navigation + GitHub access, you build a small Rust binary that depends on `mcp-methods`. The pattern is ~50-500 LOC:

```rust
use mcp_methods::server::{McpServer, ServerOptions};

let manifest = mcp_methods::server::manifest::load(&yaml_path)?;
let mut options = ServerOptions::from_manifest(Some(&manifest), "My Server");
let mut server = McpServer::new(options);

server.register_typed_tool::<MyArgs, _>(
    "my_tool",
    "Description seen by the agent",
    move |args: MyArgs| -> String { /* your logic */ },
);

server.serve(rmcp::transport::stdio()).await?.waiting().await?;
```

[`kglite-mcp-server`](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server) is the canonical worked example — ~500 LOC layering Cypher queries, graph overview, save, and code-source tools on top of this foundation. The minimal version lives at [`examples/downstream_binary/`](https://github.com/kkollsga/mcp-methods/tree/main/examples/downstream_binary) (~60 LOC) and is the starting point most consumers should copy. See [Downstream Binary](guides/downstream-binary.md).

## What the framework owns vs what you own

| The framework owns | You own |
|---|---|
| YAML parsing + strict validation | Manifest authoring |
| `McpServer` boot sequence | Custom tool implementations |
| Source navigation tools (`read_source`, `grep`, `list_source`) | Domain-specific tools (Cypher, etc.) |
| GitHub tools (`github_issues`, `git_api`) | Trust enforcement (gating extension hooks) |
| Workspace clone/local-bind + atomic root swap | Downstream binary's CLI surface |
| Trust-gate parsing | What "trust" means for each extension |
| `.env` resolution | Application secrets |
| `Manifest::to_json()` for FFI bridging | The pyo3 / JSON-RPC wrapper if you need one |

This separation keeps `mcp-methods` domain-agnostic (no graph types, no SQL types, no Slack types) while giving downstream consumers a stable contract to build against.

## Read next

- [Architecture](explanation/architecture.md) — why three crates, why advisory trust
- [Distribution Shape](explanation/distribution-shape.md) — the polars / pydantic-core pattern
- [Writing a Manifest](guides/writing-a-manifest.md) — field-by-field reference
