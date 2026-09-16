# Skills-Aware MCP — Bringing the Skill Pattern Inside the Server

**Status:** Conceptual design analysis. No implementation decision yet.
**Date:** 2026-05-14
**Companion doc:** [`agent-cli-pattern.md`](./agent-cli-pattern.md) — research on the agent CLI/skills shift and the parallel-train option.
**Audience:** Internal — explores a third path between "MCP-only" and "build a parallel CLI/skills train."

---

## TL;DR

The companion doc proposed a parallel CLI/skills train as the path for adopting the agent-skills pattern. That works for stateless workloads (ripgrep, GitHub fetching). It fundamentally **doesn't work for stateful workloads** like kglite — a 158k-node graph can't be reloaded on every CLI invocation. The MCP server's "boot once, hold state, dispatch tools" pattern is the right architecture for that workload class.

This proposal: **make the MCP server itself skills-aware** rather than building parallel CLIs. Keep the stateful tools where they are; add a methodology layer through MCP's underused `prompts/` namespace.

**Status (2026-05-14):** kglite confirmed this fits their deployment shape and committed to authoring 4 bundled skills (~1-2 days lift) plus ~10 LOC of wiring in `kglite-mcp-server/main.rs`. Design has been refined based on their feedback to incorporate **9 design refinements** (versioned frontmatter, lint CLI, domain-neutral bundled constraint, domain skill-pack layer, applies_when predicates, full-file replacement, collision logging, auto-injected discoverability hints, size limits) and **6 helper-API obligations** the framework absorbs (SkillRegistry builder, include_skill! macro, serve_prompts wiring, predicate evaluator trait, CLI subcommand kit, pyo3 coverage).

**What this gets us (the load-bearing wins):**

1. **Operator-authored methodology in markdown.** Today, tool descriptions are limited to ~200 chars. There's no good place for "FIRST STEP: call `repo_management`," "if the result has >50 nodes, narrow with a label filter," or "use `text_score()` only when `trust.allow_embedder: true`." Skills land in that gap.

2. **Library-bundled defaults — protocol-level only.** `skills: true` gives every operator rich methodology for the framework's tools and the downstream binary's custom tools. Critical constraint: **bundled skills teach the TOOL, not the GRAPH.** They never embed domain knowledge (legal jurisdiction, o&g well_id conventions, code-tree types). Domain methodology lives in operator-curated layers, not framework or downstream-binary bundles.

3. **Layered composition** (project → domain packs → inline → owned → bundled defaults). A polymorphic `skills:` field accepts `true`, paths, inline skill mappings, or a mixed list — each entry is a skill source, walked in declaration order with project-local `<basename>.skills/` always at top priority. The middle layer is **first-class for domain skill-packs**: operators with multiple deployments (e.g. legal / o&g / code servers sharing protocol-level skills but with non-overlapping domain methodology) compose them naturally.

4. **Staleness prevention via versioned frontmatter.** Every skill carries `applies_to:` semver constraints, `references_tools:` declarations, and `references_arguments:` references. Boot-time validation warns loudly on mismatches; a `skills-lint` CLI subcommand catches errors at PR time. Operators run linted skill packs through CI before deploying.

5. **Cross-content portability via filesystem.** Skills are plain markdown. Operators can symlink their domain-pack or project-local directories into `~/.claude/skills/` — Claude Code's native skill loader reads the same files. Same source-of-truth, two interfaces.

**What this does NOT claim:** this is **not primarily a context-efficiency win**. Claude Code's Tool Search already handles tool-schema bloat at the client layer (~95% reduction). Skills add ~300-500 tokens of methodology metadata on top — additive richness, not a substitute for Tool Search. The "50+ tools horror story" cited in skill-pattern literature applies to clients without Tool Search; modern Claude Code sessions already have the registry-size problem solved. The proposal stands on the methodology + authoring + portability merits, not on tokens-saved. (kglite's reply explicitly flagged this as the highest-risk failure mode — "building it as a context-efficiency play rather than as a methodology-authoring surface for operators" — and we've calibrated accordingly.)

**Backwards compatibility:** Opt-in. The default (no `skills:` declaration) is verbatim-current MCP behavior. Existing kglite deployments work unchanged.

**Sequencing constraint from kglite:** "The domain-pack composition is the only thing we'd push back on sequencing — protocol-level skills alone don't serve our deployment shape." Phase 1 ships the composition layer. `include_skill!` macro and `applies_when:` predicates can defer to Phase 2.

---

## 1. Why this path exists — the stateful-workload gap

### 1.1 What the parallel CLI/skills train solves

From the companion doc: ripgrep, GitHub fetching, HTML→markdown, text compaction — all stateless. Each invocation is independent. Loading cost ≈ zero. The CLI/skills pattern fits them naturally: each call boots a new process, does the work, prints to stdout, exits.

### 1.2 What it doesn't solve

kglite's primitives are stateful:

- `cypher_query(query)` — needs the graph in memory; loading is expensive
- `graph_overview()` — needs the graph
- `save_graph()` — mutates the graph
- `text_score(query)` — needs the embedder loaded
- `read_code_source(qualified_name)` — needs the graph to resolve qualified names to file/line

A CLI implementation would:

```text
agent → kglite-cypher "MATCH (n:Function) RETURN count(n)"
        process starts → load 158k-node graph (3s) → run cypher → return → exit
agent → kglite-cypher "MATCH (n:Class) RETURN count(n)"
        process starts → load 158k-node graph AGAIN (3s) → run cypher → return → exit
```

The Unix-tradition workaround is **daemon + thin client** (`tmux` / `gpg-agent` / `docker` model): long-running daemon holds state, CLIs talk to it over a local socket. But that's effectively re-inventing MCP with a different protocol. We'd save no implementation effort, lose protocol-level features (capabilities negotiation, structured errors, OAuth), and end up with a non-standard local-RPC story.

The honest conclusion: **MCP's long-running-server architecture is the right shape for stateful workloads.** The CLI/skills train is right for stateless workloads. They serve different categories.

### 1.3 But the skill-pattern wins are real

The companion doc enumerated the wins:

1. Operator authoring story — markdown vs Rust/Python/protocol code
2. Filesystem-discoverable behavior — `ls ~/.claude/skills/` shows what's installed
3. Faster workflow-content iteration — edit markdown, takes effect next agent invocation
4. Version control alignment — workflow lives in the project repo
5. Lower contribution barrier — anyone who can write markdown can contribute
6. Cross-vendor portability — same SKILL.md in Codex CLI, Cursor, etc.
7. No daemon lifecycle — short-lived processes
8. Composable with Unix pipelines — text in/out
9. Simpler install — drop a folder, done

(Note: "context efficiency" is often cited as a win but is actually delivered by client-layer Tool Search, not skills themselves. See §4 for the honest framing.)

Wins 1-5 are about how the agent and operator interact with the system — they don't require leaving MCP. **They require the MCP server to expose richer surface than just `tools/`.**

Wins 6-9 are about the distribution shape. They genuinely require leaving MCP. But these are the wins we need least for our specific audience (kglite + Claude/Claude Code operators).

So: **the wins we care about can come into the MCP server.** The wins we'd lose by staying in MCP are mostly wins we wouldn't use.

---

## 2. The layered skills model

### 2.1 The schema — polymorphic `skills:` field

Skills are opt-in. The default (no `skills:` declaration) is unchanged behavior — no prompts surface, no context cost beyond the existing MCP tools. To enable, an operator adds a `skills:` field to the manifest with one of these shapes:

```yaml
# Default — no skills surface (omit the field, or `skills: false`)
# Behaves identically to current MCP — backwards compatible

# Enable with library-bundled defaults only:
skills: true

# Enable with a single custom path:
skills: ./local-skills/

# Enable with a list mixing library defaults, paths, and inline skills:
skills:
  - true                          # library-bundled (mcp-methods + downstream binary defaults)
  - ./local-overrides/            # this deployment's specific tweaks
  - ~/shared-mcp-skills/          # team's shared library (in user home)
  - /etc/org-skills/              # absolute path, e.g. enterprise-managed
  - name: house_style             # an inline skill — body lives in this YAML
    description: How this deployment names and cites things.
    body: |
      # House style

      Cite by paragraph, never by page.
    references_tools:             # optional
      - cypher_query
    delivery: lazy                # optional; `eager` | `lazy`, default `lazy` (§2.10)
    applies_when:                 # optional; same predicates as a SKILL.md
      graph_has_node_type: [Case]
```

The `true` literal in a list is the special token meaning "include library-bundled skills." String entries are filesystem paths. **Mapping entries are inline skills**: the frontmatter keys that make sense without a file, plus the markdown `body` that would follow the closing `---`. `name`, `description` and `body` are required; unknown keys in the mapping are a manifest error, like an unknown `workspace:` or `builtins:` key. Inline entries all join one fixed layer wherever they appear in the list — see §2.2.

`delivery:` is validated as `eager` or `lazy` so a typo fails at manifest load, and is carried into the rendered frontmatter where the injection pass reads it: `lazy` (the default when omitted) injects routing text plus a `skill("<name>")` pointer, `eager` injects the full body — see §2.10.

**Path conventions match the existing manifest fields** (`source_root:`, `workspace.root:`, `env_file:`):

| Path form | Resolves to |
|---|---|
| `./local/` or `local/` | Manifest's parent directory (most common) |
| `~/team/` | User home (standard shell `$HOME` expansion) |
| `/abs/path/` | Absolute |
| `true` (boolean, in list) | Library-bundled skills (compiled into the binary) |
| mapping (in list) | An inline skill — no path, the body is in the YAML |

### 2.2 The layers

When skills are enabled, up to five layers contribute. Each layer has a distinct purpose and authoring audience:

```
Project layer (auto-detected, top priority):
  ./<basename>.skills/<name>.md       always considered when skills are enabled
                                      adjacent to the manifest YAML
                                      authored by: the deployment's operator
                                      purpose: per-deployment overrides
                                      examples: "legal-uk corpus specifics",
                                                "our staging quirks"

Domain skill-pack layer (operator-declared paths in the `skills:` list):
  ./<path-to-pack>/<name>.md          path entries in the YAML's skills: list
  ~/<path-to-pack>/<name>.md          walked in declaration order
  /abs/<path-to-pack>/<name>.md       first match per skill name wins
                                      authored by: domain specialists
                                      purpose: methodology for the GRAPH /
                                               domain knowledge / business rules
                                      examples: kglite-skills-legal pack,
                                                kglite-skills-og pack,
                                                kglite-skills-code pack

Inline layer (mapping entries in the `skills:` list):
  skills:                             one fixed layer, whatever the list order
    - name: ...                       authored by: the deployment's operator
      body: ...                       purpose: a skill too small or too
                                               deployment-specific to earn a file
                                      examples: "our citation convention",
                                                "this tenant's table names"

Owned layer (runtime bodies from `Registry::add_layer`):
  supplied by the host binary at       assembled at boot from whatever the host
  boot, not read from disk             reads — a graph file, a database row
                                      authored by: whoever authored the artefact
                                      purpose: an artefact that carries its own
                                               usage methodology
                                      examples: skills stored inside a .kgl graph

Bundled layer (the `true` token in the `skills:` list):
  compiled into the binary via         include_str! at compile time
  include_str!                          framework + downstream binary defaults
                                       authored by: framework / library authors
                                       purpose: methodology for the TOOL
                                                (protocol-level only)
                                       examples: cypher_query.md (how the tool
                                                 works, FORMAT CSV pattern,
                                                 common errors)
```

Per skill name, resolution walks **project → domain → inline → owned → bundled** in priority order. The first source with the matching `name` field wins. The skill body becomes an MCP prompt the agent loads on demand via `prompts/get`.

The `true` token switches on the two layers the *binary* supplies — bundled and owned. The project, domain and inline layers are the operator's own declarations, so they surface without it; only `skills: false` (which declares nothing at all) hides everything.

The layers correspond to distinct authoring audiences with non-overlapping responsibilities:

- **Bundled (framework + downstream binary author)** ships protocol-level methodology that's identical across deployments. The library author writes once; every operator inherits.
- **Owned (artefact author)** ships methodology that travels with the data the server serves, so a graph can teach its own use without the operator copying files.
- **Inline (operator)** ships a skill small enough that a separate file would be more bookkeeping than content, in the same YAML that turns the feature on.
- **Domain pack (specialist author)** ships methodology specific to a knowledge domain (legal, oil-and-gas, code analysis, etc.) but agnostic to a specific deployment. Authored once per domain, reused across many operator deployments.
- **Project (operator)** ships per-deployment specifics — "this legal corpus uses Canadian citation conventions," "this o&g database has a non-standard well_id format." Authored once per deployment.

### 2.3 Why the bundled layer must be domain-neutral

This is a load-bearing constraint surfaced by kglite's three-deployment reality. Their operator runs three servers — legal, o&g, code — each with non-overlapping domain methodology. A bundled `cypher_query.md` that says "use `.module` to filter modules" is **actively wrong** for legal and o&g (no Function nodes, no `.module` property — those graphs have Case / Statute / Well / Field types).

**Authoring rule:** Library-bundled skills (framework + downstream-binary defaults) teach the **TOOL**, not the **GRAPH**.

✓ Bundled `cypher_query.md` may say: "always pass query as a string; FORMAT CSV for >50 rows; common shape mistakes (returning whole nodes vs properties)"

✗ Bundled `cypher_query.md` must NOT say: "use `.module` to filter modules" (code-only) or "filter by jurisdiction first" (legal-only)

Domain methodology lives in the domain skill-pack layer, not the bundled layer. The framework's authoring guidelines and `skills-lint` will enforce this convention through documentation and warnings; downstream binary authors who violate it ship broken defaults to operators with mismatched domains.

### 2.4 Library-bundled skills (mechanics)

When `true` appears anywhere in the `skills:` list, the framework includes skills compiled into the binary. Two sources contribute:

- **mcp-methods's bundled defaults** — methodology for the framework's own primitives (`grep`, `read_source`, `list_source`, `github_discussions`, `repo_management`). Embedded via `include_str!` at compile time. Shipped with the `mcp-methods` library.
- **Downstream binary's bundled defaults** — kglite-mcp-server (or any other downstream Rust binary) can compile its own skills the same way. kglite would ship `cypher_query.md`, `graph_overview.md`, `save_graph.md`. These bundle into the kglite-mcp-server binary.

Both layers are loaded when `true` is in the list, providing rich methodology for all the tools the deployment exposes. The operator gets value without authoring anything.

### 2.5 Skills surfaced as MCP prompts

[MCP's Prompts feature](https://modelcontextprotocol.io/specification) is genuinely under-used. The protocol defines:

- `prompts/list` — returns metadata for all available prompts (name + description + arguments)
- `prompts/get` — returns the full prompt content by name

Most MCP servers ignore this. We use it as the natural home for skill content:

```
prompts/list response (cheap — ~hundreds of tokens):
[
  {name: "cypher_query", description: "Workflow for writing Cypher queries against this graph..."},
  {name: "github_discussions", description: "Smart issue/PR fetching with compaction..."},
  {name: "codebase_navigation", description: "How to explore an unfamiliar repo..."},
  ...
]

prompts/get?name=cypher_query response (loaded only when needed):
[full SKILL.md body — 2-5k tokens of methodology, examples, gotchas]
```

The agent's startup context now contains:
- Tool registry (existing MCP tools, unchanged behavior)
- Skill metadata (just names + descriptions)

Full skill bodies load on demand when the agent decides a skill is relevant. This is **progressive disclosure within MCP** — exactly the win that drove the agent-skills design.

Crucially: the SKILL.md files are **plain markdown on the filesystem** (or compiled bytes for bundled defaults). An operator can symlink `./local-overrides/` to `~/.claude/skills/` and Claude Code's native skill loader reads the same files. The skill content is portable to skills-supporting clients even though the MCP server's tools aren't.

### 2.6 Skill bodies are static; the resolved set is not

Two different things get called "dynamic", and only one of them is refused.

**A skill body is never rendered.** Skills are pure markdown. The framework does NOT do server-side template substitution, run shell commands, or invoke tools to splice their output into skill bodies. **Skills teach the agent how to use tools; tools provide dynamic content when the agent decides to call them.**

**The resolved set can change while the server runs.** Since 0.4.11 a downstream binary can re-run the whole resolution pass against a different registry after `serve`, via `McpServer::reinject_skills` (or a `SkillReloader` handle captured into a tool closure). That is what a server whose skills come from a swappable artefact needs: kglite reads `KgliteSkill` nodes out of the `.kgl`, and `load_graph`/`reload_graph` replaces the artefact those nodes live in. The rebuild strips the previous injection from every tool description, replaces the prompt routes the pass owns, re-registers `skill(name)` with the new bodies, and injects again; the caller then sends `notifications/tools/list_changed` and `notifications/prompts/list_changed` from the peer its request context already holds (`notify_skills_changed`). Capabilities advertise `listChanged` so a client has reason to re-fetch.

The distinction is the point: the *set* of skills follows the artefact, and each skill's *text* is still the file (or the graph record) an operator can read, with no runtime state required to understand it. Nothing below about caching or portability changes — a body that changed under a session is simply re-nudged, because what the agent is holding is no longer what `skill()` would serve.

This is the same separation Anthropic's own skills follow. The PDF skill doesn't pre-render document contents; it teaches the agent to call `pdfplumber.open()` to read what it needs. Same pattern: skill = methodology, tool = data access.

This separation has practical benefits:
- **Skill loading is cheap and deterministic** — no per-call cost variance, no surprise context blow-up if a tool's output happens to be large that session
- **Skills are reviewable** — operator reads the file, sees exactly what the agent sees, no runtime state required to understand the behavior
- **Invalidation has exactly one trigger** — a body never changes under a call, so the only thing that can invalidate a loaded skill is a deliberate re-resolve, and the rule there is one line: a name whose body hash changed is forgotten in every session, everything else stays loaded
- **Cross-vendor portability is preserved** — Claude Code's filesystem skill loader sees the same content the MCP server sees, no template-tag parser required

For dynamic content (graph schemas, current branch state, etc.), the skill instructs the agent to call the relevant tool. E.g. kglite's `cypher_query.md` would say "When you don't know the schema, call `graph_overview()` first" — and the agent does. The skill stays small and static; the dynamic content lives in the tool's response only when needed.

### 2.7 Versioned frontmatter — staleness prevention

Skills declare compatibility metadata in their YAML frontmatter. This is kglite's highest-priority design ask because they ship weekly and skill-vs-tool drift is a real failure mode.

```markdown
---
name: cypher_query
description: Workflow for writing Cypher queries against this graph.
applies_to:
  mcp_methods: ">=0.3.35, <0.4"           # framework version range
  kglite_mcp_server: ">=0.9.30, <0.10"    # downstream-binary version range (optional)
  pack_version: legal-skills/2026.05      # operator's pack stamp (optional)
references_tools:
  - cypher_query                          # tools this skill teaches
  - graph_overview                        # tools this skill references in prose
references_arguments:                     # specific argument names referenced
  - cypher_query.query
  - cypher_query.format                   # "FORMAT CSV" pattern
references_properties: []                 # graph properties referenced (domain only)
auto_inject_hint: true                    # framework auto-injects "see prompts/get name" pointer
                                          # into the matching tool's description
---

# Cypher Queries

[markdown body]
```

At boot, the framework walks each skill's frontmatter and:

- Validates `applies_to` semver constraints against the running binary versions. **Warns** (not errors) on mismatch — operators see staleness loudly without their server failing to boot.
- Walks `references_tools` against the registered tool catalogue. Warns on unknown names ("this skill references `old_tool_name`, no longer registered").
- Walks `references_arguments` against each referenced tool's input schema. Warns on argument names not in the schema ("`FORMAT` is not a known argument of `cypher_query`").
- `references_properties` is for domain-pack authors to declare graph-property dependencies (e.g. `Function.name`); the framework can't validate these statically, but the field is part of the contract for the predicate evaluator (§2.8).

Failure mode prevented: someone bumps `cypher_query`'s argument set and forgets to update the skill — every boot of every deployment emits a warning. Staleness becomes visible at deploy time, not when the operator notices the agent doing something weird three weeks later.

### 2.8 `applies_when:` predicates — context-sensitive skills

Skills that only make sense when the graph has certain shapes declare it in frontmatter:

```markdown
---
name: read_code_source
description: Resolve qualified names to source-tree files and read ranges.
applies_when:
  graph_has_node_type: [Function, Class]
  tool_registered: read_code_source
---
```

At boot, the framework evaluates the predicate. If the active graph doesn't satisfy it, the skill is omitted from `prompts/list` entirely (or surfaced with an inactive marker, TBD via the §2.7 lint warnings). Closes the "agent loads `read_code_source.md` in a legal deployment and gets nonsense" failure mode.

Predicate set is **bounded** — not a full DSL. Five types:

| Predicate | Evaluator |
|---|---|
| `framework_version_in: "<semver range>"` | Framework — checks `mcp-methods` version |
| `binary_version_in: "<semver range>"` | Framework — checks downstream binary version (kglite-mcp-server, etc.) |
| `tool_registered: <tool_name>` | Framework — checks the active tool router |
| `extension_enabled: <extension_name>` | Framework — checks `manifest.extensions` |
| `graph_has_node_type: [...]` | Downstream consumer — pluggable via the `SkillPredicateEvaluator` trait (§9 Helper #4) |
| `graph_has_property: {Type.name}` | Downstream consumer — same trait |

The framework dispatches generic predicates itself. Domain predicates (graph-shape checks) go through a `SkillPredicateEvaluator` trait that downstream binaries implement. Returning `None` from the evaluator means "I can't evaluate this predicate" — framework falls back to a conservative answer (skill stays registered with a warning).

This is Phase 2 territory — Phase 1 ships without `applies_when:` predicate evaluation (skills always loaded if they pass version checks). Phase 2 adds the predicate engine when there's a concrete demand.

### 2.9 Boot-time collision logging

When multiple layers contribute candidates for the same skill name, the framework logs which one wins:

```
mcp-server: skill cypher_query: 3 candidates resolved.
  → bundled (kglite-mcp-server 0.9.30)
  → ../skill-packs/legal/cypher_query.md
  → ./this-deployment.skills/cypher_query.md   [active]
```

Same shape as the existing manifest-load summary. Operators see exactly which file the agent ends up loading. Prevents the "I dropped a file in `./skills/` and nothing happened" debugging loop.

### 2.10 Tool descriptions auto-advertise their skills — two tiers

When `auto_inject_hint: true` (the default), the framework appends the skill to the `description` of its **name-match tool and every tool it lists in `references_tools`**. `prompts/list` is not the delivery channel: Claude Code, Claude Desktop, Cursor and Continue expose only `tools/*` to the model, so a skill that lives only on the `prompts/` plane reaches nobody.

What gets appended depends on the skill's `delivery:` key. **The default is `lazy`.**

| `delivery:` | What lands in every target tool's description |
|---|---|
| `lazy` (default) | `<!-- mcp-skill:NAME -->`, `## When to use` + the description, and one line: `Load the full methodology with skill("NAME") before first use.` |
| `eager` | `<!-- mcp-skill:NAME -->`, `## When to use` + the description, and `## Methodology` + the whole body |

Both tiers ship the **description** eagerly — it is the TRIGGER/SKIP routing the agent reads to decide whether the body is worth having, it is small by design, and it is not subject to the body's size caps. They differ only in when the body is paid for.

The lazy tier works because of the framework's **`skill(name)` tool**, registered by `serve_prompts` whenever skills are on. It returns the skill's body and is an ordinary MCP tool, so every client shows it to the model — which is exactly what the pre-0.3.37 `prompts/get` pointer could not claim. Loading is **per session**: `skill()` records what it delivered, and until it has, every call of a tool carrying an unloaded lazy skill comes back with one footer line naming the skill and the call that fetches it. Once loaded, the nudge stops for as long as the session keeps calling tools — any tool call is what keeps the record alive, not the last `skill()` call. A new session starts empty, so does one that made no tool call for ten minutes, and a re-resolve that changed a body forgets that name everywhere (§2.6). The loader is exempt from the default response budget so the body arrives verbatim; the exemption is bounded by the 16 KB per-skill hard limit, and it is the framework-owned loader only — a downstream tool that took the `skill` name is budgeted like any other.

Choose `eager` only when the body shapes the **first** call's parameters — a query language the agent has to write correctly before it has any result to learn from (`cypher_query`). Everything else, including all five framework-bundled skills, is `lazy`.

Both tiers carry the same `<!-- mcp-skill:NAME -->` marker, so the injection pass stays idempotent per `(skill, tool)` pair either way.

A skill that **declares** targets and finds every one of them unregistered is dropped entirely — no prompt route, no injection, not in `skill()`'s active set. It had no channel to the agent, and listing it told operators otherwise. Declared targets are the name-match tool when a tool by that name exists, plus every `references_tools` entry. A skill that declares no targets at all — no tool shares its name and `references_tools` is empty — is *not* dropped: it stays listed, `skill(name)` serves it, and it injects nowhere, exactly as it always has.

Opt-out per-skill (`auto_inject_hint: false`) for skills that are pure background context, not workflows. They stay reachable through `skill(name)` and `prompts/*`.

### 2.11 Size limits — anti-bloat

Skill size is bounded by the framework's lint and runtime limits:

| Threshold | Behavior |
|---|---|
| 4 KB per skill | load logs a warning ("consider splitting"); `skills-lint` warns |
| 16 KB per skill | load rejects the file — a hard error for a bundled skill, a `ParseWarning` for every other layer; `skills-lint` hard-fails |
| 64 KB total across all resolved skills | `Registry::finalise` logs a warning naming the total and the skill count. Nothing is dropped: which skills an operator runs is the operator's call, and a framework that silently unregistered some of them would be the worse failure. |

The session total is counted **at resolve time, over every resolved skill, both delivery tiers**. `lazy` changes when a body reaches the agent, not whether it can — one session may call `skill(name)` for every skill in the registry — so the worst case the limit bounds is the same as it was when every body shipped in the tool list. `ResolvedRegistry::total_body_bytes()` is the number.

Numbers are tunable; the principle is "force authors to keep skills tight as a design discipline." Prevents the "operator copies their entire onboarding doc into a skill" anti-pattern.

---

## 3. End-to-end scenarios

Six concrete `skills:` declarations. Scenarios A-E illustrate the schema mechanics; Scenario F shows kglite's actual three-deployment reality where the layered composition pays off.

### Scenario A — pure-current-MCP behavior (zero declaration)

```yaml
# Manifest with no `skills:` field
name: kglite-mcp-server
workspace:
  kind: github
```

Skills are disabled. `prompts/list` returns empty. Context cost is identical to today's MCP setup. **Existing kglite deployments work unchanged with zero edits.**

### Scenario B — library-bundled defaults only

```yaml
skills: true
```

Three characters. The framework loads:
- mcp-methods's bundled skills (methodology for `grep`, `read_source`, `list_source`, `github_discussions`, `repo_management`)
- The downstream binary's bundled skills (e.g. kglite-mcp-server's `cypher_query.md`, `graph_overview.md`, `save_graph.md`)

The agent's `prompts/list` shows ~8-10 skills, costs ~300-500 tokens. Each skill body loads on demand. **Every operator running kglite-mcp-server gets methodology for free without authoring anything.**

### Scenario C — project-local overrides + bundled defaults

```yaml
skills:
  - true
  - ./local-overrides/
```

```text
mcp-servers/
├── legal_mcp.yaml
├── legal_mcp.skills/                  # auto-detected project layer (top priority)
│   └── cypher_query.md                # legal-corpus-specific methodology, overrides bundled
└── local-overrides/                   # declared root-layer source
    └── graph_overview.md              # tweaks to the bundled version
```

Resolution per skill name:
- `cypher_query` → from `legal_mcp.skills/` (project layer wins)
- `graph_overview` → from `local-overrides/` (root layer entry 2, overrides bundled)
- `read_source`, `grep`, `list_source`, etc. → from bundled (root layer entry 1, the `true` token)

**Operator gets bundled defaults + per-deployment tweaks. Best of both worlds.**

### Scenario D — team-shared skills, no bundled

```yaml
skills:
  - ./local-overrides/             # per-deployment specifics
  - ~/mcp-servers/team-skills/     # team-wide shared library
```

No `true` in the list — bundled defaults are **not** loaded. The operator's team has curated their own methodology library at `~/mcp-servers/team-skills/` and wants only their content.

Useful for teams that disagree with the bundled defaults or want a strictly-curated skill set. Operator can always add `true` back to layer bundled defaults in as a fallback.

### Scenario E — explicit fallback chain

```yaml
skills:
  - ./local-overrides/             # this deployment's specifics
  - ~/mcp-servers/team-skills/     # team library
  - true                           # mcp-methods + downstream binary defaults as final fallback
```

Layered fallback: project beats team beats bundled. Operator gets:
- Bundled methodology for tools where they haven't customized
- Team-shared content where the team has agreed on conventions
- Project-specific tweaks for this deployment

This is the most-customized realistic shape. Most operators won't need it; available when they do.

### Scenario F — kglite's three-deployment operator (realistic production case)

The mcp-servers operator runs three kglite deployments with non-overlapping domain methodology:

```text
mcp-servers/
├── legal_mcp.yaml
├── legal_mcp.skills/                       # this deployment's specifics
│   └── canadian-citations.md               # operator's per-corpus tweaks
├── og_mcp.yaml
├── og_mcp.skills/
│   └── sodir-id-conventions.md
├── code_mcp.yaml
└── code_mcp.skills/
    └── mono-repo-conventions.md

skill-packs/                                 # domain skill-packs (authored by domain specialists)
├── legal/
│   ├── cypher_query.md                     # "filter by jurisdiction, citation network patterns"
│   ├── case-vs-statute-traversal.md
│   └── temporal-validity.md
├── og/
│   ├── cypher_query.md                     # "wells/fields/licenses, production aggregation level"
│   ├── well-license-chronology.md
│   └── reservoir-traversal.md
└── code/
    ├── cypher_query.md                     # "callers/callees, .module filtering"
    ├── read_code_source.md                 # "qualified_name resolution"
    └── code-tree-types.md
```

Each manifest declares the same composition pattern, just with different domain pack paths:

```yaml
# legal_mcp.yaml
skills:
  - true                                    # kglite-mcp-server's protocol-level bundled defaults
  - ../skill-packs/legal/                   # legal domain methodology

# og_mcp.yaml
skills:
  - true
  - ../skill-packs/og/

# code_mcp.yaml
skills:
  - true
  - ../skill-packs/code/
```

What each deployment's agent sees for `cypher_query`:

- **legal**: legal/cypher_query.md (domain pack wins over bundled). Methodology talks about jurisdictions, citations, statute-vs-case node types.
- **og**: og/cypher_query.md. Methodology talks about wells, fields, licenses.
- **code**: code/cypher_query.md. Methodology talks about callers/callees, `.module` filtering.

If any deployment needs further per-deployment tweaks (e.g. legal-canada has different citation conventions than legal-uk), the operator drops a file in `legal_mcp.skills/cypher_query.md` and it overrides both the pack AND the bundled default.

The bundled layer (`true`) provides protocol-level skills that ALL three servers share: how the `cypher_query` tool itself works as an MCP tool, the `FORMAT CSV` pattern, error-shape interpretation. **Authored once by kglite-mcp-server's maintainers, version-locked to the binary, identical across all three servers.** This is the load-bearing "domain-neutral" constraint from §2.3.

Authoring leverage breakdown for the operator:

- `cypher_query.md` protocol skill: shared by kglite, written once, ships with the binary
- `cypher_query.md` legal domain skill: written once, shared across legal-uk, legal-eu, legal-us
- `cypher_query.md` per-deployment tweak: only when the corpus has quirks worth surfacing

The same pattern composes for `graph_overview`, `save_graph`, `read_code_source`, etc. Three sources of methodology layered cleanly without duplication.

### Agent flow (any scenario with skills enabled)

1. **Boot.** Agent connects. `initialize` succeeds.
2. **`tools/list`** — kglite's existing tools (cypher_query, graph_overview, save_graph, read_code_source, plus framework tools). Unchanged from today.
3. **`prompts/list`** — N skill metadata entries from the resolved layer set. ~300-500 tokens for a typical deployment.
4. **User asks: "find all classes that depend on the orm module."**
5. **Agent picks up signal** from the skill descriptions. Calls `prompts/get?name=cypher_query`.
6. **Methodology loads** (~2-3k tokens of static markdown). Agent now knows the patterns, when to call graph_overview first, common gotchas.
7. **Agent executes** the tools (`repo_management`, then `graph_overview`, then `cypher_query`) — all stateful, graph stays in memory across calls.

### Operator experience

**Adding a new skill:** Drop `./local-overrides/wikidata-property-references.md` in the directory. Restart the MCP server. New skill appears in `prompts/list` on the next agent connection. Zero Rust code, zero Python code, zero protocol knowledge — just markdown.

**Editing methodology:** Edit the markdown file. Restart (or live with watch mode). Edits take effect immediately.

**Reviewing changes:** A PR that adds a skill is a markdown diff. Anyone on the team can review what the agent will see. No "let me boot the server and inspect its tool registry" loop.

**Cross-vendor sharing:** Operator can symlink any custom-path directory to `~/.claude/skills/`. The same markdown files work in Claude Code's native skill loader for development. When deployed as an MCP server, the agent reaches them via prompts. Same source-of-truth, two interfaces.

---

## 4. Position alongside Tool Search

The agent-skills literature heavily emphasizes context efficiency. A pre-Tool-Search MCP setup with 5 servers and 58 tools really did consume ~55k tokens for tool registries alone, and skills' per-skill ~100-token discovery cost was an order-of-magnitude improvement over that baseline.

But the landscape has shifted. Claude Code and the Claude API now ship **Tool Search** — a client-layer mechanism that defers loading individual tool schemas until the agent calls them. The ~85% context reduction skills are credited with is actually delivered by Tool Search, not by skills themselves.

This matters for the proposal's value proposition. **Skills-aware MCP is NOT primarily a context-efficiency win.** The honest framing:

- **Tool Search (client-layer, deployed today)** solves the tool-schema-context-bloat problem
- **Skills-aware MCP (this proposal)** solves the methodology-richness problem

The two are independent wins that compose. A deployment with both gets:
- Tool schemas loaded just-in-time when the agent needs them (Tool Search)
- Rich operator-authored methodology accessible on demand (this proposal)

### Concrete numbers for a typical deployment

Take a 13-tool MCP setup (kglite-mcp-server + Gmail/Calendar/Drive OAuth + IDE tools — roughly what a working Claude Code session sees today):

| Setup | Upfront token cost |
|---|---|
| Eager-loaded tool registry (pre-Tool-Search baseline) | ~2,750 tokens |
| Tool Search deferred names only (Claude Code today) | ~130 tokens |
| Tool Search + skills-aware MCP, `delivery: lazy` (the default) | ~300-400 tokens |
| Tool Search + skills-aware MCP, `delivery: eager` everywhere | ~500-700 tokens, and it scales with skills × referenced tools |

Tool Search alone saves ~95%. Skills-aware MCP **costs** methodology metadata on top, in exchange for accessible operator-authored guidance — and the two tiers decide how much.

The eager figure is the one that grows badly: a body is copied into *every* tool it references, so a cross-tool skill listed in five tools is paid five times, up front, in every session. kglite measured ~15-22 KB for a plain domain graph and ~60 KB for a code graph with recipes before 0.4.11 flipped the default.

The lazy figure is close to flat in the number of skills: each skill contributes its routing description once per target tool, and the bodies move only when the agent calls `skill(name)` — once per session per skill, and only for the skills it actually reaches for. The `skill` tool itself is one extra entry in the tool list.

The proposal is additive: it doesn't reduce Tool Search's already-low upfront cost; it adds a methodology layer alongside it.

### Where skills-aware MCP still matters

Five distinct wins, none of which Tool Search delivers:

1. **Methodology richness.** MCP tool descriptions are <200 chars by convention. There's no good place for "this is the FIRST STEP, always call repo_management first" or "if the result has >50 nodes, narrow with a label filter first." Skills land in that gap — full-paragraph guidance the agent can load on demand.

2. **Operator authoring story.** Tool Search doesn't help an operator who wants to teach the agent without writing Rust. Skills are markdown. A team lead writes a SKILL.md for "how we do code review" in 20 minutes; same in MCP-tool-form is a weekend.

3. **Library-bundled defaults.** Every operator running a downstream binary (kglite-mcp-server, …) gets methodology for the binary's tools out of the box via `skills: true`. Tool Search doesn't ship methodology; it just defers schemas.

4. **Cross-vendor reach.** Tool Search is currently a Claude-ecosystem feature. Other MCP clients may not implement it. For those clients, skills-aware MCP delivers both wins: cheap MCP-prompts-based registry AND methodology.

5. **Filesystem portability via symlinks.** Skills are markdown files. Operators can symlink the project layer (`<basename>.skills/`) into `~/.claude/skills/`. Same content, two interfaces — MCP prompts when the server is running, native skill loader when working from Claude Code directly. Tool Search has no equivalent portability story.

### The 50+ tools framing — what it actually means today

For an operator using Claude Code in 2026 with Tool Search active: **the 50+ tools problem is already solved at the client layer**. Skills don't need to solve it again. What skills add is value beyond that — methodology, authoring, portability — for which there is no Tool Search equivalent.

For agents without Tool Search (Cursor, Gemini CLI, custom MCP clients in older states, future ecosystems we don't yet see): the dual win is real, and skills-aware MCP delivers both layers (prompts-driven discovery + methodology) where Tool Search isn't available.

The proposal stands on its merits without needing context-efficiency claims to do load-bearing work.

---

## 5. What this preserves

The current MCP train is unchanged in behavior except for the additive new features:

- **kglite library dep** — `mcp-methods = "0.3"` still resolves to the same library code; kglite reads `Manifest::workspace`, `TrustConfig`, `ToolSpec`, etc. exactly as today. New `Manifest::skills` field is added but doesn't break anyone — it's opt-in via the YAML.
- **Pip wheel** — `pip install mcp-methods` keeps shipping the same `mcp-server` CLI on PATH. The CLI gains skill support but doesn't lose anything.
- **Existing tool registry** — operators using the manifest's `tools:` block (cypher / python / bundled-override) keep working. Skills are additive, not replacing.
- **JSON schema** — `Manifest::to_json()` gains a `skills` field but the rest of the shape is stable. Non-breaking addition under the existing JSON-shape stability guarantee.
- **Trust gates** — no changes. Skills are markdown that the agent reads; no new dynamic-code-execution path is introduced.
- **Path conventions** — skills paths resolve the same as `source_root:`, `workspace.root:`, `env_file:` (relative to manifest's parent dir). No new path semantics to learn.
- **Backwards compat** — every existing kglite deployment works unchanged. Operators with no `skills:` declaration see no behavior change.

The new features are purely additive:

- `Manifest::skills: SkillsSource` — new field (polymorphic value: bool, string, or list)
- `ALLOWED_TOP_KEYS` adds `"skills"`
- New MCP `prompts/list` + `prompts/get` endpoint handlers — already part of the MCP protocol, just unused before
- New filesystem walker for `<basename>.skills/` adjacent to the manifest
- Compile-time `include_str!` block for library-bundled skill defaults

## 6. What this doesn't solve

### 6.1 Cross-vendor portability of MCP tools

Same as before: kglite's Cypher engine runs in an MCP server, so its tools are reachable only from MCP-supporting clients (Claude, Claude Code, a handful of others). Skills-aware MCP doesn't widen this — it widens the *surface inside MCP*, not MCP's reach.

For the audience that needs the cypher tools, this is fine. They're using Claude already.

For an audience that wants the cypher tools in Codex CLI / Cursor / Gemini CLI — that audience doesn't exist yet for kglite. If it ever does, the question becomes whether kglite re-implements its tools as CLIs (with a daemon for state) or whether the broader ecosystem standardizes on something MCP-adjacent.

### 6.2 Stateful-workload constraints

The graph still has to fit in the server's memory; the server still needs to stay running across calls. These are inherent to the workload, not the protocol. Skills-aware MCP doesn't change them.

What it changes: the agent's surface for *how to use the tools that operate on that state* now includes rich methodology, not just JSON schemas. The state itself is unchanged.

### 6.3 The MCP tool registry context cost is Tool Search's job, not ours

Operators concerned about MCP tool-registry size should enable Claude Code's Tool Search (or its equivalent in other clients). This proposal does NOT add server-side lazy tool registration — it would duplicate Tool Search's already-superior client-layer mechanism. See §4 for the full position.

Skills-aware MCP focuses on what Tool Search doesn't address: methodology, operator authoring, library-bundled defaults, cross-vendor reach via filesystem-portable markdown.

### 6.4 Skill-content authoring conventions

This proposal makes it possible to author skills. It doesn't tell operators *how* to write good ones. Best practices for skill content (description triggers, instruction style, when to use scripts) need separate documentation. We can lean on the [Anthropic skill best-practices guide](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices) for now.

---

## 7. Comparison to the parallel CLI/skills train

| Dimension | Parallel CLI/skills train | Skills-aware MCP |
|---|---|---|
| **Code added to mcp-methods** | New crates: `mcp-rg`, `mcp-github`, `mcp-compact`, `mcp-list`, etc. ~300-500 LOC each. Plus skills library (markdown). | ~200-300 LOC of framework code: manifest schema + skills loader + MCP prompts handlers + bundled-skill embedding |
| **New crates.io entries** | 3-5 new published crate names | Zero — all in existing `mcp-methods` crate |
| **New maintenance surface** | Per-CLI: docs, tests, version coordination, platform-specific binaries | Manifest changes + framework changes; same release cadence |
| **Operator install steps** | `cargo install mcp-rg && symlink skills/ to ~/.claude/skills/` | `pip install mcp-methods` (already doing this), edit manifest |
| **Stateless workloads** (ripgrep, GitHub fetch) | Better fit — short-lived processes match the workload | Adequate fit via MCP tools — slightly more overhead than CLIs |
| **Stateful workloads** (kglite, graph queries) | Doesn't work — would need daemon (reinvents MCP) | Works — MCP's existing stateful pattern carries through |
| **Cross-vendor portability** | Yes for skills (works in Codex CLI, Cursor, etc.); CLIs are universal | No for tools (MCP-only); yes for skill content if symlinked to native skill dirs |
| **Operator authoring story** | Markdown skills + CLIs already exist | Markdown skills via MCP prompts; tools still need Rust |
| **Context efficiency for tools** | N/A — CLIs aren't registered as MCP tools | Delegated to client-layer Tool Search (already deployed in Claude Code) |
| **Context efficiency for methodology** | Yes — skill bodies load on demand | Yes — skill bodies load on demand via MCP prompts |
| **Affects kglite's library dep?** | No — purely additive | No — purely additive (new optional manifest field) |
| **Effort to ship 1.0** | ~2 weeks (3 CLIs + 4 skills + docs) | ~3-5 days (manifest changes + prompts handler + bundled-skill embedding + docs) |

### Both are additive — they're not exclusive

Critical point: **the two approaches don't conflict.** Both add features without changing the existing MCP train. We could ship skills-aware MCP first, then add CLIs later if a stateless-workload audience materializes. Or we could ship CLIs first if that's where the immediate operator pain is. Or both in parallel.

The strategic question isn't "which one?" — it's "which one delivers more value per unit of effort?"

### My read

**Skills-aware MCP delivers more value per unit of effort for our specific audience.** kglite is the deployed reality. They need stateful tool architecture. Their operators (and the mcp-servers operator) need richer methodology for those tools — that's the dominant operator-side pain.

The parallel CLI/skills train is more strategically positioned for *a future audience* that uses non-MCP clients. We have line-of-sight to that audience (Codex CLI users, Cursor users) but no concrete demand yet.

---

## 8. Implementation sketch (design-only, no commitments)

### 8.1 Schema types

The `skills:` field is polymorphic — bool, string, or list of (bool or string). Cleanest expression via serde's untagged enum:

```rust
/// One source of skills. Either the magic "library bundled" token,
/// or a filesystem path resolved against the manifest's parent dir.
#[derive(Debug, Clone)]
pub enum SkillSource {
    /// Library-bundled skills (from mcp-methods + the downstream binary).
    /// Represented in YAML as the boolean `true`.
    Bundled,
    /// Filesystem path. Conventions:
    ///   ./foo or foo  → relative to manifest's parent dir
    ///   ~/foo         → home-relative (POSIX `$HOME` expansion)
    ///   /foo          → absolute
    Path(PathBuf),
}

/// The parsed `skills:` field value.
#[derive(Debug, Clone)]
pub enum SkillsSource {
    /// `skills: false` or no declaration — skills disabled entirely.
    Disabled,
    /// One or more sources, in priority order (first wins per skill name).
    Sources(Vec<SkillSource>),
}

/// Loaded skill (post-resolution):
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub source: SkillProvenance,  // for debugging which layer this came from
}

#[derive(Debug, Clone)]
pub enum SkillProvenance {
    Project,                // from <basename>.skills/
    Bundled,                // from compiled-in library defaults
    Path(PathBuf),          // from an explicit declared path
}

// Manifest gains:
pub struct Manifest {
    // ... existing fields ...
    pub skills: SkillsSource,    // default: SkillsSource::Disabled
}
```

Manifest's `skills` is `SkillsSource::Disabled` by default. The build_skills parser populates it from YAML:

```rust
fn build_skills(raw: Option<&serde_yaml::Value>, yaml_path: &Path)
    -> Result<SkillsSource, ManifestError>
{
    match raw {
        None | Some(serde_yaml::Value::Null) => Ok(SkillsSource::Disabled),
        Some(serde_yaml::Value::Bool(false)) => Ok(SkillsSource::Disabled),
        Some(serde_yaml::Value::Bool(true)) => Ok(SkillsSource::Sources(vec![SkillSource::Bundled])),
        Some(serde_yaml::Value::String(s)) => Ok(SkillsSource::Sources(vec![SkillSource::Path(s.into())])),
        Some(serde_yaml::Value::Sequence(seq)) => {
            let sources = seq.iter().map(|item| match item {
                serde_yaml::Value::Bool(true) => Ok(SkillSource::Bundled),
                serde_yaml::Value::Bool(false) => Err(/* ... */),
                serde_yaml::Value::String(s) => Ok(SkillSource::Path(s.into())),
                _ => Err(/* ... */),
            }).collect::<Result<Vec<_>, _>>()?;
            Ok(SkillsSource::Sources(sources))
        }
        _ => Err(/* type error */),
    }
}
```

### 8.2 Manifest YAML reminder

The full schema range (already shown in §3 scenarios):

```yaml
# Disabled (default):
# (no skills declaration, or skills: false)

# Single bundled toggle:
skills: true

# Single path:
skills: ./local/

# Mixed list:
skills:
  - true
  - ./local-overrides/
  - ~/team-skills/
```

### 8.3 Skill resolution

Walks two layers in order, building a `HashMap<String, Skill>` keyed by skill name:

```rust
pub fn resolve_skills(yaml_path: &Path, manifest: &Manifest,
                     bundled_skills: &[BundledSkill]) -> Vec<Skill>
{
    let mut resolved: HashMap<String, Skill> = HashMap::new();

    // Walk root layer (manifest's `skills:` sources), latest declaration order.
    // We walk in REVERSE so that earlier entries overwrite later ones.
    if let SkillsSource::Sources(sources) = &manifest.skills {
        for source in sources.iter().rev() {
            let skills = match source {
                SkillSource::Bundled => load_bundled_skills(bundled_skills),
                SkillSource::Path(p) => load_skills_from_dir(&resolve_path(p, yaml_path)),
            };
            for skill in skills {
                resolved.insert(skill.name.clone(), skill);
            }
        }
    }

    // Project layer (auto-detected, top priority).
    // Overwrites any matching entries from the root layer.
    let project_dir = project_skills_dir(yaml_path);  // <basename>.skills/
    if project_dir.is_dir() {
        for skill in load_skills_from_dir(&project_dir) {
            resolved.insert(skill.name.clone(), skill);
        }
    }

    resolved.into_values().collect()
}

fn resolve_path(p: &Path, yaml_path: &Path) -> PathBuf {
    if p.is_absolute() { p.to_path_buf() }
    else if p.starts_with("~/") {
        let home = std::env::var("HOME").expect("$HOME unset");
        Path::new(&home).join(p.strip_prefix("~/").unwrap())
    } else {
        yaml_path.parent().unwrap().join(p)
    }
}
```

### 8.4 Project layer auto-detection

For a manifest at `mcp-servers/legal_mcp.yaml`, the project skills directory is `mcp-servers/legal_mcp.skills/`. Naming convention:

```rust
fn project_skills_dir(yaml_path: &Path) -> PathBuf {
    let basename = yaml_path.file_stem().unwrap();
    let parent = yaml_path.parent().unwrap();
    parent.join(format!("{}.skills", basename.to_string_lossy()))
}
```

If the directory exists, walk `*.md` files; each becomes a skill. Files whose `name` field (in YAML frontmatter) matches a registered tool name auto-attach as that tool's deep-methodology surface; others are standalone.

### 8.5 Library-bundled skill embedding + frontmatter

Compile-time inclusion via `include_str!`. Skills carry their own validated frontmatter at compile time via a `include_skill!` macro (helper #2):

```rust
// In crates/mcp-methods/src/server/bundled_skills.rs:
use mcp_methods::skill::include_skill;

pub fn library_bundled_skills() -> Vec<BundledSkill> {
    vec![
        include_skill!("grep", "bundled_skills/grep.md"),
        include_skill!("github_discussions", "bundled_skills/github_discussions.md"),
        include_skill!("read_source", "bundled_skills/read_source.md"),
        include_skill!("list_source", "bundled_skills/list_source.md"),
        include_skill!("repo_management", "bundled_skills/repo_management.md"),
    ]
}
```

The `include_skill!` proc-macro reads the file at compile time, parses frontmatter, validates against a const tool catalogue (registered tools known at framework-build time), and embeds bytes. A malformed skill — bad frontmatter, broken `applies_to` semver, references to a tool that doesn't exist at compile time — **fails the build**, not the boot.

Downstream binaries (kglite-mcp-server) add their own bundled skills the same way and compose via the `SkillRegistry` builder (§8.6).

### 8.6 The `SkillRegistry` builder API (helper #1)

The core API kglite-mcp-server (and any future downstream binary) consumes. ~10 LOC of glue in their `main.rs`:

```rust
use mcp_methods::skill::{Registry, include_skill};

let skills = Registry::new()
    .add_bundled(include_skill!("cypher_query",     "skills/cypher_query.md"))
    .add_bundled(include_skill!("graph_overview",   "skills/graph_overview.md"))
    .add_bundled(include_skill!("save_graph",       "skills/save_graph.md"))
    .add_bundled(include_skill!("read_code_source", "skills/read_code_source.md"))
    .merge_framework_defaults()          // mcp-methods's bundled skills
    .layer_dirs(&manifest.skills)?       // operator's `skills:` list
    .finalise()?;                        // parses frontmatter, runs lint, resolves layering
```

`Registry`'s internal pipeline:

```rust
pub struct Registry { /* ... */ }

impl Registry {
    pub fn new() -> Self;

    /// Add a compile-time-validated bundled skill (from include_skill!).
    pub fn add_bundled(mut self, skill: BundledSkill) -> Self;

    /// Add the framework's own bundled defaults (ripgrep, github_discussions, etc.).
    pub fn merge_framework_defaults(mut self) -> Self;

    /// Walk the manifest's `skills:` list, loading each declared source in order.
    /// Returns errors for unreadable paths or malformed frontmatter.
    pub fn layer_dirs(mut self, source: &SkillsSource) -> Result<Self, SkillError>;

    /// Parse all frontmatter, run lint (size limits, frontmatter validation,
    /// reference checks), resolve layered conflicts. Returns the resolved
    /// skill set ready for MCP prompts wiring.
    pub fn finalise(self) -> Result<ResolvedRegistry, SkillError>;
}
```

`ResolvedRegistry` exposes the resolved skill set and is what `serve_prompts` consumes:

```rust
pub struct ResolvedRegistry {
    skills: HashMap<String, Skill>,
    /// Per-skill provenance, for the boot-time collision log (§2.9).
    provenance: HashMap<String, Vec<SkillProvenance>>,
    /// Frontmatter-derived predicates that need evaluation at boot.
    predicates: HashMap<String, Vec<SkillPredicate>>,
}

impl ResolvedRegistry {
    pub fn skill_names(&self) -> impl Iterator<Item = &str>;
    pub fn get(&self, name: &str) -> Option<&Skill>;
    pub fn provenance_for(&self, name: &str) -> &[SkillProvenance];
}
```

### 8.7 MCP prompts wiring — `serve_prompts` (helper #3)

The framework owns the MCP protocol surface for `prompts/list` and `prompts/get`. Downstream binaries just hand over their `ResolvedRegistry`:

```rust
use mcp_methods::skill::serve_prompts;
use mcp_methods::server::McpServer;

let mut server = McpServer::new(options);
// ... register tools ...
serve_prompts(&resolved_skills, &mut server);
```

`serve_prompts` registers `prompts/list` and `prompts/get` handlers on the MCP server. Implementation detail: it uses rmcp's `PromptRouter` alongside the existing `ToolRouter`. The handlers iterate the registry, return name + description for `prompts/list`, and return the full skill body for `prompts/get?name=X`.

If the skill's frontmatter declared `auto_inject_hint: true` (default) AND the skill's name matches a registered tool, `serve_prompts` also walks the tool router and injects a one-line "see prompts/get NAME for full methodology" pointer into the tool's description (§2.10).

### 8.8 Pluggable predicate evaluator (helper #4, Phase 2)

For `applies_when:` predicates that need runtime state the framework can't see — `graph_has_node_type` requires kglite's active graph — downstream binaries implement a trait:

```rust
pub trait SkillPredicateEvaluator: Send + Sync {
    /// Evaluate a domain predicate. Return None if the predicate is unknown
    /// to this evaluator (framework falls back to the conservative answer).
    fn evaluate(&self, predicate: &SkillPredicate) -> Option<bool>;
}

// kglite-mcp-server implementation:
struct GraphAwareEvaluator(Arc<RwLock<GraphState>>);

impl SkillPredicateEvaluator for GraphAwareEvaluator {
    fn evaluate(&self, predicate: &SkillPredicate) -> Option<bool> {
        match predicate {
            SkillPredicate::GraphHasNodeType(types) => {
                let graph = self.0.read().unwrap();
                Some(types.iter().all(|t| graph.has_node_type(t)))
            }
            SkillPredicate::GraphHasProperty { ty, prop } => {
                let graph = self.0.read().unwrap();
                Some(graph.has_property(ty, prop))
            }
            // Other predicate types fall through to None — framework dispatches them.
            _ => None,
        }
    }
}
```

Wiring:

```rust
let evaluator = Arc::new(GraphAwareEvaluator(graph_state.clone()));
let resolved = Registry::new()
    /* ... */
    .with_predicate_evaluator(evaluator)
    .finalise()?;
```

The framework dispatches built-in predicates itself; domain predicates go through the evaluator. Phase 1 ships without `applies_when:` evaluation (all skills load if they pass version checks); Phase 2 wires this up.

### 8.9 CLI subcommand kit (helper #5)

Framework exposes its skill-related CLI commands as composable `clap` subcommands. Downstream binaries opt in by composition:

```rust
use clap::Command;
use mcp_methods::cli;

let app = Command::new("kglite-mcp-server")
    .subcommand(cli::skills_lint())   // walks paths, validates frontmatter, checks references
    .subcommand(cli::skills_list())   // shows all resolved skills + provenance
    .subcommand(cli::skills_show());  // dumps a specific skill's rendered body
```

Operators get `kglite-mcp-server skills-lint ./my-skills/`, `... skills-list`, `... skills-show cypher_query` for free. Framework owns the implementation; downstream owns the binary entry point. Same pattern that lets downstream binaries not reimplement `read_source` / `grep` / etc.

### 8.10 Python pyo3 coverage (helper #6)

The `mcp-methods-py` wheel exposes the skill APIs to Python consumers via pyo3 wrappers, mirroring how the existing `Manifest`, `Workspace`, `start_watch` are surfaced today:

```python
# In kglite's Python boot path:
from kglite import _mcp_internal as mcp

registry = mcp.SkillRegistry.from_manifest(manifest_path)
mcp.serve_prompts(registry, server)
```

`Skill`, `Registry`, `lint()`, `evaluate_predicate()` all wrapped. Kglite's Python entry-point binary (`kglite-mcp-server` via console-script) gets the same skill support as the Rust binary path.

### 8.11 New tests

**Resolution + composition:**
- `skills_disabled_by_default` — no `skills:` field → no prompts registered
- `skills_bool_true_loads_bundled` — `skills: true` → library-bundled skills loaded
- `skills_path_string_loads_directory` — `skills: ./local/` → that directory walked
- `skills_list_polymorphic` — `skills: [true, ./local/, ~/team/]` → all sources walked in order
- `skills_project_layer_auto_detected` — `<basename>.skills/` adjacent → top-priority overrides
- `skills_project_overrides_root` — same skill name in both layers → project wins
- `skills_root_first_wins` — multiple root sources with same skill name → earlier in list wins
- `skills_full_file_replacement` — different content in same-named layer files → later layer wins entirely (no merging)

**Frontmatter validation (§2.7):**
- `applies_to_semver_mismatch_warns` — skill's `applies_to` doesn't match running binary → warning at boot
- `references_unknown_tool_warns` — skill references a tool not in the registry → warning
- `references_unknown_argument_warns` — skill references an argument not in tool's schema → warning
- `malformed_frontmatter_rejected` — invalid YAML in frontmatter → parse error

**Size limits (§2.11):**
- `skill_over_4kb_warns` — lint warning
- `skill_over_16kb_hard_fails` — lint error
- `session_total_over_64kb_drops` — framework refuses to register beyond limit

**Auto-injected hints (§2.10):**
- `auto_inject_hint_default_true` — skill name matches tool → tool description gets the pointer
- `auto_inject_hint_false_disables` — frontmatter opts out

**Collision logging (§2.9):**
- `collision_log_lists_candidates` — multiple layers, log shows all candidates + winner

**Predicate evaluator (§2.8, Phase 2):**
- `framework_predicate_dispatches_internally` — `tool_registered` evaluated without evaluator trait
- `domain_predicate_falls_through_to_evaluator` — `graph_has_node_type` reaches the trait
- `predicate_unknown_to_evaluator_falls_back` — `None` from trait → conservative answer
- `applies_when_false_omits_skill` — skill not in `prompts/list` when predicate fails

**`SkillRegistry` builder API:**
- `add_bundled_validates_at_compile_time` — `include_skill!` compile-time errors caught
- `layer_dirs_walks_in_order` — multi-path declaration order matters
- `finalise_runs_lint_before_returning` — frontmatter errors surface at finalise

**MCP protocol wiring:**
- `prompts_list_returns_skill_metadata` — MCP protocol response
- `prompts_get_returns_skill_body` — MCP protocol response

### 8.12 New docs sections

- Sphinx site: `docs/guides/skills-aware-manifests.md`
- README: small section under "What's included"
- `docs/explanation/`: maybe `progressive-disclosure-in-mcp.md` explaining the design rationale

---

## 9. Open questions

Most questions resolved through the kglite reply (2026-05-14); one verification gate remains; two minor design questions worth resolving in the spike or Phase 1 implementation.

### 9.1 Resolved through design + kglite reply

- **Skills vs the existing `instructions:` field.** Complementary. `instructions:` is always-loaded baseline content (in the system prompt). Skills are on-demand catalog content. One blob vs many addressable units. (Resolved in design.)
- **Dynamic content / templating in skills.** Rejected. Static markdown only. (Resolved in design; kglite explicitly endorsed.)
- **Composition with the bundled-override pattern (0.3.34).** Tool description stays short via `description:`; deeper methodology lives in a same-named skill file. Agent sees both surfaces. (Resolved in design.)
- **Does kglite have operator-authoring pain?** ✓ Confirmed yes. They've been stuffing methodology into the `instructions:` blob because there's nowhere better. The 0.9.30 batch-load hint is literally this failure mode. (Resolved by kglite reply.)
- **Would kglite ship bundled skills with kglite-mcp-server?** ✓ Yes, 4 skills (`cypher_query`, `graph_overview`, `save_graph`, `read_code_source`) — ~1-2 days of authoring lift. (Resolved by kglite reply.)
- **JSON shape — content or references in `to_json()`?** ✓ References in `to_json()`, content via separate accessor (`Manifest::skill_content(name)`). Plus an `origin` field for provenance (`bundled` / `operator` / `user-shared`). (Resolved by kglite reply.)
- **Library-bundled skills: domain-neutral?** ✓ Yes — load-bearing constraint. Bundled teaches the TOOL, not the GRAPH. (Resolved by kglite reply, §2.3.)
- **Three-layer vs two-layer composition?** ✓ Three. Project / domain-pack / bundled. The domain-pack layer is first-class because kglite's operator runs three deployments (legal / o&g / code) with non-overlapping domain methodology. (Resolved by kglite reply, §2.2.)

### 9.2 (Verification gate, still open) MCP prompts ecosystem support

[The MCP spec](https://modelcontextprotocol.io/specification) defines prompts as a first-class concept, but most servers ignore them. If most clients don't implement `prompts/list` and `prompts/get`, the skill surface is invisible to those clients.

**Verification needed before Phase 1:** Read rmcp's client code (or test with a stub server) to confirm Claude Code, Claude API, and the major MCP clients support prompts. At minimum Anthropic's own should (they defined the spec). If wider ecosystem support is patchy, the proposal still works for our deployed audience (kglite operators all use Claude/Claude Code) but cross-client value is narrower.

One-hour spike. Doable in a single session.

### 9.3 (Minor design Q) Granularity of `applies_to:` semver

kglite proposed per-skill semver constraints on both `mcp_methods` and the downstream binary's version:

```yaml
applies_to:
  mcp_methods: ">=0.3.35, <0.4"
  kglite_mcp_server: ">=0.9.30, <0.10"
```

Considered: is this over-engineering for the small number of skills any deployment will have? Could a coarser per-skill `version:` string suffice ("0.3.35+ required")?

**Lean toward kglite's proposal as-is.** Real semver ranges are more honest about compatibility windows. The implementation cost is low (one `semver::VersionReq` parse per skill at boot). The cost of being wrong is real (operators run incompatible skill packs and get warnings instead of useful info).

### 9.4 (Minor design Q) `references_arguments:` validation coupling

kglite's `references_arguments:` field walks the skill's frontmatter against each referenced tool's input schema and warns on unknown argument names. The cost: the skill author has to know the exact argument names of the tool — coupling that's currently absent (tool descriptions live in code; skills live in markdown).

Considered: is this validation worth the coupling? If a tool renames an argument (e.g. cypher_query renames `format` to `output_format`), the skill suddenly emits warnings — but that's good (staleness signal), not bad.

**Lean toward keeping the validation.** The coupling is the point — it's how the framework catches stale skills when tool surfaces evolve. The warnings are advisory (boot doesn't fail), so the cost is bounded.

---

## 10. Recommendation

**Verification gates status (2026-05-14):**

1. **Does kglite see operator-authored methodology as a real need?** ✓ **Confirmed yes.** Their reply: "We've been stuffing methodology into three places of decreasing fitness" — `tools[].bundled.description:`, manifest `instructions:` block, `overview_prefix:`. The 0.9.30 batch-load hint is literally methodology injected into `instructions:` because there's nowhere better. They committed to ~1-2 days of authoring lift for 4 bundled skills.

2. **Do the agent runtimes our operators use support MCP prompts?** ⏳ **Verification still pending.** One-hour spike before Phase 1: read rmcp's client code or test `prompts/list` + `prompts/get` against a Claude Code stub.

**Skills-aware MCP is the locked design.** Three structural reasons:

- It directly addresses the operator pain kglite confirmed (rich agent-facing methodology, currently constrained to short tool descriptions and the global `instructions:` blob)
- It preserves the stateful-workload architecture kglite needs — no CLI daemon reinvention
- The methodology + authoring + portability story is value Tool Search cannot deliver — this is additive to existing client-layer optimizations, not redundant

The three-layer composition (project → domain → bundled) was added to the design based on kglite's three-deployment reality. Domain-neutral bundled constraint, versioned frontmatter, predicate evaluator, CLI subcommand kit, pyo3 coverage, `SkillRegistry` builder, `serve_prompts` wiring — all confirmed by kglite as helper APIs the framework should absorb so downstream binaries (kglite-mcp-server first, others later) don't reimplement them.

**Next concrete step:** complete the one-hour MCP prompts verification spike, then ship Phase 1 as 0.3.35 once kglite's 0.9.30 lands.

### Sequencing once verified

Updated based on kglite's feedback. Total Phase 1: ~7-10 days of framework work.

**Phase 1 — Skills-aware MCP foundation (~7-10 days, ship as 0.3.35):**

Day 1-2: **Schema + parsing**
- Manifest parser for polymorphic `skills:` field (serde untagged enum: bool / string / list of bool-or-string)
- Path resolution (relative-to-manifest, `~/`, absolute) reusing existing manifest path conventions
- Versioned frontmatter parser (`applies_to`, `references_tools`, `references_arguments`, `references_properties`, `auto_inject_hint`)
- Boot-time frontmatter validation with warnings (NOT errors) for staleness

Day 3-4: **`SkillRegistry` builder API**
- `Registry::new()`, `.add_bundled()`, `.merge_framework_defaults()`, `.layer_dirs()`, `.finalise()` chain
- Three-layer resolution: project → domain → bundled
- Full-file replacement (no merging) per §2.5
- Loud collision logging via `tracing::info!` at boot
- Size-limit enforcement (4 KB warn / 16 KB fail / 64 KB session)

Day 5: **MCP `prompts/list` + `prompts/get` wiring**
- `serve_prompts(registry, server)` API
- rmcp `PromptRouter` integration alongside the existing `ToolRouter`
- Auto-inject discoverability hints into tool descriptions when `auto_inject_hint: true`

Day 6: **CLI subcommand kit**
- `cli::skills_lint()`, `cli::skills_list()`, `cli::skills_show()` as composable clap subcommands
- Lint walks paths, validates frontmatter, checks tool/argument references, enforces size limits
- Operators run `kglite-mcp-server skills-lint ./skills/` in their CI

Day 7-8: **Library-bundled skill embedding + `include_skill!` macro**
- Proc-macro that reads at compile time, parses frontmatter, validates against a const tool catalogue, embeds bytes
- 5-6 starter skills shipped in `mcp-methods`'s `bundled_skills/`: methodology for `grep`, `read_source`, `list_source`, `github_discussions`, `repo_management`
- Compile-time errors for malformed or stale skills (the kglite "we changed cypher_query but forgot the skill" failure mode caught at the earliest possible point)

Day 9-10: **Python pyo3 wrapper coverage + docs**
- `mcp-methods-py` exposes `SkillRegistry`, `Skill`, `lint`, `evaluate_predicate` via pyo3
- kglite's Python boot path can call `mcp.SkillRegistry.from_manifest()` + `mcp.serve_prompts()`
- Sphinx docs: `docs/guides/skills-aware-manifests.md`, `docs/explanation/three-layer-composition.md`
- README: small new section under "What's included"

**Phase 2 — `applies_when:` predicates (deferred, ~3-4 days when demand surfaces):**
- `SkillPredicateEvaluator` trait + framework-internal predicate dispatch
- Wiring for downstream binaries to register a custom evaluator (kglite plugs in graph-aware predicates)
- Frontmatter parsing for the predicate set (`graph_has_node_type`, `graph_has_property`, `tool_registered`, `extension_enabled`, version predicates)
- Skills with unsatisfied predicates omitted from `prompts/list`

Defer to Phase 2 because: kglite's reply explicitly said Phase 1 doesn't need this; predicate evaluator is complex; the value depends on operators actually authoring predicate-gated skills, which won't happen until Phase 1's bundled+project layers are deployed first.

**Phase 3 — Operator-driven content (no framework work, kglite-side):**
- kglite-mcp-server ships ~4 bundled skills (`cypher_query.md`, `graph_overview.md`, `save_graph.md`, `read_code_source.md`) per kglite's commitment in their reply. ~1-2 days of authoring on their side.
- Domain skill-pack authoring (`kglite-skills-legal`, `kglite-skills-og`, `kglite-skills-code`) — owned by the mcp-servers operator, NOT kglite or mcp-methods. Operators ship these when they have time. Could be community packages over time.
- Other downstream binaries adopt the same pattern on their own timelines.

**Release shape:**
- Phase 1 ships as 0.3.35 (or higher patch — single release)
- Phase 2 ships as a follow-up patch (0.3.36 or similar) once a downstream consumer signals they need predicates
- Phase 3 has no framework release attached; each downstream binary ships on its own line

**Coordination with kglite:**
- They're mid-cycle on 0.9.30 (port collision + workspace.applies_to + bundled rename). Don't add coordination overhead during that.
- After 0.9.30 lands, send them: (a) a draft of the `SkillRegistry` builder API surface for review, (b) one worked bundled skill example (e.g. `ripgrep.md` or `read_source.md`) so they can see the shape concretely, (c) our reaction to their design asks #1 (versioning), #4 (skill-packs), #5 (applies_when) since those are load-bearing for their multi-deployment scenario.
- They explicitly committed to ~1-2 days of authoring lift + ~10 LOC of `main.rs` wiring once Phase 1 lands.

The 7-10 day framework-side estimate is conservative — kglite's helper-API absorbing means we do the infrastructure work once, every downstream binary benefits.

---

## 11. Cross-references

- [`agent-cli-pattern.md`](./agent-cli-pattern.md) — companion doc on the broader CLI/skills shift; defines the parallel-train option this doc compares against
- The implementation phasing in `agent-cli-pattern.md §9` would need revision if this approach is preferred — skills-aware MCP would slot into Phase 0/1, and the parallel CLI/skills train would move to "future state if a stateless-workload audience emerges"

---

## Sources

- [Equipping agents for the real world with Agent Skills (Anthropic engineering)](https://www.anthropic.com/engineering/equipping-agents-for-the-real-world-with-agent-skills) — progressive disclosure rationale
- [Agent Skills overview (Claude API docs)](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview) — SKILL.md spec
- [MCP Specification](https://modelcontextprotocol.io/specification) — prompts/list, prompts/get protocol methods
- [Morph LLM: Claude Code Skills vs MCP vs Plugins](https://www.morphllm.com/claude-code-skills-mcp-plugins) — Tool Search context-reduction data (~85%)
- [Anthropic skill best-practices guide](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/best-practices) — authoring conventions
- [Simon Willison: Claude Skills are awesome](https://simonwillison.net/2025/Oct/16/claude-skills/) — context-efficiency argument for skills
