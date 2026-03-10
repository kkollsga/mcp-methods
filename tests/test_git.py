"""Tests for mcp_methods git functionality (Rust-powered)."""

import json
import os
import tempfile
from unittest.mock import patch

from mcp_methods import (
    ElementCache,
    collapse_code_blocks,
    compact_discussion,
    detect_git_repo,
    extract_github_refs,
    git_api,
    github_discussions,
    has_git_token,
    ripgrep_lines,
    validate_repo,
)

# ---------------------------------------------------------------------------
# validate_repo
# ---------------------------------------------------------------------------


def test_validate_repo_valid():
    assert validate_repo("numpy/numpy") is None
    assert validate_repo("org/repo") is None


def test_validate_repo_invalid():
    assert validate_repo("noslash") is not None
    assert validate_repo("a/b/c") is not None
    assert validate_repo("/repo") is not None
    assert validate_repo("org/") is not None


# ---------------------------------------------------------------------------
# extract_github_refs
# ---------------------------------------------------------------------------


def test_extract_github_refs_url():
    text = "See https://github.com/org/repo/issues/42 for details"
    refs = extract_github_refs(text, "default/repo")
    assert ("org/repo", 42) in refs


def test_extract_github_refs_cross_ref():
    text = "Related to other/repo#99"
    refs = extract_github_refs(text, "default/repo")
    assert ("other/repo", 99) in refs


def test_extract_github_refs_short():
    text = "Fixes #123"
    refs = extract_github_refs(text, "default/repo")
    assert ("default/repo", 123) in refs


def test_extract_github_refs_empty():
    assert len(extract_github_refs("", "repo/x")) == 0
    assert len(extract_github_refs("no refs here", "repo/x")) == 0


# ---------------------------------------------------------------------------
# has_git_token
# ---------------------------------------------------------------------------


def test_has_git_token_true():
    with patch.dict(os.environ, {"GITHUB_TOKEN": "ghp_test"}):
        assert has_git_token() is True


def test_has_git_token_false():
    env = os.environ.copy()
    env.pop("GITHUB_TOKEN", None)
    env.pop("GH_TOKEN", None)
    with patch.dict(os.environ, env, clear=True):
        assert has_git_token() is False


# ---------------------------------------------------------------------------
# detect_git_repo (Rust — uses real git subprocess)
# ---------------------------------------------------------------------------


def test_detect_git_repo_real():
    # This test runs in a real git repo (our project)
    result = detect_git_repo(".")
    assert result is not None
    assert "/" in result  # Should be org/repo format


def test_detect_git_repo_not_git():
    with tempfile.TemporaryDirectory() as tmpdir:
        assert detect_git_repo(tmpdir) is None


# ---------------------------------------------------------------------------
# ElementCache
# ---------------------------------------------------------------------------


def test_element_cache_store_and_get():
    cache = ElementCache()
    elements = json.dumps({"cb_1": {"type": "code_block", "content": "x"}})
    cache.store_elements("org/repo", 1, elements)
    result = cache.get("org/repo", 1, "cb_1")
    assert result is not None
    assert json.loads(result)["content"] == "x"
    assert cache.get("org/repo", 1, "missing") is None


def test_element_cache_available():
    cache = ElementCache()
    elements = json.dumps({"cb_1": {}, "comment_1": {}})
    cache.store_elements("org/repo", 1, elements)
    assert cache.available("org/repo", 1) == ["cb_1", "comment_1"]
    assert cache.available("org/repo", 999) == []


def test_element_cache_update():
    cache = ElementCache()
    cache.store_elements("org/repo", 1, json.dumps({"cb_1": {"content": "a"}}))
    cache.update_elements("org/repo", 1, json.dumps({"overflow": {"content": "b"}}))
    assert cache.get("org/repo", 1, "cb_1") is not None
    result = cache.get("org/repo", 1, "overflow")
    assert json.loads(result)["content"] == "b"


def test_element_cache_retrieve():
    cache = ElementCache()
    cache.store_elements(
        "org/repo",
        1,
        json.dumps({"cb_1": {"type": "code_block", "content": "line1\nline2\nline3"}}),
    )
    # Full content
    result = cache.retrieve("org/repo", 1, "cb_1")
    assert "line1" in result
    # Line slicing
    result = cache.retrieve("org/repo", 1, "cb_1", lines="1-2")
    data = json.loads(result)
    assert "line1" in data["content"]
    assert "line3" not in data["content"]
    # Not found
    result = cache.retrieve("org/repo", 1, "missing")
    assert "not found" in result.lower()


def test_element_cache_fetch_discussion_no_token():
    cache = ElementCache()
    env = os.environ.copy()
    env.pop("GITHUB_TOKEN", None)
    env.pop("GH_TOKEN", None)
    with patch.dict(os.environ, env, clear=True):
        result = cache.fetch_discussion("org/repo", 1)
        assert "token" in result.lower()


def test_element_cache_refresh_returns_cached():
    """When cache has entries and refresh=False, returns summary instead of re-fetching."""
    cache = ElementCache()
    cache.store_elements("org/repo", 1, json.dumps({"cb_1": {"content": "x"}}))
    result = cache.fetch_discussion("org/repo", 1)
    assert "Cached" in result
    assert "cb_1" in result
    assert "refresh=True" in result


def test_element_cache_refresh_empty_fetches():
    """When cache is empty, fetch_discussion attempts network fetch (fails without token)."""
    cache = ElementCache()
    env = os.environ.copy()
    env.pop("GITHUB_TOKEN", None)
    env.pop("GH_TOKEN", None)
    with patch.dict(os.environ, env, clear=True):
        result = cache.fetch_discussion("org/repo", 1)
        assert "token" in result.lower()  # no cache, tried to fetch


# ---------------------------------------------------------------------------
# github_discussions
# ---------------------------------------------------------------------------


def test_github_discussions_invalid_repo():
    result = github_discussions(repo="bad-repo", number=1)
    assert "Invalid repo" in result


def test_github_discussions_no_token():
    env = os.environ.copy()
    env.pop("GITHUB_TOKEN", None)
    env.pop("GH_TOKEN", None)
    with patch.dict(os.environ, env, clear=True):
        try:
            result = github_discussions(repo="org/repo", number=1)
            assert "token" in result.lower()
        except RuntimeError as e:
            assert "token" in str(e).lower()


def test_github_discussions_list_invalid_repo():
    result = github_discussions(repo="bad-repo", kind="issue")
    assert "Invalid repo" in result


def test_github_discussions_auto_detect_repo():
    # In our git repo, with no number, should attempt listing (may fail without token)
    result = github_discussions(kind="issue", limit=1)
    # Either lists results or returns an error about token/rate limit
    assert isinstance(result, str)
    assert len(result) > 0


# ---------------------------------------------------------------------------
# collapse_code_blocks
# ---------------------------------------------------------------------------


def test_collapse_code_blocks_small_block():
    text = "```python\nline1\nline2\n```"
    result, _ = collapse_code_blocks(text)
    assert result == text


def test_collapse_code_blocks_large_block():
    inner = "\n".join(f"line{i}" for i in range(30))
    text = f"```python\n{inner}\n```"
    result, _ = collapse_code_blocks(text)
    assert "... (" in result
    assert "lines hidden" in result


def test_collapse_code_blocks_caches_elements():
    inner = "\n".join(f"line{i}" for i in range(30))
    text = f"```python\n{inner}\n```"
    cache_json = json.dumps({"_n": 0})
    _, new_cache_json = collapse_code_blocks(text, cache_json)
    cache = json.loads(new_cache_json)
    assert "cb_1" in cache
    assert cache["cb_1"]["type"] == "code_block"
    assert cache["cb_1"]["language"] == "python"


# ---------------------------------------------------------------------------
# compact_discussion
# ---------------------------------------------------------------------------


def test_compact_discussion_filters_bots():
    discussion = {
        "body": "test",
        "comments": [
            {"author": "real-user", "author_association": "MEMBER", "body": "hi"},
            {"author": "dependabot[bot]", "author_association": "NONE", "body": "bump"},
        ],
    }
    result_json, _ = compact_discussion(json.dumps(discussion), [])
    result = json.loads(result_json)
    assert len(result["comments"]) == 1
    assert result["comments"][0]["author"] == "real-user"
    assert result["_bot_comments_hidden"] == 1


def test_compact_discussion_expand_all():
    discussion = {
        "body": "test" * 5000,
        "comments": [
            {"author": "bot[bot]", "body": "bump"},
        ],
    }
    result_json, _ = compact_discussion(json.dumps(discussion), ["all"])
    result = json.loads(result_json)
    assert len(result["comments"]) == 1  # bot kept


def test_compact_discussion_patches_small_diff_inline():
    """Small diffs keep patches inline while still caching for drill-down."""
    discussion = {
        "body": "Fix bug",
        "files": [
            {
                "filename": "src/main.py",
                "status": "modified",
                "additions": 10,
                "deletions": 3,
                "patch": "@@ -1,5 +1,12 @@\n+import os\n def main():\n     pass",
            },
            {
                "filename": "tests/test_main.py",
                "status": "added",
                "additions": 20,
                "deletions": 0,
                "patch": "@@ -0,0 +1,20 @@\n+def test_main():\n+    assert True",
            },
        ],
    }
    cache_json = json.dumps({"_n": 0})
    result_json, new_cache_json = compact_discussion(json.dumps(discussion), [], cache_json)
    result = json.loads(result_json)
    cache = json.loads(new_cache_json)

    # Small diff: patches stay inline AND have patch_id for drill-down
    for f in result["files"]:
        assert "patch" in f
        assert "patch_id" in f

    # Cache should contain patch elements for drill-down
    assert "patch_1" in cache
    assert cache["patch_1"]["type"] == "patch"
    assert cache["patch_1"]["filename"] == "src/main.py"
    assert cache["patch_1"]["additions"] == 10
    assert cache["patch_1"]["deletions"] == 3
    assert "@@ -1,5" in cache["patch_1"]["content"]

    assert "patch_2" in cache
    assert cache["patch_2"]["filename"] == "tests/test_main.py"


def test_compact_discussion_patches_large_diff_collapsed():
    """Large diffs collapse patches into navigation tree with patch_ids only."""
    # Generate a large patch (> 200 lines total)
    large_patch = "@@ -1,5 +1,250 @@\n" + "\n".join(f"+line {i}" for i in range(250))
    discussion = {
        "body": "Big refactor",
        "files": [
            {
                "filename": "src/engine.py",
                "status": "modified",
                "additions": 250,
                "deletions": 5,
                "patch": large_patch,
            },
        ],
    }
    cache_json = json.dumps({"_n": 0})
    result_json, new_cache_json = compact_discussion(json.dumps(discussion), [], cache_json)
    result = json.loads(result_json)
    cache = json.loads(new_cache_json)

    # Large diff: patch removed from inline, only patch_id remains
    f = result["files"][0]
    assert "patch" not in f
    assert "patch_id" in f

    # Cache has the full patch
    assert "patch_1" in cache
    assert cache["patch_1"]["total_lines"] == 251
    assert cache["patch_1"]["filename"] == "src/engine.py"


def test_compact_discussion_patches_expanded():
    """With expand=['patches'], raw patches are kept inline."""
    discussion = {
        "body": "Fix bug",
        "files": [
            {
                "filename": "src/main.py",
                "status": "modified",
                "additions": 5,
                "deletions": 2,
                "patch": "@@ -1,3 +1,6 @@\n+import os",
            },
        ],
    }
    result_json, _ = compact_discussion(json.dumps(discussion), ["patches"])
    result = json.loads(result_json)
    # Patches kept inline
    assert result["files"][0]["patch"] == "@@ -1,3 +1,6 @@\n+import os"
    assert "patch_id" not in result["files"][0]


def test_compact_discussion_patch_drilldown():
    """Cached patches work with ElementCache retrieve (grep/lines)."""
    discussion = {
        "body": "Fix bug",
        "files": [
            {
                "filename": "src/main.py",
                "status": "modified",
                "additions": 5,
                "deletions": 2,
                "patch": "@@ -1,3 +1,6 @@\n+import os\n def main():\n-    pass\n+    print('hello')",
            },
        ],
    }
    cache = ElementCache()
    cache.compact_and_store("org/repo", 42, json.dumps(discussion), [])

    # Verify patch was cached
    available = cache.available("org/repo", 42)
    patch_ids = [eid for eid in available if eid.startswith("patch_")]
    assert len(patch_ids) == 1

    # Drill into patch with grep
    grep_result = cache.retrieve("org/repo", 42, patch_ids[0], grep="import")
    assert "import os" in grep_result

    # Drill into patch with lines
    lines_result = cache.retrieve("org/repo", 42, patch_ids[0], lines="1-2")
    parsed = json.loads(lines_result)
    assert "content" in parsed


# ---------------------------------------------------------------------------
# ripgrep_lines
# ---------------------------------------------------------------------------


def test_ripgrep_lines_basic():
    lines = ["alpha", "beta", "gamma", "delta", "alpha again"]
    matches = ripgrep_lines(lines, "alpha", 1)
    assert len(matches) == 2
    assert 1 in matches[0]["lines"]
    assert 5 in matches[1]["lines"]


def test_ripgrep_lines_overlapping_context():
    lines = ["a", "b", "match1", "c", "match2", "d", "e"]
    matches = ripgrep_lines(lines, "match", 1)
    # The two matches are close enough that their context windows merge
    assert len(matches) == 1
    assert 3 in matches[0]["lines"]
    assert 5 in matches[0]["lines"]


# ---------------------------------------------------------------------------
# git_api (Rust — uses real ureq HTTP, only test validation)
# ---------------------------------------------------------------------------


def test_git_api_invalid_repo():
    result = git_api("bad-repo", "pulls")
    assert "Invalid repo" in result
