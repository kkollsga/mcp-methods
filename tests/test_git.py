"""Tests for mcp_methods git functionality (Rust-powered)."""

import json
import os
import tempfile
from unittest.mock import patch

from mcp_methods import (
    ElementCache,
    collapse_code_blocks,
    compact_discussion,
    grep_lines,
    detect_git_repo,
    extract_github_refs,
    git_api,
    has_git_token,
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


def test_element_cache_fetch_issue_no_token():
    cache = ElementCache()
    env = os.environ.copy()
    env.pop("GITHUB_TOKEN", None)
    env.pop("GH_TOKEN", None)
    with patch.dict(os.environ, env, clear=True):
        result = cache.fetch_issue("org/repo", 1)
        assert "token" in result.lower()


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


# ---------------------------------------------------------------------------
# grep_lines
# ---------------------------------------------------------------------------


def test_grep_lines_basic():
    lines = ["alpha", "beta", "gamma", "delta", "alpha again"]
    matches = grep_lines(lines, "alpha", 1)
    assert len(matches) == 2
    assert 1 in matches[0]["lines"]
    assert 5 in matches[1]["lines"]


def test_grep_lines_overlapping_context():
    lines = ["a", "b", "match1", "c", "match2", "d", "e"]
    matches = grep_lines(lines, "match", 1)
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
