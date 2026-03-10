use pyo3::prelude::*;
use std::path::PathBuf;

/// Read a file with path-traversal protection.
///
/// Returns the file content as a formatted string with line numbers.
#[pyfunction]
#[pyo3(signature = (
    file_path,
    allowed_dirs,
    *,
    start_line = None,
    end_line = None,
    rows = None,
    max_chars = None,
    transform = None,
))]
#[allow(clippy::too_many_arguments)]
pub fn read_file(
    py: Python<'_>,
    file_path: &str,
    allowed_dirs: Vec<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
    rows: Option<Vec<usize>>,
    max_chars: Option<usize>,
    transform: Option<Py<PyAny>>,
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
                    text.truncate(mc);
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
            text.truncate(mc);
            text.push_str(&format!(
                "\n\n[... truncated at {} chars — {} total]",
                mc,
                raw.len()
            ));
        }
    }

    Ok(text)
}
