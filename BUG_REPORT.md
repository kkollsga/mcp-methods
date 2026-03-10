# Bug Report: ripgrep silently truncates results

## Version

mcp-methods 0.2.8

## Summary

`ripgrep()` silently drops results when the match count is high. No warning, no truncation marker, no error. The caller has no way to know results are incomplete.

## Evidence

Tested against xarray repo (~190 .py files). Internal Claude Grep used as ground truth.

### Affected (large result sets)

| Query | Internal (truth) | mcp ripgrep | Missing |
|-------|-----------------|-------------|---------|
| `\bxr\.` (files_with_matches) | 115 files | 30 files | 74% |
| `\{[^}]*\}` (files_with_matches) | 189 files | 11 files | 94% |
| `np\.array\(` (files_with_matches) | 75 files | 34 files | 55% |
| `\bxr\.` (count) | 4111 / 115 files | ~200 / 15 files | 87% |
| `raise\s+(Type\|Value\|...)Error` (count) | 940 / 100 files | ~240 / 25 files | 75% |

### Unaffected (small result sets)

| Query | Internal | mcp | Match? |
|-------|----------|-----|--------|
| `\bself\._data\b` (7 files) | 168 / 7 | 168 / 7 | Exact |
| `if __name__ == '__main__'` (4 files) | 4 / 4 | 4 / 4 | Exact |
| `def is_unicode_dtype` (1 file) | 1 / 1 | 1 / 1 | Exact |

## Root cause hypothesis

There appears to be an internal result cap that kicks in regardless of the `head_limit` parameter. The truncation scales with result volume and `offset` returns empty even when more results exist, suggesting the cap is at the engine/walker level rather than the output formatter.

## `head_limit` semantics

`head_limit=0` is documented as "unlimited" but appears to apply an internal default cap. Additionally, `0` is ambiguous — does it mean "zero results" or "no limit"?

**Suggestion**: Change the default to `head_limit=None` (no limit). When `None`, return all results. When set to an integer, cap at that number AND append a truncation indicator to the output so the caller knows results are incomplete. This matches the principle that the caller should control the limit, not the library.

## `offset` broken for pagination

`offset=30` on a query that returned 30 results gives empty results instead of the next page. The offset appears to be applied after the internal truncation, making it useless for paginating large result sets. Offset should be applied at the walker/engine level, before any result cap.

## `.gitignore` is NOT the cause

The repo's .gitignore only excludes build artifacts (*.pyc, __pycache__, .tox, etc.). All missing files are real source and test .py files confirmed to exist and contain matches.
