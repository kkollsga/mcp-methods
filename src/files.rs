use pyo3::prelude::*;
use regex::Regex;
use std::path::PathBuf;

/// Return type for grep_lines: total matches found, matches shown, formatted lines.
struct GrepResult {
    total: usize,
    shown: usize,
    lines: Vec<String>,
}

fn grep_lines(
    lines: &[(usize, &str)],
    re: &Regex,
    context: usize,
    max_matches: Option<usize>,
) -> GrepResult {
    // Find matching indices within the slice
    let match_indices: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, (_, content))| re.is_match(content))
        .map(|(i, _)| i)
        .collect();

    let total = match_indices.len();

    if match_indices.is_empty() {
        return GrepResult {
            total: 0,
            shown: 0,
            lines: Vec::new(),
        };
    }

    // Apply max_matches limit
    let used = match max_matches {
        Some(limit) => &match_indices[..limit.min(total)],
        None => &match_indices[..],
    };
    let shown = used.len();

    // Expand each match ±context, merge overlapping windows
    let mut windows: Vec<(usize, usize)> = Vec::new();
    for &mi in used {
        let start = mi.saturating_sub(context);
        let end = (mi + context + 1).min(lines.len());
        if let Some(last) = windows.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        windows.push((start, end));
    }

    // Format: line numbers + content, with `--` separator between windows
    let mut output: Vec<String> = Vec::new();
    for (wi, (start, end)) in windows.iter().enumerate() {
        if wi > 0 {
            output.push("--".to_string());
        }
        for &(line_num, content) in &lines[*start..*end] {
            output.push(format!("{:>5}  {}", line_num, content));
        }
    }

    GrepResult {
        total,
        shown,
        lines: output,
    }
}

/// Extract an HTML element by its `id` attribute, returning the full element
/// from opening tag to its balanced closing tag.
///
/// Returns `None` if no element with the given id is found.
fn extract_section(html: &str, section_id: &str) -> Option<String> {
    // Build pattern: <tagname ... id="section_id" ...>
    // We need to find an opening tag containing id="section_id"
    let id_attr = format!("id=\"{}\"", section_id);
    let pos = html.find(&id_attr)?;

    // Walk backwards to find the '<' that opens this tag
    let tag_start = html[..pos].rfind('<')?;

    // Extract the tag name (first word after '<')
    let after_lt = &html[tag_start + 1..];
    let tag_name: String = after_lt
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if tag_name.is_empty() {
        return None;
    }

    let open_tag = format!("<{}", tag_name);
    let close_tag = format!("</{}>", tag_name);

    // Track depth from tag_start to find the balanced closing tag
    let mut depth: usize = 0;
    let mut i = tag_start;
    let bytes = html.as_bytes();
    let len = bytes.len();

    let open_bytes = open_tag.as_bytes();
    let close_bytes = close_tag.as_bytes();

    while i < len {
        if i + open_bytes.len() <= len
            && &bytes[i..i + open_bytes.len()] == open_bytes
            && (i + open_bytes.len() == len || !bytes[i + open_bytes.len()].is_ascii_alphanumeric())
        {
            depth += 1;
            i += open_bytes.len();
        } else if i + close_bytes.len() <= len && &bytes[i..i + close_bytes.len()] == close_bytes {
            depth -= 1;
            if depth == 0 {
                return Some(html[tag_start..i + close_bytes.len()].to_string());
            }
            i += close_bytes.len();
        } else {
            i += 1;
        }
    }

    // Unclosed tag — return from opening tag to end
    Some(html[tag_start..].to_string())
}

/// Read a file with path-traversal protection.
///
/// Returns the file content as a formatted string with line numbers.
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
pub fn read_file(
    py: Python<'_>,
    file_path: &str,
    allowed_dirs: Vec<String>,
    section: Option<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    rows: Option<Vec<usize>>,
    max_chars: Option<usize>,
    transform: Option<Py<PyAny>>,
    grep: Option<String>,
    grep_context: Option<usize>,
    max_matches: Option<usize>,
) -> PyResult<String> {
    // Pre-canonicalize allowed directories once, reuse across both loops.
    let canon_dirs: Vec<PathBuf> = allowed_dirs
        .iter()
        .filter_map(|d| PathBuf::from(d).canonicalize().ok())
        .collect();

    // Resolve file against allowed directories
    let mut resolved: Option<PathBuf> = None;

    for (i, d) in allowed_dirs.iter().enumerate() {
        let candidate = PathBuf::from(d).join(file_path);
        if let Ok(canon) = candidate.canonicalize() {
            if let Some(dir_canon) = canon_dirs.get(i) {
                if canon.starts_with(dir_canon) && canon.exists() {
                    resolved = Some(canon);
                    break;
                }
            }
        }
    }

    // Try as absolute path
    if resolved.is_none() {
        let abs_path = PathBuf::from(file_path);
        if let Ok(canon) = abs_path.canonicalize() {
            for dir_canon in &canon_dirs {
                if canon.starts_with(dir_canon) && canon.exists() {
                    resolved = Some(canon);
                    break;
                }
            }
        }
    }

    let resolved = match resolved {
        Some(p) => p,
        None => {
            return Ok(format!(
                "Error: file not found or access denied: {}",
                file_path
            ));
        }
    };

    // Read file
    let raw = match std::fs::read_to_string(&resolved) {
        Ok(s) => s,
        Err(e) => return Ok(format!("Error reading file: {}", e)),
    };

    // Apply transform
    let raw = if let Some(ref tf) = transform {
        let result: String = tf.call1(py, (raw,))?.extract(py)?;
        result
    } else {
        raw
    };

    // HTML section extraction by id
    if let Some(ref sid) = section {
        return match extract_section(&raw, sid) {
            Some(fragment) => {
                // If grep is active, search within the section content
                if let Some(ref pattern) = grep {
                    let re = match Regex::new(pattern) {
                        Ok(r) => r,
                        Err(e) => return Ok(format!("Error: invalid grep pattern: {}", e)),
                    };
                    let ctx = grep_context.unwrap_or(2);
                    let section_lines: Vec<&str> = fragment.lines().collect();
                    let section_total = section_lines.len();
                    let numbered: Vec<(usize, &str)> = section_lines
                        .iter()
                        .enumerate()
                        .map(|(i, line)| (i + 1, *line))
                        .collect();

                    let gr = grep_lines(&numbered, &re, ctx, max_matches);

                    let match_label = if gr.shown < gr.total {
                        format!("showing {} of {} matches", gr.shown, gr.total)
                    } else {
                        format!("{} matches", gr.total)
                    };
                    let header = format!(
                        "{}  section '{}'  ({} in {} lines)",
                        file_path, sid, match_label, section_total
                    );

                    if gr.lines.is_empty() {
                        return Ok(header);
                    }

                    let mut text = format!("{}\n{}", header, gr.lines.join("\n"));

                    if let Some(mc) = max_chars {
                        if text.len() > mc {
                            let mut end = mc;
                            while end > 0 && !text.is_char_boundary(end) {
                                end -= 1;
                            }
                            text.truncate(end);
                            text.push_str(&format!(
                                "\n\n[... truncated at {} chars — {} matches total]",
                                mc, gr.total
                            ));
                        }
                    }

                    return Ok(text);
                }

                // Non-grep section path
                let mut fragment = fragment;
                if let Some(mc) = max_chars {
                    if fragment.len() > mc {
                        let mut end = mc;
                        while end > 0 && !fragment.is_char_boundary(end) {
                            end -= 1;
                        }
                        fragment.truncate(end);
                        fragment.push_str(&format!("\n\n[... truncated at {} chars]", mc));
                    }
                }
                Ok(fragment)
            }
            None => Ok(format!(
                "Error: section '{}' not found in {}",
                sid, file_path
            )),
        };
    }

    // CSV row slicing
    if let Some(ref row_range) = rows {
        if row_range.len() == 2 {
            let all_lines: Vec<&str> = raw.lines().collect();
            let header = all_lines.first().copied().unwrap_or("");
            let start = row_range[0] + 1; // shift for header row
            let end = row_range[1] + 2;
            let selected: Vec<&str> = all_lines
                .get(start..end.min(all_lines.len()))
                .unwrap_or(&[])
                .to_vec();
            let mut text = format!("{}\n{}", header, selected.join("\n"));
            let total_data_rows = if all_lines.is_empty() {
                0
            } else {
                all_lines.len() - 1
            };
            text.push_str(&format!(
                "\n\n[rows {}-{} of {} total]",
                row_range[0], row_range[1], total_data_rows
            ));
            if let Some(mc) = max_chars {
                if text.len() > mc {
                    let mut end = mc;
                    while end > 0 && !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    text.truncate(end);
                    text.push_str(&format!("\n\n[... truncated at {} chars]", mc));
                }
            }
            return Ok(text);
        }
    }

    let all_lines: Vec<&str> = raw.lines().collect();
    let total = all_lines.len();

    let (selected, s, e) = if start_line.is_some() || end_line.is_some() {
        let s = start_line.unwrap_or(1).max(1);
        let e = end_line.unwrap_or(total).min(total);
        let sel: Vec<&str> = all_lines
            .get(s.saturating_sub(1)..e.min(all_lines.len()))
            .unwrap_or(&[])
            .to_vec();
        (sel, s, e)
    } else {
        (all_lines.clone(), 1, total)
    };

    // Grep filtering (operates on the selected line range)
    if let Some(ref pattern) = grep {
        let re = match Regex::new(pattern) {
            Ok(r) => r,
            Err(e) => return Ok(format!("Error: invalid grep pattern: {}", e)),
        };
        let ctx = grep_context.unwrap_or(2);

        // Build (absolute_line_number, content) pairs
        let numbered_lines: Vec<(usize, &str)> = selected
            .iter()
            .enumerate()
            .map(|(i, line)| (s + i, *line))
            .collect();

        let gr = grep_lines(&numbered_lines, &re, ctx, max_matches);

        let match_label = if gr.shown < gr.total {
            format!("showing {} of {} matches", gr.shown, gr.total)
        } else {
            format!("{} matches", gr.total)
        };
        let header = format!("{}  ({} in {} lines)", file_path, match_label, total);

        if gr.lines.is_empty() {
            return Ok(header);
        }

        let mut text = format!("{}\n{}", header, gr.lines.join("\n"));

        if let Some(mc) = max_chars {
            if text.len() > mc {
                let mut end = mc;
                while end > 0 && !text.is_char_boundary(end) {
                    end -= 1;
                }
                text.truncate(end);
                text.push_str(&format!(
                    "\n\n[... truncated at {} chars — {} matches, {} chars total]",
                    mc,
                    gr.total,
                    raw.len()
                ));
            }
        }

        return Ok(text);
    }

    let numbered: Vec<String> = selected
        .iter()
        .enumerate()
        .map(|(i, line)| format!("{:>5}  {}", s + i, line))
        .collect();

    let header = if start_line.is_some() || end_line.is_some() {
        format!(
            "{}:{}-{}  ({} of {} lines)",
            file_path,
            s,
            e,
            e - s + 1,
            total
        )
    } else {
        format!("{}  ({} lines)", file_path, total)
    };

    let mut text = format!("{}\n{}", header, numbered.join("\n"));

    if let Some(mc) = max_chars {
        if text.len() > mc {
            let mut end = mc;
            while end > 0 && !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            text.push_str(&format!(
                "\n\n[... truncated at {} chars — {} total]",
                mc,
                raw.len()
            ));
        }
    }

    Ok(text)
}
