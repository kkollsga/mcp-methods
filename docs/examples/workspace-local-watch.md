# Example: Local Workspace + Watch

Bind a fixed local directory + auto-rebuild on filesystem changes. Used by downstream binaries that maintain derived artifacts (a code graph, a search index, a documentation tree).

## Manifest

```yaml
name: Local Workspace with Watch
instructions: |
  This server is bound to a local directory and auto-rebuilds the
  downstream code graph on filesystem changes. Use set_root_dir(path)
  to swap to a different local project.

workspace:
  kind: local
  root: ./project
  watch: true

trust:
  allow_python_tools: false
  allow_embedder: false
  allow_query_preprocessor: false

builtins:
  save_graph: false
  temp_cleanup: never
```

The manifest's `workspace:` block wins over CLI flags. Run with:

```bash
mcp-server --mcp-config local_watch_mcp.yaml
```

(No `--workspace` flag — the manifest provides everything.)

## What happens at boot

1. Framework parses manifest, sees `workspace.kind: local`.
2. Canonicalizes `workspace.root: ./project` relative to the YAML's parent dir.
3. Constructs a `Workspace::open_local(canonical_root, post_activate_hook)`.
4. Sees `workspace.watch: true`, spawns a `notify-debouncer-mini` watcher.
5. Binds source tools to `active_repo_path` (initially the canonical root).
6. Serves over stdio.

## Atomic root swap

```text
agent → set_root_dir("/path/to/other-project")
         framework → canonicalizes path
                  → acquires RwLock write on active_repo_path
                  → writes new path under the lock
                  → fires post-activate hook against the new path
                  → returns success

# Subsequent reads from source tools see the new root atomically.
agent → list_source(".", depth=2)
         framework → walks /path/to/other-project (the new active root)
```

The atomic-swap pattern is the [0.3.28 fix](../changelog.md). Pre-fix, `set_root_dir` was clobbering the just-set path back to the configured `workspace_dir` because `clone_or_update`'s local-mode branch always returned `workspace_dir` instead of consulting the just-set `active_repo_path`. Fixed by making the branch read state.

## Filesystem watcher

With `watch: true`, the framework watches the active root for changes. Events are debounced for 250ms (default). After the debounce window closes:

- The framework's post-activate hook fires against the active root
- Downstream binaries can register a rebuild callback via `maybe_watch(Some(root), Some(change_handler))`

The generic `mcp-server` CLI doesn't have a meaningful rebuild action — the hook is for downstream binaries.

## Downstream binary usage

```rust
let workspace = Workspace::open_local(
    root.clone(),
    Some(Arc::new(move |root, name| {
        eprintln!("rebuilding artifacts for {name} at {}", root.display());
        rebuild_my_code_graph(root)?;
        Ok(())
    })),
)?;
```

When `set_root_dir` is called or the watcher fires, the hook runs with the new root.

## See also

- [Watch + Workspace (guide)](../guides/watch-and-workspace.md) — deeper dive
- [Operating Modes](../guides/operating-modes.md) — full mode table
- The 0.3.28 entry in [CHANGELOG](../changelog.md) — the bug + fix history
