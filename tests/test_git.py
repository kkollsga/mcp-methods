"""Tests for mcp_methods.git."""

import json
import os
from unittest.mock import MagicMock, patch

from mcp_methods.git import (
    ElementCache,
    _collapse_code_blocks,
    _compact_discussion,
    _grep_lines,
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
    assert validate_repo("pydata/xarray") is None
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
    assert extract_github_refs("", "repo/x") == set()
    assert extract_github_refs("no refs here", "repo/x") == set()


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
# detect_git_repo
# ---------------------------------------------------------------------------


def test_detect_git_repo_ssh():
    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = "git@github.com:org/repo.git\n"

    with patch("mcp_methods.git.subprocess.run", return_value=mock_result):
        assert detect_git_repo("/some/path") == "org/repo"


def test_detect_git_repo_https():
    mock_result = MagicMock()
    mock_result.returncode = 0
    mock_result.stdout = "https://github.com/org/repo.git\n"

    with patch("mcp_methods.git.subprocess.run", return_value=mock_result):
        assert detect_git_repo("/some/path") == "org/repo"


def test_detect_git_repo_not_git():
    mock_result = MagicMock()
    mock_result.returncode = 128

    with patch("mcp_methods.git.subprocess.run", return_value=mock_result):
        assert detect_git_repo("/some/path") is None


# ---------------------------------------------------------------------------
# ElementCache
# ---------------------------------------------------------------------------


def test_element_cache_store_and_get():
    cache = ElementCache()
    cache.store("org/repo", 1, {"cb_1": {"type": "code_block", "content": "x"}})
    assert cache.get("org/repo", 1, "cb_1")["content"] == "x"
    assert cache.get("org/repo", 1, "missing") is None


def test_element_cache_available():
    cache = ElementCache()
    cache.store("org/repo", 1, {"cb_1": {}, "comment_1": {}})
    assert cache.available("org/repo", 1) == ["cb_1", "comment_1"]
    assert cache.available("org/repo", 999) == []


def test_element_cache_update():
    cache = ElementCache()
    cache.store("org/repo", 1, {"cb_1": {"content": "a"}})
    cache.update("org/repo", 1, {"overflow": {"content": "b"}})
    assert cache.get("org/repo", 1, "cb_1") is not None
    assert cache.get("org/repo", 1, "overflow")["content"] == "b"


# ---------------------------------------------------------------------------
# _collapse_code_blocks
# ---------------------------------------------------------------------------


def test_collapse_code_blocks_small_block():
    text = "```python\nline1\nline2\n```"
    assert _collapse_code_blocks(text) == text


def test_collapse_code_blocks_large_block():
    inner = "\n".join(f"line{i}" for i in range(30))
    text = f"```python\n{inner}\n```"
    result = _collapse_code_blocks(text)
    assert "... (" in result
    assert "lines hidden" in result


def test_collapse_code_blocks_caches_elements():
    inner = "\n".join(f"line{i}" for i in range(30))
    text = f"```python\n{inner}\n```"
    cache: dict = {"_n": 0}
    _collapse_code_blocks(text, cache)
    assert "cb_1" in cache
    assert cache["cb_1"]["type"] == "code_block"
    assert cache["cb_1"]["language"] == "python"


# ---------------------------------------------------------------------------
# _compact_discussion
# ---------------------------------------------------------------------------


def test_compact_discussion_filters_bots():
    result = {
        "body": "test",
        "comments": [
            {"author": "real-user", "author_association": "MEMBER", "body": "hi"},
            {"author": "dependabot[bot]", "author_association": "NONE", "body": "bump"},
        ],
    }
    _compact_discussion(result, set())
    assert len(result["comments"]) == 1
    assert result["comments"][0]["author"] == "real-user"
    assert result["_bot_comments_hidden"] == 1


def test_compact_discussion_expand_all():
    result = {
        "body": "test" * 5000,
        "comments": [
            {"author": "bot[bot]", "body": "bump"},
        ],
    }
    _compact_discussion(result, {"all"})
    assert len(result["comments"]) == 1  # bot kept


# ---------------------------------------------------------------------------
# _grep_lines
# ---------------------------------------------------------------------------


def test_grep_lines_basic():
    import re

    lines = ["alpha", "beta", "gamma", "delta", "alpha again"]
    pattern = re.compile("alpha")
    matches = _grep_lines(lines, pattern, context=1)
    assert len(matches) == 2
    assert 1 in matches[0]["lines"]
    assert 5 in matches[1]["lines"]


def test_grep_lines_overlapping_context():
    import re

    lines = ["a", "b", "match1", "c", "match2", "d", "e"]
    pattern = re.compile("match")
    matches = _grep_lines(lines, pattern, context=1)
    # The two matches are close enough that their context windows merge
    assert len(matches) == 1
    assert 3 in matches[0]["lines"]
    assert 5 in matches[0]["lines"]


# ---------------------------------------------------------------------------
# git_api (mocked)
# ---------------------------------------------------------------------------


def test_git_api_invalid_repo():
    result = git_api("bad-repo", "pulls")
    assert "Invalid repo" in result


def test_git_api_success():
    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_response.json.return_value = [{"number": 1, "title": "Test PR"}]

    with patch("mcp_methods.git.requests.get", return_value=mock_response):
        result = git_api("org/repo", "pulls?state=open")
        data = json.loads(result)
        assert data[0]["number"] == 1


def test_git_api_top_level_path():
    mock_response = MagicMock()
    mock_response.status_code = 200
    mock_response.json.return_value = {"items": []}

    with patch("mcp_methods.git.requests.get", return_value=mock_response) as mock_get:
        git_api("org/repo", "search/issues?q=test")
        called_url = mock_get.call_args[0][0]
        assert "/repos/" not in called_url
        assert "search/issues" in called_url


def test_git_api_404():
    mock_response = MagicMock()
    mock_response.status_code = 404

    with patch("mcp_methods.git.requests.get", return_value=mock_response):
        result = git_api("org/repo", "nonexistent")
        assert "Not found" in result
