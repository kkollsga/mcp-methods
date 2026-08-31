# Operating Modes

The framework's mode determines how source navigation, watch hooks, and the workspace root are wired at boot. Modes are picked via CLI flag or manifest `workspace:` declaration. **Manifest declarations win over CLI flags.**

## The five modes

### Bare

```bash
mcp-server
```

No `--source-root` / `--workspace` / `--watch` flag. Framework registers only the `ping` tool plus any manifest-declared `tools[]`. Source navigation tools (`read_source`, `grep`, `list_source`) are NOT registered — they need a bound root.

**Use when:** testing the MCP protocol layer in isolation, or building a tool-only server (e.g. a stateless rewrite or computation service).

### Source-root

```bash
mcp-server --source-root ./src
```

Or via manifest:

```yaml
source_roots:
  - ./src
```

Binds the source tools to a fixed directory. No clone, no watcher, no root-swap. The simplest "let an agent navigate my code" setup.

**Use when:** the directory is stable, you don't need automatic rebuilds, and you don't want to clone external repos.

### Workspace (GitHub)

```bash
mcp-server --workspace /tmp/repos
```

Clone-and-track flow. The `repo_management` tool exposes:

- `repo_management("owner/repo")` — clone the repo, fast-forward if it exists, fire the post-activate hook
- `repo_management("owner/repo", update=true)` — fetch + reset HARD if upstream has moved
- `repo_management(delete="owner/repo")` — remove a repo from the workspace
- `repo_management(update=true)` — re-activate the currently active repo (rebuild artifacts)

Repos are stored under `<workspace>/<org>/<repo>`, with an `<workspace>/inventory.json` tracking the active repo, last-built SHA per repo, and idle days. The framework auto-sweeps repos idle for `--stale-after-days` (default 7).

**Use when:** an agent needs to explore multiple GitHub repos on the fly.

### Workspace (local)

```yaml
workspace:
  kind: local
  root: ./repo
  watch: true
```

(No CLI flag — only via manifest. Manifest declarations win over `--workspace`.)

Binds a fixed local directory as the active source root. Two key behaviours:

1. **Atomic root swap**: the `set_root_dir(path)` tool swaps the active root at runtime under an internal `RwLock`, so concurrent reads from the source tools see the new root atomically. (This is the [0.3.28 fix](../changelog.md): swapping previously clobbered `active_repo_path` back to the configured root.)
2. **Filesystem watcher**: with `watch: true`, the framework's post-activate hook fires on any debounced filesystem change.

**Use when:** the source directory is local but you want runtime root-swap (e.g. an agent exploring different projects in succession) or auto-rebuilds on save.

### Watch

```bash
mcp-server --watch ./project
```

Pins source roots to the directory + registers a filesystem watcher. Downstream binaries register a rebuild callback via the post-activate hook. The framework itself doesn't do anything with the change events — it's a hook for downstream code-graph rebuilders, blueprint-validators, etc.

**Use when:** you have a downstream build step (e.g. tree-sitter parsing, manifest validation, schema regeneration) that should fire on file changes.

## Precedence

When both CLI flags and manifest declarations are present:

| Layer | Wins |
|---|---|
| Manifest `workspace.kind: local` | Yes — always wins |
| Manifest `source_root:` / `source_roots:` (with no `--source-root` flag) | Yes |
| CLI `--source-root` (with no manifest `source_root:`) | Yes |
| CLI `--workspace` (with no manifest `workspace:`) | Yes |
| Manifest `workspace.kind: github` | No — it binds nothing on its own. Only `kind: local` is converted from the manifest; a github workspace comes from `--workspace DIR`, and declaring `kind: github` without that flag boots with no workspace (and a WARN saying so). |
| Both manifest `source_root:` AND CLI `--source-root` set | Manifest wins (operator gets a warning) |

This precedence is intentional: the manifest is the source of truth, and CLI flags are a convenience for operators who can't or don't want to edit a YAML file.

## Boot sequence

For each mode, the framework runs:

1. Parse CLI flags
2. Detect manifest (auto: `<dir>/workspace_mcp.yaml`, or explicit `--mcp-config PATH`)
3. Load + validate manifest
4. Manifest's `workspace:` overrides CLI-derived mode
5. `.env` walk-up from start directory (or explicit `env_file:`)
6. Build `ServerOptions` from manifest + mode
7. Register dynamic tools (source / GitHub / manifest's `tools[]`)
8. Spawn watcher if mode requires it
9. Serve over stdio

The `env_file:` resolution happens *before* anything reads env vars — so `GITHUB_TOKEN` and similar are guaranteed to be in scope by the time the GitHub tools are constructed. A token in scope is not by itself enough to register them: the GitHub tools require `builtins.github: true` in the manifest, and only then does token reachability decide whether they appear. See [Writing a Manifest](writing-a-manifest.md#builtins).

### What is fatal at boot, and what degrades

Only a failure that makes the server's *primary* capability impossible aborts the boot. Everything peripheral warns on stderr, records itself in the boot summary line, and lets the server reach the MCP `initialize` handshake — a server that exits before the handshake cannot tell a client anything at all, so a client sees an unexplained dead server rather than a degraded one.

| Boot failure | Behaviour |
|---|---|
| Manifest missing or invalid | **Fatal** — there is nothing to serve |
| `--source-root` / `--watch` flag names a missing directory | **Fatal** — the operator asked for exactly that path on the command line |
| Workspace mode cannot open its workspace | **Fatal** — the workspace *is* the capability in those modes |
| `workspace.sandbox_root` rejected | **Fatal, deliberately** — degrading would serve unbounded `set_root_dir` swaps after the operator asked for a containment boundary |
| Manifest `source_root:` / `source_roots:` entry does not resolve | **Degrades** — warns per entry, serves the roots that did resolve, adds `unresolved source roots: [...]` to the boot summary |
| Explicit `env_file:` does not exist | **Degrades** — warns, adds `env_file unavailable: …` to the boot summary; tools needing those variables report the missing credential when called |
| Filesystem watcher will not start | **Degrades** — warns, adds `watcher unavailable: …` to the boot summary; the server serves a tree that no longer auto-refreshes |

With no source root available at all, `read_source` / `grep` / `list_source` stay in `tools/list` and answer with `Cannot read source: no active source root…` — the absence is reported where an agent will actually see it.

## Manifest auto-detection

In workspace, watch, and local-workspace modes, the framework looks for `<workspace-dir>/workspace_mcp.yaml`. If found, it's loaded automatically.

In source-root and bare modes, no auto-detection happens — you must pass `--mcp-config PATH` explicitly if you want a manifest loaded.

## Skills subcommands

When a `skills-*` subcommand is given on the command line, `mcp-server` runs that command and exits without booting the MCP server:

```bash
mcp-server skills-lint ./mcp-servers/my_mcp.skills/
mcp-server skills-list --mcp-config ./mcp-servers/my_mcp.yaml
mcp-server skills-show --mcp-config ./mcp-servers/my_mcp.yaml cypher_query
mcp-server skills-new ./mcp-servers/my_mcp.skills/ cypher_query "TRIGGER when ..."
```

These are operator tools — they don't interact with the server boot path. The same helpers are reachable from Rust as `mcp_methods::server::cli::{skills_lint, skills_list, skills_show}` so downstream binaries can offer the same surface in their own CLIs. See [Skills-Aware Manifests](skills-aware-manifests.md) for examples.

## See also

- [Writing a Manifest](writing-a-manifest.md) — the schema
- [Watch and Workspace](watch-and-workspace.md) — deeper dive on the watcher + atomic-swap behaviour
- [Architecture](../explanation/architecture.md) — why the modes are shaped this way
