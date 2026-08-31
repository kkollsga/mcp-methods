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

use std::path::PathBuf;

use mcp_methods::cache::ElementCache as CoreCache;
use mcp_methods::files::{read_file as core_read_file, ReadFileOpts};
use mcp_methods::grep::{
    ripgrep_files as core_ripgrep_files, ripgrep_lines as core_ripgrep_lines, RipgrepFilesOpts,
};
use mcp_methods::json_grep::ripgrep_json_fields as core_ripgrep_json_fields;
use mcp_methods::list_dir::{list_dir as core_list_dir, ListDirOpts};
use mcp_methods::server::find_sibling_manifest;
use mcp_methods::server::skills::{
    render_skill_template as core_render_skill_template,
    write_skill_template as core_write_skill_template, Registry as SkillsRegistry,
    ResolvedRegistry as CoreResolvedRegistry, Skill as CoreSkill,
};
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

/// In-memory cache of the oversized pieces of GitHub issues, PRs and
/// discussions, keyed by `(repo, number)` and then by element id.
///
/// Fetching and compacting an issue replaces its large fenced code
/// blocks, `<details>` sections and overflowing comment bodies with short
/// placeholders naming an element id; this cache holds the full text so a
/// caller can drill back into one element without re-fetching the issue.
///
/// Constructed with no arguments::
///
///     cache = ElementCache()
#[pyclass(name = "ElementCache")]
struct PyElementCache(CoreCache);

#[pymethods]
impl PyElementCache {
    #[new]
    fn new() -> Self {
        PyElementCache(CoreCache::new())
    }

    /// Return one cached element as a JSON string.
    ///
    /// Parameters::
    ///
    ///     repo        "org/repo" the element was cached under
    ///     number      issue / PR / discussion number
    ///     element_id  element id from the placeholder, e.g. "cb_1"
    ///
    /// Returns the element's JSON serialization, or `None` when nothing is
    /// cached for that `(repo, number)` or that element id.
    fn get(&self, repo: &str, number: u64, element_id: &str) -> Option<String> {
        self.0.get(repo, number, element_id)
    }

    /// Store the elements for one `(repo, number)`, replacing any elements
    /// already cached under it.
    ///
    /// Parameters::
    ///
    ///     repo           "org/repo"
    ///     number         issue / PR / discussion number
    ///     elements_json  JSON object string mapping element id to element.
    ///                    Keys starting with "_" are metadata and are not
    ///                    stored. Input that is not a JSON object is ignored
    ///                    silently.
    ///
    /// Returns `None`.
    fn store_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        self.0.store_elements(repo, number, elements_json);
    }

    /// Merge elements into the entry for one `(repo, number)`, keeping any
    /// already-cached elements that `elements_json` does not name.
    ///
    /// The merging counterpart to `store_elements`; same parameters, same
    /// "_"-prefixed-keys-are-metadata and ignore-invalid-JSON behaviour.
    ///
    /// Returns `None`.
    fn update_elements(&mut self, repo: &str, number: u64, elements_json: &str) {
        self.0.update_elements(repo, number, elements_json);
    }

    /// List the element ids cached for one `(repo, number)`.
    ///
    /// Parameters::
    ///
    ///     repo    "org/repo"
    ///     number  issue / PR / discussion number
    ///
    /// Returns the ids sorted alphabetically, or an empty list when nothing
    /// is cached for that pair.
    fn available(&self, repo: &str, number: u64) -> Vec<String> {
        self.0.available(repo, number)
    }

    /// Retrieve one cached element, optionally sliced to a line range or
    /// filtered by regex. This is the drill-down entry point.
    ///
    /// Parameters::
    ///
    ///     repo        "org/repo"
    ///     number      issue / PR / discussion number
    ///     element_id  element id to retrieve
    ///     lines       "N-M" 1-indexed line range of the element's content;
    ///                 for list-shaped elements (comment segments) it is a
    ///                 comment-index range instead. None = whole element.
    ///     grep        regex; only matching lines/items plus context return
    ///     context     lines of context around each grep match (default 3)
    ///
    /// Returns a JSON string. Every failure is also a string, never an
    /// exception: an unknown element id returns a message listing the ids
    /// that are cached, and an invalid regex returns "Invalid grep pattern".
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

    /// Fetch a GitHub issue or PR, compact it, store its oversized elements
    /// in this cache, and return the compacted text.
    ///
    /// With `element_id` set no network call happens — the call is exactly
    /// `retrieve()` against the already-cached issue. Without it, an issue
    /// already in the cache returns a summary of its available element ids
    /// unless `refresh=True` forces a re-fetch.
    ///
    /// Parameters (all but `repo` and `number` keyword-only)::
    ///
    ///     repo        "org/repo"
    ///     number      issue / PR number
    ///     element_id  drill into this cached element instead of fetching
    ///     lines       "N-M" line range, only with element_id
    ///     grep        regex filter, only with element_id
    ///     context     lines of context around each grep match (default 3)
    ///     refresh     re-fetch from GitHub instead of reusing the cache
    ///
    /// Returns a string for every outcome — invalid repo, fetch failure,
    /// cached summary, overflow preview and full text all come back as
    /// `str` rather than as a raised exception.
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

    /// Compact a discussion payload and store its collapsed elements in
    /// this cache under `(repo, number)`.
    ///
    /// Parameters::
    ///
    ///     repo             "org/repo" to cache the elements under
    ///     number           discussion number
    ///     discussion_json  the discussion payload as a JSON object string
    ///
    /// Returns the compacted discussion as a JSON string. Raises
    /// `ValueError` when `discussion_json` is not valid JSON.
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

/// Collapse large fenced code blocks and `<details>` sections in `text`,
/// replacing each with a short placeholder naming its element id.
///
/// Parameters::
///
///     text        the markdown / prose to collapse
///     cache_json  JSON object string to record the collapsed elements
///                 into (default: None — nothing is recorded)
///
/// Returns `(collapsed_text, updated_cache_json)`. The second item is
/// `None` when `cache_json` was not supplied or did not parse as JSON.
#[pyfunction]
#[pyo3(signature = (text, cache_json = None))]
fn collapse_code_blocks(text: &str, cache_json: Option<&str>) -> (String, Option<String>) {
    compact::collapse_code_blocks(text, cache_json)
}

/// Collapse large code blocks and `<details>` sections in `text`, then
/// truncate the result if it is still longer than `limit`.
///
/// Parameters::
///
///     text        the text to compact
///     limit       maximum output size, in characters
///     cache_json  JSON object string to record the collapsed elements
///                 into (default: None — nothing is recorded)
///
/// Returns `(text, was_truncated, updated_cache_json)`. The last item is
/// `None` when `cache_json` was not supplied or did not parse as JSON.
#[pyfunction]
#[pyo3(signature = (text, limit, cache_json = None))]
fn compact_text(
    text: &str,
    limit: usize,
    cache_json: Option<&str>,
) -> (String, bool, Option<String>) {
    compact::compact_text(text, limit, cache_json)
}

/// Compact a GitHub issue / discussion JSON payload with budget-based
/// adaptive compaction — oversized bodies and comments are collapsed to
/// placeholders until the payload fits the budget.
///
/// Parameters::
///
///     discussion_json  the payload as a JSON object string
///     cache_json       JSON object string to record collapsed elements into
///     budget           whole-payload output budget (default 60 KB)
///     item_budget      per-item output budget (default 15 KB)
///
/// Returns `(compacted_json, updated_cache_json)`. Raises `ValueError`
/// when `discussion_json` is not valid JSON.
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

/// Validate an `org/repo` repository name.
///
/// Parameters::
///
///     repo_name  the name to check, e.g. "numpy/numpy"
///
/// Returns `None` when the name is well-formed, or a human-readable
/// error string when it is not — that is, when it does not split into
/// exactly two `/`-separated parts, or either part is empty.
#[pyfunction]
fn validate_repo(repo_name: &str) -> Option<String> {
    git_refs::validate_repo(repo_name)
}

/// Extract every GitHub issue / PR reference mentioned in `text`.
///
/// Recognises full `https://github.com/org/repo/issues/N` links,
/// cross-repo `org/repo#N`, and bare `#N` (attributed to `default_repo`).
///
/// Parameters::
///
///     text          the text to scan
///     default_repo  "org/repo" that bare "#N" references belong to
///
/// Returns a list of `(repo_name, number)` tuples, deduplicated and
/// sorted; empty when `text` is empty or contains no references.
#[pyfunction]
fn extract_github_refs(text: &str, default_repo: &str) -> Vec<(String, u64)> {
    git_refs::extract_github_refs(text, default_repo)
}

/// Report whether a GitHub token is present in the environment.
///
/// Takes no parameters. Returns `True` when `GITHUB_TOKEN` or `GH_TOKEN`
/// is set to a non-empty value, `False` otherwise. The GitHub helpers
/// still work without one, but at GitHub's much lower anonymous rate
/// limit.
#[pyfunction]
fn has_git_token() -> bool {
    github::has_git_token()
}

/// Auto-detect `org/repo` from the `origin` git remote of `cwd`.
///
/// Parameters::
///
///     cwd  directory to run `git remote get-url origin` in
///
/// Returns "org/repo", or `None` when `cwd` is not inside a git
/// repository, has no `origin` remote, or its remote URL is not a
/// recognised GitHub SSH or HTTPS URL.
#[pyfunction]
fn detect_git_repo(cwd: &str) -> Option<String> {
    github::detect_git_repo(cwd)
}

/// Call the GitHub REST API and return the response as pretty-printed
/// JSON.
///
/// Parameters::
///
///     repo         "org/repo" the call is scoped to
///     path         REST path. A path naming a top-level resource
///                  ("repos/...", "search/...", with or without a leading
///                  slash) is used as given; anything else is treated as
///                  relative to `repo` and prefixed with "/repos/<repo>/".
///     truncate_at  cap the returned JSON at this many characters
///                  (keyword-only, default 80000)
///
/// Returns the response body as a string. Failures — an invalid `repo`,
/// an HTTP error — are returned as human-readable strings too, never
/// raised.
#[pyfunction]
#[pyo3(signature = (repo, path, *, truncate_at=80_000))]
fn git_api(repo: &str, path: &str, truncate_at: usize) -> String {
    github::git_api_internal(repo, path, truncate_at)
}

/// Fetch, search, or list GitHub issues, pull requests and discussions.
///
/// The mode is chosen by which arguments are set:
///
/// - `number` — fetch that one issue / PR, compacted;
/// - `query` — full-text search within the repo;
/// - neither — list the repo's issues / PRs / discussions.
///
/// All parameters are keyword-only::
///
///     repo    "org/repo" (default: auto-detected from cwd's git remote)
///     number  issue / PR number to fetch
///     query   free-text search terms
///     kind    "all" (default) | "issue" | "pr" | "discussion"
///     state   "open" (default) | "closed" | "all"
///     sort    sort key; defaults to "created" in list mode and to
///             GitHub's relevance ordering in search mode
///     limit   maximum results in search / list mode (default 20)
///     labels  comma-separated label filter, e.g. "bug,P0"
///
/// Returns a formatted string for every outcome, errors included — an
/// invalid `repo`, a repo that could not be auto-detected, or a fetch
/// failure all come back as text rather than as a raised exception.
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

/// Convert an HTML document to clean, readable plain text for LLM
/// consumption.
///
/// Strips tags, turns headings into markdown `#` prefixes, list items
/// into `- ` bullets, bold into `**text**`, images into `[image: alt]`
/// and tables into tab-separated rows, and decodes HTML entities.
///
/// Parameters::
///
///     html_str  the HTML source
///
/// Returns the extracted text.
#[pyfunction]
fn html_to_text(html_str: &str) -> String {
    html::html_to_text(html_str)
}

// ---------------------------------------------------------------------------
// read_file — bridges Python `transform=callable|"html"` to Rust closure
// ---------------------------------------------------------------------------

/// Read a file, with path-traversal protection, and return its contents
/// with line numbers.
///
/// The read is confined to `allowed_dirs`: a `file_path` that resolves
/// outside every one of them is refused.
///
/// Parameters::
///
///     file_path     path of the file to read
///     allowed_dirs  list of directories the read is confined to
///
/// Keyword-only::
///
///     section       extract the HTML element with this `id` attribute
///                   (the balanced open/close fragment)
///     start_line    slice to lines start_line..end_line, 1-indexed
///     end_line      end of the line slice (inclusive)
///     rows          [start, end] CSV row slice, 0-indexed against the
///                   data rows (after the header)
///     max_chars     cap the output at this many characters
///     transform     "html" for the built-in HTML-to-text transform, or a
///                   callable taking the raw text and returning text; it
///                   runs before section / grep selection
///     grep          regex; keep only matching lines within the selection
///     grep_context  lines of context around each grep match (default 2)
///     max_matches   cap the number of grep matches returned
///
/// Returns the formatted content as a string. Failures — a path outside
/// `allowed_dirs`, an unreadable file, an unknown `transform` name — are
/// returned as human-readable strings rather than raised.
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

/// Search for a regex pattern across files using ripgrep's engine.
///
/// Walks `source_dirs` with the `ignore` crate (parallel, `.gitignore`
/// aware) and searches with grep-searcher / grep-regex. The
/// `mcp_methods.ripgrep()` wrapper is the Claude-Grep-shaped front end
/// for this function.
///
/// Parameters::
///
///     source_dirs  list of directories (or files) to search
///     pattern      the regex to search for
///
/// Keyword-only::
///
///     glob               file-name glob, e.g. "*.py" (default "*")
///     type_filter        file type: "py", "js", "rust", …
///     output_mode        "content" (default) | "files_with_matches" | "count"
///     case_insensitive   case-insensitive search
///     multiline          multiline mode (`.` matches newlines)
///     context_before     lines before each match
///     context_after      lines after each match
///     context            symmetric context; context_before / context_after
///                        override it when set
///     line_numbers       show line numbers (default True)
///     max_results        cap the number of output entries
///     offset             skip the first N entries
///     match_limit        cap the number of matches collected per file
///     skip_dirs          directory names to skip; None uses the built-in
///                        list (.git, node_modules, target, …)
///     relative_to        base path for the printed relative paths
///     respect_gitignore  honour .gitignore rules (default True)
///     transform          callable applied to each file's raw content
///                        before searching it
///
/// Returns the formatted results as a single string.
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

/// Grep an in-memory list of lines, merging overlapping context windows.
///
/// Parameters::
///
///     text_lines  the lines to search, as a list of strings
///     pattern     the regex to search for
///     context     lines of context to keep around each match
///
/// Returns a list of dicts, one per merged context window, with keys
/// `lines` (1-indexed line numbers of the matching lines), `context_start`
/// and `context_end` (1-indexed window bounds, inclusive) and `content`
/// (the window's joined text). Raises `ValueError` on an invalid regex.
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

/// Grep the string values inside a JSON document, field by field.
///
/// Parameters::
///
///     json_str  the JSON document to search
///     pattern   the regex to search for
///     context   lines of context to keep around each match
///
/// Returns a list of dicts, one per merged context window per field,
/// with keys `field` (dotted JSON path to the matching field), `lines`
/// (1-indexed line numbers within that field's value), `context_start`,
/// `context_end` and `content`. Raises `ValueError` when `json_str` is
/// not valid JSON or `pattern` is not a valid regex.
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

/// List a directory's contents as tree-formatted text.
///
/// Parameters::
///
///     path  the directory to list
///
/// Keyword-only::
///
///     depth              recursion depth (1 = flat listing, 2+ = nested
///                        tree; default 1)
///     glob               file-name glob to filter entries
///     dirs_only          list directories only
///     relative_to        base path for the printed relative paths
///     respect_gitignore  honour .gitignore rules (default True)
///     skip_dirs          directory names to skip; None uses the built-in
///                        list (.git, node_modules, target, …)
///     include_size       append each file's size
///     annotate           callable receiving an entry's relative path and
///                        returning a string to append after it, or None
///                        to leave the entry unannotated
///
/// Returns the tree as a string. Raises `ValueError` when `path` cannot
/// be listed.
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
// Skill template — pyfunctions wrapping `render_skill_template` /
// `write_skill_template`. Operators reach these via the Python module-level
// `mcp_methods.render_skill_template(...)` and
// `mcp_methods.write_skill_template(...)` entry points.
// ---------------------------------------------------------------------------

/// Render a starter SKILL.md body as a string with the supplied
/// `name` and `description` filled into the frontmatter. The rest
/// of the optional extension fields are emitted as YAML comments.
///
/// Use `write_skill_template` for the on-disk version.
#[pyfunction]
fn render_skill_template(name: &str, description: &str) -> String {
    core_render_skill_template(name, description)
}

/// Scaffold a starter SKILL.md at `dest` and return the resolved
/// path written.
///
/// `dest` can be an existing directory (file lands at
/// `dest/<name>.md`), an explicit `.md` path (used verbatim), or a
/// not-yet-existing directory (created along with parents). Refuses
/// to overwrite — pre-existing files raise `ValueError`.
///
/// Both `name` and `description` are required. Empty values raise
/// `ValueError` — the description is the agent's only signal for
/// triggering, and a blank one undertriggers the skill silently.
#[pyfunction]
fn write_skill_template(dest: PathBuf, name: &str, description: &str) -> PyResult<PathBuf> {
    if name.trim().is_empty() {
        return Err(PyValueError::new_err("skill name must not be empty"));
    }
    if description.trim().is_empty() {
        return Err(PyValueError::new_err(
            "description must not be empty — it's the agent's only signal for triggering",
        ));
    }
    core_write_skill_template(&dest, name, description)
        .map_err(|e| PyValueError::new_err(format!("template write failed: {e}")))
}

// ---------------------------------------------------------------------------
// Skills — `#[pyclass]` thin wrappers around `ResolvedRegistry` / `Skill`
// ---------------------------------------------------------------------------

/// A single resolved skill — frontmatter metadata plus the markdown body.
/// Python consumers (FastMCP authors) read these off a [`SkillRegistry`]
/// and register them as prompts on whatever server they're hosting.
/// Snapshot of the `applies_when:` block as a plain `dict`-able
/// shape so Python callers can pre-filter their registries before
/// calling `register_skills_as_prompts`. `None` when the skill has
/// no `applies_when:` block (always active).
#[pyclass(name = "Skill", skip_from_py_object)]
#[derive(Clone)]
struct PySkill {
    name: String,
    description: String,
    body: String,
    provenance: String,
    auto_inject_hint: bool,
    references_tools: Vec<String>,
    applies_when: Option<PyAppliesWhen>,
}

#[derive(Clone)]
struct PyAppliesWhen {
    graph_has_node_type: Option<Vec<String>>,
    graph_has_property: Option<(String, String)>,
    tool_registered: Option<String>,
    extension_enabled: Option<String>,
}

impl PySkill {
    fn from_core(skill: &CoreSkill) -> Self {
        use mcp_methods::server::SkillProvenance;
        let provenance = match &skill.provenance {
            SkillProvenance::Project => "project".to_string(),
            SkillProvenance::DomainPack(path) => {
                format!("domain_pack:{}", path.display())
            }
            SkillProvenance::Bundled => "bundled".to_string(),
        };
        let applies_when = skill
            .frontmatter
            .applies_when
            .as_ref()
            .map(|aw| PyAppliesWhen {
                graph_has_node_type: aw.graph_has_node_type.clone(),
                graph_has_property: aw
                    .graph_has_property
                    .as_ref()
                    .map(|p| (p.node_type.clone(), p.prop_name.clone())),
                tool_registered: aw.tool_registered.clone(),
                extension_enabled: aw.extension_enabled.clone(),
            });
        Self {
            name: skill.name().to_string(),
            description: skill.description().to_string(),
            body: skill.body.clone(),
            provenance,
            auto_inject_hint: skill.frontmatter.auto_inject_hint,
            references_tools: skill.frontmatter.references_tools.clone(),
            applies_when,
        }
    }
}

/// The skill's name, taken from its SKILL.md frontmatter. This is the
/// key it is registered and looked up under.
#[pymethods]
impl PySkill {
    #[getter]
    fn name(&self) -> &str {
        &self.name
    }

    /// The skill's description from its SKILL.md frontmatter — the agent's
    /// only signal for when to trigger the skill.
    #[getter]
    fn description(&self) -> &str {
        &self.description
    }

    /// The skill's markdown body: everything in SKILL.md after the
    /// frontmatter block.
    #[getter]
    fn body(&self) -> &str {
        &self.body
    }

    /// Where the skill came from — one of:
    /// - `"project"` — auto-detected `<basename>.skills/` adjacent to the manifest.
    /// - `"domain_pack:<path>"` — operator-declared path from the manifest's `skills:` list.
    /// - `"bundled"` — compile-time bundled (framework or downstream binary).
    #[getter]
    fn provenance(&self) -> &str {
        &self.provenance
    }

    /// Whether the host should volunteer a pointer to this skill in its
    /// boot / tool-listing output (frontmatter `auto_inject_hint`, default
    /// `True`) rather than waiting to be asked for it.
    #[getter]
    fn auto_inject_hint(&self) -> bool {
        self.auto_inject_hint
    }

    /// Tool names this skill's body refers to, from the frontmatter
    /// `references_tools` list. Empty when the frontmatter declares none.
    #[getter]
    fn references_tools(&self) -> Vec<String> {
        self.references_tools.clone()
    }

    /// The `applies_when:` predicate block as a dict, or `None` when
    /// the skill has no predicates (always active). Keys present
    /// match the populated frontmatter fields:
    ///
    /// - `graph_has_node_type`: list[str]
    /// - `graph_has_property`: dict with `node_type` and `prop_name`
    /// - `tool_registered`: str
    /// - `extension_enabled`: str
    ///
    /// Predicate semantics are AND across populated keys. Python
    /// callers wanting to pre-filter a registry before
    /// `register_skills_as_prompts` can inspect this dict and skip
    /// skills whose predicates don't match their runtime state.
    #[getter]
    fn applies_when<'py>(&self, py: Python<'py>) -> PyResult<Option<Bound<'py, PyDict>>> {
        let Some(aw) = self.applies_when.as_ref() else {
            return Ok(None);
        };
        let dict = PyDict::new(py);
        if let Some(types) = aw.graph_has_node_type.as_ref() {
            dict.set_item("graph_has_node_type", types.clone())?;
        }
        if let Some((node_type, prop_name)) = aw.graph_has_property.as_ref() {
            let prop = PyDict::new(py);
            prop.set_item("node_type", node_type)?;
            prop.set_item("prop_name", prop_name)?;
            dict.set_item("graph_has_property", prop)?;
        }
        if let Some(tool) = aw.tool_registered.as_ref() {
            dict.set_item("tool_registered", tool)?;
        }
        if let Some(key) = aw.extension_enabled.as_ref() {
            dict.set_item("extension_enabled", key)?;
        }
        Ok(Some(dict))
    }

    /// Return `Skill(name=..., provenance=..., body_bytes=...)` — the name,
    /// provenance and body size, not the body itself.
    fn __repr__(&self) -> String {
        format!(
            "Skill(name='{}', provenance='{}', body_bytes={})",
            self.name,
            self.provenance,
            self.body.len()
        )
    }
}

/// Resolved skill set — the output of three-layer composition
/// (project → domain pack → bundled). Construct via
/// [`SkillRegistry.from_manifest`] for the common path; downstream
/// binaries with more bespoke layering should call into the Rust
/// `Registry` builder via their own pyo3 wrappers.
#[pyclass(name = "SkillRegistry")]
struct PySkillRegistry {
    inner: CoreResolvedRegistry,
}

/// Build a registry from a manifest YAML file.
///
/// Walks the manifest's `skills:` declaration (auto-detected
/// `<basename>.skills/` project layer, operator-declared paths,
/// optional bundled framework defaults) and returns the resolved
/// set. Pass `include_bundled=False` to skip framework defaults
/// — useful for tests or when a downstream binary supplies its
/// own bundled layer.
#[pymethods]
impl PySkillRegistry {
    #[staticmethod]
    #[pyo3(signature = (manifest_path, *, include_bundled=true))]
    fn from_manifest(manifest_path: PathBuf, include_bundled: bool) -> PyResult<Self> {
        let resolved = SkillsRegistry::from_manifest(&manifest_path, include_bundled)
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        Ok(Self { inner: resolved })
    }

    /// Resolve a manifest path from a graph/data path the way the
    /// `mcp-server` binary does — given e.g. `/path/foo.kdb`, looks
    /// for `/path/foo_mcp.yaml` and returns it if present, or raises
    /// `ValueError` if no sibling exists.
    #[staticmethod]
    fn find_sibling(graph_path: PathBuf) -> PyResult<PathBuf> {
        find_sibling_manifest(&graph_path).ok_or_else(|| {
            PyValueError::new_err(format!(
                "no sibling `<stem>_mcp.yaml` found next to {}",
                graph_path.display()
            ))
        })
    }

    /// All resolved skill names, sorted alphabetically.
    fn skill_names(&self) -> Vec<String> {
        self.inner.skill_names()
    }

    /// Look up a single skill by name. Returns `None` if no skill
    /// of that name was resolved.
    fn get(&self, name: &str) -> Option<PySkill> {
        self.inner.get(name).map(PySkill::from_core)
    }

    /// Iterate every resolved skill. Order matches `skill_names()`
    /// (alphabetical) so output is stable.
    fn skills(&self) -> Vec<PySkill> {
        self.inner
            .skill_names()
            .iter()
            .filter_map(|name| self.inner.get(name))
            .map(PySkill::from_core)
            .collect()
    }

    /// Non-fatal per-file load failures from the most recent
    /// `from_manifest` call. Returns a list of `{"path": str,
    /// "error": str}` dicts. Empty in the happy path. Files that
    /// fail to parse (YAML errors, missing required frontmatter,
    /// size-limit violations) are silently skipped at load time
    /// rather than failing the whole registry — this getter is the
    /// durable channel for operators to render those warnings in
    /// their boot summary instead of having to enable tracing.
    fn parse_warnings<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        self.inner
            .parse_warnings()
            .iter()
            .map(|w| {
                let d = PyDict::new(py);
                d.set_item("path", w.path.display().to_string())?;
                d.set_item("error", &w.error)?;
                Ok(d)
            })
            .collect()
    }

    /// Number of resolved skills in the registry.
    fn __len__(&self) -> usize {
        self.inner.len()
    }

    /// Whether a skill of this name resolved — `name in registry`.
    fn __contains__(&self, name: &str) -> bool {
        self.inner.get(name).is_some()
    }

    /// Return `SkillRegistry(skills=N)` with the resolved skill count.
    fn __repr__(&self) -> String {
        format!("SkillRegistry(skills={})", self.inner.len())
    }
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
    // skills
    m.add_class::<PySkill>()?;
    m.add_class::<PySkillRegistry>()?;
    m.add_function(wrap_pyfunction!(render_skill_template, m)?)?;
    m.add_function(wrap_pyfunction!(write_skill_template, m)?)?;
    Ok(())
}
