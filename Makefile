.PHONY: dev dev-with-bin bundle-bin test test-rust test-rust-all lint fmt clean \
	check-dev-docs prune-target

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
	RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
	ruff check .

fmt:
	cargo fmt
	ruff format .
	ruff check --fix .

## On workstations where `target/` is a symlink into a shared build volume
## (`target -> /Users/Shared/cargo-targets/mcp-methods`), a bare `cargo clean`
## removes *the symlink itself* — 39 B — and frees nothing: the build products
## stay behind, orphaned, and the next `cargo build` recreates `target/` as a
## real directory on the slow external volume, silently un-migrating the repo.
## So clean *through* the link: resolve it, empty the resolved directory, and
## restore the link if something already removed it. On a plain checkout (CI,
## other machines) `target/` is a directory and this is a normal `cargo clean`.
## Opt-in destination used ONLY to restore a `target` symlink that is already
## gone: `make clean SHARED_TARGET_DIR=/Users/Shared/cargo-targets/mcp-methods`.
## Empty by default on purpose — a git worktree also has no `target/`, and
## silently linking one at the main checkout's shared build dir would let a
## `make clean` in the worktree wipe the main checkout's build products.
SHARED_TARGET_DIR ?=

clean:
	@if [ -L target ]; then \
		dest=$$(readlink target); \
		case "$$dest" in /*) ;; *) dest="./$$dest" ;; esac; \
		echo "target -> $$dest (symlink); cleaning through the link"; \
		if [ -d "$$dest" ]; then CARGO_TARGET_DIR="$$dest" cargo clean; fi; \
		mkdir -p "$$dest"; \
	elif [ -d target ]; then \
		cargo clean; \
	elif [ -n "$(SHARED_TARGET_DIR)" ]; then \
		echo "target/ is missing — restoring the symlink to $(SHARED_TARGET_DIR)"; \
		ln -s "$(SHARED_TARGET_DIR)" target; \
		if [ -d "$(SHARED_TARGET_DIR)" ]; then CARGO_TARGET_DIR="$(SHARED_TARGET_DIR)" cargo clean; fi; \
		mkdir -p "$(SHARED_TARGET_DIR)"; \
	else \
		echo "no target/ — nothing to clean"; \
		echo "  (if this checkout should use a shared build volume, restore the link:"; \
		echo "   make clean SHARED_TARGET_DIR=/Users/Shared/cargo-targets/mcp-methods"; \
		echo "   — see dev-docs/plans/target-symlink-and-clean.md)"; \
	fi
	rm -rf wheels/ dist/ *.egg-info build/

## Size-gated prune of the build dir (doctrine 0.1.9, R4: "a bound checked
## only at milestones is not a bound"). The release gate runs this FIRST —
## before its heavy build — and a phased run should run it after every phase
## commit; on a lean tree it is a free no-op. Measures APPARENT size (`du -A`)
## because the on-disk meter undercounted a real ENOSPC by ~2x; resolves the
## `target` symlink the same way `clean` does so it never deletes the link.
## Override the bound: `make prune-target PRUNE_TARGET_GIB=4`.
PRUNE_TARGET_GIB ?= 12

prune-target:
	@dest=target; \
	if [ -L target ]; then dest=$$(readlink target); case "$$dest" in /*) ;; *) dest="./$$dest" ;; esac; fi; \
	if [ ! -d "$$dest" ]; then echo "prune-target: no build dir at $$dest — nothing to prune"; exit 0; fi; \
	kb=$$(du -skA "$$dest" 2>/dev/null | cut -f1); [ -n "$$kb" ] || kb=$$(du -sk "$$dest" | cut -f1); \
	gib=$$(( kb / 1048576 )); bound=$(PRUNE_TARGET_GIB); \
	if [ "$$kb" -ge $$(( bound * 1048576 )) ]; then \
		echo "prune-target: $$dest is $${gib} GiB apparent (bound $${bound} GiB) — pruning"; \
		CARGO_TARGET_DIR="$$dest" cargo clean || { echo "prune-target: cargo clean FAILED — nothing pruned"; exit 1; }; mkdir -p "$$dest"; \
		echo "prune-target: done; $$( [ -L target ] && echo 'target symlink intact' || echo 'target is a plain dir' )"; \
	else \
		echo "prune-target: $$dest is $${gib} GiB apparent (bound $${bound} GiB) — lean, no-op"; \
	fi

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
