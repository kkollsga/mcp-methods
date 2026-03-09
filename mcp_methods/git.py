"""GitHub API methods for MCP servers.

Provides ``git_api`` for generic REST API access and ``git_issue`` for
fetching issues/PRs with smart compaction and element caching.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
from pathlib import Path
from typing import Iterator

import requests

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

_GITHUB_API = "https://api.github.com"

# Reference patterns for link-following
_GITHUB_LINK_RE = re.compile(
    r"https?://github\.com/([a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+)/(?:issues|pull)/(\d+)"
)
_CROSS_REF_RE = re.compile(r"([a-zA-Z0-9_.-]+/[a-zA-Z0-9_.-]+)#(\d+)\b")
_SHORT_REF_RE = re.compile(r"(?<![a-zA-Z0-9/])#(\d+)\b")

# Discussion compaction constants
_MAINTAINER_ROLES = {"OWNER", "MEMBER", "COLLABORATOR"}
_COMMENT_PREVIEW_CHARS = 500
_CODE_BLOCK_MAX_LINES = 20
_CODE_BLOCK_KEEP = 5
_BODY_LIMIT = 10_000
_MAINTAINER_LIMIT = 5_000
_OVERFLOW_LIMIT = 50_000
_OVERFLOW_PREVIEW = 20_000

# ---------------------------------------------------------------------------
# Low-level GitHub helpers
# ---------------------------------------------------------------------------


def has_git_token() -> bool:
    """Return True if a GitHub token is available in the environment."""
    return bool(os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN"))


def _gh_headers() -> dict[str, str]:
    """Build GitHub API request headers with optional auth."""
    headers: dict[str, str] = {"Accept": "application/vnd.github+json"}
    token = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def _gh_get(
    endpoint: str, paginate: bool = False
) -> tuple[list | dict | None, str | None]:
    """GET from the GitHub API.  Returns ``(data, error_string)``.

    When *paginate* is True, follows ``Link`` headers and returns a flat list.
    """
    url = f"{_GITHUB_API}/{endpoint}"
    headers = _gh_headers()
    try:
        if not paginate:
            r = requests.get(url, headers=headers, timeout=30)
            if r.status_code == 404:
                return None, f"Not found: {endpoint}"
            if r.status_code == 403 and "rate limit" in r.text.lower():
                return None, (
                    "GitHub API rate limit exceeded. "
                    "Set GITHUB_TOKEN or GH_TOKEN env var for higher limits."
                )
            r.raise_for_status()
            return r.json(), None
        # Paginated fetch
        items: list = []
        while url:
            r = requests.get(url, headers=headers, timeout=30)
            if r.status_code == 403 and "rate limit" in r.text.lower():
                return None, (
                    "GitHub API rate limit exceeded. "
                    "Set GITHUB_TOKEN or GH_TOKEN env var for higher limits."
                )
            r.raise_for_status()
            items.extend(r.json())
            url = r.links.get("next", {}).get("url")
        return items, None
    except requests.RequestException as e:
        return None, f"GitHub API error: {e}"


def validate_repo(repo_name: str) -> str | None:
    """Validate ``org/repo`` format.  Returns an error string, or None if valid."""
    if "/" not in repo_name or repo_name.count("/") != 1:
        return "Invalid repo name. Use 'org/repo' format, e.g. 'pydata/xarray'."
    if not all(repo_name.split("/")):
        return "Invalid repo name. Use 'org/repo' format, e.g. 'pydata/xarray'."
    return None


def detect_git_repo(cwd: str | Path) -> str | None:
    """Auto-detect ``org/repo`` from the git remote in *cwd*."""
    try:
        result = subprocess.run(
            ["git", "remote", "get-url", "origin"],
            cwd=str(cwd),
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode != 0:
            return None
        url = result.stdout.strip()
        # SSH: git@github.com:org/repo.git
        m = re.match(r"git@github\.com:([^/]+/[^/]+?)(?:\.git)?$", url)
        if m:
            return m.group(1)
        # HTTPS: https://github.com/org/repo.git
        m = re.match(r"https?://github\.com/([^/]+/[^/]+?)(?:\.git)?$", url)
        if m:
            return m.group(1)
    except (subprocess.TimeoutExpired, OSError):
        pass
    return None


def extract_github_refs(text: str, default_repo: str) -> set[tuple[str, int]]:
    """Extract GitHub issue/PR references from *text*.

    Returns a set of ``(repo_name, number)`` tuples.
    """
    if not text:
        return set()
    refs: set[tuple[str, int]] = set()
    for m in _GITHUB_LINK_RE.finditer(text):
        refs.add((m.group(1), int(m.group(2))))
    for m in _CROSS_REF_RE.finditer(text):
        refs.add((m.group(1), int(m.group(2))))
    for m in _SHORT_REF_RE.finditer(text):
        refs.add((default_repo, int(m.group(1))))
    return refs


# ---------------------------------------------------------------------------
# Element cache
# ---------------------------------------------------------------------------


class ElementCache:
    """Stores collapsed elements from ``git_issue`` for later drill-down.

    Each MCP server should instantiate **one** cache and pass it to every
    ``git_issue`` call so collapsed code blocks, comments, and overflow
    previews can be retrieved by element ID.
    """

    def __init__(self) -> None:
        # {(repo, number): {element_id: element_data, ...}}
        self._store: dict[tuple[str, int], dict[str, dict]] = {}

    def get(self, repo: str, number: int, element_id: str) -> dict | None:
        return (self._store.get((repo, number)) or {}).get(element_id)

    def store(self, repo: str, number: int, elements: dict[str, dict]) -> None:
        self._store[(repo, number)] = elements

    def update(self, repo: str, number: int, elements: dict[str, dict]) -> None:
        self._store.setdefault((repo, number), {}).update(elements)

    def available(self, repo: str, number: int) -> list[str]:
        cache = self._store.get((repo, number))
        return sorted(cache.keys()) if cache else []


# ---------------------------------------------------------------------------
# Discussion helpers (internal)
# ---------------------------------------------------------------------------


def _iter_discussion_texts(result: dict) -> Iterator[str]:
    """Yield all text bodies from a discussion result dict."""
    if result.get("body"):
        yield result["body"]
    for c in result.get("comments", []):
        if c.get("body"):
            yield c["body"]
    for r in result.get("reviews", []):
        if r.get("body"):
            yield r["body"]
        for ic in r.get("inline_comments", []):
            if ic.get("body"):
                yield ic["body"]
            for rp in ic.get("replies", []):
                if rp.get("body"):
                    yield rp["body"]


def _collect_refs_from_discussion(
    result: dict, default_repo: str
) -> set[tuple[str, int]]:
    """Extract all GitHub issue/PR refs from all text fields and timeline events."""
    refs: set[tuple[str, int]] = set()
    for text in _iter_discussion_texts(result):
        refs |= extract_github_refs(text, default_repo)
    for ref in result.get("referenced_by", []):
        if ref.get("event") == "cross-reference" and ref.get("source_number"):
            refs.add((ref.get("source_repo", default_repo), ref["source_number"]))
    return refs


def _fetch_single_discussion(
    number: int,
    repo: str,
    include_files: bool = True,
    include_timeline: bool = True,
) -> tuple[dict | None, str | None]:
    """Fetch a single GitHub issue/PR as a dict.  Returns ``(result, error)``."""
    issue, err = _gh_get(f"repos/{repo}/issues/{number}")
    if err:
        return None, err

    is_pr = issue.get("pull_request") is not None
    user = issue.get("user") or {}

    result: dict = {
        "type": "pull_request" if is_pr else "issue",
        "number": number,
        "repo": repo,
        "title": issue.get("title", ""),
        "state": issue.get("state", ""),
        "author": user.get("login", "(deleted)"),
        "author_association": issue.get("author_association", ""),
        "created_at": issue.get("created_at", ""),
        "updated_at": issue.get("updated_at", ""),
        "url": issue.get("html_url", ""),
        "labels": [label["name"] for label in issue.get("labels", [])],
        "body": (issue.get("body") or "").strip() or None,
    }

    # Comments (works for both issues and PRs)
    comments, err = _gh_get(f"repos/{repo}/issues/{number}/comments", paginate=True)
    if err:
        comments = []
    result["comments"] = [
        {
            "author": (c.get("user") or {}).get("login", "(deleted)"),
            "author_association": c.get("author_association", ""),
            "created_at": c.get("created_at", ""),
            "body": (c.get("body") or "").strip() or None,
        }
        for c in comments
    ]

    # Timeline events: cross-references and commit references
    if include_timeline:
        timeline, t_err = _gh_get(
            f"repos/{repo}/issues/{number}/timeline", paginate=True
        )
        if not t_err and timeline:
            referenced_by: list[dict] = []
            for event in timeline:
                if not isinstance(event, dict):
                    continue
                etype = event.get("event")
                if etype == "cross-referenced":
                    source = (event.get("source") or {}).get("issue") or {}
                    if source.get("number"):
                        src_url = source.get("html_url", "")
                        src_repo = repo
                        m = re.match(
                            r"https://github\.com/([^/]+/[^/]+)/", src_url
                        )
                        if m:
                            src_repo = m.group(1)
                        referenced_by.append(
                            {
                                "event": "cross-reference",
                                "source_type": (
                                    "pull_request"
                                    if source.get("pull_request")
                                    else "issue"
                                ),
                                "source_number": source["number"],
                                "source_repo": src_repo,
                                "source_title": source.get("title", ""),
                                "author": (event.get("actor") or {}).get(
                                    "login", "(deleted)"
                                ),
                                "created_at": event.get("created_at", ""),
                            }
                        )
                elif etype == "referenced":
                    referenced_by.append(
                        {
                            "event": "commit-reference",
                            "commit_sha": (event.get("commit_id") or "")[:10],
                            "author": (event.get("actor") or {}).get(
                                "login", "(deleted)"
                            ),
                            "created_at": event.get("created_at", ""),
                        }
                    )
            if referenced_by:
                result["referenced_by"] = referenced_by

    # PR-specific data
    if is_pr:
        pr_data, err = _gh_get(f"repos/{repo}/pulls/{number}")
        if pr_data:
            result["merged"] = pr_data.get("merged", False)
            if result["merged"]:
                result["merged_by"] = (pr_data.get("merged_by") or {}).get("login")
                result["merged_at"] = pr_data.get("merged_at")
            result["base"] = pr_data.get("base", {}).get("ref", "")
            result["head"] = pr_data.get("head", {}).get("label", "")
            result["additions"] = pr_data.get("additions", 0)
            result["deletions"] = pr_data.get("deletions", 0)
            result["changed_files"] = pr_data.get("changed_files", 0)

        # Reviews
        reviews_raw, err = _gh_get(
            f"repos/{repo}/pulls/{number}/reviews", paginate=True
        )
        if not reviews_raw:
            reviews_raw = []

        # Inline review comments
        review_comments_raw, err = _gh_get(
            f"repos/{repo}/pulls/{number}/comments", paginate=True
        )
        if not review_comments_raw:
            review_comments_raw = []

        # Group inline comments by review ID, then thread replies
        by_review: dict[int | None, list] = {}
        reply_map: dict[int, list] = {}
        for rc in review_comments_raw:
            rid = rc.get("pull_request_review_id")
            if rc.get("in_reply_to_id"):
                reply_map.setdefault(rc["in_reply_to_id"], []).append(rc)
            else:
                by_review.setdefault(rid, []).append(rc)

        result["reviews"] = []
        for rev in reviews_raw:
            rev_body = (rev.get("body") or "").strip() or None
            rev_state = rev.get("state", "")
            rev_id = rev["id"]

            if rev_state == "COMMENTED" and not rev_body and rev_id not in by_review:
                continue

            review_obj: dict = {
                "author": (rev.get("user") or {}).get("login", "(deleted)"),
                "author_association": rev.get("author_association", ""),
                "state": rev_state,
                "submitted_at": rev.get("submitted_at", ""),
                "body": rev_body,
                "inline_comments": [],
            }

            for rc in by_review.get(rev_id, []):
                ic = {
                    "author": (rc.get("user") or {}).get("login", "(deleted)"),
                    "path": rc.get("path", ""),
                    "line": rc.get("line") or rc.get("original_line"),
                    "diff_hunk": rc.get("diff_hunk", ""),
                    "body": (rc.get("body") or "").strip() or None,
                    "created_at": rc.get("created_at", ""),
                    "replies": [
                        {
                            "author": (rp.get("user") or {}).get("login", "(deleted)"),
                            "created_at": rp.get("created_at", ""),
                            "body": (rp.get("body") or "").strip() or None,
                        }
                        for rp in reply_map.get(rc["id"], [])
                    ],
                }
                review_obj["inline_comments"].append(ic)

            result["reviews"].append(review_obj)

        # Orphan inline comments (not linked to a known review)
        known_review_ids = {rev["id"] for rev in reviews_raw}
        for rid, rcs in by_review.items():
            if rid in known_review_ids:
                continue
            for rc in rcs:
                result["reviews"].append(
                    {
                        "author": (rc.get("user") or {}).get("login", "(deleted)"),
                        "author_association": rc.get("author_association", ""),
                        "state": "COMMENTED",
                        "submitted_at": rc.get("created_at", ""),
                        "body": None,
                        "inline_comments": [
                            {
                                "author": (rc.get("user") or {}).get(
                                    "login", "(deleted)"
                                ),
                                "path": rc.get("path", ""),
                                "line": rc.get("line") or rc.get("original_line"),
                                "diff_hunk": rc.get("diff_hunk", ""),
                                "body": (rc.get("body") or "").strip() or None,
                                "created_at": rc.get("created_at", ""),
                                "replies": [
                                    {
                                        "author": (rp.get("user") or {}).get(
                                            "login", "(deleted)"
                                        ),
                                        "created_at": rp.get("created_at", ""),
                                        "body": (rp.get("body") or "").strip() or None,
                                    }
                                    for rp in reply_map.get(rc["id"], [])
                                ],
                            }
                        ],
                    }
                )

        # File changes with patches
        if include_files:
            files_raw, err = _gh_get(
                f"repos/{repo}/pulls/{number}/files", paginate=True
            )
            if not files_raw:
                files_raw = []
            result["files"] = [
                {
                    "filename": f.get("filename", ""),
                    "status": f.get("status", ""),
                    "additions": f.get("additions", 0),
                    "deletions": f.get("deletions", 0),
                    "patch": f.get("patch"),
                }
                for f in files_raw
            ]

    return result, None


# ---------------------------------------------------------------------------
# Compaction helpers
# ---------------------------------------------------------------------------


def _collapse_code_blocks(text: str, cache: dict | None = None) -> str:
    """Collapse large fenced code blocks and ``<details>`` sections.

    - Fenced blocks > 20 lines: keep first/last 5 lines, ``...`` in the middle.
    - ``<details>`` blocks: collapse to summary line only.
    - When *cache* is provided, collapsed elements are stored with IDs.
    """
    if not text:
        return text

    lines = text.split("\n")
    out: list[str] = []
    i = 0

    while i < len(lines):
        stripped = lines[i].strip().lower()

        # Collapse <details> blocks to summary
        if stripped.startswith("<details"):
            j = i + 1
            summary = ""
            while j < len(lines):
                s = lines[j].strip()
                if not summary and s.lower().startswith("<summary"):
                    summary = re.sub(r"</?summary[^>]*>", "", s).strip()
                if s.lower().startswith("</details"):
                    break
                j += 1
            hidden = j - i - 1
            if hidden > 3:
                label = summary or "collapsed section"
                if cache is not None:
                    cache["_n"] = cache.get("_n", 0) + 1
                    eid = f"details_{cache['_n']}"
                    content = "\n".join(lines[i + 1 : j])
                    cache[eid] = {
                        "type": "details",
                        "summary": label,
                        "total_lines": hidden,
                        "content": content,
                    }
                    out.append(f"[{label} — {hidden} lines hidden, id:{eid}]")
                else:
                    out.append(f"[{label} — {hidden} lines hidden]")
                i = min(j + 1, len(lines))
                continue

        # Collapse large fenced code blocks
        if stripped.startswith("```"):
            fence_line = lines[i]
            j = i + 1
            while j < len(lines) and not lines[j].strip().startswith("```"):
                j += 1
            has_close = j < len(lines)
            end = j + 1 if has_close else j
            inner = end - i - (2 if has_close else 1)

            if inner > _CODE_BLOCK_MAX_LINES:
                hidden = inner - 2 * _CODE_BLOCK_KEEP

                if cache is not None:
                    cache["_n"] = cache.get("_n", 0) + 1
                    eid = f"cb_{cache['_n']}"
                    lang_m = re.match(r"```(\w*)", fence_line.strip())
                    lang = lang_m.group(1) if lang_m else ""
                    content_end = j if has_close else end
                    cache[eid] = {
                        "type": "code_block",
                        "language": lang,
                        "total_lines": inner,
                        "content": "\n".join(lines[i + 1 : content_end]),
                    }
                    out.append(f"{fence_line} [id:{eid}, {inner} lines]")
                else:
                    out.append(fence_line)

                out.extend(lines[i + 1 : i + 1 + _CODE_BLOCK_KEEP])
                out.append(f"  ... ({hidden} lines hidden)")
                if has_close:
                    out.extend(lines[j - _CODE_BLOCK_KEEP : j])
                    out.append(lines[j])
                else:
                    out.extend(lines[end - _CODE_BLOCK_KEEP : end])
            else:
                out.extend(lines[i:end])
            i = end
            continue

        out.append(lines[i])
        i += 1

    return "\n".join(out)


def _compact_text(
    text: str | None, limit: int, cache: dict | None = None
) -> tuple[str, bool]:
    """Collapse code blocks, then truncate if still over *limit*."""
    if not text:
        return text or "", False
    text = _collapse_code_blocks(text, cache)
    if len(text) > limit:
        return text[:limit] + "\n…[truncated]", True
    return text, False


def _compact_discussion(
    result: dict, expand: set[str], cache: dict | None = None
) -> dict:
    """Trim a discussion result for compact output.

    *expand* controls which sections stay in full:

    - ``"body"`` — keep the opening body untruncated
    - ``"comments"`` — keep all comments untruncated, include bots
    - ``"patches"`` — keep file patches
    - ``"review:<author>"`` — keep full inline comments for that reviewer
    - ``"all"`` — skip compaction entirely
    """
    if "all" in expand:
        return result

    # Collapse code blocks in the opening body
    if "body" not in expand and result.get("body"):
        text, truncated = _compact_text(result["body"], _BODY_LIMIT, cache)
        result["body"] = text
        if truncated:
            result["_body_truncated"] = True

    # Filter bot comments
    if "comments" in result and "comments" not in expand:
        original = result["comments"]
        filtered = [c for c in original if not c.get("author", "").endswith("[bot]")]
        bot_count = len(original) - len(filtered)
        result["comments"] = filtered
        if bot_count:
            result["_bot_comments_hidden"] = bot_count

    # Collapse code blocks and truncate comments
    if "comments" in result and "comments" not in expand:
        for c in result["comments"]:
            is_maintainer = c.get("author_association") in _MAINTAINER_ROLES
            limit = _MAINTAINER_LIMIT if is_maintainer else _COMMENT_PREVIEW_CHARS
            original_body = c.get("body") or ""
            body, truncated = _compact_text(c.get("body"), limit, cache)
            c["body"] = body
            if truncated:
                c["_truncated"] = True
                if cache is not None:
                    cache["_n"] = cache.get("_n", 0) + 1
                    eid = f"comment_{cache['_n']}"
                    cache[eid] = {
                        "type": "comment",
                        "author": c.get("author", ""),
                        "total_lines": original_body.count("\n") + 1,
                        "content": original_body,
                    }
                    c["_element_id"] = eid

    # Strip patches from files
    if "files" in result and "patches" not in expand:
        for f in result["files"]:
            f.pop("patch", None)

    # Compact inline review comments
    if "reviews" in result:
        for review in result["reviews"]:
            reviewer = review.get("author", "")
            if f"review:{reviewer}" in expand:
                continue
            inlines = review.get("inline_comments", [])
            if inlines:
                review["inline_comments"] = [
                    {
                        "path": ic.get("path", ""),
                        "line": ic.get("line"),
                        "preview": (ic.get("body") or "").split("\n")[0][:120],
                        "replies": len(ic.get("replies", [])),
                    }
                    for ic in inlines
                ]

    # Add expand hints
    hints: list[str] = []
    if result.get("_body_truncated"):
        hints.append("body")
    if any(c.get("_truncated") for c in result.get("comments", [])):
        hints.append("comments")
    elif result.get("_bot_comments_hidden"):
        hints.append("comments")
    if result.get("files"):
        hints.append("patches")
    reviewers = [
        r["author"]
        for r in result.get("reviews", [])
        if r.get("inline_comments")
    ]
    hints.extend(f"review:{r}" for r in reviewers)
    if hints:
        result["_expand"] = (
            f"Compact view. expand=[{', '.join(repr(h) for h in hints)}] "
            f"for full content, or expand=['all']."
        )

    return result


def _grep_lines(
    text_lines: list[str], pattern: re.Pattern, context: int
) -> list[dict]:
    """Grep through lines with context, merging overlapping windows."""
    raw: list[tuple[int, int, int]] = []
    for idx, line in enumerate(text_lines):
        if pattern.search(line):
            start = max(0, idx - context)
            end = min(len(text_lines), idx + context + 1)
            raw.append((idx + 1, start, end))
    groups: list[dict] = []
    for hit_line, start, end in raw:
        if groups and start <= groups[-1]["_end"]:
            groups[-1]["lines"].append(hit_line)
            groups[-1]["_end"] = max(groups[-1]["_end"], end)
        else:
            groups.append({"lines": [hit_line], "_start": start, "_end": end})
    for g in groups:
        g["context_start"] = g.pop("_start") + 1
        g["context_end"] = g.pop("_end")
        g["content"] = "\n".join(text_lines[g["context_start"] - 1 : g["context_end"]])
    return groups


def _grep_json_fields(
    data: object, pattern: re.Pattern, context: int, path: str = ""
) -> list[dict]:
    """Walk a parsed JSON structure, grep within unescaped string values."""
    matches: list[dict] = []
    if isinstance(data, str):
        lines = data.replace("\r\n", "\n").split("\n")
        for m in _grep_lines(lines, pattern, context):
            m["field"] = path
            matches.append(m)
    elif isinstance(data, dict):
        for key, val in data.items():
            child = f"{path}.{key}" if path else key
            matches.extend(_grep_json_fields(val, pattern, context, child))
    elif isinstance(data, list):
        for i, item in enumerate(data):
            matches.extend(_grep_json_fields(item, pattern, context, f"{path}[{i}]"))
    return matches


def _retrieve_element(
    cache: ElementCache,
    repo: str,
    number: int,
    element_id: str,
    lines: str | None,
    grep: str | None,
    context: int,
) -> str:
    """Look up a cached collapsed element and return its content."""
    elem_data = cache.get(repo, number, element_id)
    if elem_data is None:
        available = cache.available(repo, number)
        msg = f"Element '{element_id}' not found for {repo}#{number}."
        if available:
            msg += f"\nAvailable: {', '.join(available)}"
        else:
            msg += "\nNo cached elements. Call git_issue first."
        return msg

    elem = elem_data.copy()
    content = elem["content"]
    content_lines = content.split("\n")

    if grep:
        try:
            pattern = re.compile(grep)
        except re.error as e:
            return f"Invalid grep pattern: {e}"

        # Overflow elements: field-aware grep through parsed JSON values
        if elem.get("type") == "overflow":
            try:
                data = json.loads(content)
            except (json.JSONDecodeError, ValueError):
                data = None
            if data is not None:
                matches = _grep_json_fields(data, pattern, context)
                return json.dumps(
                    {
                        "element_id": element_id,
                        "type": "overflow",
                        "grep": grep,
                        "matches": matches,
                    },
                    indent=2,
                    ensure_ascii=False,
                )

        # Standard elements: line-based grep on raw content
        matches = _grep_lines(content_lines, pattern, context)
        elem.pop("content", None)
        elem["grep"] = grep
        elem["matches"] = matches
        return json.dumps(elem, indent=2, ensure_ascii=False)

    if lines:
        m = re.match(r"(\d+)-(\d+)$", lines)
        if not m:
            return f"Invalid lines format: '{lines}'. Use 'start-end', e.g. '40-60'."
        start, end = int(m.group(1)), int(m.group(2))
        selected = content_lines[max(0, start - 1) : end]
        elem["content"] = "\n".join(selected)
        elem["lines_shown"] = f"{start}-{min(end, len(content_lines))}"
        return json.dumps(elem, indent=2, ensure_ascii=False)

    return json.dumps(elem, indent=2, ensure_ascii=False)


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def git_api(repo: str, path: str, *, truncate_at: int = 80_000) -> str:
    """Read-only GET against any GitHub REST API endpoint.  Returns JSON.

    *repo*: GitHub ``org/repo`` (e.g. ``"pydata/xarray"``).
    *path*: API path, e.g. ``"pulls?state=open"``, ``"commits/abc123"``,
        ``"compare/main...dev"``, or a top-level path like
        ``"search/issues?q=quickjs+repo:org/repo+type:pr"``.

    Paths that don't start with a top-level API resource are prefixed with
    ``/repos/{repo}/`` automatically.  Read-only: only HTTP GET is used.
    """
    err = validate_repo(repo)
    if err:
        return err

    # Top-level API resources that should NOT be prefixed with /repos/{repo}
    top_level = (
        "search/",
        "users/",
        "orgs/",
        "gists/",
        "rate_limit",
        "repos/",
    )
    if any(path.startswith(p) for p in top_level):
        url = f"{_GITHUB_API}/{path}"
    else:
        url = f"{_GITHUB_API}/repos/{repo}/{path}"

    headers = _gh_headers()
    try:
        r = requests.get(url, headers=headers, timeout=30)
        if r.status_code == 404:
            return f"Not found: {url}"
        if r.status_code == 403 and "rate limit" in r.text.lower():
            return (
                "GitHub API rate limit exceeded. "
                "Set GITHUB_TOKEN or GH_TOKEN for higher limits."
            )
        r.raise_for_status()
        data = r.json()
        text = json.dumps(data, indent=2, ensure_ascii=False)
        if len(text) > truncate_at:
            text = text[:truncate_at] + "\n\n... (truncated, refine your query)"
        return text
    except requests.RequestException as e:
        return f"GitHub API error: {e}"


def git_issue(
    repo: str,
    number: int,
    *,
    expand: list[str] | None = None,
    element_id: str | None = None,
    lines: str | None = None,
    grep: str | None = None,
    context: int = 3,
    cache: ElementCache | None = None,
) -> str:
    """Fetch a GitHub issue or PR conversation as JSON.

    *repo*: GitHub ``org/repo`` (e.g. ``"pydata/xarray"``).
    *number*: the issue or PR number (e.g. 11124).
    *expand*: sections to include in full (default: compact view).  Options:

        - ``"body"`` — full opening body
        - ``"comments"`` — full text of all comments, including bots
        - ``"patches"`` — include file diffs for PRs
        - ``"review:<author>"`` — full inline comments from a specific reviewer
        - ``"all"`` — everything, no compaction

    *element_id*: retrieve a specific collapsed element by ID (e.g. ``"cb_1"``).
    *lines*: line range to extract from element, e.g. ``"40-60"``.
    *grep*: regex search within element content.
    *context*: lines of context around grep matches (default 3).
    *cache*: an :class:`ElementCache` instance for storing/retrieving collapsed
        elements across calls.
    """
    err = validate_repo(repo)
    if err:
        return err

    # Element retrieval mode
    if element_id is not None:
        if cache is None:
            return "element_id requires a cache. No ElementCache was provided."
        return _retrieve_element(cache, repo, number, element_id, lines, grep, context)

    # Check for GitHub token
    if not has_git_token():
        return (
            "No GitHub token found. A token is required for fetching issues/PRs "
            "(cross-references, higher rate limits).\n\n"
            "Set the GITHUB_TOKEN or GH_TOKEN environment variable, or use "
            "load_env() to load it from a .env file.\n\n"
            "The token needs no special scopes — a classic PAT with default (no) "
            "permissions works for public repos."
        )

    expand_set = set(expand) if expand else set()

    # Fetch parent discussion (with file changes)
    parent, err = _fetch_single_discussion(number, repo)
    if err:
        return err

    # Collect GitHub refs mentioned in the discussion
    MAX_RELATED = 10
    seen = {(repo, number)}
    refs = sorted(_collect_refs_from_discussion(parent, repo) - seen)[:MAX_RELATED]

    if refs:
        parent_size = len(json.dumps(parent, ensure_ascii=False))

        if parent_size < 30_000 and len(refs) <= 5:
            # Small discussion — include full related conversations (always compact)
            related: list[dict] = []
            for ref_repo, ref_num in refs:
                disc, _err = _fetch_single_discussion(
                    ref_num, ref_repo, include_files=False, include_timeline=False
                )
                if disc:
                    related.append(_compact_discussion(disc, set()))
            if related:
                parent["related_discussions"] = related
        else:
            # Large discussion — list refs as summaries only
            summaries: list[dict] = []
            for ref_repo, ref_num in refs:
                issue_data, _err = _gh_get(f"repos/{ref_repo}/issues/{ref_num}")
                if issue_data:
                    summaries.append(
                        {
                            "type": (
                                "pull_request"
                                if issue_data.get("pull_request")
                                else "issue"
                            ),
                            "number": ref_num,
                            "repo": ref_repo,
                            "title": issue_data.get("title", ""),
                            "state": issue_data.get("state", ""),
                            "author": (issue_data.get("user") or {}).get(
                                "login", "(deleted)"
                            ),
                        }
                    )
            if summaries:
                parent["related_discussions"] = summaries
                parent["_note"] = (
                    "Related discussions shown as summaries. "
                    "Call git_issue(repo, number) to read any in full."
                )

    # Apply compaction with element caching
    inner_cache: dict = {"_n": 0}
    _compact_discussion(parent, expand_set, cache=inner_cache)

    # Store element cache for later retrieval
    elements = {k: v for k, v in inner_cache.items() if not k.startswith("_")}
    if elements and cache is not None:
        cache.store(repo, number, elements)

    text = json.dumps(parent, indent=2, ensure_ascii=False)

    # Overflow guard — cache full result as element, return truncated preview
    if len(text) > _OVERFLOW_LIMIT:
        total_lines = text.count("\n") + 1
        if cache is not None:
            cache.update(
                repo,
                number,
                {
                    "overflow": {
                        "type": "overflow",
                        "total_chars": len(text),
                        "total_lines": total_lines,
                        "content": text,
                    }
                },
            )
        preview = text[:_OVERFLOW_PREVIEW]
        last_nl = preview.rfind("\n")
        if last_nl > 0:
            preview = preview[:last_nl]
        preview += (
            f"\n\n... [{len(text):,} chars, {total_lines} lines — truncated]\n"
            f"Use element_id='overflow' with lines='N-M' or grep='pattern' "
            f"to explore the full result."
        )
        return preview

    return text
