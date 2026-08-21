"""Skill-prompt registration helper for FastMCP servers.

Mirrors the Rust `serve_prompts` flow: takes a resolved
`SkillRegistry` and registers each skill as a FastMCP prompt. The
prompt name is the skill's frontmatter name; the prompt body is the
skill's markdown body (frontmatter stripped).

Typical use::

    from mcp.server.fastmcp import FastMCP
    from mcp_methods import SkillRegistry
    from mcp_methods.fastmcp import register_skills_as_prompts

    app = FastMCP("My Server")
    registry = SkillRegistry.from_manifest("./my_mcp.yaml")
    register_skills_as_prompts(app, registry)
    app.run(transport="stdio")

Auto-injection of `prompts/get` pointers into tool descriptions is
*not* mirrored here — FastMCP's tool registration is decorator-driven
and tool descriptions are fixed at decoration time, so the hint would
need to be applied at decorator scope. Operators who want the hint
can compose tool descriptions explicitly. The Rust-side
`serve_prompts` is the canonical path when this matters.

`applies_when:` predicate gating is *not* mirrored here either: every
skill in the registry registers unconditionally, including bundled
skills that gate on `tool_registered:` (`github_issues`,
`repo_management`) — this helper has no view of which tools the
FastMCP app registered. A deployment that must not advertise gated
skills should filter before calling, or use the Rust-side
`serve_prompts`, which evaluates the predicates.
"""

from __future__ import annotations

from mcp_methods import SkillRegistry


def register_skills_as_prompts(app, registry: SkillRegistry) -> int:
    """Register every skill in `registry` as a FastMCP prompt on `app`.

    Returns the count of registered prompts (useful for boot logs).
    Empty registries are a no-op — safe to call unconditionally.
    """
    count = 0
    for skill in registry.skills():
        _register_one(app, skill.name, skill.description, skill.body)
        count += 1
    return count


def _register_one(app, name: str, description: str, body: str) -> None:
    """Register a single skill as a FastMCP prompt.

    Kept as a separate function so the closure capturing `body` is
    fresh per skill — the obvious `for skill in ...: app.prompt(...)`
    loop binds the loop variable late, leaking the last-iteration
    body into every handler.
    """

    @app.prompt(name=name, description=description)
    def _handler() -> str:
        return body
