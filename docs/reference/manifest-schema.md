# Manifest Schema Reference

The YAML manifest accepted by `mcp-server` and all downstream binaries. Strict unknown-key validation: any top-level key not listed below is rejected at boot.

## Top-level keys

| Key | Type | Required | Default | Description |
|---|---|---|---|---|
| `name` | string | no | `"MCP Server (<mode>)"` | Server display name (surfaced via MCP `initialize`) |
| `instructions` | string | no | unset | Multi-line guidance sent to the agent |
| `overview_prefix` | string | no | unset | Prefix prepended to `graph_overview` output (downstream-binary use only) |
| `source_root` | string | no\* | unset | Single source-root directory (alias for `source_roots: [PATH]`) |
| `source_roots` | list&lt;string&gt; | no\* | `[]` | List of source-root directories |
| `trust` | object | no | `{...false}` | Advisory trust gates — see below |
| `tools` | list&lt;object&gt; | no | `[]` | Manifest-declared tools — see below |
| `embedder` | object | no | unset | Embedder loader config (downstream-binary use) |
| `builtins` | object | no | `{...defaults}` | Builtin-tool behaviour switches |
| `env_file` | string | no | walk-up `.env` | Path to `.env` file (relative to YAML or absolute) |
| `workspace` | object | no | unset | Workspace mode declaration — see below |
| `extensions` | object | no | `{}` | Opaque passthrough for downstream-binary-specific config |
| `skills` | bool / string / list | no | `false` | Operator-authored methodology exposed via MCP `prompts/*` — see below |

\* `source_root` and `source_roots` are mutually exclusive — specifying both is a validation error.

## `trust:` object

| Key | Type | Default |
|---|---|---|
| `allow_python_tools` | bool | `false` |
| `allow_embedder` | bool | `false` |

Unknown keys under `trust:` are rejected.

## `tools:` list

Each entry is an object with these keys:

| Key | Type | Required | Notes |
|---|---|---|---|
| `name` | string | yes | Tool name; must be unique within the server |
| `description` | string | no | Tool description surfaced to the agent |
| `parameters` | object | no | JSON Schema for tool arguments |
| `cypher` | string | one of | Cypher query body (for `kind = cypher`) |
| `python` | string | one of | Path to Python module (for `kind = python`) |
| `function` | string | with `python:` | Callable name in the Python module |

The tool's `kind` is inferred from which discriminator key is present (`cypher:` or `python:`). Specifying both is a validation error.

## `embedder:` object

| Key | Type | Required |
|---|---|---|
| `module` | string | yes |
| `class` | string | yes |
| `kwargs` | object | no |

The framework parses but doesn't instantiate. Downstream binaries load when `trust.allow_embedder: true`.

## `builtins:` object

| Key | Type | Default | Notes |
|---|---|---|---|
| `save_graph` | bool | `false` | Whether to register `save_graph` tool (downstream-binary use) |
| `temp_cleanup` | `"never"` \| `"on_overview"` | `"never"` | When to clear `temp/` dir |

## `workspace:` object

| Key | Type | Notes |
|---|---|---|
| `kind` | `"github"` \| `"local"` | Required |
| `root` | string | Required for `kind: local`; ignored for `github` |
| `watch` | bool | Optional, `local` mode only |
| `sandbox_root` | string | Optional, `local` mode only — containment boundary for `set_root_dir` |

The manifest `workspace:` block wins over CLI `--workspace` flag.

`sandbox_root` resolves relative to the manifest YAML's directory, exactly like `root`. When set, the `set_root_dir` tool refuses any target whose *canonical* path is not inside it (so `..` traversals and symlinks out of the tree are rejected too), and the server refuses to boot if `root` itself lies outside the boundary. **Unset — the default — means unbounded**: `set_root_dir` accepts any directory on the filesystem, which is the historical behaviour.

## `skills:` polymorphic value

Opts a deployment into shipping operator-authored methodology as MCP prompts. Three-layer composition: **project layer** (auto-detected `<basename>.skills/` directory adjacent to the manifest) → **domain pack(s)** (operator-declared paths) → **bundled defaults** (framework + downstream binary's compile-time skills). Higher layers fully replace same-named entries in lower layers (no merging).

Accepted shapes:

```yaml
skills: false           # default — feature disabled, no prompts surface
skills: true            # bundled framework defaults only
skills: ./my-skills/    # one operator-declared directory
skills:                 # mixed: bundled + domain pack(s)
  - true
  - ./my-skills/
  - ~/shared-mcp-skills/
```

Paths resolve relative to the manifest YAML (or against `$HOME` when prefixed with `~/`). The auto-detected project layer is *always* probed when `skills:` is any non-`false` value — operators don't list it explicitly.

Each SKILL.md file under a declared directory ships YAML frontmatter (`name`, `description`, optional `applies_to`/`references_tools`/`auto_inject_hint`) plus a markdown body. The framework enforces 4 KB soft / 16 KB hard size caps per skill and 64 KB total per resolved set.

A no-`skills:` deployment is a verbatim-current deploy: no `prompts/*` capability advertised in MCP `initialize`, no behavioural diff against pre-0.3.35.

See [Authoring Skills](../guides/authoring-skills.md) for the walkthrough and [Three-Layer Composition](../explanation/three-layer-composition.md) for the design rationale.

### `applies_when:` predicate gating (per-skill, 0.3.36+)

Individual SKILL.md files can declare an `applies_when:` block that gates whether the skill appears in `prompts/list`. Bounded set of predicates — not a DSL:

```yaml
---
name: read_code_source
description: Resolve qualified_name → source slice. TRIGGER when ...
applies_when:
  graph_has_node_type: [Function, Class]
  graph_has_property:
    node_type: Function
    prop_name: module
  tool_registered: cypher_query
  extension_enabled: csv_http_server
---
```

All populated predicates are ANDed. A skill with `applies_when:` absent is always active.

| Predicate | Dispatched by | Semantics |
|---|---|---|
| `graph_has_node_type: [...]` | Consumer's `SkillPredicateEvaluator` | True when any listed node type exists in the active graph |
| `graph_has_property: {node_type, prop_name}` | Consumer's `SkillPredicateEvaluator` | True when the property exists on the named node type |
| `tool_registered: <name>` | Framework | True when the tool is in the registered catalogue at boot |
| `extension_enabled: <key>` | Framework | True when `manifest.extensions.<key>` is set to a truthy value (not absent / null / `false`) |

Domain predicates (`graph_has_*`) require a consumer-supplied evaluator; without one they resolve to `Unknown` → inactive. This is the safe default — a typo'd predicate or missing evaluator must not silently activate the wrong-domain skill.

Operator-facing `mcp-server skills-list` shows every skill with an `active` / `inactive` column and per-clause outcomes for inactive ones. Agent-facing `prompts/list` filters inactive skills out silently (with a `tracing::info!` line per skipped skill for boot logs).

## `extensions:` object

Opaque passthrough. Only the top-level `extensions:` key is validated; everything below is stored verbatim as a `serde_json::Map`. Downstream binaries define their own sub-schemas:

```yaml
extensions:
  cypher_preprocessor:        # kglite's preprocessor block (0.9.25+)
    module: ./preprocessor.py
    class: WikidataPreprocessor
  csv_http_server:            # kglite's CSV server block
    enabled: true
    host: 127.0.0.1
    port: 8765
```

## Programmatic access

```rust
use mcp_methods::server::manifest;
let m = manifest::load(yaml_path)?;
println!("{}", m.trust.allow_embedder);
let json = m.to_json();   // serde_json::Value for FFI bridging
```

```python
# Via the pyo3 wrapper (e.g. kglite-mcp-server's PyManifest)
m = PyManifest.load("workspace_mcp.yaml")
d = m.as_dict()
print(d["trust"]["allow_embedder"])
```

## JSON shape from `Manifest::to_json()`

Pinned by the `to_json_shape_is_stable` test in `crates/mcp-methods/src/server/manifest.rs`. Field additions in patch releases are non-breaking; renames or removals are breaking changes requiring a minor version bump.

## See also

- [Writing a Manifest](../guides/writing-a-manifest.md) — narrative walkthrough
- [Trust Gates](../guides/trust-gates.md) — `trust:` deep dive
- [Examples](../examples/minimal-manifest.md) — worked manifests
- [docs.rs/mcp-methods](https://docs.rs/mcp-methods) — Rust API rustdoc
