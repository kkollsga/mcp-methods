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
pub const OVERFLOW_LIMIT: usize = 50_000;
pub const OVERFLOW_PREVIEW: usize = 20_000;
const MAX_RELATED: usize = 10;

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
fn estimate_json_size(val: &Value) -> usize {
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

fn parse_link_next(link: &str) -> Option<String> {
    for part in link.split(',') {
        if part.contains("rel=\"next\"") {
            let start = part.find('<')? + 1;
            let end = part.find('>')?;
            return Some(part[start..end].to_string());
        }
    }
    None
}

fn gh_get_paginated(endpoint: &str) -> Result<Vec<Value>, String> {
    let mut url = format!("{}/{}", GITHUB_API, endpoint);
    let mut all_items: Vec<Value> = Vec::new();

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

        // Extract next URL before consuming response body
        let link_header: Option<String> = resp.header("link").map(String::from);
        let items: Value = resp
            .into_json()
            .map_err(|e| format!("JSON parse error: {}", e))?;

        if let Value::Array(arr) = items {
            all_items.extend(arr);
        }

        match link_header.as_deref().and_then(parse_link_next) {
            Some(u) => url = u,
            None => break,
        }
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
// Discussion fetching (parallel HTTP, no GIL)
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
    });

    // Fire all remaining requests in parallel
    std::thread::scope(|s| {
        let comments_h =
            s.spawn(|| gh_get_paginated(&format!("repos/{}/issues/{}/comments", repo, number)));
        let timeline_h = if include_timeline {
            Some(
                s.spawn(|| gh_get_paginated(&format!("repos/{}/issues/{}/timeline", repo, number))),
            )
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
pub fn fetch_issue_internal(
    repo: &str,
    number: u64,
    expand: &[String],
) -> Result<(String, Option<String>), String> {
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

    // Fetch parent discussion
    let mut parent = fetch_single_discussion(repo, number, true, true)?;

    // Collect GitHub refs
    let seen: HashSet<(String, u64)> = [(repo.to_string(), number)].into();
    let all_refs = collect_refs_from_discussion(&parent, repo);
    let mut refs: Vec<(String, u64)> = all_refs.difference(&seen).cloned().collect();
    refs.sort();
    refs.truncate(MAX_RELATED);

    if !refs.is_empty() {
        let parent_size = estimate_json_size(&parent);

        if parent_size < 30_000 && refs.len() <= 5 {
            // Fetch full related discussions in parallel
            let related: Vec<Value> = std::thread::scope(|s| {
                let handles: Vec<_> = refs
                    .iter()
                    .map(|(ref_repo, ref_num)| {
                        let rr = ref_repo.clone();
                        let rn = *ref_num;
                        s.spawn(move || fetch_single_discussion(&rr, rn, false, false))
                    })
                    .collect();
                let mut results = Vec::new();
                for h in handles {
                    if let Ok(Ok(disc)) = h.join() {
                        if let Ok(disc_json) = serde_json::to_string(&disc) {
                            let cache_json = serde_json::to_string(&json!({"_n": 0})).unwrap();
                            if let Ok((compacted, _)) = compact::compact_discussion(
                                &disc_json,
                                Vec::new(),
                                Some(&cache_json),
                            ) {
                                if let Ok(val) = serde_json::from_str(&compacted) {
                                    results.push(val);
                                }
                            }
                        }
                    }
                }
                results
            });
            if !related.is_empty() {
                parent["related_discussions"] = Value::Array(related);
            }
        } else {
            // Fetch just summaries in parallel
            let summaries: Vec<Value> = std::thread::scope(|s| {
                let handles: Vec<_> = refs
                    .iter()
                    .map(|(ref_repo, ref_num)| {
                        let rr = ref_repo.clone();
                        let rn = *ref_num;
                        s.spawn(move || gh_get(&format!("repos/{}/issues/{}", rr, rn)))
                    })
                    .collect();
                let mut results = Vec::new();
                for (i, h) in handles.into_iter().enumerate() {
                    if let Ok(Ok(issue_data)) = h.join() {
                        let (ref_repo, ref_num) = &refs[i];
                        let is_pr = issue_data.get("pull_request").is_some();
                        results.push(json!({
                            "type": if is_pr { "pull_request" } else { "issue" },
                            "number": ref_num,
                            "repo": ref_repo,
                            "title": json_str(&issue_data, "title"),
                            "state": json_str(&issue_data, "state"),
                            "author": json_author(&issue_data),
                        }));
                    }
                }
                results
            });
            if !summaries.is_empty() {
                parent["related_discussions"] = Value::Array(summaries);
                parent["_note"] = Value::String(
                    "Related discussions shown as summaries. \
                     Call fetch_issue(repo, number) to read any in full."
                        .to_string(),
                );
            }
        }
    }

    // Compact
    let parent_json = serde_json::to_string(&parent).map_err(|e| format!("JSON error: {}", e))?;
    let cache_json = serde_json::to_string(&json!({"_n": 0})).unwrap();
    let (compacted, cache_out) =
        compact::compact_discussion(&parent_json, expand.to_vec(), Some(&cache_json))
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
                    &text[..truncate_at]
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

/// Fetch a GitHub issue or PR conversation as JSON (one-shot, no cache).
#[pyfunction]
#[pyo3(signature = (repo, number, *, expand=None))]
pub fn git_issue(
    _py: Python<'_>,
    repo: &str,
    number: u64,
    expand: Option<Vec<String>>,
) -> PyResult<String> {
    if let Some(err) = git_refs::validate_repo(repo) {
        return Ok(err);
    }
    let expand = expand.unwrap_or_default();
    let (text, _cache) = fetch_issue_internal(repo, number, &expand)
        .map_err(pyo3::exceptions::PyRuntimeError::new_err)?;
    Ok(text)
}
