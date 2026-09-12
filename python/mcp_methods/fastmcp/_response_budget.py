"""Default response budgets at the official MCP SDK's final protocol boundary.

Registration helpers install this once. It also covers application-defined tools
on that app, including ones added after installation. Direct Python function
calls and non-tool protocol methods retain their existing behavior.
"""

from __future__ import annotations

import copy
import json
import uuid
import weakref

from mcp_methods._mcp_methods import ResponseBudget


def _control_name(schema):
    name = "_response"
    while name in schema.get("properties", {}):
        name += "_"
    return name


def install_response_budget(app) -> None:
    """Install default budgets on an official ``mcp.server.fastmcp.FastMCP`` app.

    All tool responses use the shared Rust engine; per-call controls and the
    expansion tool are advertised by tools/list. Repeated installation is safe.
    Lightweight registration-only app doubles have no protocol boundary to wrap.
    """
    server = getattr(app, "_mcp_server", None)
    if server is None:
        return
    if getattr(server, "_mcp_methods_response_budget", None) is not None:
        return

    from mcp import types

    store = ResponseBudget()
    sessions = weakref.WeakKeyDictionary()
    call_original = server.request_handlers[types.CallToolRequest]
    list_original = server.request_handlers[types.ListToolsRequest]

    async def definitions():
        return await app.list_tools()

    def expansion_name(tools):
        names = {t.name for t in tools}
        name = "expand_response"
        while name in names:
            name += "_"
        return name

    async def list_tools(request):
        response = await list_original(request)
        tools = copy.deepcopy(response.root.tools)
        name = expansion_name(tools)
        for tool in tools:
            control = _control_name(tool.inputSchema)
            tool.inputSchema.setdefault("properties", {})[control] = json.loads(
                store.options_schema()
            )
            tool.description = (tool.description or "") + (
                f"\nResponses default to 16384 serialized bytes. Set {control}.mode=full "
                f"or {control}.max_bytes for a larger per-call budget. Previews include "
                "calls to expand retained evidence without rerunning this tool."
            )
            if tool.outputSchema is not None:
                original = tool.outputSchema
                tool.outputSchema = {
                    "anyOf": [
                        original,
                        {
                            "type": "object",
                            "required": ["mcp_methods_preview"],
                            "properties": {"mcp_methods_preview": {"const": True}},
                        },
                    ]
                }
                if "$defs" in original:
                    tool.outputSchema["$defs"] = original.pop("$defs")
        tools.append(
            types.Tool(
                name=name,
                description=(
                    "Inspect retained output without rerunning the tool. Use result_id from "
                    "a preview, a payload JSON Pointer path, and an item/field/character offset. "
                    "response.mode=full returns the original inline result or selected value. "
                    "Results are scoped to this session and expire/evict as disclosed."
                ),
                inputSchema=json.loads(store.expansion_schema()),
                annotations=types.ToolAnnotations(readOnlyHint=True, idempotentHint=True),
            )
        )
        return types.ServerResult(response.root.model_copy(update={"tools": tools}))

    async def call_tool(request):
        tools = await definitions()
        expansion = expansion_name(tools)
        session = server.request_context.session
        session_id = sessions.setdefault(session, uuid.uuid4().hex)
        arguments = dict(request.params.arguments or {})
        try:
            if request.params.name == expansion:
                result = store.expand(session_id, json.dumps(arguments), expansion)
                return types.ServerResult(types.CallToolResult.model_validate_json(result))
            tool = next((t for t in tools if t.name == request.params.name), None)
            if tool is None:
                return await call_original(request)
            options = arguments.pop(_control_name(tool.inputSchema), {})
            store.validate(json.dumps(options))
        except ValueError as exc:
            return types.ServerResult(
                types.CallToolResult(
                    content=[types.TextContent(type="text", text=str(exc))], isError=True
                )
            )
        params = request.params.model_copy(update={"arguments": arguments})
        result = await call_original(request.model_copy(update={"params": params}))
        if not isinstance(result.root, types.CallToolResult):
            return result
        bounded = store.present(
            session_id,
            request.params.name,
            json.dumps(arguments),
            result.root.model_dump_json(by_alias=True, exclude_none=True),
            json.dumps(options),
            expansion,
        )
        return types.ServerResult(types.CallToolResult.model_validate_json(bounded))

    server.request_handlers[types.ListToolsRequest] = list_tools
    server.request_handlers[types.CallToolRequest] = call_tool
    server._mcp_methods_response_budget = store
