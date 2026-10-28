"""Default response budgets at the official MCP SDK's final protocol boundary.

Registration helpers install this once, as server middleware on an
``mcp.server.mcpserver.MCPServer`` app (mcp 2.x). It also covers
application-defined tools on that app, including ones added after
installation. Direct Python function calls and non-tool protocol methods
retain their existing behavior.
"""

from __future__ import annotations

import copy
import dataclasses
import json
import uuid
import weakref

from mcp_methods._mcp_methods import ResponseBudget


def _control_name(schema):
    name = "_response"
    while name in schema.get("properties", {}):
        name += "_"
    return name


def _dump(value) -> dict:
    """A handler result (model or dict) as a plain camelCase dict."""
    if isinstance(value, dict):
        return value
    return value.model_dump(mode="json", by_alias=True, exclude_none=True)


def _complete(ctx, result: dict) -> dict:
    """Mark a result we built ourselves ``complete`` on 2026-07-28 and later,
    where every result must say so; earlier protocol versions have no such field."""
    if ctx.protocol_version >= "2026-07-28":
        result.setdefault("resultType", "complete")
    return result


def _session_key(ctx):
    """Who retained results belong to.

    A handshake connection and a stdio connection carry one stable object for
    the life of the client: the connection itself, or its standalone channel.
    A request that arrives on its own (stateless HTTP) has neither, so each
    request is its own session and an expansion handle never resolves across
    calls there, exactly as with the Rust ``mcp-server`` over stateless HTTP.
    """
    connection = getattr(ctx.session, "_connection", None)
    if connection is None:
        return ctx.session
    return connection.outbound if connection.has_standalone_channel else connection


def install_response_budget(app) -> None:
    """Install default budgets on an ``mcp.server.mcpserver.MCPServer`` app.

    All tool responses use the shared Rust engine; per-call controls and the
    expansion tool are advertised by tools/list. Repeated installation is safe.
    Lightweight registration-only app doubles have no protocol boundary to wrap.
    """
    chain = getattr(app, "middleware", None)
    if not isinstance(chain, list):
        return
    if getattr(app, "_mcp_methods_response_budget", None) is not None:
        return

    store = ResponseBudget()
    sessions = weakref.WeakKeyDictionary()

    def expansion_name(names):
        name = "expand_response"
        while name in names:
            name += "_"
        return name

    def advertise(tool):
        control = _control_name(tool["inputSchema"])
        tool["inputSchema"].setdefault("properties", {})[control] = json.loads(
            store.options_schema()
        )
        tool["description"] = (tool.get("description") or "") + (
            f"\nResponses default to 16384 serialized bytes, except results carrying "
            f"image, audio or resource content, which are returned whole. "
            f"Set {control}.mode=full "
            f"or {control}.max_bytes for a larger per-call budget. Previews include "
            "calls to expand retained evidence without rerunning this tool."
        )
        original = tool.get("outputSchema")
        if original is not None:
            # MCP requires `type: object` at the outputSchema root;
            # `$defs`/`$schema` are hoisted to the root with it.
            wrapped = {
                "type": "object",
                "anyOf": [
                    original,
                    {
                        "type": "object",
                        "required": ["mcp_methods_preview"],
                        "properties": {"mcp_methods_preview": {"const": True}},
                    },
                ],
            }
            if "$defs" in original:
                wrapped["$defs"] = original.pop("$defs")
            if "$schema" in original:
                wrapped["$schema"] = original.pop("$schema")
            tool["outputSchema"] = wrapped

    async def list_tools(ctx, call_next):
        result = copy.deepcopy(_dump(await call_next(ctx)))
        tools = result.get("tools", [])
        name = expansion_name({t["name"] for t in tools})
        for tool in tools:
            advertise(tool)
        tools.append(
            {
                "name": name,
                "description": (
                    "Inspect retained output without rerunning the tool. Use result_id from "
                    "a preview, a payload JSON Pointer path, and an item/field/character offset. "
                    "response.mode=full returns the original inline result or selected value. "
                    "Results are scoped to this session and expire/evict as disclosed."
                ),
                "inputSchema": json.loads(store.expansion_schema()),
                "annotations": {"readOnlyHint": True, "idempotentHint": True},
            }
        )
        result["tools"] = tools
        return result

    def failure(message):
        return {"content": [{"type": "text", "text": message}], "isError": True}

    async def call_tool(ctx, call_next):
        definitions = {t.name: t.input_schema for t in await app.list_tools()}
        expansion = expansion_name(definitions)
        key = _session_key(ctx)
        session_id = sessions.setdefault(key, uuid.uuid4().hex)
        params = dict(ctx.params or {})
        name = params.get("name")
        arguments = dict(params.get("arguments") or {})
        try:
            if name == expansion:
                return _complete(
                    ctx, json.loads(store.expand(session_id, json.dumps(arguments), expansion))
                )
            if name not in definitions:
                return await call_next(ctx)
            options = arguments.pop(_control_name(definitions[name]), {})
            store.validate(json.dumps(options))
        except ValueError as exc:
            return _complete(ctx, failure(str(exc)))
        forwarded = dataclasses.replace(ctx, params={**params, "arguments": arguments})
        result = _dump(await call_next(forwarded))
        bounded = store.present(
            session_id,
            name,
            json.dumps(arguments),
            json.dumps(result),
            json.dumps(options),
            expansion,
        )
        return _complete(ctx, json.loads(bounded))

    async def budget(ctx, call_next):
        if ctx.method == "tools/list":
            return await list_tools(ctx, call_next)
        if ctx.method == "tools/call":
            return await call_tool(ctx, call_next)
        return await call_next(ctx)

    chain.append(budget)
    app._mcp_methods_response_budget = store
