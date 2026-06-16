# Trust Gates

The `trust:` block in a manifest is **advisory metadata**. The framework parses + surfaces it; downstream consumers (`kglite-mcp-server`, your own binary) enforce it. This page explains the model and how to use it correctly.

## The two flags

| Flag | Gates (consumer enforces) |
|---|---|
| `allow_python_tools` | `tools[].python:` factories — Python code the operator declared as a tool |
| `allow_embedder` | `embedder:` loaders — typically a Python class that produces vector embeddings |

Both default to `false`. The framework writes them to:

- Rust: `manifest.trust.allow_python_tools` (etc.)
- JSON: `manifest.to_json()["trust"]["allow_python_tools"]`
- pyo3: `manifest.as_dict()["trust"]["allow_python_tools"]`

## The "framework records, consumer enforces" contract

When you write:

```yaml
trust:
  allow_embedder: true
embedder:
  module: ./embedder.py
  class: SentenceTransformerEmbedder
```

the framework:

1. Parses the manifest, validating `allow_embedder` is a bool.
2. Parses the `embedder:` block (module/class/kwargs) into `manifest.embedder`.
3. Boots, exposing the manifest via `Manifest::to_json()` and the Rust struct.
4. **Does NOT instantiate the embedder.** The framework has no Python runtime.

The downstream consumer (kglite-mcp-server) then:

1. Reads `manifest.trust.allow_embedder`.
2. If false AND `embedder:` is set, **refuses to boot** with an error like: `embedder requires trust.allow_embedder: true`.
3. If true, instantiates the embedder via its pyo3 wrapper and wires it into the `text_score()` path.

This separation is intentional — see [Trust Pattern](../explanation/trust-pattern.md) for the design rationale.

## Why advisory, not enforced?

Three reasons:

1. **Domain-agnostic framework.** The framework doesn't know what an embedder is or what makes a `python:` tool "safe." Each consumer has different semantics for the same flag. Enforcement belongs where the semantics live.

2. **Audit-readable in one place.** An operator reviewing a manifest for security implications can scan `trust:` to see every dynamic-code hook the server allows, without hunting through `extensions:` for hidden gates. The block becomes the canonical "what does this server load?" surface.

3. **Forward-compatibility.** Adding a new trust gate is a non-breaking patch release (the framework just accepts and stores a new boolean key). Consumers adopt the new gate at their own pace, and the audit story scales.

## What happens if you forget to enforce?

The framework can't catch it. If a consumer reads `manifest.embedder` and instantiates the hook regardless of `trust.allow_embedder`, the operator's `false` is silently ignored.

This is why the contract is documented in CHANGELOG entries (every new gate ships with the enforcement responsibility spelled out) and in [Trust Pattern](../explanation/trust-pattern.md).

**If you're writing a downstream binary**: write a single boot-time check that walks each dynamic-code hook + its corresponding trust flag and raises a clean error before any hook is instantiated. The kglite pattern is:

```python
# In the consumer's manifest loader
if (
    manifest.embedder is not None
    and not manifest.trust.allow_embedder
):
    raise ManifestError(
        "embedder requires "
        "trust.allow_embedder: true"
    )
```

One check per hook, fail-loud, before `serve()`.

## Adding a new trust gate

If your downstream binary needs a new extension category, propose a new trust flag upstream:

1. The flag goes in `ALLOWED_TRUST_KEYS` (in `crates/mcp-methods/src/server/manifest.rs`) and `TrustConfig`.
2. The parser in `build_trust()` reads it as a bool.
3. `Manifest::to_json()` emits it under `trust`.
4. The snapshot test (`to_json_shape_is_stable`) is updated to include it.
5. CHANGELOG entry documents the consumer-enforcement contract.

This is exactly the path `allow_embedder` followed — proposed by kglite when they needed to load a Python embedder for semantic search, added upstream as a non-breaking patch, and enforced in their boot-time check.

## See also

- [Writing a Manifest](writing-a-manifest.md) — the full `trust:` block syntax
- [Trust Pattern](../explanation/trust-pattern.md) — the design rationale
- [Downstream Binary](downstream-binary.md) — where enforcement lives in your code
