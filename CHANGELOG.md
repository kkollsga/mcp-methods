# Changelog

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
- **`git_diff(base, head)`** — compare two commits/branches with full diff or stat-only summary. Supports `path_filter` for glob-based file filtering and configurable `context` lines.
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
