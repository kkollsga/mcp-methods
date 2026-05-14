//! Reusable building blocks for Rust-native MCP servers.
//!
//! The `mcp-server` binary (in the sibling `crates/mcp-server` crate)
//! is a complete generic MCP server: source navigation, GitHub access
//! (issues + REST API with drill-down), workspace mode (github
//! clone-and-track or local directory bind), watch mode, and
//! manifest-driven Cypher / tool registration. Downstream crates that
//! want the same framework with domain-specific tools layered on top
//! depend on `mcp-methods` and call into the public modules below.
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
//! graph handle. Python authors running a FastMCP server can compose
//! tools via the `mcp_methods.fastmcp` helper submodule (Python-side).
//!
//! **Note:** the legacy `embedder:` / `tools[].python:` extension hooks
//! that lived here in 0.3.25 have been removed. They required PyO3 in
//! the framework's source, which violated the pure-Rust constraint of
//! this crate. Downstream Python-aware servers (kglite, etc.) implement
//! their equivalent via a thin pyo3 wrapper in their own cdylib.

pub mod env;
pub mod manifest;
pub mod runtime;
// `server` inside the `server` feature module — the inner module is
// the rmcp `ServerHandler` impl. Rename would churn every downstream
// import; allow the inception.
#[allow(clippy::module_inception)]
pub mod server;
pub mod skills;
pub mod source;
pub mod watch;
pub mod workspace;

// Re-export the most commonly used types so downstream crates can
// `use mcp_methods::server::{Manifest, ServerOptions, McpServer};`
// without chasing the module hierarchy.
pub use manifest::{
    find_sibling_manifest, find_workspace_manifest, load as load_manifest, BuiltinsConfig,
    EmbedderConfig, Manifest, ManifestError, PythonTool, SkillSource, SkillsSource, TempCleanup,
    ToolSpec, TrustConfig, WorkspaceConfig, WorkspaceKind,
};
pub use runtime::{init_tracing, load_env_for_mode, maybe_watch, resolve_source_roots};
pub use server::{serve_prompts, McpServer, RepoProvider, ServerOptions};
pub use skills::{
    library_bundled_skills, BundledSkill, Registry as SkillRegistry, ResolvedRegistry, Skill,
    SkillError, SkillFrontmatter, SkillProvenance,
};
pub use source::SourceRootsProvider;
pub use watch::{watch as watch_dir, ChangeHandler, WatchHandle};
pub use workspace::{PostActivateHook, Workspace};
