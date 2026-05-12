# The Trust Pattern — Why Advisory, Not Enforced

The framework parses the `trust:` block in a manifest but doesn't enforce its flags. Enforcement lives in downstream consumers. This page explains the design call.

## What the framework does

```rust
// In crates/mcp-methods/src/server/manifest.rs
pub struct TrustConfig {
    pub allow_python_tools: bool,
    pub allow_embedder: bool,
    pub allow_query_preprocessor: bool,
}
```

It parses the YAML, validates each value is a bool, surfaces it via `Manifest::to_json()` and the Rust struct. **Then it stops.** No code in `mcp-methods` ever reads `manifest.trust.allow_embedder` and decides to load or skip an embedder — because the framework doesn't load embedders. The same is true for `allow_query_preprocessor` and `allow_python_tools` after the 0.3.26 framework-cleanup pass.

## Why advisory

### Domain-agnostic framework

The framework doesn't know what an "embedder" is. The first downstream consumer (`kglite-mcp-server`) loads a Python class that produces 1024-dim text embeddings via `BAAI/bge-m3`. A different downstream binary might load a face-recognition embedder, or a graph-edge embedder, or no embedder at all. Each has different semantics for the same flag.

If the framework enforced `allow_embedder`, it would need to know what an embedder is. It doesn't. Enforcement belongs where the semantics live.

### Audit readability in one place

An operator reviewing a manifest for security implications wants one place to see every dynamic-code hook the server permits. The `trust:` block is that place:

```yaml
trust:
  allow_python_tools: true           # operator approved Python factories
  allow_embedder: true               # operator approved embedder loading
  allow_query_preprocessor: false    # operator denied query rewriting
extensions:
  embedder: { ... }                  # has approval → boots
  cypher_preprocessor: { ... }       # NO approval → consumer refuses to boot
```

If trust were spread across `extensions:` (e.g. `extensions.embedder.allow_unsafe: true`), the operator would have to grep through the full manifest to audit. Centralizing it in `trust:` makes the security review a single read.

### Forward compatibility

Adding a new trust gate is a non-breaking patch release. We:

1. Add the key to `ALLOWED_TRUST_KEYS`
2. Add the field to `TrustConfig`
3. Parse it in `build_trust`
4. Emit it in `Manifest::to_json`

Downstream consumers adopt the new gate at their own pace. Consumers that don't know about the new flag read `false` (the default) and refuse to boot the new hook — which is the safe default.

This is exactly the path `allow_query_preprocessor` followed for kglite's 0.9.25 release. Proposed by kglite, shipped as 0.3.29 same-day, kglite enforces it in their boot-time check. Zero coordination overhead.

## What enforcement looks like

In a downstream binary's manifest loader:

```rust
// Rust example
if manifest.extensions.get("cypher_preprocessor").is_some()
    && !manifest.trust.allow_query_preprocessor
{
    anyhow::bail!(
        "extensions.cypher_preprocessor requires trust.allow_query_preprocessor: true"
    );
}
```

```python
# Python example (kglite's pattern)
if (
    manifest.extensions.get("cypher_preprocessor") is not None
    and not manifest.trust.allow_query_preprocessor
):
    raise ManifestError(
        "extensions.cypher_preprocessor requires "
        "trust.allow_query_preprocessor: true"
    )
```

One check per gate, fail-loud at boot, before any hook is instantiated.

## What can go wrong

The framework can't catch a consumer that forgets to enforce. If a downstream binary reads `manifest.extensions.cypher_preprocessor` and instantiates the hook without checking `manifest.trust.allow_query_preprocessor`, the operator's `false` is silently ignored.

Mitigations:

1. **CHANGELOG entries spell out the enforcement responsibility.** Every new trust gate shipped includes a one-line "downstream consumers enforce" callout.
2. **The reference downstream binary (`kglite-mcp-server`) demonstrates the correct pattern.** New consumers can crib from it.
3. **Documentation cross-references.** This page, the [Trust Gates guide](../guides/trust-gates.md), and CONTRIBUTING.md all point at the same enforcement pattern.

None of these mitigations stop a determined contributor from skipping enforcement. The trade-off is intentional: the framework stays domain-agnostic, and the enforcement responsibility is clear at the boundary.

## Comparison: framework-enforced trust

Some frameworks (e.g. Kubernetes admission webhooks, Open Policy Agent) enforce trust centrally — the framework refuses to load resources that violate the policy. Why not here?

Three reasons:

1. **The framework runs as a library.** Downstream binaries link `mcp-methods` and call `McpServer::new`. The framework has no privileged process boundary to refuse from.
2. **The relevant semantics live in the consumer.** "Load an embedder" means different things in different consumers. The framework can't enforce a verb it doesn't implement.
3. **Forward compat is cheaper.** Centrally enforced policy means every new trust gate is a coordination event between the framework and the consumer. Advisory metadata means the framework just adds the field and consumers opt in.

The advisory pattern is well-suited to a *library framework* like mcp-methods. A process-level framework (a daemon, a deployment system) might justify central enforcement; a library doesn't.

## See also

- [Trust Gates (how-to)](../guides/trust-gates.md) — usage + enforcement patterns
- [Architecture](architecture.md) — three-crate layout
- The `allow_query_preprocessor` 0.3.29 release notes in [CHANGELOG](../changelog.md) — worked example
