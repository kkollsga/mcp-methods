# Python Bindings

`pip install mcp-methods` ships a Python wheel that exposes the framework's primitives directly to Python code, plus the `mcp-server` CLI on PATH. Use it when you want to call into mcp-methods from a Python script, a FastMCP server, or a Jupyter notebook.

## Install

```bash
pip install mcp-methods
```

One abi3 wheel per OS (macOS-arm64 / manylinux-x86_64 / win-amd64). Works on Python 3.10 through 3.13 without reinstall.

## What you get

Two surfaces:

1. **The library**: `from mcp_methods import ...` — direct access to the Rust primitives.
2. **The CLI**: `mcp-server` on PATH (a Python launcher that execs the bundled native binary).

## The library surface

```python
from mcp_methods import (
    # File + directory navigation
    list_dir,
    read_file,

    # ripgrep-backed search
    ripgrep,           # Claude Code Grep-compatible interface
    ripgrep_files,     # full interface, multi-dir
    ripgrep_lines,     # search text lines
    ripgrep_json_fields,

    # GitHub
    github_discussions,
    git_api,
    has_git_token,
    extract_github_refs,

    # Git
    detect_git_repo,
    validate_repo,

    # Text processing
    html_to_text,
    compact_discussion,
    compact_text,
    collapse_code_blocks,

    # Caching
    ElementCache,
)
```

Most functions are direct passthroughs to the Rust primitives — see [Python API Reference](../reference/python-api.md) for full signatures.

## The CLI

```bash
which mcp-server
# → /opt/miniconda3/bin/mcp-server (or your venv's bin dir)

mcp-server --help
mcp-server --source-root ./src
mcp-server --mcp-config workspace_mcp.yaml
```

The Python launcher (`mcp_methods._cli:main`) execs the bundled native binary at `mcp_methods/_bin/mcp-server`. Pure-Rust binary; zero libpython link.

## FastMCP helpers

The wheel also ships `mcp_methods.fastmcp` — a thin layer for building FastMCP servers in Python that delegate to the Rust primitives. See [Using FastMCP Helpers](using-fastmcp-helpers.md).

## Skills

`SkillRegistry` and `Skill` are pyo3 wrappers around the Rust skills loader. The common path is `SkillRegistry.from_manifest(path)` — it loads a manifest, walks the three-layer composition, and returns a resolved set ready to register as MCP prompts.

```python
from mcp_methods import SkillRegistry
from mcp_methods.fastmcp import register_skills_as_prompts

registry = SkillRegistry.from_manifest("./my_mcp.yaml")
# registry.skill_names() → ['cypher_query', 'grep', 'read_source', ...]
# registry.get("grep").body → "<full markdown body>"

register_skills_as_prompts(app, registry)   # wires every skill as a @app.prompt
```

`SkillRegistry.from_manifest(path, include_bundled=True)` — pass `include_bundled=False` to skip framework defaults (useful in tests or when a downstream binary supplies its own bundled layer). `SkillRegistry.find_sibling(graph_path)` returns the `<stem>_mcp.yaml` next to a graph/data file, matching the convention `mcp-server` uses for auto-detection.

For background on how the registry composes layers and what the frontmatter looks like, see [Authoring Skills](authoring-skills.md) and [Three-Layer Composition](../explanation/three-layer-composition.md).

## Calling from a downstream Python project

```python
# my_agent.py
from mcp_methods import ripgrep_files, github_discussions

# Search for all function definitions in a Python project
matches = ripgrep_files(
    source_dirs=["./src"],
    pattern=r"def \w+",
    type_filter="py",
    max_results=50,
)

for m in matches:
    print(f"{m.path}:{m.line_number}: {m.line}")

# Fetch a GitHub issue with smart compaction
issue = github_discussions(repo="kkollsga/mcp-methods", number=42)
print(issue)
```

## When to use the pyo3 surface vs the CLI

| Use case | Surface |
|---|---|
| You're writing a Python agent that needs framework primitives in-process | `from mcp_methods import ...` |
| You're writing a FastMCP server | `from mcp_methods.fastmcp import register_*` |
| You want a standalone MCP server an agent can spawn | `mcp-server` CLI |
| You want a downstream Rust binary | Build one — see [Downstream Binary](downstream-binary.md) |
| You're building Python bindings for your own kglite-like project | Wrap mcp-methods' Rust API in pyo3 — see [kglite's pattern](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server) |

## See also

- [Python API Reference](../reference/python-api.md) — full function signatures
- [Using FastMCP Helpers](using-fastmcp-helpers.md) — the `mcp_methods.fastmcp` module
- [Distribution Shape](../explanation/distribution-shape.md) — why the abi3 wheel pattern
