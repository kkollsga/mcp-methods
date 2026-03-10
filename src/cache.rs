use pyo3::prelude::*;
use regex::Regex;
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::LazyLock;

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

static LINE_RANGE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)-(\d+)$").unwrap());

use crate::compact;
use crate::github;

/// Element cache for storing collapsed discussion elements (code blocks,
/// details sections, truncated comments, overflow).
///
/// Lives entirely in Rust memory. Python holds a reference to it.
#[pyclass]
pub struct ElementCache {
    // (repo, number) -> {element_id -> element_data_json}
    store: HashMap<(String, u64), HashMap<String, Value>>,
}

#[pymethods]
impl ElementCache {
    #[new]
    pub fn new() -> Self {
        Self {
            store: HashMap::new(),
        }
    }

    /// Get a cached element as a JSON string. Returns None if not found.
    pub fn get(&self, repo: &str, number: u64, element_id: &str) -> Option<String> {
        self.store
            .get(&(repo.to_string(), number))
            .and_then(|m| m.get(element_id))
            .map(|v| serde_json::to_string(v).unwrap_or_default())
    }

    /// Store elements for a repo/number, replacing any existing ones.
    pub fn store_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        if let Ok(val) = serde_json::from_str::<Value>(elements_json) {
            if let Some(obj) = val.as_object() {
                let mut map = HashMap::new();
                for (k, v) in obj {
                    if !k.starts_with('_') {
                        map.insert(k.clone(), v.clone());
                    }
                }
                self.store.insert((repo.to_string(), number), map);
            }
        }
    }

    /// Add elements to an existing cache entry (merge).
    pub fn update_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        if let Ok(val) = serde_json::from_str::<Value>(elements_json) {
            if let Some(obj) = val.as_object() {
                let entry = self.store.entry((repo.to_string(), number)).or_default();
                for (k, v) in obj {
                    if !k.starts_with('_') {
                        entry.insert(k.clone(), v.clone());
                    }
                }
            }
        }
    }

    /// List available element IDs for a repo/number.
    pub fn available(&self, repo: &str, number: u64) -> Vec<String> {
        match self.store.get(&(repo.to_string(), number)) {
            Some(m) => {
                let mut keys: Vec<String> = m.keys().cloned().collect();
                keys.sort();
                keys
            }
            None => Vec::new(),
        }
    }

    /// Retrieve a cached element with optional line slicing or grep.
    ///
    /// This is the main drill-down method. Returns a JSON string.
    #[pyo3(signature = (repo, number, element_id, lines=None, grep=None, context=3))]
    pub fn retrieve(
        &self,
        repo: &str,
        number: u64,
        element_id: &str,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
    ) -> String {
        let elem_data = match self
            .store
            .get(&(repo.to_string(), number))
            .and_then(|m| m.get(element_id))
        {
            Some(v) => v,
            None => {
                let available = self.available(repo, number);
                let mut msg = format!(
                    "Element '{}' not found for {}#{}.",
                    element_id, repo, number
                );
                if !available.is_empty() {
                    msg.push_str(&format!("\nAvailable: {}", available.join(", ")));
                } else {
                    msg.push_str("\nNo cached elements. Call git_issue first.");
                }
                return msg;
            }
        };

        let content = elem_data
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let content_lines: Vec<&str> = content.split('\n').collect();

        // Grep mode
        if let Some(grep_pattern) = grep {
            let regex = match get_or_compile_regex(grep_pattern) {
                Ok(r) => r,
                Err(e) => return format!("Invalid grep pattern: {}", e),
            };

            // Overflow elements: field-aware grep through parsed JSON values
            if elem_data.get("type").and_then(|v| v.as_str()) == Some("overflow") {
                if let Ok(data) = serde_json::from_str::<Value>(content) {
                    let matches = grep_json_value(&data, &regex, context, "");
                    if !matches.is_empty() {
                        let result = serde_json::json!({
                            "element_id": element_id,
                            "type": "overflow",
                            "grep": grep_pattern,
                            "matches": matches,
                        });
                        return serde_json::to_string_pretty(&result).unwrap_or_default();
                    }
                }
            }

            // Standard elements: line-based grep — build result without cloning
            let matches = grep_lines_internal(&content_lines, &regex, context);
            let mut result = serde_json::Map::new();
            if let Some(obj) = elem_data.as_object() {
                for (k, v) in obj {
                    if k != "content" {
                        result.insert(k.clone(), v.clone());
                    }
                }
            }
            result.insert("grep".to_string(), Value::String(grep_pattern.to_string()));
            result.insert("matches".to_string(), matches);
            return serde_json::to_string_pretty(&Value::Object(result)).unwrap_or_default();
        }

        // Lines mode — build result with sliced content
        if let Some(lines_str) = lines {
            let m = match LINE_RANGE_RE.captures(lines_str) {
                Some(m) => m,
                None => {
                    return format!(
                        "Invalid lines format: '{}'. Use 'start-end', e.g. '40-60'.",
                        lines_str
                    );
                }
            };
            let start: usize = m[1].parse().unwrap_or(1);
            let end: usize = m[2].parse().unwrap_or(content_lines.len());
            let selected: Vec<&str> =
                content_lines[start.saturating_sub(1)..end.min(content_lines.len())].to_vec();
            let mut result = serde_json::Map::new();
            if let Some(obj) = elem_data.as_object() {
                for (k, v) in obj {
                    if k != "content" {
                        result.insert(k.clone(), v.clone());
                    }
                }
            }
            result.insert("content".to_string(), Value::String(selected.join("\n")));
            result.insert(
                "lines_shown".to_string(),
                Value::String(format!("{}-{}", start, end.min(content_lines.len()))),
            );
            return serde_json::to_string_pretty(&Value::Object(result)).unwrap_or_default();
        }

        // Full content
        serde_json::to_string_pretty(elem_data).unwrap_or_default()
    }

    /// Compact a discussion dict and store cache entries.
    ///
    /// Takes discussion as JSON string, returns compacted JSON string.
    /// Cache entries are stored directly in this cache object.
    #[pyo3(signature = (repo, number, discussion_json, expand))]
    pub fn compact_and_store(
        &mut self,
        repo: &str,
        number: u64,
        discussion_json: &str,
        expand: Vec<String>,
    ) -> PyResult<String> {
        let cache_json = serde_json::to_string(&serde_json::json!({"_n": 0})).unwrap();
        let (compacted_json, cache_out) =
            compact::compact_discussion(discussion_json, expand, Some(&cache_json))?;

        // Extract and store cache entries
        if let Some(ref cache_str) = cache_out {
            self.store_elements(repo, number, cache_str);
        }

        Ok(compacted_json)
    }

    /// Fetch a GitHub issue/PR, compact it, and store cache entries.
    ///
    /// Releases the GIL during all HTTP and computation. This is the primary
    /// entry point for fetching discussions with caching.
    #[pyo3(signature = (repo, number, *, expand=None, element_id=None, lines=None, grep=None, context=3))]
    #[allow(clippy::too_many_arguments)]
    pub fn fetch_discussion(
        &mut self,
        repo: &str,
        number: u64,
        expand: Option<Vec<String>>,
        element_id: Option<&str>,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
    ) -> PyResult<String> {
        // Element retrieval — no network, fast
        if let Some(eid) = element_id {
            return Ok(self.retrieve(repo, number, eid, lines, grep, context));
        }

        // Validate repo
        if let Some(err) = crate::git_refs::validate_repo(repo) {
            return Ok(err);
        }

        // All HTTP + computation runs in Rust; parallel requests use std::thread::scope
        let expand_list = expand.unwrap_or_default();
        let (text, cache_json) = match github::fetch_issue_internal(repo, number, &expand_list) {
            Ok(r) => r,
            Err(e) => return Ok(e),
        };

        // Store cache entries (GIL held, &mut self available)
        if let Some(ref cj) = cache_json {
            self.store_elements(repo, number, cj);
        }

        // Overflow guard
        if text.len() > github::OVERFLOW_LIMIT {
            let total_lines = text.matches('\n').count() + 1;
            let overflow = serde_json::json!({
                "overflow": {
                    "type": "overflow",
                    "total_chars": text.len(),
                    "total_lines": total_lines,
                    "content": text,
                }
            });
            self.update_elements(
                repo,
                number,
                &serde_json::to_string(&overflow).unwrap_or_default(),
            );
            let mut preview = text[..github::OVERFLOW_PREVIEW.min(text.len())].to_string();
            if let Some(last_nl) = preview.rfind('\n') {
                if last_nl > 0 {
                    preview.truncate(last_nl);
                }
            }
            preview.push_str(&format!(
                "\n\n... [{} chars, {} lines — truncated]\n\
                 Use element_id='overflow' with lines='N-M' or grep='pattern' \
                 to explore the full result.",
                text.len(),
                total_lines
            ));
            return Ok(preview);
        }

        Ok(text)
    }
}

// --- Internal grep helpers (no PyO3, pure Rust) ---

fn grep_lines_internal(text_lines: &[&str], regex: &Regex, context: usize) -> Value {
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

    let result: Vec<Value> = groups
        .into_iter()
        .map(|g| {
            let content = text_lines[g.start..g.end].join("\n");
            serde_json::json!({
                "lines": g.lines,
                "context_start": g.start + 1,
                "context_end": g.end,
                "content": content,
            })
        })
        .collect();

    Value::Array(result)
}

fn grep_json_value(data: &Value, regex: &Regex, context: usize, path: &str) -> Vec<Value> {
    match data {
        Value::String(s) => {
            let text = s.replace("\r\n", "\n");
            let text_lines: Vec<&str> = text.split('\n').collect();
            let matches = grep_lines_internal(&text_lines, regex, context);
            if let Value::Array(arr) = matches {
                arr.into_iter()
                    .map(|mut m| {
                        m["field"] = Value::String(path.to_string());
                        m
                    })
                    .collect()
            } else {
                Vec::new()
            }
        }
        Value::Object(map) => {
            let mut matches = Vec::new();
            for (key, val) in map {
                let child = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{}.{}", path, key)
                };
                matches.extend(grep_json_value(val, regex, context, &child));
            }
            matches
        }
        Value::Array(arr) => {
            let mut matches = Vec::new();
            for (i, item) in arr.iter().enumerate() {
                let child = format!("{}[{}]", path, i);
                matches.extend(grep_json_value(item, regex, context, &child));
            }
            matches
        }
        _ => Vec::new(),
    }
}
