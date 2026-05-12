//! Walk a parsed JSON structure, grep within string values.
//!
//! Pure Rust. The Python wrapper in `mcp-methods-py` converts the
//! returned `Vec<JsonGrepMatch>` into a Python list of dicts.

use regex::Regex;
use serde_json::Value;
use std::cell::RefCell;

thread_local! {
    static CACHED_RE: RefCell<Option<(String, Regex)>> = const { RefCell::new(None) };
}

fn get_or_compile_regex(pattern: &str) -> Result<Regex, regex::Error> {
    CACHED_RE.with(|cell| {
        let mut cache = cell.borrow_mut();
        if let Some((ref cached_pat, ref re)) = *cache {
            if cached_pat == pattern {
                return Ok(re.clone());
            }
        }
        let re = Regex::new(pattern)?;
        *cache = Some((pattern.to_string(), re.clone()));
        Ok(re)
    })
}

/// One grep match within a JSON structure.
#[derive(Debug, Clone)]
pub struct JsonGrepMatch {
    /// Dotted JSON path to the field where the match occurred.
    pub field: String,
    /// 1-indexed line numbers of matching lines within the field's value.
    pub lines: Vec<usize>,
    pub context_start: usize,
    pub context_end: usize,
    pub content: String,
}

/// Grep within string values of a parsed JSON structure. Returns one
/// match per merged context window per field.
pub fn ripgrep_json_fields(
    json_str: &str,
    pattern: &str,
    context: usize,
) -> Result<Vec<JsonGrepMatch>, String> {
    let regex = get_or_compile_regex(pattern).map_err(|e| format!("Invalid regex: {}", e))?;
    let data: Value = serde_json::from_str(json_str).map_err(|e| format!("Invalid JSON: {}", e))?;
    Ok(grep_value(&data, &regex, context, ""))
}

fn grep_value(data: &Value, regex: &Regex, context: usize, path: &str) -> Vec<JsonGrepMatch> {
    match data {
        Value::String(s) => {
            let text = s.replace("\r\n", "\n");
            let text_lines: Vec<&str> = text.split('\n').collect();
            grep_lines_internal(&text_lines, regex, context, path)
        }
        Value::Object(map) => {
            let mut matches = Vec::new();
            for (key, val) in map {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                matches.extend(grep_value(val, regex, context, &child));
            }
            matches
        }
        Value::Array(arr) => {
            let mut matches = Vec::new();
            for (i, item) in arr.iter().enumerate() {
                let child = format!("{}[{}]", path, i);
                matches.extend(grep_value(item, regex, context, &child));
            }
            matches
        }
        _ => Vec::new(),
    }
}

fn grep_lines_internal(
    text_lines: &[&str],
    regex: &Regex,
    context: usize,
    field: &str,
) -> Vec<JsonGrepMatch> {
    let mut raw: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, line) in text_lines.iter().enumerate() {
        if regex.is_match(line) {
            let start = idx.saturating_sub(context);
            let end = (idx + context + 1).min(text_lines.len());
            raw.push((idx + 1, start, end));
        }
    }

    struct Group {
        lines: Vec<usize>,
        start: usize,
        end: usize,
    }
    let mut groups: Vec<Group> = Vec::new();
    for (hit_line, start, end) in raw {
        if let Some(last) = groups.last_mut() {
            if start <= last.end {
                last.lines.push(hit_line);
                last.end = last.end.max(end);
                continue;
            }
        }
        groups.push(Group {
            lines: vec![hit_line],
            start,
            end,
        });
    }

    groups
        .into_iter()
        .map(|g| {
            let content = text_lines[g.start..g.end].join("\n");
            JsonGrepMatch {
                field: field.to_string(),
                lines: g.lines,
                context_start: g.start + 1,
                context_end: g.end,
                content,
            }
        })
        .collect()
}
