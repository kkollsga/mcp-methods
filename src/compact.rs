use pyo3::prelude::*;
use regex::Regex;
use serde_json::Value;
use std::collections::HashSet;
use std::sync::LazyLock;

static SUMMARY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</?summary[^>]*>").unwrap());
static LANG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^```(\w*)").unwrap());

// Compaction constants
const CODE_BLOCK_MAX_LINES: usize = 20;
const CODE_BLOCK_KEEP: usize = 5;
const BODY_LIMIT: usize = 10_000;
const MAINTAINER_LIMIT: usize = 5_000;
const COMMENT_PREVIEW_CHARS: usize = 500;
const REVIEW_PREVIEW_LINES: usize = 3;
const REVIEW_PREVIEW_CHARS: usize = 300;
/// Total patch lines below which diffs are shown inline (with per-file collapsing).
const SMALL_DIFF_THRESHOLD: usize = 200;
/// Individual patch collapse: patches above this are collapsed even in small diffs.
const PATCH_INLINE_MAX_LINES: usize = 80;
/// How many lines to keep as preview when collapsing an inline patch.
const PATCH_INLINE_KEEP: usize = 20;

const MAINTAINER_ROLES: &[&str] = &["OWNER", "MEMBER", "COLLABORATOR"];

// ---------------------------------------------------------------------------
// Internal functions (work with &mut Option<Value> to avoid JSON round-trips)
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
        if stripped.len() >= 8 && stripped[..8].eq_ignore_ascii_case("<details") {
            let mut j = i + 1;
            let mut summary = String::new();
            while j < lines.len() {
                let s = lines[j].trim();
                if summary.is_empty() && s.len() >= 8 && s[..8].eq_ignore_ascii_case("<summary") {
                    summary = SUMMARY_RE.replace_all(s, "").trim().to_string();
                }
                if s.len() >= 9 && s[..9].eq_ignore_ascii_case("</details") {
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
        let truncated = format!("{}…[truncated]", &collapsed[..limit]);
        (truncated, true)
    } else {
        (collapsed, false)
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

/// Compact a discussion JSON string. Returns (compacted_json, cache_json).
///
/// expand is a list of section names to keep in full:
/// - "body", "comments", "patches", "review:<author>", "all"
#[pyfunction]
#[pyo3(signature = (discussion_json, expand, cache_json = None))]
pub fn compact_discussion(
    discussion_json: &str,
    expand: Vec<String>,
    cache_json: Option<&str>,
) -> PyResult<(String, Option<String>)> {
    let mut result: Value = serde_json::from_str(discussion_json)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid JSON: {}", e)))?;

    let expand_set: HashSet<String> = expand.into_iter().collect();

    if expand_set.contains("all") {
        return Ok((
            serde_json::to_string_pretty(&result).unwrap_or_default(),
            cache_json.map(|s| s.to_string()),
        ));
    }

    let mut cache: Option<Value> = cache_json.and_then(|s| serde_json::from_str(s).ok());

    // Compact body (direct mut cache — no JSON round-trip)
    if !expand_set.contains("body") {
        if let Some(body) = result
            .get("body")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
        {
            let (compacted, truncated) = compact_text_mut(&body, BODY_LIMIT, &mut cache);
            result["body"] = Value::String(compacted);
            if truncated {
                result["_body_truncated"] = Value::Bool(true);
            }
        }
    }

    // Filter bot comments
    if !expand_set.contains("comments") {
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
        }
    }

    // Collapse and truncate comments (direct mut cache — no JSON round-trip per comment)
    if !expand_set.contains("comments") {
        if let Some(comments) = result.get_mut("comments").and_then(|v| v.as_array_mut()) {
            for c in comments.iter_mut() {
                let is_maintainer = c
                    .get("author_association")
                    .and_then(|a| a.as_str())
                    .map(|a| MAINTAINER_ROLES.contains(&a))
                    .unwrap_or(false);
                let limit = if is_maintainer {
                    MAINTAINER_LIMIT
                } else {
                    COMMENT_PREVIEW_CHARS
                };
                let original_body = c
                    .get("body")
                    .and_then(|b| b.as_str())
                    .unwrap_or("")
                    .to_string();
                let (compacted, truncated) = compact_text_mut(&original_body, limit, &mut cache);
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
        }
    }

    // Collapse patches — smart sizing based on total diff size
    if !expand_set.contains("patches") {
        if let Some(files) = result.get_mut("files").and_then(|v| v.as_array_mut()) {
            // Calculate total diff lines to decide strategy
            let total_patch_lines: usize = files
                .iter()
                .filter_map(|f| {
                    f.get("patch")
                        .and_then(|p| p.as_str())
                        .map(|s| s.matches('\n').count() + 1)
                })
                .sum();

            let is_small_diff = total_patch_lines <= SMALL_DIFF_THRESHOLD;

            for f in files.iter_mut() {
                if let Some(obj) = f.as_object_mut() {
                    let patch = obj.remove("patch");
                    if let Some(Value::String(patch_text)) = patch {
                        if patch_text.is_empty() {
                            continue;
                        }
                        let total_lines = patch_text.matches('\n').count() + 1;
                        let filename = obj.get("filename").and_then(|v| v.as_str()).unwrap_or("");
                        let additions = obj.get("additions").and_then(|v| v.as_u64()).unwrap_or(0);
                        let deletions = obj.get("deletions").and_then(|v| v.as_u64()).unwrap_or(0);

                        // Always cache the full patch for drill-down
                        let eid = if let Some(ref mut c) = cache {
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
                        };

                        if is_small_diff {
                            // Small diff: show patch inline, collapse if individually large
                            if total_lines <= PATCH_INLINE_MAX_LINES {
                                obj.insert("patch".to_string(), Value::String(patch_text));
                            } else {
                                // Collapse large individual patch: show preview
                                let preview: String = patch_text
                                    .split('\n')
                                    .take(PATCH_INLINE_KEEP)
                                    .collect::<Vec<_>>()
                                    .join("\n");
                                obj.insert(
                                    "patch_preview".to_string(),
                                    Value::String(format!(
                                        "{}\n\n... [{} more lines]",
                                        preview,
                                        total_lines - PATCH_INLINE_KEEP
                                    )),
                                );
                            }
                        }

                        // Always set patch_id for drill-down
                        if let Some(eid) = eid {
                            obj.insert("patch_id".to_string(), Value::String(eid));
                        }
                    }
                }
            }
        }
    }

    // Compact inline review comments — longer previews, cache as review_N elements
    if let Some(reviews) = result.get_mut("reviews").and_then(|v| v.as_array_mut()) {
        for review in reviews.iter_mut() {
            let reviewer = review
                .get("author")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string();
            if expand_set.contains(&format!("review:{}", reviewer)) {
                continue;
            }
            if let Some(inlines) = review
                .get("inline_comments")
                .and_then(|v| v.as_array())
                .cloned()
            {
                if !inlines.is_empty() {
                    let compacted: Vec<Value> = inlines
                        .iter()
                        .map(|ic| {
                            let body = ic.get("body").and_then(|b| b.as_str()).unwrap_or("");
                            // Take first REVIEW_PREVIEW_LINES lines, up to REVIEW_PREVIEW_CHARS
                            let preview: String = {
                                let lines: Vec<&str> = body.split('\n').collect();
                                let kept: String =
                                    lines[..lines.len().min(REVIEW_PREVIEW_LINES)].join("\n");
                                if kept.len() > REVIEW_PREVIEW_CHARS {
                                    let mut s: String =
                                        kept.chars().take(REVIEW_PREVIEW_CHARS).collect();
                                    s.push_str("...");
                                    s
                                } else if lines.len() > REVIEW_PREVIEW_LINES {
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

                            // Cache full comment as review_N element
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
                        })
                        .collect();
                    review["inline_comments"] = Value::Array(compacted);
                }
            }
        }
    }

    // Add expand hints
    let mut hints: Vec<String> = Vec::new();
    if result
        .get("_body_truncated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        hints.push("body".to_string());
    }
    let has_truncated_comments = result
        .get("comments")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|c| {
                c.get("_truncated")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let has_hidden_bots = result
        .get("_bot_comments_hidden")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        > 0;
    if has_truncated_comments || has_hidden_bots {
        hints.push("comments".to_string());
    }
    if result
        .get("files")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false)
    {
        hints.push("patches".to_string());
    }
    if let Some(reviews) = result.get("reviews").and_then(|v| v.as_array()) {
        for r in reviews {
            if r.get("inline_comments")
                .and_then(|v| v.as_array())
                .map(|a| !a.is_empty())
                .unwrap_or(false)
            {
                if let Some(author) = r.get("author").and_then(|a| a.as_str()) {
                    hints.push(format!("review:{}", author));
                }
            }
        }
    }
    if !hints.is_empty() {
        let hint_str = hints
            .iter()
            .map(|h| format!("'{}'", h))
            .collect::<Vec<_>>()
            .join(", ");
        result["_expand"] = Value::String(format!(
            "Compact view. expand=[{}] for full content, or expand=['all'].",
            hint_str
        ));
    }

    let out = serde_json::to_string_pretty(&result).unwrap_or_default();
    let cache_out = cache.map(|c| serde_json::to_string(&c).unwrap_or_default());
    Ok((out, cache_out))
}
