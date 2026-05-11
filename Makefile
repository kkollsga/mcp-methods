.PHONY: dev dev-with-bin bundle-bin test lint fmt clean

# Standard dev install — cdylib only, no bundled binary on PATH.
dev:
	maturin develop --release

# Dev install with the bundled `mcp-server` binary on PATH. Builds the
# binary via cargo first, copies it into the python package's _bin/
# directory (where the launcher in _cli.py looks for it), then runs
# `maturin develop`. The same sequence is what CI runs at wheel-build
# time. Use this if you want `which mcp-server` to resolve to the
# wheel-installed binary during local development.
dev-with-bin: bundle-bin
	maturin develop --release

# Build the binary and copy it into the python package. Idempotent.
bundle-bin:
	cargo build --release --features server --bin mcp-server
	mkdir -p python/mcp_methods/_bin
	cp target/release/mcp-server python/mcp_methods/_bin/mcp-server

test:
	pytest tests/ -v

lint:
	cargo fmt -- --check
	cargo clippy -- -D warnings
	ruff check .

fmt:
	cargo fmt
	ruff format .
	ruff check --fix .

clean:
	cargo clean
	rm -rf wheels/ dist/ *.egg-info build/
	rm -f python/mcp_methods/_bin/mcp-server python/mcp_methods/_bin/mcp-server.exe
