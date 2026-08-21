---
name: github_issues
description: "Search, list, or fetch GitHub issues, pull requests, and Discussions, with smart compaction and drill-down via element_id. TRIGGER when the user names a specific issue/PR/discussion number (FETCH mode with `number=N`), wants to search across topics by keyword (SEARCH mode with `query=\"...\"`), wants to see recent open items in a repo (LIST mode, no args), or wants to drill into a collapsed element (code block `cb_N`, patch `patch_N`, long comment `comment_N`, or `overflow`) inside a previously-fetched issue using `element_id=`. ALSO TRIGGER for questions like \"what's been broken lately?\", \"how was this handled before?\", \"read PR 1234\" — these map cleanly to LIST / SEARCH / FETCH respectively. The tool only registers when the manifest opts in with `builtins.github: true` *and* a `GITHUB_TOKEN`/`GH_TOKEN` is reachable at boot; if it doesn't appear in `tools/list`, the manifest opt-in is missing or the server has no token. SKIP for commits, branches, releases, or repo metadata (use github_api), reading the actual code a PR touches (use read_source against the local checkout — the conversation tells you *why*, the code tells you *what*), or authoring/commenting on issues (out of scope, read-only)."
applies_to:
  mcp_methods: ">=0.3.35"
applies_when:
  tool_registered: github_issues
references_tools:
  - github_issues
references_arguments:
  - github_issues.number
  - github_issues.query
  - github_issues.kind
  - github_issues.state
  - github_issues.element_id
  - github_issues.lines
  - github_issues.grep
  - github_issues.refresh
auto_inject_hint: true
---

# `github_issues` methodology

## Overview

`github_issues` is a **three-mode tool**: FETCH (a specific issue, PR, or discussion by number), SEARCH (free-text across the repo's issues, PRs, and discussions), and LIST (recent open items). Choosing the right mode is the difference between a one-call answer and a ten-call wild goose chase.

## Quick Reference

| Task | Approach |
|---|---|
| "Read PR 1234" / "show issue #42" | FETCH: `number=N`. Then `element_id="patch_1"` for the diff. |
| "What's been broken lately?" | LIST: no args, defaults to `state="open"`, `kind="all"`. |
| "How was this handled before?" | SEARCH: `query="<topic>", state="closed"`. |
| "Anything tagged `bug` and `help-wanted`?" | LIST or SEARCH with `labels="bug,help-wanted"`. |
| Drill into a collapsed code block | FETCH again with `element_id="cb_1"`, optional `lines=` / `grep=`. |
| Output got truncated | FETCH again with `element_id="overflow"` for the spilled tail. |
| Bypass the ElementCache | `refresh=true` (rare — use when the issue is actively updated). |

## Mode dispatch — decision tree

```
What do you want?

1. A specific issue/PR/discussion you already have a number for?
   └── FETCH: github_issues(number=N).
       Optional: kind="pr" if you want only PRs and ambiguity is possible.

2. To search by topic / keyword?
   └── SEARCH: github_issues(query="rate limiting", state="closed").
       Quote multi-word phrases. Use state="all" if recency matters
       less than completeness.

3. To browse recent open items (triage, "what's happening"?)
   └── LIST: github_issues() — no args.
       Narrow with kind="pr" or labels="..." when you know what
       you're after.

4. You FETCHed something and the response had [cb_N] / [patch_N]
   / [comment_N] / element_id="overflow" markers?
   └── Drill: github_issues(number=N, element_id="cb_1",
                            lines="10-25", grep="...").
```

## Mode dispatch — what triggers what

- **FETCH** when `number=N` is set. Returns the full conversation for that issue/PR/discussion, with smart compaction (see below).
- **SEARCH** when `query="..."` is set (and `number` is not). Returns a list of matches across the repo. Use this when you have a *topic* (e.g. "rate limiting", "ipv6 support") but not a number.
- **LIST** when neither `number` nor `query` is set. Returns recent items, filtered by `state` (default "open") and `kind` (default "all"). Useful for triage and "what's been happening recently?" questions.

`kind` ∈ `"issue"` / `"pr"` / `"discussion"` / `"all"` (default). Most workflows touch all three — leave it as `"all"` unless you specifically want to scope down.

## FETCH compaction and `element_id` drill-down

When you FETCH an issue or PR, the response collapses large content to keep the agent's context window healthy:

- **Code blocks** longer than ~30 lines collapse to `[cb_N: collapsed N-line code block]`.
- **Diff patches** in PRs collapse to `[patch_N: collapsed N-line patch]`.
- **Long maintainer comments** keep their head + tail; long non-maintainer comments truncate harder.
- **Bot comments** filter out by default.

Each collapsed element has a stable ID (`cb_1`, `cb_2`, `patch_1`, `comment_3`, …). To drill into one:

```
github_issues(number=42, element_id="cb_1")
```

That returns *just* the named element, uncompressed. Add `lines="10-25"` to slice within a code block, or `grep="pattern"` to filter to matching lines inside it. The `comment_N` IDs work the same way.

The whole-response cap is also enforced: if the FETCH itself exceeded the budget, the result ends with `element_id="overflow"` — call back with that to read the spilled tail.

## SEARCH efficiency

- Quote multi-word phrases: `query="\"rate limiting\""` finds the phrase, not the loose union.
- Combine with `state`: `state="closed"` when looking for prior resolutions; `state="all"` when you want history regardless.
- `labels="bug,help-wanted"` (comma-separated) filters server-side. Cheaper than scrolling LIST output.
- `limit=10` (default 20) when you only need a handful of strong matches.

## Common Pitfalls

❌ Calling `github_issues(query="...")` *and* `number=N` in the same call → `number` wins; SEARCH is silently ignored.

❌ Reading the actual code a PR touches via the PR comments → use `read_source` / `grep` against the local checkout. The PR tells you *why*; the code tells you *what*.

❌ Treating `[cb_3]` as the answer — it's a marker. Drill with `element_id="cb_3"` to get the actual content.

❌ Re-FETCHing the same issue between drills → the ElementCache already has it. Drills are cheap; the only reason to refresh is when the upstream issue has changed.

✅ FETCH once, drill many times. The cache is per-issue and survives across `element_id=` calls.

✅ When SEARCH returns nothing, broaden the query before broadening the state filter — closed issues are usually irrelevant noise unless the topic is historical.

## When `github_issues` is the wrong tool

- **Reading the actual code that a PR touches?** FETCH the PR for the conversation and rationale, then use `read_source` / `grep` against the local checkout for the actual file content. Don't try to read code via the PR comments alone.
- **Anything beyond issues/PRs/Discussions** (commits, branches, releases, repo metadata) → use `github_api`. `github_issues` is scoped to those three types.
- **Authoring or commenting on issues** → out of scope. `github_issues` is read-only.

## Common patterns

- **"What's been broken lately?"**: `github_issues()` with default args (LIST recent open).
- **"How was this handled before?"**: `github_issues(query="<topic>", state="closed")`.
- **"Read PR 1234"**: `github_issues(number=1234)`. Then `element_id="patch_1"` for the diff.
- **"What's in that big code block from the bug report?"**: drill via `element_id="cb_2"` with optional `lines=` / `grep=`.
- **"Force a re-fetch"**: `refresh=true` bypasses the ElementCache (rare; use when the issue is being actively updated and you need fresh state).

## Authorisation

The tool is registered only when *both* gates pass at boot: the manifest opts in with `builtins.github: true` (default off — a reachable token is not an opt-in), and a `GITHUB_TOKEN` / `GH_TOKEN` is actually reachable (env, or the manifest's `env_file`). If you don't see it in `tools/list`, check the manifest opt-in first, then token reachability — either way it's a deployment issue, not a usage issue. Both decisions are boot-time; a token or manifest change that appears later needs a server restart.
