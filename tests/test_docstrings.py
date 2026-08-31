"""Doc-surface gate: every published callable is documented, every fence closes.

Three independent checks, each able to fail on its own:

1. :func:`test_every_pyo3_item_has_a_doc_comment` parses the pyo3 binding
   source and asserts every ``#[pyfunction]`` / ``#[pyclass]`` and every
   ``#[pymethods]`` ``fn`` carries a ``///`` block. pyo3 copies ``///`` into
   ``__doc__``, so this is the gate for the native surface — and it reads the
   *source*, so it fails on an unbuilt or stale ``_mcp_methods`` extension
   just the same, which the runtime check below cannot do.
2. :func:`test_every_pure_python_export_has_a_docstring` walks the pure-Python
   exports and asserts a non-empty ``inspect.getdoc``. The native exports are
   deliberately out of scope here — check 1 owns them, and running both
   against a stale ``.so`` would report a fix that is already in the source.
3. :func:`test_doc_blocks_have_balanced_code_fences` scans every Rust doc
   comment and every Python docstring for an unbalanced code fence. An
   unclosed fence swallows the rest of the block into a code span on
   docs.rs / in ``help()``.
"""

from __future__ import annotations

import ast
import inspect
import re
from pathlib import Path

import pytest

REPO_ROOT = Path(__file__).resolve().parent.parent
PYO3_SRC = REPO_ROOT / "crates" / "mcp-methods-py" / "src"
RUST_SRC_ROOT = REPO_ROOT / "crates"
PY_PKG = REPO_ROOT / "python" / "mcp_methods"

# Exports implemented in Rust (`crates/mcp-methods-py/src/`). Their `__doc__`
# only reflects the source after `maturin develop`, so they are gated by the
# source parse in check 1 rather than by `inspect.getdoc` in check 2.
NATIVE_EXPORTS = {
    "ElementCache",
    "Skill",
    "SkillRegistry",
    "collapse_code_blocks",
    "compact_discussion",
    "compact_text",
    "detect_git_repo",
    "extract_github_refs",
    "git_api",
    "github_discussions",
    "github_issues",
    "has_git_token",
    "html_to_text",
    "list_dir",
    "read_file",
    "render_skill_template",
    "ripgrep_files",
    "ripgrep_json_fields",
    "ripgrep_lines",
    "validate_repo",
    "write_skill_template",
}


# ---------------------------------------------------------------------------
# 1. pyo3 source parse
# ---------------------------------------------------------------------------


def _rust_items(text: str):
    """Yield (line_no, kind, name, has_doc) for every documentable pyo3 item.

    An item is documented when the run of lines immediately above it — after
    skipping any attributes — contains at least one `///` line.
    """
    lines = text.split("\n")
    in_pymethods = False
    brace_depth = 0
    for idx, raw in enumerate(lines):
        line = raw.strip()

        if in_pymethods:
            brace_depth += raw.count("{") - raw.count("}")
            if brace_depth <= 0:
                in_pymethods = False

        if line.startswith("#[pymethods]"):
            in_pymethods = True
            brace_depth = 0
            continue

        kind = name = None
        if line.startswith("#[pyfunction]"):
            kind = "pyfunction"
        elif line.startswith("#[pyclass"):
            kind = "pyclass"
        elif in_pymethods and re.match(r"(pub )?fn [A-Za-z_]", line):
            kind = "pymethod"
            name = re.match(r"(?:pub )?fn ([A-Za-z_0-9]+)", line).group(1)
        if kind is None:
            continue

        # Walk back over the item's attribute block to find its doc comment.
        # `#[new]`-trivial constructors are exempt: the class doc covers
        # construction and pyo3 has no separate `__doc__` slot for them.
        has_doc = False
        is_new = False
        j = idx - 1
        while j >= 0:
            prev = lines[j].strip()
            if prev.startswith("///"):
                has_doc = True
                break
            if prev.startswith("#["):
                if prev.startswith("#[new]"):
                    is_new = True
                j -= 1
                continue
            if prev.startswith(("]", ")", ")]", "))]")) or (
                prev and not prev.startswith(("//", "fn ", "pub fn ", "}", "{"))
            ):
                # Continuation line of a multi-line attribute, e.g. a
                # `#[pyo3(signature = (` block. Keep walking up.
                j -= 1
                continue
            break

        if is_new:
            continue
        if name is None:
            # Name lives on the `fn` / `struct` line below the attribute run.
            k = idx + 1
            while k < len(lines) and not re.match(r"\s*(pub )?(fn|struct|enum) ", lines[k]):
                k += 1
            name = (
                re.match(r"\s*(?:pub )?(?:fn|struct|enum) ([A-Za-z_0-9]+)", lines[k]).group(1)
                if k < len(lines)
                else f"<line {idx + 1}>"
            )
        yield idx + 1, kind, name, has_doc


def test_every_pyo3_item_has_a_doc_comment():
    sources = sorted(PYO3_SRC.glob("*.rs"))
    assert sources, f"no pyo3 sources under {PYO3_SRC}"

    undocumented = []
    checked = 0
    for path in sources:
        for line_no, kind, name, has_doc in _rust_items(path.read_text()):
            checked += 1
            if not has_doc:
                undocumented.append(f"{path.relative_to(REPO_ROOT)}:{line_no} {kind} `{name}`")

    assert checked > 0, "parser matched no pyo3 items — the parse is broken"
    assert not undocumented, (
        "pyo3 items with no `///` doc comment (they ship an empty `__doc__`):\n  "
        + "\n  ".join(undocumented)
    )


# ---------------------------------------------------------------------------
# 2. pure-Python runtime check
# ---------------------------------------------------------------------------


def _public_callables():
    import mcp_methods
    import mcp_methods.fastmcp as fastmcp

    for mod in (mcp_methods, fastmcp):
        for name in getattr(mod, "__all__", ()):
            if name in NATIVE_EXPORTS:
                continue
            yield f"{mod.__name__}.{name}", getattr(mod, name)


def test_every_pure_python_export_has_a_docstring():
    missing = []
    checked = 0
    for qualname, obj in _public_callables():
        if not (callable(obj) or inspect.isclass(obj)):
            continue
        checked += 1
        if not (inspect.getdoc(obj) or "").strip():
            missing.append(qualname)

    assert checked > 0, "no pure-Python exports found — the walk is broken"
    assert not missing, "public exports with an empty docstring:\n  " + "\n  ".join(missing)


# ---------------------------------------------------------------------------
# 3. fence balance
# ---------------------------------------------------------------------------

_FENCE = re.compile(r"^\s*(`{3,}|~{3,})")


def unbalanced_fence(block: str) -> bool:
    """True when `block` leaves a code fence open.

    Fence width matters: a fence opened with N markers is closed only by a
    fence of N or more of the same marker, so a narrower fence nested inside a
    wider one is content, not a delimiter.
    """
    open_marker: str | None = None
    open_width = 0
    for line in block.split("\n"):
        m = _FENCE.match(line)
        if not m:
            continue
        marker, width = m.group(1)[0], len(m.group(1))
        if open_marker is None:
            open_marker, open_width = marker, width
        elif marker == open_marker and width >= open_width:
            # A closing fence carries no info string; anything trailing means
            # this is a nested opener inside the current block's content.
            if not line.strip()[width:].strip():
                open_marker, open_width = None, 0
    return open_marker is not None


def _rust_doc_blocks():
    """Yield (path, first_line_no, text) for each contiguous Rust doc block."""
    for path in sorted(RUST_SRC_ROOT.glob("*/src/**/*.rs")):
        lines = path.read_text().split("\n")
        block: list[str] = []
        start = 0
        prefix = ""
        for idx, raw in enumerate(lines):
            line = raw.strip()
            marker = (
                "///" if line.startswith("///") else ("//!" if line.startswith("//!") else None)
            )
            if marker and (not block or marker == prefix):
                if not block:
                    start, prefix = idx + 1, marker
                block.append(line[3:])
                continue
            if block:
                yield path, start, "\n".join(block)
                block, prefix = [], ""
            if marker:
                start, prefix = idx + 1, marker
                block = [line[3:]]
        if block:
            yield path, start, "\n".join(block)


def _python_docstrings():
    """Yield (path, line_no, text) for each docstring under python/mcp_methods/."""
    for path in sorted(PY_PKG.rglob("*.py")):
        if "__pycache__" in path.parts:
            continue
        tree = ast.parse(path.read_text())
        for node in ast.walk(tree):
            if isinstance(
                node,
                (ast.Module, ast.ClassDef, ast.FunctionDef, ast.AsyncFunctionDef),
            ):
                doc = ast.get_docstring(node, clean=False)
                if doc:
                    yield path, getattr(node, "lineno", 1), doc


@pytest.mark.parametrize("kind", ["rust", "python"])
def test_doc_blocks_have_balanced_code_fences(kind):
    source = _rust_doc_blocks() if kind == "rust" else _python_docstrings()
    broken = []
    scanned = 0
    for path, line_no, text in source:
        scanned += 1
        if unbalanced_fence(text):
            broken.append(f"{path.relative_to(REPO_ROOT)}:{line_no}")

    assert scanned > 0, f"scanned no {kind} doc blocks — the walk is broken"
    assert not broken, "doc blocks with an unclosed code fence:\n  " + "\n  ".join(broken)
