//! Default bounded MCP results and session-scoped expansion of original evidence.
//!
//! Adapters pass complete MCP result objects here after their handlers finish.
//! The budget counts serialized result bytes, not tokens or JSON-RPC framing.
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub const DEFAULT_BYTES: usize = 16_384;
pub const MIN_BYTES: usize = 4_096;
const CACHE_BYTES: usize = 32 * 1024 * 1024;
const CACHE_ENTRIES: usize = 32;
const TTL: Duration = Duration::from_secs(600);

/// Presentation controls; validation must happen before invoking a tool.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ResponseOptions {
    pub mode: Mode,
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Bounded,
    Full,
}

impl ResponseOptions {
    pub fn validate(&self) -> Result<(), String> {
        if self.max_bytes.is_some_and(|n| n < MIN_BYTES) {
            return Err(format!("max_bytes must be at least {MIN_BYTES}"));
        }
        if self.mode == Mode::Full && self.max_bytes.is_some() {
            return Err("full mode cannot also specify max_bytes".into());
        }
        Ok(())
    }

    fn limit(&self) -> usize {
        self.max_bytes.unwrap_or(DEFAULT_BYTES)
    }
}

/// A JSON Pointer addresses the normalized payload described by a preview.
/// Offset selects array items, object fields, or Unicode characters of a string.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expansion {
    pub result_id: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub response: ResponseOptions,
}

struct Stored<S> {
    owner: S,
    id: String,
    tool: String,
    arguments: Value,
    result: Value,
    payload: Value,
    bytes: usize,
    created: Instant,
}

/// One bounded store per server. Owners must identify sessions, not client names.
/// Oldest results are evicted at 32 entries or 32 MiB of serialized originals
/// and normalized payloads; entries expire after ten minutes on the next access.
pub struct ResponseStore<S> {
    entries: VecDeque<Stored<S>>,
    next_id: u64,
}

impl<S> Default for ResponseStore<S> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            next_id: 0,
        }
    }
}

impl<S: PartialEq> ResponseStore<S> {
    fn expire(&mut self) {
        self.entries.retain(|e| e.created.elapsed() < TTL);
    }

    /// Apply the default to a complete result. Full calls and small results
    /// retain their original shape. An uncacheable result is returned intact
    /// with an explicit overage reason, never silently discarded after a write.
    pub fn present(
        &mut self,
        owner: S,
        tool: &str,
        arguments: Value,
        mut result: Value,
        options: &ResponseOptions,
        expansion_tool: &str,
    ) -> Value {
        self.expire();
        let bytes = size(&result);
        if options.mode == Mode::Full || bytes <= options.limit() {
            return result;
        }
        let payload = payload(&result);
        let retained_bytes = bytes
            .saturating_add(size(&payload))
            .saturating_add(size(&arguments));
        if retained_bytes > CACHE_BYTES {
            report_overage(&mut result, options.limit(), "Result exceeds the 32 MiB retention capacity. Returned intact to preserve evidence without rerunning the operation. Request narrower output on future calls.");
            return result;
        }
        while self.entries.len() >= CACHE_ENTRIES
            || self.entries.iter().map(|e| e.bytes).sum::<usize>() + retained_bytes > CACHE_BYTES
        {
            self.entries.pop_front();
        }
        self.next_id += 1;
        let entry = Stored {
            owner,
            id: format!("r{}", self.next_id),
            tool: tool.into(),
            arguments,
            result,
            payload,
            bytes: retained_bytes,
            created: Instant::now(),
        };
        let preview = render(&entry, "", 0, options.limit(), expansion_tool);
        self.entries.push_back(entry);
        preview
    }

    pub fn expand(
        &mut self,
        owner: &S,
        request: &Expansion,
        expansion_tool: &str,
    ) -> Result<Value, String> {
        request.response.validate()?;
        self.expire();
        let entry = self.entries.iter().find(|e| &e.owner == owner && e.id == request.result_id)
            .ok_or("Result unavailable in this session (expired or evicted). The original operation was not rerun.")?;
        let selected = entry
            .payload
            .pointer(&request.path)
            .ok_or("No value at that JSON Pointer")?;
        if request.offset > length(selected) {
            return Err("offset is beyond the selected value".into());
        }
        if request.response.mode == Mode::Full {
            if request.offset != 0 {
                return Err("full mode requires offset 0".into());
            }
            return Ok(if request.path.is_empty() {
                entry.result.clone()
            } else {
                text_result(
                    selected.to_string(),
                    entry.result["isError"].as_bool().unwrap_or(false),
                )
            });
        }
        Ok(render(
            entry,
            &request.path,
            request.offset,
            request.response.limit(),
            expansion_tool,
        ))
    }
}

fn report_overage(result: &mut Value, limit: usize, reason: &str) {
    let original_bytes = size(result);
    if !result["_meta"].is_object() {
        result["_meta"] = json!({});
    }
    result["_meta"]["mcp_methods/response_budget"] = json!({
        "complete":true, "budget_exceeded":true, "max_bytes":limit,
        "original_bytes":original_bytes, "reason":reason
    });
    if let Some(content) = result["content"].as_array_mut() {
        content.insert(
            0,
            json!({"type":"text","text":format!("Response budget exceeded: {reason}")}),
        );
    }
}

fn size(value: &Value) -> usize {
    serde_json::to_vec(value).expect("JSON value").len()
}

fn payload(result: &Value) -> Value {
    if let Some(value) = result.get("structuredContent").filter(|v| !v.is_null()) {
        return value.clone();
    }
    if let Some(content) = result["content"].as_array().filter(|c| c.len() == 1) {
        if let Some(text) = content[0]["text"].as_str() {
            return serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into()));
        }
    }
    result["content"].clone()
}

fn text_result(text: String, error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": error})
}

fn length(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::String(s) => s.chars().count(),
        _ => 1,
    }
}

fn pointer(path: &str, key: &str) -> String {
    format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"))
}

/// Structural evidence only: shortened values are explicitly wrapped, never
/// substituted under the original field name as if they were complete values.
fn outline(v: &Value, path: &str, offset: usize, allowance: usize, depth: usize) -> (Value, usize) {
    if offset == 0 && size(v) <= allowance {
        return (json!({"path":path,"complete":true,"value":v}), length(v));
    }
    if depth >= 6 || allowance < 80 {
        return (
            json!({"path":path,"complete":false,"available":length(v)}),
            offset,
        );
    }
    match v {
        Value::String(s) => {
            let mut text: String = s.chars().skip(offset).take(allowance / 8).collect();
            if let Some(last_line) = text.rfind('\n').filter(|i| *i >= text.len() / 2) {
                text.truncate(last_line + 1);
            }
            let end = offset + text.chars().count();
            (
                json!({"path":path,"complete":false,"unit":"characters","total":s.chars().count(),"offset":offset,"end":end,"excerpt":text}),
                end,
            )
        }
        Value::Array(a) => {
            let sample: Vec<_> = a.iter().skip(offset).take(8).collect();
            let average = sample.iter().map(|v| size(v) + 80).sum::<usize>() / sample.len().max(1);
            let count = (allowance / average.clamp(400, 1600))
                .clamp(1, 64)
                .min(a.len().saturating_sub(offset));
            let items: Vec<_> = a
                .iter()
                .enumerate()
                .skip(offset)
                .take(count)
                .map(|(i, v)| {
                    outline(
                        v,
                        &pointer(path, &i.to_string()),
                        0,
                        allowance / count.max(1),
                        depth + 1,
                    )
                    .0
                })
                .collect();
            (
                json!({"path":path,"complete":false,"unit":"items","total":a.len(),"offset":offset,"end":offset+count,"omitted_items":a.len()-count,"items":items}),
                offset + count,
            )
        }
        Value::Object(o) => {
            let count = (allowance / 400)
                .clamp(1, 12)
                .min(o.len().saturating_sub(offset));
            let mut ordered: Vec<_> = o.iter().collect();
            ordered.sort_by_key(|(key, _)| {
                (
                    !matches!(
                        key.as_str(),
                        "status"
                            | "summary"
                            | "warnings"
                            | "warning"
                            | "errors"
                            | "error"
                            | "diagnostics"
                            | "coverage"
                    ),
                    key.as_str(),
                )
            });
            let fields: Vec<_> = ordered
                .into_iter()
                .skip(offset)
                .take(count)
                .map(|(k, v)| {
                    outline(v, &pointer(path, k), 0, allowance / count.max(1), depth + 1).0
                })
                .collect();
            (
                json!({"path":path,"complete":false,"unit":"fields","total":o.len(),"offset":offset,"end":offset+count,"omitted_fields":o.len()-count,"fields":fields}),
                offset + count,
            )
        }
        _ => (json!({"path":path,"complete":true,"value":v}), 1),
    }
}

fn summary_outline(value: &Value, allowance: usize) -> Value {
    fn strip_paths(node: &mut Value) {
        if let Some(object) = node.as_object_mut() {
            if let Some(path) = object.remove("path") {
                object.insert("location".into(), path);
            }
            for key in ["fields", "items"] {
                if let Some(Value::Array(children)) = object.get_mut(key) {
                    for child in children {
                        strip_paths(child);
                    }
                }
            }
        }
    }
    let mut summary = outline(value, "", 0, allowance, 0).0;
    strip_paths(&mut summary);
    summary
}

fn render<S>(
    entry: &Stored<S>,
    path: &str,
    offset: usize,
    limit: usize,
    expansion_tool: &str,
) -> Value {
    let selected = entry.payload.pointer(path).expect("validated pointer");
    let action = |path: &str, offset, mode| {
        json!({"name":expansion_tool,"arguments":{
            "result_id":entry.id,"path":path,"offset":offset,"response":{"mode":mode}
        }})
    };
    let original_bytes = size(&entry.result);
    let build = |allowance| {
        let (preview, end) = outline(selected, path, offset, allowance, 0);
        let mut body = json!({
            "response_budget": {
                "complete": false, "max_bytes":limit,"original_bytes":original_bytes,
                "tool":entry.tool,"result_id":entry.id,
                "scope":summary_outline(&entry.arguments,400),
                "selection":"Arrays in original order; object fields prioritize status, summary, warnings, errors, diagnostics and coverage, then lexical order; paths are JSON Pointers into structuredContent, parsed single-text JSON, single text, or content blocks (in that order).",
                "coverage":"Presentation preview only. Query limits and tool-level omissions still apply; semantic coverage and database-wide totals are unknown.",
                "retention":"Same session, up to 10 minutes; may be evicted at 32 entries / 32 MiB. Expansion never reruns the tool.",
                "preview":preview,
                "next":{
                    "full_result":action("",0,"full"),
                    "selected_value":action(path,0,"full"),
                    "inspect":"Use a preview path with the expansion tool to inspect an omitted value."
                }
            }
        });
        if end < length(selected) && end > offset {
            body["response_budget"]["next"]["page"] = action(path, end, "bounded");
        }
        if let Some(text) = selected.as_str() {
            let tail: String = text
                .chars()
                .rev()
                .take((allowance / 16).min(512))
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            body["response_budget"]["tail_excerpt"] = json!(tail);
        }
        if entry.result.get("structuredContent").is_some() {
            body["response_budget"]["content_overview"] =
                summary_outline(&entry.result["content"], allowance / 8);
        }
        if let Some(meta) = entry.result.get("_meta").and_then(Value::as_object) {
            let mut metadata = meta.clone();
            metadata.remove("mcp_methods/preview");
            if !metadata.is_empty() {
                body["response_budget"]["metadata_overview"] =
                    summary_outline(&json!(metadata), allowance / 8);
            }
        }
        if let Some(hint) = entry.result.pointer("/_meta/mcp_methods~1preview") {
            body["response_budget"]["domain_guidance"] = summary_outline(hint, allowance / 4);
        }
        let mut result = text_result(
            body.to_string(),
            entry.result["isError"].as_bool().unwrap_or(false),
        );
        // A small discriminant satisfies the advertised preview schema without
        // duplicating evidence in text and structured content.
        result["structuredContent"] = json!({"mcp_methods_preview":true});
        result
    };
    let mut low = 0;
    let mut high = limit.min(size(selected).saturating_mul(16).max(MIN_BYTES));
    let mut best = build(0);
    while low <= high {
        let mid = low + (high - low) / 2;
        let candidate = build(mid);
        if size(&candidate) <= limit {
            best = candidate;
            low = mid + 1;
        } else {
            if mid == 0 {
                break;
            }
            high = mid - 1;
        }
    }
    // Extremely long tool names/paths can make even control metadata exceed
    // the limit. Preserve the result and disclose the overage instead of loss.
    if size(&best) > limit {
        let mut full = entry.result.clone();
        report_overage(
            &mut full,
            limit,
            "Expansion metadata cannot fit the budget; original result returned intact.",
        );
        return full;
    }
    best
}

pub fn options_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
        "mode":{"type":"string","enum":["bounded","full"],"default":"bounded"},
        "max_bytes":{"type":"integer","minimum":MIN_BYTES}
    },"description":"Default 16384 serialized result bytes. Use mode=full for complete inline output, or max_bytes for a larger one-call budget. Tool/query limits still apply."})
}

pub fn expansion_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"required":["result_id"],"properties":{
        "result_id":{"type":"string"},"path":{"type":"string","default":""},
        "offset":{"type":"integer","minimum":0,"default":0},"response":options_schema()
    }})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large() -> Value {
        let mut result = text_result("界\\\"\n".repeat(8000), true);
        result["_meta"] = json!({"stamp":"original"});
        result
    }

    fn request(id: &str) -> Expansion {
        Expansion {
            result_id: id.into(),
            path: String::new(),
            offset: 0,
            response: ResponseOptions {
                mode: Mode::Full,
                max_bytes: None,
            },
        }
    }

    fn body(v: &Value) -> Value {
        serde_json::from_str::<Value>(v["content"][0]["text"].as_str().unwrap()).unwrap()
            ["response_budget"]
            .clone()
    }

    #[test]
    fn utf8_escaped_data_fits_measured_result_and_recovers_exactly() {
        let original = large();
        let mut store = ResponseStore::default();
        for limit in [MIN_BYTES, 8192, DEFAULT_BYTES, 32768] {
            let preview = store.present(
                "session",
                "tool",
                json!({"query":"scope"}),
                original.clone(),
                &ResponseOptions {
                    mode: Mode::Bounded,
                    max_bytes: Some(limit),
                },
                "expand_response",
            );
            assert!(size(&preview) <= limit, "{} > {limit}", size(&preview));
            let p = body(&preview);
            assert_eq!(preview["isError"], true);
            assert!(!p["preview"]["excerpt"].as_str().unwrap().is_empty());
            let id = p["result_id"].as_str().unwrap();
            assert_eq!(
                store
                    .expand(&"session", &request(id), "expand_response")
                    .unwrap(),
                original
            );
        }
    }

    #[test]
    fn count_eviction_expiry_and_wrong_owner_do_not_rerun_or_leak() {
        let mut store = ResponseStore::default();
        for _ in 0..33 {
            store.present(
                "owner",
                "tool",
                json!({}),
                large(),
                &ResponseOptions::default(),
                "expand_response",
            );
        }
        assert!(store
            .expand(&"owner", &request("r1"), "expand_response")
            .is_err());
        assert!(store
            .expand(&"other", &request("r33"), "expand_response")
            .is_err());
        store.entries.back_mut().unwrap().created = Instant::now() - TTL;
        assert!(store
            .expand(&"owner", &request("r33"), "expand_response")
            .is_err());
        assert!(store.entries.len() < CACHE_ENTRIES);
    }

    #[test]
    fn byte_eviction_and_uncacheable_results_preserve_evidence() {
        let mut store = ResponseStore::default();
        for _ in 0..5 {
            store.present(
                "owner",
                "tool",
                json!({}),
                text_result("x".repeat(4 * 1024 * 1024), false),
                &ResponseOptions::default(),
                "expand_response",
            );
        }
        assert!(store.entries.iter().map(|e| e.bytes).sum::<usize>() <= CACHE_BYTES);
        assert!(store
            .expand(&"owner", &request("r1"), "expand_response")
            .is_err());
        let too_large = text_result("y".repeat(CACHE_BYTES), false);
        let result = store.present(
            "owner",
            "tool",
            json!({}),
            too_large.clone(),
            &ResponseOptions::default(),
            "expand_response",
        );
        assert_eq!(result["content"][1], too_large["content"][0]);
        assert!(result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Response budget exceeded"));
        assert_eq!(
            result["_meta"]["mcp_methods/response_budget"]["budget_exceeded"],
            true
        );
    }

    #[test]
    fn nested_values_are_labeled_and_json_pointer_expands_original() {
        let original = json!({"content":[{"type":"image","data":"base64".repeat(10000),"mimeType":"image/png"}],
            "structuredContent":{"a/b~c":[{"id":7,"giant":"nested".repeat(20000)}],"warnings":["only write callers were queried"]},"isError":false});
        let mut store = ResponseStore::default();
        let preview = store.present(
            "owner",
            "query",
            json!({"limit":120}),
            original.clone(),
            &ResponseOptions::default(),
            "expand_response",
        );
        assert!(size(&preview) <= DEFAULT_BYTES);
        let p = body(&preview);
        assert_eq!(p["complete"], false);
        let mut req = request(p["result_id"].as_str().unwrap());
        req.path = "/a~1b~0c/0/giant".into();
        let recovered = store.expand(&"owner", &req, "expand_response").unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(recovered["content"][0]["text"].as_str().unwrap())
                .unwrap(),
            original["structuredContent"]["a/b~c"][0]["giant"]
        );
        req.path = "/not-present".into();
        assert!(store.expand(&"owner", &req, "expand_response").is_err());
    }
}
