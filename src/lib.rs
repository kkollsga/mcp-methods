//! ``mcp-methods`` — primitives for building MCP servers.
//!
//! The crate ships as both a Python extension (`cdylib`, exported as the
//! `mcp_methods._mcp_methods` Python module) and a Rust library (`rlib`,
//! consumable from other Rust crates such as the sibling `mcp-server`
//! binary in this workspace).
//!
//! Public Rust API surface — call these from another Rust crate via
//! ``use _mcp_methods::module::function;``. The PyO3 wrappers in
//! ``#[pymodule] fn _mcp_methods(...)`` re-export the same functions for
//! Python callers.

use pyo3::prelude::*;

pub mod cache;
pub mod compact;
pub mod files;
pub mod git_refs;
pub mod github;
pub mod grep;
pub mod html;
pub mod json_grep;
pub mod list_dir;

#[pymodule]
fn _mcp_methods(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // git_refs
    m.add_function(wrap_pyfunction!(git_refs::validate_repo, m)?)?;
    m.add_function(wrap_pyfunction!(git_refs::extract_github_refs, m)?)?;
    // grep
    m.add_function(wrap_pyfunction!(grep::ripgrep_files, m)?)?;
    m.add_function(wrap_pyfunction!(grep::ripgrep_lines, m)?)?;
    // files
    m.add_function(wrap_pyfunction!(files::read_file, m)?)?;
    // html
    m.add_function(wrap_pyfunction!(html::html_to_text, m)?)?;
    m.add_function(wrap_pyfunction!(list_dir::list_dir, m)?)?;
    // compact
    m.add_function(wrap_pyfunction!(compact::collapse_code_blocks, m)?)?;
    m.add_function(wrap_pyfunction!(compact::compact_text, m)?)?;
    m.add_function(wrap_pyfunction!(compact::compact_discussion, m)?)?;
    // json_grep
    m.add_function(wrap_pyfunction!(json_grep::ripgrep_json_fields, m)?)?;
    // github
    m.add_function(wrap_pyfunction!(github::has_git_token, m)?)?;
    m.add_function(wrap_pyfunction!(github::detect_git_repo, m)?)?;
    m.add_function(wrap_pyfunction!(github::git_api, m)?)?;
    m.add_function(wrap_pyfunction!(github::github_issues, m)?)?;
    // cache
    m.add_class::<cache::ElementCache>()?;
    Ok(())
}
