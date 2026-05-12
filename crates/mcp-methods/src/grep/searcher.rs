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

/// Build a reusable Searcher with the given configuration.
pub fn build_searcher(
    context_before: usize,
    context_after: usize,
    multiline: bool,
    use_mmap: bool,
) -> Searcher {
    let mut builder = SearcherBuilder::new();
    builder
        .binary_detection(grep_searcher::BinaryDetection::quit(0))
        .multi_line(multiline)
        .before_context(context_before)
        .after_context(context_after)
        .line_number(true);
    if use_mmap {
        builder.memory_map(unsafe { MmapChoice::auto() });
    }
    builder.build()
}

/// Reusable Sink that collects both matches and context lines.
/// Call `clear()` between files to reuse allocations.
pub struct CollectSink {
    line_matches: Vec<LineMatch>,
    context_lines: Vec<(u64, String)>,
    has_context: bool,
}

impl CollectSink {
    pub fn new(has_context: bool) -> Self {
        Self {
            line_matches: Vec::new(),
            context_lines: Vec::new(),
            has_context,
        }
    }

    /// Reset for reuse without deallocating buffers.
    pub fn clear(&mut self) {
        self.line_matches.clear();
        self.context_lines.clear();
    }

    fn line_from_bytes(bytes: &[u8]) -> String {
        // Strip trailing \r\n as raw bytes before UTF-8 decode
        let mut end = bytes.len();
        while end > 0 && (bytes[end - 1] == b'\n' || bytes[end - 1] == b'\r') {
            end -= 1;
        }
        String::from_utf8_lossy(&bytes[..end]).into_owned()
    }

    /// True if the last search produced matches.
    pub fn has_matches(&self) -> bool {
        !self.line_matches.is_empty()
    }

    /// Take collected results, leaving empty vecs behind (preserves capacity).
    pub fn take_results(&mut self) -> (Vec<LineMatch>, Vec<(u64, String)>) {
        (
            std::mem::take(&mut self.line_matches),
            std::mem::take(&mut self.context_lines),
        )
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

/// Search a file using a pre-built Searcher and reusable Sink.
/// Caller must call `sink.clear()` before each invocation.
/// Returns None if no matches or file is binary/unreadable.
pub fn search_file(
    path: &Path,
    matcher: &grep_regex::RegexMatcher,
    searcher: &mut Searcher,
    sink: &mut CollectSink,
) -> Option<FileMatch> {
    let result = searcher.search_path(matcher, path, &mut *sink);

    match result {
        Ok(()) if sink.has_matches() => {
            let (line_matches, context_lines) = sink.take_results();
            let match_count = line_matches.len();
            Some(FileMatch {
                path: path.to_path_buf(),
                line_matches,
                context_lines,
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
    searcher: &mut Searcher,
    sink: &mut CollectSink,
) -> Option<(Vec<LineMatch>, Vec<(u64, String)>)> {
    let result = searcher.search_reader(matcher, text.as_bytes(), &mut *sink);

    match result {
        Ok(()) if sink.has_matches() => Some(sink.take_results()),
        _ => None,
    }
}
