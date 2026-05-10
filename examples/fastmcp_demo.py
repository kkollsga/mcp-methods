"""End-to-end example: compose a FastMCP server from mcp_methods helpers.

Run with::

    python examples/fastmcp_demo.py

…then connect a Claude Code MCP client over stdio. Pre-requisites:
the `mcp` Python SDK installed (`pip install mcp`) and a knowledge-graph
object exposing `describe()`, `cypher()`, and `save()`. The example uses
a tiny in-memory stub so it can run without external dependencies — swap
it for `kglite.KnowledgeGraph.load("…")` (or similar) in real use.
"""

from __future__ import annotations

import sys
from pathlib import Path

try:
    from mcp.server.fastmcp import FastMCP
except ImportError:
    print("This example requires `pip install mcp`. Bail.")
    sys.exit(1)

from mcp_methods.fastmcp import (
    register_cypher_query,
    register_overview,
    register_save_graph,
    register_source_tools,
    serve_csv_via_http,
)


class StubGraph:
    """Minimal graph stub. Replace with kglite.KnowledgeGraph in real use."""

    def describe(self, *, types=None, connections=False, limit=20):
        return f"stub-graph: types={types} connections={connections} limit={limit}"

    def cypher(self, query, format="text"):
        if format == "csv":
            return b"node,degree\nfoo,3\nbar,2\n"
        return f"stub-result for: {query}"

    def save(self, path):
        Path(path).write_text("stub graph payload")


def main() -> None:
    app = FastMCP("FastMCP Demo")
    graph = StubGraph()

    register_overview(app, graph, overview_prefix="Demo server — backed by a stub graph.")
    register_cypher_query(app, graph, csv_dir="temp/")
    register_save_graph(app, graph)

    # Source tools default to the project root for the demo. In real use,
    # point them at the data directory the agent should explore.
    register_source_tools(app, source_roots=[str(Path(__file__).parent.parent)])

    # Optional: serve `temp/` over HTTP for browser-side fetch().
    _server, base_url = serve_csv_via_http("temp/", port=0)
    print(f"CSV exports available at {base_url}/<filename>", file=sys.stderr)

    app.run(transport="stdio")


if __name__ == "__main__":
    main()
