# Using FastMCP Helpers

`mcp_methods.fastmcp` provides drop-in tool registrations for [FastMCP](https://github.com/modelcontextprotocol/python-sdk) servers — Python MCP servers built on the official `mcp` SDK. Use these helpers when you're writing your MCP server in Python (instead of Rust) but still want the same tool surface the bundled `mcp-server` binary ships.

## Install

```bash
pip install mcp-methods mcp
```

## The helpers

```python
from mcp.server.fastmcp import FastMCP
from mcp_methods.fastmcp import (
    register_overview,  # graph_overview tool
    register_cypher_query,  # cypher_query tool with CSV export
    register_source_tools,  # read_source, grep, list_source
    register_save_graph,  # save_graph tool
    serve_csv_via_http,  # CORS-enabled HTTP server for CSV exports
)

app = FastMCP("My Server")

register_overview(app, graph, overview_prefix="My custom guidance")
register_cypher_query(app, graph, csv_dir="temp/")
register_source_tools(app, source_roots=["./source"])
register_save_graph(app, graph)
_server, base_url = serve_csv_via_http("temp/")  # optional

app.run(transport="stdio")
```

Each helper is a thin (~10-line) wrapper over the Rust PyO3 surface — there's no logic duplication between the YAML-driven binary and these helpers, so agent behaviour is identical regardless of which path booted the server.

## The `graph` parameter

`graph` is any object exposing `describe()` / `cypher()` / `save()`. [kglite's](https://github.com/kkollsga/kglite) `KnowledgeGraph` satisfies this duck-type interface. Other graph backends that adopt the same three methods work too.

## End-to-end example

A runnable stub lives at [`python_tests/fastmcp_demo.py`](https://github.com/kkollsga/mcp-methods/blob/main/python_tests/fastmcp_demo.py) in the repo. It boots a FastMCP server with all five helpers wired up and a kglite-style graph backend.

## When to use FastMCP vs the Rust binary

| You want | Use |
|---|---|
| A pure-Python MCP server (no Rust toolchain) | FastMCP + these helpers |
| A YAML-driven server with all the operating modes | `mcp-server` CLI |
| Custom tool logic in Rust | A downstream binary — see [Downstream Binary](downstream-binary.md) |
| Custom tool logic in Python with graph backend | FastMCP + your own graph wrapper |

## See also

- [Python Bindings](python-bindings.md) — the broader Python surface
- [Operating Modes](operating-modes.md) — what the Rust binary supports that FastMCP doesn't
