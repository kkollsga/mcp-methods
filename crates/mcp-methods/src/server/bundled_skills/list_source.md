---
name: list_source
description: "List directory contents under the configured source roots in tree format, with optional depth control. TRIGGER when the user wants to see how a repository is organized (\"what's in this repo?\", \"show me the layout\", \"first contact\" exploration), confirm a path exists before passing it to `read_source` or `grep`, drill into a subdirectory to see its modules, or spot conventions (monorepo `crates/`/`packages/`, test layout `tests/`/`test/`, migrations dir). ALSO TRIGGER when an agent has been wandering in `read_source` for a while without finding what it wants — a quick `list_source` of the parent dir often reveals what they missed. SKIP for finding files by content (use grep with a glob), reading file contents (use read_source), or symbol-graph navigation (use cypher_query / equivalent when available)."
applies_to:
  mcp_methods: ">=0.3.35"
references_tools:
  - list_source
references_arguments:
  - list_source.path
  - list_source.depth
auto_inject_hint: true
---

# `list_source` methodology

## Overview

`list_source` returns a tree-formatted directory listing under the configured source root(s). It is the **layout discovery** tool — use it when you need to see how a repository is organised before deciding where to look.

## Quick Reference

| Task | Approach |
|---|---|
| Top-level survey of a new repo | `path: "."` with default `depth: 1` |
| Drill into source / crates | `path: "src/"` or `"crates/"`, `depth: 2` |
| Confirm a path exists | `path: "docs/guides/"` with `depth: 0` |
| Spot conventions (monorepo, tests, migrations) | Top-level survey + skim the directory names |
| Avoid noise from `node_modules`/`target` | Don't list those directly; drill into a subset |

## When to reach for `list_source`

- **First contact with an unfamiliar repo**. `list_source(path=".")` with the default depth gives a useful overview of the top-level layout. From there you can drill: `list_source(path="src/")`, `list_source(path="crates/")`.
- **Confirming a path exists** before passing it to `read_source` or `grep`. Saves a round-trip when you're unsure of the exact name (`api/` vs. `apis/`, `tests/` vs. `test/`).
- **Spotting conventions**. A `crates/`/`packages/`/`apps/` directory tells you it's a monorepo; an `internal/` directory in Go projects signals what's intentionally non-public; a `migrations/` directory tells you where the schema lives.

## Depth control

`depth` defaults to 1 — one level under `path`. For a top-level survey, that's plenty. For deeper drilldown:

- `depth: 2` — useful for `path="src/"` or `path="crates/"` to see one level of submodule structure.
- `depth: 3+` — only when you have a specific reason. Most repos at depth 3 produce noise that's better surfaced via `grep` or a graph query.

`depth: 0` returns just the named directory (its own existence/type), which is occasionally useful as a probe.

## Reading the tree output

`list_source` returns indented tree-style output (`├─`, `└─`) with directory names trailed by `/` and file sizes / line counts noted alongside files when available. Skim for:

- **Top-level config files** (`Cargo.toml`, `package.json`, `pyproject.toml`) — these tell you the build system and dependencies.
- **Build / CI directories** (`.github/`, `ci/`, `scripts/`) — useful when investigating release or test pipelines.
- **Test directories** (`tests/`, `test/`, `__tests__/`) — separate-tests-tree conventions vary by ecosystem.
- **Documentation directories** (`docs/`, `book/`, `dev-documentation/`) — operator-facing context often lives here.

If a directory contains hundreds of entries (vendored deps, generated code, lockfiles), the listing will be long. Consider drilling into a subset rather than reading the full tree at depth 2+ of a `node_modules/` or `target/`.

## Common Pitfalls

❌ Running `list_source` on `node_modules/`, `target/`, or `vendor/` — these have thousands of entries and the listing is unreadable.

❌ Running `depth: 3+` on the repo root — most repos at that depth produce more noise than signal. Drill into a specific subdir instead.

❌ Using `list_source` to find a known file by name — that's a job for `grep` (pattern-by-filename) or, if you really do mean directory layout, `list_source` on the suspected parent.

✅ Survey first, drill second. One `path: "."` call tells you the shape; subsequent calls go where the shape pointed you.

✅ When a path doesn't exist (typo, wrong convention), `list_source` is the cheapest probe. Pass `depth: 0` to just check existence.

## When `list_source` is the wrong tool

- **Looking for files matching a pattern?** `grep` with a `glob` argument finds files by name + content in one call.
- **Finding a specific symbol's home file?** A graph query returns the exact path; `list_source` is for discovery, not lookup.
- **Inspecting file contents?** `list_source` only tells you that a file exists. Once you've located it, switch to `read_source`.

## Common patterns

- **Top-level survey**: `list_source(path=".")` — start here on any unfamiliar repo.
- **Drill into source**: `list_source(path="src/", depth=2)` — see modules and submodules.
- **Confirm a path**: `list_source(path="docs/guides/")` before fetching a specific guide.
- **Spot a missing piece**: `list_source(path="crates/mcp-methods/src/server/")` — if `skills/` isn't listed, the feature isn't wired up.
