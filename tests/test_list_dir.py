"""Tests for mcp_methods.list_dir."""

import tempfile
from pathlib import Path

from mcp_methods import list_dir


def _make_tree(tmp: Path) -> None:
    """Create a directory tree for testing."""
    (tmp / "src").mkdir()
    (tmp / "src" / "core").mkdir()
    (tmp / "src" / "core" / "engine.py").write_text("# engine\n")
    (tmp / "src" / "core" / "types.py").write_text("# types\n")
    (tmp / "src" / "utils.py").write_text("# utils\n")
    (tmp / "src" / "main.py").write_text("# main\n")
    (tmp / "tests").mkdir()
    (tmp / "tests" / "test_core.py").write_text("# test\n")
    (tmp / "docs").mkdir()
    (tmp / "docs" / "guide.md").write_text("# guide\n")
    (tmp / "README.md").write_text("# readme\n")
    (tmp / ".gitignore").write_text("*.pyc\n")


def test_list_dir_flat():
    """depth=1 shows top-level entries with dir summaries."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=1)
        # Should show directories and files
        assert "src/" in result
        assert "tests/" in result
        assert "docs/" in result
        assert "README.md" in result
        assert ".gitignore" in result
        # Directories should have summaries since depth=1
        assert "[" in result  # at least one summary bracket


def test_list_dir_depth_2():
    """depth=2 shows one level of nesting."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=2)
        # Should show nested files
        assert "main.py" in result
        assert "utils.py" in result
        assert "test_core.py" in result
        assert "guide.md" in result
        # core/ should appear as a directory
        assert "core/" in result


def test_list_dir_depth_3():
    """depth=3 shows full tree."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=3)
        assert "engine.py" in result
        assert "types.py" in result


def test_list_dir_dirs_only():
    """dirs_only=True shows only directories."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=2, dirs_only=True)
        assert "src/" in result
        assert "core/" in result
        assert "tests/" in result
        assert "docs/" in result
        # Files should not appear
        assert "main.py" not in result
        assert "README.md" not in result


def test_list_dir_glob_filter():
    """glob filters to matching files, pruning empty dirs."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=3, glob="*.py")
        assert "engine.py" in result
        assert "main.py" in result
        assert "utils.py" in result
        assert "test_core.py" in result
        # Non-py files and empty dirs should be pruned
        assert "guide.md" not in result
        assert "README.md" not in result
        # docs/ has no .py files, should be pruned
        assert "docs/" not in result


def test_list_dir_include_size():
    """include_size shows file sizes."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=1, include_size=True)
        # Files should have size annotations
        assert "README.md" in result
        assert " B)" in result or "KB)" in result


def test_list_dir_relative_to():
    """relative_to controls the root display name."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        src_path = str(Path(tmp) / "src")
        result = list_dir(src_path, depth=1, relative_to=tmp)
        # Root should show as "src" not the full path
        assert result.startswith("src/")


def test_list_dir_skip_dirs():
    """skip_dirs excludes named directories."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=2, skip_dirs=["docs", "tests"])
        assert "docs/" not in result
        assert "tests/" not in result
        # src should still be there
        assert "src/" in result


def test_list_dir_empty():
    """Empty directory shows (empty) marker."""
    with tempfile.TemporaryDirectory() as tmp:
        result = list_dir(tmp)
        assert "(empty)" in result


def test_list_dir_tree_chars():
    """Output uses unicode box-drawing characters."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=1)
        assert "├── " in result or "└── " in result


def test_list_dir_not_a_directory():
    """Non-directory path returns error."""
    with tempfile.TemporaryDirectory() as tmp:
        f = Path(tmp) / "file.txt"
        f.write_text("hello")
        result = list_dir(str(f))
        assert "Error" in result or "not a directory" in result


def test_list_dir_gitignore():
    """respect_gitignore=True skips gitignored files."""
    with tempfile.TemporaryDirectory() as tmp:
        tmp = Path(tmp)
        (tmp / ".gitignore").write_text("*.log\n")
        (tmp / "app.py").write_text("# app\n")
        (tmp / "debug.log").write_text("log\n")
        # Init git so .gitignore is respected
        import subprocess

        subprocess.run(["git", "init", str(tmp)], capture_output=True)
        result = list_dir(str(tmp), depth=1, respect_gitignore=True)
        assert "app.py" in result
        assert "debug.log" not in result


def test_list_dir_summary_format():
    """Leaf directories show [N files] summary."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = list_dir(tmp, depth=1)
        # src/ at depth 1 should show a summary with files count
        for line in result.split("\n"):
            if "src/" in line and "[" in line:
                assert "file" in line
                break


# ---------------------------------------------------------------------------
# annotate callback
# ---------------------------------------------------------------------------


def test_list_dir_annotate():
    """Annotate callback adds metadata to entries."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))

        def annotate(rel_path):
            if rel_path.endswith(".py"):
                return "(42 loc)"
            return None

        result = list_dir(tmp, depth=2, annotate=annotate)
        assert "(42 loc)" in result
        # .md files should NOT have annotation
        for line in result.split("\n"):
            if ".md" in line:
                assert "(42 loc)" not in line


def test_list_dir_annotate_none_passthrough():
    """Annotate returning None for all entries produces same output as no annotate."""
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        without = list_dir(tmp, depth=2)
        with_none = list_dir(tmp, depth=2, annotate=lambda _path: None)
        assert without == with_none


def test_list_dir_annotate_relative_to_subdir():
    """Annotate callback receives paths relative to relative_to, not bare names.

    Regression: listing a subdirectory with relative_to set to a parent used to
    pass bare filenames (e.g. 'engine.py') instead of full relative paths
    (e.g. 'src/core/engine.py').
    """
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        received_paths = []

        def annotate(rel_path):
            received_paths.append(rel_path)
            return None

        # List src/core/ with relative_to=tmp (the project root)
        list_dir(str(Path(tmp) / "src" / "core"), depth=1, relative_to=tmp, annotate=annotate)
        # Callback should receive "src/core/engine.py", not "engine.py"
        assert any("src/core/" in p for p in received_paths), (
            f"Expected paths relative to project root, got: {received_paths}"
        )


def test_list_dir_annotate_alignment():
    """Annotations within same directory level are column-aligned."""
    with tempfile.TemporaryDirectory() as tmp:
        p = Path(tmp)
        (p / "short.py").write_text("x")
        (p / "very_long_name.py").write_text("y")

        def annotate(rel_path):
            return "(ok)"

        result = list_dir(tmp, depth=1, annotate=annotate)
        lines = [line for line in result.split("\n") if "(ok)" in line]
        assert len(lines) == 2
        # Both annotations should start at the same column
        col0 = lines[0].index("(ok)")
        col1 = lines[1].index("(ok)")
        assert col0 == col1
