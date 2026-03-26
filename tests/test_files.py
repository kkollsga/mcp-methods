"""Tests for mcp_methods.files."""

import tempfile
from pathlib import Path

from mcp_methods import html_to_text, read_file, ripgrep, ripgrep_files


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


def test_read_file_grep_max_matches():
    """max_matches limits how many matches are included."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 51))
        (Path(tmp) / "data.txt").write_text(content)
        # All 50 lines match "line", limit to 5
        result = read_file("data.txt", [tmp], grep="line", max_matches=5)
        assert "showing 5 of 50 matches" in result
        # First 5 matches present (lines 1-5)
        assert "    1  line 1" in result
        assert "    5  line 5" in result
        # Line 10 should NOT be present
        assert "line 10" not in result


def test_read_file_grep_max_matches_larger_than_total():
    """max_matches larger than total matches shows normal header."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i}" for i in range(1, 21))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line 1$", max_matches=100)
        assert "1 matches in 20 lines" in result
        assert "showing" not in result


def test_read_file_grep_max_matches_with_context():
    """max_matches works with grep_context."""
    with tempfile.TemporaryDirectory() as tmp:
        # Lines 1-30, matches on 5, 15, 25 — well separated
        content = "\n".join(f"line {i}" for i in range(1, 31))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file(
            "data.txt",
            [tmp],
            grep="line (5|15|25)$",
            grep_context=1,
            max_matches=2,
        )
        assert "showing 2 of 3 matches" in result
        # First two matches with context
        assert "line 5" in result
        assert "line 15" in result
        # Third match excluded
        assert "line 25" not in result


def test_read_file_grep_max_matches_with_section():
    """max_matches works within HTML section grep."""
    with tempfile.TemporaryDirectory() as tmp:
        lines = "\n".join(f"<p>item {i}</p>" for i in range(1, 11))
        html = f'<div id="s1">\n{lines}\n</div>'
        (Path(tmp) / "doc.html").write_text(html)
        result = read_file(
            "doc.html",
            [tmp],
            section="s1",
            grep="item",
            max_matches=3,
        )
        assert "showing 3 of 10 matches" in result
        assert "section 's1'" in result


def test_read_file_grep_max_chars_shows_match_count():
    """max_chars truncation footer includes total match count."""
    with tempfile.TemporaryDirectory() as tmp:
        content = "\n".join(f"line {i} with some padding text" for i in range(1, 101))
        (Path(tmp) / "data.txt").write_text(content)
        result = read_file("data.txt", [tmp], grep="line", max_chars=200)
        assert "truncated" in result
        assert "100 matches" in result


# ---------------------------------------------------------------------------
# html_to_text — standalone function
# ---------------------------------------------------------------------------


def test_html_to_text_strips_head():
    html = "<html><head><title>T</title><style>body{}</style></head><body>Hello</body></html>"
    assert html_to_text(html) == "Hello"


def test_html_to_text_headings():
    html = "<h1>Title</h1><h3>Sub</h3><p>Text</p>"
    result = html_to_text(html)
    assert "# Title" in result
    assert "### Sub" in result
    assert "Text" in result


def test_html_to_text_list_items():
    html = "<ul><li>Alpha</li><li>Beta</li></ul>"
    result = html_to_text(html)
    assert "- Alpha" in result
    assert "- Beta" in result


def test_html_to_text_bold():
    html = "<p>Hello <strong>world</strong> and <b>rust</b></p>"
    result = html_to_text(html)
    assert "**world**" in result
    assert "**rust**" in result


def test_html_to_text_images():
    html = '<img alt="logo" src="logo.png"><img src="spacer.gif">'
    result = html_to_text(html)
    assert "[image: logo]" in result
    assert "spacer" not in result


def test_html_to_text_tables():
    html = "<table><tr><th>Name</th><th>Age</th></tr><tr><td>Alice</td><td>30</td></tr></table>"
    result = html_to_text(html)
    assert "Name" in result
    assert "Alice" in result
    assert "30" in result


def test_html_to_text_entities():
    html = "<p>&lt;tag&gt; &amp; &quot;quotes&quot; &#169; &#x00A7;</p>"
    result = html_to_text(html)
    assert "<tag>" in result
    assert '& "quotes"' in result
    assert "\u00a9" in result  # ©
    assert "\u00a7" in result  # §


def test_html_to_text_double_encoded_entities():
    """&amp;lt; should become literal &lt;, not <"""
    html = "<p>&amp;lt; test</p>"
    result = html_to_text(html)
    assert "&lt;" in result
    assert "< test" not in result


def test_html_to_text_scripts_removed():
    html = "<p>A</p><script>alert('x')</script><style>.a{}</style><p>B</p>"
    result = html_to_text(html)
    assert "A" in result
    assert "B" in result
    assert "alert" not in result


def test_html_to_text_links_stripped():
    html = '<a href="https://example.com">click here</a>'
    result = html_to_text(html)
    assert "click here" in result
    assert "https://" not in result


def test_html_to_text_whitespace():
    html = "<p>  lots   of   spaces  </p>\n\n\n\n<p>after gap</p>"
    result = html_to_text(html)
    assert "   " not in result
    assert "\n\n\n" not in result


def test_html_to_text_br_tags():
    html = "line1<br>line2<br/>line3<br />line4"
    result = html_to_text(html)
    assert "line1\nline2" in result
    assert "line3" in result


def test_html_to_text_comments():
    html = "<p>A<!-- hidden -->B</p>"
    result = html_to_text(html)
    assert "hidden" not in result


def test_html_to_text_complex_legal_doc():
    """Realistic legal HTML document."""
    html = """<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>§ 4-7</title>
<style>body{font-family:serif}</style></head>
<body>
<h1>Lov om aksjeselskaper</h1>
<section id="PARAGRAF_4-7">
  <h2>&sect; 4-7. S&aelig;rlige regler</h2>
  <p>F&oslash;lgende regler gjelder for <strong>overdragelse</strong> av aksjer:</p>
  <ul>
    <li>Styret kan nekte samtykke</li>
    <li>Frist p&aring; <b>to m&aring;neder</b></li>
  </ul>
  <p>Se ogs&aring; <a href="/lov/1997-06-13-44/§4-8">§ 4-8</a>.</p>
</section>
</body></html>"""
    result = html_to_text(html)
    assert "# Lov om aksjeselskaper" in result
    assert "## § 4-7. Særlige regler" in result
    assert "**overdragelse**" in result
    assert "- Styret kan nekte samtykke" in result
    assert "**to måneder**" in result
    assert "§ 4-8" in result
    assert "href" not in result
    assert "<" not in result or "&lt;" in html  # no stray tags


# ---------------------------------------------------------------------------
# read_file — transform="html"
# ---------------------------------------------------------------------------


def test_read_file_transform_html_basic():
    """transform='html' converts HTML to clean text."""
    with tempfile.TemporaryDirectory() as tmp:
        html = "<html><head><title>T</title></head><body><h1>Hello</h1><p>World</p></body></html>"
        (Path(tmp) / "doc.html").write_text(html)
        result = read_file("doc.html", [tmp], transform="html")
        assert "# Hello" in result
        assert "World" in result
        assert "<head>" not in result
        assert "<title>" not in result


def test_read_file_transform_html_with_section():
    """transform='html' is applied after section extraction."""
    with tempfile.TemporaryDirectory() as tmp:
        html = (
            '<div id="s1"><h2>Title</h2><p>Hello <strong>world</strong></p></div>'
            '<div id="s2"><p>Other</p></div>'
        )
        (Path(tmp) / "doc.html").write_text(html)
        result = read_file("doc.html", [tmp], section="s1", transform="html")
        assert "## Title" in result
        assert "**world**" in result
        assert "Other" not in result
        # Section extraction works because ids are still in raw HTML
        assert "<div" not in result


def test_read_file_transform_html_with_grep():
    """grep matches against clean text, not raw HTML."""
    with tempfile.TemporaryDirectory() as tmp:
        html = "<p>line one</p>\n<p>line two</p>\n<p>line three</p>"
        (Path(tmp) / "doc.html").write_text(html)
        result = read_file("doc.html", [tmp], transform="html", grep="two")
        assert "1 matches" in result
        assert "two" in result
        # Should NOT match HTML tags
        result2 = read_file("doc.html", [tmp], transform="html", grep="<p>")
        assert "0 matches" in result2


def test_read_file_transform_html_section_grep():
    """Section extraction + html transform + grep all work together."""
    with tempfile.TemporaryDirectory() as tmp:
        lines = "\n".join(f"<p>item {i}</p>" for i in range(1, 11))
        html = f'<div id="s1">\n{lines}\n</div>'
        (Path(tmp) / "doc.html").write_text(html)
        result = read_file("doc.html", [tmp], section="s1", transform="html", grep="item 5")
        assert "1 matches" in result
        assert "item 5" in result


def test_read_file_transform_unknown():
    """Unknown transform name returns error."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.txt").write_text("hello")
        result = read_file("data.txt", [tmp], transform="xml")
        assert "Error" in result
        assert "unknown transform" in result


def test_read_file_transform_callable_still_works():
    """Callable transform still works as before."""
    with tempfile.TemporaryDirectory() as tmp:
        (Path(tmp) / "data.txt").write_text("hello\nworld\n")
        result = read_file("data.txt", [tmp], transform=lambda t: t.upper())
        assert "HELLO" in result
        assert "WORLD" in result
