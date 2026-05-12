# Contributing to mcp-methods

## Development Setup

```bash
# Clone and set up
git clone https://github.com/kkollsga/mcp-methods.git
cd mcp-methods
python3 -m venv .venv
source .venv/bin/activate
pip install maturin pytest ruff

# Build the Python wheel + install editably into the venv
make dev               # cdylib only, no bundled binary on PATH
# OR
make dev-with-bin      # also build + bundle the mcp-server binary

# Run tests
make test              # Python (pytest)
make test-rust         # Rust library only
make test-rust-all     # Rust workspace (mcp-methods + mcp-server)
```

After `make dev-with-bin`, `which mcp-server` should resolve to a
binary inside your venv's `bin/` directory.

## Project Structure

```
crates/
  mcp-methods/                 # Pure Rust library — zero pyo3 in dep tree
    src/
      lib.rs                   # Public module list
      cache.rs, compact.rs, files.rs, github.rs, grep/, html.rs,
      json_grep.rs, list_dir.rs       # primitives (always available)
      server/                  # MCP server framework (feature = "server")
        manifest.rs            # YAML manifest parser + Manifest::to_json
        workspace.rs           # github + local workspace, watch hook
        server.rs              # McpServer, ServerOptions
        env.rs, runtime.rs, source.rs, watch.rs
    tests/
      deployed_manifests.rs    # regression suite — production YAML shapes
  mcp-methods-py/              # PyO3 binding crate — builds the Python wheel
    src/lib.rs                 # #[pymodule] re-exporting the surface
  mcp-server/                  # standalone CLI binary
    src/main.rs                # clap + tokio + McpServer::serve

python/mcp_methods/            # Python package
  __init__.py                  # imports from _mcp_methods (the cdylib)
  _cli.py                      # console-script launcher for mcp-server
  fastmcp/                     # FastMCP helper module
  _bin/                        # bundled binary (gitignored, built by wheel)

tests/                         # Python tests (pytest)
docs/                          # Sphinx docs site (Read the Docs)
examples/                      # Runnable example consumers

CHANGELOG.md                   # release notes — high quality, please match
Cargo.toml                     # workspace root (virtual manifest)
pyproject.toml                 # maturin config + Python packaging
```

## The pre-commit hook

Every commit runs four checks (see `.git/hooks/pre-commit`):

- `cargo fmt` — must be clean
- `cargo clippy --workspace -- -D warnings` — no clippy warnings
- `ruff format --check` — Python format
- `ruff check` — Python lints

If a commit fails the hook, fix the issue and recommit. **Don't use
`--no-verify`** — the hook catches real problems.

## Tests

| Layer | Command | What it covers |
|---|---|---|
| Rust library | `cargo test -p mcp-methods` | The framework, primitives, manifest parser |
| Rust binary | `cargo test -p mcp-server` | The CLI argument-handling smoke tests |
| Python wheel | `pytest tests/` | The pyo3 surface end-to-end |
| Deployed manifests | `cargo test -p mcp-methods --test deployed_manifests` | The 5 production YAML shapes still parse |

Total: ~107 Rust tests, ~176 Python tests. All must pass for any PR.

### When adding a manifest field

The `Manifest::to_json()` shape is pinned by the
`to_json_shape_is_stable` test in `crates/mcp-methods/src/server/manifest.rs`.
If you add a field:

1. Add the field to the `Manifest` struct.
2. Add the parser logic to `build()` / `build_*` in the same file.
3. Add it to `Manifest::to_json()`.
4. Update `ALLOWED_TOP_KEYS` (or the relevant sub-list).
5. Update the snapshot test literal.
6. Add a fixture to `deployed_manifests.rs` if the field shows up in
   a production YAML.

This sequence is non-negotiable. The snapshot test is the JSON
contract for downstream FFI consumers (kglite, etc.) — silently
dropping a field is a worse failure mode than a noisy test failure.

## Release cadence

We ship patch releases (`0.3.X`) liberally — additive changes are
non-breaking and downstream consumers handle them transparently.

| Bump | When |
|---|---|
| Patch (0.3.30 → 0.3.31) | Bug fixes, new advisory trust gates, additive manifest fields, JSON-shape field additions |
| Minor (0.3.X → 0.4.0) | Breaking changes to the Rust API, manifest field renames or removals, breaking JSON shape changes |
| Major (0.3 → 1.0) | API frozen, breaking changes telegraphed in advance |

CHANGELOG entries are required. Match the prose quality of recent
entries — explain the *why*, not just the *what*. Multi-day arcs
(0.3.26 through 0.3.30) get cross-referenced so readers can follow
the architectural evolution.

## Distribution

Three artifacts ship from this repo:

- **`pip install mcp-methods`** — Python library + bundled
  `mcp-server` CLI on PATH (3 abi3 wheels per OS).
- **`cargo add mcp-methods`** — pure-Rust library, zero pyo3 in
  the dep tree.
- **`crates/mcp-server/` source** — workspace member, NOT published
  to crates.io. Downstream Rust users who want the binary without
  pip run `cargo install --git https://github.com/kkollsga/mcp-methods mcp-server`.

The Python wheel and the Rust library publish in lockstep (same
version number, same release commit). The version is the
single source of truth in `crates/mcp-methods/Cargo.toml`.

## Commit messages

Use this format:

```
type(scope): short description

Optional longer explanation.

Co-Authored-By: …
```

| Type | When |
|---|---|
| `feat` | New feature or capability |
| `fix` | Bug fix |
| `refactor` | Restructure without behavior change |
| `docs` | Documentation only |
| `chore` | Build, CI, dependency updates |
| `test` | Adding/updating tests |

Scope is the affected crate or area (`mcp-server`, `manifest`,
`workspace`, `docs`, etc.). Examples:

```
feat(0.3.29): add trust.allow_query_preprocessor advisory gate
fix(workspace): set_root_dir no longer clobbers active_repo_path
refactor(0.3.26): split into pure-Rust library + PyO3 bindings
```

## Issues and PRs

- File issues at https://github.com/kkollsga/mcp-methods/issues
- Open PRs against `main`
- One concept per PR — easier to review and revert

For downstream coordination (kglite + similar consumers), the `inbox/`
directory at the repo root tracks per-day correspondence threads. If
your change affects a downstream consumer, drop a note there.

## License

MIT. Contributions are accepted under the same license.
