mod searcher;
mod types;
mod walker;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};
use std::collections::HashSet;
use std::path::PathBuf;

use types::{FileMatch, OutputMode};

// ---------------------------------------------------------------------------
// ripgrep_files — ripgrep-powered file search
// ---------------------------------------------------------------------------

/// Search for a regex pattern across files using ripgrep's engine.
///
/// Uses grep-searcher (mmap, SIMD, binary detection), grep-regex (literal
/// optimization), and ignore (parallel walk, .gitignore support).
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
    head_limit = 0,
    offset = 0,
    max_results = None,
    skip_dirs = None,
    relative_to = None,
    respect_gitignore = true,
    transform = None,
))]
#[allow(clippy::too_many_arguments)]
pub fn ripgrep_files(
    py: Python<'_>,
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
    head_limit: usize,
    offset: usize,
    max_results: Option<usize>,
    skip_dirs: Option<Vec<String>>,
    relative_to: Option<String>,
    respect_gitignore: bool,
    transform: Option<Py<PyAny>>,
) -> PyResult<String> {
    // Parse output mode
    let mode = match OutputMode::from_str(output_mode) {
        Ok(m) => m,
        Err(e) => return Ok(e),
    };

    // Build matcher
    let matcher = match searcher::build_matcher(pattern, case_insensitive, multiline) {
        Ok(m) => m,
        Err(e) => return Ok(e),
    };

    // Resolve context: -C sets both, but specific -A/-B override
    let ctx_before = if context_before > 0 {
        context_before
    } else {
        context
    };
    let ctx_after = if context_after > 0 {
        context_after
    } else {
        context
    };

    let skip_refs: Option<Vec<String>> = skip_dirs;
    let rel_base = relative_to.map(PathBuf::from);

    // Collect file matches
    let file_matches: Vec<FileMatch> = if let Some(ref tf) = transform {
        // Sequential path: needs GIL for transform callback
        let paths = match walker::walk_sequential(
            &source_dirs,
            glob,
            type_filter,
            skip_refs.as_deref(),
            respect_gitignore,
        ) {
            Ok(p) => p,
            Err(e) => return Ok(e),
        };
        let mut matches = Vec::new();
        let mut total = 0;
        let has_context = ctx_before > 0 || ctx_after > 0;
        let mut text_searcher = searcher::build_searcher(ctx_before, ctx_after, multiline, false);
        let mut sink = searcher::CollectSink::new(has_context);

        for path in &paths {
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let text: String = tf.call1(py, (text,))?.extract(py)?;

            sink.clear();
            if let Some((line_matches, context_lines)) =
                searcher::search_text(&text, &matcher, &mut text_searcher, &mut sink)
            {
                total += line_matches.len();
                matches.push(FileMatch {
                    path: path.clone(),
                    match_count: line_matches.len(),
                    line_matches,
                    context_lines,
                });
                if let Some(cap) = max_results {
                    if total >= cap {
                        break;
                    }
                }
            }
        }
        matches
    } else {
        // Parallel path: walk + search in parallel walker threads
        match walker::walk_and_search_parallel(
            &source_dirs,
            glob,
            type_filter,
            skip_refs.as_deref(),
            respect_gitignore,
            &matcher,
            ctx_before,
            ctx_after,
            multiline,
            max_results.unwrap_or(0),
        ) {
            Ok(m) => m,
            Err(e) => return Ok(e),
        }
    };

    // Format output
    let source_path = PathBuf::from(&source_dirs[0]);
    let output = format_output(
        &file_matches,
        pattern,
        mode,
        line_numbers,
        head_limit,
        offset,
        max_results,
        rel_base.as_deref(),
        &source_path,
        glob,
    );

    Ok(output)
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn format_output(
    file_matches: &[FileMatch],
    pattern: &str,
    mode: OutputMode,
    line_numbers: bool,
    head_limit: usize,
    offset: usize,
    max_results: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
    glob: &str,
) -> String {
    match mode {
        OutputMode::Content => format_content(
            file_matches,
            pattern,
            line_numbers,
            head_limit,
            offset,
            max_results,
            relative_to,
            source_path,
            glob,
        ),
        OutputMode::FilesWithMatches => format_files(
            file_matches,
            head_limit,
            offset,
            max_results,
            relative_to,
            source_path,
        ),
        OutputMode::Count => format_count(
            file_matches,
            head_limit,
            offset,
            max_results,
            relative_to,
            source_path,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn format_content(
    file_matches: &[FileMatch],
    pattern: &str,
    line_numbers: bool,
    head_limit: usize,
    offset: usize,
    max_results: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
    glob: &str,
) -> String {
    let mut lines: Vec<String> = Vec::new();

    for fm in file_matches {
        let rel = walker::relativize(&fm.path, relative_to, source_path);

        if fm.context_lines.is_empty() {
            // Fast path: no context — matches are already in order, skip HashSet/sort/dedup
            for lm in &fm.line_matches {
                if line_numbers {
                    lines.push(format!(
                        "  {}:{}:{} {}",
                        rel, lm.line_number, ':', lm.content
                    ));
                } else {
                    lines.push(format!("  {}  {}", rel, lm.content));
                }
            }
        } else {
            // Context path: merge matches + context lines, sorted by line number
            let match_lines: HashSet<u64> = fm.line_matches.iter().map(|m| m.line_number).collect();

            let mut all_lines: Vec<(u64, &str, bool)> = Vec::new();
            for lm in &fm.line_matches {
                all_lines.push((lm.line_number, &lm.content, true));
            }
            for (ln, content) in &fm.context_lines {
                if !match_lines.contains(ln) {
                    all_lines.push((*ln, content, false));
                }
            }
            all_lines.sort_by_key(|(ln, _, _)| *ln);
            all_lines.dedup_by_key(|(ln, _, _)| *ln);

            let mut prev_ln: Option<u64> = None;
            for (ln, content, is_match) in &all_lines {
                if let Some(prev) = prev_ln {
                    if *ln > prev + 1 {
                        lines.push("--".to_string());
                    }
                }
                prev_ln = Some(*ln);

                if line_numbers {
                    let sep = if *is_match { ':' } else { '-' };
                    lines.push(format!("  {}:{}{} {}", rel, ln, sep, content));
                } else {
                    lines.push(format!("  {}  {}", rel, content));
                }
            }
        }
    }

    // Apply offset + head_limit
    if offset > 0 && offset < lines.len() {
        lines = lines[offset..].to_vec();
    } else if offset >= lines.len() && !lines.is_empty() {
        lines.clear();
    }
    if head_limit > 0 && lines.len() > head_limit {
        lines.truncate(head_limit);
    }

    if lines.is_empty() {
        return format!("No matches for '{}' in {} files.", pattern, glob);
    }

    let total_matches: usize = file_matches.iter().map(|fm| fm.match_count).sum();
    let mut header = format!("Found {} match(es) for '{}'", total_matches, pattern);
    if let Some(cap) = max_results {
        if total_matches >= cap {
            header.push_str(&format!(" (capped at {})", cap));
        }
    }
    header.push(':');

    format!("{}\n{}", header, lines.join("\n"))
}

fn format_files(
    file_matches: &[FileMatch],
    head_limit: usize,
    offset: usize,
    max_results: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
) -> String {
    let mut paths: Vec<String> = file_matches
        .iter()
        .map(|fm| walker::relativize(&fm.path, relative_to, source_path))
        .collect();

    if offset > 0 && offset < paths.len() {
        paths = paths[offset..].to_vec();
    } else if offset >= paths.len() && !paths.is_empty() {
        paths.clear();
    }
    if head_limit > 0 && paths.len() > head_limit {
        paths.truncate(head_limit);
    }

    if paths.is_empty() {
        return "No matching files.".to_string();
    }

    let mut result = paths.join("\n");
    if let Some(cap) = max_results {
        let total_matches: usize = file_matches.iter().map(|fm| fm.match_count).sum();
        if total_matches >= cap {
            result.push_str(&format!(
                "\n\n(results may be incomplete — hit {} match limit across {} files)",
                cap,
                file_matches.len()
            ));
        }
    }
    result
}

fn format_count(
    file_matches: &[FileMatch],
    head_limit: usize,
    offset: usize,
    max_results: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
) -> String {
    let mut entries: Vec<String> = file_matches
        .iter()
        .map(|fm| {
            let rel = walker::relativize(&fm.path, relative_to, source_path);
            format!("{}:{}", rel, fm.match_count)
        })
        .collect();

    if offset > 0 && offset < entries.len() {
        entries = entries[offset..].to_vec();
    } else if offset >= entries.len() && !entries.is_empty() {
        entries.clear();
    }
    if head_limit > 0 && entries.len() > head_limit {
        entries.truncate(head_limit);
    }

    if entries.is_empty() {
        return "No matching files.".to_string();
    }

    let mut result = entries.join("\n");
    if let Some(cap) = max_results {
        let total_matches: usize = file_matches.iter().map(|fm| fm.match_count).sum();
        if total_matches >= cap {
            result.push_str(&format!(
                "\n\n(results may be incomplete — hit {} match limit across {} files)",
                cap,
                file_matches.len()
            ));
        }
    }
    result
}

// ---------------------------------------------------------------------------
// ripgrep_lines — search through a list of text lines with context
// ---------------------------------------------------------------------------

/// Grep through lines with context, merging overlapping windows.
/// Returns a list of dicts with keys: lines, context_start, context_end, content.
#[pyfunction]
#[pyo3(signature = (text_lines, pattern, context))]
pub fn ripgrep_lines(
    py: Python<'_>,
    text_lines: Vec<String>,
    pattern: &str,
    context: usize,
) -> PyResult<Py<PyAny>> {
    let regex = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "Invalid regex: {}",
                e
            )));
        }
    };

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

    let result = PyList::empty(py);
    for g in groups {
        let content = text_lines[g.start..g.end].join("\n");
        let dict = PyDict::new(py);
        dict.set_item("lines", g.lines)?;
        dict.set_item("context_start", g.start + 1)?;
        dict.set_item("context_end", g.end)?;
        dict.set_item("content", content)?;
        result.append(dict)?;
    }

    Ok(result.into_any().unbind())
}
