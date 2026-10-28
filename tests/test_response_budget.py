"""Protocol-level default budgets, including application-defined MCPServer tools."""

import json

import pytest

mcp = pytest.importorskip("mcp", reason="MCPServer protocol tests require the optional mcp SDK")
import anyio
from mcp.client import Client
from mcp.server.mcpserver import MCPServer
from mcp.server.mcpserver.exceptions import ToolError
from mcp_methods.fastmcp import install_response_budget, register_cypher_query
from pydantic import BaseModel


class CustomResult(BaseModel):
    rows: list[int]
    warning: str


def _preview(result):
    return json.loads(result.content[0].text)["response_budget"]


def test_default_budget_and_expansion_never_repeat_mutations(tmp_path):
    async def scenario():
        app = MCPServer("budget-test")
        calls = []
        data = json.dumps([{"id": i, "text": '界\\"' * 400} for i in range(120)])

        class Graph:
            def cypher(self, query):
                calls.append(query)
                return data

        register_cypher_query(app, Graph(), csv_dir=tmp_path)
        install_response_budget(app)  # Idempotent: helpers already installed it.

        @app.tool()
        def custom() -> CustomResult:
            return CustomResult(rows=list(range(6000)), warning="coverage unknown")

        async with Client(app, mode="legacy") as client:
            listing = await client.list_tools()
            tools = {tool.name: tool for tool in listing.tools}
            assert "_response" in tools["cypher_query"].input_schema["properties"]
            assert "expand_response" in tools
            invalid = await client.call_tool(
                "cypher_query", {"query": "mutate", "_response": {"max_bytes": 1}}
            )
            assert invalid.is_error and not calls
            result = await client.call_tool("cypher_query", {"query": "mutate"})
            assert len(result.model_dump_json(by_alias=True, exclude_none=True).encode()) <= 16384
            preview = _preview(result)
            assert preview["complete"] is False
            action = preview["next"]["full_result"]
            expanded = await client.call_tool(action["name"], action["arguments"])
            assert expanded.content[0].text == data
            assert calls == ["mutate"]
            async with Client(app, mode="legacy") as outsider:
                denied = await outsider.call_tool(action["name"], action["arguments"])
                assert denied.is_error
                assert calls == ["mutate"]
            custom_result = await client.call_tool("custom", {})
            assert _preview(custom_result)["complete"] is False
            full = await client.call_tool("custom", {"_response": {"mode": "full"}})
            assert full.structured_content["warning"] == "coverage unknown"
            assert len(full.structured_content["rows"]) == 6000
            again = await client.call_tool("custom", {})
            assert _preview(again)["max_bytes"] == 16384

    anyio.run(scenario)


def test_collision_controls_and_failed_tool_output():
    async def scenario():
        app = MCPServer("collisions")

        @app.tool()
        def expand_response() -> str:
            return "application tool"

        @app.tool()
        def fail() -> str:
            raise ToolError("operation failed\n" * 5000)

        install_response_budget(app)
        async with Client(app, mode="legacy") as client:
            listing = await client.list_tools()
            assert "expand_response_" in {t.name for t in listing.tools}
            existing = await client.call_tool("expand_response", {})
            assert existing.content[0].text == "application tool"
            result = await client.call_tool("fail", {})
            assert result.is_error
            action = _preview(result)["next"]["full_result"]
            assert action["name"] == "expand_response_"
            recovered = await client.call_tool(action["name"], action["arguments"])
            assert recovered.is_error
            assert "operation failed\n" * 5000 in recovered.content[0].text

    anyio.run(scenario)


def test_output_schema_keeps_object_type_at_the_root():
    async def scenario():
        app = MCPServer("schemas")

        @app.tool()
        def typed() -> CustomResult:
            return CustomResult(rows=[1], warning="w")

        install_response_budget(app)
        async with Client(app, mode="legacy") as client:
            listing = await client.list_tools()
            checked = 0
            for tool in listing.tools:
                if tool.output_schema is not None:
                    checked += 1
                    assert tool.output_schema["type"] == "object"
                    assert "anyOf" in tool.output_schema, "budget preview branch advertised"
            assert checked >= 1

    anyio.run(scenario)


def test_modern_stdio_client_previews_and_expands_across_calls():
    """A 2026-07-28 client has no handshake; its session is its stdio connection."""
    import sys
    from pathlib import Path

    from mcp.client.stdio import StdioServerParameters

    server = Path(__file__).parent / "_budget_stdio_server.py"

    async def scenario():
        params = StdioServerParameters(command=sys.executable, args=[str(server)])
        async with Client(params, mode="auto") as client:
            result = await client.call_tool("big", {})
            preview = _preview(result)
            assert preview["complete"] is False
            action = preview["next"]["full_result"]
            expanded = await client.call_tool(action["name"], action["arguments"])
            assert not expanded.is_error
            assert expanded.content[0].text.startswith('[{"id": 0')

    anyio.run(scenario)
