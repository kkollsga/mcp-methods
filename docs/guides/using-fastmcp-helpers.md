# Using FastMCP Helpers

`mcp_methods.fastmcp` provides drop-in tool registrations for [FastMCP](https://github.com/modelcontextprotocol/python-sdk) servers — Python MCP servers built on the official `mcp` SDK. Use these helpers when you're writing your MCP server in Python (instead of Rust) but still want the same tool surface the bundled `mcp-server` binary ships.

## Install

```bash
pip install "mcp-methods[fastmcp]"
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

The helpers delegate source operations to Rust and graph operations to the supplied
graph object. Registering a tool helper also installs the shared Rust response
budget at the app's MCP dispatch boundary, covering its custom tools too.

## Default response budgets

Completed tool results default to **16,384 serialized result bytes**. Small
results retain their original shape. Larger results carry a preview describing
scope, known counts, omitted values, selection order, and executable follow-up
calls. Errors keep `isError: true`. This changes the default for existing clients.

For one call, add `_response` to the normal arguments:

```json
{"query": "MATCH (n) RETURN n", "_response": {"max_bytes": 32768}}
```

Or request the complete inline result:

```json
{"query": "MATCH (n) RETURN n", "_response": {"mode": "full"}}
```

`max_bytes` must be at least 4096 and cannot be combined with `mode: full`.
Controls are removed before invoking the tool. If a tool already declares an
`_response` argument, the framework appends underscores until the control name
is unused; consult `tools/list` for that tool's actual name.

A preview's `next` object contains calls to `expand_response`. Copy those calls
to retrieve a page or the original complete result **without rerunning the
operation**. Its `path` selects a JSON Pointer into the payload: structured
content, parsed JSON from a single text block, single text, or the original
content-block array, in that order. `offset` counts array items, ordered object
fields, or Unicode characters. Paths on shortened nested values support focused
inspection. An existing application tool named `expand_response` is preserved;
the framework adds underscores to its expansion tool name.

Array previews preserve result order. Object previews put status, summary,
warning/error, diagnostics and coverage fields first, then sort lexically.
Text previews include a beginning and tail excerpt. Counts describe the
returned collection, not the underlying database. Query `LIMIT`, source-tool
limits and existing graph previews still apply even in full mode. The framework
cannot infer missing semantic groups or repair query selection.

Results are retained in memory per app, accessible only to the originating MCP
session. Storage holds at most 32 entries and 32 MiB of serialized results,
normalized payloads and arguments; oldest entries are evicted first. Expiry is
ten minutes, checked on access. Storage is released when the server is dropped.
Expired/evicted handles fail explicitly and never cause automatic re-execution.
If a result cannot fit the retention capacity, or even preview control metadata
cannot fit the requested budget, the result is returned intact with an explicit
budget-overage reason. This avoids losing evidence after a mutation. No files
are created. Byte limits concern completed MCP result objects, including content,
metadata and JSON escaping, not JSON-RPC framing or token counts. Transport
limits, protocol control messages and asynchronous task delivery remain owned
by the host protocol implementation.

The preview uses text for its evidence and a small
`structuredContent: {"mcp_methods_preview": true}` discriminator. Advertised
output schemas allow that alternative. Full results preserve their original
structured content and content blocks.

The supported Python host is the official `mcp.server.fastmcp.FastMCP` from
`mcp>=1.26,<2`. If using only application-defined tools, call
`install_response_budget(app)` once before serving. Helpers install it
automatically; ordinary Python calls, prompts, resource responses, and independent
downstream CLIs are outside this adapter's tool-response boundary. Install after
any code that replaces the host's low-level request handlers.

For domain-aware guidance, a custom tool can return a `CallToolResult` carrying
`_meta["mcp_methods/preview"]` with a summary, coverage, warnings and concrete
follow-up queries. This guidance is itself bounded and labeled if shortened.
Tools own domain relevance; the generic fallback only describes structure.


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
