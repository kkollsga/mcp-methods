"""Head-to-head benchmark: mcp_methods.grep_files vs native rg (ripgrep)."""

import statistics
import subprocess
import time

from mcp_methods import grep_files

RG = "/opt/homebrew/bin/rg"
KGLITE = "/Volumes/EksternalHome/Koding/Rust/KGLite"
SCRAPED = "/Volumes/EksternalHome/Koding/Python/Scraping/processed"


def bench(label, fn, runs=5):
    """Run fn multiple times, report median."""
    times = []
    for _ in range(runs):
        t0 = time.perf_counter()
        fn()
        t1 = time.perf_counter()
        times.append((t1 - t0) * 1000)
    return statistics.median(times)


def rg_run(args, cwd=None):
    """Run native rg, return stdout."""
    subprocess.run(
        [RG] + args,
        capture_output=True,
        cwd=cwd,
        timeout=120,
    )


def compare(label, ours_fn, rg_args, corpus, runs=5):
    """Run both and print comparison."""
    ours = bench(label, ours_fn, runs=runs)
    rg_time = bench(label, lambda: rg_run(rg_args, cwd=corpus), runs=runs)
    ratio = ours / rg_time if rg_time > 0 else float("inf")
    marker = "✓" if ratio <= 1.5 else ("~" if ratio <= 3.0 else "✗")
    print(
        f"  {label:48s}  ours: {ours:8.1f} ms  rg: {rg_time:8.1f} ms  ratio: {ratio:.2f}x  {marker}"
    )
    return ours, rg_time


def main():
    print("=" * 105)
    print("HEAD-TO-HEAD: mcp_methods.grep_files vs native rg 15.1.0")
    print("  ✓ = within 1.5x of rg    ~ = within 3x    ✗ = slower than 3x")
    print("=" * 105)

    # ── KGLite ──────────────────────────────────────────────────────────
    print(f"\n{'─' * 105}")
    print(
        "CORPUS: KGLite — Rust project (163K files, 39GB, .gitignore filters to ~5K source files)"
    )
    print(f"{'─' * 105}")

    print("\n  Pattern searches (with .gitignore):")
    compare(
        "Simple literal: 'fn main'",
        lambda: grep_files([KGLITE], r"fn main", max_results=50),
        ["fn main", "-m", "50"],
        KGLITE,
    )
    compare(
        "Common pattern: 'fn \\w+'",
        lambda: grep_files([KGLITE], r"fn \w+", max_results=50),
        [r"fn \w+", "-m", "50"],
        KGLITE,
    )
    compare(
        "Case insensitive: 'error'",
        lambda: grep_files(
            [KGLITE], "error", case_insensitive=True, type_filter="rust", max_results=50
        ),
        ["error", "-i", "-t", "rust", "-m", "50"],
        KGLITE,
    )
    compare(
        "No match: 'XYZZY_NONEXISTENT'",
        lambda: grep_files([KGLITE], "XYZZY_NONEXISTENT", type_filter="rust"),
        ["XYZZY_NONEXISTENT", "-t", "rust"],
        KGLITE,
    )
    compare(
        "Complex regex: 'fn\\s+\\w+<[^>]+>'",
        lambda: grep_files([KGLITE], r"fn\s+\w+<[^>]+>", type_filter="rust", max_results=50),
        [r"fn\s+\w+<[^>]+>", "-t", "rust", "-m", "50"],
        KGLITE,
    )

    print("\n  Context & output modes:")
    compare(
        "Context -C 3: 'panic!'",
        lambda: grep_files([KGLITE], r"panic!", context=3, type_filter="rust", max_results=20),
        ["panic!", "-C", "3", "-t", "rust", "-m", "20"],
        KGLITE,
    )
    compare(
        "Files only (-l): 'struct'",
        lambda: grep_files(
            [KGLITE],
            r"struct",
            output_mode="files_with_matches",
            type_filter="rust",
            max_results=50,
        ),
        ["struct", "-l", "-t", "rust", "-m", "50"],
        KGLITE,
    )
    compare(
        "Count (-c): 'use '",
        lambda: grep_files(
            [KGLITE], r"use ", output_mode="count", type_filter="rust", max_results=50
        ),
        ["use ", "-c", "-t", "rust", "-m", "50"],
        KGLITE,
    )
    compare(
        "Multiline: 'struct.*\\n.*pub'",
        lambda: grep_files(
            [KGLITE], r"struct \w+.*\n.*pub", multiline=True, type_filter="rust", max_results=20
        ),
        ["-U", r"struct \w+.*\n.*pub", "-t", "rust", "-m", "20"],
        KGLITE,
    )

    print("\n  Glob & type filters:")
    compare(
        "Glob --glob '*.rs': 'impl'",
        lambda: grep_files([KGLITE], r"impl ", glob="*.rs", max_results=50),
        ["impl ", "--glob", "*.rs", "-m", "50"],
        KGLITE,
    )
    compare(
        "Type --type py: 'def '",
        lambda: grep_files([KGLITE], r"def ", type_filter="py", max_results=50),
        ["def ", "-t", "py", "-m", "50"],
        KGLITE,
    )

    # ── HTML corpus ─────────────────────────────────────────────────────
    print(f"\n{'─' * 105}")
    print("CORPUS: Scraping/processed — 53K HTML files, 2.1GB (no .gitignore)")
    print(f"{'─' * 105}")

    print("\n  Pattern searches:")
    compare(
        "Literal: '<title>'",
        lambda: grep_files([SCRAPED], r"<title>", max_results=50),
        ["<title>", "-m", "50"],
        SCRAPED,
    )
    compare(
        "Common tag: '<div'",
        lambda: grep_files([SCRAPED], r"<div", max_results=50),
        ["<div", "-m", "50"],
        SCRAPED,
    )
    compare(
        "Complex regex: 'class=\"[^\"]*nav'",
        lambda: grep_files([SCRAPED], r'class="[^"]*nav', max_results=50),
        [r'class="[^"]*nav', "-m", "50"],
        SCRAPED,
    )

    print("\n  Worst case — full scan, no matches:")
    compare(
        "No match full scan (53K files, 2.1GB)",
        lambda: grep_files([SCRAPED], "XYZZY_NONEXISTENT_42"),
        ["XYZZY_NONEXISTENT_42"],
        SCRAPED,
        runs=3,
    )

    print("\n  Context & output modes:")
    compare(
        "Context -C 2: '<h1>'",
        lambda: grep_files([SCRAPED], r"<h1>", context=2, max_results=20),
        ["<h1>", "-C", "2", "-m", "20"],
        SCRAPED,
    )
    compare(
        "Files only (-l): '<table'",
        lambda: grep_files([SCRAPED], r"<table", output_mode="files_with_matches", max_results=100),
        ["<table", "-l", "-m", "100"],
        SCRAPED,
    )
    compare(
        "Count (-c): '<a '",
        lambda: grep_files([SCRAPED], r"<a ", output_mode="count", max_results=100),
        ["<a ", "-c", "-m", "100"],
        SCRAPED,
    )
    compare(
        "High cap max_results=500: '<div'",
        lambda: grep_files([SCRAPED], r"<div", max_results=500),
        ["<div", "-m", "500"],
        SCRAPED,
    )


if __name__ == "__main__":
    main()
