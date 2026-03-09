use std::path::PathBuf;

/// Output mode for grep results.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OutputMode {
    /// Show matching lines with optional context.
    Content,
    /// Show only file paths that contain matches.
    FilesWithMatches,
    /// Show match count per file.
    Count,
}

impl OutputMode {
    pub fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "content" => Ok(Self::Content),
            "files_with_matches" => Ok(Self::FilesWithMatches),
            "count" => Ok(Self::Count),
            _ => Err(format!(
                "Invalid output_mode '{}'. Use 'content', 'files_with_matches', or 'count'.",
                s
            )),
        }
    }
}

/// A single matching line within a file.
#[derive(Clone, Debug)]
pub struct LineMatch {
    pub line_number: u64,
    pub content: String,
}

/// Matches found in a single file.
#[derive(Clone, Debug)]
pub struct FileMatch {
    pub path: PathBuf,
    pub line_matches: Vec<LineMatch>,
    pub context_lines: Vec<(u64, String)>,
    pub match_count: usize,
}
