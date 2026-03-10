# Feature Request: `list_dir` — directory listing with tree formatting

## Summary

Add a `list_dir` function for browsable directory listings with depth control, glob filtering, and `.gitignore` support. This is the missing piece in the file exploration flow: `list_dir` → `ripgrep` → `read_file`.

## Proposed signature

```python
def list_dir(
    path: str,
    *,
    depth: int = 1,
    glob: str | None = None,
    dirs_only: bool = False,
    relative_to: str | None = None,
    respect_gitignore: bool = True,
    skip_dirs: list[str] | None = None,
    include_size: bool = False,
) -> str:
    """List directory contents with tree-formatted output.

    path: directory to list.
    depth: recursion depth (1 = flat ls, 2+ = tree). Default 1.
    glob: filter entries by pattern, e.g. "*.py", "test_*".
    dirs_only: only show directories, not files.
    relative_to: base for relative paths in output.
    respect_gitignore: skip .gitignore'd paths (default True).
    skip_dirs: additional directory names to skip (e.g. ["node_modules", "__pycache__"]).
    include_size: show file sizes.
    """
```

## Expected output format

### `list_dir("/project/src/algorithms", depth=1)`
```
algorithms/
├── __init__.py
├── astar.py
├── dense.py
├── generic.py
├── weighted.py
└── tests/           [6 files]
```

### `list_dir("/project/src", depth=2, dirs_only=True)`
```
src/
├── algorithms/
│   └── tests/
├── core/
│   ├── dtypes/
│   └── tests/
├── io/
└── utils/
```

### `list_dir("/project/src", glob="test_*.py", depth=3)`
```
src/
├── algorithms/
│   └── tests/
│       ├── test_astar.py
│       └── test_weighted.py
├── core/
│   └── tests/
│       ├── test_dtypes.py
│       └── test_indexing.py
└── io/
    └── tests/
        └── test_csv.py
```

## Design notes

### Directory summaries
When a directory's contents are not shown (below depth limit or filtered out), show a summary: `[6 files]`, `[3 dirs, 12 files]`. This gives orientation without flooding context.

### `.gitignore` support
Reuse the same `ignore` crate walker used by `ripgrep_files`. Build artifacts, `__pycache__`, `.tox`, `node_modules` etc. should be excluded by default.

### Tree formatting
Use Unicode box-drawing characters (`├──`, `└──`, `│`) for the tree. This is the standard `tree` command format that agents and humans both parse easily.

### No file metadata by default
Keep the default output compact — just names and directory summaries. The `include_size` flag adds file sizes for when that's useful. Line counts (loc) are intentionally omitted — MCP server wrappers can enrich the output with graph-derived metadata (loc, function count, etc.) since that requires a knowledge graph, not just filesystem access.

## How MCP servers will use it

Servers wrap `list_dir` as `list_source` with:
1. Path resolution relative to active repo/project root
2. `relative_to` set to project root automatically
3. Optional graph enrichment (loc counts from File nodes via Cypher)

```python
@mcp.tool()
@timed
def list_source(path: str = "", depth: int = 1, glob: str | None = None,
                dirs_only: bool = False) -> str:
    """List source files and directories in the active repository."""
    full_path = str(active_repo_path / path)
    result = list_dir(full_path, depth=depth, glob=glob, dirs_only=dirs_only,
                      relative_to=str(active_repo_path))
    # Optional: enrich with loc from graph
    if active_graph and not dirs_only:
        result = _enrich_with_loc(result, active_graph)
    return result
```

## Fits with existing tools

The complete file exploration flow becomes:

| Step | Tool | Purpose |
|------|------|---------|
| Orient | `list_dir` / `list_source` | Understand project structure |
| Search | `ripgrep` / `ripgrep_files` | Find patterns across files |
| Read | `read_file` / `read_source` | Read specific file content |

All three share consistent conventions: `relative_to` for paths, `.gitignore` respect, `skip_dirs` for exclusions.
