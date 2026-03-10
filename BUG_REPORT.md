# ripgrep: open issues

## `head_limit` semantics

`head_limit=0` means "unlimited" but `0` is ambiguous — could mean "zero results" or "no limit". Makes conditional checks awkward (`if head_limit` is falsy for both).

**Suggestion**: `head_limit: int | None = None` where `None = unlimited`.

## `relative_to` missing from `ripgrep`

`ripgrep_files` has `relative_to` but `ripgrep` (the Claude Grep-compatible interface) does not.

When searching a subdirectory (`path="/project/src/backends"`), results show bare filenames (`common.py:205`) instead of project-relative paths (`backends/common.py:205`). Less useful for navigation.

**Suggestion**: Add `relative_to: str | None = None` to `ripgrep`. MCP wrappers can then pass `relative_to=str(project_root)` so paths are always project-relative.
