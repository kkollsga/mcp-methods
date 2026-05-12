# Architecture

`mcp-methods` is structured as a three-crate Rust workspace plus a Python wheel that bundles the CLI binary. This page explains why.

## The three crates

```
crates/
├── mcp-methods/          Pure-Rust library — zero pyo3 in dep tree
├── mcp-methods-py/       PyO3 binding crate — builds the Python wheel cdylib
└── mcp-server/           Standalone CLI binary
```

### `mcp-methods/`

The framework's core. Contains:

- **Primitives** (always available): `cache`, `compact`, `files`, `git_refs`, `github`, `grep`, `html`, `json_grep`, `list_dir`
- **Server framework** (feature = "server", default): `manifest`, `server`, `workspace`, `watch`, `env`, `runtime`, `source`

Zero pyo3 in source or transitive dependencies. `cargo tree -p mcp-methods | grep pyo3` returns nothing. CI gate enforces this.

### `mcp-methods-py/`

PyO3 bindings crate. Builds the cdylib (`_mcp_methods.so`) that Python's `from mcp_methods import ...` loads from. Depends on `mcp-methods` from the workspace path.

Why a separate crate? Two reasons:

1. **The pure-Rust library stays clean.** Anyone reading `crates/mcp-methods/src/cache.rs` sees no `#[cfg_attr(feature = "python", pyfunction)]`, no `Py<PyAny>`, no `Python::attach`. Just Rust.
2. **The cdylib has different constraints than the rlib.** It needs `crate-type = ["cdylib"]`, the `pyo3/extension-module` feature, and `abi3-py310` for the single-abi-wheel matrix. Keeping these in their own crate avoids polluting the library's Cargo.toml with concerns that have nothing to do with library consumers.

This is the [polars](https://github.com/pola-rs/polars) / [pydantic-core](https://github.com/pydantic/pydantic-core) pattern.

### `mcp-server/`

Standalone CLI binary. Depends on `mcp-methods` with the `server` feature. ~370 LOC: clap arg parsing, mode dispatch, manifest loading, rmcp boot, stdio serve.

Not published to crates.io (it ships bundled in the pip wheel — see [Distribution Shape](distribution-shape.md)). Available for cargo-install via git rev.

## The wheel-bundled binary

`pip install mcp-methods` packages:

```
mcp_methods/
├── __init__.py             # imports from _mcp_methods (cdylib)
├── __init__.pyi            # type stubs
├── _cli.py                 # Python launcher for the binary
├── _mcp_methods.so         # cdylib (built from crates/mcp-methods-py)
├── _bin/
│   └── mcp-server          # native Rust binary (built from crates/mcp-server)
└── fastmcp/                # FastMCP helpers (pure Python)
```

`[project.scripts] mcp-server = "mcp_methods._cli:main"` in `pyproject.toml` registers the launcher; `[tool.maturin] include` forces the binary into the wheel.

This works cleanly because **our binary has zero libpython link**. The pure-Rust separation in 0.3.26 (the three-crate split) made this possible — pre-0.3.26 the binary was bundled with libpython via shared cfg-feature toggles, which forced per-Python-version wheels (3 OS × 4 Python = 12 wheels). Post-0.3.26 the binary is libpython-free, so we ship one abi3 wheel per OS regardless of Python version.

## Why this shape, briefly

- **Library consumers (kglite + future Rust downstream)** get a clean Rust API with no Python concerns. `cargo add mcp-methods` from a Rust project sees zero pyo3 in `cargo tree`.
- **Python consumers** get `pip install mcp-methods` and have both the library and the CLI on PATH in one install. Three abi3 wheels per OS.
- **Operators who only want the CLI** get it via pip (no Rust toolchain required) or via `cargo install --git ...` (if they prefer cargo).
- **Downstream-binary builders** depend on `mcp-methods` from crates.io, wrap `McpServer::new`, and ship their own crate or pip wheel — the [kglite-mcp-server](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server) pattern.

## Historical arc (0.3.20 → 0.3.30)

| Release | Change |
|---|---|
| 0.3.20-0.3.24 | Single crate, optional pyo3 feature gate, cfg-toggled annotations sprinkled through source |
| 0.3.25 | Workspace consolidation; bundled binary in wheel via `_bin/`; 12-wheel matrix (3 OS × 4 Python) |
| 0.3.26 | Three-crate split (polars pattern); zero pyo3 in mcp-methods source/deps; bundled binary REMOVED on architectural grounds |
| 0.3.27 | `Manifest::to_json()` for FFI bridging |
| 0.3.28 | `set_root_dir` atomic-swap fix |
| 0.3.29 | `trust.allow_query_preprocessor` advisory gate |
| 0.3.30 | Bundled binary RESTORED (zero-libpython binary makes per-Python matrix unnecessary); crates.io publication |

The 0.3.26 → 0.3.30 arc is the interesting one — the architectural separation (0.3.26) enabled the distribution simplification (0.3.30) without compromising the clean Rust library interface.

## See also

- [Distribution Shape](distribution-shape.md) — deeper dive on the wheel + crates.io split
- [Trust Pattern](trust-pattern.md) — why `trust:` is advisory-not-enforced
- [Downstream Binary](../guides/downstream-binary.md) — how consumers layer on top
