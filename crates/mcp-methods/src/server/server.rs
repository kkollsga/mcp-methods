//! MCP `ServerHandler` implementation.
//!
//! Tool surface, top to bottom:
//!
//! - **Always registered**: `ping`; the source tools (`read_source`,
//!   `grep`, `list_source`) gated on an active source-roots provider;
//!   `repo_management` (no-ops outside `--workspace` mode).
//! - **Conditionally registered at boot** (dynamic):
//!   - `github_issues` and `github_api` — only when `GITHUB_TOKEN` is
//!     reachable. This is "honest tool listing": agents see the tools
//!     only when they can succeed. Decision is boot-time; restart the
//!     server to pick up a token that appears later.
//!   - `set_root_dir` — only when the bound workspace is local-flavoured
//!     (`workspace.kind: local`); swaps the active root at runtime.
//!   - Manifest-declared `python:` tools and `cypher:` tools — added by
//!     downstream binaries through `apply_python_extensions`.
//!
//! The source-roots provider is dynamic — workspace mode swaps it as
//! the active repo changes; source-root and watch modes wire it to a
//! fixed root; local-workspace mode rebinds it on `set_root_dir`. An
//! empty list signals "no active source" and the tools return a
//! friendly error rather than failing the call.
//!
//! Per-server state held on `McpServer` (cloned per request via `Arc`):
//! a `ServerOptions` struct (providers + workspace handle + manifest
//! builtins) and the rmcp `ToolRouter`. The `github_issues` closure
//! additionally captures an `Arc<Mutex<ElementCache>>` so FETCH calls
//! can cache collapsed elements (`cb_N`, `patch_N`, `comment_N`,
//! `overflow`) for the agent to drill into via `element_id` on
//! subsequent calls — no re-fetching.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use rmcp::handler::server::router::prompt::{PromptRoute, PromptRouter};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::*;
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::server::manifest::Manifest;
use crate::server::skills::ResolvedRegistry;
use crate::server::source::{
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
    pub workspace: Option<crate::server::workspace::Workspace>,
    /// Manifest-declared `builtins:` block. Surfaced verbatim so
    /// downstream consumers (kglite's `graph_overview` tool, for
    /// example) can read `temp_cleanup` / `save_graph` settings and
    /// implement the corresponding behaviour without re-parsing YAML.
    pub builtins: crate::server::manifest::BuiltinsConfig,
    /// Manifest-declared `extensions:` block. The framework uses this
    /// for the `extension_enabled:` skill predicate; downstream
    /// consumers can also read it for their own per-extension config.
    /// Empty map when no `extensions:` block is present.
    pub extensions: serde_json::Map<String, serde_json::Value>,
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
            builtins: manifest.map(|m| m.builtins.clone()).unwrap_or_default(),
            extensions: manifest.map(|m| m.extensions.clone()).unwrap_or_default(),
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
    pub fn with_workspace(mut self, ws: crate::server::workspace::Workspace) -> Self {
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

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SetRootDirArgs {
    /// Absolute or relative path to bind as the new source root.
    pub path: String,
}

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
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
    /// Bypass the auto-rebuild gate: re-run the post-activate hook
    /// even when the HEAD SHA matches the last successful build.
    /// Useful after upgrading the builder code itself.
    #[serde(default)]
    pub force_rebuild: bool,
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
    /// Skill-backed prompt routes. Empty until [`serve_prompts`] is
    /// called with a resolved skill registry; remains empty for the
    /// existing zero-skills boot path so `prompts/list` returns the
    /// rmcp default (empty result, no capability advertised).
    prompt_router: PromptRouter<McpServer>,
}

#[tool_router]
impl McpServer {
    pub fn new(options: ServerOptions) -> Self {
        let mut server = Self {
            options,
            tool_router: Self::tool_router(),
            prompt_router: PromptRouter::new(),
        };
        server.register_github_tools_if_authorized();
        server.register_local_workspace_tools();
        server.gate_workspace_tools();
        server
    }

    /// Drop `repo_management` from the router when no workspace is
    /// bound — `tools/list` should reflect the actual surface, not a
    /// tool whose handler immediately errors out with "requires
    /// --workspace mode." Mirrors the gating downstream binaries
    /// (e.g. `kglite-mcp-server`) apply to the same tool. Operators
    /// comparing the bare framework against a downstream binary's
    /// surface see consistent behaviour now.
    fn gate_workspace_tools(&mut self) {
        if self.options.workspace.is_none() {
            self.tool_router.remove_route("repo_management");
        }
    }

    /// Register `set_root_dir` when the bound workspace is local-flavoured.
    /// Github workspaces use `repo_management(name='org/repo')` to swap
    /// roots; local workspaces need this alternative entry point.
    fn register_local_workspace_tools(&mut self) {
        let Some(ws) = self.options.workspace.clone() else {
            return;
        };
        if !matches!(ws.kind(), crate::server::workspace::WorkspaceKind::Local) {
            return;
        }
        self.register_typed_tool::<SetRootDirArgs, _>(
            "set_root_dir",
            "Swap the active source root (local-workspace mode only). Pass `path` \
             to a directory; the framework canonicalises it, rebinds the source \
             tools (`read_source`, `grep`, `list_source`), and fires the post-\
             activate hook so any downstream graph rebuilds against the new root. \
             Inventory persists across swaps; SHA-gating skips rebuilds when the \
             same root is re-bound with no content changes.",
            move |args: SetRootDirArgs| {
                let p = std::path::PathBuf::from(&args.path);
                ws.set_root_dir(&p)
            },
        );
    }

    /// Register `github_issues` + `github_api` as dynamic tools — but
    /// only when a GitHub token is reachable. This is honest tool
    /// listing: agents see the tool only if it can actually succeed.
    /// Decision is boot-time; restart the server to pick up a token
    /// that appears later.
    fn register_github_tools_if_authorized(&mut self) {
        if !crate::github::has_git_token() {
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
        let cache: Arc<Mutex<crate::cache::ElementCache>> =
            Arc::new(Mutex::new(crate::cache::ElementCache::new()));
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
                // All paths return a status `String` — invalid-repo,
                // fetch-failure, cached-summary, overflow, full-text.
                if let Some(number) = args.number {
                    let context = args.context.unwrap_or(3);
                    let mut guard = cache_for_issues.lock().unwrap();
                    return guard.fetch_issue(
                        &repo,
                        number,
                        args.element_id.as_deref(),
                        args.lines.as_deref(),
                        args.grep.as_deref(),
                        context,
                        args.refresh,
                    );
                }
                if args.element_id.is_some() {
                    return "element_id requires `number=N` (the issue/PR being drilled into)."
                        .to_string();
                }
                // SEARCH / LIST: no caching, pure delegation.
                crate::github::github_issues_rust(
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
                    crate::github::git_api_internal(&repo, &args.path, truncate_at)
                }
                Err(msg) => msg,
            },
        );
    }

    /// Read the manifest-declared `builtins:` config. Downstream
    /// consumers (e.g. a `graph_overview` tool that wipes a `temp/`
    /// directory when `temp_cleanup: on_overview` is set) call this
    /// to discover what flags the operator asked for. The framework
    /// itself does not act on this — that would force it to interpret
    /// graph-specific semantics it shouldn't know about.
    pub fn builtins(&self) -> &crate::server::manifest::BuiltinsConfig {
        &self.options.builtins
    }

    /// Mutable access to the tool router for dynamic tool registration.
    ///
    /// Use only at server-construction time (before [`serve`](rmcp::ServiceExt::serve)).
    /// Once dispatching starts, the router is cloned per request and
    /// mutation would race.
    pub fn tool_router_mut(&mut self) -> &mut ToolRouter<McpServer> {
        &mut self.tool_router
    }

    /// Mutable access to the prompt router for dynamic skill / prompt
    /// registration. Same lifecycle contract as [`tool_router_mut`]:
    /// boot-time only. Most operators reach prompts via
    /// [`serve_prompts`] rather than touching the router directly.
    pub fn prompt_router_mut(&mut self) -> &mut PromptRouter<McpServer> {
        &mut self.prompt_router
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
                       repo (rebuild auto-skipped when HEAD hasn't moved since the last \
                       build; set `force_rebuild=true` to bypass). Call with no \
                       arguments to list all known repos with their last-access counts. \
                       Idle repos auto-sweep on each call (default 7 days, configurable \
                       via --stale-after-days)."
    )]
    async fn repo_management(
        &self,
        Parameters(args): Parameters<RepoManagementArgs>,
    ) -> Result<CallToolResult, McpError> {
        let body = match &self.options.workspace {
            Some(ws) => ws.repo_management(
                args.name.as_deref(),
                args.delete,
                args.update,
                args.force_rebuild,
            ),
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
        if let Some(err) = crate::git_refs::validate_repo(&r) {
            return Err(err);
        }
        return Ok(r);
    }
    if let Some(provider) = default_repo {
        if let Some(r) = provider() {
            if let Some(err) = crate::git_refs::validate_repo(&r) {
                return Err(err);
            }
            return Ok(r);
        }
    }
    if let Some(detected) = crate::github::detect_git_repo(".") {
        if crate::git_refs::validate_repo(&detected).is_none() {
            return Ok(detected);
        }
    }
    Err(
        "No active repository. Pass `repo_name='org/repo'`, configure a default in the \
         server, or run from a directory whose git remote points at github.com."
            .to_string(),
    )
}

/// Wire a resolved skill registry into a server's `prompts/list` and
/// `prompts/get` surface, and apply auto-injection hints to tool
/// descriptions for skills whose name matches a registered tool.
///
/// Call at boot time after all tools have been registered (so the
/// auto-inject pass sees the final tool catalogue) and before
/// `serve(...)`. Idempotent in spirit but not by construction:
/// calling twice with the same registry would re-append the hint to
/// already-injected descriptions, so don't.
///
/// The function is additive and a no-op when the registry is empty
/// — downstream callers can wire it unconditionally without breaking
/// the zero-skills boot path.
pub fn serve_prompts(registry: &ResolvedRegistry, server: &mut McpServer) {
    use std::borrow::Cow;
    use std::collections::HashSet;

    // Build the framework-internal predicate state once. The tool
    // router has the full registered-tool list; extensions come from
    // the manifest's builtins block (operators may have nothing
    // here, in which case all `extension_enabled:` predicates fail).
    let registered_tools: HashSet<String> = server
        .tool_router
        .list_all()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    let extensions = server.options.extensions.clone();

    // For the auto-inject pass: skills whose name matches a registered
    // tool get their full body embedded into that tool's description.
    // Tracking (name, body) — see the comment at the bottom of the
    // function for why this is the body, not a pointer.
    let mut auto_inject: Vec<(String, String)> = Vec::new();

    for name in registry.skill_names() {
        let Some(skill) = registry.get(&name) else {
            continue;
        };

        // Evaluate `applies_when:` against the runtime state. Skills
        // with all predicates satisfied register; others are
        // suppressed from the agent-facing surface.
        let activation = registry.activation_for(skill, &registered_tools, &extensions);
        if !activation.active {
            let failed_clauses: Vec<&str> = activation
                .clauses
                .iter()
                .filter(|(_, outcome)| {
                    *outcome != crate::server::skills::PredicateOutcome::Satisfied
                })
                .map(|(clause, _)| clause.as_str())
                .collect();
            tracing::info!(
                skill = %name,
                suppressed_by = ?failed_clauses,
                "skill suppressed by applies_when predicates"
            );
            continue;
        }

        let prompt = Prompt::new(
            skill.name().to_string(),
            Some(skill.description().to_string()),
            None,
        );
        let body = skill.body.clone();
        let route = PromptRoute::new_dyn(prompt, move |_ctx| {
            let body = body.clone();
            Box::pin(async move {
                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    PromptMessageRole::Assistant,
                    body,
                )]))
            })
        });
        server.prompt_router.add_route(route);

        if skill.frontmatter.auto_inject_hint {
            auto_inject.push((skill.name().to_string(), skill.body.clone()));
        }
    }

    // Auto-inject the full skill body into matching tool descriptions.
    //
    // Background: pre-0.3.37 this loop appended a short pointer line
    // (`See `prompts/get` <name> for the full methodology.`) to the
    // tool description, assuming agents could call `prompts/get` to
    // fetch the body. **They can't** in real MCP clients — Claude Code,
    // Claude Desktop, Cursor, and Continue all expose only `tools/*`
    // to the model; the `prompts/` plane was designed for human-
    // invoked slash commands. Operators authoring against the pointer
    // pattern shipped methodology the agent literally could not read.
    //
    // The fix: when `auto_inject_hint: true` AND a registered tool
    // shares the skill's name, embed the full body under a
    // `## Methodology` header. Bounded by the 4 KB soft / 16 KB hard
    // size caps the framework already enforces per skill. Operators
    // who want the smaller pointer-only behaviour set
    // `auto_inject_hint: false` per skill.
    //
    // `prompts/list` / `prompts/get` continue to work for any client
    // that does surface them to the agent, plus CLI introspection.
    // This pass just makes the *primary* delivery channel a place
    // agents actually look.
    for (skill_name, body) in &auto_inject {
        let key = Cow::<'static, str>::Owned(skill_name.clone());
        if let Some(route) = server.tool_router.map.get_mut(&key) {
            let trimmed_body = body.trim();
            let inject = format!("\n\n## Methodology\n\n{trimmed_body}");
            let new_desc = match route.attr.description.take() {
                Some(existing) => format!("{existing}{inject}"),
                None => inject.trim_start().to_string(),
            };
            route.attr.description = Some(Cow::Owned(new_desc));
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let name = self
            .options
            .name
            .clone()
            .unwrap_or_else(|| "MCP Server".to_string());
        // Only advertise the prompts capability when at least one skill
        // is registered. The zero-skills boot path is the existing
        // contract and must keep producing capability output that's
        // byte-identical to today. ServerCapabilities is `#[non_exhaustive]`
        // but its fields are pub, so we mutate after `build()` rather
        // than fighting the type-state builder.
        let mut caps = ServerCapabilities::builder().enable_tools().build();
        if !self.prompt_router.map.is_empty() {
            caps.prompts = Some(PromptsCapability::default());
        }
        let mut info = ServerInfo::new(caps)
            .with_server_info(Implementation::new(name, env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2024_11_05);
        if let Some(text) = &self.options.instructions {
            info = info.with_instructions(text.clone());
        }
        info
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult {
            meta: None,
            next_cursor: None,
            prompts: self.prompt_router.list_all(),
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResult, McpError> {
        let prompt_context = rmcp::handler::server::prompt::PromptContext::new(
            self,
            request.name,
            request.arguments,
            context,
        );
        self.prompt_router.get_prompt(prompt_context).await
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
    fn builtins_exposed_via_server() {
        use crate::server::manifest::{BuiltinsConfig, TempCleanup};
        let opts = ServerOptions {
            builtins: BuiltinsConfig {
                save_graph: true,
                temp_cleanup: TempCleanup::OnOverview,
            },
            ..ServerOptions::default()
        };
        let server = McpServer::new(opts);
        assert!(server.builtins().save_graph);
        assert_eq!(server.builtins().temp_cleanup, TempCleanup::OnOverview);
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
    fn repo_management_gated_to_workspace_mode() {
        // Bare (no workspace): repo_management should NOT be in the
        // router. Mirrors the gating downstream binaries apply.
        let server = McpServer::new(ServerOptions::default());
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            !names.contains(&"repo_management"),
            "repo_management should be gated out without a workspace; tools were {names:?}"
        );
    }

    #[test]
    fn repo_management_present_when_workspace_bound() {
        // With a workspace handle bound, repo_management should be
        // registered.
        use crate::server::workspace::Workspace;
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let opts = ServerOptions::default().with_workspace(ws);
        let server = McpServer::new(opts);
        let tools = server.tool_router.list_all();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(
            names.contains(&"repo_management"),
            "repo_management should be registered with a workspace; tools were {names:?}"
        );
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

    // ─── Prompt / skill wiring ────────────────────────────────────

    fn build_test_registry(
        skills: &[(&str, &str, &str, bool)],
    ) -> crate::server::skills::ResolvedRegistry {
        use crate::server::skills::Registry;
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("manifest.yaml");
        let skills_dir = dir.path().join("manifest.skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        for (name, description, body, auto_inject) in skills {
            let auto = if *auto_inject { "true" } else { "false" };
            let content = format!(
                "---\nname: {name}\ndescription: {description}\nauto_inject_hint: {auto}\n---\n\n{body}\n"
            );
            std::fs::write(skills_dir.join(format!("{name}.md")), content).unwrap();
        }
        Registry::new()
            .auto_detect_project_layer(&yaml_path)
            .finalise()
            .unwrap()
    }

    #[test]
    fn prompt_router_empty_by_default() {
        let server = McpServer::new(ServerOptions::default());
        assert!(server.prompt_router.map.is_empty());
    }

    #[test]
    fn get_info_no_prompts_capability_when_empty() {
        // Zero-impact invariant: a server with no skills must not
        // advertise the prompts capability. kglite's existing
        // deployment depends on this byte-for-byte.
        let server = McpServer::new(ServerOptions::default());
        let info = server.get_info();
        assert!(
            info.capabilities.prompts.is_none(),
            "prompts capability must be absent when no skills are registered"
        );
    }

    #[test]
    fn serve_prompts_registers_routes_with_metadata() {
        let registry = build_test_registry(&[
            ("alpha", "First skill.", "Alpha body.", true),
            ("beta", "Second skill.", "Beta body.", true),
        ]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);

        let prompts = server.prompt_router.list_all();
        let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);

        let alpha = prompts.iter().find(|p| p.name == "alpha").unwrap();
        assert_eq!(alpha.description.as_deref(), Some("First skill."));
        assert!(alpha.arguments.is_none());
    }

    #[test]
    fn serve_prompts_empty_registry_is_noop() {
        let registry = crate::server::skills::ResolvedRegistry::default();
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        assert!(server.prompt_router.map.is_empty());
        assert!(server.get_info().capabilities.prompts.is_none());
    }

    #[test]
    fn get_info_advertises_prompts_when_present() {
        let registry = build_test_registry(&[("alpha", "First skill.", "Alpha body.", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let info = server.get_info();
        assert!(
            info.capabilities.prompts.is_some(),
            "prompts capability must be advertised once a skill is registered"
        );
    }

    #[test]
    fn serve_prompts_auto_injects_full_body_into_matching_tool() {
        // `ping` is registered by every server. A skill named `ping`
        // with `auto_inject_hint: true` should embed its full body
        // under a `## Methodology` header in the ping tool's
        // description. Pre-0.3.37 this appended a short pointer at
        // `prompts/get`, but agents in real MCP clients can't reach
        // that surface — see the comment on the auto-inject loop in
        // `serve_prompts`.
        let registry =
            build_test_registry(&[("ping", "Ping methodology.", "PING-BODY-SENTINEL", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        let before = server
            .tool_router
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        super::serve_prompts(&registry, &mut server);
        let after = server
            .tool_router
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        assert!(after.starts_with(&before), "original description preserved");
        assert!(
            after.contains("## Methodology"),
            "inject should include a Methodology header; got: {after}"
        );
        assert!(
            after.contains("PING-BODY-SENTINEL"),
            "inject should embed the full skill body; got: {after}"
        );
        assert!(
            !after.contains("prompts/get"),
            "post-0.3.37 inject should NOT reference the prompts/get surface (agents can't reach it); got: {after}"
        );
    }

    #[test]
    fn serve_prompts_skips_injection_when_disabled() {
        let registry = build_test_registry(&[("ping", "Ping methodology.", "Ping body.", false)]);
        let mut server = McpServer::new(ServerOptions::default());
        let before = server
            .tool_router
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        super::serve_prompts(&registry, &mut server);
        let after = server
            .tool_router
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        assert_eq!(
            before, after,
            "auto_inject_hint=false must leave tool description untouched"
        );
    }

    #[test]
    fn serve_prompts_skips_injection_when_no_matching_tool() {
        // Skill name doesn't match any registered tool; nothing to
        // inject into, but the prompt route is still added.
        let registry = build_test_registry(&[("no_such_tool", "Methodology.", "Body.", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        assert!(server.prompt_router.map.contains_key("no_such_tool"));
        // No panic, no mutation of unrelated tools — the ping tool's
        // description is unchanged.
        let ping_desc = server
            .tool_router
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        assert!(!ping_desc.contains("no_such_tool"));
    }

    fn write_gated_project_skill(applies_when_yaml: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        std::fs::write(&yaml, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        std::fs::create_dir(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("gated_skill.md"),
            format!(
                "---\n\
                 name: gated_skill\n\
                 description: A predicate-gated skill for testing.\n\
                 applies_when:\n\
                 {applies_when_yaml}\n\
                 ---\n\n\
                 Body.\n",
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn serve_prompts_suppresses_skill_with_unsatisfied_predicate() {
        // `tool_registered: nonexistent_tool` — that tool isn't in
        // the registered catalogue, so the predicate fails and the
        // skill is omitted from `prompts/list`.
        use crate::server::skills::Registry as SkillsBuilder;
        let dir = write_gated_project_skill("  tool_registered: nonexistent_tool");
        let yaml = dir.path().join("test_mcp.yaml");
        let registry = SkillsBuilder::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        assert!(
            !server.prompt_router.map.contains_key("gated_skill"),
            "skill with unsatisfied predicate must be suppressed"
        );
    }

    #[test]
    fn serve_prompts_keeps_skill_with_satisfied_predicate() {
        // `tool_registered: ping` — ping is always registered, so
        // the predicate satisfies and the skill registers.
        use crate::server::skills::Registry as SkillsBuilder;
        let dir = write_gated_project_skill("  tool_registered: ping");
        let yaml = dir.path().join("test_mcp.yaml");
        let registry = SkillsBuilder::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        assert!(
            server.prompt_router.map.contains_key("gated_skill"),
            "skill with satisfied predicate must register"
        );
    }

    #[test]
    fn serve_prompts_evaluates_extension_enabled_from_manifest() {
        // The `extension_enabled:` predicate reads from
        // `ServerOptions.extensions`. Verify it integrates end-to-end
        // when the manifest declares the extension.
        use crate::server::skills::Registry as SkillsBuilder;
        let dir = write_gated_project_skill("  extension_enabled: csv_http_server");
        let yaml = dir.path().join("test_mcp.yaml");
        let registry = SkillsBuilder::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();

        // Without the extension declared — suppressed.
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        assert!(!server.prompt_router.map.contains_key("gated_skill"));

        // With the extension declared — registers.
        let mut extensions = serde_json::Map::new();
        extensions.insert("csv_http_server".to_string(), serde_json::json!(true));
        let opts = ServerOptions {
            extensions,
            ..ServerOptions::default()
        };
        let mut server = McpServer::new(opts);
        super::serve_prompts(&registry, &mut server);
        assert!(server.prompt_router.map.contains_key("gated_skill"));
    }
}
