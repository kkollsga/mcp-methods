//! Reusable building blocks for Rust-native MCP servers.
//!
//! The `mcp-server` binary in this crate is a complete generic MCP
//! server: source navigation, GitHub access (issues + REST API with
//! drill-down), workspace mode (github clone-and-track or local
//! directory bind), watch mode, manifest-driven `python:` tools, and
//! a lifecycle-aware embedder loader. Downstream crates that want the
//! same framework with extra domain-specific tools layered on top
//! depend on this crate as a library and call into the public modules
//! below.
//!
//! Typical layering pattern:
//! 1. Construct [`server::ServerOptions`] from a manifest, optionally
//!    binding source roots, a default repo, or a workspace handle.
//! 2. `let mut server = server::McpServer::new(options);`
//! 3. Register your domain-specific tools with
//!    [`server::McpServer::register_typed_tool`] — typed arg struct
//!    plus a `Fn(T) -> String` handler. (For lower-level control, use
//!    [`server::McpServer::tool_router_mut`] and rmcp's `ToolRoute`
//!    directly.)
//! 4. `server.serve(rmcp::transport::stdio()).await`.
//!
//! See `kglite-mcp-server` for a real example: it adds `cypher_query`,
//! `graph_overview`, and `save_graph` tools that close over an active
//! `KnowledgeGraph` PyObject and fire after the framework's
//! workspace/watch hooks rebuild the graph. Python authors running a
//! FastMCP server instead of this binary can compose the same tools
//! via the `mcp_methods.fastmcp` helper submodule on the Python side.

pub mod embedder;
pub mod env;
pub mod manifest;
pub mod python;
pub mod runtime;
pub mod server;
pub mod source;
pub mod watch;
pub mod workspace;

// Re-export the most commonly used types so downstream crates can
// `use mcp_server::{Manifest, ServerOptions, McpServer};` without
// chasing the module hierarchy.
pub use embedder::{spawn_idle_watch as spawn_embedder_watch, EmbedderHandle};
pub use manifest::{
    find_sibling_manifest, find_workspace_manifest, load as load_manifest, BuiltinsConfig,
    EmbedderConfig, Manifest, ManifestError, PythonTool, TempCleanup, ToolSpec, TrustConfig,
    WorkspaceConfig, WorkspaceKind,
};
pub use runtime::{
    apply_python_extensions, init_tracing, load_env_for_mode, maybe_watch, resolve_source_roots,
    PythonExtensions,
};
pub use server::{McpServer, RepoProvider, ServerOptions};
pub use source::SourceRootsProvider;
pub use watch::{watch as watch_dir, ChangeHandler, WatchHandle};
pub use workspace::{PostActivateHook, Workspace};
