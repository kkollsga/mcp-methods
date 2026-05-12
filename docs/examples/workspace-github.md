# Example: GitHub Workspace

Clone-and-track multiple GitHub repos. The agent uses `repo_management` to activate and update repos on the fly.

## CLI

```bash
mcp-server --workspace /tmp/repos
```

Or with an auto-detected manifest at `/tmp/repos/workspace_mcp.yaml`:

```bash
mcp-server --workspace /tmp/repos
# Reads /tmp/repos/workspace_mcp.yaml automatically
```

## Manifest

```yaml
name: GitHub Workspace
instructions: |
  Use repo_management to clone or activate a GitHub repo, then use the
  source tools (read_source / grep / list_source) to explore it. Use
  github_issues to read issue and PR discussions, git_api for the full
  REST surface.

# No source_roots — the active repo's root is dynamic.

trust:
  allow_python_tools: false
  allow_embedder: false
  allow_query_preprocessor: false

builtins:
  save_graph: false
  temp_cleanup: never

env_file: .env       # for GITHUB_TOKEN
```

`.env` resolution walks upward from the workspace dir if `env_file:` is unset.

## Agent flow

```text
agent → repo_management("rust-lang/cargo")
         framework → clones rust-lang/cargo to /tmp/repos/rust-lang/cargo
                  → fires post-activate hook (no-op in generic mcp-server)
                  → updates active_repo inventory entry
                  → returns "Cloned 'rust-lang/cargo' at /tmp/repos/rust-lang/cargo"

agent → list_source(".", depth=2)
         framework → walks the active repo (/tmp/repos/rust-lang/cargo) at depth 2

agent → grep(r"fn build_command")
         framework → ripgreps the active repo

agent → repo_management("rust-lang/rust")
         framework → clones rust-lang/rust to /tmp/repos/rust-lang/rust
                  → switches active_repo to rust-lang/rust
                  → fires post-activate hook
                  → source tools now point at the new repo

agent → github_issues(repo="rust-lang/rust", number=12345)
         framework → fetches via GitHub API (using GITHUB_TOKEN from .env)
```

## Auto-rebuild gating

`repo_management(name, update=true)` fetches origin and resets HARD. If the SHA hasn't moved, the post-activate hook is skipped (`action == "current"`). If you need the hook to fire unconditionally, use `force_rebuild=true`.

## Idle-sweep

The framework auto-removes repos that have been idle longer than `--stale-after-days` (default 7). This keeps the workspace from accumulating stale clones.

## See also

- [Operating Modes](../guides/operating-modes.md) — full mode table
- [Watch + Workspace](../guides/watch-and-workspace.md) — local-mode atomic-swap deep dive
