.PHONY: dev dev-with-bin bundle-bin test test-rust test-rust-all lint fmt clean \
	check-dev-docs

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

lint: check-dev-docs
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

## Mechanical owner for the gitignored dev-docs/ working folder (R4: every
## file accumulation has a bound and an owner). The decision of 2026-07-29
## deliberately gave this repo no bench/temp/bin purge tiers and declined the
## `dev-docs-cleanup` skill, on the grounds that dev-docs/ is a handful of
## markdown files and no step generates into it — with an explicit revisit
## trigger: more than 20 files, or any step writing generated output into it.
## Both halves of that trigger are stated as the constants below and checked
## here, so crossing it is a build failure rather than something someone
## notices a year later. This never deletes: the revisit is a decision (add
## the tier that owns the growth, or prune the backlog), not a cleanup.
##
## The trigger and its rationale are written here on purpose. dev-docs/ is
## unbacked, so a committed file must not send its reader into it for the
## rule it is enforcing.
DEV_DOCS_MAX_FILES := 20
DEV_DOCS_MAX_MB := 256
.PHONY: check-dev-docs
check-dev-docs:
	@[ -d dev-docs ] || { echo "no dev-docs/ — nothing to bound"; exit 0; }; \
	n=$$(find dev-docs -type f ! -name '.DS_Store' | wc -l | tr -d ' '); \
	mb=$$(du -sm dev-docs | cut -f1); \
	fail=0; \
	if [ "$$n" -gt $(DEV_DOCS_MAX_FILES) ]; then \
		echo "FAIL: dev-docs/ holds $$n files (> $(DEV_DOCS_MAX_FILES))"; fail=1; fi; \
	if [ "$${mb:-0}" -ge $(DEV_DOCS_MAX_MB) ]; then \
		echo "FAIL: dev-docs/ is $${mb} MB (>= $(DEV_DOCS_MAX_MB) MB) — something is generating into it"; \
		du -sm dev-docs/* 2>/dev/null | sort -rn | head -5 | sed 's/^/    /'; fail=1; fi; \
	[ "$$fail" = 0 ] || { \
		echo "  -> this is the revisit point the 2026-07-29 decision named:"; \
		echo "     add the purge tier that owns the growth, or prune the backlog."; \
		exit 1; }; \
	echo "dev-docs/ is $$n files / $${mb} MB (limits $(DEV_DOCS_MAX_FILES) files, $(DEV_DOCS_MAX_MB) MB)"
