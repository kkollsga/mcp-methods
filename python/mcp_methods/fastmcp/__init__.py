"""Composable FastMCP tool helpers.

Built for users running a `FastMCP` server who want to register the same
tools the YAML-driven `mcp-server` binary ships, without rewriting them.
Each helper takes a FastMCP `app` and the dependency it needs (a graph,
a list of source roots, etc.) and registers one or more tools on the app.

Typical use::

    from mcp.server.fastmcp import FastMCP
    from mcp_methods.fastmcp import (
        register_overview, register_cypher_query, register_source_tools,
        register_save_graph, serve_csv_via_http,
    )

    app = FastMCP("My Server")
    register_overview(app, graph, overview_prefix="...")
    register_cypher_query(app, graph, csv_dir="temp/")
    register_source_tools(app, source_roots=["./source"])
    register_save_graph(app, graph)
    app.run(transport="stdio")

All helpers are thin wrappers — the implementation work lives in the Rust
`_mcp_methods` cdylib (for source tools) or in the user-supplied `graph`
object (for graph tools). The helpers exist so each FastMCP author does
not re-implement parameter validation, default values, and tool-description
strings; they mirror the corresponding YAML+CLI tool one-to-one so an
agent's behaviour is identical regardless of which path booted the server.
"""

from __future__ import annotations

from ._csv_http import serve_csv_via_http
from ._cypher import register_cypher_query
from ._overview import register_overview
from ._save import register_save_graph
from ._skills import register_skills_as_prompts
from ._source import register_source_tools

__all__ = [
    "register_cypher_query",
    "register_overview",
    "register_save_graph",
    "register_skills_as_prompts",
    "register_source_tools",
    "serve_csv_via_http",
]
