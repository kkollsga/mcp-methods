---
name: repo_management
description: Methodology for the `repo_management` workspace tool — when to clone, update, delete, force-rebuild, and how workspace state interacts with `set_root_dir`.
applies_to:
  mcp_methods: ">=0.3.35"
references_tools:
  - repo_management
  - set_root_dir
references_arguments:
  - repo_management.action
  - repo_management.name
  - repo_management.expire_days
auto_inject_hint: true
---

# `repo_management` methodology

`repo_management` is the workspace-mode control surface: it clones GitHub repos into a workspace directory, swaps which repo is currently active, updates clones to latest, and tears them down on a TTL. It's only registered when the server is started in workspace mode — `tools/list` will not include it otherwise.

## Actions at a glance

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
- **`workspace.kind: local`**: `repo_management` is *not* the right entry point — use `set_root_dir(path)` instead to point at an existing directory on disk. The two modes are mutually exclusive at boot.

Operators see `set_root_dir` *or* `repo_management` in `tools/list`, never both. If the operator wants both behaviours in the same session, they have to restart.

## SHA-gating and the post-activate hook

Workspace mode tracks the last-built SHA per repo. When you `set` or `update`:

1. The clone is brought to latest (`update`) or just looked up (`set`).
2. The current HEAD SHA is compared to the inventory's `last_built_sha`.
3. If they match, the post-activate hook is **skipped** — no expensive rebuild for state that hasn't changed.
4. If they differ (or the entry is brand new), the hook fires and the new SHA is recorded.

This is the "incremental warm cache" behaviour kglite and other downstream graphs depend on. Don't bypass it lightly. `force_rebuild` is the explicit escape hatch.

## TTL cleanup

The workspace's `expire_days` setting controls when inactive clones get cleaned up. Repos that haven't been the active repo within that window are eligible for eviction. The cleanup runs lazily — typically on `list` or on a fresh `set`. `expire_days=0` disables TTL cleanup entirely (operator decision, not yours to override).

## Common patterns

- **First-time setup**: `repo_management(action="set", name="pydata/xarray")`. Clones and binds.
- **Switching context**: `repo_management(action="set", name="numpy/numpy")`. Was on xarray; now on numpy. The previous clone stays on disk for the TTL window.
- **Pulling latest**: `repo_management(action="update")` with no `name` updates the active repo. Pass `name="org/other"` to update a non-active clone (rare but supported).
- **What do I have?**: `repo_management(action="list")` — returns the inventory.
- **Drop a clone**: `repo_management(action="delete", name="org/repo")`. Reclaims disk; removes from inventory.
- **Stale graph despite same git state**: `repo_management(action="force_rebuild", name="org/repo")`. Bypasses the SHA gate.

## When `repo_management` is the wrong tool

- **Reading code from the active repo?** That's `read_source` / `grep`. `repo_management` doesn't read files — it just chooses which checkout the source tools see.
- **Switching to a directory not on GitHub?** That's `set_root_dir` in a local-mode workspace, or a server restart with a different bind. Not `repo_management`.
- **One-off remote inspection without cloning?** Use `github_api` or `github_issues` — they read the API directly, no working tree needed.
