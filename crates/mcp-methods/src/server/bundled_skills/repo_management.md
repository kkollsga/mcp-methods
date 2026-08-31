---
name: repo_management
description: "Clone GitHub repos into the workspace, swap which repo is currently active, update clones to latest, force a rebuild, or delete a tracked repo. TRIGGER when the user wants to start work on a new repo (`set` with `name=\"org/repo\"`), pull latest on the active checkout (`update`), see the workspace inventory (`list`), drop a clone to reclaim disk (`delete`), or bypass the last-built-SHA gate (`force_rebuild`). ALSO TRIGGER for phrasings like \"switch to numpy\", \"what repos do I have?\", \"rebuild the graph\" — these map to set / list / force_rebuild. The tool only registers when the server has a `kind: github` workspace configured; sessions with no workspace, or with a `kind: local` one, don't see it at all — local-workspace deployments use `set_root_dir` instead. SKIP for reading code (use read_source/grep — repo_management only chooses *which* checkout the source tools see), one-off GitHub API queries without cloning (use github_api or github_issues), or pointing at a directory that's not on GitHub (use a local-mode workspace with `set_root_dir`)."
applies_to:
  mcp_methods: ">=0.3.35"
applies_when:
  tool_registered: repo_management
references_tools:
  - repo_management
  - set_root_dir
references_arguments:
  - repo_management.action
  - repo_management.name
  - repo_management.expire_days
  - repo_management.revs
auto_inject_hint: true
---

# `repo_management` methodology

## Overview

`repo_management` is the workspace-mode control surface: it clones GitHub repos into a workspace directory, swaps which repo is currently active, updates clones to latest, and tears them down on a TTL. It's only registered when the server is started with a `kind: github` workspace; `tools/list` won't include it otherwise. A `kind: local` workspace uses `set_root_dir` instead — every action here is a GitHub-workspace operation against a clone directory local mode does not have.

## Quick Reference

| Action | Effect |
|---|---|
| `set` | Make `name` the active repo. Clones if not already present. Binds `read_source`/`grep`/`list_source` to it. |
| `update` | Pull latest on `name` (defaults to the active repo). Re-runs the post-activate hook (graph rebuild, etc.) if the SHA changed. |
| `delete` | Remove `name`'s clone and inventory entry. If it's the active repo, the source tools return "no active source" until a new `set` happens. |
| `list` | Return the workspace inventory: every tracked repo, its last-built SHA, last-touched timestamp, and active flag. |
| `force_rebuild` | Re-run the post-activate hook for `name` regardless of SHA. Use when the graph or derived state is stale but git state is unchanged (rare). |

`name` takes the GitHub form `org/repo`. The framework validates it before doing anything.

## Workspace types: github vs. local

- **`workspace.kind: github`** (the common case): `repo_management` clones from GitHub. Repos live under the workspace root (`<workspace_dir>/clones/<org>/<repo>/`).
- **`workspace.kind: local`**: `repo_management` is not registered at all — use `set_root_dir(path)` instead to point at an existing directory on disk. The two modes are mutually exclusive at boot.

The two tools never appear together: `tools/list` carries `repo_management` and not `set_root_dir` in a `kind: github` workspace, `set_root_dir` and not `repo_management` in a `kind: local` one, and neither when no workspace is configured. (Earlier framework versions also listed `repo_management` in local mode, where its clone/update actions could not apply.) Swapping between the two workspace kinds requires a restart.

## SHA-gating and the post-activate hook

Workspace mode tracks the last-built SHA per repo. When you `set` or `update`:

1. The clone is brought to latest (`update`) or just looked up (`set`).
2. The current HEAD SHA is compared to the inventory's `last_built_sha`.
3. If they match, the post-activate hook is **skipped** — no expensive rebuild for state that hasn't changed.
4. If they differ (or the entry is brand new), the hook fires and the new SHA is recorded.

This is the "incremental warm cache" behaviour kglite and other downstream graphs depend on. Don't bypass it lightly. `force_rebuild` is the explicit escape hatch.

## Loading multiple revisions (`revs`)

Pass `revs` to load several git revisions of the repo into one graph
(when the downstream builder supports it — e.g. kglite's multi-rev code
graph). Two forms:

- **`revs=N`** (integer): the last **N version-sorted release tags plus
  HEAD** — e.g. `repo_management(name="numpy/numpy", revs=10)` loads the
  10 newest tags and HEAD. Errors if the repo has no tags.
- **`revs=["v2.0.0", "v2.1.0", "main"]`** (list): those git revspecs
  (tags, branches, or SHAs) verbatim, in the order given. Errors on the
  first revision that doesn't exist.

Resolved revisions are reported on a `revs:` line in the activation
message (oldest→newest, HEAD last), so you can see exactly what got
loaded. A `revs` request **always rebuilds** — the SHA-skip gate applies
only to plain single-revision activations. Omit `revs` for the default
(HEAD-only) behaviour.

## TTL cleanup

The workspace's `expire_days` setting controls when inactive clones get cleaned up. Repos that haven't been the active repo within that window are eligible for eviction. The cleanup runs lazily — typically on `list` or on a fresh `set`. `expire_days=0` disables TTL cleanup entirely (operator decision, not yours to override).

## Common patterns

- **First-time setup**: `repo_management(action="set", name="pydata/xarray")`. Clones and binds.
- **Switching context**: `repo_management(action="set", name="numpy/numpy")`. Was on xarray; now on numpy. The previous clone stays on disk for the TTL window.
- **Pulling latest**: `repo_management(action="update")` with no `name` updates the active repo. Pass `name="org/other"` to update a non-active clone (rare but supported).
- **What do I have?**: `repo_management(action="list")` — returns the inventory.
- **Drop a clone**: `repo_management(action="delete", name="org/repo")`. Reclaims disk; removes from inventory.
- **Stale graph despite same git state**: `repo_management(action="force_rebuild", name="org/repo")`. Bypasses the SHA gate.

## Common Pitfalls

❌ Calling `force_rebuild` to "just be sure" — it bypasses the SHA gate and re-runs the post-activate hook. Use `update` first; only force-rebuild when git state matches but derived state is genuinely stale.

❌ Calling `delete` on the active repo and expecting the next tool call to "just work" — the source tools return "no active source" until a new `set` happens.

❌ Trying to `set` a path on disk that isn't a GitHub repo → that's `set_root_dir` in a local-mode workspace, not `repo_management`.

❌ Using `repo_management(action="update")` when you really wanted to read the latest code → `update` pulls *and* re-runs the hook; if you just want to peek at HEAD, fetch the file via `github_api` instead.

✅ `set` then `update` is the standard flow. The SHA-gate makes `update` cheap when nothing has changed.

✅ `list` is free and idempotent — call it whenever you've lost track of what's bound or what's been touched recently.

## When `repo_management` is the wrong tool

- **Reading code from the active repo?** That's `read_source` / `grep`. `repo_management` doesn't read files — it just chooses which checkout the source tools see.
- **Switching to a directory not on GitHub?** That's `set_root_dir` in a local-mode workspace, or a server restart with a different bind. Not `repo_management`.
- **One-off remote inspection without cloning?** Use `github_api` or `github_issues` — they read the API directly, no working tree needed.
