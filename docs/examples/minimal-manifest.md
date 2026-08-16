# Example: Minimal Manifest

The smallest manifest that does something useful — bind source navigation tools to a directory.

```yaml
name: My Source Navigator
source_roots:
  - ./src
```

That's it. Run with:

```bash
mcp-server --mcp-config minimal_mcp.yaml
```

## What you get

The framework registers:

- `read_source(path, line_range)` — read a file under any source root
- `grep(pattern, ...)` — ripgrep across all source roots
- `list_source(path, depth)` — tree listing

Plus the built-in `ping` tool (lets the agent verify the server is alive).

## Operator config (Claude Code)

```json
{
  "mcpServers": {
    "navigator": {
      "command": "mcp-server",
      "args": ["--mcp-config", "/abs/path/to/minimal_mcp.yaml"]
    }
  }
}
```

## When to use this shape

- You want the agent to navigate a single fixed codebase
- You don't need GitHub access (this manifest leaves `builtins.github` at its default `false`, so the GitHub tools stay unregistered even if a `GITHUB_TOKEN` is in env)
- You don't need watch/rebuild hooks
- You don't have domain-specific tools

For multi-root, GitHub, or watch, see [Workspace (GitHub)](workspace-github.md), [Workspace (local) + Watch](workspace-local-watch.md). For custom tools, see [Tools + Trust](tools-and-trust.md).
