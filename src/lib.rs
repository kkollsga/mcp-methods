use pyo3::prelude::*;

mod cache;
mod compact;
mod files;
mod git_refs;
mod github;
mod grep;
mod json_grep;

#[pymodule]
fn _mcp_methods(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // git_refs
    m.add_function(wrap_pyfunction!(git_refs::validate_repo, m)?)?;
    m.add_function(wrap_pyfunction!(git_refs::extract_github_refs, m)?)?;
    // grep
    m.add_function(wrap_pyfunction!(grep::grep_files, m)?)?;
    m.add_function(wrap_pyfunction!(grep::grep_lines, m)?)?;
    // files
    m.add_function(wrap_pyfunction!(files::read_file, m)?)?;
    // compact
    m.add_function(wrap_pyfunction!(compact::collapse_code_blocks, m)?)?;
    m.add_function(wrap_pyfunction!(compact::compact_text, m)?)?;
    m.add_function(wrap_pyfunction!(compact::compact_discussion, m)?)?;
    // json_grep
    m.add_function(wrap_pyfunction!(json_grep::grep_json_fields, m)?)?;
    // github
    m.add_function(wrap_pyfunction!(github::has_git_token, m)?)?;
    m.add_function(wrap_pyfunction!(github::detect_git_repo, m)?)?;
    m.add_function(wrap_pyfunction!(github::git_api, m)?)?;
    m.add_function(wrap_pyfunction!(github::git_issue, m)?)?;
    // cache
    m.add_class::<cache::ElementCache>()?;
    Ok(())
}
