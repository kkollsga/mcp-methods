"""Tests for the mcp_methods.fastmcp helper submodule.

We don't boot a real FastMCP server (that would pull in the `mcp` SDK
plus async machinery just to assert tool registration). Instead each
test uses a tiny `_FakeApp` that captures `@app.tool` invocations so we
can introspect what was registered, then invokes the registered functions
directly to verify behaviour.
"""

from __future__ import annotations

import os
import tempfile
import urllib.request
from pathlib import Path

from mcp_methods.fastmcp import (
    register_cypher_query,
    register_overview,
    register_save_graph,
    register_source_tools,
    serve_csv_via_http,
)


class _FakeApp:
    """Captures FastMCP-style tool registrations.

    Each `@app.tool(description=...)` decorator stores the function in
    `self.tools[name]` and returns the function unchanged.
    """

    def __init__(self) -> None:
        self.tools: dict[str, callable] = {}
        self.descriptions: dict[str, str] = {}

    def tool(self, *, description: str | None = None):
        def decorator(fn):
            self.tools[fn.__name__] = fn
            self.descriptions[fn.__name__] = description or ""
            return fn

        return decorator


def test_register_source_tools_registers_three():
    with tempfile.TemporaryDirectory() as tmpdir:
        Path(tmpdir, "hello.txt").write_text("hi\n")
        app = _FakeApp()
        register_source_tools(app, source_roots=[tmpdir])
        assert set(app.tools) == {"read_source", "grep", "list_source"}
        # Descriptions are user-facing — make sure they're non-empty.
        for desc in app.descriptions.values():
            assert desc.strip()


def test_register_source_tools_empty_roots_rejected():
    app = _FakeApp()
    try:
        register_source_tools(app, source_roots=[])
    except ValueError as e:
        assert "at least one" in str(e)
    else:
        raise AssertionError("expected ValueError")


def test_register_source_tools_nonexistent_root_rejected():
    app = _FakeApp()
    try:
        register_source_tools(app, source_roots=["/nope/this/does/not/exist"])
    except ValueError as e:
        assert "not a directory" in str(e)
    else:
        raise AssertionError("expected ValueError")


def test_read_source_rejects_path_traversal():
    with tempfile.TemporaryDirectory() as tmpdir:
        Path(tmpdir, "x.txt").write_text("hi\n")
        app = _FakeApp()
        register_source_tools(app, source_roots=[tmpdir])
        # Escaping the root → friendly error, not an exception
        result = app.tools["read_source"](file_path="../../../etc/passwd")
        assert "outside the configured source roots" in result


def test_grep_returns_matches():
    with tempfile.TemporaryDirectory() as tmpdir:
        Path(tmpdir, "a.py").write_text("def foo():\n    pass\n")
        Path(tmpdir, "b.py").write_text("def bar():\n    return 1\n")
        app = _FakeApp()
        register_source_tools(app, source_roots=[tmpdir])
        body = app.tools["grep"](pattern="def ")
        assert "foo" in body and "bar" in body


def test_list_source_lists_entries():
    with tempfile.TemporaryDirectory() as tmpdir:
        Path(tmpdir, "x.txt").write_text("hi\n")
        Path(tmpdir, "y.txt").write_text("ya\n")
        os.mkdir(Path(tmpdir, "sub"))
        app = _FakeApp()
        register_source_tools(app, source_roots=[tmpdir])
        body = app.tools["list_source"](path=".")
        assert "x.txt" in body and "y.txt" in body and "sub" in body


def test_register_overview_uses_prefix():
    class FakeGraph:
        # Mirrors kglite 0.10's describe() signature — no `limit` kwarg.
        # If the framework regresses to forwarding `limit`, this raises.
        def describe(self, *, types=None, connections=False):
            return f"schema(types={types}, connections={connections})"

    app = _FakeApp()
    register_overview(app, FakeGraph(), overview_prefix="HELLO")
    body = app.tools["graph_overview"]()  # bare call → prefix prepended
    assert body.startswith("HELLO\n\n")
    # With filters, prefix is dropped (it's only for the bare-summary case).
    body2 = app.tools["graph_overview"](types=["Function"])
    assert not body2.startswith("HELLO")


def test_register_overview_no_prefix():
    class FakeGraph:
        def describe(self, *, types=None, connections=False):
            return "schema-body"

    app = _FakeApp()
    register_overview(app, FakeGraph())
    assert app.tools["graph_overview"]() == "schema-body"


def test_register_cypher_query_text_mode():
    class FakeGraph:
        def cypher(self, query, format="text"):
            return f"format={format} q={query}"

    with tempfile.TemporaryDirectory() as tmpdir:
        app = _FakeApp()
        register_cypher_query(app, FakeGraph(), csv_dir=tmpdir)
        body = app.tools["cypher_query"]("MATCH (n) RETURN n")
        assert "format=text" in body


def test_register_cypher_query_text_mode_coerces_non_str():
    # kglite's cypher() returns a lazy ResultView (not a str) whose
    # __str__ renders the table. The tool is typed `-> str`, so it must
    # coerce; without it, FastMCP output validation rejects the object.
    class ResultViewLike:
        def __str__(self):
            return "rendered-table"

    class FakeGraph:
        def cypher(self, query, format="text"):
            return ResultViewLike()

    with tempfile.TemporaryDirectory() as tmpdir:
        app = _FakeApp()
        register_cypher_query(app, FakeGraph(), csv_dir=tmpdir)
        body = app.tools["cypher_query"]("MATCH (n) RETURN n")
        assert isinstance(body, str)
        assert body == "rendered-table"


def test_register_cypher_query_csv_mode_writes_file():
    class FakeGraph:
        def cypher(self, query, format="text"):
            assert format == "csv"
            return b"name,age\nalice,30\n"

    with tempfile.TemporaryDirectory() as tmpdir:
        app = _FakeApp()
        register_cypher_query(app, FakeGraph(), csv_dir=tmpdir)
        body = app.tools["cypher_query"]("MATCH (n) RETURN n", format="csv")
        assert body.startswith("CSV written: ")
        path = Path(body[len("CSV written: ") :].strip())
        assert path.read_bytes() == b"name,age\nalice,30\n"


def test_register_save_graph_calls_save():
    class FakeGraph:
        def __init__(self) -> None:
            self.saved_to: str | None = None

        def save(self, path: str) -> None:
            self.saved_to = path

    graph = FakeGraph()
    app = _FakeApp()
    register_save_graph(app, graph)
    body = app.tools["save_graph"]("/tmp/x.kgl")
    assert graph.saved_to == "/tmp/x.kgl"
    assert "saved" in body.lower()


def test_serve_csv_via_http_actually_serves():
    """Start the server, drop a file, fetch it, shut down. CORS header
    must be present (this is the load-bearing claim for browser use)."""
    with tempfile.TemporaryDirectory() as tmpdir:
        Path(tmpdir, "out.csv").write_text("a,b\n1,2\n")
        server, base_url = serve_csv_via_http(tmpdir, port=0)
        try:
            with urllib.request.urlopen(f"{base_url}/out.csv") as resp:
                body = resp.read().decode()
                cors = resp.headers.get("Access-Control-Allow-Origin")
            assert body == "a,b\n1,2\n"
            assert cors == "*"
        finally:
            server.shutdown()
