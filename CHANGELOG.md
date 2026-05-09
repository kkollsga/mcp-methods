# Changelog

## Unreleased

### Added — `mcp-server` crate (Rust-native MCP server framework + binary)

A new sibling crate at `crates/mcp-server/` providing a Rust-native MCP
server built on the official `rmcp` SDK (v1.6) with a stdio transport.
Designed to replace the Python `kglite.mcp_server` over the next few
phases. The new binary is `mcp-server`.

**Phase 1 (this release)** — bootstrap. Boots a working MCP server with
the framework wired end-to-end, plus the manifest schema parsed and
validated. No real tools yet — that's phase 2+.

- Cargo workspace conversion of `mcp-methods` (root crate unchanged;
  `crates/mcp-server/` added as a sibling member).
- YAML manifest parser at `crates/mcp-server/src/manifest.rs` — direct
  port of the Python `kglite.mcp_server.manifest` schema. Same keys
  (`name`, `instructions`, `overview_prefix`, `source_root(s)`,
  `trust.{allow_python_tools, allow_embedder}`, `tools`, `embedder`,
  `builtins.{save_graph, temp_cleanup}`), same validation messages,
  same auto-detection of sibling (`<basename>_mcp.yaml`) and
  workspace-level (`workspace_mcp.yaml`) manifests.
- `clap`-based CLI matching the Python flags: `--graph` /
  `--workspace` / `--watch` (mutually exclusive), `--mcp-config`,
  `--embedder`, `--name`, `--trust-tools`, `--stale-after-days`.
- rmcp `ServerHandler` impl with one `ping` tool to verify the
  framework dispatch is wired. End-to-end stdio handshake confirmed
  (initialize → tools/list → tools/call).
- 25 unit tests covering manifest parsing edge cases and CLI mode
  picking.

## 0.3.19

- **Built-in `html_to_text` function** — converts HTML to clean, readable text optimized for LLM consumption. Strips `<head>`, `<script>`, `<style>`, and comments. Converts headings to markdown `#` prefixes, list items to `- ` bullets, bold/strong to `**text**`, images to `[image: alt]`, tables to tab-separated text, and links to plain text. Decodes 75+ named HTML entities (including Scandinavian æøå) plus numeric/hex references. Available as a standalone function (`from mcp_methods import html_to_text`) and as a string-based transform on `read_file` (`transform="html"`).
- **String-based `transform` on `read_file`** — `transform` now accepts `"html"` in addition to callables. The built-in html transform runs *after* section extraction (so `id` attributes remain available) but *before* grep (so patterns match clean text, not raw tags). Callable transforms preserve existing behaviour.

## 0.3.17

- **Neutral error for non-existent items** — fetching a number that doesn't exist as an Issue, PR, or Discussion now returns `#N not found in repo (checked Issues, PRs, and Discussions)` instead of leaking the GraphQL fallback error (`"Could not resolve to a Discussion"`).

## 0.3.16

- **Renamed `github_discussions` → `github_issues`** — one tool, three modes: FETCH (by number), SEARCH (by query), LIST (default). `github_discussions` remains as a backward-compat alias.
- **Search mode** — `query="datatree coordinates"` searches via REST `search/issues` for issues+PRs, and GraphQL `search(type: DISCUSSION)` for Discussions. `kind` routes to the right API; `kind="all"` runs both and concatenates results. Sort defaults to relevance for search, `"created"` for listing.
- **Renamed `ElementCache.fetch_discussion` → `fetch_issue`**.
- **`labels` parameter simplified** — now a comma-separated string (`"bug,P0"`) instead of `Vec<String>`.
- **`sort` parameter now optional** — defaults depend on mode: `None` (relevance) for search, `"created"` for listing.

## 0.3.15

- **GitHub Discussions support (GraphQL)** — `fetch_discussion` and `github_discussions` now transparently handle GitHub Discussions, not just Issues and PRs. When a REST lookup returns 404, the library falls back to the GraphQL API to fetch the Discussion with full threaded comments (top-level + nested replies), category, and answered status. Listing mode supports `kind="discussion"` to query Discussions via GraphQL with state and sort filtering. Ref collection also extracts GitHub references from threaded reply bodies.

## 0.3.14

- **`max_matches` parameter for `read_file` grep** — limit the number of matches returned when grepping within a file. When a dense document has 125 hits, `max_matches=20` returns the first 20 with context, and the header changes to "showing 20 of 125 matches". The ripgrep module already had `max_results`/`match_limit`; this brings the same control to single-file grep. Also improved truncation footers: when `max_chars` cuts grep output, the footer now includes the total match count (e.g., "125 matches, 279865 chars total") so you know whether to refine the pattern or increase the limit.

## 0.3.13

- **`grep` parameter for `read_file`** — search within a single file and return only matching lines with context. Avoids reading entire large documents into context when only specific passages are needed. Supports `grep_context` (default 2) for surrounding lines, merges overlapping context windows, works with `section`, `start_line`/`end_line`, `transform`, and `max_chars`. Uses `--` separators between non-contiguous match groups, consistent with ripgrep output.

## 0.3.12

- **UTF-8 fix for `section` extraction** — the byte-stepping loop panicked on multi-byte characters (§, æ, ø, å, é, etc.) inside extracted sections. Now advances by full UTF-8 codepoints. Also hardened all `max_chars` truncation sites to avoid splitting multi-byte characters.

## 0.3.11

- **`section` parameter for `read_file`** — extract an HTML element by its `id` attribute. Solves single-line HTML navigation: instead of getting 127KB on line 96, request `section="PARAGRAF_4-7"` to get just that element. Infers tag name from the matched opening tag, handles nested elements of the same type, works with `transform` and `max_chars`.

## 0.3.10

- **Comment TOC** — bare `element_id="comments_middle"` returns a lightweight table of contents (`_index`, `author`, `created_at`, 80-char snippet) instead of dumping raw JSON.

## 0.3.9

- **`lines` on array elements** — `lines="1-20"` on `comments_middle` returns structured comment objects by index range.
- **`_index` on highlights** — maintainer highlights include `_index` for drill-down.
- **Grep metadata on comments** — grep matches include `author`, `created_at`, `comment_index`, and `element_id`.

## 0.3.7

- **Thread digest** — discussions with 50+ comments are condensed: first 5 + maintainer highlights + last 5 inline, middle cached as `comment_N` and searchable `comments_middle`.
- **Bookend pagination** — fetches first 5 + last 5 comment pages, skipping the middle. Prevents freezing on huge threads.
- **Lazy related refs** — cross-references listed as `related_refs` instead of eagerly fetched.
- **Unicode safety** — fixed byte-slicing panics on multi-byte UTF-8 characters.

## 0.3.6

### Improvements

- **Budget-based adaptive compaction** — `fetch_discussion` / `compact_discussion` now start with full content and only compact what's needed to fit within a byte budget (default 60KB). Small/medium PRs return fully expanded with no user effort. Large PRs gracefully degrade through 9 progressive tiers (bot filter → code blocks → comments → large patches → body → reviews → all patches → aggressive).
- **Removed `expand` parameter** — no longer needed. Compaction is fully automatic based on size constraints. Compacted content is always available via `element_id` drill-down.
- **Per-item budget** (default 15KB) prevents any single patch, comment, or body from consuming more than 25% of the total budget, ensuring balanced output even when one file dominates a PR.
- **`budget` / `item_budget` parameters** on `compact_discussion` for power users who want to tune output size.
- **`_compaction` metadata** in output describes what was compacted and at which tier, so callers know what to drill into.

## 0.3.5

### Improvements

- **Smart diff sizing** — small PR diffs (≤200 total lines) are shown inline with per-file collapsing for large individual patches. Large diffs show a navigation tree with `+/-` counts; drill into specific files via `element_id="patch_N"`. All patches are always cached regardless of size.
- **Review comments as `review_N` cache elements** — inline review comments are now individually drillable via `element_id="review_3"` with `grep` and `lines` support. Previews extended from 1 line/120 chars to 3 lines/300 chars.

## 0.3.4

### Fixes

- **`list_dir` annotate callback paths** — the callback now receives paths relative to `relative_to` (e.g. `src/core/engine.py`) instead of bare filenames (`engine.py`). This was a bug that made it impossible to match callback paths against external data sources (knowledge graphs, databases) when listing subdirectories.

## 0.3.3

### Improvements

- **PR diffs as collapsed cache elements** — when fetching a PR via `ElementCache.fetch_discussion`, diff patches are automatically collapsed into `patch_N` cache elements. Each stores filename, additions/deletions, and full diff text. Drill into specific files with `element_id="patch_3"`, search within patches with `grep="pattern"`, or slice with `lines="10-30"`. Fits the existing progressive-disclosure pattern alongside `cb_N`, `comment_N`, and `details_N`.
- **`refresh` flag on `fetch_discussion`** — subsequent calls for the same `(repo, number)` now return a cached summary instead of re-fetching. Pass `refresh=True` to force a fresh fetch when the discussion has changed.
- **Removed `git_diff`** — PR diffs are better served as collapsed elements in `fetch_discussion` (progressive disclosure). For comparing tags/branches outside a PR context, use `git_api("compare/v1.0...v2.0")`.
- Removed `globset` direct dependency (was only used by `git_diff`)

## 0.3.2

### Improvements

- **`git_diff` GitHub API fallback** — when local `git diff` fails (shallow clones, missing refs), automatically falls back to the GitHub compare API (`/repos/{repo}/compare/{base}...{head}`). Repo is auto-detected from git remote or can be passed explicitly via the new `repo` parameter.

## 0.3.1

### Fixes

- Fixed CI import check referencing removed `git_issue` export
- Fixed stale `git_issue` reference in `ElementCache` error message

## 0.3.0

### Breaking changes

- **Renamed `head_limit` → `max_results`** across the API. In `ripgrep()`, the parameter is now `max_results`. In `ripgrep_files()`, the old `max_results` (engine-level early termination) is now `match_limit`, and `head_limit` (output truncation) is now `max_results`.
- **Renamed `git_issue` → `github_discussions`**. Now supports both fetching a single discussion and listing discussions with filters.
- **Renamed `ElementCache.fetch_issue` → `ElementCache.fetch_discussion`**.

### New features

- **`github_discussions` listing mode** — list issues, PRs, or both with `kind`, `state`, `sort`, `limit`, and `labels` filters. Auto-detects repo from git remote when `repo` is omitted.
- **`list_dir` annotate callback** — optional `annotate` parameter accepts a callable that receives each entry's relative path and returns an annotation string (e.g. `"(144 loc)"`). The tree formatter handles column alignment within each directory level.

## 0.2.11

### New features

- **`list_dir`** — tree-formatted directory listing with depth control, glob filtering, `.gitignore` support, directory summaries, and `relative_to` for path display.

### Performance optimizations

- Eliminated double filesystem walk in `list_dir` (merged into single walk at depth+1)
- Replaced HashSet+sort+dedup with O(n) sorted merge for context line merging in grep
- Thread-local buffering with `FlushGuard` Drop pattern in parallel walker (one lock per thread instead of per file)
- Thread-local single-entry regex cache in `json_grep` and `cache` modules
- Pre-canonicalize `allowed_dirs` in `read_file` to reduce filesystem stat calls
- `eq_ignore_ascii_case()` replacing `.to_lowercase()` allocation in HTML tag detection
- `LazyLock<HashSet<&'static str>>` for zero-allocation default skip directories

## 0.2.10

### Bug fixes

- Fixed `head_limit` semantics: changed from `int` (0 = unlimited) to `int | None` (None = unlimited)
- Added `relative_to` parameter to `ripgrep()` wrapper

## 0.2.9

### Bug fixes

- Removed default `max_results` limit that silently truncated search results

## 0.2.8

### Changes

- Renamed all `grep` methods to `ripgrep` across the codebase
- Dropped deprecated `macos-13` CI runners from wheel build workflow
- Removed tracked `__pycache__` files from git

## 0.2.7

### Changes

- Fixed wheel build workflow for cross-platform distribution

## 0.2.5

### Improvements

- Searcher reuse in transform (callback) path — build searcher and sink once, reuse across files
- Fast path for no-context matches in `format_content` (skip HashSet/sort/dedup)
- Context merge moved inside the context branch to avoid allocation when unused

## 0.2.3

### Major rewrite — Rust conversion

- Rewrote core from pure Python to Rust via PyO3/maturin
- **grep**: Uses `grep-regex`, `grep-searcher`, and `ignore` crates (parallel file walking, mmap, SIMD literal optimization, `.gitignore` support)
- **GitHub integration**: `git_issue`, `git_api`, `ElementCache` with drill-down caching, text compaction
- **File I/O**: `read_file` with path traversal protection
- **Compaction**: `compact_discussion`, `collapse_code_blocks`, `compact_text`
- Added `ripgrep()` Claude Code Grep-compatible wrapper
- Added CI (cargo fmt, clippy, pytest) and wheel build workflows

## 0.1.1

- Packaging fix

## 0.1.0

- Initial release — pure Python implementation
