"""`graph_overview` tool registration.

The graph object passed in must expose `describe(types=None,
connections=False) -> str` — kglite's `KnowledgeGraph` has this method. The
`overview_prefix` string (if set) is prepended on every *bare* call (no
filters) so an operator can pin custom guidance the agent always sees.
"""

from __future__ import annotations


def register_overview(app, graph, *, overview_prefix: str | None = None) -> None:
    """Register a `graph_overview` tool on a FastMCP app.

    `graph` is any object exposing `describe(types, connections)`.
    """

    @app.tool(
        description=(
            "Inspect the knowledge graph. With no arguments: schema overview "
            "(node types, edge types, counts). Pass `types=['Function']` to "
            "drill into a specific type. `connections=true` shows edge "
            "details."
        )
    )
    def graph_overview(
        types: list[str] | None = None,
        connections: bool = False,
    ) -> str:
        body = graph.describe(types=types, connections=connections)
        if overview_prefix and not types and not connections:
            return f"{overview_prefix}\n\n{body}"
        return body
