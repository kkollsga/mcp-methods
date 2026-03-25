use pyo3::prelude::*;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::env;
use std::sync::LazyLock;
use std::time::Duration;

use crate::compact;
use crate::git_refs;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const GITHUB_API: &str = "https://api.github.com";
pub const OVERFLOW_LIMIT: usize = 100_000;
pub const OVERFLOW_PREVIEW: usize = 40_000;
const MAX_RELATED: usize = 10;
/// Comment pages: first 5 + last 5 (30 per page → 300 comments max, skipping middle).
const COMMENT_HEAD_PAGES: usize = 5;
const COMMENT_TAIL_PAGES: usize = 5;
/// Timeline pages: first 3 + last 2 (enough for cross-refs + recent activity).
const TIMELINE_HEAD_PAGES: usize = 3;
const TIMELINE_TAIL_PAGES: usize = 2;

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https://github\.com/([^/]+/[^/]+)/").unwrap());

static GIT_SSH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^git@github\.com:([^/]+/[^/]+?)(?:\.git)?$").unwrap());

static GIT_HTTPS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^https?://github\.com/([^/]+/[^/]+?)(?:\.git)?$").unwrap());

/// Shared HTTP agent with connection pooling (keep-alive).
static AGENT: LazyLock<ureq::Agent> = LazyLock::new(|| {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
});

/// Rough byte-size estimate for a serde_json::Value without allocating a string.
pub fn estimate_json_size(val: &Value) -> usize {
    match val {
        Value::Null => 4,
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Number(n) => {
            // Rough: number of digits
            let s = n.to_string();
            s.len()
        }
        Value::String(s) => s.len() + 2, // quotes
        Value::Array(arr) => 2 + arr.iter().map(|v| estimate_json_size(v) + 1).sum::<usize>(),
        Value::Object(map) => {
            2 + map
                .iter()
                .map(|(k, v)| k.len() + 3 + estimate_json_size(v) + 1)
                .sum::<usize>()
        }
    }
}

// ---------------------------------------------------------------------------
// Token / auth
// ---------------------------------------------------------------------------

fn auth_token() -> Option<String> {
    env::var("GITHUB_TOKEN")
        .or_else(|_| env::var("GH_TOKEN"))
        .ok()
}

/// Check if a GitHub token is available in the environment.
#[pyfunction]
pub fn has_git_token() -> bool {
    auth_token().is_some()
}

/// Auto-detect `org/repo` from the git remote in *cwd*.
#[pyfunction]
pub fn detect_git_repo(cwd: &str) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if let Some(cap) = GIT_SSH_RE.captures(&url) {
        return Some(cap[1].to_string());
    }
    if let Some(cap) = GIT_HTTPS_RE.captures(&url) {
        return Some(cap[1].to_string());
    }
    None
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

fn gh_get(endpoint: &str) -> Result<Value, String> {
    let url = if endpoint.starts_with("http") {
        endpoint.to_string()
    } else {
        format!("{}/{}", GITHUB_API, endpoint)
    };

    let mut req = AGENT
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "mcp-methods");

    if let Some(token) = auth_token() {
        req = req.set("Authorization", &format!("Bearer {}", token));
    }

    match req.call() {
        Ok(resp) => resp
            .into_json::<Value>()
            .map_err(|e| format!("JSON parse error: {}", e)),
        Err(ureq::Error::Status(404, _)) => Err(format!("Not found: {}", endpoint)),
        Err(ureq::Error::Status(403, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            if body.to_lowercase().contains("rate limit") {
                Err(
                    "GitHub API rate limit exceeded. Set GITHUB_TOKEN or GH_TOKEN env var for higher limits."
                        .into(),
                )
            } else {
                Err(format!("GitHub API forbidden: {}", body))
            }
        }
        Err(ureq::Error::Status(code, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            Err(format!("GitHub API error ({}): {}", code, body))
        }
        Err(e) => Err(format!("GitHub API error: {}", e)),
    }
}

fn gh_graphql(query: &str, variables: Value) -> Result<Value, String> {
    let token = auth_token().ok_or(
        "GitHub token required for Discussions (GraphQL API). \
         Set GITHUB_TOKEN or GH_TOKEN.",
    )?;

    let body = json!({
        "query": query,
        "variables": variables,
    });

    let resp = AGENT
        .post("https://api.github.com/graphql")
        .set("Authorization", &format!("Bearer {}", token))
        .set("User-Agent", "mcp-methods")
        .send_json(&body)
        .map_err(|e| match e {
            ureq::Error::Status(401, _) => {
                "GitHub token is invalid or expired. Check GITHUB_TOKEN / GH_TOKEN.".to_string()
            }
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                format!("GitHub GraphQL error ({}): {}", code, body)
            }
            other => format!("GitHub GraphQL error: {}", other),
        })?;

    let result: Value = resp
        .into_json()
        .map_err(|e| format!("GraphQL JSON parse error: {}", e))?;

    // GraphQL returns errors in {"errors": [...]} even on HTTP 200
    if let Some(errors) = result.get("errors").and_then(|v| v.as_array()) {
        if let Some(first) = errors.first() {
            let msg = first
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("Unknown GraphQL error");
            return Err(format!("GitHub GraphQL error: {}", msg));
        }
    }

    result
        .get("data")
        .cloned()
        .ok_or_else(|| "GitHub GraphQL: no 'data' in response".to_string())
}

fn parse_link_rel(link: &str, rel: &str) -> Option<String> {
    let tag = format!("rel=\"{}\"", rel);
    for part in link.split(',') {
        if part.contains(&tag) {
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}

fn parse_link_next(link: &str) -> Option<String> {
    parse_link_rel(link, "next")
}

/// Extract the last page number from a Link header URL (?page=N or &page=N).
fn parse_last_page(link: &str) -> Option<usize> {
    let url = parse_link_rel(link, "last")?;
    // Find page=N in query string
    url.split('?').nth(1)?.split('&').find_map(|param| {
        let (k, v) = param.split_once('=')?;
        if k == "page" {
            v.parse().ok()
        } else {
            None
        }
    })
}

fn gh_get_paginated(endpoint: &str) -> Result<Vec<Value>, String> {
    gh_get_paginated_bookends(endpoint, 0, 0)
}

/// Fetch a single page by URL (used for tail pages).
fn gh_get_page(url: &str) -> Result<Vec<Value>, String> {
    let mut req = AGENT
        .get(url)
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "mcp-methods");
    if let Some(token) = auth_token() {
        req = req.set("Authorization", &format!("Bearer {}", token));
    }
    let resp = req.call().map_err(|e| format!("GitHub API error: {}", e))?;
    let items: Value = resp
        .into_json()
        .map_err(|e| format!("JSON parse error: {}", e))?;
    match items {
        Value::Array(arr) => Ok(arr),
        _ => Ok(vec![]),
    }
}

/// Fetch paginated results: first `head` pages + last `tail` pages.
/// If head=0 and tail=0, fetch all pages (unlimited).
/// When total pages <= head+tail, all pages are fetched (no gap).
fn gh_get_paginated_bookends(
    endpoint: &str,
    head: usize,
    tail: usize,
) -> Result<Vec<Value>, String> {
    let mut url = format!("{}/{}", GITHUB_API, endpoint);
    let mut all_items: Vec<Value> = Vec::new();
    let mut pages_fetched: usize = 0;
    let unlimited = head == 0 && tail == 0;
    let max_head = if unlimited { usize::MAX } else { head };
    let mut last_page: Option<usize> = None;
    let mut skipped = false;

    loop {
        let mut req = AGENT
            .get(&url)
            .set("Accept", "application/vnd.github+json")
            .set("User-Agent", "mcp-methods");

        if let Some(token) = auth_token() {
            req = req.set("Authorization", &format!("Bearer {}", token));
        }

        let resp = match req.call() {
            Ok(r) => r,
            Err(ureq::Error::Status(403, resp)) => {
                let body = resp.into_string().unwrap_or_default();
                if body.to_lowercase().contains("rate limit") {
                    return Err(
                        "GitHub API rate limit exceeded. Set GITHUB_TOKEN or GH_TOKEN env var for higher limits."
                            .into(),
                    );
                }
                return Err(format!("GitHub API forbidden: {}", body));
            }
            Err(e) => return Err(format!("GitHub API error: {}", e)),
        };

        let link_header: Option<String> = resp.header("link").map(String::from);
        let items: Value = resp
            .into_json()
            .map_err(|e| format!("JSON parse error: {}", e))?;

        if let Value::Array(arr) = items {
            all_items.extend(arr);
        }

        pages_fetched += 1;

        // On first page, discover total page count
        if pages_fetched == 1 && last_page.is_none() {
            last_page = link_header.as_deref().and_then(parse_last_page);
        }

        if pages_fetched >= max_head {
            break;
        }

        match link_header.as_deref().and_then(parse_link_next) {
            Some(u) => url = u,
            None => break,
        }
    }

    // Fetch tail pages if there's a gap
    if !unlimited && tail > 0 {
        if let Some(total) = last_page {
            let tail_start = (head + 1).max(total.saturating_sub(tail) + 1);
            if tail_start <= total {
                skipped = tail_start > head + 1;
                // Build URLs for tail pages
                let base = format!("{}/{}", GITHUB_API, endpoint);
                let sep = if base.contains('?') { '&' } else { '?' };
                for page_num in tail_start..=total {
                    let page_url = format!("{}{}page={}", base, sep, page_num);
                    if let Ok(items) = gh_get_page(&page_url) {
                        all_items.extend(items);
                    }
                }
            }
        }
    }

    if skipped {
        // Insert a marker so callers know comments were skipped
        all_items.push(json!({"_skipped_middle": true}));
    }

    Ok(all_items)
}

// ---------------------------------------------------------------------------
// Discussion assembly helpers
// ---------------------------------------------------------------------------

fn json_str(val: &Value, key: &str) -> String {
    val.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn json_author(val: &Value) -> String {
    val.get("user")
        .and_then(|u| u.get("login"))
        .and_then(|v| v.as_str())
        .unwrap_or("(deleted)")
        .to_string()
}

fn json_body(val: &Value) -> Value {
    match val.get("body").and_then(|v| v.as_str()) {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Value::Null
            } else {
                Value::String(trimmed.to_string())
            }
        }
        None => Value::Null,
    }
}

fn parse_timeline(timeline: &[Value], repo: &str) -> Vec<Value> {
    let mut referenced_by = Vec::new();
    for event in timeline {
        let etype = event.get("event").and_then(|v| v.as_str()).unwrap_or("");
        match etype {
            "cross-referenced" => {
                let source = event
                    .get("source")
                    .and_then(|s| s.get("issue"))
                    .unwrap_or(&Value::Null);
                if let Some(source_number) = source.get("number").and_then(|v| v.as_u64()) {
                    let src_url = source
                        .get("html_url")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let src_repo = URL_RE
                        .captures(src_url)
                        .map(|c| c[1].to_string())
                        .unwrap_or_else(|| repo.to_string());
                    let is_pr = source.get("pull_request").is_some();
                    referenced_by.push(json!({
                        "event": "cross-reference",
                        "source_type": if is_pr { "pull_request" } else { "issue" },
                        "source_number": source_number,
                        "source_repo": src_repo,
                        "source_title": json_str(source, "title"),
                        "author": event.get("actor")
                            .and_then(|a| a.get("login"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("(deleted)"),
                        "created_at": json_str(event, "created_at"),
                    }));
                }
            }
            "referenced" => {
                let sha = json_str(event, "commit_id");
                referenced_by.push(json!({
                    "event": "commit-reference",
                    "commit_sha": &sha[..sha.len().min(10)],
                    "author": event.get("actor")
                        .and_then(|a| a.get("login"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("(deleted)"),
                    "created_at": json_str(event, "created_at"),
                }));
            }
            _ => {}
        }
    }
    referenced_by
}

fn build_inline_comment(rc: &Value, reply_map: &HashMap<u64, Vec<&Value>>) -> Value {
    let rc_id = rc.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let replies: Vec<Value> = reply_map
        .get(&rc_id)
        .map(|rps| {
            rps.iter()
                .map(|rp| {
                    json!({
                        "author": json_author(rp),
                        "created_at": json_str(rp, "created_at"),
                        "body": json_body(rp),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    json!({
        "author": json_author(rc),
        "path": json_str(rc, "path"),
        "line": rc.get("line").or_else(|| rc.get("original_line")).cloned().unwrap_or(Value::Null),
        "diff_hunk": json_str(rc, "diff_hunk"),
        "body": json_body(rc),
        "created_at": json_str(rc, "created_at"),
        "replies": replies,
    })
}

fn build_reviews(reviews_raw: &[Value], review_comments_raw: &[Value]) -> Vec<Value> {
    let mut by_review: HashMap<Option<u64>, Vec<&Value>> = HashMap::new();
    let mut reply_map: HashMap<u64, Vec<&Value>> = HashMap::new();

    for rc in review_comments_raw {
        let rid = rc.get("pull_request_review_id").and_then(|v| v.as_u64());
        if rc.get("in_reply_to_id").and_then(|v| v.as_u64()).is_some() {
            let reply_to = rc["in_reply_to_id"].as_u64().unwrap();
            reply_map.entry(reply_to).or_default().push(rc);
        } else {
            by_review.entry(rid).or_default().push(rc);
        }
    }

    let mut reviews = Vec::new();
    let mut known_review_ids = HashSet::new();

    for rev in reviews_raw {
        let rev_id = rev.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
        known_review_ids.insert(rev_id);

        let rev_body = json_body(rev);
        let rev_state = json_str(rev, "state");

        if rev_state == "COMMENTED" && rev_body.is_null() && !by_review.contains_key(&Some(rev_id))
        {
            continue;
        }

        let inlines: Vec<Value> = by_review
            .get(&Some(rev_id))
            .map(|rcs| {
                rcs.iter()
                    .map(|rc| build_inline_comment(rc, &reply_map))
                    .collect()
            })
            .unwrap_or_default();

        reviews.push(json!({
            "author": json_author(rev),
            "author_association": json_str(rev, "author_association"),
            "state": rev_state,
            "submitted_at": json_str(rev, "submitted_at"),
            "body": rev_body,
            "inline_comments": inlines,
        }));
    }

    // Orphan inline comments (not linked to a known review)
    for (rid, rcs) in &by_review {
        if let Some(id) = rid {
            if known_review_ids.contains(id) {
                continue;
            }
        }
        for rc in rcs {
            reviews.push(json!({
                "author": json_author(rc),
                "author_association": json_str(rc, "author_association"),
                "state": "COMMENTED",
                "submitted_at": json_str(rc, "created_at"),
                "body": Value::Null,
                "inline_comments": vec![build_inline_comment(rc, &reply_map)],
            }));
        }
    }

    reviews
}

// ---------------------------------------------------------------------------
// GitHub Discussions (GraphQL)
// ---------------------------------------------------------------------------

const DISCUSSION_QUERY: &str = r#"query($owner: String!, $repo: String!, $number: Int!) {
  repository(owner: $owner, name: $repo) {
    discussion(number: $number) {
      number
      title
      body
      author { login }
      authorAssociation
      createdAt
      updatedAt
      url
      closed
      locked
      answer { id }
      labels(first: 20) { nodes { name } }
      category { name }
      comments(first: 100) {
        totalCount
        nodes {
          author { login }
          authorAssociation
          createdAt
          body
          isAnswer
          replies(first: 100) {
            nodes {
              author { login }
              authorAssociation
              createdAt
              body
            }
          }
        }
      }
    }
  }
}"#;

fn gql_author(val: &Value) -> String {
    val.get("author")
        .and_then(|u| u.get("login"))
        .and_then(|v| v.as_str())
        .unwrap_or("(deleted)")
        .to_string()
}

fn gql_body(val: &Value) -> Value {
    match val.get("body").and_then(|v| v.as_str()) {
        Some(s) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                Value::Null
            } else {
                Value::String(trimmed.to_string())
            }
        }
        None => Value::Null,
    }
}

fn fetch_discussion_graphql(repo: &str, number: u64) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| "Invalid repo format for GraphQL".to_string())?;

    let data = gh_graphql(
        DISCUSSION_QUERY,
        json!({"owner": owner, "repo": name, "number": number as i64}),
    )?;

    let disc = data
        .get("repository")
        .and_then(|r| r.get("discussion"))
        .ok_or_else(|| format!("Discussion #{} not found in {}", number, repo))?;

    if disc.is_null() {
        return Err(format!("Discussion #{} not found in {}", number, repo));
    }

    let closed = disc
        .get("closed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_answer = disc.get("answer").map(|v| !v.is_null()).unwrap_or(false);

    let labels: Vec<Value> = disc
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| {
                    l.get("name")
                        .and_then(|n| n.as_str())
                        .map(|s| Value::String(s.to_string()))
                })
                .collect()
        })
        .unwrap_or_default();

    let category = disc
        .get("category")
        .and_then(|c| c.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let comment_count = disc
        .get("comments")
        .and_then(|c| c.get("totalCount"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    // Build threaded comments
    let comments: Vec<Value> = disc
        .get("comments")
        .and_then(|c| c.get("nodes"))
        .and_then(|v| v.as_array())
        .map(|nodes| {
            nodes
                .iter()
                .map(|c| {
                    let replies: Vec<Value> = c
                        .get("replies")
                        .and_then(|r| r.get("nodes"))
                        .and_then(|v| v.as_array())
                        .map(|rps| {
                            rps.iter()
                                .map(|rp| {
                                    json!({
                                        "author": gql_author(rp),
                                        "author_association": rp.get("authorAssociation")
                                            .and_then(|v| v.as_str()).unwrap_or(""),
                                        "created_at": rp.get("createdAt")
                                            .and_then(|v| v.as_str()).unwrap_or(""),
                                        "body": gql_body(rp),
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();

                    let is_answer = c.get("isAnswer").and_then(|v| v.as_bool()).unwrap_or(false);

                    let mut comment = json!({
                        "author": gql_author(c),
                        "author_association": c.get("authorAssociation")
                            .and_then(|v| v.as_str()).unwrap_or(""),
                        "created_at": c.get("createdAt")
                            .and_then(|v| v.as_str()).unwrap_or(""),
                        "body": gql_body(c),
                    });

                    if is_answer {
                        comment["is_answer"] = Value::Bool(true);
                    }
                    if !replies.is_empty() {
                        comment["replies"] = Value::Array(replies);
                    }

                    comment
                })
                .collect()
        })
        .unwrap_or_default();

    let mut result = json!({
        "type": "discussion",
        "number": number,
        "repo": repo,
        "title": disc.get("title").and_then(|v| v.as_str()).unwrap_or(""),
        "state": if closed { "closed" } else { "open" },
        "author": gql_author(disc),
        "author_association": disc.get("authorAssociation")
            .and_then(|v| v.as_str()).unwrap_or(""),
        "created_at": disc.get("createdAt").and_then(|v| v.as_str()).unwrap_or(""),
        "updated_at": disc.get("updatedAt").and_then(|v| v.as_str()).unwrap_or(""),
        "url": disc.get("url").and_then(|v| v.as_str()).unwrap_or(""),
        "labels": labels,
        "body": gql_body(disc),
        "comment_count": comment_count,
        "comments": comments,
    });

    if !category.is_empty() {
        result["category"] = Value::String(category);
    }
    if has_answer {
        result["answered"] = Value::Bool(true);
    }

    Ok(result)
}

/// Fetch a GitHub Discussion via GraphQL, collect refs, compact.
/// Parallel to `fetch_issue_internal` but for Discussions.
fn fetch_gh_discussion_internal(
    repo: &str,
    number: u64,
) -> Result<(String, Option<String>), String> {
    let mut parent = fetch_discussion_graphql(repo, number)?;

    // Collect GitHub refs
    let seen: HashSet<(String, u64)> = [(repo.to_string(), number)].into();
    let all_refs = collect_refs_from_discussion(&parent, repo);
    let mut refs: Vec<(String, u64)> = all_refs.difference(&seen).cloned().collect();
    refs.sort();
    refs.truncate(MAX_RELATED);

    if !refs.is_empty() {
        let ref_list: Vec<Value> = refs
            .iter()
            .map(|(r, n)| json!({"repo": r, "number": n}))
            .collect();
        parent["related_refs"] = Value::Array(ref_list);
    }

    // Compact
    let parent_json = serde_json::to_string(&parent).map_err(|e| format!("JSON error: {}", e))?;
    let cache_json = serde_json::to_string(&json!({"_n": 0})).unwrap();
    let (compacted, cache_out) =
        compact::compact_discussion(&parent_json, Some(&cache_json), None, None)
            .map_err(|e| format!("Compaction error: {}", e))?;

    Ok((compacted, cache_out))
}

// ---------------------------------------------------------------------------
// Issue/PR fetching (parallel HTTP, no GIL)
// ---------------------------------------------------------------------------

fn fetch_single_discussion(
    repo: &str,
    number: u64,
    include_files: bool,
    include_timeline: bool,
) -> Result<Value, String> {
    // First request must be sequential — need to know if it's a PR
    let issue = gh_get(&format!("repos/{}/issues/{}", repo, number))?;
    let is_pr = issue.get("pull_request").is_some();

    let mut result = json!({
        "type": if is_pr { "pull_request" } else { "issue" },
        "number": number,
        "repo": repo,
        "title": json_str(&issue, "title"),
        "state": json_str(&issue, "state"),
        "author": json_author(&issue),
        "author_association": json_str(&issue, "author_association"),
        "created_at": json_str(&issue, "created_at"),
        "updated_at": json_str(&issue, "updated_at"),
        "url": json_str(&issue, "html_url"),
        "labels": issue.get("labels")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(|s| Value::String(s.to_string())))
                .collect::<Vec<_>>())
            .unwrap_or_default(),
        "body": json_body(&issue),
        "comment_count": issue.get("comments").and_then(|v| v.as_u64()).unwrap_or(0),
    });

    // Fire all remaining requests in parallel
    std::thread::scope(|s| {
        let comments_h = s.spawn(|| {
            gh_get_paginated_bookends(
                &format!("repos/{}/issues/{}/comments", repo, number),
                COMMENT_HEAD_PAGES,
                COMMENT_TAIL_PAGES,
            )
        });
        let timeline_h = if include_timeline {
            Some(s.spawn(|| {
                gh_get_paginated_bookends(
                    &format!("repos/{}/issues/{}/timeline", repo, number),
                    TIMELINE_HEAD_PAGES,
                    TIMELINE_TAIL_PAGES,
                )
            }))
        } else {
            None
        };
        let pr_h = if is_pr {
            Some(s.spawn(|| gh_get(&format!("repos/{}/pulls/{}", repo, number))))
        } else {
            None
        };
        let reviews_h = if is_pr {
            Some(s.spawn(|| gh_get_paginated(&format!("repos/{}/pulls/{}/reviews", repo, number))))
        } else {
            None
        };
        let review_comments_h = if is_pr {
            Some(s.spawn(|| gh_get_paginated(&format!("repos/{}/pulls/{}/comments", repo, number))))
        } else {
            None
        };
        let files_h = if is_pr && include_files {
            Some(s.spawn(|| gh_get_paginated(&format!("repos/{}/pulls/{}/files", repo, number))))
        } else {
            None
        };

        // Collect: comments
        let comments = comments_h.join().unwrap().unwrap_or_default();
        result["comments"] = Value::Array(
            comments
                .iter()
                .map(|c| {
                    if c.get("_skipped_middle").is_some() {
                        return json!({
                            "author": "[system]",
                            "body": "--- older comments omitted (middle pages skipped) ---",
                        });
                    }
                    json!({
                        "author": json_author(c),
                        "author_association": json_str(c, "author_association"),
                        "created_at": json_str(c, "created_at"),
                        "body": json_body(c),
                    })
                })
                .collect(),
        );

        // Collect: timeline
        if let Some(handle) = timeline_h {
            if let Ok(timeline) = handle.join().unwrap() {
                let referenced_by = parse_timeline(&timeline, repo);
                if !referenced_by.is_empty() {
                    result["referenced_by"] = Value::Array(referenced_by);
                }
            }
        }

        // Collect: PR data
        if is_pr {
            if let Some(handle) = pr_h {
                if let Ok(pr_data) = handle.join().unwrap() {
                    let merged = pr_data
                        .get("merged")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    result["merged"] = Value::Bool(merged);
                    if merged {
                        result["merged_by"] = pr_data
                            .get("merged_by")
                            .and_then(|u| u.get("login"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        result["merged_at"] =
                            pr_data.get("merged_at").cloned().unwrap_or(Value::Null);
                    }
                    result["base"] = Value::String(
                        pr_data
                            .get("base")
                            .and_then(|b| b.get("ref"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    result["head"] = Value::String(
                        pr_data
                            .get("head")
                            .and_then(|h| h.get("label"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string(),
                    );
                    result["additions"] =
                        pr_data.get("additions").cloned().unwrap_or(Value::from(0));
                    result["deletions"] =
                        pr_data.get("deletions").cloned().unwrap_or(Value::from(0));
                    result["changed_files"] = pr_data
                        .get("changed_files")
                        .cloned()
                        .unwrap_or(Value::from(0));
                }
            }

            let reviews = reviews_h
                .and_then(|h| h.join().ok())
                .and_then(|r| r.ok())
                .unwrap_or_default();
            let review_comments = review_comments_h
                .and_then(|h| h.join().ok())
                .and_then(|r| r.ok())
                .unwrap_or_default();
            result["reviews"] = Value::Array(build_reviews(&reviews, &review_comments));

            if let Some(handle) = files_h {
                let files = handle.join().unwrap().unwrap_or_default();
                result["files"] = Value::Array(
                    files
                        .iter()
                        .map(|f| {
                            json!({
                                "filename": json_str(f, "filename"),
                                "status": json_str(f, "status"),
                                "additions": f.get("additions").and_then(|v| v.as_u64()).unwrap_or(0),
                                "deletions": f.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0),
                                "patch": f.get("patch").cloned().unwrap_or(Value::Null),
                            })
                        })
                        .collect(),
                );
            }
        }
    });

    Ok(result)
}

// ---------------------------------------------------------------------------
// Ref collection from discussion
// ---------------------------------------------------------------------------

fn iter_discussion_texts(result: &Value) -> Vec<&str> {
    let mut texts = Vec::new();
    if let Some(body) = result.get("body").and_then(|v| v.as_str()) {
        if !body.is_empty() {
            texts.push(body);
        }
    }
    for field in &["comments", "reviews"] {
        if let Some(arr) = result.get(*field).and_then(|v| v.as_array()) {
            for item in arr {
                if let Some(body) = item.get("body").and_then(|v| v.as_str()) {
                    if !body.is_empty() {
                        texts.push(body);
                    }
                }
                // Direct replies on comments (Discussions — threaded)
                if let Some(replies) = item.get("replies").and_then(|v| v.as_array()) {
                    for rp in replies {
                        if let Some(body) = rp.get("body").and_then(|v| v.as_str()) {
                            if !body.is_empty() {
                                texts.push(body);
                            }
                        }
                    }
                }
                // Inline comments (reviews only)
                if let Some(inlines) = item.get("inline_comments").and_then(|v| v.as_array()) {
                    for ic in inlines {
                        if let Some(body) = ic.get("body").and_then(|v| v.as_str()) {
                            if !body.is_empty() {
                                texts.push(body);
                            }
                        }
                        if let Some(replies) = ic.get("replies").and_then(|v| v.as_array()) {
                            for rp in replies {
                                if let Some(body) = rp.get("body").and_then(|v| v.as_str()) {
                                    if !body.is_empty() {
                                        texts.push(body);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    texts
}

fn collect_refs_from_discussion(result: &Value, default_repo: &str) -> HashSet<(String, u64)> {
    let mut refs = HashSet::new();
    for text in iter_discussion_texts(result) {
        for (repo, num) in git_refs::extract_github_refs(text, default_repo) {
            refs.insert((repo, num));
        }
    }
    if let Some(referenced_by) = result.get("referenced_by").and_then(|v| v.as_array()) {
        for ref_item in referenced_by {
            if ref_item.get("event").and_then(|v| v.as_str()) == Some("cross-reference") {
                if let Some(source_number) = ref_item.get("source_number").and_then(|v| v.as_u64())
                {
                    let source_repo = ref_item
                        .get("source_repo")
                        .and_then(|v| v.as_str())
                        .unwrap_or(default_repo)
                        .to_string();
                    refs.insert((source_repo, source_number));
                }
            }
        }
    }
    refs
}

// ---------------------------------------------------------------------------
// Public internal API (called from cache.rs with GIL released)
// ---------------------------------------------------------------------------

/// Fetch, assemble, compact, and return (compacted_json, cache_entries_json).
///
/// This function does all network I/O and CPU work. Designed to run with the
/// GIL released via `py.allow_threads()`.
pub fn fetch_issue_internal(repo: &str, number: u64) -> Result<(String, Option<String>), String> {
    if !has_git_token() {
        return Err(
            "No GitHub token found. A token is required for fetching issues/PRs \
             (cross-references, higher rate limits).\n\n\
             Set the GITHUB_TOKEN or GH_TOKEN environment variable, or use \
             load_env() to load it from a .env file.\n\n\
             The token needs no special scopes — a classic PAT with default (no) \
             permissions works for public repos."
                .into(),
        );
    }

    // Fetch parent: try REST (issue/PR) first; fall back to GraphQL (Discussion) on 404
    let mut parent = match fetch_single_discussion(repo, number, true, true) {
        Ok(val) => val,
        Err(e) if e.starts_with("Not found:") => {
            // REST 404 — might be a Discussion. Try GraphQL.
            return match fetch_gh_discussion_internal(repo, number) {
                Ok(result) => Ok(result),
                Err(_) => Err(format!(
                    "#{} not found in {} (checked Issues, PRs, and Discussions).",
                    number, repo
                )),
            };
        }
        Err(e) => return Err(e),
    };

    // Collect GitHub refs
    let seen: HashSet<(String, u64)> = [(repo.to_string(), number)].into();
    let all_refs = collect_refs_from_discussion(&parent, repo);
    let mut refs: Vec<(String, u64)> = all_refs.difference(&seen).cloned().collect();
    refs.sort();
    refs.truncate(MAX_RELATED);

    if !refs.is_empty() {
        // List refs for the agent to dive into on demand — no extra fetches
        let ref_list: Vec<Value> = refs
            .iter()
            .map(|(r, n)| json!({"repo": r, "number": n}))
            .collect();
        parent["related_refs"] = Value::Array(ref_list);
    }

    // Compact
    let parent_json = serde_json::to_string(&parent).map_err(|e| format!("JSON error: {}", e))?;
    let cache_json = serde_json::to_string(&json!({"_n": 0})).unwrap();
    let (compacted, cache_out) =
        compact::compact_discussion(&parent_json, Some(&cache_json), None, None)
            .map_err(|e| format!("Compaction error: {}", e))?;

    Ok((compacted, cache_out))
}

// ---------------------------------------------------------------------------
// git_api — generic GitHub REST API access (no GIL needed)
// ---------------------------------------------------------------------------

pub fn git_api_internal(repo: &str, path: &str, truncate_at: usize) -> String {
    if let Some(err) = git_refs::validate_repo(repo) {
        return err;
    }

    let top_level = [
        "search/",
        "users/",
        "orgs/",
        "gists/",
        "rate_limit",
        "repos/",
    ];
    let url = if top_level.iter().any(|p| path.starts_with(p)) {
        format!("{}/{}", GITHUB_API, path)
    } else {
        format!("{}/repos/{}/{}", GITHUB_API, repo, path)
    };

    match gh_get(&url) {
        Ok(data) => {
            let text = serde_json::to_string_pretty(&data).unwrap_or_default();
            if text.len() > truncate_at {
                format!(
                    "{}\n\n... (truncated, refine your query)",
                    &text[..compact::safe_byte_index(&text, truncate_at)]
                )
            } else {
                text
            }
        }
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// PyO3 wrappers
// ---------------------------------------------------------------------------

/// Read-only GET against any GitHub REST API endpoint. Returns JSON.
#[pyfunction]
#[pyo3(signature = (repo, path, *, truncate_at=80_000))]
pub fn git_api(_py: Python<'_>, repo: &str, path: &str, truncate_at: usize) -> String {
    git_api_internal(repo, path, truncate_at)
}

/// Fetch, search, or list GitHub issues/PRs/Discussions.
///
/// Mode determined by parameters:
/// - `number` given → FETCH a single issue/PR/Discussion
/// - `query` given → SEARCH via GitHub search API
/// - neither → LIST recent items
#[pyfunction]
#[pyo3(signature = (
    *,
    repo = None,
    number = None,
    query = None,
    kind = "all",
    state = "open",
    sort = None,
    limit = 20,
    labels = None,
))]
#[allow(clippy::too_many_arguments)]
pub fn github_issues(
    _py: Python<'_>,
    repo: Option<&str>,
    number: Option<u64>,
    query: Option<&str>,
    kind: &str,
    state: &str,
    sort: Option<&str>,
    limit: usize,
    labels: Option<&str>,
) -> PyResult<String> {
    // Resolve repo: explicit or auto-detect from cwd
    let repo_str = match repo {
        Some(r) => r.to_string(),
        None => match detect_git_repo(".") {
            Some(r) => r,
            None => {
                return Ok(
                    "No repo specified and could not auto-detect from git remote.".to_string(),
                )
            }
        },
    };
    if let Some(err) = git_refs::validate_repo(&repo_str) {
        return Ok(err);
    }

    match (number, query) {
        (Some(num), _) => {
            // FETCH mode
            let (text, _cache) = fetch_issue_internal(&repo_str, num)
                .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
            Ok(text)
        }
        (None, Some(q)) => {
            // SEARCH mode
            Ok(search_issues_dispatch(
                &repo_str, q, kind, state, sort, limit, labels,
            ))
        }
        (None, None) => {
            // LIST mode
            Ok(list_issues_internal(
                &repo_str,
                kind,
                state,
                sort.unwrap_or("created"),
                limit,
                labels,
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Build GitHub search qualifier string from structured parameters.
fn build_search_qualifiers(repo: &str, kind: &str, state: &str, labels: Option<&str>) -> String {
    let mut q = format!(" repo:{}", repo);
    match kind {
        "issue" => q.push_str(" type:issue"),
        "pr" => q.push_str(" type:pr"),
        _ => {} // "all" / "discussion" — no type qualifier
    }
    match state {
        "open" => q.push_str(" state:open"),
        "closed" => q.push_str(" state:closed"),
        _ => {} // "all"
    }
    if let Some(lbls) = labels {
        for label in lbls.split(',') {
            let label = label.trim();
            if !label.is_empty() {
                if label.contains(' ') {
                    q.push_str(&format!(" label:\"{}\"", label));
                } else {
                    q.push_str(&format!(" label:{}", label));
                }
            }
        }
    }
    q
}

/// SEARCH mode: issues + PRs via REST search/issues API.
fn search_issues_internal(
    repo: &str,
    user_query: &str,
    kind: &str,
    state: &str,
    sort: Option<&str>,
    limit: usize,
    labels: Option<&str>,
) -> String {
    let q = format!(
        "{}{}",
        user_query,
        build_search_qualifiers(repo, kind, state, labels)
    );
    let per_page = limit.min(100);

    let mut req = AGENT
        .get(&format!("{}/search/issues", GITHUB_API))
        .set("Accept", "application/vnd.github+json")
        .set("User-Agent", "mcp-methods")
        .query("q", &q)
        .query("per_page", &per_page.to_string());

    if let Some(s) = sort {
        req = req.query("sort", s);
    }
    // When sort is None, GitHub defaults to "best match" (relevance)

    if let Some(token) = auth_token() {
        req = req.set("Authorization", &format!("Bearer {}", token));
    }

    match req.call() {
        Ok(resp) => {
            let data: Value = match resp.into_json() {
                Ok(v) => v,
                Err(e) => return format!("JSON parse error: {}", e),
            };
            format_search_results(repo, user_query, &data)
        }
        Err(ureq::Error::Status(422, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            format!("GitHub search validation error: {}", body)
        }
        Err(ureq::Error::Status(403, resp)) => {
            let body = resp.into_string().unwrap_or_default();
            if body.to_lowercase().contains("rate limit") {
                "GitHub API rate limit exceeded. Set GITHUB_TOKEN or GH_TOKEN for higher limits."
                    .to_string()
            } else {
                format!("GitHub API forbidden: {}", body)
            }
        }
        Err(e) => format!("GitHub search error: {}", e),
    }
}

/// SEARCH mode: Discussions via GraphQL search(type: DISCUSSION).
fn search_discussions_graphql(
    repo: &str,
    user_query: &str,
    state: &str,
    sort: Option<&str>,
    limit: usize,
    labels: Option<&str>,
) -> String {
    let qualifiers = build_search_qualifiers(repo, "discussion", state, labels);
    let q = format!("{}{}", user_query, qualifiers);
    let per_page = limit.min(100);

    // GraphQL search doesn't support sort directly in the query — the search
    // endpoint always returns by relevance. sort is ignored for Discussions.
    let _ = sort;

    let query = r#"query($q: String!, $first: Int!) {
  search(type: DISCUSSION, query: $q, first: $first) {
    discussionCount
    nodes {
      ... on Discussion {
        number
        title
        author { login }
        createdAt
        closed
        comments { totalCount }
        category { name }
        labels(first: 5) { nodes { name } }
        answer { id }
      }
    }
  }
}"#;

    let vars = json!({"q": q, "first": per_page as i64});

    let data = match gh_graphql(query, vars) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let total = data
        .get("search")
        .and_then(|s| s.get("discussionCount"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let nodes = match data
        .get("search")
        .and_then(|s| s.get("nodes"))
        .and_then(|v| v.as_array())
    {
        Some(n) if !n.is_empty() => n,
        _ => return format!("No discussion results for \"{}\" in {}.", user_query, repo),
    };

    let mut out = format!(
        "{} discussion{} (of {}) for \"{}\" in {}:\n",
        nodes.len(),
        if nodes.len() == 1 { "" } else { "s" },
        total,
        user_query,
        repo,
    );

    for d in nodes {
        let number = d.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        if number == 0 {
            continue; // skip non-Discussion nodes in union result
        }
        let title = d.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let author = gql_author(d);
        let date = d
            .get("createdAt")
            .and_then(|v| v.as_str())
            .and_then(|s| s.get(..10))
            .unwrap_or("");
        let comment_count = d
            .get("comments")
            .and_then(|c| c.get("totalCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let comments = if comment_count > 0 {
            format!(
                ", {} comment{}",
                comment_count,
                if comment_count == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let category = d
            .get("category")
            .and_then(|c| c.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cat_tag = if category.is_empty() {
            String::new()
        } else {
            format!(" [{}]", category)
        };
        let label_str: String = d
            .get("labels")
            .and_then(|l| l.get("nodes"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!(" [{}]", s))
            .unwrap_or_default();
        let answered = if d.get("answer").map(|v| !v.is_null()).unwrap_or(false) {
            " [answered]"
        } else {
            ""
        };

        out.push_str(&format!(
            "  #{}{}{}{} {} — {} ({}{})\n",
            number, cat_tag, label_str, answered, title, author, date, comments
        ));
    }

    out.trim_end().to_string()
}

/// Route SEARCH mode to the right backend based on `kind`.
fn search_issues_dispatch(
    repo: &str,
    query: &str,
    kind: &str,
    state: &str,
    sort: Option<&str>,
    limit: usize,
    labels: Option<&str>,
) -> String {
    match kind {
        "discussion" => search_discussions_graphql(repo, query, state, sort, limit, labels),
        "issue" | "pr" => search_issues_internal(repo, query, kind, state, sort, limit, labels),
        _ => {
            // kind="all": run REST for issues + PRs, and GraphQL for Discussions.
            // GitHub's search/issues endpoint requires a type qualifier, so we run
            // two separate REST searches and merge the results.
            let issues = search_issues_internal(repo, query, "issue", state, sort, limit, labels);
            let prs = search_issues_internal(repo, query, "pr", state, sort, limit, labels);
            let rest = match (
                issues.starts_with("No results"),
                prs.starts_with("No results"),
            ) {
                (true, true) => issues, // both empty — return the "No results" message
                (true, false) => prs,
                (false, true) => issues,
                (false, false) => format!("{}\n\n{}", issues, prs),
            };
            let gql = search_discussions_graphql(repo, query, state, sort, limit, labels);
            if gql.starts_with("No discussion") {
                rest
            } else if rest.starts_with("No results") {
                gql
            } else {
                format!("{}\n\n{}", rest, gql)
            }
        }
    }
}

/// Format REST search/issues results.
fn format_search_results(repo: &str, user_query: &str, data: &Value) -> String {
    let total = data
        .get("total_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let items = match data.get("items").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return format!("No results for \"{}\" in {}.", user_query, repo),
    };

    let mut out = format!(
        "{} result{} (of {}) for \"{}\" in {}:\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        total,
        user_query,
        repo,
    );

    for item in items {
        let is_pr = item.get("pull_request").is_some();
        if is_pr {
            let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            let title = json_str(item, "title");
            let author = json_author(item);
            let labels = format_label_tags(item);
            let date = format_date(item, "created_at");
            let comments = format_comments(item);
            out.push_str(&format!(
                "  #{}{} [PR] {} — {} ({}{})\n",
                number, labels, title, author, date, comments
            ));
        } else {
            out.push_str(&format_issue_line(item));
            out.push('\n');
        }
    }

    out.trim_end().to_string()
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

fn list_discussions_graphql(repo: &str, state: &str, sort: &str, per_page: usize) -> String {
    let (owner, name) = match repo.split_once('/') {
        Some(pair) => pair,
        None => return format!("Invalid repo format: {}", repo),
    };

    let order_field = match sort {
        "updated" => "UPDATED_AT",
        _ => "CREATED_AT",
    };

    let states: Value = match state {
        "open" => json!(["OPEN"]),
        "closed" => json!(["CLOSED"]),
        _ => Value::Null,
    };

    // orderBy uses an enum value, so interpolate it into the query string
    let query = format!(
        r#"query($owner: String!, $repo: String!, $first: Int!, $states: [DiscussionState!]) {{
  repository(owner: $owner, name: $repo) {{
    discussions(first: $first, states: $states, orderBy: {{field: {}, direction: DESC}}) {{
      nodes {{
        number
        title
        author {{ login }}
        createdAt
        closed
        comments {{ totalCount }}
        category {{ name }}
        labels(first: 5) {{ nodes {{ name }} }}
        answer {{ id }}
      }}
    }}
  }}
}}"#,
        order_field
    );

    let vars = json!({
        "owner": owner,
        "repo": name,
        "first": per_page.min(100) as i64,
        "states": states,
    });

    let data = match gh_graphql(&query, vars) {
        Ok(d) => d,
        Err(e) => return e,
    };

    let nodes = match data
        .get("repository")
        .and_then(|r| r.get("discussions"))
        .and_then(|d| d.get("nodes"))
        .and_then(|v| v.as_array())
    {
        Some(n) if !n.is_empty() => n,
        _ => return format!("No {} discussions in {}.", state, repo),
    };

    let mut out = format!(
        "{} discussion{} in {} ({}):\n",
        nodes.len(),
        if nodes.len() == 1 { "" } else { "s" },
        repo,
        state
    );

    for d in nodes {
        let number = d.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
        let title = d.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let author = gql_author(d);
        let date = d
            .get("createdAt")
            .and_then(|v| v.as_str())
            .and_then(|s| s.get(..10))
            .unwrap_or("");
        let comment_count = d
            .get("comments")
            .and_then(|c| c.get("totalCount"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let comments = if comment_count > 0 {
            format!(
                ", {} comment{}",
                comment_count,
                if comment_count == 1 { "" } else { "s" }
            )
        } else {
            String::new()
        };
        let category = d
            .get("category")
            .and_then(|c| c.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let cat_tag = if category.is_empty() {
            String::new()
        } else {
            format!(" [{}]", category)
        };
        let label_str: String = d
            .get("labels")
            .and_then(|l| l.get("nodes"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .map(|s| format!(" [{}]", s))
            .unwrap_or_default();
        let answered = if d.get("answer").map(|v| !v.is_null()).unwrap_or(false) {
            " [answered]"
        } else {
            ""
        };
        let is_closed = d.get("closed").and_then(|v| v.as_bool()).unwrap_or(false);
        let state_tag = if is_closed { " [closed]" } else { "" };

        out.push_str(&format!(
            "  #{}{}{}{}{} {} — {} ({}{})\n",
            number, cat_tag, label_str, answered, state_tag, title, author, date, comments
        ));
    }

    out.trim_end().to_string()
}

fn list_issues_internal(
    repo: &str,
    kind: &str,
    state: &str,
    sort: &str,
    limit: usize,
    labels: Option<&str>,
) -> String {
    let per_page = limit.min(100);
    let direction = "desc";

    match kind {
        "pr" => list_pulls(repo, state, sort, direction, per_page),
        "issue" => list_issues_only(repo, state, sort, direction, per_page, labels),
        "discussion" => list_discussions_graphql(repo, state, sort, per_page),
        _ => list_all(repo, state, sort, direction, per_page, labels),
    }
}

fn list_pulls(repo: &str, state: &str, sort: &str, direction: &str, per_page: usize) -> String {
    let path = format!(
        "repos/{}/pulls?state={}&sort={}&direction={}&per_page={}",
        repo, state, sort, direction, per_page
    );
    match gh_get(&format!("{}/{}", GITHUB_API, &path)) {
        Ok(Value::Array(items)) => format_pull_list(repo, state, &items),
        Ok(_) => "Unexpected response format.".to_string(),
        Err(e) => e,
    }
}

fn list_issues_only(
    repo: &str,
    state: &str,
    sort: &str,
    direction: &str,
    per_page: usize,
    labels: Option<&str>,
) -> String {
    let mut path = format!(
        "repos/{}/issues?state={}&sort={}&direction={}&per_page={}",
        repo, state, sort, direction, per_page
    );
    if let Some(lbls) = labels {
        if !lbls.is_empty() {
            path.push_str(&format!("&labels={}", lbls));
        }
    }
    match gh_get(&format!("{}/{}", GITHUB_API, &path)) {
        Ok(Value::Array(items)) => {
            // Filter out PRs (GitHub Issues API returns both)
            let issues: Vec<&Value> = items
                .iter()
                .filter(|item| item.get("pull_request").is_none())
                .collect();
            format_issue_list(repo, state, &issues)
        }
        Ok(_) => "Unexpected response format.".to_string(),
        Err(e) => e,
    }
}

fn list_all(
    repo: &str,
    state: &str,
    sort: &str,
    direction: &str,
    per_page: usize,
    labels: Option<&str>,
) -> String {
    let mut path = format!(
        "repos/{}/issues?state={}&sort={}&direction={}&per_page={}",
        repo, state, sort, direction, per_page
    );
    if let Some(lbls) = labels {
        if !lbls.is_empty() {
            path.push_str(&format!("&labels={}", lbls));
        }
    }
    match gh_get(&format!("{}/{}", GITHUB_API, &path)) {
        Ok(Value::Array(items)) => {
            let refs: Vec<&Value> = items.iter().collect();
            format_mixed_list(repo, state, &refs)
        }
        Ok(_) => "Unexpected response format.".to_string(),
        Err(e) => e,
    }
}

// ---------------------------------------------------------------------------
// List formatting helpers
// ---------------------------------------------------------------------------

fn format_label_tags(item: &Value) -> String {
    item.get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty())
        .map(|s| format!(" [{}]", s))
        .unwrap_or_default()
}

fn format_date(item: &Value, key: &str) -> String {
    item.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.get(..10).unwrap_or(s).to_string())
        .unwrap_or_default()
}

fn format_comments(item: &Value) -> String {
    let count = item.get("comments").and_then(|v| v.as_u64()).unwrap_or(0);
    if count > 0 {
        format!(", {} comment{}", count, if count == 1 { "" } else { "s" })
    } else {
        String::new()
    }
}

fn format_issue_line(item: &Value) -> String {
    let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
    let title = json_str(item, "title");
    let author = json_author(item);
    let labels = format_label_tags(item);
    let date = format_date(item, "created_at");
    let comments = format_comments(item);
    format!(
        "  #{}{} {} — {} ({}{})",
        number, labels, title, author, date, comments
    )
}

fn format_pr_line(item: &Value) -> String {
    let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
    let title = json_str(item, "title");
    let author = json_author(item);
    let labels = format_label_tags(item);
    let date = format_date(item, "created_at");
    let comments = format_comments(item);
    let draft = if item.get("draft").and_then(|v| v.as_bool()).unwrap_or(false) {
        " [draft]"
    } else {
        ""
    };
    let base = item
        .get("base")
        .and_then(|b| b.get("ref"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let head = item
        .get("head")
        .and_then(|h| h.get("ref"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let branch_info = if !base.is_empty() && !head.is_empty() {
        format!(" {} -> {}", head, base)
    } else {
        String::new()
    };
    format!(
        "  #{}{}{} {} — {} ({}{}){}",
        number, labels, draft, title, author, date, comments, branch_info
    )
}

fn format_issue_list(repo: &str, state: &str, items: &[&Value]) -> String {
    if items.is_empty() {
        return format!("No {} issues in {}.", state, repo);
    }
    let mut out = format!(
        "{} issue{} in {} ({}):\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        repo,
        state
    );
    for item in items {
        out.push_str(&format_issue_line(item));
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn format_pull_list(repo: &str, state: &str, items: &[Value]) -> String {
    if items.is_empty() {
        return format!("No {} pull requests in {}.", state, repo);
    }
    let mut out = format!(
        "{} pull request{} in {} ({}):\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        repo,
        state
    );
    for item in items {
        out.push_str(&format_pr_line(item));
        out.push('\n');
    }
    out.trim_end().to_string()
}

fn format_mixed_list(repo: &str, state: &str, items: &[&Value]) -> String {
    if items.is_empty() {
        return format!("No {} discussions in {}.", state, repo);
    }
    let mut out = format!(
        "{} discussion{} in {} ({}):\n",
        items.len(),
        if items.len() == 1 { "" } else { "s" },
        repo,
        state
    );
    for item in items {
        let is_pr = item.get("pull_request").is_some();
        if is_pr {
            // Issues API doesn't return full PR data (base/head), so format as issue with PR marker
            let number = item.get("number").and_then(|v| v.as_u64()).unwrap_or(0);
            let title = json_str(item, "title");
            let author = json_author(item);
            let labels = format_label_tags(item);
            let date = format_date(item, "created_at");
            let comments = format_comments(item);
            out.push_str(&format!(
                "  #{}{} [PR] {} — {} ({}{})\n",
                number, labels, title, author, date, comments
            ));
        } else {
            out.push_str(&format_issue_line(item));
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}
