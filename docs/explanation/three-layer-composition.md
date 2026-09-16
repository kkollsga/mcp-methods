# Skill Layer Composition

Skills resolve through five layers — **project → domain pack → manifest-inline → owned → bundled defaults**. Each layer has a distinct authoring home, and higher layers fully replace same-named entries in lower layers. This page explains what each layer is for, and when it is the right place to author.

Three of the five carry the weight: bundled, domain pack, project. Those are the three authorship roles — framework author, library author, deployment author — and a skill that does not obviously belong to one of the other two belongs to one of them. The inline and owned layers exist for two narrower cases, described below.

## The layers

### Layer 1 — Bundled defaults

Compile-time skills shipped inside a binary's `.so` / executable. Two sources:

- **Framework** — the five SKILL.md files in `mcp-methods` (`grep`, `read_source`, `list_source`, `github_issues`, `repo_management`). All five ship with `mcp_methods::server::library_bundled_skills()`; the first three cover always-registered framework tools, while `github_issues` and `repo_management` carry `applies_when: tool_registered:` gates and surface only in sessions where their tool actually registered. (The gate is evaluated by the Rust `serve_prompts` path; the Python FastMCP helper registers skills unconditionally — see its module docs.)
- **Downstream binary** — a domain binary (e.g. `kglite-mcp-server`) can add its own bundled skills via `Registry::add_bundled_many(...)` before finalising. These ship inside the binary; operators don't see them as files.

When to put a skill here:
- The methodology is **canonical and stable** — it applies to every deployment of this binary.
- The binary author maintains it.
- It documents a tool that ships with the binary.

When **not** to put a skill here:
- The methodology is deployment-specific (legal corpus vs. open source corpus).
- The methodology references config or env vars that vary across deployments.
- An operator might want to tweak it for their domain.

### Layer 2 — Owned (runtime-supplied bodies)

Skill bodies a host binary hands to `Registry::add_layer` at boot, assembled from whatever it reads — a graph file, a database row, a downloaded pack. No file on disk, no compile-time `include_str!`.

When to put a skill here:
- The methodology belongs to an **artefact**, not to a deployment: the data the server serves should teach its own use, and should keep teaching it when the same artefact is served somewhere else.

Owned skills are switched on by the same `true` marker as the bundled layer — they are, from the operator's side, another thing the binary supplies. A malformed owned body is a parse warning, not a boot failure: it is data assembled at runtime, and one bad row must not deny service.

### Layer 3 — Manifest-inline

Mapping entries in the manifest's `skills:` list. The body lives in the YAML:

```yaml
skills:
  - true
  - name: house_style
    description: How this deployment names and cites things.
    body: |
      # House style

      Cite by paragraph, never by page.
```

When to put a skill here:
- The methodology is **a few lines long** and specific to this deployment, so a separate file is more bookkeeping than content.
- You want the skill to travel with the manifest as one artefact — one file to copy, one file to review.

Reach for the project layer instead as soon as the body wants its own headings, examples and edit history. An inline entry is subject to the same 16 KB hard limit as a file, and a manifest that has grown several of them is telling you they want to be files.

Every inline entry joins this one layer wherever it sits in the list, so reordering the list never changes what overrides what. A same-named file in a declared directory still wins, which is how an operator overrides an inline skill without editing the manifest.

### Layer 4 — Domain pack

Operator-declared directories listed in the manifest's `skills:` field. Each entry is a path to a directory of SKILL.md files:

```yaml
skills:
  - true                      # include bundled defaults
  - ~/shared-mcp-skills/      # shared across operator's deployments
  - ./vendor-skills/          # vendored from a domain library
```

When to put a skill here:
- The methodology is **shared across multiple deployments** but maintained by the operator (not the binary author).
- A domain library (e.g. a CAD/EDA-specific skill pack) ships skills as a versioned bundle.
- You want the skill in version control alongside your manifest, but separate from a single deployment.

Domain packs let you reuse methodology across a fleet without forking the upstream binary.

### Layer 5 — Project layer

Auto-detected `<manifest_basename>.skills/` directory adjacent to the manifest YAML. For a manifest at `mcp-servers/legal_mcp.yaml`, the project layer lives at `mcp-servers/legal_mcp.skills/`.

When to put a skill here:
- The methodology is **specific to this single deployment** — the legal corpus, this team's workflow, this repo's conventions.
- You want to override a bundled or domain-pack skill with a custom version for just this server.
- It's the most natural authoring home for tweaks that won't be shared.

The project layer is the top-priority layer: a project-layer `cypher_query.md` masks the bundled `cypher_query.md` and any domain-pack version. This is by design — operators always win.

## Why the three authorship layers, and not fewer

### One layer is too few

If everything had to live in one place, you'd have two bad choices:

- **All bundled** — operators can't author anything without modifying the binary. Fork-and-rebuild for every methodology tweak. Bundled becomes a versioning nightmare; operator wishes block on upstream releases.
- **All operator-authored** — every deployment ships with empty methodology by default. The framework's known-good defaults (`grep` methodology, etc.) get re-invented per operator, badly.

Splitting framework defaults from operator authoring is the obvious first cut.

### Two layers is *almost* enough but misses the sharing case

With just "bundled" and "project," there's no good home for skills that are:
- Operator-authored (not bundled)
- But shared across many deployments (not project-specific)

Domain packs fill this hole. They let an operator maintain a single shared library and reference it from multiple manifests. Without that layer, you'd either copy-paste or hack the project layer (e.g. symlink a shared dir into every `<name>.skills/`) — both fragile.

### An "org" or "vendor" layer buys you nothing

The cardinality of *authorship* layers should match the cardinality of authorship roles, not deployment hierarchy — and authorship splits cleanly into framework-author / library-author / deployment-author. A layer per rung of an org chart would only add collision rules and places to look when a skill behaves unexpectedly.

The inline and owned layers are not counter-examples: neither adds an authorship role. Inline is the deployment author again, writing in the manifest instead of a file. Owned is the framework's way of letting an artefact carry methodology, authored by whoever authored the artefact. Both slot beneath the layer whose author could overrule them.

## Resolution rules

The resolver walks layers from highest to lowest priority:

1. Project layer entries (highest)
2. Domain pack entries — in declaration order (the first pack to mention a name wins among packs)
3. Manifest-inline entries — one layer, regardless of list position
4. Owned entries from `Registry::add_layer` — later calls win over earlier ones
5. Bundled defaults — both framework and downstream-binary contributions (lowest)

For each name encountered, the first occurrence wins; subsequent occurrences from lower-priority layers are masked. Masking is logged at `INFO` level via `tracing` so operators can see "your project `grep` skill overrode the bundled `grep` at boot" in their logs.

**No merging.** A higher-priority skill is a *full replacement* of the same-named lower-priority skill — not a diff or overlay. This is intentional: merging markdown bodies is a guaranteed source of confused output ("did this paragraph come from the override or the original?"). Full replacement keeps the model simple and the behaviour predictable.

## Collision-logging

When a name appears in multiple layers, the resolver logs at INFO level:

```
skill `grep` resolved from project (mcp-servers/legal_mcp.skills/grep.md);
  masked: bundled (framework default)
```

Operators see this once at boot, never on subsequent `prompts/get` calls (resolution happens at startup). The log line is enough to debug "I authored a project skill but the agent's still getting the old methodology" — they look at the boot log and see whether their skill won.

## Authoring decision tree

Use this when you're unsure where to put a new skill:

1. **Does it apply to every deployment of this binary?** → Bundled (framework or binary author).
2. **Does it belong to the artefact the server serves rather than to any deployment?** → Owned, via `Registry::add_layer`.
3. **Does it apply to many deployments but not all, and someone other than the binary author owns it?** → Domain pack.
4. **Is it specific to this one deployment, or does it override something for just this server?** → Project layer — or, if it is only a few lines, inline in the manifest.

If you find yourself answering #3 but the methodology might be useful to other operators, that's a signal to upstream it into a domain pack or contribute it to the bundled defaults — but the project layer is always a fine starting place.

## See also

- [Authoring Skills](../guides/authoring-skills.md) — file format and how-to
- [Manifest Schema](../reference/manifest-schema.md#skills-polymorphic-value) — the `skills:` declaration
- [Architecture](architecture.md) — where skills fit in the framework's boot sequence
