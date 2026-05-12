# Distribution Shape — Polars / Pydantic-Core Pattern

The 0.3.26 → 0.3.30 arc reshaped how mcp-methods is distributed, settling on three publication paths that map cleanly to three audience types. This page explains the model and the constraints that drove it.

## The three publication paths

| Path | Audience | Artifact |
|---|---|---|
| `pip install mcp-methods` | Python users and operators | Wheel (cdylib + bundled CLI binary + Python source) |
| `cargo add mcp-methods` | Rust library consumers | Pure Rust crate (zero pyo3, library only) |
| `cargo install --git ... mcp-server` | Rust operators preferring cargo | Standalone CLI binary, built from source |

The first two are publishing-relationship paths; the third is opt-in for Rust toolchain users who don't want pip.

## The polars / pydantic-core pattern

We follow the same three-crate pattern as [polars](https://github.com/pola-rs/polars) and [pydantic-core](https://github.com/pydantic/pydantic-core):

- A pure-Rust library crate (`mcp-methods`) on crates.io, with zero Python in its source or transitive dependencies
- A separate pyo3 binding crate (`mcp-methods-py`) that builds the Python wheel cdylib
- Both crates share the same release version and CHANGELOG

This separates **library design concerns** (clean Rust API, predictable trait bounds, focused dep tree) from **binding design concerns** (pyo3 traits, Python type conversions, GIL handling, cdylib build settings). A Rust consumer reading `crates/mcp-methods/src/cache.rs` sees idiomatic Rust; a Python wheel maintainer reading `crates/mcp-methods-py/src/lib.rs` sees pyo3 machinery.

## Why kglite couldn't keep the bundled-binary pattern (and why we can)

kglite (a downstream consumer of mcp-methods) bundled their `kglite-mcp-server` binary in their pip wheel for 0.9.18-0.9.19. The binary worked, but their dep graph forced libpython linkage:

```
kglite-mcp-server (binary)
  → kglite (rlib)         ← links libpython via pyo3
  → mcp-methods (rlib)
```

A binary that transitively links libpython needs a wheel per Python version (the ABI changes between cpython 3.10 and 3.11). They were shipping 12 wheels (3 OS × 4 Python).

In 0.9.20 they pivoted to a Python entry point that uses FastMCP and calls into their pyo3 bindings — no binary, no libpython problem, back to 3 abi3 wheels per OS.

**Our binary has zero libpython link:**

```
mcp-server (binary)
  → mcp-methods (rlib)    ← zero pyo3 in source or deps
```

Because `mcp-methods` is pure Rust (the 0.3.26 split), the binary is libpython-free regardless of how the wheel that bundles it is structured. We ship one abi3 wheel per OS that includes:

- The `_mcp_methods.so` cdylib (pyo3-linked, but only the cdylib is — the binary isn't)
- The native `mcp-server` binary
- The Python source

Same wheel, both surfaces, zero ABI compatibility issues.

## The 0.3.26 → 0.3.30 evolution

| Release | Distribution change | Why |
|---|---|---|
| 0.3.25 | Single crate, bundled binary, 12-wheel matrix | Pre-split: library + binary + cdylib all in one crate; pyo3 cfg-toggled |
| 0.3.26 | Three crates, bundled binary REMOVED, 3-wheel abi3 matrix | Polars-pattern split for architectural cleanliness; binary removed because "polars-shape clean split" argument said it didn't belong in the wheel |
| 0.3.30 | Three crates, bundled binary RESTORED, 3-wheel abi3 matrix maintained | The 0.3.26 architectural separation made libpython-free binaries possible; bundling them back is a UX win with no architectural cost |

The 0.3.26 decision to remove the binary was correct *for its time* — without the libpython-free guarantee, the bundle would have re-introduced the wheel-matrix complexity. With the guarantee in place (from the three-crate split itself), restoring the bundle becomes a simple UX improvement.

## Comparison to other ecosystems

| Project | Distribution shape | Notes |
|---|---|---|
| `polars` | crates.io + PyPI; pyo3 binding crate separate from rlib | Same as us; many abi3 wheels per OS-arch |
| `pydantic-core` | crates.io + PyPI; pyo3 binding crate separate | Same |
| `cryptography` | PyPI only (uses cffi, not pyo3) | Binary-distribution-only; no Rust API surface |
| `tokenizers` (HF) | crates.io + PyPI; pyo3 binding crate | Same pattern |
| `kglite` (0.9.20+) | PyPI only; Python entry point invokes pyo3 bindings | Couldn't keep binary bundle due to libpython linkage |

The pattern is well-established for Rust crates that want both audiences.

## What this means for consumers

- **Python users**: `pip install mcp-methods` and you're done. The CLI is on PATH.
- **Rust library consumers**: `cargo add mcp-methods` from a Rust project. Zero pyo3 in your `cargo tree`. Library version moves in lockstep with the wheel.
- **Operators preferring cargo over pip**: `cargo install --git https://github.com/kkollsga/mcp-methods mcp-server`. Not on crates.io because we don't want the maintenance overhead of a second published name.

The release line is single — `0.3.X` covers all three paths. No "library is at 0.5 but wheel is at 0.3.30" version skew.

## See also

- [Architecture](architecture.md) — the three-crate structure
- The 0.3.26 + 0.3.30 entries in [CHANGELOG](../changelog.md) — full historical record
- [Polars's architecture overview](https://docs.pola.rs/development/contributing/) — the canonical example of this pattern
