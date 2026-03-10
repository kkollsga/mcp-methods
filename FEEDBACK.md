# mcp-methods feedback — from MCP server integration

Version: 0.3.3

## `github_discussions` — unified listing + fetch

Works well as a unified interface. The MCP server exposes it as `github_discussion` (singular) which handles both:
- `github_discussion()` — list mode (issues, PRs, filtered)
- `github_discussion(number=N)` — single fetch with compaction via `ElementCache`

The `kind` parameter ("issue", "pr", "all") is clear for listing. The `expand` / `element_id` / `grep` parameters activate in fetch mode. Docstring examples make the dual-mode intuitive.

## PR diffs as collapsed elements — 0.3.3

PR patches are now collapsed into `patch_N` cache elements in the compact view. The agent drills into specific files with `element_id="patch_3"`, searches within patches with `grep="pattern"`, or slices with `lines="10-30"`. This replaces the standalone `git_diff` function — for tag/branch comparisons outside PR context, use `git_api("compare/v1.0...v2.0")`.

## `fetch_discussion` caching with refresh

`fetch_discussion` now returns a cached summary on subsequent calls (no re-fetch). Pass `refresh=True` to force a fresh fetch when the discussion has changed.

## Shipped and working well

- `list_dir` with `annotate` callback — clean integration with graph-derived loc counts
- `ripgrep` with `max_results` (renamed from `head_limit`) and `relative_to`
- `ElementCache.fetch_discussion` (renamed from `fetch_issue`) — compaction + caching
- `github_discussions` listing mode with `kind`, `state`, `sort`, `labels`
