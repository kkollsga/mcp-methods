use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use regex::Regex;
use serde_json::Value;

/// Walk a parsed JSON structure, grep within string values.
/// Returns a list of match dicts with field, lines, context_start, context_end, content.
#[pyfunction]
#[pyo3(signature = (json_str, pattern, context))]
pub fn grep_json_fields(
    py: Python<'_>,
    json_str: &str,
    pattern: &str,
    context: usize,
) -> PyResult<Py<PyAny>> {
    let regex = match Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "Invalid regex: {}",
                e
            )));
        }
    };

    let data: Value = serde_json::from_str(json_str)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("Invalid JSON: {}", e)))?;

    let matches = grep_value(&data, &regex, context, "");
    let result = PyList::empty(py);

    for m in matches {
        let dict = PyDict::new(py);
        dict.set_item("field", m.field)?;
        let lines_list = PyList::new(py, &m.lines)?;
        dict.set_item("lines", lines_list)?;
        dict.set_item("context_start", m.context_start)?;
        dict.set_item("context_end", m.context_end)?;
        dict.set_item("content", m.content)?;
        result.append(dict)?;
    }

    Ok(result.into_any().unbind())
}

struct GrepMatch {
    field: String,
    lines: Vec<usize>,
    context_start: usize,
    context_end: usize,
    content: String,
}

fn grep_value(data: &Value, regex: &Regex, context: usize, path: &str) -> Vec<GrepMatch> {
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
) -> Vec<GrepMatch> {
    let mut raw: Vec<(usize, usize, usize)> = Vec::new();
    for (idx, line) in text_lines.iter().enumerate() {
        if regex.is_match(line) {
            let start = idx.saturating_sub(context);
            let end = (idx + context + 1).min(text_lines.len());
            raw.push((idx + 1, start, end));
        }
    }

    // Merge overlapping windows
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
            GrepMatch {
                field: field.to_string(),
                lines: g.lines,
                context_start: g.start + 1,
                context_end: g.end,
                content,
            }
        })
        .collect()
}
