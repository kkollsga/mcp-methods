# Skills-Aware Manifests

The shortest path from "I want to ship methodology with my MCP server" to a running deployment. For background on what skills are and why they exist, see [Three-Layer Composition](../explanation/three-layer-composition.md). For the authoring details, see [Authoring Skills](authoring-skills.md).

## The five-minute version

```yaml
# my_mcp.yaml
name: my-server
skills: true                       # opt in to bundled framework skills
```

Boot the server:

```bash
mcp-server --mcp-config ./my_mcp.yaml
```

Connect any MCP client. You'll now see three prompts in `prompts/list`: `grep`, `read_source`, `list_source`. Each returns methodology for the corresponding framework tool. Two more bundled skills — `github_issues` and `repo_management` — gate on their tool actually being registered (`applies_when: tool_registered:`), so they appear only when the manifest opts into `builtins.github: true` (with a token reachable) or configures a workspace, respectively.

That's the bundled-only path. Three lines of YAML, zero new files.

## Adding a project skill

The project layer lives at `<manifest_basename>.skills/` next to the manifest. For `my_mcp.yaml`, that's `my_mcp.skills/`. Create a SKILL.md:

```markdown
<!-- my_mcp.skills/cypher_query.md -->
---
name: cypher_query
description: Cypher patterns specific to this deployment's graph schema.
auto_inject_hint: true
---

# Cypher methodology

For "what calls X" questions: ...
```

Re-boot the server. `prompts/list` now includes `cypher_query` alongside the bundled defaults active in the session (three in a default deployment; `github_issues` and `repo_management` join when their tools register). If your binary registers a tool named `cypher_query`, the framework also appends a pointer to that tool's description: "See `prompts/get` `cypher_query` for the full methodology."

## Adding a shared skill pack

If you maintain a library of skills used by multiple deployments, declare a path:

```yaml
name: my-server
skills:
  - true                           # bundled framework defaults
  - ~/shared-skills/               # operator's shared library
  - ./my_mcp.skills/               # (implicit — auto-detected)
```

Paths resolve against the manifest directory; `~/` prefixes against `$HOME`. The auto-detected project layer is always probed in addition to any explicit paths — you don't have to list it.

## Verifying what's resolved

The `mcp-server` binary ships three subcommands for inspecting your skill setup without booting a full server:

```bash
# Validate every SKILL.md in a directory.
mcp-server skills-lint ./my_mcp.skills/

# List every resolved skill, showing which layer each one came from.
mcp-server skills-list --mcp-config ./my_mcp.yaml

# Print the body of a single resolved skill.
mcp-server skills-show --mcp-config ./my_mcp.yaml cypher_query
```

`skills-list` is the easiest sanity check: it shows you, at a glance, every skill the server will surface and where each came from.

```
name                          provenance      status        description
----------------------------  --------------  ------------  ----------------------------------------
cypher_query                  project         active        Cypher patterns specific to this graph's schema, plus query
github_issues                 bundled         conditional   Search, list, or fetch GitHub issues, pull requests, and Dis
    [RUNTIME]  tool_registered: github_issues — resolved against the live tool router at boot
grep                          bundled         active        Regex text search across the configured source roots, with f
list_source                   bundled         active        List directory contents under the configured source roots in
read_source                   bundled         active        Read source files from the configured source roots, with opt
repo_management               bundled         conditional   Clone GitHub repos into the workspace, swap which repo is cu
    [RUNTIME]  tool_registered: repo_management — resolved against the live tool router at boot
```

`conditional` means the skill's `applies_when:` gate depends on runtime
state the CLI can't see (which tools actually registered): it activates at
boot if its tool does. `inactive` + `[FAIL]` is reserved for predicates
the CLI *can* decide offline — those skills really are suppressed.

A project-layer skill with the same name as a bundled one will show up with `provenance: project`, masking the bundled version. That's how you tell whether your override is winning.

## Backwards compatibility

A manifest with no `skills:` declaration boots identically to pre-0.3.35: no `prompts/*` capability advertised in MCP `initialize`, no behavioural difference. You can deploy 0.3.35 against existing manifests without touching anything; opt in to skills only when you're ready.

## Python / FastMCP path

Authoring is identical (same SKILL.md format, same `skills:` YAML field). Wiring is different — instead of relying on `mcp-server`, you compose at boot:

```python
from mcp.server.fastmcp import FastMCP
from mcp_methods import SkillRegistry
from mcp_methods.fastmcp import register_skills_as_prompts

app = FastMCP("My Server")
registry = SkillRegistry.from_manifest("./my_mcp.yaml")
register_skills_as_prompts(app, registry)
app.run(transport="stdio")
```

`SkillRegistry.from_manifest` includes bundled framework defaults by default; pass `include_bundled=False` to skip them.

## See also

- [Manifest Schema Reference](../reference/manifest-schema.md#skills-polymorphic-value) — the `skills:` field spec
- [Authoring Skills](authoring-skills.md) — frontmatter, size limits, the full file format
- [Three-Layer Composition](../explanation/three-layer-composition.md) — design rationale and resolution rules
