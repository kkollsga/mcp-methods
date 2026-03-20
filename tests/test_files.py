"""Tests for mcp_methods.files."""

import tempfile
from pathlib import Path

from mcp_methods import read_file, ripgrep, ripgrep_files


def _make_tree(tmp: Path) -> None:
    """Create a small file tree for testing."""
    (tmp / "src").mkdir()
    (tmp / "src" / "main.py").write_text("import os\nprint('hello')\n# TODO: fix\n")
    (tmp / "src" / "utils.py").write_text("def helper():\n    return 42\n")
    (tmp / "data.txt").write_text("line1\nline2\nline3\n")
    (tmp / ".git").mkdir()
    (tmp / ".git" / "config").write_text("gitconfig\n")


# ---------------------------------------------------------------------------
# ripgrep_files
# ---------------------------------------------------------------------------


def test_ripgrep_files_basic():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "hello")
        assert "1 match" in result
        assert "main.py:2" in result


def test_ripgrep_files_case_insensitive():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "HELLO", case_insensitive=True)
        assert "1 match" in result


def test_ripgrep_files_glob_filter():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "line", glob="*.txt")
        assert "3 match" in result


def test_ripgrep_files_no_match():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "nonexistent_string_xyz")
        assert "No matches" in result


def test_ripgrep_files_skips_git():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "gitconfig")
        assert "No matches" in result


def test_ripgrep_files_match_limit():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "line", glob="*.txt", match_limit=2)
        assert "capped at 2" in result


def test_ripgrep_files_relative_to():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([f"{tmp}/src"], "hello", relative_to=tmp)
        assert "src/main.py" in result


def test_ripgrep_files_transform():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files(
            [tmp], "TRANSFORMED", glob="*.py", transform=lambda _: "TRANSFORMED content\n"
        )
        assert "match" in result.lower()


def test_ripgrep_files_invalid_regex():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "[invalid")
        assert "Invalid regex" in result


def test_ripgrep_files_multiple_dirs():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        (Path(tmp) / "extra").mkdir()
        (Path(tmp) / "extra" / "test.py").write_text("hello from extra\n")
        result = ripgrep_files([f"{tmp}/src", f"{tmp}/extra"], "hello")
        assert "2 match" in result


def test_ripgrep_files_output_mode_files():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "hello", output_mode="files_with_matches")
        assert "main.py" in result
        # Should NOT contain line numbers or content
        assert "print" not in result


def test_ripgrep_files_output_mode_count():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "line", glob="*.txt", output_mode="count")
        assert "data.txt:3" in result


def test_ripgrep_files_context():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "hello", context=1)
        # Should include the line before (import os) and after (# TODO: fix)
        assert "import os" in result
        assert "TODO" in result


def test_ripgrep_files_context_before():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "hello", context_before=1)
        assert "import os" in result


def test_ripgrep_files_context_after():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "hello", context_after=1)
        assert "TODO" in result


def test_ripgrep_files_type_filter():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "line", type_filter="txt")
        assert "3 match" in result
        # With py filter, should not find "line" in txt
        result2 = ripgrep_files([tmp], "line", type_filter="py")
        assert "No matches" in result2


def test_ripgrep_files_max_results():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "line", glob="*.txt", max_results=2)
        lines = [line for line in result.split("\n") if line.startswith("  ")]
        assert len(lines) == 2


def test_ripgrep_files_offset():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "line", glob="*.txt", offset=1, max_results=1)
        lines = [line for line in result.split("\n") if line.startswith("  ")]
        assert len(lines) == 1


def test_ripgrep_files_multiline():
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "multi.txt").write_text("start\nmiddle\nend\n")
        result = ripgrep_files([tmp], r"start\nmiddle", multiline=True)
        assert "1 match" in result


def test_ripgrep_files_binary_skip():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        # Write a file with null bytes (binary)
        (Path(tmp) / "binary.bin").write_bytes(b"hello\x00world")
        result = ripgrep_files([tmp], "hello")
        # Should only match main.py, not the binary file
        assert "1 match" in result


def test_ripgrep_files_respects_gitignore():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        # Initialize a git repo and add .gitignore
        import subprocess

        subprocess.run(["git", "init", tmp], capture_output=True)
        (Path(tmp) / ".gitignore").write_text("ignored_dir/\n")
        (Path(tmp) / "ignored_dir").mkdir()
        (Path(tmp) / "ignored_dir" / "secret.py").write_text("hello secret\n")
        result = ripgrep_files([tmp], "secret", respect_gitignore=True)
        assert "No matches" in result
        # With gitignore disabled, should find it
        result2 = ripgrep_files([tmp], "secret", respect_gitignore=False)
        assert "1 match" in result2


def test_ripgrep_files_no_line_numbers():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep_files([tmp], "hello", line_numbers=False)
        assert ":2:" not in result
        assert "hello" in result


def test_ripgrep_files_custom_skip_dirs():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        # With custom skip_dirs that includes "src", should skip src/
        result = ripgrep_files([tmp], "hello", skip_dirs=["src"])
        assert "No matches" in result


# ---------------------------------------------------------------------------
# ripgrep — Claude-compatible wrapper
# ---------------------------------------------------------------------------


def test_ripgrep_basic():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep("hello", path=tmp)
        # Default output_mode is files_with_matches
        assert "main.py" in result
        assert "print" not in result  # no content in files_with_matches mode


def test_ripgrep_content_mode():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep("hello", path=tmp, output_mode="content")
        assert "1 match" in result
        assert "main.py:2:" in result


def test_ripgrep_type_filter():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep("line", path=tmp, type="txt")
        assert "data.txt" in result


def test_ripgrep_context():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep("hello", path=tmp, output_mode="content", context=1)
        assert "import os" in result
        assert "TODO" in result


def test_ripgrep_case_insensitive():
    with tempfile.TemporaryDirectory() as tmp:
        _make_tree(Path(tmp))
        result = ripgrep("HELLO", path=tmp, case_insensitive=True)
        assert "main.py" in result


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


def test_read_file_csv_rows():
    with tempfile.TemporaryDirectory() as tmp:
        csv = Path(tmp) / "data.csv"
        csv.write_text("name,value\nalpha,1\nbeta,2\ngamma,3\ndelta,4\n")
        result = read_file("data.csv", [tmp], rows=[1, 2])
        assert "name,value" in result  # header always included
        assert "beta,2" in result
        assert "gamma,3" in result
        assert "rows 1-2 of 4 total" in result


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
            "data.txt",
            [tmp],
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


# ---------------------------------------------------------------------------
# read_file — section parameter
# ---------------------------------------------------------------------------

HTML_DOC = (
    "<html><body>"
    '<div class="chapter" id="ch1"><h2>Chapter 1</h2><p>Intro</p></div>'
    '<section id="part-2"><h2>Part 2</h2>'
    '<div id="nested">inner</div>'
    "</section>"
    '<article id="post_3"><p>Post content</p></article>'
    "</body></html>"
)


def test_read_file_section_basic():
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "doc.html").write_text(HTML_DOC)
        result = read_file("doc.html", [tmp], section="ch1")
        assert result.startswith('<div class="chapter" id="ch1">')
        assert result.endswith("</div>")
        assert "Chapter 1" in result
        # No line numbers or header
        assert "lines" not in result


def test_read_file_section_different_tags():
    """section works for <section> and <article>, not just <div>."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "doc.html").write_text(HTML_DOC)
        result = read_file("doc.html", [tmp], section="part-2")
        assert result.startswith('<section id="part-2">')
        assert result.endswith("</section>")
        assert "nested" in result

        result2 = read_file("doc.html", [tmp], section="post_3")
        assert result2.startswith('<article id="post_3">')
        assert result2.endswith("</article>")


def test_read_file_section_nested():
    """Nested tags of the same type are balanced correctly."""
    with tempfile.TemporaryDirectory() as tmp:
        html = '<div id="outer"><div id="inner">deep</div></div><div id="after">next</div>'
        (Path(tmp) / "nested.html").write_text(html)
        result = read_file("nested.html", [tmp], section="outer")
        assert result == '<div id="outer"><div id="inner">deep</div></div>'


def test_read_file_section_not_found():
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "doc.html").write_text(HTML_DOC)
        result = read_file("doc.html", [tmp], section="nonexistent")
        assert "Error: section 'nonexistent' not found" in result


def test_read_file_section_max_chars():
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "doc.html").write_text(HTML_DOC)
        result = read_file("doc.html", [tmp], section="ch1", max_chars=20)
        assert "truncated" in result
        assert len(result.split("\n\n[... truncated")[0]) <= 20


def test_read_file_section_with_transform():
    """transform is applied before section extraction."""
    with tempfile.TemporaryDirectory() as tmp:
        # The raw file has an encoded id; transform decodes it before extraction
        encoded = '<div id="sec_1">&lt;em&gt;Hello&lt;/em&gt;</div>'
        (Path(tmp) / "doc.html").write_text(encoded)
        result = read_file(
            "doc.html",
            [tmp],
            section="sec_1",
            transform=lambda t: t.replace("&lt;", "<").replace("&gt;", ">"),
        )
        assert "<em>Hello</em>" in result


def test_read_file_section_utf8():
    """Section extraction handles multi-byte UTF-8 characters without panicking."""
    with tempfile.TemporaryDirectory() as tmp:
        html = '<div id="PARAGRAF_4-7"><h3>§ 4-7. Særlige regler — æøå</h3><p>Résumé über Ölfeld</p></div>'
        (Path(tmp) / "law.html").write_text(html, encoding="utf-8")
        result = read_file("law.html", [tmp], section="PARAGRAF_4-7")
        assert result.startswith("<div")
        assert "§ 4-7" in result
        assert "æøå" in result
        assert result.endswith("</div>")


def test_read_file_section_utf8_max_chars():
    """max_chars truncation doesn't split multi-byte characters."""
    with tempfile.TemporaryDirectory() as tmp:
        html = '<div id="s1">§§§§§§§§§§</div>'
        (Path(tmp) / "doc.html").write_text(html, encoding="utf-8")
        # § is 2 bytes; truncate at a boundary that would split one
        result = read_file("doc.html", [tmp], section="s1", max_chars=16)
        assert "truncated" in result
        # The pre-truncation part must be valid UTF-8 (no panic = pass)
        result.encode("utf-8")


def test_read_file_section_single_line():
    """Section extraction works on single-line HTML (the main use case)."""
    with tempfile.TemporaryDirectory() as tmp:
        single_line = (
            "<html>"
            + '<div id="s1">content1</div>' * 100
            + '<div id="s50">target</div>'
            + '<div id="s99">end</div>'
            + "</html>"
        )
        (Path(tmp) / "big.html").write_text(single_line)
        result = read_file("big.html", [tmp], section="s50")
        assert result == '<div id="s50">target</div>'


# ---------------------------------------------------------------------------
# read_file — grep parameter
# ---------------------------------------------------------------------------


def test_read_file_grep_basic():
    """grep returns matching lines with context."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line 10")
        assert "1 matches in 20 lines" in result
        assert "   10  line 10" in result
        # Default context=2
        assert "    8  line 8" in result
        assert "   12  line 12" in result


def test_read_file_grep_multiple_matches_merged():
    """Multiple matches with overlapping context are merged."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        # Matches on lines 5 and 7 — context windows overlap with default ctx=2
        result = read_file("data.txt", [tmp], grep="line (5|7)$")
        assert "2 matches in 20 lines" in result
        # Overlapping context should be merged (no separator)
        assert "--" not in result


def test_read_file_grep_separated_groups():
    """Non-contiguous matches produce -- separator."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        # Matches on lines 3 and 15 — far apart, should have separator
        result = read_file("data.txt", [tmp], grep="line (3|15)$")
        assert "2 matches" in result
        assert "\n--\n" in result


def test_read_file_grep_no_matches():
    """No matches returns header only."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.txt").write_text("hello\nworld\n")
        result = read_file("data.txt", [tmp], grep="nonexistent")
        assert "0 matches in 2 lines" in result
        # No numbered lines in output
        assert "    1" not in result


def test_read_file_grep_context_zero():
    """grep_context=0 returns only matching lines."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line 10", grep_context=0)
        assert "   10  line 10" in result
        assert "line 9" not in result
        assert "line 11" not in result


def test_read_file_grep_with_line_range():
    """grep operates within start_line/end_line range with absolute line numbers."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line 10", start_line=8, end_line=15)
        assert "1 matches in 20 lines" in result
        assert "   10  line 10" in result


def test_read_file_grep_with_line_range_no_match():
    """grep within a range that excludes the match returns 0 matches."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line 10", start_line=1, end_line=5)
        assert "0 matches" in result


def test_read_file_grep_with_section():
    """grep within an HTML section."""
    with tempfile.TemporaryDirectory() as tmp:
        html = '<div id="s1"><p>alpha</p>\n<p>beta match</p>\n<p>gamma</p></div>'
        (Path(tmp) / "doc.html").write_text(html)
        result = read_file("doc.html", [tmp], section="s1", grep="match")
        assert "section 's1'" in result
        assert "1 matches" in result
        assert "beta match" in result


def test_read_file_grep_with_transform():
    """transform runs before grep."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.txt").write_text("hello\nworld\n")
        result = read_file("data.txt", [tmp], transform=lambda t: t.upper(), grep="WORLD")
        assert "1 matches" in result
        assert "WORLD" in result


def test_read_file_grep_with_max_chars():
    """max_chars truncates grep output."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i} with some padding text" for i in range(1, 101))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line", max_chars=200)
        assert "truncated" in result


def test_read_file_grep_invalid_regex():
    """Invalid regex returns error message, not exception."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.txt").write_text("hello\n")
        result = read_file("data.txt", [tmp], grep="[invalid")
        assert "Error" in result
        assert "grep pattern" in result


def test_read_file_grep_case_insensitive():
    """Case insensitive grep using inline flag."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.txt").write_text("Hello World\ngoodbye\n")
        result = read_file("data.txt", [tmp], grep="(?i)hello")
        assert "1 matches" in result
        assert "Hello World" in result


def test_read_file_grep_rows_ignored():
    """grep is ignored when rows is set (CSV mode)."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.csv").write_text("name,value\nalpha,1\nbeta,2\n")
        result = read_file("data.csv", [tmp], rows=[0, 1], grep="alpha")
        # Should return CSV output, not grep output
        assert "rows 0-1" in result
        assert "matches" not in result
