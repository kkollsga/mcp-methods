//! MCP `ServerHandler` implementation.
//!
//! Tool surface, top to bottom:
//!
//! - **Always registered**: `ping`; the source tools (`read_source`,
//!   `grep`, `list_source`) gated on an active source-roots provider.
//! - **Conditionally registered at boot** (dynamic):
//!   - `repo_management` — only with a `kind: github` workspace bound.
//!     A local workspace uses `set_root_dir` instead, and a server
//!     with no workspace at all has nothing for the tool to manage.
//!   - `github_issues`, `github_api` and `screen_stargazers` — only
//!     when the manifest opts in with `builtins.github: true` (default
//!     off, so a `GITHUB_TOKEN` reachable in the environment or via the
//!     `.env` walk-up never widens the surface on its own) *and* a
//!     token is actually reachable. The second gate is "honest tool
//!     listing": agents see the tools only when they can succeed. Both
//!     decisions are boot-time; restart the server to pick up a token
//!     or manifest change that appears later.
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

/// Read-only runtime context handed to a [`ResultPostprocessHook`].
/// Exposes the active source roots and repo so a consumer's hook can
/// tailor its footer to the current binding without capturing the
/// workspace itself. Decoupled by design — no framework types leak.
pub struct ResultCtx {
    /// Active source roots at call time (empty when none bound).
    pub source_roots: Vec<String>,
    /// Active workspace repo (`org/repo` or a synthetic local name),
    /// or `None` when nothing is bound.
    pub active_repo: Option<String>,
}

/// Hook invoked after every builtin tool produces its text result.
///
/// Receives the tool name, the call arguments (as JSON), the result
/// body, and a read-only [`ResultCtx`]. Returns `Some(footer)` to
/// append a steering line (the framework inserts a blank separator
/// line), or `None` to leave the result byte-for-byte unchanged.
///
/// This is the framework's *runtime* consumer→agent text channel — the
/// counterpart to the load-once tool descriptions. Consumers supply the
/// domain-aware content: e.g. a graph-backed server can detect a
/// definition-shaped `grep` pattern (or a zero-match result) and steer
/// the agent to `cypher_query`. The framework owns the hook; the graph
/// knowledge stays downstream.
pub type ResultPostprocessHook =
    Arc<dyn Fn(&str, &serde_json::Value, &str, &ResultCtx) -> Option<String> + Send + Sync>;

/// Append a hook-produced footer to a result body, separated by a
/// blank line. Empty/`None` footers leave the body untouched. Shared
/// by both dispatch paths so the footer contract lives in one place.
fn append_footer(body: String, footer: Option<String>) -> String {
    match footer {
        Some(f) if !f.is_empty() => format!("{body}\n\n{f}"),
        _ => body,
    }
}

/// The per-call body of a dynamically registered typed tool:
/// deserialise the arguments, run the handler, apply the consumer's
/// result-postprocess hook, and pick the MCP envelope. Both
/// [`McpServer::register_typed_tool`] and
/// [`McpServer::register_typed_tool_fallible`] install the same dyn
/// route and differ only in how their handler spells failure, so the
/// plumbing lives here once rather than in two near-identical
/// closures.
///
/// The hook runs on every arm — handler `Ok`, handler `Err`, and
/// arguments that never deserialised — because a footer that vanishes
/// exactly when something went wrong is a footer the agent can't rely
/// on: a downstream server that stamps identity or rebuild state onto
/// results needs that stamp most on the failure. `is_error` is the
/// only thing the arms disagree on.
fn dispatch_typed_call<T, F>(
    tool_name: &str,
    arguments: Option<rmcp::model::JsonObject>,
    handler: &F,
    postprocess: Option<&ResultPostprocessHook>,
    source_roots: Option<&SourceRootsProvider>,
    workspace: Option<&crate::server::workspace::Workspace>,
) -> rmcp::model::CallToolResult
where
    T: for<'de> serde::Deserialize<'de> + Default,
    F: Fn(T) -> Result<String, String>,
{
    // Preserve the raw args as JSON for the hook, before consuming
    // them into the typed `T`.
    let args_json = match &arguments {
        Some(map) => serde_json::Value::Object(map.clone()),
        None => serde_json::Value::Null,
    };
    let outcome = match arguments {
        Some(map) => match serde_json::from_value::<T>(serde_json::Value::Object(map)) {
            Ok(args) => handler(args),
            Err(e) => Err(format!("invalid arguments: {e}")),
        },
        None => handler(T::default()),
    };
    let is_error = outcome.is_err();
    let body = match outcome {
        Ok(body) | Err(body) => body,
    };
    let body = match postprocess {
        Some(hook) => {
            let ctx = ResultCtx {
                source_roots: source_roots.map(|p| p()).unwrap_or_default(),
                active_repo: workspace.and_then(|w| w.active_repo_name()),
            };
            let footer = hook(tool_name, &args_json, &body, &ctx);
            append_footer(body, footer)
        }
        None => body,
    };
    let content = vec![rmcp::model::ContentBlock::text(body)];
    if is_error {
        rmcp::model::CallToolResult::error(content)
    } else {
        rmcp::model::CallToolResult::success(content)
    }
}

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
    /// Ready-to-print explanations for `source_root(s)` entries the
    /// caller *declared* but could not resolve at boot.
    ///
    /// A boot path that degrades instead of dying (see
    /// [`resolve_source_roots_lenient`](crate::server::resolve_source_roots_lenient))
    /// has the only copy of that diagnosis, and it lands on stderr where
    /// no agent will ever see it. Handing it here lets `read_source` /
    /// `grep` / `list_source` say *which* declared root is missing and
    /// where it was looked for, instead of telling an operator who
    /// already configured `source_root:` to go configure `source_root:`.
    /// Empty when nothing was declared, or when everything resolved.
    /// Set via [`with_unresolved_source_roots`](Self::with_unresolved_source_roots).
    pub unresolved_source_roots: Vec<String>,
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
    /// Optional consumer hook run after every builtin tool result to
    /// append a runtime steering footer. `None` (default) leaves every
    /// result unchanged. See [`ResultPostprocessHook`].
    pub result_postprocess: Option<ResultPostprocessHook>,
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
            unresolved_source_roots: Vec::new(),
            default_repo: None,
            workspace: None,
            builtins: manifest.map(|m| m.builtins.clone()).unwrap_or_default(),
            extensions: manifest.map(|m| m.extensions.clone()).unwrap_or_default(),
            result_postprocess: None,
        }
    }

    pub fn with_static_source_roots(mut self, roots: Vec<String>) -> Self {
        let captured = Arc::new(roots);
        self.source_roots = Some(Arc::new(move || captured.as_ref().clone()));
        self
    }

    /// Record `source_root(s)` entries that were declared but did not
    /// resolve, as `(declared, path_it_was_looked_for_at)` pairs.
    ///
    /// Additive to whatever [`with_static_source_roots`](Self::with_static_source_roots)
    /// served: a manifest with three roots and one gone passes two
    /// resolved roots *and* one entry here. The pairs are rendered once,
    /// at call time, into the message the source tools return when no
    /// root is active — the wording matches the `ManifestError` the
    /// strict resolver would have produced.
    pub fn with_unresolved_source_roots(
        mut self,
        roots: Vec<(String, std::path::PathBuf)>,
    ) -> Self {
        self.unresolved_source_roots = roots
            .into_iter()
            .map(|(declared, path)| {
                format!(
                    "declared source root {declared:?} did not resolve: {:?} is not an \
                     existing directory",
                    path.display()
                )
            })
            .collect();
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
        self.default_repo = Some(Arc::new(move || ws_for_repo.default_github_repo()));
        self
    }

    /// Register a [`ResultPostprocessHook`] run after every builtin
    /// tool result. Consumers use this to append runtime steering (e.g.
    /// a graph-backed server nudging the agent from `grep` toward
    /// `cypher_query` when a pattern is definition-shaped).
    pub fn with_result_postprocess(mut self, hook: ResultPostprocessHook) -> Self {
        self.result_postprocess = Some(hook);
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
    /// Read the file at this git revision (tag, branch, or commit SHA)
    /// via `git show` instead of the working tree. Requires the active
    /// source root to be a git repository. All other options
    /// (`start_line`/`grep`/`max_chars`/…) apply to the historical
    /// content unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<String>,
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
    /// Optionally load multiple git revisions of the new root into one
    /// graph. An integer N loads the newest N stable release tags of the
    /// repo's dominant tag family plus HEAD (prereleases like rc/dev and
    /// unrelated tag families are skipped); a list of strings uses those
    /// git revspecs (tags, branches, or SHAs) verbatim. Requires the root
    /// to be a git repo. Omit for the default single-revision (working
    /// tree) activation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revs: Option<crate::server::workspace::RevsRequest>,
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
    /// Optionally load multiple git revisions of the repo into one graph.
    /// An integer N loads the newest N stable release tags of the repo's
    /// dominant tag family plus HEAD (prereleases like rc/dev and
    /// unrelated tag families are skipped); a list of strings uses those
    /// git revspecs (tags, branches, or SHAs) verbatim. Omit for the
    /// default single-revision (HEAD) activation. A revs request always
    /// rebuilds (the SHA-skip gate applies only to the plain path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revs: Option<crate::server::workspace::RevsRequest>,
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
    /// API path, with or without a leading slash. Repo-relative paths
    /// (e.g. "pulls?state=open", "commits/abc", "branches",
    /// "compare/main...x") are prefixed with /repos/<repo_name>/. Top-level
    /// resources ("search/issues?q=...", "users/octocat", "repos/o/r") pass
    /// through. A leading slash is accepted on either form — "/repos/o/r"
    /// and "repos/o/r" resolve identically.
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

#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ScreenStargazersArgs {
    /// Repo whose stargazers to screen, as "owner/repo".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Alternatively, screen an explicit set of users — comma-separated
    /// logins ("octocat,torvalds"). Takes precedence over `repo`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub users: Option<String>,
    /// Focused view via a named preset: "outreach" (relevant+active by
    /// reach), "peers" (your stack by effort), "legends" (biggest reach),
    /// "intel" (on-domain by popularity), "adopters" (actual users).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Or rank explicitly by one axis: relatedness | popularity | effort | recency.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank_by: Option<String>,
    /// Top-K for the focused/preset view (default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top: Option<usize>,
    /// Filter: minimum distinct keyword hits (relatedness gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_keywords: Option<usize>,
    /// Filter: only people active since this date (YYYY-MM-DD).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_since: Option<String>,
    /// Filter: only people who actually depend on the seed package.
    #[serde(default)]
    pub adopters_only: bool,
    /// Filter: only architectural (stack) peers.
    #[serde(default)]
    pub stack_only: bool,
    /// Comma-separated topic keywords for the relevance gate (e.g.
    /// "graph,rag,agent,llm"). Matched whole-word against repo
    /// name/topics/description; devs hitting ≥2 distinct keywords are
    /// surfaced as leads, single-keyword hits demoted to a footnote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keywords: Option<String>,
    /// Comma-separated languages defining the seed project's stack (e.g.
    /// "Rust,Python"). Stargazers using all of them are flagged as a
    /// keyword-invisible "stack match" to drill into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// Cap the number of stargazers screened (most-recent first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_stargazers: Option<usize>,
    /// Drill into the cached screen instead of returning the overview:
    /// `cohort:<key>`, `user:<login>`, `user:<login>/repo:<name>`, or
    /// ".../readme". Requires a prior no-element_id call for the repo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub element_id: Option<String>,
    /// Re-fetch from GitHub instead of reusing the cached screen.
    #[serde(default)]
    pub refresh: bool,
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

    /// Keep `repo_management` in the router only for a `kind: github`
    /// workspace — `tools/list` should reflect the actual surface, not
    /// a tool whose actions cannot apply.
    ///
    /// Three cases:
    ///
    /// - **No workspace**: dropped. The handler would immediately error
    ///   out with "requires --workspace mode." Mirrors the gating
    ///   downstream binaries (e.g. `kglite-mcp-server`) apply, so
    ///   operators comparing the bare framework against a downstream
    ///   binary's surface see consistent behaviour.
    /// - **`kind: github`**: kept. This is the tool's home — clone,
    ///   activate, update, delete.
    /// - **`kind: local`**: dropped. Every action `repo_management`
    ///   offers is a GitHub-workspace operation against a clone
    ///   directory a local workspace does not have; the entry point
    ///   there is `set_root_dir`, registered by
    ///   [`register_local_workspace_tools`](Self::register_local_workspace_tools).
    ///   Dropping the route also lets the bundled `repo_management`
    ///   skill's `applies_when: tool_registered:` gate suppress its
    ///   prompt in local mode.
    fn gate_workspace_tools(&mut self) {
        let kind = self.options.workspace.as_ref().map(|ws| ws.kind());
        if !matches!(kind, Some(crate::server::workspace::WorkspaceKind::Github)) {
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
             Pass `revs` (an integer N, or a list of git revspecs) to load multiple \
             revisions of the root into one graph — N loads the newest N stable \
             release tags of the dominant tag family plus HEAD (prereleases and \
             unrelated tag families skipped); requires the root to be a git repo. \
             Inventory persists across swaps; SHA-gating skips rebuilds when \
             the same root is re-bound with no content changes.",
            move |args: SetRootDirArgs| {
                let p = std::path::PathBuf::from(&args.path);
                ws.set_root_dir(&p, args.revs.as_ref())
            },
        );
    }

    /// Register `github_issues` + `github_api` (+ `screen_stargazers`)
    /// as dynamic tools, behind two gates in this order:
    ///
    /// 1. **Manifest opt-in** — `builtins.github: true`. Default off, so
    ///    a server that never asked for GitHub tooling never grows it.
    ///    A reachable token is not an intent: `GITHUB_TOKEN` in the
    ///    environment, or one the `.env` walk-up finds several
    ///    directories above the server's root, used to be enough to add
    ///    three authenticated GitHub tools to an unrelated server.
    /// 2. **Token reachability** — with the opt-in set, the tools still
    ///    only register when a token is actually reachable. That is
    ///    honest tool listing: agents see the tool only if it can
    ///    succeed.
    ///
    /// Both decisions are boot-time; restart the server to pick up a
    /// token (or a manifest change) that appears later.
    fn register_github_tools_if_authorized(&mut self) {
        if !self.options.builtins.github {
            // The normal case now — keep it at debug so an ordinary
            // non-GitHub server doesn't log about a feature it never
            // asked for.
            tracing::debug!(
                "GitHub tools disabled (default) — set `builtins.github: true` in the manifest \
                 to register github_issues / github_api / screen_stargazers."
            );
            return;
        }
        if !crate::github::has_git_token() {
            tracing::info!(
                "`builtins.github: true` is set but no GitHub token is reachable — \
                 github_issues / github_api tools hidden from the agent. Set GITHUB_TOKEN \
                 (env or the manifest's env_file) and restart to enable them."
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
        let repo_provider = default_repo.clone();
        let repo_for_screen = default_repo;
        self.register_typed_tool::<GithubApiArgs, _>(
            "github_api",
            "Read-only GET against the GitHub REST API. `path` may be a \
             repo-relative endpoint (\"pulls?state=open\", \"commits/abc123\", \
             \"branches\", \"compare/main...feature\") which is auto-prefixed \
             with /repos/<repo_name>/, or a top-level resource (\"search/issues?q=...\", \
             \"users/octocat\", \"repos/owner/name\") which passes through. A \
             leading slash is optional and accepted on either form. Returns \
             JSON, truncated at 80 KB by default.",
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

        // screen_stargazers — bulk-screen a repo's stargazers over cheap
        // REST into a per-server store, return a compact cohort+relevance
        // overview, and let the agent drill via `element_id` (cache hits;
        // only `.../readme` costs a request). The store is the stargazer
        // analogue of `github_issues`' ElementCache. Registered here, so
        // it inherits both gates above (`builtins.github: true` + a
        // reachable token). Within an opted-in deployment operators can
        // drop just this tool (keeping the other GitHub tools) via
        // `builtins.screen_stargazers: false`; default on.
        if self.options.builtins.screen_stargazers {
            let screen_store: Arc<Mutex<crate::screen::ScreenStore>> =
                Arc::new(Mutex::new(crate::screen::ScreenStore::new()));
            self.register_typed_tool::<ScreenStargazersArgs, _>(
                "screen_stargazers",
                "Screen the people around a GitHub project to find relevant developers, \
             notable/legendary devs, architectural peers, and actual users — cheaply. \
             Seed on a repo (`repo=\"owner/repo\"` → screens its stargazers) OR an \
             explicit user list (`users=\"alice,bob\"` → screens them directly). With \
             just a repo it auto-derives relevance keywords + tech stack from the repo \
             itself, bulk-fetches each person's public repo portfolio over plain REST \
             (~1 request per person, no GraphQL, no READMEs), classifies them, and \
             enriches a bounded shortlist with follower counts, dependency-adoption, \
             stack co-location, and contributions. Every person gets a normalized \
             0–100 score vector on four axes — relatedness, popularity, effort, \
             recency. RANK/FILTER: pass a `preset` (\"outreach\"=relevant+active by \
             reach, \"peers\"=your stack by effort, \"legends\"=biggest reach any \
             domain, \"intel\"=on-domain by popularity, \"adopters\"=actual users), or \
             `rank_by`=relatedness|popularity|effort|recency with filters \
             (`min_keywords`, `active_since`, `adopters_only`, `stack_only`) and \
             `top`=N (rank-then-take-N, default 10) for a focused filter→rank→take \
             view; with none, the full multi-lens browse: \
             `✅ ADOPTERS` (stargazers whose repos actually declare your package as a \
             dependency — real users, not just watchers), `★ MOST RELEVANT` \
             (relatedness — repos matching your topic keywords, with follower counts \
             and external contributions), `🏆 NOTABLE` (popularity/reach lens — your \
             highest-traction stargazers, flagged `LEGEND` for big audiences/projects), \
             `✦ QUALITY` (best-kept maintained projects), `⚙ STACK MATCH` (architectural \
             peers who build in your stack — co-location-confirmed where possible), and \
             a cohort inventory. Override the auto-config with `keywords=\"graph,rag,agent\"` \
             (single words — \"knowledge,graph\" not \"knowledge-graph\") and \
             `stack=\"Rust,Python\"`; re-calling with new values re-ranks the cached \
             fetch for free. Treat description-based leads as candidates to verify by \
             drilling. DRILL via `element_id`: `\"cohort:<key>\"` (established / single / \
             prolific / casual / dormant / consumers — the overview lists each key), \
             `\"user:<login>\"` (portfolio), `\"user:<login>/repo:<name>\"` (repo profile), \
             or `\"user:<login>/repo:<name>/readme\"` (README gist — the only drill that \
             costs a request). `max_stargazers` samples the most-recent N (the overview \
             reports if results are partial); `refresh=true` re-fetches.",
                move |args: ScreenStargazersArgs| {
                    use crate::screen::{self, Filters, RankBy, Seed, Selection};
                    let split_csv = |s: Option<String>| -> Vec<String> {
                        s.map(|v| {
                            v.split(',')
                                .map(|t| t.trim().to_string())
                                .filter(|t| !t.is_empty())
                                .collect()
                        })
                        .unwrap_or_default()
                    };
                    // Seed: explicit user list wins; else the repo (or active repo).
                    let seed = if let Some(u) = &args.users {
                        Seed::Users(split_csv(Some(u.clone())))
                    } else {
                        let repo =
                            match resolve_repo_from(repo_for_screen.as_ref(), args.repo.clone()) {
                                Ok(r) => r,
                                Err(msg) => return msg,
                            };
                        if let Some(err) = crate::git_refs::validate_repo(&repo) {
                            return err;
                        }
                        Seed::Repo(repo)
                    };
                    let cfg = screen::ScreenConfig {
                        max_stargazers: args.max_stargazers,
                        max_repos_per_user: 100,
                        relevance_keywords: split_csv(args.keywords)
                            .into_iter()
                            .map(|k| k.to_lowercase())
                            .collect(),
                        stack_languages: split_csv(args.stack),
                    };
                    // Selection: preset, else explicit rank/filters, else none.
                    let top = args.top.unwrap_or(10);
                    let filters = Filters {
                        min_keywords: args.min_keywords,
                        active_since: args.active_since.clone(),
                        adopters_only: args.adopters_only,
                        stack_only: args.stack_only,
                        ..Default::default()
                    };
                    let filters_active = filters.min_keywords.is_some()
                        || filters.active_since.is_some()
                        || filters.adopters_only
                        || filters.stack_only;
                    let selection: Option<Selection> = if let Some(name) = &args.preset {
                        screen::preset(name, top)
                    } else if args.rank_by.is_some() || filters_active {
                        Some(Selection {
                            filters,
                            rank: args
                                .rank_by
                                .as_deref()
                                .and_then(RankBy::parse)
                                .unwrap_or(RankBy::Relatedness),
                            label: "SELECTION".into(),
                            take: top,
                        })
                    } else {
                        None
                    };
                    screen::screen_dispatch(
                        &screen_store,
                        &seed,
                        &cfg,
                        selection.as_ref(),
                        args.element_id.as_deref(),
                        args.refresh,
                    )
                },
            );
        }
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
    /// registration. Same lifecycle contract as
    /// [`tool_router_mut`](Self::tool_router_mut):
    /// boot-time only. Most operators reach prompts via
    /// [`serve_prompts`] rather than touching the router directly.
    pub fn prompt_router_mut(&mut self) -> &mut PromptRouter<McpServer> {
        &mut self.prompt_router
    }

    /// Register a typed dynamic tool with an infallible handler.
    /// Compresses the boilerplate of:
    /// 1. Generating a JSON Schema for the args type via `schemars`.
    /// 2. Building a [`rmcp::model::Tool`] attr from the schema +
    ///    name + description.
    /// 3. Deserialising the per-call JSON arguments via serde.
    /// 4. Wrapping the handler in a [`rmcp::handler::server::router::tool::ToolRoute::new_dyn`]
    ///    closure suitable for [`tool_router_mut`](Self::tool_router_mut).
    ///
    /// The handler is `Fn(T) -> String`; it owns whatever state it
    /// needs through the closure environment (typically an Arc-clone
    /// of a domain-specific state handle). A `String` is the only
    /// outcome the *handler* can produce, so every call that reaches
    /// it reports a success envelope (`isError: false`). That makes
    /// this the entry point for tools that genuinely cannot fail, and
    /// for tools that
    /// deliberately render their own failures as ordinary prose the
    /// agent reads and moves on from — the "errors as values" shape
    /// the source / GitHub builtins use.
    ///
    /// A tool whose failure the *client* should be able to branch on
    /// wants [`register_typed_tool_fallible`](Self::register_typed_tool_fallible)
    /// instead: it takes `Fn(T) -> Result<String, String>` and routes
    /// the `Err` body through the MCP error envelope, so a caller sees
    /// `isError: true` rather than having to pattern-match the text.
    ///
    /// Arguments that fail to deserialise are an error envelope on
    /// either method — a call the framework could not even hand to
    /// the handler is not a result the agent should read as one.
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
        // The fallible route is the general case; an infallible
        // handler is just one that never takes the `Err` arm.
        self.register_typed_route(name, description, move |args: T| Ok(handler(args)));
    }

    /// Register a typed dynamic tool whose handler can fail.
    ///
    /// Same shape as [`register_typed_tool`](Self::register_typed_tool)
    /// — same schema generation, same argument deserialisation, same
    /// dyn route — except the handler is
    /// `Fn(T) -> Result<String, String>`. `Ok(body)` produces the
    /// usual success envelope; `Err(body)` produces an MCP error
    /// envelope (`isError: true`) carrying the error text verbatim.
    /// That string is what the agent reads, so write it for that
    /// reader rather than dumping a `Debug` of some internal type
    /// into it.
    ///
    /// The consumer's [`ResultPostprocessHook`] runs on **both** arms,
    /// with the same [`ResultCtx`], and its footer is appended to the
    /// error text exactly as it is to a success body. A downstream
    /// server that stamps identity or rebuild state onto every result
    /// keeps that stamp on the failure path, where an unplaceable
    /// error would otherwise send the agent hunting in the wrong
    /// graph.
    pub fn register_typed_tool_fallible<T, F>(
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
        F: Fn(T) -> Result<String, String> + Send + Sync + 'static,
    {
        self.register_typed_route(name, description, handler);
    }

    /// The registration half both public typed-tool methods share:
    /// build the schema + attr, capture the postprocess plumbing, and
    /// install one dyn route that defers every per-call decision to
    /// [`dispatch_typed_call`].
    fn register_typed_route<T, F>(
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
        F: Fn(T) -> Result<String, String> + Send + Sync + 'static,
    {
        use std::pin::Pin;
        type DynFut<'a, R> = Pin<Box<dyn std::future::Future<Output = R> + Send + 'a>>;

        let schema_obj = serde_json::to_value(schemars::schema_for!(T))
            .ok()
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        let attr = rmcp::model::Tool::new(name, description, Arc::new(schema_obj));
        let handler = std::sync::Arc::new(handler);
        // Capture the result-postprocess plumbing: the dyn closure has
        // no `&self`, so the hook and the state needed to build a
        // `ResultCtx` are cloned in here (Arc-cheap). `tool_name` is a
        // `&'static str`, Copy into the closure.
        let tool_name = name;
        let postprocess = self.options.result_postprocess.clone();
        let source_roots = self.options.source_roots.clone();
        let workspace = self.options.workspace.clone();

        self.tool_router
            .add_route(rmcp::handler::server::router::tool::ToolRoute::new_dyn(
                attr,
                move |ctx: rmcp::handler::server::tool::ToolCallContext<'_, McpServer>|
                    -> DynFut<'_, Result<rmcp::model::CallToolResponse, rmcp::ErrorData>> {
                    let handler = handler.clone();
                    let arguments = ctx.arguments.clone();
                    let postprocess = postprocess.clone();
                    let source_roots = source_roots.clone();
                    let workspace = workspace.clone();
                    Box::pin(async move {
                        Ok(dispatch_typed_call(
                            tool_name,
                            arguments,
                            handler.as_ref(),
                            postprocess.as_ref(),
                            source_roots.as_ref(),
                            workspace.as_ref(),
                        )
                        .into())
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

    /// The body `read_source` / `grep` / `list_source` return when no
    /// source root is active. `lead` is the per-tool opener (e.g.
    /// `"Cannot read source"`).
    ///
    /// "Configure `source_root:`" is the right advice only when nobody
    /// configured one. When the boot *did* find a declaration and could
    /// not resolve it, that advice sends the operator to re-do the thing
    /// they already did — the real cause (a directory that moved, or a
    /// manifest copied away from its tree) is otherwise visible only on
    /// stderr. So append one line per declared-but-unresolved root,
    /// naming it and the path it was looked for at.
    fn no_source_root_message(&self, lead: &str) -> String {
        let mut msg = format!(
            "{lead}: no active source root. Configure source_root in your manifest or \
             activate one (e.g. via repo_management in workspace mode)."
        );
        for note in &self.options.unresolved_source_roots {
            msg.push('\n');
            msg.push_str(note);
        }
        msg
    }

    /// Run the consumer's result-postprocess hook (if any) against a
    /// builtin tool's text `body`, appending any returned footer. The
    /// single application point for the static `#[tool]` methods; the
    /// dynamic `register_typed_tool` path applies the same contract at
    /// its own choke point via captured clones (the closure has no
    /// `&self`).
    fn finish(&self, tool: &str, args: &serde_json::Value, body: String) -> String {
        let Some(hook) = &self.options.result_postprocess else {
            return body;
        };
        let ctx = ResultCtx {
            source_roots: self.current_source_roots(),
            active_repo: self
                .options
                .workspace
                .as_ref()
                .and_then(|w| w.active_repo_name()),
        };
        let footer = hook(tool, args, &body, &ctx);
        append_footer(body, footer)
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
        let args_json = serde_json::to_value(&args).unwrap_or(serde_json::Value::Null);
        let body = args.message.unwrap_or_else(|| "pong".to_string());
        let body = self.finish("ping", &args_json, body);
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(description = "Read a file from the configured source root(s). Pass \
                       `start_line`/`end_line` to slice, `grep` to filter to matching \
                       lines, `max_chars` to cap output. Pass `rev` (a tag, branch, or \
                       commit SHA) to read the file's content at that git revision via \
                       `git show` instead of the working tree — useful for comparing a \
                       file across releases (requires a git repo source root). Path \
                       traversal attempts are rejected. Available only when source roots \
                       are configured.")]
    async fn read_source(
        &self,
        Parameters(args): Parameters<ReadSourceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let roots = self.current_source_roots();
        if roots.is_empty() {
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                self.no_source_root_message("Cannot read source"),
            )]));
        }
        let args_json = serde_json::to_value(&args).unwrap_or(serde_json::Value::Null);
        let opts = ReadOpts {
            start_line: args.start_line,
            end_line: args.end_line,
            grep: args.grep,
            grep_context: args.grep_context,
            max_matches: args.max_matches,
            max_chars: args.max_chars,
            rev: args.rev,
        };
        let body = source::read_source(&args.file_path, &roots, &opts);
        let body = self.finish("read_source", &args_json, body);
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
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
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                self.no_source_root_message("Cannot grep"),
            )]));
        }
        let args_json = serde_json::to_value(&args).unwrap_or(serde_json::Value::Null);
        let opts = GrepOpts {
            glob: args.glob,
            context: args.context,
            max_results: Some(args.max_results.unwrap_or(50)),
            case_insensitive: args.case_insensitive,
        };
        let body = source::grep(&roots, &args.pattern, &opts);
        let body = self.finish("grep", &args_json, body);
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
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
            return Ok(CallToolResult::success(vec![ContentBlock::text(
                self.no_source_root_message("Cannot list source"),
            )]));
        }
        let primary = std::path::PathBuf::from(&roots[0]);
        let target = match resolve_dir_under_roots(&args.path, &roots) {
            Some(p) => p,
            None => {
                return Ok(CallToolResult::success(vec![ContentBlock::text(format!(
                    "Error: path '{}' resolves outside the configured source roots.",
                    args.path
                ))]));
            }
        };
        let args_json = serde_json::to_value(&args).unwrap_or(serde_json::Value::Null);
        let opts = ListOpts {
            depth: args.depth,
            glob: args.glob,
            dirs_only: args.dirs_only,
        };
        let body = source::list_source(&target, &primary, &opts);
        let body = self.finish("list_source", &args_json, body);
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
    }

    #[tool(
        description = "Manage GitHub repos in the workspace. Pass `name='org/repo'` to \
                       clone (if missing) and activate it as the source root for \
                       read_source / grep / list_source. Pass `delete=true` to remove a \
                       repo. Pass `update=true` to fetch upstream changes for the active \
                       repo (rebuild auto-skipped when HEAD hasn't moved since the last \
                       build; set `force_rebuild=true` to bypass). Pass `revs` (an \
                       integer N, or a list of git revspecs) to load multiple revisions \
                       of the repo into one graph — N loads the newest N stable release \
                       tags of the dominant tag family plus HEAD (prereleases and \
                       unrelated tag families skipped); a revs request always rebuilds. \
                       Call with no \
                       arguments to list all known repos with their last-access counts. \
                       Idle repos auto-sweep on each call (default 7 days, configurable \
                       via --stale-after-days)."
    )]
    async fn repo_management(
        &self,
        Parameters(args): Parameters<RepoManagementArgs>,
    ) -> Result<CallToolResult, McpError> {
        let args_json = serde_json::to_value(&args).unwrap_or(serde_json::Value::Null);
        let body = match &self.options.workspace {
            Some(ws) => ws.repo_management(
                args.name.as_deref(),
                args.delete,
                args.update,
                args.force_rebuild,
                args.revs.as_ref(),
            ),
            None => "repo_management requires --workspace mode.".to_string(),
        };
        let body = self.finish("repo_management", &args_json, body);
        Ok(CallToolResult::success(vec![ContentBlock::text(body)]))
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

    // For the auto-inject pass: skills with `auto_inject_hint` get
    // their `description` (routing) and `body` (methodology) embedded
    // into the descriptions of their name-match tool AND every tool
    // they list in `references_tools`. See the comment at the bottom
    // of the function for why this is the content, not a pointer.
    struct InjectSkill {
        name: String,
        description: String,
        body: String,
        references_tools: Vec<String>,
    }
    let mut auto_inject: Vec<InjectSkill> = Vec::new();

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
                Ok(
                    GetPromptResult::new(vec![PromptMessage::new_text(Role::Assistant, body)])
                        .into(),
                )
            })
        });
        server.prompt_router.add_route(route);

        if skill.frontmatter.auto_inject_hint {
            auto_inject.push(InjectSkill {
                name: skill.name().to_string(),
                description: skill.description().to_string(),
                body: skill.body.clone(),
                references_tools: skill.frontmatter.references_tools.clone(),
            });
        }
    }

    // Auto-inject the skill's routing + methodology into tool
    // descriptions.
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
    // The fix, in two parts:
    //   * Embed the skill's `description` under a `## When to use`
    //     header and its `body` under `## Methodology`. The
    //     description carries the TRIGGER/SKIP routing — small by
    //     design, so it leads and isn't subject to the body's size
    //     caps (4 KB soft / 16 KB hard, enforced at load). An empty
    //     description omits the `## When to use` block.
    //   * Inject into the skill's name-match tool AND every tool it
    //     lists in `references_tools`. This is the only way to express
    //     a *cross-tool* skill — one not named after any single tool.
    //
    // A tool may now carry several skills (its own plus any that
    // reference it). Each injection is fenced by a per-skill marker
    // (`<!-- mcp-skill:<name> -->`) so the pass stays idempotent per
    // (skill, tool) pair: a tool that is both the name-match and a
    // `references_tools` entry of the same skill gets one injection,
    // and re-running the pass never double-appends.
    //
    // Operators who want the smaller pointer-only behaviour set
    // `auto_inject_hint: false` per skill. `prompts/list` /
    // `prompts/get` continue to work for any client that does surface
    // them to the agent, plus CLI introspection. This pass just makes
    // the *primary* delivery channel a place agents actually look.
    for inj in &auto_inject {
        // The skill's name-match tool plus every tool it references,
        // deduped so a self-reference doesn't queue the same tool twice.
        let mut targets: Vec<&str> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        for tool in std::iter::once(inj.name.as_str())
            .chain(inj.references_tools.iter().map(String::as_str))
        {
            if seen.insert(tool) {
                targets.push(tool);
            }
        }

        // Build the injected block once. Marker first (idempotency
        // fence), then the routing, then the methodology body.
        let marker = format!("<!-- mcp-skill:{} -->", inj.name);
        let mut block = format!("\n\n{marker}");
        let description = inj.description.trim();
        if !description.is_empty() {
            block.push_str("\n\n## When to use\n\n");
            block.push_str(description);
        }
        block.push_str("\n\n## Methodology\n\n");
        block.push_str(inj.body.trim());

        for tool in targets {
            let key = Cow::<'static, str>::Owned(tool.to_string());
            let Some(route) = server.tool_router.map.get_mut(&key) else {
                continue;
            };
            // Per-skill idempotency: never inject the same skill twice
            // into one tool's description.
            if route
                .attr
                .description
                .as_deref()
                .is_some_and(|d| d.contains(&marker))
            {
                continue;
            }
            let new_desc = match route.attr.description.take() {
                Some(existing) => format!("{existing}{block}"),
                None => block.trim_start().to_string(),
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

    /// `notifications/initialized` — the one point at which a
    /// client-advertised root can be adopted (see [`crate::server::roots`]).
    ///
    /// rmcp dispatches every peer notification on a task it spawns
    /// (`spawn_service_task`, which is `tokio::spawn` unless rmcp's `local`
    /// feature is enabled), and the response router lives in the same select
    /// loop, so awaiting a server→client `roots/list` request here cannot
    /// deadlock and cannot delay the client's session. **If rmcp's `local`
    /// feature is ever enabled that becomes `spawn_local` and this reasoning
    /// must be re-checked.**
    ///
    /// Everything about adoption is opt-in and guarded inside the `roots`
    /// module: with no `workspace.adopt_client_roots` this returns after two
    /// field reads, having sent nothing.
    async fn on_initialized(&self, context: rmcp::service::NotificationContext<rmcp::RoleServer>) {
        // Same line rmcp's default handler emits — overriding the method
        // must not cost an operator the log they have today.
        tracing::info!("client initialized");
        crate::server::roots::on_client_initialized(&self.options, &context.peer).await;
    }

    /// `notifications/roots/list_changed` — re-run adoption, unless the
    /// operator has claimed the root in the meantime.
    async fn on_roots_list_changed(
        &self,
        context: rmcp::service::NotificationContext<rmcp::RoleServer>,
    ) {
        crate::server::roots::on_client_roots_changed(&self.options, &context.peer).await;
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListPromptsResult, McpError> {
        Ok(ListPromptsResult {
            prompts: self.prompt_router.list_all(),
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<GetPromptResponse, McpError> {
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
                ..Default::default()
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

    /// With nothing declared, the advice to configure a root is the
    /// whole story and must stay exactly that.
    #[test]
    fn no_root_message_without_declarations_is_the_configure_advice() {
        let server = McpServer::new(ServerOptions::default());
        let msg = server.no_source_root_message("Cannot read source");
        assert_eq!(
            msg,
            "Cannot read source: no active source root. Configure source_root in your \
             manifest or activate one (e.g. via repo_management in workspace mode)."
        );
    }

    /// The operator's repro: `source_root: source` *is* configured, the
    /// directory is not there. Telling them to configure `source_root:`
    /// is a misdirection, so all three source tools must also name the
    /// declared root and the path it was looked for at.
    #[test]
    fn no_root_message_names_a_declared_root_that_did_not_resolve() {
        let opts = ServerOptions::default().with_unresolved_source_roots(vec![(
            "source".to_string(),
            std::path::PathBuf::from("/nowhere/proj/source"),
        )]);
        let server = McpServer::new(opts);
        for lead in ["Cannot read source", "Cannot grep", "Cannot list source"] {
            let msg = server.no_source_root_message(lead);
            assert!(
                msg.contains("no active source root"),
                "{lead}: the unconfigured-case sentence must survive: {msg}"
            );
            assert!(
                msg.contains(r#"declared source root "source" did not resolve"#),
                "{lead}: the message must name the declared root: {msg}"
            );
            assert!(
                msg.contains("/nowhere/proj/source"),
                "{lead}: the message must name the path it was looked for at: {msg}"
            );
            assert!(
                msg.contains("is not an existing directory"),
                "{lead}: the message must say why it failed: {msg}"
            );
        }
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

    /// Build a server with `builtins.github` set as given, under a
    /// process env where the GitHub token is either present or absent,
    /// and return the resulting tool names. Restores the previous env
    /// before returning; the crate-wide `env_lock` serialises against
    /// the other env-mutating tests.
    fn github_tool_surface(github_opt_in: bool, token_present: bool) -> Vec<String> {
        use crate::server::manifest::BuiltinsConfig;
        let _g = crate::github::env_lock();
        let prev_token = std::env::var("GITHUB_TOKEN").ok();
        let prev_alt = std::env::var("GH_TOKEN").ok();
        unsafe {
            std::env::remove_var("GH_TOKEN");
            if token_present {
                std::env::set_var("GITHUB_TOKEN", "ghp_surface_test_not_real");
            } else {
                std::env::remove_var("GITHUB_TOKEN");
            }
        }
        let opts = ServerOptions {
            builtins: BuiltinsConfig {
                github: github_opt_in,
                ..Default::default()
            },
            ..ServerOptions::default()
        };
        let server = McpServer::new(opts);
        let names: Vec<String> = server
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        unsafe {
            match prev_token {
                Some(v) => std::env::set_var("GITHUB_TOKEN", v),
                None => std::env::remove_var("GITHUB_TOKEN"),
            }
            match prev_alt {
                Some(v) => std::env::set_var("GH_TOKEN", v),
                None => std::env::remove_var("GH_TOKEN"),
            }
        }
        names
    }

    const GITHUB_TOOLS: [&str; 3] = ["github_issues", "github_api", "screen_stargazers"];

    #[test]
    fn github_tools_absent_by_default_even_with_a_token() {
        // The security-critical case: an ambient credential (plain env
        // var, or one the `.env` walk-up found several directories up)
        // must not widen an unrelated server's tool surface.
        let names = github_tool_surface(false, true);
        for tool in GITHUB_TOOLS {
            assert!(
                !names.iter().any(|n| n == tool),
                "{tool} registered without `builtins.github: true`; tools were {names:?}"
            );
        }
    }

    #[test]
    fn github_tools_register_on_opt_in_with_a_token() {
        let names = github_tool_surface(true, true);
        for tool in GITHUB_TOOLS {
            assert!(
                names.iter().any(|n| n == tool),
                "{tool} missing with `builtins.github: true` and a token; tools were {names:?}"
            );
        }
    }

    #[test]
    fn github_tools_absent_on_opt_in_without_a_token() {
        // Opt-in declares intent; the token gate still decides whether
        // the tools can actually succeed, so they stay hidden.
        let names = github_tool_surface(true, false);
        for tool in GITHUB_TOOLS {
            assert!(
                !names.iter().any(|n| n == tool),
                "{tool} registered with no reachable token; tools were {names:?}"
            );
        }
    }

    #[test]
    fn repo_management_present_when_workspace_bound() {
        // With a github workspace handle bound, repo_management should
        // be registered.
        let (_dir, names) = tool_surface_for_workspace(WorkspaceFlavour::Github);
        assert!(
            names.iter().any(|n| n == "repo_management"),
            "repo_management should be registered with a github workspace; tools were {names:?}"
        );
    }

    enum WorkspaceFlavour {
        Github,
        Local,
    }

    /// Boot a server against a workspace of the given flavour and
    /// return (the tempdir keeping it alive, the tool names).
    fn tool_surface_for_workspace(flavour: WorkspaceFlavour) -> (tempfile::TempDir, Vec<String>) {
        use crate::server::workspace::Workspace;
        let dir = tempfile::tempdir().unwrap();
        let ws = match flavour {
            WorkspaceFlavour::Github => Workspace::open(dir.path().to_path_buf(), 7, None).unwrap(),
            WorkspaceFlavour::Local => {
                Workspace::open_local(dir.path().to_path_buf(), None).unwrap()
            }
        };
        let server = McpServer::new(ServerOptions::default().with_workspace(ws));
        let names = server
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        (dir, names)
    }

    #[test]
    fn local_workspace_drops_repo_management_and_keeps_set_root_dir() {
        // Every `repo_management` action is a GitHub-workspace
        // operation (clone / update / delete a tracked clone); a
        // `kind: local` workspace has none of that, so the tool must
        // not be advertised. `set_root_dir` is the local entry point
        // and must be there.
        let (_dir, names) = tool_surface_for_workspace(WorkspaceFlavour::Local);
        assert!(
            !names.iter().any(|n| n == "repo_management"),
            "repo_management must be gated out of a kind: local workspace; tools were {names:?}"
        );
        assert!(
            names.iter().any(|n| n == "set_root_dir"),
            "set_root_dir must stay registered in a kind: local workspace; tools were {names:?}"
        );
    }

    #[test]
    fn github_workspace_keeps_repo_management_and_has_no_set_root_dir() {
        // The github case must be untouched by the kind-aware gate.
        let (_dir, names) = tool_surface_for_workspace(WorkspaceFlavour::Github);
        assert!(
            names.iter().any(|n| n == "repo_management"),
            "repo_management must stay registered in a kind: github workspace; tools were {names:?}"
        );
        assert!(
            !names.iter().any(|n| n == "set_root_dir"),
            "set_root_dir is local-only; tools were {names:?}"
        );
    }

    #[test]
    fn no_workspace_has_neither_workspace_tool() {
        let server = McpServer::new(ServerOptions::default());
        let names: Vec<String> = server
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(
            !names.iter().any(|n| n == "repo_management")
                && !names.iter().any(|n| n == "set_root_dir"),
            "a workspace-less server must advertise neither workspace tool; tools were {names:?}"
        );
    }

    /// The bundled `repo_management` skill carries
    /// `applies_when: tool_registered: repo_management`. Now that the
    /// tool is gated out in local mode, the prompt must vanish with it
    /// — otherwise the agent reads clone/update methodology for a tool
    /// it cannot call.
    fn bundled_prompt_names(flavour: WorkspaceFlavour) -> (tempfile::TempDir, Vec<String>) {
        use crate::server::skills::Registry as SkillsBuilder;
        use crate::server::workspace::Workspace;
        let dir = tempfile::tempdir().unwrap();
        let ws = match flavour {
            WorkspaceFlavour::Github => Workspace::open(dir.path().to_path_buf(), 7, None).unwrap(),
            WorkspaceFlavour::Local => {
                Workspace::open_local(dir.path().to_path_buf(), None).unwrap()
            }
        };
        let registry = SkillsBuilder::new()
            .merge_framework_defaults()
            .layer_dirs(
                &crate::server::manifest::SkillsSource::Sources(vec![
                    crate::server::manifest::SkillSource::Bundled,
                ]),
                &dir.path().join("test_mcp.yaml"),
            )
            .unwrap()
            .finalise()
            .unwrap();
        let mut server = McpServer::new(ServerOptions::default().with_workspace(ws));
        super::serve_prompts(&registry, &mut server);
        let names = server
            .prompt_router
            .map
            .keys()
            .map(|k| k.to_string())
            .collect();
        (dir, names)
    }

    #[test]
    fn bundled_repo_management_skill_suppressed_in_local_mode() {
        let (_dir, local) = bundled_prompt_names(WorkspaceFlavour::Local);
        assert!(
            !local.iter().any(|n| n == "repo_management"),
            "the bundled repo_management skill must be gated out with its tool; prompts were \
             {local:?}"
        );
        let (_dir2, github) = bundled_prompt_names(WorkspaceFlavour::Github);
        assert!(
            github.iter().any(|n| n == "repo_management"),
            "the bundled repo_management skill must still surface with a github workspace; \
             prompts were {github:?}"
        );
    }

    #[test]
    fn result_postprocess_appends_footer_and_sees_ctx() {
        use std::sync::Mutex;
        // Capture what the hook receives so we can assert the ctx.
        type Seen = Option<(String, serde_json::Value, String, Vec<String>)>;
        let seen: Arc<Mutex<Seen>> = Arc::new(Mutex::new(None));
        let seen_c = seen.clone();
        let hook: ResultPostprocessHook = Arc::new(move |tool, args, body, ctx| {
            *seen_c.lock().unwrap() = Some((
                tool.to_string(),
                args.clone(),
                body.to_string(),
                ctx.source_roots.clone(),
            ));
            // Only steer on grep — proves per-tool selectivity.
            if tool == "grep" {
                Some("↳ prefer cypher_query".to_string())
            } else {
                None
            }
        });
        let opts = ServerOptions::default()
            .with_static_source_roots(vec!["/src".to_string()])
            .with_result_postprocess(hook);
        let server = McpServer::new(opts);

        let args = serde_json::json!({ "pattern": "^fn " });
        let out = server.finish("grep", &args, "match line".to_string());
        assert_eq!(out, "match line\n\n↳ prefer cypher_query");

        let rec = seen.lock().unwrap().clone().unwrap();
        assert_eq!(rec.0, "grep");
        assert_eq!(rec.1, args);
        assert_eq!(rec.2, "match line");
        assert_eq!(rec.3, vec!["/src".to_string()]);

        // A tool the hook ignores → body byte-for-byte unchanged.
        let out2 = server.finish("read_source", &args, "file body".to_string());
        assert_eq!(out2, "file body");
    }

    #[test]
    fn no_result_postprocess_leaves_body_unchanged() {
        let server = McpServer::new(ServerOptions::default());
        let out = server.finish("grep", &serde_json::Value::Null, "x".to_string());
        assert_eq!(out, "x");
    }

    #[test]
    fn append_footer_ignores_empty_footers() {
        assert_eq!(append_footer("a".to_string(), None), "a");
        assert_eq!(append_footer("a".to_string(), Some(String::new())), "a");
        assert_eq!(
            append_footer("a".to_string(), Some("b".to_string())),
            "a\n\nb"
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

    // ─── Typed dynamic tools ──────────────────────────────────────

    /// Args type for the typed-tool tests. `count` is the typed field
    /// an invalid-arguments call feeds a string to.
    #[derive(Default, serde::Deserialize, schemars::JsonSchema)]
    struct EchoArgs {
        #[serde(default)]
        text: String,
        #[serde(default)]
        count: u32,
    }

    /// Concatenate the text blocks of a dispatch result. Every typed
    /// tool emits exactly one, but joining keeps the assertion honest
    /// if that ever changes.
    fn result_text(result: &CallToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("")
    }

    /// A hook that footers every tool unconditionally, so the tests
    /// can assert the footer's presence per arm rather than per tool.
    fn footer_hook() -> ResultPostprocessHook {
        Arc::new(|_tool, _args, _body, _ctx| Some("↳ footer".to_string()))
    }

    fn args_map(json: serde_json::Value) -> Option<rmcp::model::JsonObject> {
        json.as_object().cloned()
    }

    #[test]
    fn fallible_ok_reports_success_with_footer() {
        let hook = footer_hook();
        let out = dispatch_typed_call(
            "echo",
            args_map(serde_json::json!({ "text": "hi", "count": 2 })),
            &|args: EchoArgs| Ok(format!("{} x{}", args.text, args.count)),
            Some(&hook),
            None,
            None,
        );
        assert_eq!(out.is_error, Some(false));
        assert_eq!(result_text(&out), "hi x2\n\n↳ footer");
    }

    #[test]
    fn fallible_err_sets_is_error_and_keeps_footer() {
        // kglite's requirement: the postprocess hook runs on the error
        // arm too, so identity footers survive a failed call.
        let hook = footer_hook();
        let out = dispatch_typed_call(
            "echo",
            args_map(serde_json::json!({ "text": "hi" })),
            &|_args: EchoArgs| Err::<String, String>("no rows matched".to_string()),
            Some(&hook),
            None,
            None,
        );
        assert_eq!(out.is_error, Some(true));
        assert_eq!(result_text(&out), "no rows matched\n\n↳ footer");
    }

    #[test]
    fn fallible_err_without_hook_is_error_text_verbatim() {
        let out = dispatch_typed_call(
            "echo",
            args_map(serde_json::json!({})),
            &|_args: EchoArgs| Err::<String, String>("boom".to_string()),
            None,
            None,
            None,
        );
        assert_eq!(out.is_error, Some(true));
        assert_eq!(result_text(&out), "boom");
    }

    #[test]
    fn postprocess_ctx_reaches_both_arms() {
        use std::sync::Mutex;
        // (body, ctx.source_roots) per hook invocation.
        type Seen = Arc<Mutex<Vec<(String, Vec<String>)>>>;
        let seen: Seen = Arc::new(Mutex::new(Vec::new()));
        let seen_c = seen.clone();
        let hook: ResultPostprocessHook = Arc::new(move |_tool, _args, body, ctx| {
            seen_c
                .lock()
                .unwrap()
                .push((body.to_string(), ctx.source_roots.clone()));
            None
        });
        let roots: SourceRootsProvider = Arc::new(|| vec!["/src".to_string()]);
        for handler_result in ["ok", "err"] {
            let _ = dispatch_typed_call(
                "echo",
                args_map(serde_json::json!({})),
                &|_args: EchoArgs| {
                    if handler_result == "ok" {
                        Ok("body".to_string())
                    } else {
                        Err("failed".to_string())
                    }
                },
                Some(&hook),
                Some(&roots),
                None,
            );
        }
        let rec = seen.lock().unwrap().clone();
        assert_eq!(rec.len(), 2, "hook must run on both arms");
        assert_eq!(rec[0].0, "body");
        assert_eq!(rec[1].0, "failed");
        for (_, roots) in &rec {
            assert_eq!(roots, &vec!["/src".to_string()]);
        }
    }

    #[test]
    fn invalid_arguments_set_is_error_on_both_registrations() {
        // `count` is a u32; a string can't deserialise into it. The
        // handler never runs, so the arm is identical for a fallible
        // handler and for the infallible one `register_typed_tool`
        // wraps into `Ok(...)`.
        let bad = || args_map(serde_json::json!({ "count": "not a number" }));

        let fallible = dispatch_typed_call(
            "echo",
            bad(),
            &|_args: EchoArgs| Ok("unreachable".to_string()),
            None,
            None,
            None,
        );
        assert_eq!(fallible.is_error, Some(true));
        assert!(
            result_text(&fallible).starts_with("invalid arguments: "),
            "got {:?}",
            result_text(&fallible)
        );

        // Exactly the wrapping `register_typed_tool` applies.
        let plain_handler = |_args: EchoArgs| "unreachable".to_string();
        let plain = dispatch_typed_call(
            "echo",
            bad(),
            &move |args: EchoArgs| Ok(plain_handler(args)),
            None,
            None,
            None,
        );
        assert_eq!(plain.is_error, Some(true));
        assert!(result_text(&plain).starts_with("invalid arguments: "));
    }

    #[test]
    fn invalid_arguments_still_get_the_footer() {
        let hook = footer_hook();
        let out = dispatch_typed_call(
            "echo",
            args_map(serde_json::json!({ "count": "not a number" })),
            &|_args: EchoArgs| Ok("unreachable".to_string()),
            Some(&hook),
            None,
            None,
        );
        assert_eq!(out.is_error, Some(true));
        assert!(result_text(&out).ends_with("\n\n↳ footer"));
    }

    #[test]
    fn plain_handler_success_unchanged() {
        // The pre-existing contract: an infallible handler's body,
        // footered, in a success envelope.
        let hook = footer_hook();
        let plain_handler = |args: EchoArgs| format!("said {}", args.text);
        let out = dispatch_typed_call(
            "echo",
            args_map(serde_json::json!({ "text": "hello" })),
            &move |args: EchoArgs| Ok(plain_handler(args)),
            Some(&hook),
            None,
            None,
        );
        assert_eq!(out.is_error, Some(false));
        assert_eq!(result_text(&out), "said hello\n\n↳ footer");
    }

    #[test]
    fn missing_arguments_fall_back_to_default_args() {
        // No `arguments` at all — `T::default()`, handler still runs.
        let out = dispatch_typed_call(
            "echo",
            None,
            &|args: EchoArgs| Ok(format!("[{}]", args.text)),
            None,
            None,
            None,
        );
        assert_eq!(out.is_error, Some(false));
        assert_eq!(result_text(&out), "[]");
    }

    #[test]
    fn both_registrations_reach_the_router() {
        let mut server = McpServer::new(ServerOptions::default());
        server.register_typed_tool("echo_plain", "plain", |args: EchoArgs| args.text);
        server.register_typed_tool_fallible("echo_fallible", "fallible", |args: EchoArgs| {
            if args.text.is_empty() {
                Err("text is required".to_string())
            } else {
                Ok(args.text)
            }
        });
        let names: Vec<String> = server
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        assert!(names.iter().any(|n| n == "echo_plain"), "{names:?}");
        assert!(names.iter().any(|n| n == "echo_fallible"), "{names:?}");
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

    /// Like [`build_test_registry`] but lets each skill declare a
    /// `references_tools` list (a YAML inline array, e.g. `[ping]`) so
    /// the cross-tool injection path can be exercised. Every skill is
    /// `auto_inject_hint: true`.
    fn build_registry_with_refs(
        skills: &[(&str, &str, &str, &str)],
    ) -> crate::server::skills::ResolvedRegistry {
        use crate::server::skills::Registry;
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("manifest.yaml");
        let skills_dir = dir.path().join("manifest.skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        for (name, description, body, references_tools) in skills {
            let content = format!(
                "---\nname: {name}\ndescription: {description}\n\
                 auto_inject_hint: true\nreferences_tools: {references_tools}\n---\n\n{body}\n"
            );
            std::fs::write(skills_dir.join(format!("{name}.md")), content).unwrap();
        }
        Registry::new()
            .auto_detect_project_layer(&yaml_path)
            .finalise()
            .unwrap()
    }

    fn tool_desc(server: &McpServer, tool: &str) -> String {
        server
            .tool_router
            .get(tool)
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default()
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

    #[test]
    fn serve_prompts_injects_description_under_when_to_use() {
        // The skill's `description` carries the TRIGGER/SKIP routing —
        // it must reach the live tool-description channel under a
        // `## When to use` header, ahead of the methodology body.
        let registry = build_test_registry(&[("ping", "ROUTING-SENTINEL", "BODY-SENTINEL", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let desc = tool_desc(&server, "ping");
        assert!(
            desc.contains("## When to use\n\nROUTING-SENTINEL"),
            "description should be injected under `## When to use`; got: {desc}"
        );
        assert!(
            desc.contains("<!-- mcp-skill:ping -->"),
            "injection should carry the per-skill idempotency marker; got: {desc}"
        );
        // Routing leads, methodology follows.
        let when = desc.find("## When to use").unwrap();
        let method = desc.find("## Methodology").unwrap();
        assert!(when < method, "`When to use` must precede `Methodology`");
    }

    #[test]
    fn serve_prompts_honors_references_tools() {
        // A cross-tool skill named after no tool injects into every
        // tool it lists in `references_tools`. `ping` is always
        // registered; the skill name (`graph_strategy`) is not a tool.
        let registry = build_registry_with_refs(&[(
            "graph_strategy",
            "Map structure first.",
            "GRAPH-BODY-SENTINEL",
            "[ping]",
        )]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        // The prompt route still registers under the skill name.
        assert!(server.prompt_router.map.contains_key("graph_strategy"));
        // ...and the referenced tool carries the full injection.
        let desc = tool_desc(&server, "ping");
        assert!(
            desc.contains("<!-- mcp-skill:graph_strategy -->"),
            "referenced tool should carry the skill marker; got: {desc}"
        );
        assert!(
            desc.contains("Map structure first."),
            "referenced tool should carry the skill routing; got: {desc}"
        );
        assert!(
            desc.contains("GRAPH-BODY-SENTINEL"),
            "referenced tool should carry the skill body; got: {desc}"
        );
    }

    #[test]
    fn serve_prompts_idempotent_when_skill_self_references() {
        // A skill named after its own tool that also lists that tool in
        // `references_tools` must inject exactly once — the dedup of
        // the target set plus the per-skill marker keep the pass clean.
        let registry = build_registry_with_refs(&[("ping", "Routing.", "Body.", "[ping]")]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let desc = tool_desc(&server, "ping");
        let marker_count = desc.matches("<!-- mcp-skill:ping -->").count();
        assert_eq!(
            marker_count, 1,
            "self-referencing skill must inject exactly once; got {marker_count}: {desc}"
        );
    }

    #[test]
    fn serve_prompts_idempotent_across_repeated_passes() {
        // Re-running the pass over the same server must not double-
        // append: the per-skill marker fences each (skill, tool) pair.
        let registry = build_test_registry(&[("ping", "Routing.", "Body.", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let once = tool_desc(&server, "ping");
        super::serve_prompts(&registry, &mut server);
        let twice = tool_desc(&server, "ping");
        assert_eq!(
            once, twice,
            "second pass must be a no-op for an already-injected tool"
        );
    }

    #[test]
    fn serve_prompts_multiple_skills_stack_on_one_tool() {
        // A tool can carry its own name-match skill plus a referencing
        // cross-tool skill — both injections coexist, each fenced by
        // its own marker.
        let registry = build_registry_with_refs(&[
            ("ping", "Ping routing.", "PING-BODY", "[]"),
            ("ping_strategy", "Strategy routing.", "STRAT-BODY", "[ping]"),
        ]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let desc = tool_desc(&server, "ping");
        assert!(desc.contains("<!-- mcp-skill:ping -->"), "got: {desc}");
        assert!(
            desc.contains("<!-- mcp-skill:ping_strategy -->"),
            "got: {desc}"
        );
        assert!(
            desc.contains("PING-BODY") && desc.contains("STRAT-BODY"),
            "got: {desc}"
        );
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
