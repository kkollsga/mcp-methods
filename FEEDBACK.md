# mcp-methods feedback — from MCP server integration

Version: 0.3.1

## `git_diff` and shallow clones

The open-source MCP server clones repos with `git clone --depth 1`. This means only the latest commit exists locally. `git_diff(base, head)` will fail for most useful comparisons:

- `diff("main", "v2.0")` — tag doesn't exist in shallow clone
- `diff("abc123", "HEAD")` — old commit not available
- `diff("main~5", "main")` — no history

The only diff that works reliably is `diff("HEAD~0", "HEAD")` — effectively nothing.

### Options

1. **Library-side**: `git_diff` could detect a shallow clone and auto-fetch the missing refs before diffing. `git fetch --depth=N origin base head` would pull just enough history.

2. **Library-side**: Add a `fetch_depth` parameter to `git_diff` that fetches the needed commits on demand: `git_diff("v2.0", "v2.1", fetch_depth=100)`.

3. **Server-side**: The MCP server could unshallow on demand. But this defeats the purpose of shallow clones (fast, small).

4. **GitHub API fallback**: For comparing tags/branches, `github_api("compare/v2.0...v2.1")` works without any local clone. `git_diff` could fall back to this when the local refs aren't available.

Option 4 is probably the most pragmatic — try local git diff first, fall back to GitHub compare API if refs are missing. No need to unshallow.

## `github_discussions` — unified listing + fetch

Works well as a unified interface. The MCP server exposes it as `github_discussion` (singular) which handles both:
- `github_discussion()` — list mode (issues, PRs, filtered)
- `github_discussion(number=N)` — single fetch with compaction via `ElementCache`

The `kind` parameter ("issue", "pr", "all") is clear for listing. The `expand` / `element_id` / `grep` parameters activate in fetch mode. Docstring examples make the dual-mode intuitive.

## Shipped and working well

- `list_dir` with `annotate` callback — clean integration with graph-derived loc counts
- `ripgrep` with `max_results` (renamed from `head_limit`) and `relative_to`
- `ElementCache.fetch_discussion` (renamed from `fetch_issue`) — compaction + caching
- `github_discussions` listing mode with `kind`, `state`, `sort`, `labels`
