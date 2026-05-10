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

use std::sync::{Arc, Mutex};

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
    /// Workspace handle (when `--workspace` mode is active).
    pub workspace: Option<crate::workspace::Workspace>,
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
            workspace: None,
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

    /// Bind a workspace handle. Source roots and default repo become
    /// dynamic — both are read from the workspace's active-repo state
    /// at every tool call, so `repo_management` swapping the active
    /// repo immediately re-points the source tools.
    pub fn with_workspace(mut self, ws: crate::workspace::Workspace) -> Self {
        let ws_for_roots = ws.clone();
        let ws_for_repo = ws.clone();
        self.workspace = Some(ws);
        self.source_roots = Some(Arc::new(move || {
            ws_for_roots
                .active_repo_path()
                .map(|p| vec![p.to_string_lossy().into_owned()])
                .unwrap_or_default()
        }));
        self.default_repo = Some(Arc::new(move || ws_for_repo.active_repo_name()));
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
pub struct RepoManagementArgs {
    /// org/repo to clone and activate. Omit for list mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Delete the repo + inventory entry instead of activating.
    #[serde(default)]
    pub delete: bool,
    /// Refresh the active repo (no name required).
    #[serde(default)]
    pub update: bool,
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
    /// Drill-down: cached collapsed-element ID returned by a previous
    /// FETCH (e.g. ``"cb_1"``, ``"comment_3"``, ``"overflow"``). When
    /// set, `number` is required and the call returns the cached
    /// element instead of re-fetching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    /// Line range filter for drill-down (``"N-M"`` 1-indexed). Only
    /// meaningful alongside `element_id`. For comment segments,
    /// interpreted as comment-index range.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<String>,
    /// Regex pattern for drill-down. Only meaningful alongside
    /// `element_id`. Returns matching lines/items plus context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grep: Option<String>,
    /// Context lines around each grep match in drill-down mode
    /// (default 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<usize>,
    /// Force a re-fetch (skip cache) when in FETCH mode. Useful after
    /// an issue has been updated upstream.
    #[serde(default)]
    pub refresh: bool,
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

impl Default for GithubIssuesArgs {
    fn default() -> Self {
        Self {
            number: None,
            repo_name: None,
            query: None,
            kind: default_kind(),
            state: default_state(),
            sort: None,
            limit: default_limit(),
            labels: None,
            element_id: None,
            lines: None,
            grep: None,
            context: None,
            refresh: false,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
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
        let mut server = Self {
            options,
            tool_router: Self::tool_router(),
        };
        server.register_github_tools_if_authorized();
        server
    }

    /// Register `github_issues` + `github_api` as dynamic tools — but
    /// only when a GitHub token is reachable. This is honest tool
    /// listing: agents see the tool only if it can actually succeed.
    /// Decision is boot-time; restart the server to pick up a token
    /// that appears later.
    fn register_github_tools_if_authorized(&mut self) {
        if !_mcp_methods::github::has_git_token() {
            tracing::info!(
                "GITHUB_TOKEN not set — github_issues / github_api tools hidden from the agent. \
                 Set the env var and restart to enable them."
            );
            return;
        }
        let default_repo = self.options.default_repo.clone();
        let repo_provider = default_repo.clone();
        // Per-server ElementCache: stores collapsed elements (cb_1,
        // patch_2, comment_3, overflow) emitted by FETCH so the agent
        // can drill down via `element_id` on subsequent calls without
        // re-fetching the whole issue. Mutex contention is negligible
        // for MCP's serial request dispatch.
        let cache: Arc<Mutex<_mcp_methods::cache::ElementCache>> =
            Arc::new(Mutex::new(_mcp_methods::cache::ElementCache::new()));
        let cache_for_issues = cache.clone();
        self.register_typed_tool::<GithubIssuesArgs, _>(
            "github_issues",
            "Search, list, or fetch GitHub issues / pull requests / Discussions. \
             Pass `number=N` for FETCH (single issue/PR/discussion); `query=\"...\"` \
             for SEARCH (across issues+PRs and Discussions); neither for LIST. \
             `kind` ∈ \"issue\" / \"pr\" / \"discussion\" / \"all\" (default). \
             `state` ∈ \"open\" (default) / \"closed\" / \"all\". `limit` caps \
             result count (default 20). `labels` is a comma-separated string. \
             `repo_name=\"org/repo\"` overrides the active repo for one call. \
             FETCH responses collapse big code blocks / patches / comments into \
             `cb_N` / `patch_N` / `comment_N` / `overflow` placeholders; pass \
             `element_id=\"cb_1\"` (with the same `number`) to retrieve a single \
             element, optionally narrowed by `lines=\"40-60\"` or `grep=\"pat\"`. \
             `refresh=true` bypasses the cache for re-fetch.",
            move |args: GithubIssuesArgs| {
                let repo = match resolve_repo_from(repo_provider.as_ref(), args.repo_name.clone()) {
                    Ok(r) => r,
                    Err(msg) => return msg,
                };
                // FETCH / drill-down: route through ElementCache so cb_*,
                // patch_*, overflow stays addressable. Cache.fetch_issue
                // does both the network fetch and the drill-down branch.
                if let Some(number) = args.number {
                    let context = args.context.unwrap_or(3);
                    let mut guard = cache_for_issues.lock().unwrap();
                    return match guard.fetch_issue(
                        &repo,
                        number,
                        args.element_id.as_deref(),
                        args.lines.as_deref(),
                        args.grep.as_deref(),
                        context,
                        args.refresh,
                    ) {
                        Ok(body) => body,
                        Err(e) => format!("github_issues fetch error: {e}"),
                    };
                }
                if args.element_id.is_some() {
                    return "element_id requires `number=N` (the issue/PR being drilled into)."
                        .to_string();
                }
                // SEARCH / LIST: no caching, pure delegation.
                _mcp_methods::github::github_issues_rust(
                    Some(&repo),
                    args.number,
                    args.query.as_deref(),
                    &args.kind,
                    &args.state,
                    args.sort.as_deref(),
                    args.limit,
                    args.labels.as_deref(),
                )
            },
        );
        let repo_provider = default_repo;
        self.register_typed_tool::<GithubApiArgs, _>(
            "github_api",
            "Read-only GET against the GitHub REST API. `path` may be a \
             repo-relative endpoint (\"pulls?state=open\", \"commits/abc123\", \
             \"branches\", \"compare/main...feature\") which is auto-prefixed \
             with /repos/<repo_name>/, or an absolute resource (\"search/issues?q=...\", \
             \"users/octocat\") which passes through. Returns JSON, truncated at \
             80 KB by default.",
            move |args: GithubApiArgs| match resolve_repo_from(
                repo_provider.as_ref(),
                args.repo_name.clone(),
            ) {
                Ok(repo) => {
                    let truncate_at = args.truncate_at.unwrap_or(80_000);
                    _mcp_methods::github::git_api_internal(&repo, &args.path, truncate_at)
                }
                Err(msg) => msg,
            },
        );
    }

    /// Mutable access to the tool router for dynamic tool registration.
    ///
    /// Use only at server-construction time (before [`serve`](rmcp::ServiceExt::serve)).
    /// Once dispatching starts, the router is cloned per request and
    /// mutation would race.
    pub fn tool_router_mut(&mut self) -> &mut ToolRouter<McpServer> {
        &mut self.tool_router
    }

    /// Register a typed dynamic tool. Compresses the boilerplate of:
    /// 1. Generating a JSON Schema for the args type via `schemars`.
    /// 2. Building a [`rmcp::model::Tool`] attr from the schema +
    ///    name + description.
    /// 3. Deserialising the per-call JSON arguments via serde.
    /// 4. Wrapping the handler in a [`rmcp::handler::server::router::tool::ToolRoute::new_dyn`]
    ///    closure suitable for [`tool_router_mut`](Self::tool_router_mut).
    ///
    /// The handler is `Fn(T) -> String`; it owns whatever state it
    /// needs through the closure environment (typically an Arc-clone
    /// of a domain-specific state handle). Returning a string means
    /// the tool reports a clean text body to the agent rather than
    /// exposing a tool-error envelope — matches the framework's
    /// "errors as values" convention for source / GitHub tools.
    pub fn register_typed_tool<T, F>(
        &mut self,
        name: &'static str,
        description: &'static str,
        handler: F,
    ) where
        T: for<'de> serde::Deserialize<'de>
            + schemars::JsonSchema
            + Default
            + Send
            + Sync
            + 'static,
        F: Fn(T) -> String + Send + Sync + 'static,
    {
        use std::pin::Pin;
        type DynFut<'a, R> = Pin<Box<dyn std::future::Future<Output = R> + Send + 'a>>;

        let schema_obj = serde_json::to_value(schemars::schema_for!(T))
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let attr = rmcp::model::Tool::new(name, description, Arc::new(schema_obj));
        let handler = std::sync::Arc::new(handler);

        self.tool_router
            .add_route(rmcp::handler::server::router::tool::ToolRoute::new_dyn(
                attr,
                move |ctx: rmcp::handler::server::tool::ToolCallContext<'_, McpServer>|
                    -> DynFut<'_, Result<rmcp::model::CallToolResult, rmcp::ErrorData>> {
                    let handler = handler.clone();
                    let arguments = ctx.arguments.clone();
                    Box::pin(async move {
                        let args: T = match arguments {
                            Some(map) => {
                                match serde_json::from_value(serde_json::Value::Object(map)) {
                                    Ok(a) => a,
                                    Err(e) => {
                                        return Ok(rmcp::model::CallToolResult::success(vec![
                                            rmcp::model::Content::text(format!(
                                                "invalid arguments: {e}"
                                            )),
                                        ]));
                                    }
                                }
                            }
                            None => T::default(),
                        };
                        let body = handler(args);
                        Ok(rmcp::model::CallToolResult::success(vec![
                            rmcp::model::Content::text(body),
                        ]))
                    })
                },
            ));
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
    #[allow(dead_code)]
    fn resolve_repo(&self, override_repo: Option<String>) -> Result<String, String> {
        resolve_repo_from(self.options.default_repo.as_ref(), override_repo)
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
        description = "Manage GitHub repos in the workspace. Pass `name='org/repo'` to \
                       clone (if missing) and activate it as the source root for \
                       read_source / grep / list_source. Pass `delete=true` to remove a \
                       repo. Pass `update=true` to fetch upstream changes for the active \
                       repo. Call with no arguments to list all known repos with their \
                       last-access counts. Idle repos auto-sweep on each call (default 7 \
                       days, configurable via --stale-after-days)."
    )]
    async fn repo_management(
        &self,
        Parameters(args): Parameters<RepoManagementArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = match &self.options.workspace {
            Some(ws) => ws.repo_management(args.name.as_deref(), args.delete, args.update),
            None => "repo_management requires --workspace mode.".to_string(),
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }
}

/// Resolve `org/repo`: per-call override → configured default →
/// auto-detect from cwd. Returns either the resolved repo or a
/// formatted user-facing error message.
///
/// Free function (not a method) so it can be called from closures
/// captured by [`McpServer::register_typed_tool`] which only see
/// `Fn(T) -> String` — no `&self`.
fn resolve_repo_from(
    default_repo: Option<&RepoProvider>,
    override_repo: Option<String>,
) -> Result<String, String> {
    if let Some(r) = override_repo {
        if let Some(err) = _mcp_methods::git_refs::validate_repo(&r) {
            return Err(err);
        }
        return Ok(r);
    }
    if let Some(provider) = default_repo {
        if let Some(r) = provider() {
            if let Some(err) = _mcp_methods::git_refs::validate_repo(&r) {
                return Err(err);
            }
            return Ok(r);
        }
    }
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

#[tool_handler(router = self.tool_router)]
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
