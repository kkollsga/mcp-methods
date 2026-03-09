"""Thorough benchmark of ripgrep-powered grep_files across real codebases."""

import time
import statistics
from mcp_methods import grep_files

KGLITE = "/Volumes/EksternalHome/Koding/Rust/KGLite"
SCRAPED = "/Volumes/EksternalHome/Koding/Python/Scraping/processed"


def bench(label, fn, runs=3):
    """Run fn multiple times, report median and all times."""
    times = []
    result = None
    for _ in range(runs):
        t0 = time.perf_counter()
        result = fn()
        t1 = time.perf_counter()
        times.append((t1 - t0) * 1000)
    median = statistics.median(times)
    # Extract summary from first line of result
    first_line = result.split("\n")[0] if result else "(empty)"
    if len(first_line) > 100:
        first_line = first_line[:100] + "..."
    print(f"  {label:50s} {median:8.1f} ms  (runs: {', '.join(f'{t:.1f}' for t in times)})")
    print(f"    -> {first_line}")
    return median


def main():
    print("=" * 90)
    print("RIPGREP BENCHMARK — grep_files (all features)")
    print("=" * 90)

    # -------------------------------------------------------------------------
    # KGLite: 163K files, 39GB (Rust project with build artifacts)
    # -------------------------------------------------------------------------
    print(f"\n{'─' * 90}")
    print(f"CORPUS: KGLite — 163K files, 39GB (Rust project)")
    print(f"{'─' * 90}")

    print("\n[1] Basic search — common pattern across all files")
    bench("grep 'fn ' (all files, gitignore ON)",
          lambda: grep_files([KGLITE], r"fn \w+", max_results=50))

    print("\n[2] Basic search — gitignore OFF (scans build artifacts)")
    bench("grep 'fn ' (all files, gitignore OFF)",
          lambda: grep_files([KGLITE], r"fn \w+", respect_gitignore=False, max_results=50))

    print("\n[3] Type filter — only Rust files")
    bench("grep 'struct' type=rust",
          lambda: grep_files([KGLITE], r"struct \w+", type_filter="rust", max_results=50))

    print("\n[4] Type filter — only Python files")
    bench("grep 'def ' type=py",
          lambda: grep_files([KGLITE], r"def \w+", type_filter="py", max_results=50))

    print("\n[5] Glob filter — *.rs files only")
    bench("grep 'impl' glob=*.rs",
          lambda: grep_files([KGLITE], r"impl ", glob="*.rs", max_results=50))

    print("\n[6] Case-insensitive search")
    bench("grep 'error' case_insensitive",
          lambda: grep_files([KGLITE], "error", case_insensitive=True, type_filter="rust", max_results=50))

    print("\n[7] Context lines (-C 3)")
    bench("grep 'panic!' context=3, type=rust",
          lambda: grep_files([KGLITE], r"panic!", context=3, type_filter="rust", max_results=20))

    print("\n[8] Context before/after (-B 2, -A 5)")
    bench("grep 'unsafe' -B2 -A5, type=rust",
          lambda: grep_files([KGLITE], r"unsafe", context_before=2, context_after=5, type_filter="rust", max_results=20))

    print("\n[9] Output mode: files_with_matches")
    bench("grep 'TODO' mode=files_with_matches",
          lambda: grep_files([KGLITE], "TODO", output_mode="files_with_matches", type_filter="rust", max_results=50))

    print("\n[10] Output mode: count")
    bench("grep 'use ' mode=count, type=rust",
          lambda: grep_files([KGLITE], r"use ", output_mode="count", type_filter="rust", max_results=50))

    print("\n[11] Multiline pattern")
    bench("grep 'struct.*\\n.*pub' multiline",
          lambda: grep_files([KGLITE], r"struct \w+.*\n.*pub", multiline=True, type_filter="rust", max_results=20))

    print("\n[12] head_limit + offset (pagination)")
    bench("grep 'let' head_limit=10 offset=5",
          lambda: grep_files([KGLITE], r"let ", head_limit=10, offset=5, type_filter="rust"))

    print("\n[13] No line numbers")
    bench("grep 'fn' line_numbers=False",
          lambda: grep_files([KGLITE], r"fn ", line_numbers=False, type_filter="rust", max_results=50))

    print("\n[14] Custom skip_dirs")
    bench("grep 'fn' skip_dirs=['target','venv']",
          lambda: grep_files([KGLITE], r"fn ", skip_dirs=["target", "venv", ".git"], max_results=50))

    print("\n[15] No match (rare pattern)")
    bench("grep 'XYZZY_NONEXISTENT_42'",
          lambda: grep_files([KGLITE], "XYZZY_NONEXISTENT_42", type_filter="rust"))

    print("\n[16] Complex regex")
    bench("grep 'fn\\s+\\w+<[^>]+>' (generics)",
          lambda: grep_files([KGLITE], r"fn\s+\w+<[^>]+>", type_filter="rust", max_results=50))

    print("\n[17] Transform callback (forces sequential)")
    bench("grep 'fn' with transform (seq)",
          lambda: grep_files([KGLITE], r"fn ", type_filter="rust", max_results=20,
                             transform=lambda t: t.lower()))

    print("\n[18] max_results cap test")
    bench("grep 'let' max_results=500",
          lambda: grep_files([KGLITE], r"let ", type_filter="rust", max_results=500))

    # -------------------------------------------------------------------------
    # Scraping/processed: 53K HTML files, 2.1GB
    # -------------------------------------------------------------------------
    print(f"\n{'─' * 90}")
    print(f"CORPUS: Scraping/processed — 53K files, 2.1GB (HTML corpus)")
    print(f"{'─' * 90}")

    print("\n[19] Basic search — HTML content")
    bench("grep '<title>' in HTML corpus",
          lambda: grep_files([SCRAPED], r"<title>", max_results=50))

    print("\n[20] Glob filter — *.html")
    bench("grep 'href=' glob=*.html",
          lambda: grep_files([SCRAPED], r"href=", glob="*.html", max_results=50))

    print("\n[21] Case-insensitive HTML tags")
    bench("grep '<div' case_insensitive",
          lambda: grep_files([SCRAPED], r"<div", case_insensitive=True, max_results=50))

    print("\n[22] files_with_matches on HTML")
    bench("grep '<table' mode=files_with_matches",
          lambda: grep_files([SCRAPED], r"<table", output_mode="files_with_matches", max_results=100))

    print("\n[23] Count mode on HTML")
    bench("grep '<a ' mode=count",
          lambda: grep_files([SCRAPED], r"<a ", output_mode="count", max_results=100))

    print("\n[24] Context in HTML")
    bench("grep '<h1>' context=2",
          lambda: grep_files([SCRAPED], r"<h1>", context=2, max_results=20))

    print("\n[25] Complex regex in HTML")
    bench("grep 'class=\"[^\"]*nav[^\"]*\"'",
          lambda: grep_files([SCRAPED], r'class="[^"]*nav[^"]*"', max_results=50))

    print("\n[26] No match in HTML")
    bench("grep 'XYZZY_NONEXISTENT_42'",
          lambda: grep_files([SCRAPED], "XYZZY_NONEXISTENT_42"))

    print("\n[27] max_results=500 on HTML")
    bench("grep '<div' max_results=500",
          lambda: grep_files([SCRAPED], r"<div", max_results=500))

    print("\n[28] Multiline HTML pattern")
    bench("grep '<head>.*\\n.*<title' multiline",
          lambda: grep_files([SCRAPED], r"<head>.*\n.*<title", multiline=True, max_results=20))

    # -------------------------------------------------------------------------
    # Both directories combined
    # -------------------------------------------------------------------------
    print(f"\n{'─' * 90}")
    print(f"COMBINED: Both corpora as multiple source_dirs")
    print(f"{'─' * 90}")

    print("\n[29] Multi-dir search")
    bench("grep 'import' in both dirs",
          lambda: grep_files([KGLITE, SCRAPED], r"import", max_results=50))

    print("\n[30] Multi-dir with type filter")
    bench("grep 'def ' type=py in both dirs",
          lambda: grep_files([KGLITE, SCRAPED], r"def ", type_filter="py", max_results=50))


if __name__ == "__main__":
    main()
