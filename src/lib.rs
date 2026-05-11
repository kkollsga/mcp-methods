//! ``mcp-methods`` — primitives for building MCP servers.
//!
//! The crate ships as both a Python extension (`cdylib`, exported as the
//! `mcp_methods._mcp_methods` Python module when built with the `python`
//! feature) and a Rust library (`rlib`, consumable from other Rust crates
//! such as the sibling `mcp-server` binary in this workspace).
//!
//! Public Rust API surface — call these from another Rust crate via
//! ``use _mcp_methods::module::function;``. The PyO3 wrappers in
//! ``#[pymodule] fn _mcp_methods(...)`` re-export the same functions for
//! Python callers when the `python` feature is enabled.
//!
//! # Cargo features
//!
//! - `python` (default): pulls in PyO3 + every Python-callable surface.
//!   `cargo install` of downstream binaries and the wheel build path
//!   both keep this on. Disabling drops the PyO3 dep entirely — useful
//!   for distributing a pure-Rust binary without libpython linkage.
//! - `python-extension`: enables `pyo3/extension-module`. Set by maturin
//!   for the cdylib wheel build path; implies `python`.
//!
//! Modules that don't take a Python callback (cache, compact, git_refs,
//! github, html) stay available with the feature off — their pyo3
//! annotations are stripped via `cfg_attr` but the underlying Rust API
//! is unchanged. Modules that *do* take a Python callback (files, grep,
//! json_grep, list_dir) are gated entirely; they have no analog in
//! pure-Rust mode.

#[cfg(feature = "python")]
use pyo3::prelude::*;

pub mod cache;
pub mod compact;
pub mod git_refs;
pub mod github;
pub mod html;

#[cfg(feature = "python")]
pub mod files;
#[cfg(feature = "python")]
pub mod grep;
#[cfg(feature = "python")]
pub mod json_grep;
#[cfg(feature = "python")]
pub mod list_dir;

// The MCP server framework — moved here from the former
// `crates/mcp-server` workspace member in 0.3.25. Gated behind the
// `server` feature so pure-primitives consumers don't pay for the
// rmcp + tokio + clap dep chain.
#[cfg(feature = "server")]
pub mod server;

#[cfg(feature = "python")]
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
    m.add_function(wrap_pyfunction!(compact::py_compact_discussion, m)?)?;
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
