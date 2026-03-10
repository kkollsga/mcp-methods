"""Benchmark ripgrep_files against a real codebase."""

import time

from mcp_methods import ripgrep_files

SOURCE_DIR = "/Volumes/EksternalHome/Koding/Rust/KGLite"

CASES = [
    ("Simple word", "cypher", "*.py", False),
    ("Regex pattern", r"def\s+\w+_query", "*.py", False),
    ("Case insensitive", "graph", "*.rs", True),
    ("All files", "TODO", "*", False),
    ("Rust files", "fn ", "*.rs", False),
    ("Markdown", "performance", "*.md", True),
]


def main():
    print("=" * 70)
    print(f"ripgrep_files benchmark — source: {SOURCE_DIR}")
    print("=" * 70)

    for label, pattern, glob, ci in CASES:
        t0 = time.perf_counter()
        result = ripgrep_files(
            [SOURCE_DIR], pattern, glob=glob, case_insensitive=ci, max_results=100
        )
        dt = time.perf_counter() - t0

        lines = result.strip().split("\n")
        header = lines[0] if lines else ""
        print(f"\n{label:25s}  {dt * 1000:7.1f} ms  {header}")

    print("\n" + "=" * 70)
    print("Done.")


if __name__ == "__main__":
    main()
