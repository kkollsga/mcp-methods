# Authoring Skills

Skills are operator-authored markdown files that ship as MCP prompts. The agent fetches them via `prompts/get` when it wants methodology for a specific tool or workflow. This guide covers the file format, where to put them, and the validation surface.

## When to author a skill

Reach for a skill when you find yourself writing the same methodology paragraph into a tool description over and over. Tool descriptions live in `tools/list` output — they're scanned at session start and budget-constrained. Methodology that's longer than a paragraph belongs in a prompt, not a description.

Good skill candidates:
- "How to choose between FETCH / SEARCH / LIST modes on the github_issues tool"
- "Cypher patterns for graph traversal questions"
- "When to use `force_rebuild` versus `update`"

Not skill candidates:
- One-line clarifications (just put them in the tool description)
- Workflows that the framework already enforces at the type level (`tools/list` is already honest about what's reachable)
- Domain knowledge unrelated to specific tools (those go in the agent's system prompt, not as MCP prompts)

## File format

A SKILL.md file is two parts: YAML frontmatter (between `---` lines) and a markdown body.

```markdown
---
name: cypher_query
description: Cypher patterns for graph traversal and counting questions.
applies_to:
  mcp_methods: ">=0.3.35"
  kglite_mcp_server: ">=0.9.30"
references_tools:
  - cypher_query
references_arguments:
  - cypher_query.format
references_properties:
  - Function.module
  - Class.name
auto_inject_hint: true
---

# Cypher methodology

Body content. Use markdown freely — headings, lists, code fences.

## Choose your traversal pattern

For "what calls X" questions: `MATCH (caller)-[:CALLS]->(:Function {name: "X"})`.
For "where is X defined" questions: graph queries beat grep here.

...
```

### Frontmatter fields

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | **yes** | The lookup key. `prompts/get` requests come in with this name. Must match the filename's stem by convention (`cypher_query.md` → `name: cypher_query`). |
| `description` | string | **yes** | One-line summary for `prompts/list`. The agent decides whether to fetch the body based on this. Keep it under ~100 chars. |
| `applies_to` | mapping | no | Semver constraints. The framework records but doesn't enforce yet — lint warnings only. |
| `references_tools` | list&lt;string&gt; | no | Tools this skill discusses. Used for the auto-inject pass (see below). |
| `references_arguments` | list&lt;string&gt; | no | Specific tool arguments mentioned. Documentation-grade; no runtime effect yet. |
| `references_properties` | list&lt;string&gt; | no | Domain-specific entities (graph node types, etc.). Documentation-grade. |
| `auto_inject_hint` | bool | no (default `true`) | When `true` and the skill's name matches a registered tool, the framework appends a "see `prompts/get` `<name>` for full methodology" pointer to that tool's description. |
| `applies_when` | list&lt;predicate&gt; | no | Reserved for Phase 3 — predicates parse but don't gate yet. |

## Where to put skills

Three layers, highest priority first:

1. **Project layer** — `<manifest_basename>.skills/` directory adjacent to the manifest YAML. For `mcp-servers/legal_mcp.yaml`, the project layer is `mcp-servers/legal_mcp.skills/`. Auto-detected; you don't have to list it in `skills:`.
2. **Domain pack(s)** — operator-declared paths in the manifest's `skills:` list. Use for shared skill libraries across deployments.
3. **Bundled defaults** — compile-time skills shipped by the framework and (optionally) downstream binaries. Opt in with `skills: true` (or include `true` in the list form).

Same-named skills in higher layers fully replace lower ones — no merging. A project-layer `grep.md` completely masks the bundled `grep` skill.

## Size limits

- **4 KB** — soft limit. Lint emits a `WARN` line above this size, but resolution proceeds.
- **16 KB** — hard limit. A skill that exceeds this rejects at load time. Split into multiple skills if you need more.
- **64 KB** — total resolved-set cap. If the sum of all resolved skill bodies exceeds this, late skills drop with a `tracing::warn` at boot.

These exist because the agent's context window is finite. A 16 KB prompt is already a meaningful chunk of context; respect the agent's budget.

## Scaffold a starter skill

Authoring from a blank file is the worst part of writing skills — operators stare at frontmatter syntax and skip the description because it's the dullest field to draft. Two surfaces help:

```bash
# CLI — writes <skill_name>.md into the chosen directory.
mcp-server skills-new ./mcp-servers/my_mcp.skills/ cypher_query \
  "TRIGGER when the user asks a question about the graph that requires multi-hop traversal..."
```

```python
# Python — same behaviour, returns the resolved path.
from mcp_methods import write_skill_template

write_skill_template(
    "./mcp-servers/my_mcp.skills/",
    name="cypher_query",
    description="TRIGGER when the user asks a question about the graph...",
)
```

The template lands with the name + description filled in, the optional extension fields commented out, and a body skeleton (Overview → Quick Reference → Common Pitfalls → "When wrong") with `<TODO>` placeholders the operator fills in. See [Writing Effective Skills](writing-effective-skills.md) for the patterns each section follows.

The helpers refuse to overwrite existing files — delete first if you want to replace. Empty descriptions also refuse: a blank description guarantees undertriggering, so the helpers force the operator to commit to one before writing.

## Lint and inspect

Three CLI subcommands ship with `mcp-server` (and are available to downstream binaries via `mcp_methods::server::cli`):

```bash
# Validate every SKILL.md in a directory. Exit code 1 on hard error.
mcp-server skills-lint ./mcp-servers/legal_mcp.skills/

# List every resolved skill for a manifest, with provenance.
mcp-server skills-list --mcp-config ./mcp-servers/legal_mcp.yaml

# Print the full body of one resolved skill.
mcp-server skills-show --mcp-config ./mcp-servers/legal_mcp.yaml cypher_query
```

`skills-list` is especially useful for confirming that your project-layer skill is actually winning over a bundled default of the same name — the `provenance` column shows where each resolved skill came from.

## Auto-injection

Skills with `auto_inject_hint: true` (the default) get a pointer auto-appended to the matching tool's description when their `name` matches a registered tool. The pointer is roughly:

> See `prompts/get` `<name>` for the full methodology.

This means agents that scan `tools/list` first can still discover the methodology even if they never explicitly look at `prompts/list`. The hint is appended once at boot; tool-description mutation is non-destructive (the original description is preserved as a prefix).

Skills whose name doesn't match any registered tool are still surfaced via `prompts/list` — they're just not auto-injected anywhere. Use this for cross-cutting methodology like `workflows/test-driven-development.md`.

## See also

- [Writing Effective Skills](writing-effective-skills.md) — the *craft* side: description patterns, body anatomy, tone, what we learned from reading Anthropic's published skills
- [Manifest Schema Reference](../reference/manifest-schema.md#skills-polymorphic-value) — the `skills:` field shape
- [Three-Layer Composition](../explanation/three-layer-composition.md) — why three layers, when each is the right home
- [Python bindings](python-bindings.md#skills) — `SkillRegistry` and `register_skills_as_prompts` for FastMCP authors
