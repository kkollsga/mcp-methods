use grep_regex::RegexMatcherBuilder;
use grep_searcher::{MmapChoice, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use std::io;
use std::path::Path;

use super::types::{FileMatch, LineMatch};

/// Build a grep-regex matcher from a pattern string.
pub fn build_matcher(
    pattern: &str,
    case_insensitive: bool,
    multiline: bool,
) -> Result<grep_regex::RegexMatcher, String> {
    let mut builder = RegexMatcherBuilder::new();
    builder.case_insensitive(case_insensitive);
    builder.multi_line(multiline);
    builder.dot_matches_new_line(multiline);
    builder
        .build(pattern)
        .map_err(|e| format!("Invalid regex pattern: {}", e))
}

/// Custom Sink that collects both matches and context lines.
struct CollectSink {
    line_matches: Vec<LineMatch>,
    context_lines: Vec<(u64, String)>,
    has_context: bool,
}

impl CollectSink {
    fn new(has_context: bool) -> Self {
        Self {
            line_matches: Vec::new(),
            context_lines: Vec::new(),
            has_context,
        }
    }

    fn line_from_bytes(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes)
            .trim_end_matches('\n')
            .trim_end_matches('\r')
            .to_string()
    }
}

impl Sink for CollectSink {
    type Error = io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, io::Error> {
        let line_number = mat.line_number().unwrap_or(0);
        let content = Self::line_from_bytes(mat.bytes());
        self.line_matches.push(LineMatch {
            line_number,
            content,
        });
        Ok(true)
    }

    fn context(&mut self, _searcher: &Searcher, ctx: &SinkContext<'_>) -> Result<bool, io::Error> {
        if self.has_context {
            let line_number = ctx.line_number().unwrap_or(0);
            let content = Self::line_from_bytes(ctx.bytes());
            self.context_lines.push((line_number, content));
        }
        Ok(true)
    }
}

/// Search a file for matches using grep-searcher.
/// Returns None if no matches or file is binary/unreadable.
pub fn search_file(
    path: &Path,
    matcher: &grep_regex::RegexMatcher,
    context_before: usize,
    context_after: usize,
    multiline: bool,
) -> Option<FileMatch> {
    let has_context = context_before > 0 || context_after > 0;
    let mut sink = CollectSink::new(has_context);

    let mut searcher = SearcherBuilder::new()
        .binary_detection(grep_searcher::BinaryDetection::quit(0))
        .memory_map(unsafe { MmapChoice::auto() })
        .multi_line(multiline)
        .before_context(context_before)
        .after_context(context_after)
        .line_number(true)
        .build();

    let result = searcher.search_path(matcher, path, &mut sink);

    match result {
        Ok(()) if !sink.line_matches.is_empty() => {
            let match_count = sink.line_matches.len();
            Some(FileMatch {
                path: path.to_path_buf(),
                line_matches: sink.line_matches,
                context_lines: sink.context_lines,
                match_count,
            })
        }
        _ => None,
    }
}

/// Search already-loaded text content (for transform callback path).
#[allow(clippy::type_complexity)]
pub fn search_text(
    text: &str,
    matcher: &grep_regex::RegexMatcher,
    context_before: usize,
    context_after: usize,
    multiline: bool,
) -> Option<(Vec<LineMatch>, Vec<(u64, String)>)> {
    let has_context = context_before > 0 || context_after > 0;
    let mut sink = CollectSink::new(has_context);

    let mut searcher = SearcherBuilder::new()
        .binary_detection(grep_searcher::BinaryDetection::quit(0))
        .multi_line(multiline)
        .before_context(context_before)
        .after_context(context_after)
        .line_number(true)
        .build();

    let result = searcher.search_reader(matcher, text.as_bytes(), &mut sink);

    match result {
        Ok(()) if !sink.line_matches.is_empty() => Some((sink.line_matches, sink.context_lines)),
        _ => None,
    }
}
