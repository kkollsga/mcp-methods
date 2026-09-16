# Authoring Skills

Skills are operator-authored markdown files carrying methodology for a tool or workflow. The framework appends each one to the descriptions of the tools it targets — either the whole body, or a pointer the agent follows with the `skill(name)` tool — and also serves it on the MCP `prompts/*` plane for clients and CLI tooling that read it. This guide covers the file format, where to put them, and the validation surface.

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
delivery: eager
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
| `auto_inject_hint` | bool | no (default `true`) | When `true`, the framework appends the skill to the description of its name-match tool and every tool in `references_tools`. See [Auto-injection](#auto-injection). |
| `delivery` | `eager` \| `lazy` | no (**default `lazy`**) | Which tier that injection uses. `lazy` ships the description plus a `skill("<name>")` pointer; `eager` ships the whole body. See [Auto-injection](#auto-injection). Any other value is a parse error. |
| `applies_when` | mapping | no | Bounded predicate block (`tool_registered`, `extension_enabled`, `graph_has_node_type`, `graph_has_property`). All populated predicates must hold or the skill is suppressed. |

## Where to put skills

Five layers, highest priority first:

1. **Project layer** — `<manifest_basename>.skills/` directory adjacent to the manifest YAML. For `mcp-servers/legal_mcp.yaml`, the project layer is `mcp-servers/legal_mcp.skills/`. Auto-detected; you don't have to list it in `skills:`.
2. **Domain pack(s)** — operator-declared paths in the manifest's `skills:` list. Use for shared skill libraries across deployments.
3. **Manifest-inline** — mapping entries in the `skills:` list, body and all. One layer whatever their position in the list. Use for a skill too short to earn its own file.
4. **Owned** — bodies the host binary supplies at runtime via `Registry::add_layer`, e.g. skills carried inside the artefact the server serves.
5. **Bundled defaults** — compile-time skills shipped by the framework and (optionally) downstream binaries. Opt in with `skills: true` (or include `true` in the list form), which also switches on the owned layer.

Same-named skills in higher layers fully replace lower ones — no merging. A project-layer `grep.md` completely masks the bundled `grep` skill.

## Size limits

- **4 KB** — soft limit. Lint emits a `WARN` line above this size, but resolution proceeds.
- **16 KB** — hard limit. A skill that exceeds this rejects at load time. Split into multiple skills if you need more.
- **64 KB** — total resolved-set budget. If the sum of all resolved skill bodies exceeds this, boot logs a `tracing::warn` naming the total. **Nothing is dropped** — which skills you run stays your call. The sum counts every resolved skill whatever its `delivery:`, because a lazy body is one `skill(name)` call away from the same session.

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

Skills with `auto_inject_hint: true` (the default) are appended at boot to the description of their name-match tool **and** every tool listed in `references_tools`. Agents scan `tools/list`; they do not see `prompts/list`, so this is the delivery channel that matters. Mutation is non-destructive — the tool's original description stays as the prefix — and each `(skill, tool)` pair is fenced by a `<!-- mcp-skill:<name> -->` marker, so the pass never double-appends.

**`delivery:` decides what gets appended, and it defaults to `lazy`.**

`delivery: lazy` — the description, then a pointer:

```
<!-- mcp-skill:cypher_query -->

## When to use

Cypher patterns for graph traversal and counting questions.

Load the full methodology with skill("cypher_query") before first use.
```

`delivery: eager` — the description, then the whole body under `## Methodology`.

The pointer names the framework's **`skill(name)` tool**, registered automatically whenever skills are on. It is an ordinary MCP tool, so every client shows it to the model — unlike `prompts/get`, which is why the pre-0.3.37 pointer pattern failed. Loading is per session: until the agent has called `skill("<name>")`, every result from a tool carrying that unloaded lazy skill comes back with one footer line reminding it. After that it stays quiet for as long as the session keeps calling tools. Three things bring the reminder back: a new session, a session that made no tool call at all for ten minutes, and a re-resolve that changed that skill's body (see [Rebuilding skills at runtime](#rebuilding-skills-at-runtime)). The loader returns the body verbatim — it is exempt from the default response budget, which is safe precisely because a body is capped at 16 KB when it loads.

Reach for `eager` only when the body shapes the **first** call's parameters — a query language the agent must write correctly before it has any result to learn from. Everything else is cheaper lazy, and the tool list stops growing with skills × referenced tools.

A skill that **declares** targets and finds every one of them unregistered is dropped: no prompt route, no injection, and `skill()` will not serve it. It had no way to reach the agent — it asked for tools this deployment does not run. "Declared" means the name-match tool when a tool by that name exists, plus every `references_tools` entry.

A skill that declares no targets at all — nothing shares its name, `references_tools` is empty — is kept. It injects nowhere by construction rather than by accident, stays in `prompts/list`, and `skill(name)` serves it, which is how another skill's body can point at it.

## Rebuilding skills at runtime

Skill **bodies** are static — the framework never renders anything into them
(see [Skill Layer Composition](../explanation/three-layer-composition.md)). The
**resolved set** is not: a downstream binary that swaps the artefact its skills
come from — a graph reloaded by a tool call, say — can re-resolve a registry
after the server is already serving.

`serve_prompts` at boot and `McpServer::reinject_skills` afterwards are the same
pass. The rebuild strips every `<!-- mcp-skill:… -->` block from every tool
description, removes the prompt routes the previous pass registered (and only
those — a route you added through `prompt_router_mut()` stays), re-registers the
`skill(name)` loader with the new bodies, and injects again.

A dynamically registered tool handler is a plain `Fn(T) -> Result<String,
String>` with no `&self` in reach, so take a handle **before** you register the
tool and capture it:

```rust
let reloader = server.skill_reloader();
server.register_typed_tool_fallible("reload_graph", "…", move |args: ReloadArgs| {
    let registry = swap_the_graph(args)?;              // your domain work
    let active = reloader.reinject_skills(&registry)?; // the framework's
    Ok(format!("reloaded; {} skills active", active.len()))
});
```

**You must notify the peer.** The framework stores no peer handles, so
`tools/list` and `prompts/list` change under clients that will keep serving
their cached copies. A handler holds a `RequestContext`, which holds the peer:

```rust
mcp_methods::server::notify_skills_changed(&context.peer).await?;
```

That is `notify_tool_list_changed()` followed by `notify_prompt_list_changed()`;
call them yourself if you want only one. `get_info` advertises
`tools.listChanged: true` unconditionally and `prompts.listChanged: true`
whenever the prompts capability is advertised at all, so a client has reason to
re-fetch.

Three rules worth knowing before you wire this:

- **Capabilities are fixed at `initialize`.** A server that boots with *no*
  skills advertises no prompts capability, and rebuilding into one does not
  change that for the live session — the new prompt routes are registered but
  unadvertised. Boot with at least one skill if the set can grow later.
- **The session loaded-set survives, selectively.** A skill an agent already
  fetched with `skill(name)` stays fetched when its body is unchanged. A name
  whose **body changed** is forgotten in every session, so the next call of a
  tool that advertises it nudges the agent to re-read it.
- **Refused from the wrong places.** A rebuild started from inside a
  `with_result_postprocess` hook, or from inside the resolution pass itself
  (a `SkillPredicateEvaluator`, say), returns an `Err` naming
  `reinject_skills` rather than running. Neither has a peer to announce the
  change on, and a nested rebuild is overwritten by the pass containing it.
  From an ordinary tool handler it is the intended use, including while other
  calls are in flight — no lock is held across a tool handler, so a long call
  and a rebuild do not block each other.

## See also

- [Writing Effective Skills](writing-effective-skills.md) — the *craft* side: description patterns, body anatomy, tone, what we learned from reading Anthropic's published skills
- [Manifest Schema Reference](../reference/manifest-schema.md#skills-polymorphic-value) — the `skills:` field shape
- [Skill Layer Composition](../explanation/three-layer-composition.md) — why the layers exist, when each is the right home
- [Python bindings](python-bindings.md#skills) — `SkillRegistry` and `register_skills_as_prompts` for FastMCP authors
