# Example: Tools + Trust Gates

A manifest declaring Cypher tools (for downstream binaries with a graph backend), an embedder, and a query preprocessor — illustrating all three trust gates.

```yaml
name: KGLite-style Knowledge Graph Server
instructions: |
  A knowledge graph server with Cypher querying, semantic search via
  text_score(), and a query preprocessor that normalizes incoming
  Cypher before dispatch.

source_roots:
  - ./source

# All three gates set to true — the operator has approved each
# corresponding extension category.
trust:
  allow_python_tools: false           # no python: tools below
  allow_embedder: true                # embedder block below requires this
  allow_query_preprocessor: true      # extensions.cypher_preprocessor requires this

builtins:
  save_graph: true                    # register save_graph tool
  temp_cleanup: on_overview           # clear temp/ when graph_overview runs

# Cypher tools — dispatched by downstream binaries that implement
# cypher_query. The generic mcp-server CLI parses but doesn't dispatch.
tools:
  - name: prospects_in_region
    cypher: |
      MATCH (p:Prospect)-[:LOCATED_IN]->(r:Region {name: $region})
      RETURN p.name, p.commercial_status
    description: "List prospects in a named region"
    parameters:
      type: object
      properties:
        region:
          type: string
      required: [region]

  - name: missing_required_edges
    cypher: |
      MATCH (n:Prospect)
      WHERE NOT (n)-[:LOCATED_IN]->(:Region)
      RETURN n.name
    description: "Find prospects missing a region (data-integrity validator)"

# Embedder configuration — downstream binaries load this when
# trust.allow_embedder: true.
embedder:
  module: ./embedder.py
  class: SentenceTransformerEmbedder
  kwargs:
    model_name: BAAI/bge-m3
    cache_dir: ./.cache/embedder

# Opaque passthrough — downstream-binary-specific config.
# kglite's preprocessor reads this block when
# trust.allow_query_preprocessor: true.
extensions:
  cypher_preprocessor:
    module: ./preprocessor.py
    class: WikidataPreprocessor
    kwargs:
      log_rewrites: false

env_file: .env
```

## What the generic `mcp-server` CLI does

```text
mcp-server --mcp-config kglite_style.yaml
```

1. Parses + validates the full manifest (all keys recognised).
2. Boots the framework with `source_roots: [./source]` bound to the source tools.
3. Registers `read_source`, `grep`, `list_source`, plus `ping`.
4. **Does NOT dispatch `prospects_in_region` or `missing_required_edges`** — no graph backend. Logs a warning.
5. **Does NOT instantiate the embedder** — framework doesn't load embedders.
6. **Does NOT load `extensions.cypher_preprocessor`** — framework doesn't know what it is.
7. Serves over stdio.

## What kglite-mcp-server does with the same manifest

1. Parses + validates (same framework code).
2. Reads `trust.allow_embedder` and `trust.allow_query_preprocessor`. Both are `true`. ✓
3. Reads `embedder:`, instantiates the Python class via pyo3, wires it into `text_score()`.
4. Reads `extensions.cypher_preprocessor`, instantiates the Python class, wires it into the Cypher dispatch path.
5. Registers `cypher_query`, `graph_overview`, `save_graph`, `read_code_source` tools.
6. Dispatches `prospects_in_region` and `missing_required_edges` against the active graph.
7. Serves over stdio.

Same manifest, different runtime behaviour. The trust-gate split is what makes this safe to share across binaries — kglite enforces, generic mcp-server ignores (because it has nothing to enforce).

## Enforcement failure mode

If the manifest declares an extension WITHOUT the corresponding trust gate:

```yaml
trust:
  allow_query_preprocessor: false      # ← denied
extensions:
  cypher_preprocessor:                 # ← but block is present
    module: ./preprocessor.py
    class: WikidataPreprocessor
```

kglite-mcp-server's boot-time check refuses to start:

```text
$ kglite-mcp-server --mcp-config kglite_style.yaml
ManifestError: extensions.cypher_preprocessor requires trust.allow_query_preprocessor: true
```

The generic `mcp-server` boots fine (it doesn't read `extensions.cypher_preprocessor`). This is intentional — see [Trust Pattern](../explanation/trust-pattern.md).

## See also

- [Trust Gates](../guides/trust-gates.md) — usage + enforcement patterns
- [Trust Pattern](../explanation/trust-pattern.md) — the design rationale
- [Writing a Manifest](../guides/writing-a-manifest.md) — field-by-field reference
