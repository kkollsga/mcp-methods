use pyo3::prelude::*;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

static SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?summary[^>]*>").unwrap());
static LANG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^```(\w*)").unwrap());

/// Case-insensitive ASCII prefix check that is safe for multi-byte UTF-8 strings.
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Find the largest valid char boundary <= `pos` in `s`.
pub fn safe_byte_index(s: &str, pos: usize) -> usize {
    let pos = pos.min(s.len());
    // Walk backwards to find a char boundary
    let mut i = pos;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// ---------------------------------------------------------------------------
// Compaction constants
// ---------------------------------------------------------------------------

const CODE_BLOCK_MAX_LINES: usize = 20;
const CODE_BLOCK_KEEP: usize = 5;
const MAINTAINER_LIMIT: usize = 5_000;
const COMMENT_PREVIEW_CHARS: usize = 500;
const REVIEW_PREVIEW_LINES: usize = 3;
const REVIEW_PREVIEW_CHARS: usize = 300;
/// Individual patch collapse: patches above this are collapsed even in small diffs.
const PATCH_INLINE_MAX_LINES: usize = 80;
/// How many lines to keep as preview when collapsing an inline patch.
const PATCH_INLINE_KEEP: usize = 20;

const MAINTAINER_ROLES: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

// Budget constants
const DEFAULT_BUDGET: usize = 60_000;
const DEFAULT_ITEM_BUDGET: usize = 15_000;
/// Safety margin: consider budget hit when within 10% of limit.
const BUDGET_MARGIN: f64 = 0.90;

// Tier-specific thresholds
const TIER5_PATCH_MAX_LINES: usize = 30;
const TIER5_PATCH_KEEP: usize = 15;
const TIER6_BODY_LIMIT: usize = 5_000;
const TIER9_BODY_LIMIT: usize = 2_000;
const TIER9_COMMENT_LIMIT: usize = 200;
const TIER9_REVIEW_CHARS: usize = 150;

// Thread digest constants (for huge discussions)
const HUGE_THREAD_THRESHOLD: usize = 50;
const DIGEST_HEAD: usize = 5;
const DIGEST_TAIL: usize = 5;
const DIGEST_MAINTAINER_MAX: usize = 15;
const DIGEST_MAINTAINER_CHARS: usize = 300;

// ---------------------------------------------------------------------------
// Size estimation (mirrors github.rs estimate_json_size)
// ---------------------------------------------------------------------------

fn estimate_size(val: &Value) -> usize {
    crate::github::estimate_json_size(val)
}

// ---------------------------------------------------------------------------
// Internal text helpers (unchanged from before)
// ---------------------------------------------------------------------------

/// Collapse large fenced code blocks and <details> sections, mutating cache in place.
pub fn collapse_code_blocks_mut(text: &str, cache: &mut Option<Value>) -> String {
    if text.is_empty() {
        return text.to_string();
    }

    let lines: Vec<&str> = text.split('\n').collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;

    while i < lines.len() {
        let stripped = lines[i].trim();

        // Collapse <details> blocks
        if starts_with_ignore_ascii_case(stripped, "<details") {
            let mut j = i + 1;
            let mut summary = String::new();
            while j < lines.len() {
                let s = lines[j].trim();
                if summary.is_empty() && starts_with_ignore_ascii_case(s, "<summary") {
                    summary = SUMMARY_RE.replace_all(s, "").trim().to_string();
                }
                if starts_with_ignore_ascii_case(s, "</details") {
                    break;
                }
                j += 1;
            }
            let hidden = if j > i { j - i - 1 } else { 0 };
            if hidden > 3 {
                let label = if summary.is_empty() {
                    "collapsed section".to_string()
                } else {
                    summary
                };
                if let Some(ref mut c) = cache {
                    let n = c.get("_n").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
                    c["_n"] = Value::from(n);
                    let eid = format!("details_{}", n);
                    let content: String = lines[(i + 1)..j].join("\n");
                    c[&eid] = serde_json::json!({
                        "type": "details",
                        "summary": label,
                        "total_lines": hidden,
                        "content": content,
                    });
                    out.push(format!("[{} — {} lines hidden, id:{}]", label, hidden, eid));
                } else {
                    out.push(format!("[{} — {} lines hidden]", label, hidden));
                }
                i = (j + 1).min(lines.len());
                continue;
            }
        }

        // Collapse large fenced code blocks
        if stripped.starts_with("```") {
            let fence_line = lines[i];
            let mut j = i + 1;
            while j < lines.len() && !lines[j].trim().starts_with("```") {
                j += 1;
            }
            let has_close = j < lines.len();
            let end = if has_close { j + 1 } else { j };
            let inner = end - i - if has_close { 2 } else { 1 };

            if inner > CODE_BLOCK_MAX_LINES {
                let hidden = inner - 2 * CODE_BLOCK_KEEP;

                if let Some(ref mut c) = cache {
                    let n = c.get("_n").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
                    c["_n"] = Value::from(n);
                    let eid = format!("cb_{}", n);
                    let lang = LANG_RE
                        .captures(fence_line.trim())
                        .and_then(|cap| cap.get(1))
                        .map(|m| m.as_str().to_string())
                        .unwrap_or_default();
                    let content_end = if has_close { j } else { end };
                    let content: String = lines[(i + 1)..content_end].join("\n");
                    c[&eid] = serde_json::json!({
                        "type": "code_block",
                        "language": lang,
                        "total_lines": inner,
                        "content": content,
                    });
                    out.push(format!("{} [id:{}, {} lines]", fence_line, eid, inner));
                } else {
                    out.push(fence_line.to_string());
                }

                // Keep first CODE_BLOCK_KEEP lines
                for line in lines
                    .iter()
                    .take((i + 1 + CODE_BLOCK_KEEP).min(lines.len()))
                    .skip(i + 1)
                {
                    out.push(line.to_string());
                }
                out.push(format!("  ... ({} lines hidden)", hidden));

                // Keep last CODE_BLOCK_KEEP lines + closing fence
                if has_close {
                    let start = j.saturating_sub(CODE_BLOCK_KEEP);
                    for line in lines.iter().take(j).skip(start) {
                        out.push(line.to_string());
                    }
                    out.push(lines[j].to_string());
                } else {
                    let start = end.saturating_sub(CODE_BLOCK_KEEP);
                    for line in lines.iter().take(end).skip(start) {
                        out.push(line.to_string());
                    }
                }
            } else {
                for line in lines.iter().take(end).skip(i) {
                    out.push(line.to_string());
                }
            }
            i = end;
            continue;
        }

        out.push(lines[i].to_string());
        i += 1;
    }

    out.join("\n")
}

/// Collapse code blocks then truncate if over limit, mutating cache in place.
/// Returns (text, was_truncated).
pub fn compact_text_mut(text: &str, limit: usize, cache: &mut Option<Value>) -> (String, bool) {
    if text.is_empty() {
        return (String::new(), false);
    }
    let collapsed = collapse_code_blocks_mut(text, cache);
    if collapsed.len() > limit {
        let truncated = format!(
            "{}…[truncated]",
            &collapsed[..safe_byte_index(&collapsed, limit)]
        );
        (truncated, true)
    } else {
        (collapsed, false)
    }
}

// ---------------------------------------------------------------------------
// Tier helper functions — each modifies `result` in place
// ---------------------------------------------------------------------------

/// Tier 0a: Filter bot comments. Returns count of bots removed.
fn filter_bot_comments(result: &mut Value) -> usize {
    if let Some(comments) = result.get_mut("comments").and_then(|v| v.as_array_mut()) {
        let original_len = comments.len();
        comments.retain(|c| {
            c.get("author")
                .and_then(|a| a.as_str())
                .map(|a| !a.ends_with("[bot]"))
                .unwrap_or(true)
        });
        let bot_count = original_len - comments.len();
        if bot_count > 0 {
            result["_bot_comments_hidden"] = Value::from(bot_count as u64);
        }
        bot_count
    } else {
        0
    }
}

/// Tier 0b: Collapse code blocks in body text only (not truncation, just block collapsing).
fn collapse_body_code_blocks(result: &mut Value, cache: &mut Option<Value>) {
    if let Some(body) = result
        .get("body")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    {
        let collapsed = collapse_code_blocks_mut(&body, cache);
        result["body"] = Value::String(collapsed);
    }
}

/// Tier 1: Collapse code blocks in all comment bodies.
fn collapse_comment_code_blocks(result: &mut Value, cache: &mut Option<Value>) {
    if let Some(comments) = result.get_mut("comments").and_then(|v| v.as_array_mut()) {
        for c in comments.iter_mut() {
            if let Some(body) = c
                .get("body")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            {
                let collapsed = collapse_code_blocks_mut(&body, cache);
                c["body"] = Value::String(collapsed);
            }
        }
    }
}

/// Tier 2: Truncate non-maintainer comments.
fn truncate_non_maintainer_comments(result: &mut Value, limit: usize, cache: &mut Option<Value>) {
    if let Some(comments) = result.get_mut("comments").and_then(|v| v.as_array_mut()) {
        for c in comments.iter_mut() {
            let is_maintainer = c
                .get("author_association")
                .and_then(|a| a.as_str())
                .map(|a| MAINTAINER_ROLES.contains(&a))
                .unwrap_or(false);
            if is_maintainer {
                continue;
            }
            truncate_comment(c, limit, cache);
        }
    }
}

/// Tier 3/5: Collapse patches over a line threshold.
fn collapse_patches_over(
    result: &mut Value,
    max_lines: usize,
    keep_lines: usize,
    cache: &mut Option<Value>,
) {
    if let Some(files) = result.get_mut("files").and_then(|v| v.as_array_mut()) {
        for f in files.iter_mut() {
            if let Some(obj) = f.as_object_mut() {
                let patch_text = match obj.get("patch").and_then(|v| v.as_str()) {
                    Some(p) if !p.is_empty() => p.to_string(),
                    _ => continue,
                };
                let total_lines = patch_text.matches('\n').count() + 1;
                if total_lines <= max_lines {
                    continue;
                }

                let filename = obj
                    .get("filename")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let additions = obj.get("additions").and_then(|v| v.as_u64()).unwrap_or(0);
                let deletions = obj.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);

                let eid = ensure_patch_cached(
                    obj,
                    &patch_text,
                    &filename,
                    additions,
                    deletions,
                    total_lines,
                    cache,
                );

                obj.remove("patch");
                let preview: String = patch_text
                    .split('\n')
                    .take(keep_lines)
                    .collect::<Vec<_>>()
                    .join("\n");
                obj.insert(
                    "patch_preview".to_string(),
                    Value::String(format!(
                        "{}\n\n... [{} more lines]",
                        preview,
                        total_lines - keep_lines
                    )),
                );
                if let Some(eid) = eid {
                    obj.insert("patch_id".to_string(), Value::String(eid));
                }
            }
        }
    }
}

/// Tier 4: Truncate maintainer comments.
fn truncate_maintainer_comments(result: &mut Value, limit: usize, cache: &mut Option<Value>) {
    if let Some(comments) = result.get_mut("comments").and_then(|v| v.as_array_mut()) {
        for c in comments.iter_mut() {
            let is_maintainer = c
                .get("author_association")
                .and_then(|a| a.as_str())
                .map(|a| MAINTAINER_ROLES.contains(&a))
                .unwrap_or(false);
            if !is_maintainer {
                continue;
            }
            truncate_comment(c, limit, cache);
        }
    }
}

/// Tier 6: Truncate PR body.
fn truncate_body(result: &mut Value, limit: usize, cache: &mut Option<Value>) {
    if let Some(body) = result
        .get("body")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    {
        if body.len() <= limit {
            return;
        }
        let (compacted, truncated) = compact_text_mut(&body, limit, cache);
        result["body"] = Value::String(compacted);
        if truncated {
            result["_body_truncated"] = Value::Bool(true);
        }
    }
}

/// Tier 7: Compact review inline comments to previews.
fn compact_reviews(
    result: &mut Value,
    preview_lines: usize,
    preview_chars: usize,
    cache: &mut Option<Value>,
) {
    if let Some(reviews) = result.get_mut("reviews").and_then(|v| v.as_array_mut()) {
        for review in reviews.iter_mut() {
            let reviewer = review
                .get("author")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(inlines) = review
                .get("inline_comments")
                .and_then(|v| v.as_array())
                .cloned()
            {
                if !inlines.is_empty() {
                    let compacted: Vec<Value> = inlines
                        .iter()
                        .map(|ic| {
                            compact_single_review_comment(
                                ic,
                                &reviewer,
                                preview_lines,
                                preview_chars,
                                cache,
                            )
                        })
                        .collect();
                    review["inline_comments"] = Value::Array(compacted);
                }
            }
        }
    }
}

/// Tier 8: Remove all inline patches, keep only file metadata.
fn remove_inline_patches(result: &mut Value, cache: &mut Option<Value>) {
    if let Some(files) = result.get_mut("files").and_then(|v| v.as_array_mut()) {
        for f in files.iter_mut() {
            if let Some(obj) = f.as_object_mut() {
                if let Some(patch_text) = obj
                    .get("patch")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                {
                    if !patch_text.is_empty() {
                        let filename = obj
                            .get("filename")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let additions = obj.get("additions").and_then(|v| v.as_u64()).unwrap_or(0);
                        let deletions = obj.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);
                        let total_lines = patch_text.matches('\n').count() + 1;
                        ensure_patch_cached(
                            obj,
                            &patch_text,
                            &filename,
                            additions,
                            deletions,
                            total_lines,
                            cache,
                        );
                    }
                }
                obj.remove("patch");
                obj.remove("patch_preview");
            }
        }
    }
}

/// Per-item budget: cap any single oversized item. Returns descriptions of what was capped.
fn enforce_per_item_limits(
    result: &mut Value,
    item_budget: usize,
    cache: &mut Option<Value>,
) -> Vec<String> {
    let mut actions: Vec<String> = Vec::new();

    // Cap body
    if let Some(body) = result.get("body").and_then(|v| v.as_str()).map(|s| s.len()) {
        if body > item_budget {
            truncate_body(result, item_budget, cache);
            actions.push("body truncated (over per-item limit)".into());
        }
    }

    // Cap individual patches
    let mut patches_capped = 0usize;
    if let Some(files) = result.get_mut("files").and_then(|v| v.as_array_mut()) {
        for f in files.iter_mut() {
            if let Some(obj) = f.as_object_mut() {
                let patch_len = obj
                    .get("patch")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                if patch_len > item_budget {
                    let patch_text = obj.remove("patch").unwrap();
                    let patch_str = patch_text.as_str().unwrap_or("");
                    let total_lines = patch_str.matches('\n').count() + 1;
                    let filename = obj
                        .get("filename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let additions = obj.get("additions").and_then(|v| v.as_u64()).unwrap_or(0);
                    let deletions = obj.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);

                    let eid = ensure_patch_cached(
                        obj,
                        patch_str,
                        &filename,
                        additions,
                        deletions,
                        total_lines,
                        cache,
                    );

                    let preview: String = patch_str
                        .split('\n')
                        .take(PATCH_INLINE_KEEP)
                        .collect::<Vec<_>>()
                        .join("\n");
                    obj.insert(
                        "patch_preview".to_string(),
                        Value::String(format!(
                            "{}\n\n... [{} more lines]",
                            preview,
                            total_lines.saturating_sub(PATCH_INLINE_KEEP)
                        )),
                    );
                    if let Some(eid) = eid {
                        obj.insert("patch_id".to_string(), Value::String(eid));
                    }
                    patches_capped += 1;
                }
            }
        }
    }
    if patches_capped > 0 {
        actions.push(format!(
            "{} large patch(es) collapsed (over per-item limit)",
            patches_capped
        ));
    }

    // Cap individual comments
    if let Some(comments) = result.get_mut("comments").and_then(|v| v.as_array_mut()) {
        for c in comments.iter_mut() {
            let body_len = c
                .get("body")
                .and_then(|v| v.as_str())
                .map(|s| s.len())
                .unwrap_or(0);
            if body_len > item_budget {
                truncate_comment(c, item_budget, cache);
            }
        }
    }

    actions
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Truncate a comment body in place, caching the original if truncated.
fn truncate_comment(c: &mut Value, limit: usize, cache: &mut Option<Value>) {
    if c.get("_truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return; // already truncated
    }
    let original_body = c
        .get("body")
        .and_then(|b| b.as_str())
        .unwrap_or("")
        .to_string();
    if original_body.len() <= limit {
        return;
    }
    let (compacted, truncated) = compact_text_mut(&original_body, limit, cache);
    c["body"] = Value::String(compacted);
    if truncated {
        c["_truncated"] = Value::Bool(true);
        if let Some(ref mut cv) = cache {
            let n = cv.get("_n").and_then(|v| v.as_u64()).unwrap_or(0);
            let eid = format!("comment_{}", n);
            cv[&eid] = serde_json::json!({
                "type": "comment",
                "author": c.get("author").and_then(|a| a.as_str()).unwrap_or(""),
                "total_lines": original_body.matches('\n').count() + 1,
                "content": original_body,
            });
            c["_element_id"] = Value::String(eid);
        }
    }
}

/// Ensure a patch is cached, returning the element_id. Checks if patch_id already set.
fn ensure_patch_cached(
    obj: &mut serde_json::Map<String, Value>,
    patch_text: &str,
    filename: &str,
    additions: u64,
    deletions: u64,
    total_lines: usize,
    cache: &mut Option<Value>,
) -> Option<String> {
    if let Some(existing) = obj.get("patch_id").and_then(|v| v.as_str()) {
        return Some(existing.to_string());
    }
    if let Some(ref mut c) = cache {
        let n = c.get("_n").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        c["_n"] = Value::from(n);
        let eid = format!("patch_{}", n);
        c[&eid] = serde_json::json!({
            "type": "patch",
            "filename": filename,
            "additions": additions,
            "deletions": deletions,
            "total_lines": total_lines,
            "content": patch_text,
        });
        Some(eid)
    } else {
        None
    }
}

/// Compact a single review inline comment into a preview entry.
fn compact_single_review_comment(
    ic: &Value,
    reviewer: &str,
    preview_lines: usize,
    preview_chars: usize,
    cache: &mut Option<Value>,
) -> Value {
    let body = ic.get("body").and_then(|b| b.as_str()).unwrap_or("");
    let preview: String = {
        let lines: Vec<&str> = body.split('\n').collect();
        let kept: String = lines[..lines.len().min(preview_lines)].join("\n");
        if kept.len() > preview_chars {
            let mut s: String = kept.chars().take(preview_chars).collect();
            s.push_str("...");
            s
        } else if lines.len() > preview_lines {
            format!("{}...", kept)
        } else {
            kept
        }
    };
    let replies = ic
        .get("replies")
        .and_then(|r| r.as_array())
        .map(|r| r.len())
        .unwrap_or(0);
    let path = ic.get("path").and_then(|p| p.as_str()).unwrap_or("");

    let eid = if let Some(ref mut c) = cache {
        let n = c.get("_n").and_then(|v| v.as_u64()).unwrap_or(0) + 1;
        c["_n"] = Value::from(n);
        let eid = format!("review_{}", n);
        c[&eid] = serde_json::json!({
            "type": "review_comment",
            "author": reviewer,
            "path": path,
            "line": ic.get("line"),
            "total_lines": body.matches('\n').count() + 1,
            "content": body,
            "replies": ic.get("replies"),
        });
        Some(eid)
    } else {
        None
    };

    let mut entry = serde_json::json!({
        "path": path,
        "line": ic.get("line"),
        "preview": preview,
        "replies": replies,
    });
    if let Some(eid) = eid {
        entry["_element_id"] = Value::String(eid);
    }
    entry
}

// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Thread digest for huge discussions
// ---------------------------------------------------------------------------

/// For discussions with many comments, replace the flat array with a digest:
/// first N + maintainer highlights from middle + last M, caching the full middle.
fn build_thread_digest(result: &mut Value, cache: &mut Option<Value>) -> String {
    let comments = match result.get_mut("comments").and_then(|v| v.as_array_mut()) {
        Some(c) => c,
        None => return String::new(),
    };

    let total = comments.len();
    let head = DIGEST_HEAD.min(total);
    let tail = DIGEST_TAIL.min(total.saturating_sub(head));
    let middle_start = head;
    let middle_end = total.saturating_sub(tail);

    if middle_start >= middle_end {
        return String::new(); // not enough comments for a meaningful digest
    }

    // Clone the full middle for caching before we mutate
    let middle_comments: Vec<Value> = comments[middle_start..middle_end].to_vec();
    let middle_count = middle_comments.len();

    // Cache individual middle comments and extract maintainer highlights
    let mut maintainer_highlights: Vec<Value> = Vec::new();
    let mut maintainer_total = 0usize;
    for (i, c) in middle_comments.iter().enumerate() {
        let eid = format!("comment_{}", middle_start + i);

        // Cache the full comment for drill-down
        if let Some(ref mut cache_obj) = cache {
            if let Some(obj) = cache_obj.as_object_mut() {
                let mut cached = c.clone();
                cached["_index"] = Value::Number((middle_start + i).into());
                obj.insert(
                    eid.clone(),
                    serde_json::json!({
                        "type": "comment",
                        "content": cached,
                    }),
                );
            }
        }

        let assoc = c
            .get("author_association")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if MAINTAINER_ROLES.contains(&assoc) {
            maintainer_total += 1;
            if maintainer_highlights.len() < DIGEST_MAINTAINER_MAX {
                let mut highlight = c.clone();
                // Truncate body for the inline preview
                if let Some(body) = highlight.get("body").and_then(|v| v.as_str()) {
                    if body.len() > DIGEST_MAINTAINER_CHARS {
                        let cut = safe_byte_index(body, DIGEST_MAINTAINER_CHARS);
                        highlight["body"] = Value::String(format!("{}…", &body[..cut]));
                    }
                }
                highlight["_element_id"] = Value::String(eid);
                maintainer_highlights.push(highlight);
            }
        }
    }

    // Date range for the middle
    let first_date = middle_comments
        .first()
        .and_then(|c| c.get("created_at"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let last_date = middle_comments
        .last()
        .and_then(|c| c.get("created_at"))
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    // Build the replacement array
    let head_comments: Vec<Value> = comments[..head].to_vec();
    let tail_comments: Vec<Value> = comments[total - tail..].to_vec();

    let mut digest: Vec<Value> = Vec::new();
    digest.extend(head_comments);

    // System note about the gap
    let gap_msg = format!(
        "--- {middle_count} comments omitted ({maintainer_total} from maintainers). \
         Date range: {first_date} to {last_date}. \
         Use element_id='comments_middle' with grep='pattern' to search. ---"
    );
    digest.push(serde_json::json!({
        "author": "[system]",
        "body": gap_msg,
    }));

    // Maintainer highlights
    if !maintainer_highlights.is_empty() {
        digest.extend(maintainer_highlights);
        digest.push(serde_json::json!({
            "author": "[system]",
            "body": "--- end maintainer highlights, recent comments follow ---",
        }));
    }

    digest.extend(tail_comments);

    // Cache the full middle for drill-down (with _index for grep metadata)
    if let Some(ref mut cache_obj) = cache {
        if let Some(obj) = cache_obj.as_object_mut() {
            let indexed: Vec<Value> = middle_comments
                .into_iter()
                .enumerate()
                .map(|(i, mut c)| {
                    c["_index"] = Value::Number((middle_start + i).into());
                    c
                })
                .collect();
            obj.insert(
                "comments_middle".to_string(),
                serde_json::json!({
                    "type": "comment_segment",
                    "label": "middle",
                    "comment_count": middle_count,
                    "content": Value::Array(indexed),
                }),
            );
        }
    }

    // Replace comments array
    *comments = digest;

    let comment_count = result
        .get("comment_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(total as u64);

    format!(
        "thread digest ({} total comments, {} shown inline, {} maintainer highlights)",
        comment_count,
        head + tail,
        maintainer_total.min(DIGEST_MAINTAINER_MAX),
    )
}

// ---------------------------------------------------------------------------
// Main adaptive compaction function
// ---------------------------------------------------------------------------

/// Compact a discussion JSON, using budget-based adaptive compaction.
///
/// Starts with full content. If over budget, iteratively applies compaction
/// tiers (lowest-value first) until the output fits.
fn compact_discussion_internal(
    result: &mut Value,
    cache: &mut Option<Value>,
    budget: usize,
    item_budget: usize,
) -> Vec<String> {
    let effective_budget = (budget as f64 * BUDGET_MARGIN) as usize;

    // --- Tier 0: Always-applied cleanup ---
    let mut compacted_sections: Vec<String> = Vec::new();

    // 0a: Filter bot comments
    let bot_count = filter_bot_comments(result);
    if bot_count > 0 {
        compacted_sections.push(format!("{} bot comments filtered", bot_count));
    }

    // 0b: Collapse <details> blocks in body
    collapse_body_code_blocks(result, cache);

    // --- Thread digest for huge discussions (proactive, before budget check) ---
    let comment_count = result
        .get("comments")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    if comment_count > HUGE_THREAD_THRESHOLD {
        let digest_desc = build_thread_digest(result, cache);
        if !digest_desc.is_empty() {
            compacted_sections.push(digest_desc);
        }
    }

    // --- Per-item budget enforcement (pre-pass) ---
    let item_actions = enforce_per_item_limits(result, item_budget, cache);
    compacted_sections.extend(item_actions);

    // --- Check if under budget ---
    let mut size = estimate_size(result);
    if size <= effective_budget {
        // Everything fits! Cache patches for drill-down even though they're inline.
        cache_all_patches(result, cache);
        if !compacted_sections.is_empty() {
            result["_compaction"] = Value::String(format!(
                "{}. Use element_id to drill down.",
                compacted_sections.join("; ")
            ));
        }
        return compacted_sections;
    }

    // --- Iterative tier compaction ---
    let mut tier_reached: u8 = 0;

    // Tier 1: Code blocks in comments
    if size > effective_budget {
        tier_reached = 1;
        collapse_comment_code_blocks(result, cache);
        compacted_sections.push("code blocks collapsed".into());
        size = estimate_size(result);
    }

    // Tier 2: Non-maintainer comments truncated
    if size > effective_budget {
        tier_reached = 2;
        truncate_non_maintainer_comments(result, COMMENT_PREVIEW_CHARS, cache);
        compacted_sections.push("non-maintainer comments truncated".into());
        size = estimate_size(result);
    }

    // Tier 3: Large patches (>80 lines) collapsed
    if size > effective_budget {
        tier_reached = 3;
        collapse_patches_over(result, PATCH_INLINE_MAX_LINES, PATCH_INLINE_KEEP, cache);
        compacted_sections.push("large patches (>80 lines) collapsed".into());
        size = estimate_size(result);
    }

    // Tier 4: Maintainer comments truncated
    if size > effective_budget {
        tier_reached = 4;
        truncate_maintainer_comments(result, MAINTAINER_LIMIT, cache);
        compacted_sections.push("maintainer comments truncated".into());
        size = estimate_size(result);
    }

    // Tier 5: Medium patches (>30 lines) collapsed
    if size > effective_budget {
        tier_reached = 5;
        collapse_patches_over(result, TIER5_PATCH_MAX_LINES, TIER5_PATCH_KEEP, cache);
        compacted_sections.push("medium patches (>30 lines) collapsed".into());
        size = estimate_size(result);
    }

    // Tier 6: PR body truncated
    if size > effective_budget {
        tier_reached = 6;
        truncate_body(result, TIER6_BODY_LIMIT, cache);
        compacted_sections.push("PR body truncated".into());
        size = estimate_size(result);
    }

    // Tier 7: Review inline comments compacted
    if size > effective_budget {
        tier_reached = 7;
        compact_reviews(result, REVIEW_PREVIEW_LINES, REVIEW_PREVIEW_CHARS, cache);
        compacted_sections.push("review comments compacted".into());
        size = estimate_size(result);
    }

    // Tier 8: All patches → summary only
    if size > effective_budget {
        tier_reached = 8;
        remove_inline_patches(result, cache);
        compacted_sections.push("all patches removed (use patch_id to drill down)".into());
        size = estimate_size(result);
    }

    // Tier 9: Aggressive truncation
    if size > effective_budget {
        tier_reached = 9;
        truncate_body(result, TIER9_BODY_LIMIT, cache);
        truncate_non_maintainer_comments(result, TIER9_COMMENT_LIMIT, cache);
        truncate_maintainer_comments(result, TIER9_COMMENT_LIMIT, cache);
        compact_reviews(result, 1, TIER9_REVIEW_CHARS, cache);
        compacted_sections.push("aggressive compaction applied".into());
        let _ = estimate_size(result);
    }

    // Cache any patches still inline (they survived compaction)
    cache_all_patches(result, cache);

    // Add compaction info
    if tier_reached > 0 {
        result["_compaction"] = Value::String(format!(
            "Budget compaction (tier {}). {}. Use element_id to drill down.",
            tier_reached,
            compacted_sections.join("; ")
        ));
    }

    compacted_sections
}

/// Cache all inline patches that don't have a patch_id yet (for drill-down support).
fn cache_all_patches(result: &mut Value, cache: &mut Option<Value>) {
    if let Some(files) = result.get_mut("files").and_then(|v| v.as_array_mut()) {
        for f in files.iter_mut() {
            if let Some(obj) = f.as_object_mut() {
                if obj.contains_key("patch_id") {
                    continue;
                }
                if let Some(patch_text) = obj
                    .get("patch")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                {
                    if patch_text.is_empty() {
                        continue;
                    }
                    let filename = obj
                        .get("filename")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let additions = obj.get("additions").and_then(|v| v.as_u64()).unwrap_or(0);
                    let deletions = obj.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);
                    let total_lines = patch_text.matches('\n').count() + 1;
                    let eid = ensure_patch_cached(
                        obj,
                        &patch_text,
                        &filename,
                        additions,
                        deletions,
                        total_lines,
                        cache,
                    );
                    if let Some(eid) = eid {
                        obj.insert("patch_id".to_string(), Value::String(eid));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PyO3 wrappers (serialize/deserialize cache at boundary)
// ---------------------------------------------------------------------------

/// Collapse large fenced code blocks and <details> sections in text.
///
/// When cache_json is provided (a JSON object string), collapsed elements are stored with IDs.
/// Returns (collapsed_text, updated_cache_json).
#[pyfunction]
#[pyo3(signature = (text, cache_json = None))]
pub fn collapse_code_blocks(text: &str, cache_json: Option<&str>) -> (String, Option<String>) {
    let mut cache: Option<Value> = cache_json.and_then(|s| serde_json::from_str(s).ok());
    let result = collapse_code_blocks_mut(text, &mut cache);
    let cache_out = cache.map(|c| serde_json::to_string(&c).unwrap_or_default());
    (result, cache_out)
}

/// Collapse code blocks then truncate if over limit.
/// Returns (text, was_truncated, cache_json).
#[pyfunction]
#[pyo3(signature = (text, limit, cache_json = None))]
pub fn compact_text(
    text: &str,
    limit: usize,
    cache_json: Option<&str>,
) -> (String, bool, Option<String>) {
    let mut cache: Option<Value> = cache_json.and_then(|s| serde_json::from_str(s).ok());
    let (result, truncated) = compact_text_mut(text, limit, &mut cache);
    let cache_out = cache.map(|c| serde_json::to_string(&c).unwrap_or_default());
    (result, truncated, cache_out)
}

/// Compact a discussion JSON string using budget-based adaptive compaction.
/// Returns (compacted_json, cache_json).
///
/// budget/item_budget control output size limits (defaults: 60KB / 15KB).
#[pyfunction]
#[pyo3(signature = (discussion_json, cache_json = None, budget = None, item_budget = None))]
pub fn compact_discussion(
    discussion_json: &str,
    cache_json: Option<&str>,
    budget: Option<usize>,
    item_budget: Option<usize>,
) -> PyResult<(String, Option<String>)> {
    let mut result: Value = serde_json::from_str(discussion_json)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid JSON: {}", e)))?;

    let mut cache: Option<Value> = cache_json.and_then(|s| serde_json::from_str(s).ok());

    let budget = budget.unwrap_or(DEFAULT_BUDGET);
    let item_budget = item_budget.unwrap_or(DEFAULT_ITEM_BUDGET);

    compact_discussion_internal(&mut result, &mut cache, budget, item_budget);

    let out = serde_json::to_string_pretty(&result).unwrap_or_default();
    let cache_out = cache.map(|c| serde_json::to_string(&c).unwrap_or_default());
    Ok((out, cache_out))
}
