.PHONY: dev dev-with-bin bundle-bin test test-rust test-rust-all lint fmt clean

# Build + install the Python wheel into the active env. Cdylib only,
# no bundled `mcp-server` binary on PATH — use `make dev-with-bin` if
# you want the CLI installed alongside.
dev:
	maturin develop --release

# Dev install with the bundled `mcp-server` binary on PATH. Builds the
# Rust binary, copies it under `python/mcp_methods/_bin/`, then runs
# `maturin develop` so the binary is force-included via the
# `[tool.maturin] include` block in `pyproject.toml`.
dev-with-bin: bundle-bin
	maturin develop --release

# Build the binary from `crates/mcp-server` and copy it into the
# python package. Idempotent. The wheel-build workflow does the same
# steps before `maturin build`.
bundle-bin:
	cargo build --release -p mcp-server
	mkdir -p python/mcp_methods/_bin
	cp target/release/mcp-server python/mcp_methods/_bin/mcp-server

test:
	pytest tests/ -v

# Run the Rust library tests (pure Rust, no Python).
test-rust:
	cargo test -p mcp-methods

# Run all Rust tests across the workspace.
test-rust-all:
	cargo test --workspace

lint:
	cargo fmt -- --check
	cargo clippy --workspace -- -D warnings
	ruff check .

fmt:
	cargo fmt
	ruff format .
	ruff check --fix .

clean:
	cargo clean
	rm -rf wheels/ dist/ *.egg-info build/
