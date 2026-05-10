# Changelog

## Unreleased

Closing the kglite wishlist gaps (inbox/read/2026-05-10) so the 5 deployed
YAML manifests + ~3,000 LoC of custom Python MCP servers can retire onto
this framework.

### `mcp-server` framework

- **`.env` auto-loading at startup.** Walks upward from the workspace /
  source-root / watch / cwd looking for `.env`; loads `KEY=VALUE` lines
  into the process env (skip blanks/`#`, strip outer quotes, do not
  overwrite existing). Operators who want a non-implicit pick declare
  `env_file: ../.env` at the YAML top level.
- **`github_issues` element drill-down.** New `element_id` / `lines` /
  `grep` / `context` / `refresh` arguments on the MCP tool. FETCH
  responses cache collapsed `cb_N` / `patch_N` / `comment_N` / `overflow`
  elements server-side (via `_mcp_methods::cache::ElementCache`); pass
  `element_id="cb_1"` (with the same `number=N`) to retrieve a single
  element without re-fetching, optionally narrowed by `lines="40-60"` or
  `grep="pat"`.
- **Honest tool listing.** `github_issues` and `github_api` are now
  registered dynamically at boot only when `GITHUB_TOKEN` is set; they
  no longer appear in tool-listing responses when the agent couldn't use
  them. Boot-time decision — restart to pick up a token that appears
  later. The framework binary logs an `info` line announcing the skip.
- **`Workspace::last_built_sha(name)`.** New public reader exposing the
  HEAD SHA persisted after the last successful post-activate hook for a
  repo. Backed by an additive `last_built_sha` field on the per-repo
  `inventory.json` entry (`#[serde(default)]` keeps older inventories
  loading cleanly). Foundation for the auto-rebuild-gating work landing
  in Phase C.
- **Embedder lifecycle (`mcp_server::embedder`).** New module exposing
  `EmbedderHandle` (load/unload/embed/touch + idle tracking) and
  `spawn_idle_watch` for the eviction tokio task. `PythonExtensions`
  now yields `Option<Arc<EmbedderHandle>>` plus `embedder_cooldown` and
  `embedder_watcher`. The framework owns the cooldown timer; the value
  is extracted from `embedder.kwargs.cooldown` in the manifest. Embedder
  classes must expose `embed` + `dimension`; `load` / `unload` are
  optional but called automatically when present.
- **Manifest schema** — new `env_file:` top-level key (added to
  `ALLOWED_TOP_KEYS`; strict-unknown-key validation preserved).

## 0.3.21

- **`McpServer::register_typed_tool<T, F>(name, description, handler)`**
  — typed dynamic-tool registration helper. Compresses the boilerplate
  of building a `Tool` attr from a JSON Schema (via schemars), serde-
  deserialising per-call arguments, and wrapping the handler in a
  `ToolRoute::new_dyn` closure. Domain binaries (kglite-mcp-server) use
  this to register their tools in ~5 lines instead of ~35. The handler
  is `Fn(T) -> String`; state lives in the closure environment.

## 0.3.20

### Added — `mcp-server` crate (Rust-native MCP server framework + binary)

A new sibling crate at `crates/mcp-server/` providing a Rust-native MCP
server built on the official `rmcp` SDK (v1.6) with a stdio transport.
Designed to replace the Python `kglite.mcp_server` over the next few
phases. The new binary is `mcp-server`.

**CLI refit** — drop `--graph` (graph concept lives in downstream
binaries like kglite-mcp-server, not in mcp-methods). Replace with
`--source-root DIR` for direct binding of the source tools to a fixed
directory. Modes are now: `bare` / `--source-root` / `--workspace` /
`--watch`. Help text rewritten to clarify the framework's domain-
agnostic role; downstream binaries (kglite, etc.) layer their domain
tools on top by re-using `McpServer::new` with custom registrations.
Also dropped `--embedder` (the manifest's `embedder:` block remains
the source of truth).

**Library extraction (post-phase-6).** The crate now exposes a
`[lib]` target alongside the `[[bin]]`, with the framework boilerplate
(`apply_python_extensions`, `resolve_source_roots`, `init_tracing`,
`maybe_watch`) lifted into a new `mcp_server::runtime` module. This
lets downstream binaries (kglite-mcp-server, etc.) reuse the entire
boot sequence without copy-pasting hundreds of LoC of glue.

`mcp_server::python::json_to_py` is now `pub` so dynamic-tool
implementations can reuse it for forwarding JSON kwargs to Python
callables (e.g. `graph.describe(**kwargs)`).

**Phase 6** — Workspace mode (`--workspace DIR`). Multi-repo
clone-and-track flow with idle-sweep inventory.

- `repo_management` MCP tool: pass `name='org/repo'` to clone (if
  missing) and activate; `delete=true` to remove; `update=true` to
  fast-forward the active repo; no args to list with access counts.
- Active repo state lives in `Workspace::Arc<RwLock<…>>`; source tools
  pick up the swap on the very next call (`with_workspace` wires
  dynamic source-roots and default-repo providers).
- Inventory persists to `<workspace>/inventory.json` with
  `cloned_at` / `last_accessed` / `access_count` / `stale` per repo;
  reconciles with on-disk state at boot (un-tracked clones get
  inventory entries; vanished repos marked stale).
- Auto-sweeps idle repos older than `--stale-after-days` (default 7);
  active repo is exempt; stale entries preserve their access history
  even after deletion.
- Git operations shell out to `git` (clone --depth 1, fetch + reset
  --hard FETCH_HEAD); no `libgit2` dep.
- `PostActivateHook` callback type lets downstream binaries fire
  custom logic after each successful clone/update — kglite-mcp-server
  will use this to invoke `code_tree::build` on the freshly-activated
  repo and pin the resulting graph.
- Self-contained ISO-8601 (seconds-precision) formatter avoids
  pulling in `chrono` for a handful of timestamps.
- End-to-end test: cloned `rust-lang/rustlings` from github.com,
  `list_source` returned the active repo's tree, `repo_management()`
  listing shows it as `[active]` with access counts.

**Phase 5** — Watch mode (`--watch DIR`). The CLI now spawns a
recursive debounced filesystem watcher (default 500 ms debounce) that
logs change events at INFO level and can fire a downstream-supplied
callback on each batch. Source roots are auto-pinned to the watched
directory so the source tools (`read_source` / `grep` / `list_source`)
operate on the live tree. Powered by the `notify` + `notify-debouncer-mini`
crates. Downstream binaries (kglite-mcp-server, etc.) plug in their
rebuild logic via the `ChangeHandler` callback type — phase-6 work
will wire one in for code-tree rebuilding once kglite layers in.

**Phase 4** — Python extension layer. Embeds CPython into the
binary via PyO3's `auto-initialize` feature so manifest-declared
`python:` tools and custom embedder factories work out of the box.

- Manifest `tools: [{ name, python: ./X.py, function: F }]` entries
  load via `importlib.util` (no `sys.path` mutation), introspect the
  function signature with `inspect.signature` to derive a JSON Schema
  for MCP, and register dynamically on rmcp's tool router via
  `ToolRoute::new_dyn` so the agent sees them in `tools/list`
  alongside the built-in source / GitHub tools.
- Manifest `embedder: { module, class, kwargs }` block is loaded
  + instantiated at boot. The PyObject is held in memory; downstream
  binaries (kglite-mcp-server) wire it to a graph's text-score path
  via their own integration.
- Two-signal trust gating: `python:` tools require both
  `trust.allow_python_tools: true` and `--trust-tools`; embedders
  require `trust.allow_embedder: true` + `--trust-tools`. Refusing
  either way is the default. Boot-time errors are clear and specific.
- Type-hint → JSON Schema mapping: `str → string`, `int → integer`,
  `float → number`, `bool → boolean`, `list → array`, `dict → object`.
  Unmatched annotations fall back to `string` with the Python repr
  in the schema's `title` field. Defaults are JSON-encoded into
  `default`.
- `ServerHandler` impl on `McpServer` switched from `Self::tool_router()`
  (static) to `self.tool_router` (instance) so dynamic routes added
  before serving show up in `tools/list`.
- End-to-end test: a `def greet(name: str, count: int = 3) -> str`
  Python function loads, registers with the right schema (`name`
  required, `count` defaulted), and dispatches correctly through
  `tools/call`.

The FastMCP-API-compat shim (`from mcp_methods.fastmcp import FastMCP`)
is deferred to a follow-up. The manifest form already accepts any
plain Python function — users with existing FastMCP code just declare
each decorated function as a separate manifest entry until the shim
ships.

**Phase 3** — GitHub tools (`github_issues`, `github_api`) registered
on the rmcp server. Both tools resolve the active repo via a
caller-supplied dynamic provider with an optional per-call `repo_name=`
override; auto-detects from cwd's git remote as a last resort.
`github_issues` covers all three modes — FETCH (`number=`), SEARCH
(`query=`), LIST (no args) — by delegating to the existing
`mcp_methods::github` Rust internals (no PyO3 in the hot path). To make
that delegation possible, mcp-methods now ships as both a Python
extension (`cdylib`) and a regular Rust library (`rlib`), with the
`extension-module` PyO3 feature gated behind the new `python-extension`
Cargo feature so `cargo build` can produce both. `pyproject.toml`
maturin features updated accordingly. End-to-end stdio test against
github.com/rust-lang/rustlings confirmed.

**Phase 2** — source tools (`read_source`, `grep`, `list_source`)
registered on the rmcp server. Each tool is gated on the server having
an active source-roots provider (static or dynamic); when none is
configured, the tool returns a friendly "configure source_root in your
manifest" error rather than crashing. Implementation lives in
`crates/mcp-server/src/source.rs` and uses the same `ignore` +
`grep-matcher`/`grep-regex`/`grep-searcher` crates as the existing
mcp-methods primitives. `ServerOptions` gains `with_static_source_roots`
+ `with_dynamic_source_roots`. Manifest `source_root(s)` are now
canonicalised at boot and wired into the server. 15 new unit tests
covering read/grep/list semantics, glob filtering, traversal blocking.

**Phase 1** — bootstrap. Boots a working MCP server with the framework
wired end-to-end, plus the manifest schema parsed and validated. No
real tools yet — that's phase 2+.

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
