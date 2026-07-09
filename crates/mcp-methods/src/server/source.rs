//! Source-file tooling: ``read_source`` / ``grep`` / ``list_source``.
//!
//! Operates on a *dynamic* source root provider — a closure returning
//! the active list of allowed dirs at the moment of each tool call.
//! GitHub-workspace mode wires this to the active repo's path; local-
//! workspace mode wires it to the bound root (re-routed on each
//! `set_root_dir` call); `--source-root` and `--watch` modes wire it
//! to a fixed root. An empty list signals "no active source" and the
//! tools return a friendly error.
//!
//! All path traversal protection is done by canonicalising the
//! resolved path against the allowed dirs before any I/O happens.
//!
//! Design: stay close to the existing Python `mcp_methods` semantics
//! (line numbers, header format, "showing N of M matches", etc.) so a
//! manifest written for the legacy Python server returns visually
//! similar output.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use grep_matcher::Matcher;
use grep_regex::RegexMatcherBuilder;
use grep_searcher::sinks::UTF8;
use grep_searcher::SearcherBuilder;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex::Regex;

/// Provider returning the current allowed source dirs.
pub type SourceRootsProvider = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

// ---------------------------------------------------------------------------
// read_source
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ReadOpts {
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
    pub grep: Option<String>,
    pub grep_context: Option<usize>,
    pub max_matches: Option<usize>,
    pub max_chars: Option<usize>,
    /// When set, read the file's content at this git revision
    /// (tag / branch / SHA) via `git show <rev>:<path>` instead of the
    /// working tree. The slicing / grep / max_chars pipeline is applied
    /// to the historical content unchanged.
    pub rev: Option<String>,
}

/// Read a file from one of the allowed source dirs.
///
/// Returns a user-facing string. Path traversal attempts and missing
/// files surface as ``Error: …`` strings rather than panics, mirroring
/// the existing Python-server behaviour so the agent sees a clean
/// error in tool output rather than an MCP error envelope.
pub fn read_source(file_path: &str, allowed_dirs: &[String], opts: &ReadOpts) -> String {
    if let Some(rev) = opts.rev.as_deref() {
        return read_source_at_rev(file_path, rev, allowed_dirs, opts);
    }
    let resolved = match resolve_under_roots(file_path, allowed_dirs) {
        Some(p) => p,
        None => return format!("Error: file not found or access denied: {file_path}"),
    };
    let raw = match fs::read_to_string(&resolved) {
        Ok(s) => s,
        Err(e) => return format!("Error reading file: {e}"),
    };
    apply_read_options(file_path, &raw, opts)
}

/// Read `file_path` at git revision `rev` via `git show <rev>:<path>`,
/// then run the same slicing / grep / max_chars pipeline as a live read.
///
/// The path is validated to be a safe repo-relative path *before* it
/// reaches git (no absolute paths, no `..` escape), so a rev read can't
/// be used to smuggle a traversal past the sandbox. The active source
/// root must be a git work tree; otherwise a clear error is returned.
fn read_source_at_rev(
    file_path: &str,
    rev: &str,
    allowed_dirs: &[String],
    opts: &ReadOpts,
) -> String {
    if !is_safe_relative_path(file_path) {
        return format!("Error: file not found or access denied: {file_path}");
    }
    let root = match git_work_tree_root(allowed_dirs) {
        Some(r) => r,
        None => {
            return "Error: rev-aware read requires a git repository as the active source root, \
                    but the current root is not a git work tree."
                .to_string()
        }
    };
    // `<rev>:<path>` — git treats the path as relative to the repo root.
    let spec = format!("{rev}:{file_path}");
    let out = match std::process::Command::new("git")
        .args(["-C", &root, "show", &spec])
        .output()
    {
        Ok(o) => o,
        Err(e) => return format!("Error: failed to run git: {e}"),
    };
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let msg = stderr.trim();
        let msg = msg.strip_prefix("fatal: ").unwrap_or(msg);
        return format!("Error reading {file_path}@{rev}: {msg}");
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    apply_read_options(&format!("{file_path}@{rev}"), &raw, opts)
}

/// True if `p` is a repo-relative path that cannot escape the repo root:
/// not absolute, and with no `..` / root / prefix components.
fn is_safe_relative_path(p: &str) -> bool {
    use std::path::Component;
    let path = Path::new(p);
    if path.is_absolute() {
        return false;
    }
    path.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Return the first allowed dir that is a git work tree, if any.
fn git_work_tree_root(allowed_dirs: &[String]) -> Option<String> {
    for d in allowed_dirs {
        if let Ok(out) = std::process::Command::new("git")
            .args(["-C", d, "rev-parse", "--is-inside-work-tree"])
            .output()
        {
            if out.status.success() && String::from_utf8_lossy(&out.stdout).trim() == "true" {
                return Some(d.clone());
            }
        }
    }
    None
}

fn apply_read_options(file_path: &str, raw: &str, opts: &ReadOpts) -> String {
    let all_lines: Vec<&str> = raw.lines().collect();
    let total = all_lines.len();

    let (selected, start) = if opts.start_line.is_some() || opts.end_line.is_some() {
        let s = opts.start_line.unwrap_or(1).max(1);
        let e = opts.end_line.unwrap_or(total).min(total);
        let sel: Vec<&str> = all_lines
            .get(s.saturating_sub(1)..e.min(all_lines.len()))
            .unwrap_or(&[])
            .to_vec();
        (sel, s)
    } else {
        (all_lines.clone(), 1usize)
    };

    if let Some(pattern) = opts.grep.as_deref() {
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return format!("Error: invalid grep pattern: {e}"),
        };
        let ctx = opts.grep_context.unwrap_or(2);
        let numbered: Vec<(usize, &str)> = selected
            .iter()
            .enumerate()
            .map(|(i, line)| (start + i, *line))
            .collect();
        let gr = grep_lines(&numbered, &re, ctx, opts.max_matches);
        let match_label = if gr.shown < gr.total {
            format!("showing {} of {} matches", gr.shown, gr.total)
        } else {
            format!("{} matches", gr.total)
        };
        let header = format!("{file_path}  ({match_label} in {total} lines)");
        if gr.lines.is_empty() {
            return header;
        }
        let mut text = format!("{header}\n{}", gr.lines.join("\n"));
        truncate_at_max_chars(&mut text, opts.max_chars, gr.total);
        return text;
    }

    let body = selected.join("\n");
    let mut text = if opts.start_line.is_some() || opts.end_line.is_some() {
        let s = opts.start_line.unwrap_or(1).max(1);
        let e = opts.end_line.unwrap_or(total).min(total);
        format!("{file_path}  (lines {s}-{e} of {total})\n{body}")
    } else {
        format!("{file_path}  ({total} lines)\n{body}")
    };
    truncate_at_max_chars(&mut text, opts.max_chars, 0);
    text
}

struct GrepResult {
    total: usize,
    shown: usize,
    lines: Vec<String>,
}

/// In-memory grep over (line_number, line_text) pairs.
fn grep_lines(
    lines: &[(usize, &str)],
    re: &Regex,
    context: usize,
    max_matches: Option<usize>,
) -> GrepResult {
    let mut match_idx: Vec<usize> = Vec::new();
    for (i, (_, content)) in lines.iter().enumerate() {
        if re.is_match(content) {
            match_idx.push(i);
        }
    }
    let total = match_idx.len();
    let shown_idx = if let Some(cap) = max_matches {
        match_idx.into_iter().take(cap).collect::<Vec<_>>()
    } else {
        match_idx
    };
    let shown = shown_idx.len();

    if shown_idx.is_empty() {
        return GrepResult {
            total,
            shown: 0,
            lines: Vec::new(),
        };
    }

    // Build inclusive (start, end) windows for each match, then merge overlapping.
    let mut windows: Vec<(usize, usize)> = shown_idx
        .iter()
        .map(|&i| {
            (
                i.saturating_sub(context),
                (i + context).min(lines.len() - 1),
            )
        })
        .collect();
    windows.sort_by_key(|w| w.0);

    let mut merged: Vec<(usize, usize)> = Vec::new();
    for w in windows {
        if let Some(last) = merged.last_mut() {
            if w.0 <= last.1 + 1 {
                last.1 = last.1.max(w.1);
                continue;
            }
        }
        merged.push(w);
    }

    let mut out: Vec<String> = Vec::new();
    for (k, (s, e)) in merged.iter().enumerate() {
        if k > 0 {
            out.push("--".to_string());
        }
        for &(lineno, text) in lines.iter().take(*e + 1).skip(*s) {
            out.push(format!("{lineno:>6}: {text}"));
        }
    }

    GrepResult {
        total,
        shown,
        lines: out,
    }
}

fn truncate_at_max_chars(text: &mut String, max_chars: Option<usize>, total_matches: usize) {
    let Some(mc) = max_chars else { return };
    if text.len() <= mc {
        return;
    }
    let mut end = mc;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    if total_matches > 0 {
        text.push_str(&format!(
            "\n\n[... truncated at {mc} chars — {total_matches} matches total]"
        ));
    } else {
        text.push_str(&format!("\n\n[... truncated at {mc} chars]"));
    }
}

// ---------------------------------------------------------------------------
// grep — ripgrep across files
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct GrepOpts {
    pub glob: Option<String>,
    pub context: usize,
    pub max_results: Option<usize>,
    pub case_insensitive: bool,
}

pub fn grep(allowed_dirs: &[String], pattern: &str, opts: &GrepOpts) -> String {
    if allowed_dirs.is_empty() {
        return "Error: no source roots configured.".to_string();
    }
    let matcher = match RegexMatcherBuilder::new()
        .case_insensitive(opts.case_insensitive)
        .build(pattern)
    {
        Ok(m) => m,
        Err(e) => return format!("Error: invalid regex pattern: {e}"),
    };

    let primary = PathBuf::from(&allowed_dirs[0]);
    let mut walker = WalkBuilder::new(&primary);
    for d in allowed_dirs.iter().skip(1) {
        walker.add(d);
    }
    walker
        .standard_filters(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .hidden(true);

    if let Some(g) = &opts.glob {
        if !g.is_empty() && g != "*" {
            let mut overrides = OverrideBuilder::new(&primary);
            if let Err(e) = overrides.add(g) {
                return format!("Error: invalid glob pattern '{g}': {e}");
            }
            match overrides.build() {
                Ok(ov) => {
                    walker.overrides(ov);
                }
                Err(e) => return format!("Error: failed to compile glob '{g}': {e}"),
            }
        }
    }

    let mut searcher = SearcherBuilder::new()
        .before_context(opts.context)
        .after_context(opts.context)
        .build();

    let mut output: Vec<String> = Vec::new();
    let mut total_matches: usize = 0;
    let cap = opts.max_results;

    'walk: for result in walker.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().map(|ft| ft.is_file()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let mut path_matches: Vec<(u64, String, bool)> = Vec::new();
        let sink_result = searcher.search_path(
            &matcher,
            path,
            UTF8(|lnum, line| {
                let hit = matcher.find(line.as_bytes()).ok().flatten().is_some();
                path_matches.push((lnum, line.trim_end().to_string(), hit));
                Ok(true)
            }),
        );
        if sink_result.is_err() {
            continue;
        }
        if path_matches.is_empty() {
            continue;
        }
        let rel = path.strip_prefix(&primary).unwrap_or(path);
        let prefix = rel.display().to_string();
        for (lnum, content, is_match) in path_matches {
            let sep = if is_match { ":" } else { "-" };
            if is_match {
                total_matches += 1;
            }
            output.push(format!("{prefix}{sep}{lnum}{sep}{content}"));
            if let Some(c) = cap {
                if total_matches >= c {
                    break 'walk;
                }
            }
        }
    }

    if output.is_empty() {
        return format!("No matches for pattern '{pattern}'.");
    }
    let mut text = output.join("\n");
    if let Some(c) = cap {
        if total_matches >= c {
            text.push_str(&format!(
                "\n\n(showing first {c} matches — pass max_results=None for all)"
            ));
        }
    }
    text
}

// ---------------------------------------------------------------------------
// list_source — directory listing
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Clone)]
pub struct ListOpts {
    pub depth: usize,
    pub glob: Option<String>,
    pub dirs_only: bool,
}

pub fn list_source(target: &Path, primary_root: &Path, opts: &ListOpts) -> String {
    if !target.exists() {
        return format!("Error: path '{}' does not exist.", target.display());
    }
    if !target.is_dir() {
        return format!("Error: path '{}' is not a directory.", target.display());
    }

    let depth = if opts.depth == 0 { 1 } else { opts.depth };
    let glob_re = opts
        .glob
        .as_deref()
        .map(glob_to_regex)
        .transpose()
        .unwrap_or_else(|e| {
            tracing::warn!("ignoring invalid glob: {e}");
            None
        });

    let mut entries: Vec<String> = Vec::new();
    walk_listing(
        target,
        primary_root,
        opts,
        glob_re.as_ref(),
        0,
        depth,
        &mut entries,
    );

    if entries.is_empty() {
        return format!("No entries in '{}'.", target.display());
    }
    entries.join("\n")
}

fn walk_listing(
    dir: &Path,
    primary_root: &Path,
    opts: &ListOpts,
    glob_re: Option<&Regex>,
    current_depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
) {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut children: Vec<_> = read.filter_map(|e| e.ok()).collect();
    children.sort_by_key(|e| e.file_name());

    for entry in children {
        let path = entry.path();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if opts.dirs_only && !is_dir {
            continue;
        }
        if let Some(re) = glob_re {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !is_dir && !re.is_match(&name) {
                continue;
            }
        }
        let rel = path
            .strip_prefix(primary_root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let indent = "  ".repeat(current_depth);
        let suffix = if is_dir { "/" } else { "" };
        out.push(format!("{indent}{rel}{suffix}"));
        if is_dir && current_depth + 1 < max_depth {
            walk_listing(
                &path,
                primary_root,
                opts,
                glob_re,
                current_depth + 1,
                max_depth,
                out,
            );
        }
    }
}

/// Translate a shell glob to a regex anchored at start/end.
fn glob_to_regex(glob: &str) -> Result<Regex, regex::Error> {
    let mut out = String::with_capacity(glob.len() * 2 + 4);
    out.push('^');
    let mut chars = glob.chars().peekable();
    for c in &mut chars {
        match c {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out.push('$');
    Regex::new(&out)
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Resolve ``file_path`` against the allowed dirs and verify the canonical
/// path lives under at least one of them. Returns ``None`` when the file
/// is missing or the path traversal lands outside the sandbox.
pub fn resolve_under_roots(file_path: &str, allowed_dirs: &[String]) -> Option<PathBuf> {
    if allowed_dirs.is_empty() {
        return None;
    }
    let canon_dirs: Vec<PathBuf> = allowed_dirs
        .iter()
        .filter_map(|d| PathBuf::from(d).canonicalize().ok())
        .collect();

    for (i, d) in allowed_dirs.iter().enumerate() {
        let candidate = PathBuf::from(d).join(file_path);
        if let Ok(canon) = candidate.canonicalize() {
            if let Some(dir_canon) = canon_dirs.get(i) {
                if canon.starts_with(dir_canon) && canon.exists() {
                    return Some(canon);
                }
            }
        }
    }

    let abs = PathBuf::from(file_path);
    if let Ok(canon) = abs.canonicalize() {
        for dir_canon in &canon_dirs {
            if canon.starts_with(dir_canon) && canon.exists() {
                return Some(canon);
            }
        }
    }
    None
}

/// Resolve a path under the first allowed dir for directory listing.
/// Differs from [`resolve_under_roots`] in that it accepts directories,
/// non-existent paths included only after canonicalisation succeeds.
pub fn resolve_dir_under_roots(path: &str, allowed_dirs: &[String]) -> Option<PathBuf> {
    if allowed_dirs.is_empty() {
        return None;
    }
    let primary = PathBuf::from(&allowed_dirs[0]);
    let canon_primary = primary.canonicalize().ok()?;
    let candidate = if path == "." {
        canon_primary.clone()
    } else {
        primary.join(path).canonicalize().ok()?
    };
    let canon_dirs: Vec<PathBuf> = allowed_dirs
        .iter()
        .filter_map(|d| PathBuf::from(d).canonicalize().ok())
        .collect();
    for d in &canon_dirs {
        if candidate.starts_with(d) {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("hello.txt"),
            "line one\nline two with marker\nline three\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("data.json"), "{\"name\": \"Alice\"}\n").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("nested.txt"), "nested file\n").unwrap();
        dir
    }

    #[test]
    fn read_source_full_file() {
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let out = read_source("hello.txt", &roots, &ReadOpts::default());
        assert!(out.contains("line one"));
        assert!(out.contains("line three"));
    }

    #[test]
    fn read_source_grep_filter() {
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            grep: Some("marker".to_string()),
            ..Default::default()
        };
        let out = read_source("hello.txt", &roots, &opts);
        assert!(out.contains("marker"));
        assert!(out.contains("matches"));
    }

    #[test]
    fn read_source_blocks_traversal() {
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let out = read_source("../escape.txt", &roots, &ReadOpts::default());
        assert!(out.starts_with("Error:"));
    }

    #[test]
    fn read_source_line_range() {
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            start_line: Some(2),
            end_line: Some(2),
            ..Default::default()
        };
        let out = read_source("hello.txt", &roots, &opts);
        assert!(out.contains("line two with marker"));
        assert!(!out.contains("line one"));
        assert!(!out.contains("line three"));
    }

    #[test]
    fn grep_finds_pattern() {
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let out = grep(&roots, "Alice", &GrepOpts::default());
        assert!(out.contains("data.json"));
    }

    #[test]
    fn grep_glob_filter() {
        let dir = make_tree();
        std::fs::write(dir.path().join("extra.json"), "marker in json\n").unwrap();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = GrepOpts {
            glob: Some("*.txt".to_string()),
            ..Default::default()
        };
        let out = grep(&roots, "marker", &opts);
        assert!(out.contains("hello.txt"));
        assert!(!out.contains("extra.json"));
    }

    #[test]
    fn grep_no_matches() {
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let out = grep(&roots, "xyznotfound", &GrepOpts::default());
        assert!(out.contains("No matches"));
    }

    #[test]
    fn list_source_root() {
        let dir = make_tree();
        let primary = dir.path();
        let out = list_source(primary, primary, &ListOpts::default());
        assert!(out.contains("hello.txt"));
        assert!(out.contains("data.json"));
    }

    #[test]
    fn list_source_dirs_only() {
        let dir = make_tree();
        let primary = dir.path();
        let opts = ListOpts {
            dirs_only: true,
            depth: 1,
            ..Default::default()
        };
        let out = list_source(primary, primary, &opts);
        assert!(out.contains("sub"));
        assert!(!out.contains("hello.txt"));
    }

    #[test]
    fn list_source_subdir() {
        let dir = make_tree();
        let target = dir.path().join("sub");
        let out = list_source(&target, dir.path(), &ListOpts::default());
        assert!(out.contains("nested.txt"));
    }

    #[test]
    fn glob_translation() {
        let re = glob_to_regex("*.py").unwrap();
        assert!(re.is_match("foo.py"));
        assert!(!re.is_match("foo.rs"));
        let re = glob_to_regex("test_*").unwrap();
        assert!(re.is_match("test_x"));
        assert!(!re.is_match("xtest"));
    }

    #[test]
    fn resolve_blocks_escape() {
        let dir = make_tree();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "x").unwrap();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let escape = format!(
            "../{}/secret.txt",
            outside.path().file_name().unwrap().to_string_lossy()
        );
        assert!(resolve_under_roots(&escape, &roots).is_none());
    }

    // --- rev-aware read_source ---------------------------------------------

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git spawn");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Init a repo with two commits of `file.txt`, tagging the first as
    /// `v1`. Returns the tempdir.
    fn make_git_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        git(p, &["init", "-q"]);
        std::fs::write(p.join("file.txt"), "old content v1\n").unwrap();
        git(p, &["add", "file.txt"]);
        git(p, &["commit", "-q", "-m", "v1"]);
        git(p, &["tag", "v1"]);
        std::fs::write(p.join("file.txt"), "new content v2\n").unwrap();
        git(p, &["add", "file.txt"]);
        git(p, &["commit", "-q", "-m", "v2"]);
        dir
    }

    #[test]
    fn read_source_rev_reads_historical_content() {
        let dir = make_git_repo();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        // Working tree has v2; rev=v1 must surface the old content.
        let opts = ReadOpts {
            rev: Some("v1".to_string()),
            ..Default::default()
        };
        let out = read_source("file.txt", &roots, &opts);
        assert!(out.contains("old content v1"), "got: {out}");
        assert!(!out.contains("new content v2"), "got: {out}");
        assert!(out.contains("file.txt@v1"), "header missing rev: {out}");

        // Without rev, the working tree (v2) is read.
        let live = read_source("file.txt", &roots, &ReadOpts::default());
        assert!(live.contains("new content v2"), "got: {live}");
    }

    #[test]
    fn read_source_rev_pipeline_applies() {
        let dir = make_git_repo();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            rev: Some("v1".to_string()),
            grep: Some("old".to_string()),
            ..Default::default()
        };
        let out = read_source("file.txt", &roots, &opts);
        assert!(out.contains("old content v1"), "got: {out}");
        assert!(out.contains("matches"), "grep header missing: {out}");
    }

    #[test]
    fn read_source_rev_bad_rev_errors() {
        let dir = make_git_repo();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            rev: Some("no-such-rev".to_string()),
            ..Default::default()
        };
        let out = read_source("file.txt", &roots, &opts);
        assert!(out.starts_with("Error"), "got: {out}");
    }

    #[test]
    fn read_source_rev_bad_path_errors() {
        let dir = make_git_repo();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            rev: Some("v1".to_string()),
            ..Default::default()
        };
        let out = read_source("does_not_exist.txt", &roots, &opts);
        assert!(out.starts_with("Error"), "got: {out}");
    }

    #[test]
    fn read_source_rev_non_git_root_errors() {
        // A plain (non-git) tempdir must be rejected with a clear message.
        let dir = make_tree();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            rev: Some("v1".to_string()),
            ..Default::default()
        };
        let out = read_source("hello.txt", &roots, &opts);
        assert!(out.starts_with("Error"), "got: {out}");
        assert!(out.contains("git"), "should mention git requirement: {out}");
    }

    #[test]
    fn read_source_rev_blocks_traversal() {
        let dir = make_git_repo();
        let roots = vec![dir.path().to_string_lossy().into_owned()];
        let opts = ReadOpts {
            rev: Some("v1".to_string()),
            ..Default::default()
        };
        let out = read_source("../escape.txt", &roots, &opts);
        assert!(out.starts_with("Error"), "got: {out}");
        // Must be rejected by our lexical guard, not handed to git.
        assert!(
            !out.contains("@v1"),
            "traversal path reached git show: {out}"
        );
    }

    #[test]
    fn is_safe_relative_path_guards() {
        assert!(is_safe_relative_path("a/b.txt"));
        assert!(is_safe_relative_path("./a.txt"));
        assert!(!is_safe_relative_path("../a.txt"));
        assert!(!is_safe_relative_path("a/../../b"));
        assert!(!is_safe_relative_path("/etc/passwd"));
    }
}
