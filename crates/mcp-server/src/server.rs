//! MCP `ServerHandler` implementation.
//!
//! Phase 1: server identity, ping tool.
//! Phase 2: source tools (`read_source`, `grep`, `list_source`)
//!          gated on an active source-roots provider.
//! Phase 3: github tools (`github_issues`, `github_api`) — always
//!          registered (they don't need a source root); the active
//!          repo is resolved per-call from a configured default + an
//!          optional `repo_name=` argument.
//!
//! The source-roots provider is dynamic — workspace mode swaps it as
//! the active repo changes; single-graph and watch modes wire it to
//! a fixed root. An empty list signals "no active source" and the tools
//! return a friendly error.

#![allow(dead_code)]

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::manifest::Manifest;
use crate::source::{
    self, resolve_dir_under_roots, GrepOpts, ListOpts, ReadOpts, SourceRootsProvider,
};

/// Provider returning the active GitHub repo (e.g. `"pydata/xarray"`)
/// or `None` when nothing is bound. Workspace mode wires this to the
/// active workspace repo; single-graph mode can pin a fixed value.
pub type RepoProvider = Arc<dyn Fn() -> Option<String> + Send + Sync>;

/// Per-server runtime state shared by every tool dispatch.
#[derive(Clone, Default)]
pub struct ServerOptions {
    /// Server display name surfaced via initialize.
    pub name: Option<String>,
    /// Free-form text shown to the agent at session start.
    pub instructions: Option<String>,
    /// Dynamic provider returning the active source roots, if any.
    /// `None` disables the source tools entirely.
    pub source_roots: Option<SourceRootsProvider>,
    /// Dynamic provider returning the active GitHub repo (org/repo).
    /// When `None`, github tools require a per-call `repo_name=` arg.
    pub default_repo: Option<RepoProvider>,
}

impl std::fmt::Debug for ServerOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOptions")
            .field("name", &self.name)
            .field("instructions", &self.instructions)
            .field(
                "source_roots",
                &self.source_roots.as_ref().map(|_| "<provider>"),
            )
            .field(
                "default_repo",
                &self.default_repo.as_ref().map(|_| "<provider>"),
            )
            .finish()
    }
}

impl ServerOptions {
    pub fn from_manifest(manifest: Option<&Manifest>, fallback_name: &str) -> Self {
        Self {
            name: manifest
                .and_then(|m| m.name.clone())
                .or_else(|| Some(fallback_name.to_string())),
            instructions: manifest.and_then(|m| m.instructions.clone()),
            source_roots: None,
            default_repo: None,
        }
    }

    pub fn with_static_source_roots(mut self, roots: Vec<String>) -> Self {
        let captured = Arc::new(roots);
        self.source_roots = Some(Arc::new(move || captured.as_ref().clone()));
        self
    }

    pub fn with_dynamic_source_roots(mut self, provider: SourceRootsProvider) -> Self {
        self.source_roots = Some(provider);
        self
    }

    pub fn with_static_repo(mut self, repo: String) -> Self {
        self.default_repo = Some(Arc::new(move || Some(repo.clone())));
        self
    }

    pub fn with_dynamic_repo(mut self, provider: RepoProvider) -> Self {
        self.default_repo = Some(provider);
        self
    }
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct PingArgs {
    /// Optional message to echo back. Defaults to "pong".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ReadSourceArgs {
    /// File path relative to the configured source root(s).
    pub file_path: String,
    /// Start line (1-indexed). Defaults to start-of-file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_line: Option<usize>,
    /// End line (1-indexed, inclusive). Defaults to end-of-file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_line: Option<usize>,
    /// Regex pattern to filter lines. Returns matching lines plus context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep: Option<String>,
    /// Lines of context around each grep match (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep_context: Option<usize>,
    /// Cap the number of matches returned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<usize>,
    /// Cap output size in characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GrepArgs {
    /// Regex pattern (Rust regex syntax).
    pub pattern: String,
    /// File-name glob (e.g. ``"*.py"``). Defaults to all files.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Lines of context around each match (default 0).
    #[serde(default)]
    pub context: usize,
    /// Cap the number of matches (default 50; pass null/None for unlimited).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<usize>,
    /// Case-insensitive matching.
    #[serde(default)]
    pub case_insensitive: bool,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GithubIssuesArgs {
    /// GitHub issue / PR / Discussion number (FETCH mode).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    /// org/repo override; defaults to the active server repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    /// Free-text query (SEARCH mode). When set, `number` is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// "issue" | "pr" | "discussion" | "all" (default).
    #[serde(default = "default_kind")]
    pub kind: String,
    /// "open" (default) | "closed" | "all".
    #[serde(default = "default_state")]
    pub state: String,
    /// Sort key. Default "created" for list mode, relevance for search.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<String>,
    /// Max results to return (default 20).
    #[serde(default = "default_limit")]
    pub limit: usize,
    /// Comma-separated label filter (e.g. "bug,P0").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<String>,
}

fn default_kind() -> String {
    "all".to_string()
}
fn default_state() -> String {
    "open".to_string()
}
fn default_limit() -> usize {
    20
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct GithubApiArgs {
    /// API path. Relative paths (e.g. "pulls?state=open", "commits/abc",
    /// "branches", "compare/main...x") are prefixed with /repos/<repo_name>/.
    /// Absolute resources ("search/issues?q=...", "users/octocat") pass through.
    pub path: String,
    /// org/repo override; defaults to the active server repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    /// Truncate response body at N chars (default 80,000).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncate_at: Option<usize>,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ListSourceArgs {
    /// Subdirectory relative to the source root (default ``"."``).
    #[serde(default = "default_path")]
    pub path: String,
    /// Recursion depth (1 = flat ls; 2+ = tree).
    #[serde(default = "default_depth")]
    pub depth: usize,
    /// Glob filter for entry names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub glob: Option<String>,
    /// Show only directories.
    #[serde(default)]
    pub dirs_only: bool,
}

fn default_path() -> String {
    ".".to_string()
}
fn default_depth() -> usize {
    1
}

/// MCP server backed by the rmcp framework.
///
/// The struct is cloned per request by rmcp's handler dispatch; the
/// expensive bits (provider closure) are behind an Arc so cloning is cheap.
#[derive(Clone)]
pub struct McpServer {
    options: ServerOptions,
    tool_router: ToolRouter<McpServer>,
}

#[tool_router]
impl McpServer {
    pub fn new(options: ServerOptions) -> Self {
        Self {
            options,
            tool_router: Self::tool_router(),
        }
    }

    fn current_source_roots(&self) -> Vec<String> {
        match &self.options.source_roots {
            Some(provider) => provider(),
            None => Vec::new(),
        }
    }

    /// Resolve the active repo: per-call override → configured default →
    /// auto-detect from cwd (last-resort fallback). Returns the resolved
    /// repo string and an `Err` (formatted user message) if none is found
    /// or the value is malformed.
    fn resolve_repo(&self, override_repo: Option<String>) -> Result<String, String> {
        if let Some(r) = override_repo {
            if let Some(err) = _mcp_methods::git_refs::validate_repo(&r) {
                return Err(err);
            }
            return Ok(r);
        }
        if let Some(provider) = &self.options.default_repo {
            if let Some(r) = provider() {
                if let Some(err) = _mcp_methods::git_refs::validate_repo(&r) {
                    return Err(err);
                }
                return Ok(r);
            }
        }
        // Auto-detect last-resort
        if let Some(detected) = _mcp_methods::github::detect_git_repo(".") {
            if _mcp_methods::git_refs::validate_repo(&detected).is_none() {
                return Ok(detected);
            }
        }
        Err(
            "No active repository. Pass `repo_name='org/repo'`, configure a default in the \
             server, or run from a directory whose git remote points at github.com."
                .to_string(),
        )
    }

    #[tool(
        description = "Liveness probe — returns 'pong' (or echoes `message` if supplied). \
                          Use to confirm the server framework is wired correctly before \
                          relying on graph- or source-aware tools."
    )]
    async fn ping(
        &self,
        Parameters(args): Parameters<PingArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = args.message.unwrap_or_else(|| "pong".to_string());
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(description = "Read a file from the configured source root(s). Pass \
                       `start_line`/`end_line` to slice, `grep` to filter to matching \
                       lines, `max_chars` to cap output. Path traversal attempts are \
                       rejected. Available only when source roots are configured.")]
    async fn read_source(
        &self,
        Parameters(args): Parameters<ReadSourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let roots = self.current_source_roots();
        if roots.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Cannot read source: no active source root. Configure source_root in your manifest \
                 or activate one (e.g. via repo_management in workspace mode).",
            )]));
        }
        let opts = ReadOpts {
            start_line: args.start_line,
            end_line: args.end_line,
            grep: args.grep,
            grep_context: args.grep_context,
            max_matches: args.max_matches,
            max_chars: args.max_chars,
        };
        let body = source::read_source(&args.file_path, &roots, &opts);
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Search source files using ripgrep. `pattern` is a regex (Rust \
                       syntax). `glob` filters file paths (e.g. \"*.py\"). `context` adds \
                       N surrounding lines per match. Set `case_insensitive=true` for \
                       case-insensitive matching. `max_results` caps total matches \
                       (default 50)."
    )]
    async fn grep(
        &self,
        Parameters(args): Parameters<GrepArgs>,
    ) -> Result<CallToolResult, McpError> {
        let roots = self.current_source_roots();
        if roots.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Cannot grep: no active source root. Configure source_root in your manifest \
                 or activate one (e.g. via repo_management in workspace mode).",
            )]));
        }
        let opts = GrepOpts {
            glob: args.glob,
            context: args.context,
            max_results: Some(args.max_results.unwrap_or(50)),
            case_insensitive: args.case_insensitive,
        };
        let body = source::grep(&roots, &args.pattern, &opts);
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "List directory contents under the configured source root. `path` \
                       is resolved against the first source root (\".\" lists the root \
                       itself). `depth` controls recursion (1 = flat ls, 2+ = tree). \
                       `glob` filters entry names. `dirs_only=true` shows only \
                       directories."
    )]
    async fn list_source(
        &self,
        Parameters(args): Parameters<ListSourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let roots = self.current_source_roots();
        if roots.is_empty() {
            return Ok(CallToolResult::success(vec![Content::text(
                "Cannot list source: no active source root. Configure source_root in your \
                 manifest or activate one (e.g. via repo_management in workspace mode).",
            )]));
        }
        let primary = std::path::PathBuf::from(&roots[0]);
        let target = match resolve_dir_under_roots(&args.path, &roots) {
            Some(p) => p,
            None => {
                return Ok(CallToolResult::success(vec![Content::text(format!(
                    "Error: path '{}' resolves outside the configured source roots.",
                    args.path
                ))]));
            }
        };
        let opts = ListOpts {
            depth: args.depth,
            glob: args.glob,
            dirs_only: args.dirs_only,
        };
        let body = source::list_source(&target, &primary, &opts);
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Search, list, or fetch GitHub issues / pull requests / Discussions. \
                       Pass `number=N` for FETCH (single issue/PR/discussion); `query=\"...\"` \
                       for SEARCH (across issues+PRs and Discussions); neither for LIST. \
                       `kind` ∈ \"issue\" / \"pr\" / \"discussion\" / \"all\" (default). \
                       `state` ∈ \"open\" (default) / \"closed\" / \"all\". `limit` caps \
                       result count (default 20). `labels` is a comma-separated string. \
                       `repo_name=\"org/repo\"` overrides the active repo for one call."
    )]
    async fn github_issues(
        &self,
        Parameters(args): Parameters<GithubIssuesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let repo = match self.resolve_repo(args.repo_name) {
            Ok(r) => r,
            Err(msg) => {
                return Ok(CallToolResult::success(vec![Content::text(msg)]));
            }
        };
        let body = _mcp_methods::github::github_issues_rust(
            Some(&repo),
            args.number,
            args.query.as_deref(),
            &args.kind,
            &args.state,
            args.sort.as_deref(),
            args.limit,
            args.labels.as_deref(),
        );
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    #[tool(
        description = "Read-only GET against the GitHub REST API. `path` may be a \
                       repo-relative endpoint (\"pulls?state=open\", \"commits/abc123\", \
                       \"branches\", \"compare/main...feature\") which is auto-prefixed \
                       with /repos/<repo_name>/, or an absolute resource (\"search/issues?q=...\", \
                       \"users/octocat\") which passes through. Returns JSON, truncated at \
                       80 KB by default."
    )]
    async fn github_api(
        &self,
        Parameters(args): Parameters<GithubApiArgs>,
    ) -> Result<CallToolResult, McpError> {
        let repo = match self.resolve_repo(args.repo_name) {
            Ok(r) => r,
            Err(msg) => {
                return Ok(CallToolResult::success(vec![Content::text(msg)]));
            }
        };
        let truncate_at = args.truncate_at.unwrap_or(80_000);
        let body = _mcp_methods::github::git_api_internal(&repo, &args.path, truncate_at);
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

#[tool_handler]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let name = self
            .options
            .name
            .clone()
            .unwrap_or_else(|| "MCP Server".to_string());
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(name, env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2024_11_05);
        if let Some(text) = &self.options.instructions {
            info = info.with_instructions(text.clone());
        }
        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_from_manifest_uses_name_when_set() {
        let opts = ServerOptions::from_manifest(None, "Fallback");
        assert_eq!(opts.name.as_deref(), Some("Fallback"));
    }

    #[test]
    fn server_constructs() {
        let _server = McpServer::new(ServerOptions::default());
    }

    #[test]
    fn static_source_roots_provider() {
        let opts = ServerOptions::default()
            .with_static_source_roots(vec!["/tmp/a".to_string(), "/tmp/b".to_string()]);
        let server = McpServer::new(opts);
        assert_eq!(
            server.current_source_roots(),
            vec!["/tmp/a".to_string(), "/tmp/b".to_string()]
        );
    }

    #[test]
    fn no_provider_returns_empty_roots() {
        let server = McpServer::new(ServerOptions::default());
        assert!(server.current_source_roots().is_empty());
    }

    #[test]
    fn dynamic_provider_swaps_at_call_time() {
        use std::sync::Mutex;
        let state = Arc::new(Mutex::new(vec!["/initial".to_string()]));
        let s2 = state.clone();
        let provider: SourceRootsProvider = Arc::new(move || s2.lock().unwrap().clone());
        let opts = ServerOptions::default().with_dynamic_source_roots(provider);
        let server = McpServer::new(opts);
        assert_eq!(server.current_source_roots(), vec!["/initial".to_string()]);
        *state.lock().unwrap() = vec!["/swapped".to_string()];
        assert_eq!(server.current_source_roots(), vec!["/swapped".to_string()]);
    }
}
