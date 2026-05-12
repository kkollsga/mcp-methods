//! ripgrep-powered file + line search.
//!
//! Pure Rust — Python bindings are in `mcp-methods-py`. Two entry
//! points:
//! - [`ripgrep_files`] walks a directory tree and searches every file
//!   matching the glob/type filter. Optional `transform` callback runs
//!   per-file before search (the Python wrapper bridges a callable into
//!   `&dyn Fn(&str) -> String`).
//! - [`ripgrep_lines`] greps through a `Vec<String>` of lines with
//!   context-window merging. Returns structured matches.

mod searcher;
mod types;
mod walker;

use std::path::PathBuf;

use types::{FileMatch, OutputMode};

/// Result of [`ripgrep_lines`] — one entry per merged context window.
#[derive(Debug, Clone)]
pub struct RipgrepLinesGroup {
    /// 1-indexed line numbers of the matching lines in this window.
    pub lines: Vec<usize>,
    /// 1-indexed start line of the window (inclusive).
    pub context_start: usize,
    /// 1-indexed end line of the window (inclusive).
    pub context_end: usize,
    /// Joined content of the window.
    pub content: String,
}

/// Optional knobs for [`ripgrep_files`].
#[derive(Default)]
pub struct RipgrepFilesOpts<'a> {
    /// File-name glob (default `"*"`).
    pub glob: Option<&'a str>,
    /// File-type filter (`"py"`, `"rust"`, …).
    pub type_filter: Option<&'a str>,
    /// `"content"` (default) | `"files_with_matches"` | `"count"`.
    pub output_mode: Option<&'a str>,
    pub case_insensitive: bool,
    pub multiline: bool,
    pub context_before: usize,
    pub context_after: usize,
    /// Symmetric context — overridden by `context_before` / `context_after` when set.
    pub context: usize,
    pub line_numbers: bool,
    pub max_results: Option<usize>,
    pub offset: usize,
    pub match_limit: Option<usize>,
    pub skip_dirs: Option<&'a [String]>,
    pub relative_to: Option<&'a str>,
    pub respect_gitignore: bool,
    /// Per-file transform applied to raw content before search. Used by
    /// the Python wrapper to bridge a Python callable; pure-Rust callers
    /// typically leave this `None`.
    pub transform: Option<&'a dyn Fn(&str) -> String>,
}

impl<'a> RipgrepFilesOpts<'a> {
    /// Builder-style helper for the most common defaults.
    pub fn new() -> Self {
        Self {
            line_numbers: true,
            respect_gitignore: true,
            ..Default::default()
        }
    }
}

/// Search for a regex pattern across files using ripgrep's engine.
///
/// Uses grep-searcher (mmap, SIMD, binary detection), grep-regex (literal
/// optimization), and ignore (parallel walk, .gitignore support).
pub fn ripgrep_files(source_dirs: &[String], pattern: &str, opts: &RipgrepFilesOpts) -> String {
    let glob = opts.glob.unwrap_or("*");
    let output_mode = opts.output_mode.unwrap_or("content");

    let mode = match OutputMode::from_str(output_mode) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let matcher = match searcher::build_matcher(pattern, opts.case_insensitive, opts.multiline) {
        Ok(m) => m,
        Err(e) => return e,
    };

    let ctx_before = if opts.context_before > 0 {
        opts.context_before
    } else {
        opts.context
    };
    let ctx_after = if opts.context_after > 0 {
        opts.context_after
    } else {
        opts.context
    };

    let rel_base = opts.relative_to.map(PathBuf::from);

    let file_matches: Vec<FileMatch> = if let Some(transform) = opts.transform {
        // Sequential path: caller-supplied transform runs per-file before search.
        let paths = match walker::walk_sequential(
            source_dirs,
            glob,
            opts.type_filter,
            opts.skip_dirs,
            opts.respect_gitignore,
        ) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let mut matches = Vec::new();
        let mut total = 0;
        let has_context = ctx_before > 0 || ctx_after > 0;
        let mut text_searcher =
            searcher::build_searcher(ctx_before, ctx_after, opts.multiline, false);
        let mut sink = searcher::CollectSink::new(has_context);

        for path in &paths {
            let raw = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(_) => continue,
            };
            let text = transform(&raw);

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
                if let Some(cap) = opts.match_limit {
                    if total >= cap {
                        break;
                    }
                }
            }
        }
        matches
    } else {
        // Parallel path: walk + search in parallel walker threads.
        match walker::walk_and_search_parallel(
            source_dirs,
            glob,
            opts.type_filter,
            opts.skip_dirs,
            opts.respect_gitignore,
            &matcher,
            ctx_before,
            ctx_after,
            opts.multiline,
            opts.match_limit.unwrap_or(0),
        ) {
            Ok(m) => m,
            Err(e) => return e,
        }
    };

    let source_path = PathBuf::from(&source_dirs[0]);
    format_output(
        &file_matches,
        pattern,
        mode,
        opts.line_numbers,
        opts.max_results,
        opts.offset,
        opts.match_limit,
        rel_base.as_deref(),
        &source_path,
        glob,
    )
}

/// Grep through `text_lines` with context-window merging.
/// Returns one [`RipgrepLinesGroup`] per merged window.
pub fn ripgrep_lines(
    text_lines: &[String],
    pattern: &str,
    context: usize,
) -> Result<Vec<RipgrepLinesGroup>, String> {
    let regex = regex::Regex::new(pattern).map_err(|e| format!("Invalid regex: {}", e))?;

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

    Ok(groups
        .into_iter()
        .map(|g| {
            let content = text_lines[g.start..g.end].join("\n");
            RipgrepLinesGroup {
                lines: g.lines,
                context_start: g.start + 1,
                context_end: g.end,
                content,
            }
        })
        .collect())
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
    max_results: Option<usize>,
    offset: usize,
    match_limit: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
    glob: &str,
) -> String {
    match mode {
        OutputMode::Content => format_content(
            file_matches,
            pattern,
            line_numbers,
            max_results,
            offset,
            match_limit,
            relative_to,
            source_path,
            glob,
        ),
        OutputMode::FilesWithMatches => format_files(
            file_matches,
            max_results,
            offset,
            match_limit,
            relative_to,
            source_path,
        ),
        OutputMode::Count => format_count(
            file_matches,
            max_results,
            offset,
            match_limit,
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
    max_results: Option<usize>,
    offset: usize,
    match_limit: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
    glob: &str,
) -> String {
    let estimated: usize = file_matches
        .iter()
        .map(|fm| fm.line_matches.len() + fm.context_lines.len())
        .sum();
    let mut lines: Vec<String> = Vec::with_capacity(estimated);

    for fm in file_matches {
        let rel = walker::relativize(&fm.path, relative_to, source_path);

        if fm.context_lines.is_empty() {
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
            let matches = &fm.line_matches;
            let contexts = &fm.context_lines;
            let mut mi = 0;
            let mut ci = 0;
            let mut prev_ln: Option<u64> = None;

            while mi < matches.len() || ci < contexts.len() {
                let (ln, content, is_match) = match (matches.get(mi), contexts.get(ci)) {
                    (Some(m), Some((cln, _))) if m.line_number <= *cln => {
                        if *cln == m.line_number {
                            ci += 1;
                        }
                        mi += 1;
                        (m.line_number, m.content.as_str(), true)
                    }
                    (Some(_), Some((cln, cc))) => {
                        ci += 1;
                        (*cln, cc.as_str(), false)
                    }
                    (Some(m), None) => {
                        mi += 1;
                        (m.line_number, m.content.as_str(), true)
                    }
                    (None, Some((cln, cc))) => {
                        ci += 1;
                        (*cln, cc.as_str(), false)
                    }
                    (None, None) => unreachable!(),
                };

                if let Some(prev) = prev_ln {
                    if ln > prev + 1 {
                        lines.push("--".to_string());
                    }
                }
                prev_ln = Some(ln);

                if line_numbers {
                    let sep = if is_match { ':' } else { '-' };
                    lines.push(format!("  {}:{}{} {}", rel, ln, sep, content));
                } else {
                    lines.push(format!("  {}  {}", rel, content));
                }
            }
        }
    }

    if offset > 0 && offset < lines.len() {
        lines.drain(..offset);
    } else if offset >= lines.len() && !lines.is_empty() {
        lines.clear();
    }
    if let Some(limit) = max_results {
        if lines.len() > limit {
            lines.truncate(limit);
        }
    }

    if lines.is_empty() {
        return format!("No matches for '{}' in {} files.", pattern, glob);
    }

    let total_matches: usize = file_matches.iter().map(|fm| fm.match_count).sum();
    let mut header = format!("Found {} match(es) for '{}'", total_matches, pattern);
    if let Some(cap) = match_limit {
        if total_matches >= cap {
            header.push_str(&format!(" (capped at {})", cap));
        }
    }
    header.push(':');

    format!("{}\n{}", header, lines.join("\n"))
}

fn format_files(
    file_matches: &[FileMatch],
    max_results: Option<usize>,
    offset: usize,
    match_limit: Option<usize>,
    relative_to: Option<&std::path::Path>,
    source_path: &std::path::Path,
) -> String {
    let mut paths: Vec<String> = file_matches
        .iter()
        .map(|fm| walker::relativize(&fm.path, relative_to, source_path))
        .collect();

    if offset > 0 && offset < paths.len() {
        paths.drain(..offset);
    } else if offset >= paths.len() && !paths.is_empty() {
        paths.clear();
    }
    if let Some(limit) = max_results {
        if paths.len() > limit {
            paths.truncate(limit);
        }
    }

    if paths.is_empty() {
        return "No matching files.".to_string();
    }

    let mut result = paths.join("\n");
    if let Some(cap) = match_limit {
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
    max_results: Option<usize>,
    offset: usize,
    match_limit: Option<usize>,
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
        entries.drain(..offset);
    } else if offset >= entries.len() && !entries.is_empty() {
        entries.clear();
    }
    if let Some(limit) = max_results {
        if entries.len() > limit {
            entries.truncate(limit);
        }
    }

    if entries.is_empty() {
        return "No matching files.".to_string();
    }

    let mut result = entries.join("\n");
    if let Some(cap) = match_limit {
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
