# Getting Started

A 15-minute walkthrough from `pip install` to a running MCP server with one custom tool.

## Step 1 — Install

```bash
pip install mcp-methods
```

This puts two things on PATH:

- The `mcp-methods` Python library (for the FastMCP helpers and primitives reachable from Python — `from mcp_methods import ripgrep, ElementCache, ...`).
- The `mcp-server` CLI binary, bundled in the wheel at `mcp_methods/_bin/mcp-server` and exposed as a console-script entry point.

Verify:

```bash
which mcp-server
mcp-server --help
```

You should see the help text with operating modes: `--source-root`, `--workspace`, `--watch`, and bare (no flag).

## Step 2 — Run in bare mode

```bash
mcp-server
```

This starts an MCP server over stdio with the framework's built-in tool surface (`ping`, plus any tools declared in a manifest if one is provided). Press Ctrl-C to stop.

To connect a client, write a config like the one Claude Code accepts:

```json
{
  "mcpServers": {
    "demo": {
      "command": "mcp-server"
    }
  }
}
```

## Step 3 — Add a source root

Source-root mode binds three tools (`read_source`, `grep`, `list_source`) against a fixed directory. Useful for letting an agent navigate a codebase.

```bash
mcp-server --source-root ~/my-project
```

The agent can now call `list_source` to see the tree, `grep("def \w+")` to find functions, `read_source("src/main.py")` to read files.

## Step 4 — Write a manifest

A YAML manifest gives you persistent configuration without long CLI invocations. Create `my_server.yaml`:

```yaml
name: My First MCP Server
instructions: |
  A source-navigator MCP server. Use the source tools to find and
  read code; ask follow-up questions about what you find.
source_roots:
  - ~/my-project
trust:
  allow_python_tools: false
  allow_embedder: false
builtins:
  save_graph: false
  temp_cleanup: never
```

Then:

```bash
mcp-server --mcp-config my_server.yaml
```

You can also drop the manifest at `<workspace-dir>/workspace_mcp.yaml` and it'll be auto-detected when you use `--workspace`.

## Step 5 — Pick what to do next

| Goal | Read |
|---|---|
| Understand the manifest's fields | [Writing a Manifest](guides/writing-a-manifest.md) |
| Use the operating modes (workspace / watch / local) | [Operating Modes](guides/operating-modes.md) |
| Layer custom tools on top of the framework | [Downstream Binary](guides/downstream-binary.md) |
| Embed the primitives in your own Python project | [Python Bindings](guides/python-bindings.md) |
| Understand the trust-gate model | [Trust Gates](guides/trust-gates.md) |

For the why behind the architecture (three crates, zero pyo3 in the library, bundled binary in the wheel), see [Architecture](explanation/architecture.md) and [Distribution Shape](explanation/distribution-shape.md).
