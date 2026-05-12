use fancy_regex::Regex as FancyRegex;
use regex::Regex;
use std::collections::HashSet;
use std::sync::LazyLock;

static LINK_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://github\.com/([a-zA-Z0-9_.\-]+/[a-zA-Z0-9_.\-]+)/(?:issues|pull)/(\d+)")
        .unwrap()
});

static CROSS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([a-zA-Z0-9_.\-]+/[a-zA-Z0-9_.\-]+)#(\d+)\b").unwrap());

static SHORT_RE: LazyLock<FancyRegex> =
    LazyLock::new(|| FancyRegex::new(r"(?<![a-zA-Z0-9/])#(\d+)\b").unwrap());

/// Validate `org/repo` format. Returns an error string, or empty string if valid.
pub fn validate_repo(repo_name: &str) -> Option<String> {
    let parts: Vec<&str> = repo_name.split('/').collect();
    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
        return Some("Invalid repo name. Use 'org/repo' format, e.g. 'numpy/numpy'.".to_string());
    }
    None
}

/// Extract GitHub issue/PR references from text.
/// Returns a list of (repo_name, number) tuples.
pub fn extract_github_refs(text: &str, default_repo: &str) -> Vec<(String, u64)> {
    if text.is_empty() {
        return Vec::new();
    }

    let mut refs: HashSet<(String, u64)> = HashSet::new();

    // Full GitHub URLs: https://github.com/org/repo/issues/123
    for cap in LINK_RE.captures_iter(text) {
        if let (Some(repo), Some(num)) = (cap.get(1), cap.get(2)) {
            if let Ok(n) = num.as_str().parse::<u64>() {
                refs.insert((repo.as_str().to_string(), n));
            }
        }
    }

    // Cross-repo refs: org/repo#123
    for cap in CROSS_RE.captures_iter(text) {
        if let (Some(repo), Some(num)) = (cap.get(1), cap.get(2)) {
            if let Ok(n) = num.as_str().parse::<u64>() {
                refs.insert((repo.as_str().to_string(), n));
            }
        }
    }

    // Short refs: #123 (with lookbehind to exclude org/repo#123)
    let mut start = 0;
    while start < text.len() {
        match SHORT_RE.find_from_pos(text, start) {
            Ok(Some(m)) => {
                if let Ok(Some(cap)) = SHORT_RE.captures_from_pos(text, m.start()) {
                    if let Some(num_match) = cap.get(1) {
                        if let Ok(n) = num_match.as_str().parse::<u64>() {
                            refs.insert((default_repo.to_string(), n));
                        }
                    }
                }
                start = m.end();
            }
            _ => break,
        }
    }

    let mut result: Vec<(String, u64)> = refs.into_iter().collect();
    result.sort();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_repo_valid() {
        assert_eq!(validate_repo("numpy/numpy"), None);
    }

    #[test]
    fn test_validate_repo_invalid() {
        assert!(validate_repo("noslash").is_some());
        assert!(validate_repo("/empty").is_some());
        assert!(validate_repo("empty/").is_some());
    }

    #[test]
    fn test_extract_refs() {
        let text = "See #42 and https://github.com/org/repo/issues/10 and other/lib#5";
        let refs = extract_github_refs(text, "default/repo");
        assert!(refs.contains(&("default/repo".to_string(), 42)));
        assert!(refs.contains(&("org/repo".to_string(), 10)));
        assert!(refs.contains(&("other/lib".to_string(), 5)));
    }
}
