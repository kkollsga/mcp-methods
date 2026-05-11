#[cfg(feature = "python")]
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
/// Lives entirely in Rust memory. Python holds a reference to it when
/// the `python` feature is enabled; pure-Rust consumers (e.g.
/// mcp-server) use it directly as a normal struct.
#[cfg_attr(feature = "python", pyclass)]
pub struct ElementCache {
    // (repo, number) -> {element_id -> element_data_json}
    store: HashMap<(String, u64), HashMap<String, Value>>,
}

impl Default for ElementCache {
    fn default() -> Self {
        Self::new()
    }
}

// Plain Rust impl — all the actual logic, callable from any Rust crate
// regardless of feature flags. The `#[pymethods]` impl below is a thin
// delegating layer added only with the `python` feature.
//
// Two impl blocks (rather than one with cfg_attr) because pyo3's
// `#[new]` / `#[pyo3(signature = ...)]` sub-attributes can't be
// produced via `cfg_attr` in pyo3 0.28 — they're recognised only when
// `#[pymethods]` is present at parse time, not after attribute
// resolution.
impl ElementCache {
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
                    msg.push_str("\nNo cached elements. Call fetch_issue first.");
                }
                return msg;
            }
        };

        // Content can be a string or a structured JSON value (array/object)
        let content_val = elem_data.get("content");
        let content_str: String;
        let content_is_structured;

        match content_val {
            Some(Value::String(s)) => {
                content_str = s.clone();
                content_is_structured = false;
            }
            Some(val) => {
                content_str = serde_json::to_string_pretty(val).unwrap_or_default();
                content_is_structured = true;
            }
            None => {
                content_str = String::new();
                content_is_structured = false;
            }
        }
        let content_lines: Vec<&str> = content_str.split('\n').collect();

        // Grep mode
        if let Some(grep_pattern) = grep {
            let regex = match get_or_compile_regex(grep_pattern) {
                Ok(r) => r,
                Err(e) => return format!("Invalid grep pattern: {}", e),
            };

            // Structured content (overflow, comment segments): field-aware grep
            if content_is_structured {
                if let Some(data) = content_val {
                    let matches = grep_json_value(data, &regex, context, "");
                    let elem_type = elem_data
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    let result = serde_json::json!({
                        "element_id": element_id,
                        "type": elem_type,
                        "grep": grep_pattern,
                        "matches": matches,
                    });
                    return serde_json::to_string_pretty(&result).unwrap_or_default();
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

        // Lines mode
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
            let end: usize = m[2].parse().unwrap_or(usize::MAX);

            // Array elements (e.g. comments_middle): interpret lines as item index range
            if content_is_structured {
                if let Some(Value::Array(arr)) = content_val {
                    let from = start.saturating_sub(1);
                    let to = end.min(arr.len());
                    let selected: Vec<Value> = arr[from..to].to_vec();
                    let mut result = serde_json::Map::new();
                    if let Some(obj) = elem_data.as_object() {
                        for (k, v) in obj {
                            if k != "content" {
                                result.insert(k.clone(), v.clone());
                            }
                        }
                    }
                    result.insert("content".to_string(), Value::Array(selected));
                    result.insert(
                        "items_shown".to_string(),
                        Value::String(format!("{}-{}", start, to)),
                    );
                    result.insert("total_items".to_string(), Value::from(arr.len()));
                    return serde_json::to_string_pretty(&Value::Object(result))
                        .unwrap_or_default();
                }
            }

            // String elements: interpret lines as text line range
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

        // Comment segments: return a TOC (index + author + date + snippet) instead
        // of dumping the full content. Use lines="1-20" to paginate.
        let elem_type = elem_data.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if elem_type == "comment_segment" {
            if let Some(Value::Array(arr)) = content_val {
                let toc: Vec<Value> = arr
                    .iter()
                    .map(|c| {
                        let body = c.get("body").and_then(|v| v.as_str()).unwrap_or("");
                        let snippet: String = body
                            .chars()
                            .filter(|ch| !ch.is_control())
                            .take(80)
                            .collect();
                        serde_json::json!({
                            "_index": c.get("_index"),
                            "author": c.get("author"),
                            "created_at": c.get("created_at"),
                            "author_association": c.get("author_association"),
                            "snippet": snippet,
                        })
                    })
                    .collect();
                let result = serde_json::json!({
                    "element_id": element_id,
                    "type": elem_type,
                    "total_comments": arr.len(),
                    "hint": "Use lines='1-20' to paginate, or grep='pattern' to search.",
                    "comments": toc,
                });
                return serde_json::to_string_pretty(&result).unwrap_or_default();
            }
        }

        // Full content
        serde_json::to_string_pretty(elem_data).unwrap_or_default()
    }

    /// Fetch a GitHub issue/PR, compact it, and store cache entries.
    ///
    /// Releases the GIL during all HTTP and computation (when the
    /// `python` feature is on). This is the primary entry point for
    /// fetching discussions with caching, callable from both Python
    /// and pure Rust.
    ///
    /// Every code path returns a status string — invalid-repo, fetch-
    /// failure, cached-summary, overflow-preview, and full-text are
    /// all returned as `String`; the return is never a real error
    /// envelope. Pyo3 wraps the return as a Python `str` when the
    /// `python` feature is enabled.
    #[allow(clippy::too_many_arguments)]
    pub fn fetch_issue(
        &mut self,
        repo: &str,
        number: u64,
        element_id: Option<&str>,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
        refresh: bool,
    ) -> String {
        // Element retrieval — no network, fast
        if let Some(eid) = element_id {
            return self.retrieve(repo, number, eid, lines, grep, context);
        }

        // Validate repo
        if let Some(err) = crate::git_refs::validate_repo(repo) {
            return err;
        }

        // If cached and not refreshing, return summary of available elements
        let key = (repo.to_string(), number);
        if !refresh {
            if let Some(elements) = self.store.get(&key) {
                if !elements.is_empty() {
                    let mut ids: Vec<&String> = elements.keys().collect();
                    ids.sort();
                    return format!(
                        "Cached {}#{} — {} elements available: {}\n\
                         Use element_id='...' to drill down, or refresh=True to re-fetch.",
                        repo,
                        number,
                        ids.len(),
                        ids.iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }

        // All HTTP + computation runs in Rust; parallel requests use std::thread::scope
        let (text, cache_json) = match github::fetch_issue_internal(repo, number) {
            Ok(r) => r,
            Err(e) => return e,
        };

        // Store cache entries
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
            let safe_end = compact::safe_byte_index(&text, github::OVERFLOW_PREVIEW);
            let mut preview = text[..safe_end].to_string();
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
            return preview;
        }

        text
    }
}

// PyO3 wrapper impl — thin delegating layer that re-exposes the plain
// impl's surface to Python. Methods need pyo3-specific sub-attributes
// (`#[new]`, `#[pyo3(signature = ...)]`) which cannot be produced via
// `cfg_attr`, so we keep them inside a separate `#[pymethods]` impl
// that's only compiled with the `python` feature.
//
// Each wrapper renames the Python-side method via `#[pyo3(name = "X")]`
// to avoid colliding with the plain impl's identifier. The bodies just
// delegate.
#[cfg(feature = "python")]
#[pymethods]
impl ElementCache {
    #[new]
    fn py_new() -> Self {
        Self::new()
    }

    #[pyo3(name = "get")]
    fn py_get(&self, repo: &str, number: u64, element_id: &str) -> Option<String> {
        self.get(repo, number, element_id)
    }

    #[pyo3(name = "store_elements")]
    fn py_store_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        self.store_elements(repo, number, elements_json);
    }

    #[pyo3(name = "update_elements")]
    fn py_update_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        self.update_elements(repo, number, elements_json);
    }

    #[pyo3(name = "available")]
    fn py_available(&self, repo: &str, number: u64) -> Vec<String> {
        self.available(repo, number)
    }

    #[pyo3(
        name = "retrieve",
        signature = (repo, number, element_id, lines=None, grep=None, context=3)
    )]
    fn py_retrieve(
        &self,
        repo: &str,
        number: u64,
        element_id: &str,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
    ) -> String {
        self.retrieve(repo, number, element_id, lines, grep, context)
    }

    #[pyo3(
        name = "fetch_issue",
        signature = (repo, number, *, element_id=None, lines=None, grep=None, context=3, refresh=false)
    )]
    #[allow(clippy::too_many_arguments)]
    fn py_fetch_issue(
        &mut self,
        repo: &str,
        number: u64,
        element_id: Option<&str>,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
        refresh: bool,
    ) -> String {
        self.fetch_issue(repo, number, element_id, lines, grep, context, refresh)
    }

    /// Compact a discussion dict and store cache entries. Python-only —
    /// `compact::compact_discussion` returns `Result<_, String>`; we
    /// map that to a `PyValueError` for the Python side.
    #[pyo3(name = "compact_and_store", signature = (repo, number, discussion_json))]
    fn py_compact_and_store(
        &mut self,
        repo: &str,
        number: u64,
        discussion_json: &str,
    ) -> PyResult<String> {
        let cache_json = serde_json::to_string(&serde_json::json!({"_n": 0})).unwrap();
        let (compacted_json, cache_out) =
            compact::compact_discussion(discussion_json, Some(&cache_json), None, None)
                .map_err(pyo3::exceptions::PyValueError::new_err)?;
        if let Some(ref cache_str) = cache_out {
            self.store_elements(repo, number, cache_str);
        }
        Ok(compacted_json)
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
                let mut item_matches = grep_json_value(item, regex, context, &child);

                // Enrich matches from comment-like objects with metadata
                if let Value::Object(obj) = item {
                    if obj.contains_key("author") && obj.contains_key("body") {
                        for m in &mut item_matches {
                            if let Some(author) = obj.get("author") {
                                m["author"] = author.clone();
                            }
                            if let Some(date) = obj.get("created_at") {
                                m["created_at"] = date.clone();
                            }
                            if let Some(assoc) = obj.get("author_association") {
                                m["author_association"] = assoc.clone();
                            }
                            if let Some(idx) = obj.get("_index") {
                                m["comment_index"] = idx.clone();
                                m["element_id"] =
                                    Value::String(format!("comment_{}", idx.as_u64().unwrap_or(0)));
                            }
                        }
                    }
                }

                matches.extend(item_matches);
            }
            matches
        }
        _ => Vec::new(),
    }
}
