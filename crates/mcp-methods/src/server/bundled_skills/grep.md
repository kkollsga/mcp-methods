---
name: grep
description: "Regex text search across the configured source roots, with file-glob scoping and result caps. SKIP FIRST for structural questions when a code graph / structural index is active — \"where is X defined?\", \"what calls Y?\", finding a function/class/variable by name, or locating call sites are graph queries (exact); grep is fuzzy text and will validate a wrong turn. Also SKIP single-file reads (use read_source) and directory-layout questions (use list_source). TRIGGER for literal text that no graph indexes: error-message strings, log lines, comments, config keys, string literals, TODO/FIXME markers, license headers — or, only when NO structural index is available for this source root, symbol/definition/call-site sweeps as a fallback."
applies_to:
  mcp_methods: ">=0.3.35"
references_tools:
  - grep
references_arguments:
  - grep.pattern
  - grep.glob
  - grep.max_results
auto_inject_hint: true
---

# `grep` methodology

> **Doctrine first.** If this deployment has a code graph / structural index, structural questions ("where is X defined?", "what calls Y?", find a symbol by name) go to the graph tools (`graph_overview` → `cypher_query`), not here — the graph is exact, grep is fuzzy and will happily confirm a wrong guess. Reach for `grep` for **literal text a graph can't index** (error strings, log lines, comments, config keys), or for symbol sweeps only when no structural index exists.

## Overview

`grep` runs a regex across the configured source roots and returns line-level matches. It is the **broad sweep** tool — use it when you don't yet know which file holds what you're looking for. Once you've narrowed to a specific file, switch to `read_source` for slices and `list_source` for directory layout.

## Quick Reference

| Task | Approach |
|---|---|
| Find a function definition | `pattern: "fn foo\\b\|def foo\\b"`, glob the language |
| Find call sites of a known function | `pattern: "\\bfoo\\("` — word boundary plus opening paren |
| Find every error path | `pattern: "return Err\\("`, glob to the module |
| Trim a huge match set | Add `glob: "src/**/*.rs"` or `glob: "!vendor/**"` |
| Hit `max_results` cap | Tighten the pattern — don't raise the cap blindly |

## Choosing a good pattern

- **Be specific**. `fn foo` finds far less noise than `foo`. Anchor on syntax you expect: `pub fn `, `class `, `def `, `impl Display for`, `#[derive(`.
- **Use word boundaries** when the symbol could be embedded in longer names: `\bfoo\b` instead of plain `foo`.
- **Escape regex metacharacters** in literal strings: `\.unwrap\(\)`, `Vec<u8>` is fine but `Vec\[u8\]` (square brackets are character-class delimiters).
- **Match the surrounding language**. Searching Rust? `-> Result<` is a stronger signal than `Result`. Python? `def fn(` beats `fn`.

## Scoping with `glob`

The `glob` argument restricts the search to matching paths. Use it whenever you can — it makes the result narrower *and* faster:

- `glob: "*.rs"` → only Rust files
- `glob: "src/**/*.ts"` → TypeScript under `src/`
- `glob: "**/test_*.py"` → Python test files anywhere
- `glob: "!vendor/**"` (negation) → everything except vendored code

Negation patterns matter: large repos have generated code, vendored deps, and lockfiles that swamp regex results. If you see thousands of matches in `dist/`, `target/`, or `node_modules/`, add a negation glob and retry.

## Reading results

`grep` returns `path:line: matched-line`. Treat each match as a *pointer*, not the answer — the surrounding context lives in the file. After narrowing to N interesting matches:

1. Pick the most specific match (best file path, most unique line).
2. Open it with `read_source(file_path, start_line=…, end_line=…)`. Choose a window that covers the match plus 10-20 lines on each side.
3. If you need more context, expand the window. Don't re-grep with a slightly different pattern when you already know the line.

## Common Pitfalls

❌ Running `grep` with a generic pattern and getting 200+ matches → tighten the regex, don't read everything.

❌ Re-grepping with slightly different patterns when you've already found the file → switch to `read_source` and expand the window.

❌ Searching for `Foo` when you mean the function call `Foo()` — add the parens; word boundaries alone won't help.

✅ Glob early. A 10-character glob saves the agent from reading 100 KB of vendored junk.

✅ Reach for `cypher_query` (or your domain's graph tool) when the question is structural. Grep is text-only.

## When `grep` is the wrong tool

- **Looking up a known symbol's definition?** If the codebase has a knowledge graph (`cypher_query`-style tooling), querying the graph is exact; `grep` is fuzzy. The graph beats grep for "where is `Foo` defined?"-class questions.
- **Tracing call sites?** Grep finds textual occurrences. A call to `bar()` and a function named `bar()` look the same to grep. Use a graph traversal for accurate call-site analysis.
- **Reading a specific file?** `read_source` skips the regex machinery entirely.
- **Listing what's in a directory?** `list_source` is the right shape — it returns a tree, not flat path hits.

## Size limits

`grep` truncates at `max_results` (default 200). If you hit the cap, the result is *probably* not telling you what you think. Tighten the pattern, add a glob, or split the search by directory.
