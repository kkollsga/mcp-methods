"""`save_graph` tool registration."""

from __future__ import annotations


def register_save_graph(app, graph) -> None:
    """Register a `save_graph(path)` tool on a FastMCP app.

    `graph.save(path)` must exist on the supplied object.
    """

    @app.tool(
        description=(
            "Persist the active knowledge graph to a file. `path` is the "
            "target location — the graph's own serialisation format is "
            "used (typically a single binary file with embeddings stored "
            "alongside)."
        )
    )
    def save_graph(path: str) -> str:
        graph.save(path)
        return f"Graph saved to {path}."
