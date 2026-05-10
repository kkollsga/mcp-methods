"""`cypher_query` tool registration with CSV-to-file transport.

The graph object must expose `cypher(query, format="text"|"csv") -> str|bytes`.
When `format="csv"` is requested the bytes are written to a uuid-named
file under `csv_dir` and the tool returns the file path; the agent can
then `read_source` it or fetch over HTTP if `serve_csv_via_http` is also
mounted.
"""

from __future__ import annotations

import os
import uuid
from pathlib import Path


def register_cypher_query(app, graph, *, csv_dir: str | os.PathLike[str] = "temp/") -> None:
    """Register a `cypher_query` tool on a FastMCP app."""
    csv_path = Path(csv_dir)
    csv_path.mkdir(parents=True, exist_ok=True)

    @app.tool(
        description=(
            "Run a Cypher query against the active knowledge graph. "
            "`format='text'` (default) returns a formatted table. "
            "`format='csv'` writes the result to a uuid-named file under "
            f"`{csv_dir}` and returns the path — use it for large result "
            "sets the agent should explore via read_source or HTTP."
        )
    )
    def cypher_query(query: str, format: str = "text") -> str:
        if format == "csv":
            try:
                payload = graph.cypher(query, format="csv")
            except TypeError:
                # Some graph impls take the FORMAT clause inline rather
                # than a kwarg. Try the alternative spelling.
                payload = graph.cypher(f"{query} FORMAT CSV")
            path = csv_path / f"{uuid.uuid4().hex}.csv"
            if isinstance(payload, (bytes, bytearray)):
                path.write_bytes(payload)
            else:
                path.write_text(str(payload))
            return f"CSV written: {path}"
        return graph.cypher(query)
