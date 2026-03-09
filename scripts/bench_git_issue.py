"""Fetch a real GitHub issue and benchmark compaction."""

import json
import time

from mcp_methods import (
    ElementCache,
    collapse_code_blocks,
    compact_discussion,
    extract_github_refs,
    has_git_token,
    load_env,
)

load_env(".env")

REPO = "pydata/xarray"
ISSUE_NUMBER = 11199


def main():
    print("=" * 70)
    print(f"git_issue benchmark — {REPO}#{ISSUE_NUMBER}")
    print("=" * 70)

    if not has_git_token():
        print("\nNo GITHUB_TOKEN set. Set it to run this benchmark.")
        print("  export GITHUB_TOKEN=ghp_...")
        return

    cache = ElementCache()

    # Fetch with compact view (default) — uses Rust HTTP + parallel requests
    print("\n--- Compact fetch (Rust ureq + parallel) ---")
    t0 = time.perf_counter()
    result = cache.fetch_issue(REPO, ISSUE_NUMBER)
    dt_fetch = time.perf_counter() - t0

    print(f"  Time:   {dt_fetch * 1000:.0f} ms")
    print(f"  Size:   {len(result):,} chars")
    print(f"  Lines:  {result.count(chr(10)) + 1}")

    # Parse and show summary
    try:
        data = json.loads(result)
        print(f"  Type:   {data.get('type', '?')}")
        print(f"  Title:  {data.get('title', '?')[:80]}")
        print(f"  State:  {data.get('state', '?')}")
        print(f"  Author: {data.get('author', '?')}")
        n_comments = len(data.get("comments", []))
        n_reviews = len(data.get("reviews", []))
        print(f"  Comments: {n_comments}, Reviews: {n_reviews}")
        if data.get("_expand"):
            print(f"  Expand:   {data['_expand'][:100]}")
        if data.get("_bot_comments_hidden"):
            print(f"  Bot comments hidden: {data['_bot_comments_hidden']}")
    except json.JSONDecodeError:
        print("  (preview, not full JSON)")
        print(f"  First 200 chars: {result[:200]}")

    # Show cached elements
    available = cache.available(REPO, ISSUE_NUMBER)
    if available:
        print(f"\n  Cached elements: {', '.join(available)}")

    # Element drill-down test
    if available:
        eid = available[0]
        print(f"\n--- Element drill-down: {eid} ---")
        t0 = time.perf_counter()
        elem = cache.retrieve(REPO, ISSUE_NUMBER, eid)
        dt_elem = time.perf_counter() - t0
        print(f"  Time: {dt_elem * 1000:.2f} ms")
        print(f"  Size: {len(elem):,} chars")

    # Fetch with expand=all
    print("\n--- Full fetch (expand=['all']) ---")
    cache2 = ElementCache()
    t0 = time.perf_counter()
    result_full = cache2.fetch_issue(REPO, ISSUE_NUMBER, expand=["all"])
    dt_full = time.perf_counter() - t0
    print(f"  Time: {dt_full * 1000:.0f} ms")
    print(f"  Size: {len(result_full):,} chars")

    # Benchmark extract_github_refs on the full body
    try:
        full_data = json.loads(result_full)
        body = full_data.get("body", "")
        if body:
            print(f"\n--- extract_github_refs on body ({len(body)} chars) ---")
            t0 = time.perf_counter()
            for _ in range(1000):
                refs = extract_github_refs(body, REPO)
            dt_refs = time.perf_counter() - t0
            print(f"  1000 iterations: {dt_refs * 1000:.1f} ms ({dt_refs:.4f} ms/call)")
            print(f"  Refs found: {len(refs)}")
            for repo, num in sorted(refs)[:10]:
                print(f"    {repo}#{num}")
    except json.JSONDecodeError:
        pass

    # Benchmark compact_discussion on the full data
    print("\n--- compact_discussion benchmark ---")
    try:
        full_json = json.dumps(full_data, ensure_ascii=False)
        t0 = time.perf_counter()
        for _ in range(100):
            compact_discussion(full_json, [])
        dt_compact = time.perf_counter() - t0
        print(f"  100 iterations: {dt_compact * 1000:.1f} ms ({dt_compact * 10:.2f} ms/call)")
    except Exception as e:
        print(f"  Error: {e}")

    # Benchmark collapse_code_blocks
    if body and len(body) > 100:
        print(f"\n--- collapse_code_blocks benchmark ({len(body)} chars) ---")
        t0 = time.perf_counter()
        for _ in range(1000):
            collapse_code_blocks(body)
        dt_collapse = time.perf_counter() - t0
        print(f"  1000 iterations: {dt_collapse * 1000:.1f} ms ({dt_collapse:.4f} ms/call)")

    print("\n" + "=" * 70)
    print("Done.")


if __name__ == "__main__":
    main()
