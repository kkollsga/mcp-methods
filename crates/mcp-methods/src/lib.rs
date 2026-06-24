//! ``mcp-methods`` — pure-Rust primitives for building MCP servers.
//!
//! Zero PyO3 in this crate's source tree. Python bindings live in the
//! sibling `mcp-methods-py` crate, which depends on this rlib and adds
//! a `#[pymodule]` of PyO3 wrappers for the cdylib. Pure-Rust consumers
//! (kglite, any downstream Rust binary) depend on `mcp-methods`
//! directly and see no traces of Python in their dep tree or source.
//!
//! Public API surface — `use mcp_methods::<module>::<fn>;` from any
//! Rust crate:
//! - [`cache::ElementCache`] — drill-down cache for collapsed GitHub
//!   discussion elements (code blocks, comments, patches, overflow).
//! - [`compact`] — text compaction utilities + adaptive budget-based
//!   JSON discussion compaction.
//! - [`files::read_file`] — safe file reading with allowed-dir sandbox.
//! - [`git_refs`] — `org/repo` format validation + reference extraction.
//! - [`github`] — GitHub REST API client + issue/PR fetching with
//!   smart compaction.
//! - [`grep`] — ripgrep-powered file + line search.
//! - [`html::html_to_text`] — lightweight HTML → markdown-flavoured text.
//! - [`json_grep::ripgrep_json_fields`] — extract fields from JSON text.
//! - [`list_dir::list_dir`] — tree-formatted directory listing.
//!
//! With `feature = "server"` (default-on), additionally:
//! - [`server`] — rmcp-backed MCP server framework (manifest parsing,
//!   source/github tools, workspace mode, watch mode, etc.).

pub mod cache;
pub mod compact;
pub mod files;
pub mod git_refs;
pub mod github;
pub mod grep;
pub mod html;
pub mod json_grep;
pub mod list_dir;
pub mod screen;

#[cfg(feature = "server")]
pub mod server;
