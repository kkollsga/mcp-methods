.PHONY: dev test test-rust test-rust-all lint fmt clean

# Build + install the Python wheel into the active env. The wheel
# contains the `_mcp_methods` cdylib only; the `mcp-server` CLI is a
# separate crate — install it via `cargo install --path crates/mcp-server`
# if you need it on PATH.
dev:
	maturin develop --release

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
