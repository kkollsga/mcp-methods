# Watch + Workspace Modes

Deep dive on the framework's two stateful modes: filesystem watching and the local-workspace atomic-swap pattern. Both are designed for downstream binaries that maintain derived artifacts (a code graph, a search index, a knowledge base) and need to rebuild them on file changes or active-root swaps.

## The PostActivateHook

The framework's stateful modes fire a single callback type:

```rust
pub type PostActivateHook =
    Arc<dyn Fn(&Path, &str) -> Result<()> + Send + Sync>;
```

The hook receives the active root path and the active repo name. Return `Ok(())` to signal success (the framework records the new HEAD SHA in the inventory); return `Err(...)` to signal failure (the SHA is NOT recorded, so the next `update=True` retries the build).

Provide the hook at construction time:

```rust
use mcp_methods::server::workspace::{Workspace, PostActivateHook};

let hook: PostActivateHook = Arc::new(|root, name| {
    println!("rebuilding artifacts for {name} at {}", root.display());
    // your rebuild logic here
    Ok(())
});

let workspace = Workspace::open(workspace_dir, 7, Some(hook))?;
// or for local mode:
let workspace = Workspace::open_local(local_root, Some(hook))?;
```

## Workspace (GitHub) mode

```bash
mcp-server --workspace /tmp/repos
```

Clone-and-track. The hook fires after every successful `repo_management` action:

- `repo_management("org/repo")` → clone if new, fast-forward if existing, fire hook
- `repo_management("org/repo", update=true)` → fetch + reset HARD, fire hook
- `repo_management(delete="org/repo")` → remove repo, no hook fire

The framework's auto-rebuild gate skips the hook when `force_rebuild=false` AND the repo is already at the HEAD it was last built at (`action == "current"` AND `prev_built_sha == new_head`). This makes `repo_management(update=True)` cheap when upstream hasn't moved.

## Workspace (local) mode

```yaml
workspace:
  kind: local
  root: ./repo
  watch: true
```

Local-bind mode does NOT clone. It binds a fixed directory as the active source root, with two key behaviours:

### Atomic root swap

The `set_root_dir(path)` tool swaps the active root at runtime:

```text
agent → set_root_dir("/other/path")
framework → canonicalize, write to active_repo_path under RwLock,
            fire hook against new path, return success
```

Concurrent reads from the source tools see the new path atomically once the RwLock write completes.

**Note:** the [0.3.28 fix](../changelog.md) corrected a bug where `set_root_dir` was clobbering the just-set path back to the configured `workspace_dir`. If you're on a pre-0.3.28 pin, upgrade.

### Bounding the swap: `sandbox_root`

The swap above is **unbounded** — `set_root_dir` accepts any directory on the filesystem. That is the default and it does not change. When a deployment wants an outer boundary, declare one:

```yaml
workspace:
  kind: local
  root: ./repos/active        # the initial root
  sandbox_root: ./repos       # the outer boundary swaps may never leave
  watch: true
```

With `sandbox_root` set:

```text
agent → set_root_dir("./repos/other")   → activates (inside the boundary)
agent → set_root_dir("/etc")            → "set_root_dir: /etc escapes
                                           workspace.sandbox_root (…/repos).
                                           The active root is unchanged."
```

The check runs on the **canonicalized** target, so `../` traversals and symlinks pointing out of the tree are rejected as well, and it runs *before* activation — a rejected swap leaves the active root and the post-activate hook untouched. A manifest whose `root` sits outside its own `sandbox_root` fails at boot rather than at the first swap.

`sandbox_root` is `local` mode only (a `github` workspace declaring it is a manifest error) and resolves relative to the manifest YAML, exactly like `root`. Downstream binaries wire the same boundary with `Workspace::open_local(root, hook)?.with_sandbox_root(&boundary)?`.

### Filesystem watcher

When `workspace.watch: true`, the framework spawns a `notify-debouncer-mini` watcher over `workspace.root`. The watcher debounces filesystem events for 250ms then fires the post-activate hook against the current active root.

This means: an editor that saves a file triggers a single hook fire per ~250ms window, regardless of how many files change.

## Watch-only mode

```bash
mcp-server --watch ./project
```

Same watcher as local-workspace mode but without the `set_root_dir` swap surface. The root is pinned at boot.

Use this when the active root is stable but downstream artifacts need to rebuild on filesystem changes.

## Calling the hook from your binary

In your downstream binary's main:

```rust
let watch_handle = mcp_methods::server::maybe_watch(
    Some(&watch_dir),
    Some(my_change_handler),
)?;
```

The hook signature is `Fn(&[std::path::PathBuf]) -> ()` for `maybe_watch` (a slice of changed paths), distinct from the `PostActivateHook` (single root + name).

For more complex rebuild logic, your downstream binary typically:

1. Wraps a domain handle (`Arc<RwLock<MyGraph>>`) in the hook's closure
2. On hook fire, parses the changed paths, decides what to rebuild
3. Builds the new artifact off-thread
4. Acquires the RwLock write, atomic-swaps the active slot
5. Releases the lock

This is what `kglite-mcp-server` does for its code-tree-graph rebuild — see the [reference implementation](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server).

## See also

- [Operating Modes](operating-modes.md) — the broader mode table
- [Downstream Binary](downstream-binary.md) — wiring up a hook in your binary
- [Architecture](../explanation/architecture.md) — why the workspace types are shaped this way
