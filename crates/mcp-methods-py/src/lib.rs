//! PyO3 bindings for `mcp-methods`.
//!
//! This crate builds the `_mcp_methods` cdylib that ships in the
//! Python wheel. Every function and class is a thin wrapper around
//! the pure-Rust `mcp_methods` library — no business logic lives
//! here. The Python `mcp_methods/__init__.py` re-exports from
//! `mcp_methods._mcp_methods` (this cdylib).
//!
//! Wheel build: see the workspace `pyproject.toml` which sets
//! `manifest-path = "crates/mcp-methods-py/Cargo.toml"`. The
//! `pyo3/abi3-py310` feature collapses the per-Python-version wheel
//! matrix to a single abi3 wheel per OS.

use mcp_methods::cache::ElementCache as CoreCache;
use mcp_methods::files::{read_file as core_read_file, ReadFileOpts};
use mcp_methods::grep::{
    ripgrep_files as core_ripgrep_files, ripgrep_lines as core_ripgrep_lines, RipgrepFilesOpts,
};
use mcp_methods::json_grep::ripgrep_json_fields as core_ripgrep_json_fields;
use mcp_methods::list_dir::{list_dir as core_list_dir, ListDirOpts};
use mcp_methods::{compact, git_refs, github, html};

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

/// String-returning transform closure type used by the Python wrappers
/// around `read_file` and `ripgrep_files` — bridges a `Py<PyAny>`
/// callable to a Rust `&dyn Fn`.
type StringTransform = Box<dyn Fn(&str) -> String>;

/// Optional-string-returning annotate closure type used by the Python
/// wrapper around `list_dir`.
type OptStringTransform = Box<dyn Fn(&str) -> Option<String>>;

// ---------------------------------------------------------------------------
// ElementCache — `#[pyclass]` newtype wrapper around `CoreCache`
// ---------------------------------------------------------------------------

#[pyclass(name = "ElementCache")]
struct PyElementCache(CoreCache);

#[pymethods]
impl PyElementCache {
    #[new]
    fn new() -> Self {
        PyElementCache(CoreCache::new())
    }

    fn get(&self, repo: &str, number: u64, element_id: &str) -> Option<String> {
        self.0.get(repo, number, element_id)
    }

    fn store_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        self.0.store_elements(repo, number, elements_json);
    }

    fn update_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        self.0.update_elements(repo, number, elements_json);
    }

    fn available(&self, repo: &str, number: u64) -> Vec<String> {
        self.0.available(repo, number)
    }

    #[pyo3(signature = (repo, number, element_id, lines=None, grep=None, context=3))]
    fn retrieve(
        &self,
        repo: &str,
        number: u64,
        element_id: &str,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
    ) -> String {
        self.0
            .retrieve(repo, number, element_id, lines, grep, context)
    }

    #[pyo3(signature = (repo, number, *, element_id=None, lines=None, grep=None, context=3, refresh=false))]
    #[allow(clippy::too_many_arguments)]
    fn fetch_issue(
        &mut self,
        repo: &str,
        number: u64,
        element_id: Option<&str>,
        lines: Option<&str>,
        grep: Option<&str>,
        context: usize,
        refresh: bool,
    ) -> String {
        self.0
            .fetch_issue(repo, number, element_id, lines, grep, context, refresh)
    }

    /// Compact a discussion dict and store cache entries.
    #[pyo3(signature = (repo, number, discussion_json))]
    fn compact_and_store(
        &mut self,
        repo: &str,
        number: u64,
        discussion_json: &str,
    ) -> PyResult<String> {
        let cache_json = serde_json::to_string(&serde_json::json!({"_n": 0})).unwrap();
        let (compacted_json, cache_out) =
            compact::compact_discussion(discussion_json, Some(&cache_json), None, None)
                .map_err(PyValueError::new_err)?;
        if let Some(ref cache_str) = cache_out {
            self.0.store_elements(repo, number, cache_str);
        }
        Ok(compacted_json)
    }
}

// ---------------------------------------------------------------------------
// compact — wrap `collapse_code_blocks`, `compact_text`, `compact_discussion`
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (text, cache_json = None))]
fn collapse_code_blocks(text: &str, cache_json: Option<&str>) -> (String, Option<String>) {
    compact::collapse_code_blocks(text, cache_json)
}

#[pyfunction]
#[pyo3(signature = (text, limit, cache_json = None))]
fn compact_text(
    text: &str,
    limit: usize,
    cache_json: Option<&str>,
) -> (String, bool, Option<String>) {
    compact::compact_text(text, limit, cache_json)
}

#[pyfunction]
#[pyo3(signature = (discussion_json, cache_json = None, budget = None, item_budget = None))]
fn compact_discussion(
    discussion_json: &str,
    cache_json: Option<&str>,
    budget: Option<usize>,
    item_budget: Option<usize>,
) -> PyResult<(String, Option<String>)> {
    compact::compact_discussion(discussion_json, cache_json, budget, item_budget)
        .map_err(PyValueError::new_err)
}

// ---------------------------------------------------------------------------
// git_refs / github / html
// ---------------------------------------------------------------------------

#[pyfunction]
fn validate_repo(repo_name: &str) -> Option<String> {
    git_refs::validate_repo(repo_name)
}

#[pyfunction]
fn extract_github_refs(text: &str, default_repo: &str) -> Vec<(String, u64)> {
    git_refs::extract_github_refs(text, default_repo)
}

#[pyfunction]
fn has_git_token() -> bool {
    github::has_git_token()
}

#[pyfunction]
fn detect_git_repo(cwd: &str) -> Option<String> {
    github::detect_git_repo(cwd)
}

#[pyfunction]
#[pyo3(signature = (repo, path, *, truncate_at=80_000))]
fn git_api(repo: &str, path: &str, truncate_at: usize) -> String {
    github::git_api_internal(repo, path, truncate_at)
}

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
fn github_issues(
    repo: Option<&str>,
    number: Option<u64>,
    query: Option<&str>,
    kind: &str,
    state: &str,
    sort: Option<&str>,
    limit: usize,
    labels: Option<&str>,
) -> String {
    github::github_issues_rust(repo, number, query, kind, state, sort, limit, labels)
}

#[pyfunction]
fn html_to_text(html_str: &str) -> String {
    html::html_to_text(html_str)
}

// ---------------------------------------------------------------------------
// read_file — bridges Python `transform=callable|"html"` to Rust closure
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (
    file_path,
    allowed_dirs,
    *,
    section = None,
    start_line = None,
    end_line = None,
    rows = None,
    max_chars = None,
    transform = None,
    grep = None,
    grep_context = None,
    max_matches = None,
))]
#[allow(clippy::too_many_arguments)]
fn read_file(
    py: Python<'_>,
    file_path: &str,
    allowed_dirs: Vec<String>,
    section: Option<&str>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    rows: Option<Vec<usize>>,
    max_chars: Option<usize>,
    transform: Option<Py<PyAny>>,
    grep: Option<&str>,
    grep_context: Option<usize>,
    max_matches: Option<usize>,
) -> PyResult<String> {
    // Translate the legacy `transform=` argument:
    //   - string "html"         → ReadFileOpts.html_transform = true
    //   - any other string      → friendly error
    //   - callable              → Rust closure that re-enters Python
    let mut html_transform = false;
    let mut callable: Option<Py<PyAny>> = None;
    if let Some(ref tf) = transform {
        match tf.extract::<String>(py) {
            Ok(name) => match name.as_str() {
                "html" => html_transform = true,
                other => {
                    return Ok(format!(
                        "Error: unknown transform '{}'. Available: html",
                        other
                    ))
                }
            },
            Err(_) => callable = Some(tf.clone_ref(py)),
        }
    }

    let rows_pair = rows.and_then(|v| {
        if v.len() == 2 {
            Some((v[0], v[1]))
        } else {
            None
        }
    });

    // Build the closure outside the opts struct so its lifetime spans
    // the call. The `&dyn Fn` ends up borrowing from `transform_closure`.
    let transform_closure: Option<StringTransform> = callable.map(|cb| {
        Box::new(move |s: &str| -> String {
            Python::attach(|py| {
                cb.call1(py, (s,))
                    .and_then(|r| r.extract::<String>(py))
                    .unwrap_or_else(|_| String::new())
            })
        }) as Box<dyn Fn(&str) -> String>
    });

    let opts = ReadFileOpts {
        section,
        start_line,
        end_line,
        rows: rows_pair,
        max_chars,
        html_transform,
        transform: transform_closure.as_deref(),
        grep,
        grep_context,
        max_matches,
    };
    Ok(core_read_file(file_path, &allowed_dirs, &opts))
}

// ---------------------------------------------------------------------------
// ripgrep_files — bridges `transform=callable`
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (
    source_dirs,
    pattern,
    *,
    glob = "*",
    type_filter = None,
    output_mode = "content",
    case_insensitive = false,
    multiline = false,
    context_before = 0,
    context_after = 0,
    context = 0,
    line_numbers = true,
    max_results = None,
    offset = 0,
    match_limit = None,
    skip_dirs = None,
    relative_to = None,
    respect_gitignore = true,
    transform = None,
))]
#[allow(clippy::too_many_arguments)]
fn ripgrep_files(
    source_dirs: Vec<String>,
    pattern: &str,
    glob: &str,
    type_filter: Option<&str>,
    output_mode: &str,
    case_insensitive: bool,
    multiline: bool,
    context_before: usize,
    context_after: usize,
    context: usize,
    line_numbers: bool,
    max_results: Option<usize>,
    offset: usize,
    match_limit: Option<usize>,
    skip_dirs: Option<Vec<String>>,
    relative_to: Option<&str>,
    respect_gitignore: bool,
    transform: Option<Py<PyAny>>,
) -> String {
    let transform_closure: Option<StringTransform> = transform.map(|cb| {
        Box::new(move |s: &str| -> String {
            Python::attach(|py| {
                cb.call1(py, (s,))
                    .and_then(|r| r.extract::<String>(py))
                    .unwrap_or_else(|_| String::new())
            })
        }) as Box<dyn Fn(&str) -> String>
    });

    let opts = RipgrepFilesOpts {
        glob: Some(glob),
        type_filter,
        output_mode: Some(output_mode),
        case_insensitive,
        multiline,
        context_before,
        context_after,
        context,
        line_numbers,
        max_results,
        offset,
        match_limit,
        skip_dirs: skip_dirs.as_deref(),
        relative_to,
        respect_gitignore,
        transform: transform_closure.as_deref(),
    };
    core_ripgrep_files(&source_dirs, pattern, &opts)
}

// ---------------------------------------------------------------------------
// ripgrep_lines — returns a Python list of dicts
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (text_lines, pattern, context))]
fn ripgrep_lines(
    py: Python<'_>,
    text_lines: Vec<String>,
    pattern: &str,
    context: usize,
) -> PyResult<Py<PyAny>> {
    let groups =
        core_ripgrep_lines(&text_lines, pattern, context).map_err(PyValueError::new_err)?;
    let result = PyList::empty(py);
    for g in groups {
        let dict = PyDict::new(py);
        dict.set_item("lines", g.lines)?;
        dict.set_item("context_start", g.context_start)?;
        dict.set_item("context_end", g.context_end)?;
        dict.set_item("content", g.content)?;
        result.append(dict)?;
    }
    Ok(result.into_any().unbind())
}

// ---------------------------------------------------------------------------
// ripgrep_json_fields — list of dicts
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (json_str, pattern, context))]
fn ripgrep_json_fields(
    py: Python<'_>,
    json_str: &str,
    pattern: &str,
    context: usize,
) -> PyResult<Py<PyAny>> {
    let matches =
        core_ripgrep_json_fields(json_str, pattern, context).map_err(PyValueError::new_err)?;
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

// ---------------------------------------------------------------------------
// list_dir — bridges `annotate=callable`
// ---------------------------------------------------------------------------

#[pyfunction]
#[pyo3(signature = (
    path,
    *,
    depth = 1,
    glob = None,
    dirs_only = false,
    relative_to = None,
    respect_gitignore = true,
    skip_dirs = None,
    include_size = false,
    annotate = None,
))]
#[allow(clippy::too_many_arguments)]
fn list_dir(
    path: &str,
    depth: usize,
    glob: Option<&str>,
    dirs_only: bool,
    relative_to: Option<&str>,
    respect_gitignore: bool,
    skip_dirs: Option<Vec<String>>,
    include_size: bool,
    annotate: Option<Py<PyAny>>,
) -> PyResult<String> {
    let annotate_closure: Option<OptStringTransform> = annotate.map(|cb| {
        Box::new(move |s: &str| -> Option<String> {
            Python::attach(|py| {
                let res = cb.call1(py, (s,)).ok()?;
                if res.is_none(py) {
                    return None;
                }
                res.extract::<String>(py).ok()
            })
        }) as Box<dyn Fn(&str) -> Option<String>>
    });

    let opts = ListDirOpts {
        depth: Some(depth),
        glob,
        dirs_only,
        relative_to,
        respect_gitignore,
        skip_dirs: skip_dirs.as_deref(),
        include_size,
        annotate: annotate_closure.as_deref(),
    };
    core_list_dir(path, &opts).map_err(PyValueError::new_err)
}

// ---------------------------------------------------------------------------
// Module init
// ---------------------------------------------------------------------------

#[pymodule]
fn _mcp_methods(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // git_refs
    m.add_function(wrap_pyfunction!(validate_repo, m)?)?;
    m.add_function(wrap_pyfunction!(extract_github_refs, m)?)?;
    // grep
    m.add_function(wrap_pyfunction!(ripgrep_files, m)?)?;
    m.add_function(wrap_pyfunction!(ripgrep_lines, m)?)?;
    // files
    m.add_function(wrap_pyfunction!(read_file, m)?)?;
    // html
    m.add_function(wrap_pyfunction!(html_to_text, m)?)?;
    // list_dir
    m.add_function(wrap_pyfunction!(list_dir, m)?)?;
    // compact
    m.add_function(wrap_pyfunction!(collapse_code_blocks, m)?)?;
    m.add_function(wrap_pyfunction!(compact_text, m)?)?;
    m.add_function(wrap_pyfunction!(compact_discussion, m)?)?;
    // json_grep
    m.add_function(wrap_pyfunction!(ripgrep_json_fields, m)?)?;
    // github
    m.add_function(wrap_pyfunction!(has_git_token, m)?)?;
    m.add_function(wrap_pyfunction!(detect_git_repo, m)?)?;
    m.add_function(wrap_pyfunction!(git_api, m)?)?;
    m.add_function(wrap_pyfunction!(github_issues, m)?)?;
    // cache
    m.add_class::<PyElementCache>()?;
    Ok(())
}
