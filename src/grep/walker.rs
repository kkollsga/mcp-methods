use ignore::overrides::OverrideBuilder;
use ignore::types::TypesBuilder;
use ignore::WalkBuilder;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::searcher;
use super::types::FileMatch;

const DEFAULT_SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "dist",
    "build",
    ".eggs",
    "venv",
    ".venv",
    "target",
    ".cargo",
    ".ruff_cache",
];

fn build_walker(
    source_dirs: &[String],
    glob_pattern: &str,
    type_filter: Option<&str>,
    skip_dirs: Option<&[String]>,
    respect_gitignore: bool,
) -> Result<WalkBuilder, String> {
    if source_dirs.is_empty() {
        return Err("No source directories provided.".into());
    }

    let skip: HashSet<String> = match skip_dirs {
        Some(dirs) => dirs.iter().map(|s| s.to_string()).collect(),
        None => DEFAULT_SKIP_DIRS.iter().map(|s| s.to_string()).collect(),
    };

    let mut builder = WalkBuilder::new(&source_dirs[0]);
    for dir in &source_dirs[1..] {
        builder.add(dir);
    }

    // Don't skip dotfiles by default (we use skip_dirs for .git etc.)
    builder.hidden(false);
    builder.git_ignore(respect_gitignore);
    builder.git_global(respect_gitignore);
    builder.git_exclude(respect_gitignore);

    // File type filtering (e.g., "py", "rs", "js")
    if let Some(type_name) = type_filter {
        let mut types_builder = TypesBuilder::new();
        types_builder.add_defaults();
        types_builder.select(type_name);
        let types = types_builder
            .build()
            .map_err(|e| format!("Unknown file type '{}': {}", type_name, e))?;
        builder.types(types);
    }

    // Glob filtering via overrides
    if glob_pattern != "*" {
        let mut overrides = OverrideBuilder::new(&source_dirs[0]);
        overrides
            .add(glob_pattern)
            .map_err(|e| format!("Invalid glob '{}': {}", glob_pattern, e))?;
        let built = overrides
            .build()
            .map_err(|e| format!("Glob build error: {}", e))?;
        builder.overrides(built);
    }

    // Skip directories
    builder.filter_entry(move |entry| {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(name) = entry.file_name().to_str() {
                return !skip.contains(name as &str);
            }
        }
        true
    });

    Ok(builder)
}

/// Walk files sequentially (needed when transform callback requires GIL).
pub fn walk_sequential(
    source_dirs: &[String],
    glob_pattern: &str,
    type_filter: Option<&str>,
    skip_dirs: Option<&[String]>,
    respect_gitignore: bool,
) -> Result<Vec<PathBuf>, String> {
    let mut builder = build_walker(
        source_dirs,
        glob_pattern,
        type_filter,
        skip_dirs,
        respect_gitignore,
    )?;
    let walker = builder.sort_by_file_path(|a, b| a.cmp(b)).build();

    let mut paths = Vec::new();
    for entry in walker.flatten() {
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            paths.push(entry.into_path());
        }
    }
    Ok(paths)
}

/// Walk and search files in parallel using ignore crate's thread pool.
/// Each walker thread searches files as it discovers them.
/// Stops early when max_results is reached.
/// Searcher and Sink are created once per thread and reused across files.
#[allow(clippy::too_many_arguments)]
pub fn walk_and_search_parallel(
    source_dirs: &[String],
    glob_pattern: &str,
    type_filter: Option<&str>,
    skip_dirs: Option<&[String]>,
    respect_gitignore: bool,
    matcher: &grep_regex::RegexMatcher,
    context_before: usize,
    context_after: usize,
    multiline: bool,
    max_results: usize,
) -> Result<Vec<FileMatch>, String> {
    let builder = build_walker(
        source_dirs,
        glob_pattern,
        type_filter,
        skip_dirs,
        respect_gitignore,
    )?;
    let walker = builder.build_parallel();

    let matches: Arc<Mutex<Vec<FileMatch>>> = Arc::new(Mutex::new(Vec::new()));
    let total_count = Arc::new(AtomicUsize::new(0));

    let has_context = context_before > 0 || context_after > 0;

    walker.run(|| {
        let matches = Arc::clone(&matches);
        let total_count = Arc::clone(&total_count);
        // Create searcher and sink ONCE per thread — reused across all files
        let mut thread_searcher = searcher::build_searcher(
            context_before,
            context_after,
            multiline,
            true, // use mmap
        );
        let mut thread_sink = searcher::CollectSink::new(has_context);

        Box::new(move |entry| {
            // Check if we've already hit max_results
            if total_count.load(Ordering::Relaxed) >= max_results {
                return ignore::WalkState::Quit;
            }

            let entry = match entry {
                Ok(e) => e,
                Err(_) => return ignore::WalkState::Continue,
            };

            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                return ignore::WalkState::Continue;
            }

            // Clear sink for reuse (preserves allocated capacity)
            thread_sink.clear();

            // Search this file in the walker thread with reused searcher/sink
            if let Some(fm) = searcher::search_file(
                entry.path(),
                matcher,
                &mut thread_searcher,
                &mut thread_sink,
            ) {
                let count = fm.match_count;
                matches.lock().unwrap().push(fm);
                total_count.fetch_add(count, Ordering::Relaxed);

                if total_count.load(Ordering::Relaxed) >= max_results {
                    return ignore::WalkState::Quit;
                }
            }

            ignore::WalkState::Continue
        })
    });

    let mut matches = Arc::try_unwrap(matches).unwrap().into_inner().unwrap();
    matches.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(matches)
}

/// Relativize a path for display.
pub fn relativize(path: &Path, relative_to: Option<&Path>, source_dir: &Path) -> String {
    let base = relative_to.unwrap_or(source_dir);
    path.strip_prefix(base)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}
