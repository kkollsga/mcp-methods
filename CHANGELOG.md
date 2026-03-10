# Changelog

## 0.3.7

### Improvements

- **Thread digest for large discussions** — discussions with 50+ comments are automatically condensed: first 5 comments + up to 15 maintainer highlights (with `_element_id` for drill-down) + last 5 comments. Middle comments are cached individually as `comment_N` elements and as a searchable `comments_middle` segment.
- **Bookend pagination** — comment fetching now retrieves first 5 + last 5 pages (skipping the middle) instead of all pages sequentially. Timeline capped at 3+2 pages. Prevents freezing on huge issues (e.g. numpy#10161, vscode#10121).
- **Related discussions replaced with ref list** — instead of eagerly fetching up to 10 related issues/PRs, cross-references are listed as `related_refs` for the agent to dive into on demand. Eliminates cascading fetch delays.
- **Structured content drill-down** — `ElementCache.retrieve()` now supports `grep` on JSON array/object content (not just strings), enabling search within `comments_middle` and other structured cache elements.
- **`comment_count` field** — total comment count from the GitHub API is included in the output, so agents know the full thread size even when comments are digested.

### Fixes

- **Unicode safety** — fixed byte-slicing panics on multi-byte UTF-8 characters (em-dash, smart quotes) in `compact_text`, overflow preview, and `git_api` truncation. Added `safe_byte_index()` helper.
- **HTML tag detection** — replaced `stripped[..8]` byte slicing with `starts_with_ignore_ascii_case()` to handle non-ASCII content safely.

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
