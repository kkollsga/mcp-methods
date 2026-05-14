---
name: read_source
description: "Read source files from the configured source roots, with optional line slicing and in-file regex filtering. TRIGGER when the user wants the actual contents of a file at a known path, to slice into a specific line window after `grep` has pointed somewhere, to extract a function or section, or to filter to lines matching a pattern within a file. The framework enforces path-traversal protection — `..`, absolute paths, and symlinks that escape the configured roots all reject. ALSO TRIGGER when the user names a file (\"show me src/lib.rs\", \"open the manifest YAML\", \"what's in that test file\") rather than asking the agent to discover it. SKIP for locating which file holds something (use grep), seeing directory layout (use list_source), or reading binary artefacts and large logs (the tool is for source code, not raw artefacts)."
applies_to:
  mcp_methods: ">=0.3.35"
references_tools:
  - read_source
references_arguments:
  - read_source.file_path
  - read_source.start_line
  - read_source.end_line
  - read_source.grep
  - read_source.grep_context
  - read_source.max_chars
auto_inject_hint: true
---

# `read_source` methodology

## Overview

`read_source` reads a file from the configured source root(s), with optional line slicing and in-file regex filtering. It is the **focused read** tool — once `grep` or a graph query has pointed you at a specific file, use this to pull the actual content.

## Quick Reference

| Task | Approach |
|---|---|
| Read a small whole file | `file_path: "src/lib.rs"` — anything under ~300 lines is fine whole |
| Read around a known line | `file_path, start_line=120, end_line=170` — N±20 lines window |
| Read a function whose name you know | Locate with `grep` or a graph query first, then read the window |
| Filter a large file to a pattern | `grep="impl Display", grep_context=5` |
| Path traversal rejected | The root binding is wrong, not the path; ask, don't work around |

## Path semantics

- `file_path` is **relative to the configured source root(s)**, not absolute and not relative to cwd. Pass `src/lib.rs`, not `/abs/path/repo/src/lib.rs`.
- The framework rejects path-traversal attempts (`..`, absolute paths, symlinks escaping the root). If you get a rejection, you're outside the configured roots — that's a server-config issue, not a read issue. Don't try to work around it; ask whether the right root is bound.
- Multiple source roots: the path is resolved against each in turn; first match wins. Ambiguity is rare in practice.

## Slicing

By default `read_source` returns the whole file. For anything larger than ~200 lines, slice:

- `start_line=42, end_line=80` → lines 42–80 inclusive (1-indexed).
- `start_line=42` only → from line 42 to EOF.
- `end_line=80` only → from start to line 80.

A common workflow: `grep` returns `src/foo.rs:142: ...`. Read with `start_line=120, end_line=170` to see the match plus ~20 lines of context on each side. Don't pull the whole file unless you genuinely need it.

## In-file filtering with `grep`

The `grep` argument filters to lines matching a regex *within the requested slice*. Pair it with `grep_context` to keep the surrounding lines:

- `grep="impl Display"` → just the lines matching the pattern.
- `grep="impl Display", grep_context=5` → matches plus 5 lines on each side.

Use this when the file is large and you only need the methodology-relevant parts (e.g. "show me every `impl Display` in this 2000-line file"). For one or two known line ranges, plain `start_line`/`end_line` is simpler.

## Output sizing

- `max_chars` caps the returned content. Default is generous; you rarely need to set it. When you do, prefer narrowing `start_line`/`end_line` first — clipping at `max_chars` mid-content is a worse experience than reading the right window.
- `max_matches` caps grep-matched lines (when `grep=` is set). Same logic: tighten the regex before tightening the cap.

## Common Pitfalls

❌ Pulling the whole file when you only need 20 lines around a match — wasteful of the agent's context.

❌ Passing absolute paths or `../` to escape the configured root — the framework rejects them, and trying to work around the rejection means the root binding is wrong (server-config issue, not a read issue).

❌ Calling `read_source` to "find" something — that's `grep`'s job. `read_source` shows you what's at a known path.

✅ Read once, expand if needed. A tight window then a wider one beats reading the whole file speculatively.

✅ When in doubt about the file's size, slice first. You can always expand; you can't un-read 2000 lines.

## When `read_source` is the wrong tool

- **You don't know which file?** Use `grep` to locate, then `read_source` to read.
- **You want directory layout?** Use `list_source`.
- **You want to navigate symbol definitions?** A graph-aware query (`cypher_query` or equivalent) returns exact file/line ranges without you guessing.
- **The file is a binary or a 200 MB log?** `read_source` is for source code, not raw artefacts. The framework caps response sizes — large logs come back truncated and confusing.

## Common patterns

- **Read a function**: locate with `cypher_query` or `grep`, then `read_source` with a tight window.
- **Read all of a small file**: pass just `file_path`. Files under ~300 lines are fine to read whole.
- **Find every error path in a module**: `grep="return Err\\("` + `grep_context=2`.
- **Inspect a test fixture**: `file_path="tests/fixtures/foo.yaml"` — fixtures are source too.
