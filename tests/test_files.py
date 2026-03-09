"""Tests for mcp_methods.files."""

import tempfile
from pathlib import Path

from mcp_methods.files import grep_files, read_file


def _make_tree(tmp: Path) -> None:
    """Create a small file tree for testing."""
    (tmp / "src").mkdir()
    (tmp / "src" / "main.py").write_text("import os\nprint('hello')\n# TODO: fix\n")
    (tmp / "src" / "utils.py").write_text("def helper():\n    return 42\n")
    (tmp / "data.txt").write_text("line1\nline2\nline3\n")
    (tmp / ".git").mkdir()
    (tmp / ".git" / "config").write_text("gitconfig\n")


# ---------------------------------------------------------------------------
# grep_files
# ---------------------------------------------------------------------------


def test_grep_files_basic():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "hello")
        assert "1 match" in result
        assert "main.py:2" in result


def test_grep_files_case_insensitive():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "HELLO", case_insensitive=True)
        assert "1 match" in result


def test_grep_files_glob_filter():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "line", glob="*.txt")
        assert "3 match" in result


def test_grep_files_no_match():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "nonexistent_string_xyz")
        assert "No matches" in result


def test_grep_files_skips_git():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "gitconfig")
        assert "No matches" in result


def test_grep_files_max_results():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "line", glob="*.txt", max_results=2)
        assert "capped at 2" in result


def test_grep_files_relative_to():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([f"{tmp}/src"], "hello", relative_to=tmp)
        assert "src/main.py" in result


def test_grep_files_transform():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files(
            [tmp], "TRANSFORMED", glob="*.py",
            transform=lambda _: "TRANSFORMED content\n"
        )
        assert "match" in result.lower()


def test_grep_files_invalid_regex():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = grep_files([tmp], "[invalid")
        assert "Invalid regex" in result


def test_grep_files_multiple_dirs():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        (Path(tmp) / "extra").mkdir()
        (Path(tmp) / "extra" / "test.py").write_text("hello from extra\n")
        result = grep_files([f"{tmp}/src", f"{tmp}/extra"], "hello")
        assert "2 match" in result


# ---------------------------------------------------------------------------
# read_file
# ---------------------------------------------------------------------------


def test_read_file_basic():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file("data.txt", [tmp])
        assert "data.txt" in result
        assert "3 lines" in result
        assert "line1" in result
        assert "line3" in result


def test_read_file_line_range():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file("data.txt", [tmp], start_line=2, end_line=2)
        assert "1 of 3 lines" in result
        assert "line2" in result


def test_read_file_max_chars():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file("data.txt", [tmp], max_chars=30)
        assert "truncated" in result


def test_read_file_not_found():
    with tempfile.TemporaryDirectory() as tmp:
        result = read_file("nonexistent.txt", [tmp])
        assert "not found" in result.lower() or "access denied" in result.lower()


def test_read_file_path_traversal():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file("../../etc/passwd", [f"{tmp}/src"])
        assert "not found" in result.lower() or "access denied" in result.lower()


def test_read_file_transform():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file(
            "data.txt", [tmp],
            transform=lambda t: t.upper(),
        )
        assert "LINE1" in result


def test_read_file_multiple_allowed_dirs():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file("main.py", [f"{tmp}/src", tmp])
        assert "import os" in result


def test_read_file_line_numbers():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = read_file("src/main.py", [tmp])
        assert "    1  import os" in result
        assert "    2  print" in result
