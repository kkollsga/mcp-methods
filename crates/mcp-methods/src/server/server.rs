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
//! builtins) and a shared `SkillState` — the rmcp `ToolRouter` and
//! `PromptRouter` plus everything the skill-resolution pass writes,
//! behind one `RwLock` so `reinject_skills` can replace it from `&self`
//! after the server is serving. Every clone shares that state; request
//! paths take an `Arc` snapshot and release the lock immediately, so
//! nothing holds it across an awaited handler. The `github_issues` closure
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
use rmcp::{tool, tool_router, ErrorData as McpError, ServerHandler};
use serde::{Deserialize, Serialize};

use crate::server::manifest::Manifest;
use crate::server::skills::{Delivery, ResolvedRegistry, SkillProvenance};
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

// ---------------------------------------------------------------------------
// Skill-rebuild re-entrancy guard
// ---------------------------------------------------------------------------

thread_local! {
    /// Depth of nesting on *this* thread inside a region a skill
    /// rebuild must not start from: a consumer's
    /// [`ResultPostprocessHook`], or the skill-resolution pass itself.
    ///
    /// A counter rather than a bool because the resolution pass can
    /// legitimately run a consumer's `SkillPredicateEvaluator`, and
    /// nothing forbids a consumer nesting its own bookkeeping.
    static NO_REBUILD_DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// RAII marker for "a skill rebuild started from this thread right now
/// would be wrong". See [`reject_rebuild_reentry`] for why each region
/// is one.
struct NoRebuildGuard;

impl NoRebuildGuard {
    fn enter() -> Self {
        NO_REBUILD_DEPTH.with(|d| d.set(d.get() + 1));
        NoRebuildGuard
    }
}

impl Drop for NoRebuildGuard {
    fn drop(&mut self) {
        NO_REBUILD_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

fn inside_no_rebuild_region() -> bool {
    NO_REBUILD_DEPTH.with(|d| d.get() > 0)
}

/// The message a re-entrant rebuild gets back, naming the entry point
/// it was refused at.
///
/// Neither region deadlocks — no lock is held across either of them —
/// so this is a refusal on the merits, not a lock-ordering dodge:
///
/// - **From a `ResultPostprocessHook`**: the hook's only output is a
///   footer string and its [`ResultCtx`] carries no `Peer`, so a
///   rebuild there cannot send `notifications/tools/list_changed` and
///   cannot report its own failure. The surface would change while
///   every connected client kept serving its cached `tools/list`. The
///   footer for *this* call was also computed before the handler ran,
///   against the set the rebuild is replacing.
/// - **From inside the resolution pass** (a `SkillPredicateEvaluator`,
///   say): the containing pass finishes last and overwrites whatever
///   the nested rebuild installed, so the nested one is work that
///   silently does not happen.
fn reject_rebuild_reentry(who: &str) -> String {
    format!(
        "{who} was called from inside a result-postprocess hook or a skill-resolution \
         pass, and is refused there. Neither has a peer to send \
         notifications/tools/list_changed through, so the rebuilt surface would be \
         invisible to every connected client; a rebuild nested inside the resolution \
         pass is additionally overwritten by the pass containing it. Return first, \
         then drive {who} from the tool handler and notify the peer."
    )
}

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
///
/// `skill_notice` is the framework's unloaded-lazy-skill footer,
/// computed by the caller (which has the server) and applied here so
/// the consumer's hook sees the composed body rather than replacing
/// it. Same every-arm rule as the hook.
fn dispatch_typed_call<T, F>(
    tool_name: &str,
    arguments: Option<rmcp::model::JsonObject>,
    handler: &F,
    postprocess: Option<&ResultPostprocessHook>,
    source_roots: Option<&SourceRootsProvider>,
    workspace: Option<&crate::server::workspace::Workspace>,
    skill_notice: Option<String>,
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
    let body = append_footer(body, skill_notice);
    let body = match postprocess {
        Some(hook) => {
            let ctx = ResultCtx {
                source_roots: source_roots.map(|p| p()).unwrap_or_default(),
                active_repo: workspace.and_then(|w| w.active_repo_name()),
            };
            // Marked so a hook that reaches back for a skill rebuild
            // is refused by name instead of silently changing the
            // surface with no peer to announce it on.
            let _no_rebuild = NoRebuildGuard::enter();
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

/// Build the dyn tool route behind [`McpServer::register_typed_tool`]
/// and its fallible sibling: generate the JSON Schema for `T`, build
/// the [`rmcp::model::Tool`] attr, capture the result-postprocess
/// plumbing, and defer every per-call decision to
/// [`dispatch_typed_call`].
///
/// A free function rather than a method because the skill-resolution
/// pass builds the `skill(name)` route while it already holds the
/// state lock, where `&mut self` is not available.
fn typed_route<T, F>(
    name: &'static str,
    description: &'static str,
    handler: F,
    options: &ServerOptions,
) -> rmcp::handler::server::router::tool::ToolRoute<McpServer>
where
    T: for<'de> serde::Deserialize<'de> + schemars::JsonSchema + Default + Send + Sync + 'static,
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
    let postprocess = options.result_postprocess.clone();
    let source_roots = options.source_roots.clone();
    let workspace = options.workspace.clone();

    rmcp::handler::server::router::tool::ToolRoute::new_dyn(
        attr,
        move |ctx: rmcp::handler::server::tool::ToolCallContext<'_, McpServer>|
              -> DynFut<'_, Result<rmcp::model::CallToolResponse, rmcp::ErrorData>> {
            let handler = handler.clone();
            let arguments = ctx.arguments.clone();
            let postprocess = postprocess.clone();
            let source_roots = source_roots.clone();
            let workspace = workspace.clone();
            // The dyn route has no `&self`, but rmcp hands the
            // serving instance over on the call context — the
            // one place a dynamic handler can reach the skill
            // state the resolution pass filled in.
            let skill_notice = ctx.service.unloaded_skill_notice(tool_name);
            Box::pin(async move {
                Ok(dispatch_typed_call(
                    tool_name,
                    arguments,
                    handler.as_ref(),
                    postprocess.as_ref(),
                    source_roots.as_ref(),
                    workspace.as_ref(),
                    skill_notice,
                )
                .into())
            })
        },
    )
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

/// Arguments to the framework's `skill(name)` loader.
#[derive(Debug, Default, Deserialize, Serialize, schemars::JsonSchema)]
pub struct SkillArgs {
    /// Name of the skill to load, exactly as the pointer in a tool
    /// description spells it.
    pub name: String,
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

/// Domain-supplied preview guidance: tool name, original arguments, complete MCP
/// result -> JSON summary/coverage/next-query hints. Guidance is itself budgeted;
/// it never changes the original data or reruns a handler.
pub type ResponsePreviewHook =
    Arc<dyn Fn(&str, &serde_json::Value, &serde_json::Value) -> serde_json::Value + Send + Sync>;

// Hold the negotiated-info Arc with retained results so an allocator cannot
// reuse a disconnected peer's address to grant another session access.
#[derive(Clone)]
struct ResponseSession(Arc<InitializeRequestParams>);
impl PartialEq for ResponseSession {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

tokio::task_local! {
    /// The session a tool call belongs to, scoped around the router
    /// dispatch in [`McpServer::call_tool`].
    ///
    /// The two places that need it — the `skill()` handler marking a
    /// body delivered, and the unloaded-skill footer — sit below the
    /// point where the session is known: a static `#[tool]` method
    /// receives only its `Parameters`, and a dynamically registered
    /// handler is a plain `Fn(T) -> Result<String, String>`. Widening
    /// either signature would change a public contract for every
    /// consumer to serve one framework footer, so the session travels
    /// out-of-band instead. Absent outside a dispatch (unit tests that
    /// call a handler directly), where every reader treats it as
    /// "no session" and stays silent.
    static CURRENT_SESSION: ResponseSession;
}

/// The name of the framework tool that fetches a lazy skill's body.
pub const SKILL_TOOL_NAME: &str = "skill";

/// Which lazy skills each session has fetched through `skill()`.
///
/// Keyed by the same [`ResponseSession`] identity the 0.4.9 response
/// store uses — the negotiated `InitializeRequestParams` Arc, compared
/// by pointer — so "this session" means the same thing on both sides,
/// and a reconnecting client starts empty rather than inheriting a
/// dead peer's state.
///
/// A session's record is kept alive by **any** tool call:
/// [`McpServer::call_tool`] touches it on every dispatch, so a session
/// that keeps working keeps what it has loaded however long the work
/// runs. Only a session that makes no tool call at all for
/// [`ttl`](Self::ttl) is dropped, and it then starts empty — an agent
/// that has been away that long is worth re-nudging. The window is
/// borrowed from [`crate::response_budget::TTL`] for one reason: it is
/// the same "this session went quiet" threshold, not because the two
/// expire together. The response store expires each retained *entry*
/// on its own creation time and does not track sessions at all, so a
/// busy session can lose an old retained result while keeping every
/// skill it loaded.
struct LoadedSkills {
    sessions: Vec<(
        ResponseSession,
        std::collections::HashSet<String>,
        std::time::Instant,
    )>,
    /// How long a session's loaded set survives with no tool calls.
    /// [`crate::response_budget::TTL`] in production; the tests
    /// shorten it so expiry is observable without a ten-minute wait.
    ttl: std::time::Duration,
}

impl Default for LoadedSkills {
    fn default() -> Self {
        Self {
            sessions: Vec::new(),
            ttl: crate::response_budget::TTL,
        }
    }
}

/// Cap on tracked sessions, mirroring the response store's entry cap.
/// Least-recently-active first; an evicted session is re-nudged, never
/// wrongly silenced.
const LOADED_SKILL_SESSIONS: usize = 32;

impl LoadedSkills {
    fn expire(&mut self) {
        let ttl = self.ttl;
        self.sessions
            .retain(|(_, _, touched)| touched.elapsed() < ttl);
        while self.sessions.len() > LOADED_SKILL_SESSIONS {
            let stalest = self
                .sessions
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, _, touched))| *touched)
                .map_or(0, |(index, _)| index);
            self.sessions.remove(stalest);
        }
    }

    /// Record that `owner` is still working, and expire whoever is not.
    ///
    /// Called once per tool call, before anything reads the set, so
    /// activity — not load recency — is what keeps a record alive.
    /// Creates nothing: a session that has never loaded a skill has no
    /// record to keep, and giving it one would grow the table for every
    /// client that never calls `skill()`.
    fn touch(&mut self, owner: &ResponseSession) {
        self.expire();
        if let Some((_, _, touched)) = self
            .sessions
            .iter_mut()
            .find(|(session, _, _)| session == owner)
        {
            *touched = std::time::Instant::now();
        }
    }

    fn mark(&mut self, owner: &ResponseSession, name: &str) {
        self.expire();
        if let Some((_, loaded, touched)) = self
            .sessions
            .iter_mut()
            .find(|(session, _, _)| session == owner)
        {
            loaded.insert(name.to_string());
            *touched = std::time::Instant::now();
            return;
        }
        let mut loaded = std::collections::HashSet::new();
        loaded.insert(name.to_string());
        self.sessions
            .push((owner.clone(), loaded, std::time::Instant::now()));
    }

    fn contains(&self, owner: &ResponseSession, name: &str) -> bool {
        self.sessions
            .iter()
            .find(|(session, _, _)| session == owner)
            .is_some_and(|(_, loaded, _)| loaded.contains(name))
    }

    /// Forget one skill in every session, so the next call of a tool
    /// that advertises it nudges again. Used when a re-resolve changes
    /// a body under a name an agent has already fetched: what that
    /// agent is working from is no longer what `skill()` would hand
    /// out.
    fn forget(&mut self, name: &str) {
        for (_, loaded, _) in &mut self.sessions {
            loaded.remove(name);
        }
    }
}

/// One skill that survived both activation gates in
/// [`serve_prompts`] — its `applies_when:` predicates and the
/// registered-target check — and is therefore reachable by the agent.
///
/// Returned by [`serve_prompts`] and kept on the server behind
/// [`McpServer::active_skills`] so a downstream handler can print the
/// index (kglite's bare `graph_overview()` does) without re-running
/// the activation pass and getting a different answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveSkill {
    /// Skill name — the argument `skill(name)` takes.
    pub name: String,
    /// One-line routing description, as injected under
    /// `## When to use`.
    pub description: String,
    /// Which tier the skill was injected on.
    pub delivery: Delivery,
    /// Which layer the skill resolved from.
    pub provenance: SkillProvenance,
}

/// Everything the skill-resolution pass writes, behind one lock so
/// [`McpServer::reinject_skills`] can replace it from `&self` after the
/// server is already serving.
///
/// Both routers are `Arc`-wrapped *inside* the lock so a request path
/// takes a snapshot (one `Arc::clone`) under a short read guard and
/// then releases it. Nothing holds this lock across an awaited tool
/// handler or prompt handler, which is what lets a tool handler drive a
/// rebuild without blocking on its own dispatch, and lets a rebuild
/// land while a long call is in flight. Mutation is copy-on-write via
/// `Arc::make_mut`, so the snapshot an in-flight call is running
/// against stays valid while the rebuild installs a new one.
struct SkillState {
    tools: Arc<ToolRouter<McpServer>>,
    /// Skill-backed prompt routes. Empty until the resolution pass runs
    /// with a non-empty registry; it stays empty for the zero-skills
    /// boot path so `prompts/list` returns the rmcp default (empty
    /// result, no capability advertised).
    prompts: Arc<PromptRouter<McpServer>>,
    /// The prompt routes *this module* registered, in registration
    /// order. A re-resolve removes exactly these, so a route a
    /// downstream binary added through
    /// [`McpServer::prompt_router_mut`] survives the rebuild.
    skill_prompts: Vec<String>,
    /// Tool name → the lazy skills advertised on that tool's
    /// description. The source of the per-call "you have not loaded
    /// this skill" footer.
    lazy_skill_targets: std::collections::HashMap<String, Vec<String>>,
    /// Post-activation skill set in name order, as returned by
    /// [`serve_prompts`]. Empty until it runs.
    active_skills: Vec<ActiveSkill>,
    /// `Some` once the pass has registered the `skill(name)` tool.
    /// `None` means lazy routing has nowhere to point, and the
    /// injection pass falls back to embedding bodies.
    skill_loader: Option<&'static str>,
    /// Hash of the body every skill name has resolved to, accumulated
    /// across rebuilds. Drives the loaded-set rule: a name whose body
    /// changed is forgotten in every session, a name whose body is
    /// unchanged stays loaded. Names that leave the active set keep
    /// their entry, so a skill that disappears and comes back with a
    /// different body is still caught.
    body_hashes: std::collections::HashMap<String, u64>,
}

/// Write access to the tool router for dynamic tool registration.
///
/// Returned by [`McpServer::tool_router_mut`]; derefs to the
/// [`ToolRouter`] itself, so `server.tool_router_mut().add_route(..)`
/// reads the same as it did when the router was a plain field. Holds
/// the state lock for as long as it lives — keep it to one statement.
pub struct ToolRouterMut<'a> {
    guard: std::sync::RwLockWriteGuard<'a, SkillState>,
}

impl std::ops::Deref for ToolRouterMut<'_> {
    type Target = ToolRouter<McpServer>;
    fn deref(&self) -> &Self::Target {
        &self.guard.tools
    }
}

impl std::ops::DerefMut for ToolRouterMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.guard.tools)
    }
}

/// Write access to the prompt router. Same contract as
/// [`ToolRouterMut`].
pub struct PromptRouterMut<'a> {
    guard: std::sync::RwLockWriteGuard<'a, SkillState>,
}

impl std::ops::Deref for PromptRouterMut<'_> {
    type Target = PromptRouter<McpServer>;
    fn deref(&self) -> &Self::Target {
        &self.guard.prompts
    }
}

impl std::ops::DerefMut for PromptRouterMut<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Arc::make_mut(&mut self.guard.prompts)
    }
}

/// MCP server backed by the rmcp framework.
///
/// The struct is cloned per request by rmcp's handler dispatch; the
/// expensive bits (provider closure, routers, session state) are behind
/// an Arc so cloning is cheap. Every clone shares one skill state,
/// so a rebuild driven through one clone is visible to all of them.
#[derive(Clone)]
pub struct McpServer {
    options: ServerOptions,
    skills: Arc<std::sync::RwLock<SkillState>>,
    responses: Arc<Mutex<crate::response_budget::ResponseStore<ResponseSession>>>,
    response_preview: Option<ResponsePreviewHook>,
    /// Per-session record of the lazy bodies `skill()` has handed out.
    /// Behind an `Arc` because every clone of the server serves the
    /// same sessions.
    loaded_skills: Arc<Mutex<LoadedSkills>>,
}

#[tool_router]
impl McpServer {
    pub fn new(options: ServerOptions) -> Self {
        let mut server = Self {
            options,
            skills: Arc::new(std::sync::RwLock::new(SkillState {
                tools: Arc::new(Self::tool_router()),
                prompts: Arc::new(PromptRouter::new()),
                skill_prompts: Vec::new(),
                lazy_skill_targets: std::collections::HashMap::new(),
                active_skills: Vec::new(),
                skill_loader: None,
                body_hashes: std::collections::HashMap::new(),
            })),
            responses: Arc::new(Mutex::new(crate::response_budget::ResponseStore::default())),
            response_preview: None,
            loaded_skills: Arc::new(Mutex::new(LoadedSkills::default())),
        };
        server.register_github_tools_if_authorized();
        server.register_local_workspace_tools();
        server.gate_workspace_tools();
        server
    }

    /// The tool router as it stands right now, as a cheap snapshot.
    ///
    /// The read guard is taken and dropped inside this call: callers
    /// get an `Arc` they can dispatch against, list from, or hold
    /// across an `await` without blocking a rebuild.
    fn tools_snapshot(&self) -> Arc<ToolRouter<McpServer>> {
        self.skill_state().tools.clone()
    }

    /// The prompt router as it stands right now. Same contract as
    /// [`tools_snapshot`](Self::tools_snapshot).
    fn prompts_snapshot(&self) -> Arc<PromptRouter<McpServer>> {
        self.skill_state().prompts.clone()
    }

    /// Read the shared skill state. A poisoned lock is recovered
    /// rather than propagated: every writer leaves the state
    /// structurally intact, and a panic in one tool handler must not
    /// take the whole tool surface down with it.
    fn skill_state(&self) -> std::sync::RwLockReadGuard<'_, SkillState> {
        self.skills.read().unwrap_or_else(|e| e.into_inner())
    }

    /// Write the shared skill state. Same poison handling as
    /// [`skill_state`](Self::skill_state).
    fn skill_state_mut(&self) -> std::sync::RwLockWriteGuard<'_, SkillState> {
        self.skills.write().unwrap_or_else(|e| e.into_inner())
    }

    /// The name of the `skill(name)` loader **this crate registered**,
    /// or `None` when no loader is registered — including the case
    /// where a downstream tool owns the name and the resolution pass
    /// yielded it.
    ///
    /// The one thing that distinction buys: the framework loader
    /// returns a skill body, which
    /// [`HARD_SIZE_LIMIT_BYTES`](crate::server::skills::HARD_SIZE_LIMIT_BYTES)
    /// bounds at load, so it is exempt from the response budget. A
    /// downstream tool of the same name carries no such bound and is
    /// budgeted like any other.
    fn framework_skill_loader(&self) -> Option<&'static str> {
        self.skill_state().skill_loader
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
            self.tool_router_mut().remove_route("repo_management");
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
    /// Use at server-construction time (before [`serve`](rmcp::ServiceExt::serve)).
    /// The `&mut self` receiver is what enforces that: once the server
    /// is serving, rmcp hands handlers `&self` only. The one supported
    /// way to change the surface after that is
    /// [`reinject_skills`](Self::reinject_skills), which rebuilds the
    /// skill layer atomically and tells you to notify the peer.
    ///
    /// The returned guard holds the shared state lock; bind it no
    /// longer than the statement that uses it.
    pub fn tool_router_mut(&mut self) -> ToolRouterMut<'_> {
        ToolRouterMut {
            guard: self.skill_state_mut(),
        }
    }

    /// Mutable access to the prompt router for dynamic skill / prompt
    /// registration. Same lifecycle contract as
    /// [`tool_router_mut`](Self::tool_router_mut):
    /// boot-time only. Most operators reach prompts via
    /// [`serve_prompts`] rather than touching the router directly.
    ///
    /// A route added here is *not* one the skill pass owns, so
    /// [`reinject_skills`](Self::reinject_skills) leaves it in place.
    pub fn prompt_router_mut(&mut self) -> PromptRouterMut<'_> {
        PromptRouterMut {
            guard: self.skill_state_mut(),
        }
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
    /// build the route with [`typed_route`] and install it.
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
        let route = typed_route(name, description, handler, &self.options);
        self.tool_router_mut().add_route(route);
    }

    /// The skills that survived activation in the resolution pass, in
    /// name order. Empty before it runs, and on a server booted with
    /// skills off.
    ///
    /// This is the same list [`serve_prompts`] (or the most recent
    /// [`reinject_skills`](Self::reinject_skills)) returned; reading it
    /// here lets a tool handler print the index at request time without
    /// re-evaluating `applies_when:` against state that has since
    /// moved.
    pub fn active_skills(&self) -> Vec<ActiveSkill> {
        self.skill_state().active_skills.clone()
    }

    /// One line per lazy skill advertised on `tool` that this session
    /// has not fetched yet, or `None` when there is nothing to say.
    ///
    /// Never fires for the `skill()` tool itself (loading a skill is
    /// not an occasion to be told to load it) and never for
    /// `expand_response`, which returns before the router dispatch
    /// this reads from. Silent when no session is in scope.
    ///
    /// Silent once the body has been delivered, for as long as the
    /// session keeps calling tools and the body stays the same. Two
    /// things bring the line back: a
    /// [`reinject_skills`](Self::reinject_skills) that changed that
    /// skill's body under the agent, and a session that made no tool
    /// call at all for [`crate::response_budget::TTL`] — both cases
    /// where what the agent is holding is stale or gone. See
    /// [`LoadedSkills`].
    fn unloaded_skill_notice(&self, tool: &str) -> Option<String> {
        // Read the skill state into owned values and release the lock
        // before anything else: this runs on the dispatch path, and a
        // rebuild must never queue behind it.
        let (loader, advertised) = {
            let state = self.skill_state();
            if Some(tool) == state.skill_loader {
                return None;
            }
            let loader = state.skill_loader?;
            (loader, state.lazy_skill_targets.get(tool)?.clone())
        };
        let session = CURRENT_SESSION.try_with(|session| session.clone()).ok()?;
        let loaded = self.loaded_skills.lock().unwrap_or_else(|e| e.into_inner());
        let lines: Vec<String> = advertised
            .iter()
            .filter(|name| !loaded.contains(&session, name))
            .map(|name| {
                format!(
                    "Skill {name:?} applies to this tool and has not been loaded \
                     this session — call {loader}({name:?})."
                )
            })
            .collect();
        (!lines.is_empty()).then(|| lines.join("\n"))
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
        // Framework footer first, then the consumer's hook sees the
        // composed body. The single hook slot is the consumer's; a
        // framework notice that replaced it would silently drop a
        // downstream server's identity or rebuild stamp.
        let body = append_footer(body, self.unloaded_skill_notice(tool));
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
        // Same refusal region as the dynamic path — see
        // [`reject_rebuild_reentry`].
        let _no_rebuild = NoRebuildGuard::enter();
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

/// The opening half of the per-(skill, tool) injection fence. Written
/// by [`injection_block`], matched by the idempotency check, and
/// searched for by [`strip_injected_skills`] — one constant so the
/// three cannot drift apart.
const SKILL_MARKER_OPEN: &str = "<!-- mcp-skill:";

/// The `skill(name)` loader's description. A `&'static str` because
/// [`typed_route`] takes one, and a constant because the resolution
/// pass re-registers the route on every rebuild.
///
/// "Verbatim" is load-bearing and true: the framework-owned loader is
/// exempt from the response budget (see
/// [`McpServer::framework_skill_loader`]), so a body inside the
/// [`crate::server::skills::HARD_SIZE_LIMIT_BYTES`] cap is never
/// returned as a preview excerpt.
const SKILL_TOOL_DESCRIPTION: &str =
    "Load a skill's full methodology by name. A tool description that ends in \
     `skill(\"<name>\")` is telling you to call this before you use that tool. \
     Returns the skill body verbatim — never truncated, never a preview. Pass \
     `name` exactly as the pointer spells it. Skills are per-session: what you \
     load is remembered for as long as this session keeps working, and a new \
     session, or one that has made no tool call for ten minutes, starts empty.";

/// Wire a resolved skill registry into a server's `prompts/list` and
/// `prompts/get` surface, register the `skill(name)` loader, and apply
/// auto-injection hints to the descriptions of the tools each skill
/// targets.
///
/// Returns the skills that survived both activation gates — their
/// `applies_when:` predicates and the registered-target check — sorted
/// by name. The same list is stored on the server behind
/// [`McpServer::active_skills`].
///
/// Call at boot time after all tools have been registered (so the
/// auto-inject pass sees the final tool catalogue) and before
/// `serve(...)`. To re-resolve a registry *after* the server is
/// serving — a downstream graph was swapped, say — call
/// [`McpServer::reinject_skills`], which runs this same pass from
/// `&self` and hands back a refusal instead of an empty vector when it
/// is called from somewhere a rebuild cannot work.
///
/// Re-running the pass over the same server is safe: it strips what the
/// previous pass injected before injecting again, so a repeated call
/// with the same registry leaves every description byte-for-byte where
/// it was.
///
/// The function is additive and a no-op when the registry is empty
/// — downstream callers can wire it unconditionally without breaking
/// the zero-skills boot path.
pub fn serve_prompts(registry: &ResolvedRegistry, server: &mut McpServer) -> Vec<ActiveSkill> {
    match server.reinject_skills(registry) {
        Ok(active) => active,
        // Boot is never inside one of the refused regions, so this arm
        // means a consumer wired `serve_prompts` into a postprocess
        // hook or a predicate evaluator. Say so and serve no skills
        // rather than pretending the pass ran.
        Err(refusal) => {
            tracing::error!("{refusal}");
            Vec::new()
        }
    }
}

impl McpServer {
    /// Re-resolve `registry` into the live surface, replacing whatever
    /// the previous pass installed.
    ///
    /// This is the post-`serve` entry point: it takes `&self`, so a
    /// tool handler can drive it (see
    /// [`skill_reloader`](Self::skill_reloader) for how a handler gets
    /// hold of one). It strips every `<!-- mcp-skill:… -->` block from
    /// every tool description, removes the prompt routes the previous
    /// pass registered — and only those — drops the previous lazy-skill
    /// index, then runs the same resolution [`serve_prompts`] runs and
    /// publishes the result. Returns the new active set.
    ///
    /// **The caller must notify the peer.** Nothing here stores peers,
    /// so `tools/list` and `prompts/list` change under clients that
    /// will keep serving their cached copies until they are told
    /// otherwise. The sequence, from inside a tool handler that holds a
    /// `RequestContext`:
    ///
    /// ```ignore
    /// let registry = rebuild_my_registry()?;          // domain-side
    /// let active = reloader.reinject_skills(&registry)?;
    /// notify_skills_changed(&context.peer).await?;    // one call for both lists
    /// ```
    ///
    /// [`notify_skills_changed`] is the helper; it is two rmcp calls
    /// and you can make them yourself.
    ///
    /// **Session loaded-set rule.** A skill an agent already fetched
    /// with `skill(name)` stays fetched across a rebuild when its body
    /// is unchanged — the agent is not re-nudged for something it is
    /// already holding. A name whose **body changed** is forgotten in
    /// every session, because what that agent is working from is no
    /// longer what `skill()` would hand out. Names that leave the
    /// active set keep their recorded hash, so a skill that disappears
    /// and returns with a different body is still caught.
    ///
    /// **Capabilities do not change.** `initialize` has already
    /// happened, so a server that booted with no skills at all cannot
    /// start advertising the prompts capability by rebuilding into
    /// one; its new prompts are registered but unadvertised. Boot with
    /// at least one skill if the set can grow later.
    ///
    /// **Refused** from inside a [`ResultPostprocessHook`] or from
    /// inside the resolution pass itself, by name. Neither has a `Peer`
    /// in reach, so the rebuilt surface would never be announced to a
    /// client; a rebuild nested inside the pass is additionally
    /// overwritten by the pass containing it. Calling it from an
    /// ordinary tool handler is the intended use and is not refused.
    ///
    /// **Stripping is by marker.** A tool description that contains the
    /// literal `<!-- mcp-skill:` for its own reasons loses everything
    /// from that point on. Nothing in this crate writes that sequence
    /// except the injection pass.
    pub fn reinject_skills(&self, registry: &ResolvedRegistry) -> Result<Vec<ActiveSkill>, String> {
        resolve_skills(&self.skills, &self.loaded_skills, &self.options, registry)
    }

    /// A cloneable handle that can drive
    /// [`reinject_skills`](Self::reinject_skills) from a dynamically
    /// registered tool handler.
    ///
    /// A handler registered through
    /// [`register_typed_tool`](Self::register_typed_tool) is a plain
    /// `Fn(T) -> Result<String, String>` with no `&self` in reach, so a
    /// downstream binary that wants to rebuild its skills from a tool
    /// call takes this handle **before** it registers the tool and
    /// captures it into the closure:
    ///
    /// ```ignore
    /// let reloader = server.skill_reloader();
    /// server.register_typed_tool_fallible("reload_graph", "…", move |args: Args| {
    ///     let registry = swap_the_graph(args)?;
    ///     let active = reloader.reinject_skills(&registry)?;
    ///     Ok(format!("reloaded; {} skills active", active.len()))
    /// });
    /// ```
    ///
    /// The handle holds the server's shared state weakly, so capturing
    /// it into a route the server owns does not keep the server alive
    /// forever. It carries a snapshot of [`ServerOptions`] taken at
    /// this call, which is why it is taken after the options are
    /// final — the options only feed the re-registered `skill(name)`
    /// route's postprocess plumbing.
    pub fn skill_reloader(&self) -> SkillReloader {
        SkillReloader {
            skills: Arc::downgrade(&self.skills),
            loaded_skills: Arc::downgrade(&self.loaded_skills),
            options: self.options.clone(),
        }
    }
}

/// A detached handle on one server's skill layer.
///
/// Obtained from [`McpServer::skill_reloader`]; see that method for the
/// pattern it exists for and for the full contract of the rebuild it
/// drives.
#[derive(Clone)]
pub struct SkillReloader {
    skills: std::sync::Weak<std::sync::RwLock<SkillState>>,
    loaded_skills: std::sync::Weak<Mutex<LoadedSkills>>,
    options: ServerOptions,
}

impl std::fmt::Debug for SkillReloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillReloader")
            .field("live", &(self.skills.strong_count() > 0))
            .finish()
    }
}

impl SkillReloader {
    /// Re-resolve `registry` into the server this handle came from.
    /// Identical to [`McpServer::reinject_skills`], plus one more way
    /// to fail: the server may already be gone.
    pub fn reinject_skills(&self, registry: &ResolvedRegistry) -> Result<Vec<ActiveSkill>, String> {
        let (Some(skills), Some(loaded_skills)) =
            (self.skills.upgrade(), self.loaded_skills.upgrade())
        else {
            return Err(
                "reinject_skills: the server this SkillReloader came from has been dropped"
                    .to_string(),
            );
        };
        resolve_skills(&skills, &loaded_skills, &self.options, registry)
    }

    /// Whether the server this handle came from is still alive.
    pub fn is_live(&self) -> bool {
        self.skills.strong_count() > 0
    }
}

/// Tell a connected client that both the tool list and the prompt list
/// have changed, in that order.
///
/// The counterpart to [`McpServer::reinject_skills`], which changes the
/// two lists but stores no peers. A handler that holds a
/// `RequestContext` has the peer:
/// `notify_skills_changed(&context.peer).await`. Sending the tool
/// notification first is deliberate — the tool plane is the one every
/// real client exposes to the model.
///
/// Returns on the first failure; a client that never advertised
/// interest in either list still accepts both notifications.
pub async fn notify_skills_changed(
    peer: &rmcp::service::Peer<rmcp::RoleServer>,
) -> Result<(), rmcp::service::ServiceError> {
    peer.notify_tool_list_changed().await?;
    peer.notify_prompt_list_changed().await
}

/// Remove every `<!-- mcp-skill:… -->` block the injection pass
/// appended to a tool description, returning the description as it was
/// before the first injection — or `None` when the whole description
/// *was* the injection (the tool had none of its own).
///
/// A block runs from its marker to the next marker, or to the end of
/// the description. [`injection_block`] prefixes each block with a
/// blank line, so that separator comes off with it; a description that
/// began with the marker had no separator to start with.
fn strip_injected_skills(description: &str) -> Option<String> {
    let mut kept = String::with_capacity(description.len());
    let mut rest = description;
    while let Some(at) = rest.find(SKILL_MARKER_OPEN) {
        let head = &rest[..at];
        kept.push_str(head.strip_suffix("\n\n").unwrap_or(head));
        let after_marker = &rest[at + SKILL_MARKER_OPEN.len()..];
        rest = match after_marker.find(SKILL_MARKER_OPEN) {
            Some(next) => &after_marker[next..],
            None => "",
        };
    }
    kept.push_str(rest);
    (!kept.is_empty()).then_some(kept)
}

/// Hash a skill body for the loaded-set rule. Only ever compared
/// against another hash of the same function in the same process, so
/// the unspecified stability of `DefaultHasher` across releases does
/// not matter here.
fn body_hash(body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

/// The single skill-resolution pass, behind both [`serve_prompts`] and
/// [`McpServer::reinject_skills`].
///
/// Two phases, and the split is what makes a post-`serve` rebuild safe:
///
/// - **Phase A** runs against a router *snapshot* with no lock held.
///   `applies_when:` evaluation (which calls the consumer's
///   `SkillPredicateEvaluator`), the registered-target check, prompt
///   route construction and block rendering all happen here, so a slow
///   evaluator cannot stall a concurrent tool call.
/// - **Phase B** takes the state lock once and applies the plan: strip,
///   replace the prompt routes, (de)register the loader, inject,
///   publish. Nothing awaits inside it and it calls no consumer code.
///
/// A tool registered between the two phases is simply not a target this
/// round; the next rebuild picks it up.
fn resolve_skills(
    skills: &Arc<std::sync::RwLock<SkillState>>,
    loaded_skills: &Arc<Mutex<LoadedSkills>>,
    options: &ServerOptions,
    registry: &ResolvedRegistry,
) -> Result<Vec<ActiveSkill>, String> {
    use std::borrow::Cow;
    use std::collections::{HashMap, HashSet};

    if inside_no_rebuild_region() {
        return Err(reject_rebuild_reentry("reinject_skills"));
    }
    // Held for the whole pass, so consumer code the pass itself calls
    // — a `SkillPredicateEvaluator`, in phase A — is refused if it
    // reaches back here, rather than being silently overwritten by the
    // pass containing it.
    let _no_rebuild = NoRebuildGuard::enter();

    // ── Phase A — resolve against a snapshot, no lock held ──────────

    // The tool router has the full registered-tool list; extensions
    // come from the manifest's builtins block (operators may have
    // nothing here, in which case all `extension_enabled:` predicates
    // fail).
    let (tools, loader_is_ours) = {
        let state = skills.read().unwrap_or_else(|e| e.into_inner());
        (state.tools.clone(), state.skill_loader.is_some())
    };
    let registered_tools: HashSet<String> = tools
        .list_all()
        .iter()
        .map(|t| t.name.to_string())
        .collect();
    let extensions = options.extensions.clone();

    // For the auto-inject pass: skills with `auto_inject_hint` get
    // their `description` (routing) embedded into the descriptions of
    // their name-match tool AND every tool they list in
    // `references_tools`, followed by either the full body (eager) or
    // a line naming the loader tool (lazy). See `injection_block`.
    let mut auto_inject: Vec<InjectSkill> = Vec::new();
    let mut active: Vec<ActiveSkill> = Vec::new();
    let mut bodies: HashMap<String, String> = HashMap::new();
    let mut prompt_routes: Vec<PromptRoute<McpServer>> = Vec::new();

    for name in registry.skill_names() {
        let Some(skill) = registry.get(&name) else {
            continue;
        };

        // A skill named after the loader would inject into the tool
        // that fetches it and answer `skill("skill")` with its own
        // methodology. Nothing downstream needs that shape, and every
        // part of the routing text below would read as a loop.
        if name == SKILL_TOOL_NAME {
            tracing::warn!(
                skill = %name,
                "skill name collides with the framework's skill-loader tool; skipped"
            );
            continue;
        }

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

        // What the skill claims to teach, deduped so a self-reference
        // doesn't queue the same tool twice: its name-match tool *if a
        // tool by that name exists*, plus every `references_tools`
        // entry whether or not one does. `targets` is the registered
        // subset — where the injection actually lands.
        //
        // The name-match is only ever a *declared* target when it is
        // also a registered one: a skill named after nothing is not
        // claiming a tool called that, it just isn't using the
        // name-match channel.
        let mut declared: Vec<&str> = Vec::new();
        let mut targets: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        if tools.map.contains_key(skill.name()) {
            seen.insert(skill.name());
            declared.push(skill.name());
            targets.push(skill.name().to_string());
        }
        for tool in skill
            .frontmatter
            .references_tools
            .iter()
            .map(String::as_str)
        {
            if seen.insert(tool) {
                declared.push(tool);
                if tools.map.contains_key(tool) {
                    targets.push(tool.to_string());
                }
            }
        }

        // A skill that declares targets and finds none of them
        // registered has no channel to the agent: nothing to inject
        // into, and `prompts/get` is not a surface any real client
        // exposes to the model. Before 0.4.11 it still appeared in
        // `prompts/list`, which told operators the skill was live when
        // it reached nobody.
        //
        // A skill that declares *no* targets is a different animal and
        // stays: it is deliberate cross-cutting background, it injects
        // nowhere by construction rather than by accident, and
        // `skill(name)` will serve it to an agent another skill's body
        // points at.
        if !declared.is_empty() && targets.is_empty() {
            tracing::info!(
                skill = %name,
                declared_targets = ?declared,
                "skill declares only unregistered target tools; not advertised"
            );
            continue;
        }

        let prompt = Prompt::new(
            skill.name().to_string(),
            Some(skill.description().to_string()),
            None,
        );
        let body = skill.body.clone();
        prompt_routes.push(PromptRoute::new_dyn(prompt, move |_ctx| {
            let body = body.clone();
            Box::pin(async move {
                Ok(
                    GetPromptResult::new(vec![PromptMessage::new_text(Role::Assistant, body)])
                        .into(),
                )
            })
        }));

        active.push(ActiveSkill {
            name: skill.name().to_string(),
            description: skill.description().to_string(),
            delivery: skill.delivery(),
            provenance: skill.provenance.clone(),
        });
        bodies.insert(skill.name().to_string(), skill.body.clone());

        if skill.frontmatter.auto_inject_hint {
            auto_inject.push(InjectSkill {
                name: skill.name().to_string(),
                description: skill.description().to_string(),
                body: skill.body.clone(),
                delivery: skill.delivery(),
                targets,
                loader_tool: None,
            });
        }
    }

    // Read off the hashes before the bodies move into the loader's
    // closure; `state.body_hashes` compares against them in phase B.
    let resolved_hashes: Vec<(String, u64)> = bodies
        .iter()
        .map(|(name, body)| (name.clone(), body_hash(body)))
        .collect();

    // Decide the loader before rendering any block: a lazy skill with
    // nowhere to point falls back to the eager shape.
    //
    // It is registered whenever skills are on, not only when a lazy
    // skill exists, so an active-skills index a downstream tool prints
    // can always point at a tool that answers.
    //
    // A downstream binary that already owns the name keeps it: the
    // framework will not overwrite a route it did not create. Lazy
    // skills then fall back to eager injection, because a routing line
    // pointing at a tool that fetches something else is the exact
    // pre-0.3.37 failure this tier depends on not repeating.
    let loader = if registry.is_empty() {
        None
    } else if loader_is_ours || !tools.map.contains_key(SKILL_TOOL_NAME) {
        Some(SKILL_TOOL_NAME)
    } else {
        tracing::warn!(
            tool = SKILL_TOOL_NAME,
            "a registered tool already owns the skill-loader name; lazy skills will be \
             delivered eagerly instead"
        );
        None
    };

    // Auto-inject the skill's routing into tool descriptions, plus
    // either the methodology itself or a pointer at the loader.
    //
    // Background: pre-0.3.37 this appended a short pointer line
    // (`See `prompts/get` <name> for the full methodology.`) to the
    // tool description, assuming agents could call `prompts/get` to
    // fetch the body. **They can't** in real MCP clients — Claude Code,
    // Claude Desktop, Cursor, and Continue all expose only `tools/*`
    // to the model; the `prompts/` plane was designed for human-
    // invoked slash commands. Operators authoring against the pointer
    // pattern shipped methodology the agent literally could not read.
    // 0.3.37 answered that by embedding every body in every target
    // tool's description, which works but makes `tools/list` scale
    // with skills × referenced tools.
    //
    // 0.4.11 restores the pointer for the `lazy` tier — and it works
    // this time because the pointer names the `skill(name)` **tool**,
    // which every client does expose to the model. The routing
    // description still travels eagerly on both tiers: it is what the
    // agent reads to decide whether the body is worth fetching, it is
    // small by design, and it is not subject to the body's size caps
    // (4 KB soft / 16 KB hard, enforced at load). An empty description
    // omits the `## When to use` block.
    //
    // Injection goes to the skill's name-match tool AND every tool it
    // lists in `references_tools` — the only way to express a
    // *cross-tool* skill, one not named after any single tool.
    //
    // A tool may carry several skills (its own plus any that reference
    // it). Each injection is fenced by a per-skill marker
    // (`<!-- mcp-skill:<name> -->`) so the pass stays idempotent per
    // (skill, tool) pair on both tiers: a tool that is both the
    // name-match and a `references_tools` entry of the same skill gets
    // one injection, and re-running the pass never double-appends.
    //
    // Operators who want the skill off the tool plane entirely set
    // `auto_inject_hint: false` per skill; it stays on `prompts/*`
    // and reachable through `skill(name)`.
    for inj in &mut auto_inject {
        inj.loader_tool = loader;
    }

    // ── Phase B — apply the plan under one write lock ───────────────

    let mut state = skills.write().unwrap_or_else(|e| e.into_inner());

    // Undo the previous pass first, so this one is a replacement
    // rather than a second layer. Descriptions go back to their
    // pre-injection bytes; only the prompt routes *this* module
    // registered are dropped, leaving anything a downstream binary
    // added through `prompt_router_mut` alone.
    {
        let router = Arc::make_mut(&mut state.tools);
        for route in router.map.values_mut() {
            let Some(description) = route.attr.description.as_deref() else {
                continue;
            };
            if !description.contains(SKILL_MARKER_OPEN) {
                continue;
            }
            route.attr.description = strip_injected_skills(description).map(Cow::Owned);
        }
    }
    let previous_prompts = std::mem::take(&mut state.skill_prompts);
    let mut skill_prompts = Vec::with_capacity(prompt_routes.len());
    {
        let router = Arc::make_mut(&mut state.prompts);
        for name in previous_prompts {
            router.remove_route(&name);
        }
        for route in prompt_routes {
            skill_prompts.push(route.attr.name.to_string());
            router.add_route(route);
        }
    }
    state.skill_prompts = skill_prompts;

    // (De)register the loader. `add_route` replaces by name, so a
    // rebuild refreshes the bodies the tool serves without leaving a
    // second copy behind; a registry that went empty takes the tool
    // back out rather than answering from a stale map.
    match loader {
        Some(name) => {
            let names: Vec<String> = active.iter().map(|s| s.name.clone()).collect();
            let loaded = loaded_skills.clone();
            let route = typed_route(
                name,
                SKILL_TOOL_DESCRIPTION,
                move |args: SkillArgs| {
                    let requested = args.name.trim();
                    match bodies.get(requested) {
                        Some(body) => {
                            if let Ok(session) = CURRENT_SESSION.try_with(|s| s.clone()) {
                                loaded
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .mark(&session, requested);
                            }
                            Ok(body.clone())
                        }
                        None => Err(format!(
                            "No skill named {requested:?} is active in this session. \
                             Active skills: {}.",
                            if names.is_empty() {
                                "(none)".to_string()
                            } else {
                                names.join(", ")
                            }
                        )),
                    }
                },
                options,
            );
            Arc::make_mut(&mut state.tools).add_route(route);
            state.skill_loader = Some(name);
        }
        None => {
            if state.skill_loader.take().is_some() {
                Arc::make_mut(&mut state.tools).remove_route(SKILL_TOOL_NAME);
            }
        }
    }

    let mut lazy_targets: HashMap<String, Vec<String>> = HashMap::new();
    {
        let router = Arc::make_mut(&mut state.tools);
        for inj in &auto_inject {
            let marker = format!("{SKILL_MARKER_OPEN}{} -->", inj.name);
            let block = injection_block(inj);
            let lazy = inj.delivery == Delivery::Lazy && loader.is_some();

            for tool in &inj.targets {
                let key = Cow::<'static, str>::Owned(tool.clone());
                let Some(route) = router.map.get_mut(&key) else {
                    continue;
                };
                // Per-skill idempotency: never inject the same skill twice
                // into one tool's description.
                let already = route
                    .attr
                    .description
                    .as_deref()
                    .is_some_and(|d| d.contains(&marker));
                if !already {
                    let new_desc = match route.attr.description.take() {
                        Some(existing) => format!("{existing}{block}"),
                        None => block.trim_start().to_string(),
                    };
                    route.attr.description = Some(Cow::Owned(new_desc));
                }
                if lazy {
                    let entry = lazy_targets.entry(tool.clone()).or_default();
                    if !entry.contains(&inj.name) {
                        entry.push(inj.name.clone());
                    }
                }
            }
        }
    }

    state.lazy_skill_targets = lazy_targets;
    state.active_skills = active.clone();

    // Loaded-set rule: a name whose body changed under it is forgotten
    // in every session, so the next call of a tool that advertises it
    // nudges again. Everything else stays loaded.
    let mut changed: Vec<String> = Vec::new();
    for (name, hash) in resolved_hashes {
        match state.body_hashes.insert(name.clone(), hash) {
            Some(previous) if previous != hash => changed.push(name),
            _ => {}
        }
    }
    drop(state);
    if !changed.is_empty() {
        let mut loaded = loaded_skills.lock().unwrap_or_else(|e| e.into_inner());
        for name in &changed {
            loaded.forget(name);
        }
        tracing::info!(
            skills = ?changed,
            "skill bodies changed in a re-resolve; sessions will be nudged to reload them"
        );
    }

    Ok(active)
}

/// One skill's auto-inject inputs, as resolved against the live tool
/// catalogue. `targets` is already filtered to registered tools and
/// deduped; `loader_tool` is the name of the tool that fetches lazy
/// bodies, or `None` when no such tool could be registered.
struct InjectSkill {
    name: String,
    description: String,
    body: String,
    delivery: Delivery,
    targets: Vec<String>,
    loader_tool: Option<&'static str>,
}

/// Render the block appended to a target tool's description.
///
/// Pure: same `InjectSkill`, same string, no server state read. Both
/// tiers open with the idempotency marker and the routing description,
/// and they differ only in what follows — the methodology itself, or
/// one line naming the tool that fetches it. A lazy skill with no
/// loader registered falls back to the eager shape rather than
/// pointing at nothing.
fn injection_block(inj: &InjectSkill) -> String {
    let mut block = format!("\n\n{SKILL_MARKER_OPEN}{} -->", inj.name);
    let description = inj.description.trim();
    if !description.is_empty() {
        block.push_str("\n\n## When to use\n\n");
        block.push_str(description);
    }
    match (inj.delivery, inj.loader_tool) {
        (Delivery::Lazy, Some(loader)) => {
            let name = &inj.name;
            block.push_str(&format!(
                "\n\nLoad the full methodology with {loader}({name:?}) before first use."
            ));
        }
        _ => {
            block.push_str("\n\n## Methodology\n\n");
            block.push_str(inj.body.trim());
        }
    }
    block
}

fn response_control_name(schema: &rmcp::model::JsonObject) -> String {
    let mut name = "_response".to_string();
    while schema
        .get("properties")
        .and_then(|v| v.get(&name))
        .is_some()
    {
        name.push('_');
    }
    name
}

fn budgeted_tool(mut tool: Tool) -> Tool {
    let control = response_control_name(&tool.input_schema);
    let schema = Arc::make_mut(&mut tool.input_schema);
    schema
        .entry("properties")
        .or_insert_with(|| serde_json::json!({}))[&control] =
        crate::response_budget::options_schema();
    let description = format!("{}\nResponses default to 16384 serialized bytes. Set {control}.mode=full for complete inline output or {control}.max_bytes for a larger per-call budget. Previews include calls to expand retained evidence without rerunning this tool.", tool.description.as_deref().unwrap_or(""));
    tool.description = Some(description.into());
    if let Some(output) = tool.output_schema.take() {
        let mut output = output.as_ref().clone();
        let definitions = output.remove("$defs");
        let mut union = serde_json::json!({"anyOf":[output,{
            "type":"object","required":["mcp_methods_preview"],
            "properties":{"mcp_methods_preview":{"const":true}}
        }]});
        if let Some(definitions) = definitions {
            union["$defs"] = definitions;
        }
        tool.output_schema = Some(Arc::new(union.as_object().unwrap().clone()));
    }
    tool
}

impl McpServer {
    /// Supply domain-aware findings, coverage and follow-up queries for previews.
    /// The generic fallback reports structure and omissions without guessing
    /// relevance. Configure this before serving; clones share the same hook.
    pub fn with_response_preview_hook(mut self, hook: ResponsePreviewHook) -> Self {
        self.response_preview = Some(hook);
        self
    }

    fn response_expansion_name(&self) -> String {
        let tools = self.tools_snapshot();
        let mut name = "expand_response".to_string();
        while tools.get(&name).is_some() {
            name.push('_');
        }
        name
    }

    fn response_expansion_tool(&self) -> Tool {
        let mut tool = Tool::new(self.response_expansion_name(),
            "Inspect retained output without rerunning a tool. Use result_id from a preview; path is a JSON Pointer into its payload; offset selects items/fields/Unicode characters. response.mode=full returns the original inline result (empty path) or a complete selected value. Results are session-scoped and expire/evict as disclosed in the preview.",
            Arc::new(crate::response_budget::expansion_schema().as_object().unwrap().clone()));
        tool.annotations = Some(ToolAnnotations::new().read_only(true).idempotent(true));
        tool
    }
}

// No `#[tool_handler]` here. The macro only fills in `call_tool`,
// `list_tools`, `get_tool` and `get_info` when the impl block does not
// already define them, and this one defines all four — the generated
// bodies would dispatch straight at a router field and skip the
// response budget, the session scope and the skill notice. Every tool
// method below is hand-written on purpose.
impl ServerHandler for McpServer {
    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        use crate::response_budget::{Expansion, ResponseOptions};
        let expansion_tool = self.response_expansion_name();
        let owner = ResponseSession(
            context
                .peer
                .peer_info()
                .ok_or_else(|| McpError::invalid_request("initialize is required", None))?,
        );
        // Any tool call is proof this session is still working, so its
        // loaded-skill record survives. Before the `expand_response`
        // branch, because that is a tool call too. See [`LoadedSkills`].
        self.loaded_skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .touch(&owner);
        if request.name == expansion_tool {
            let args: Expansion =
                serde_json::from_value(serde_json::json!(request.arguments.unwrap_or_default()))
                    .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
            let result = self
                .responses
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .expand(&owner, &args, &expansion_tool)
                .map_err(|e| McpError::invalid_params(e, None))?;
            return serde_json::from_value::<CallToolResult>(result)
                .map(Into::into)
                .map_err(|e| McpError::internal_error(e.to_string(), None));
        }
        // One snapshot for the whole call: the lock is released here,
        // and the `Arc` keeps this call's view of the router alive
        // across the awaited handler. A rebuild landing mid-call
        // installs a new router for the *next* call and cannot block
        // on this one.
        let tools = self.tools_snapshot();
        let tool = tools
            .get(&request.name)
            .ok_or_else(|| McpError::invalid_params("Unknown tool", None))?;
        let control = response_control_name(&tool.input_schema);
        let options: ResponseOptions = request
            .arguments
            .as_mut()
            .and_then(|a| a.remove(&control))
            .map(serde_json::from_value)
            .transpose()
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?
            .unwrap_or_default();
        options
            .validate()
            .map_err(|e| McpError::invalid_params(e, None))?;
        let name = request.name.to_string();
        let arguments = serde_json::json!(request.arguments);
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        // Scope the session across the whole dispatch: the `skill()`
        // handler records what it delivered, and every other tool's
        // footer asks what this session has already loaded.
        let dispatched = CURRENT_SESSION
            .scope(owner.clone(), tools.call(tcc))
            .await?;
        match dispatched {
            CallToolResponse::Complete(result) => {
                let mut result = serde_json::to_value(result)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))?;
                if let Some(hook) = &self.response_preview {
                    let guidance = hook(&name, &arguments, &result);
                    if result.get("_meta").is_none() {
                        result["_meta"] = serde_json::json!({});
                    }
                    result["_meta"]["mcp_methods/preview"] = guidance;
                }
                // The framework's own `skill(name)` loader is exempt,
                // the way `expand_response` is: its result is one skill
                // body, and a body is bounded by
                // `HARD_SIZE_LIMIT_BYTES` (16 KB) at load, so the
                // exemption is bounded too. Without it a legal ~16 KB
                // skill serializes past the 16,384-byte default and
                // comes back as a ~2 KB preview excerpt — while
                // `LoadedSkills::mark` has already run inside the
                // handler, so the footer goes quiet and the agent
                // never learns it is holding a fragment. The eager
                // tier injects the same body whole; the tiers must not
                // disagree about what the methodology is.
                //
                // Only the *framework-owned* loader qualifies. A
                // downstream tool that took the `skill` name is an
                // ordinary tool with no such bound and stays budgeted.
                let exempt = Some(name.as_str()) == self.framework_skill_loader();
                let result = if exempt {
                    result
                } else {
                    self.responses
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .present(owner, &name, arguments, result, &options, &expansion_tool)
                };
                serde_json::from_value::<CallToolResult>(result)
                    .map(Into::into)
                    .map_err(|e| McpError::internal_error(e.to_string(), None))
            }
            other => Ok(other),
        }
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        let supports_cache_hints = context
            .protocol_version()
            .is_some_and(|version| version >= ProtocolVersion::V_2026_07_28);
        // The framework loader is exempt from the budget, so it must
        // not advertise the `_response` controls or the default-byte
        // sentence either — a schema promising a knob that does
        // nothing is a description the tool contradicts.
        let loader = self.framework_skill_loader();
        let mut tools: Vec<_> = self
            .tools_snapshot()
            .list_all()
            .into_iter()
            .map(|tool| {
                if Some(tool.name.as_ref()) == loader {
                    tool
                } else {
                    budgeted_tool(tool)
                }
            })
            .collect();
        tools.push(self.response_expansion_tool());
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            tools,
            meta: None,
            next_cursor: None,
            ttl_ms: supports_cache_hints.then_some(0),
            cache_scope: supports_cache_hints.then_some(CacheScope::Public),
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if name == self.response_expansion_name() {
            return Some(self.response_expansion_tool());
        }
        let tool = self.tools_snapshot().get(name).cloned()?;
        if Some(name) == self.framework_skill_loader() {
            return Some(tool);
        }
        Some(budgeted_tool(tool))
    }

    fn get_info(&self) -> ServerInfo {
        let name = self
            .options
            .name
            .clone()
            .unwrap_or_else(|| "MCP Server".to_string());
        // `list_changed` on tools is unconditional: any server built on
        // this crate can have its skill layer rebuilt through
        // [`McpServer::reinject_skills`], and a client that was not
        // told the list can change has no reason to re-fetch it after
        // the notification.
        //
        // Prompts still only appear when at least one skill is
        // registered. The zero-skills boot path is the existing
        // contract and must keep producing capability output that's
        // byte-identical to today — which also means a server that
        // boots with no skills and grows them via `reinject_skills`
        // serves them unadvertised, because `initialize` is long over
        // by then. ServerCapabilities is `#[non_exhaustive]` but its
        // fields are pub, so we mutate after `build()` rather than
        // fighting the type-state builder.
        let mut caps = ServerCapabilities::builder()
            .enable_tools()
            .enable_tool_list_changed()
            .build();
        if !self.prompts_snapshot().map.is_empty() {
            let mut prompts = PromptsCapability::default();
            prompts.list_changed = Some(true);
            caps.prompts = Some(prompts);
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
            prompts: self.prompts_snapshot().list_all(),
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
        // Snapshot, then await: the same rule the tool path follows, so
        // a rebuild cannot queue behind a prompt handler.
        let prompts = self.prompts_snapshot();
        prompts.get_prompt(prompt_context).await
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
        let tools = server.tools_snapshot().list_all();
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
            .tools_snapshot()
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
            .tools_snapshot()
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
            .tools_snapshot()
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
            .prompts_snapshot()
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
            .tools_snapshot()
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
    /// `auto_inject_hint: true` and **`delivery: eager`** — the tests
    /// built on this one assert where the body lands, which is only a
    /// question on the eager tier.
    fn build_registry_with_refs(
        skills: &[(&str, &str, &str, &str)],
    ) -> crate::server::skills::ResolvedRegistry {
        let with_tier: Vec<(String, String, String, String)> = skills
            .iter()
            .map(|(name, description, body, refs)| {
                (
                    name.to_string(),
                    description.to_string(),
                    body.to_string(),
                    format!("references_tools: {refs}\ndelivery: eager"),
                )
            })
            .collect();
        let borrowed: Vec<(&str, &str, &str, &str)> = with_tier
            .iter()
            .map(|(n, d, b, extra)| (n.as_str(), d.as_str(), b.as_str(), extra.as_str()))
            .collect();
        build_registry_with_frontmatter(&borrowed)
    }

    /// A registry whose skills carry arbitrary extra frontmatter lines
    /// (`delivery:`, `references_tools:`, ...). Every skill is
    /// `auto_inject_hint: true`.
    fn build_registry_with_frontmatter(
        skills: &[(&str, &str, &str, &str)],
    ) -> crate::server::skills::ResolvedRegistry {
        use crate::server::skills::Registry;
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("manifest.yaml");
        let skills_dir = dir.path().join("manifest.skills");
        std::fs::create_dir_all(&skills_dir).unwrap();
        for (name, description, body, extra_frontmatter) in skills {
            let content = format!(
                "---\nname: {name}\ndescription: {description}\n\
                 auto_inject_hint: true\n{extra_frontmatter}\n---\n\n{body}\n"
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
            .tools_snapshot()
            .get(tool)
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default()
    }

    #[test]
    fn prompt_router_empty_by_default() {
        let server = McpServer::new(ServerOptions::default());
        assert!(server.prompts_snapshot().map.is_empty());
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

        let prompts = server.prompts_snapshot().list_all();
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
        assert!(server.prompts_snapshot().map.is_empty());
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
        let registry = build_registry_with_frontmatter(&[(
            "ping",
            "Ping methodology.",
            "PING-BODY-SENTINEL",
            "delivery: eager",
        )]);
        let mut server = McpServer::new(ServerOptions::default());
        let before = server
            .tools_snapshot()
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        super::serve_prompts(&registry, &mut server);
        let after = server
            .tools_snapshot()
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
            .tools_snapshot()
            .get("ping")
            .and_then(|t| t.description.clone())
            .map(|c| c.into_owned())
            .unwrap_or_default();
        super::serve_prompts(&registry, &mut server);
        let after = server
            .tools_snapshot()
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
    fn serve_prompts_drops_skill_with_no_registered_target() {
        // Defect fix, independent of the delivery tiers: a skill that
        // declares targets and finds every one of them unregistered
        // injects nowhere, and `prompts/get` is not a surface a real
        // client shows the model — so listing it told operators a
        // skill was live when it reached nobody. (A skill that
        // declares no targets at all is kept — see
        // `serve_prompts_keeps_skill_that_declares_no_targets`.)
        //
        // Mutation: move `prompt_router.add_route` back above the
        // target computation — the route reappears and this fails.
        let registry = build_registry_with_refs(&[(
            "no_such_tool",
            "Methodology.",
            "Body.",
            "[also_not_a_tool]",
        )]);
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);
        assert!(
            !server.prompts_snapshot().map.contains_key("no_such_tool"),
            "an unadvertisable skill must not appear in prompts/list"
        );
        assert!(active.is_empty(), "{active:?}");
        // No panic, no mutation of unrelated tools — the ping tool's
        // description is unchanged.
        let ping_desc = tool_desc(&server, "ping");
        assert!(!ping_desc.contains("no_such_tool"));
    }

    #[test]
    fn serve_prompts_keeps_skill_that_declares_no_targets() {
        // The drop rule is "declared targets, none registered", not
        // "no registered targets". A skill named after no tool with an
        // empty `references_tools` declares nothing, so it has no
        // unregistered target to be judged on: it stays listed, stays
        // servable by `skill()`, and injects nowhere — which is what
        // it has always done.
        //
        // Mutation: treat empty targets as all-missing (`if
        // targets.is_empty()`) — the skill vanishes and this fails.
        let registry = build_test_registry(&[("cross_cutting", "Routing.", "Body.", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);
        assert!(
            server.prompts_snapshot().map.contains_key("cross_cutting"),
            "a skill that declares no targets must stay advertised"
        );
        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(active[0].name, "cross_cutting");
        // It injects nowhere — no tool is named after it and it
        // references none.
        assert!(!tool_desc(&server, "ping").contains("cross_cutting"));
    }

    #[test]
    fn serve_prompts_keeps_skill_with_one_registered_target_of_several() {
        // The check is "every target missing", not "any target
        // missing". Mutation: flip the `targets.is_empty()` guard to
        // "any declared target unregistered" — this fails.
        let registry = build_registry_with_refs(&[(
            "cross_tool",
            "Routing.",
            "CROSS-BODY",
            "[not_a_tool, ping]",
        )]);
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);
        assert!(server.prompts_snapshot().map.contains_key("cross_tool"));
        assert_eq!(active.len(), 1, "{active:?}");
        assert!(tool_desc(&server, "ping").contains("CROSS-BODY"));
    }

    #[test]
    fn serve_prompts_injects_description_under_when_to_use() {
        // The skill's `description` carries the TRIGGER/SKIP routing —
        // it must reach the live tool-description channel under a
        // `## When to use` header, ahead of the methodology body.
        let registry = build_registry_with_frontmatter(&[(
            "ping",
            "ROUTING-SENTINEL",
            "BODY-SENTINEL",
            "delivery: eager",
        )]);
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
        assert!(server.prompts_snapshot().map.contains_key("graph_strategy"));
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
            !server.prompts_snapshot().map.contains_key("gated_skill"),
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
            server.prompts_snapshot().map.contains_key("gated_skill"),
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
        assert!(!server.prompts_snapshot().map.contains_key("gated_skill"));

        // With the extension declared — registers.
        let mut extensions = serde_json::Map::new();
        extensions.insert("csv_http_server".to_string(), serde_json::json!(true));
        let opts = ServerOptions {
            extensions,
            ..ServerOptions::default()
        };
        let mut server = McpServer::new(opts);
        super::serve_prompts(&registry, &mut server);
        assert!(server.prompts_snapshot().map.contains_key("gated_skill"));
    }

    // ─── Delivery tiers ───────────────────────────────────────────

    #[test]
    fn lazy_skill_injects_routing_and_a_loader_pointer_but_no_body() {
        // Mutation: swap the tiers (`delivery: eager`) — the body
        // reappears in the description and both negative assertions
        // fail.
        let registry = build_registry_with_frontmatter(&[(
            "ping",
            "ROUTING-SENTINEL",
            "BODY-SENTINEL",
            "delivery: lazy\nreferences_tools: [grep]",
        )]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);

        for tool in ["ping", "grep"] {
            let desc = tool_desc(&server, tool);
            assert!(
                desc.contains("<!-- mcp-skill:ping -->"),
                "{tool} must carry the idempotency marker on the lazy tier: {desc}"
            );
            assert!(
                desc.contains("## When to use\n\nROUTING-SENTINEL"),
                "{tool} must carry the routing description: {desc}"
            );
            assert!(
                desc.contains("Load the full methodology with skill(\"ping\") before first use."),
                "{tool} must carry the loader pointer: {desc}"
            );
            assert!(
                !desc.contains("BODY-SENTINEL"),
                "{tool} must NOT carry the body on the lazy tier: {desc}"
            );
            assert!(
                !desc.contains("## Methodology"),
                "{tool} must NOT carry a Methodology block on the lazy tier: {desc}"
            );
        }
    }

    #[test]
    fn eager_skill_injects_the_body_and_no_loader_pointer() {
        // The other half of the swap mutation above.
        let registry = build_registry_with_frontmatter(&[(
            "ping",
            "ROUTING-SENTINEL",
            "BODY-SENTINEL",
            "delivery: eager",
        )]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let desc = tool_desc(&server, "ping");
        assert!(desc.contains("## Methodology\n\nBODY-SENTINEL"), "{desc}");
        assert!(!desc.contains("Load the full methodology"), "{desc}");
    }

    #[test]
    fn delivery_defaults_to_lazy_in_the_injection_pass() {
        // A SKILL.md with no `delivery:` key is lazy. Mutation: flip
        // `#[default]` on `Delivery` to `Eager` — the body reappears.
        let registry = build_test_registry(&[("ping", "Routing.", "BODY-SENTINEL", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);
        let desc = tool_desc(&server, "ping");
        assert!(!desc.contains("BODY-SENTINEL"), "{desc}");
        assert!(desc.contains("skill(\"ping\")"), "{desc}");
        assert_eq!(active[0].delivery, Delivery::Lazy);
    }

    #[test]
    fn serve_prompts_idempotent_across_repeated_passes_on_the_lazy_tier() {
        // The marker fences both tiers. Mutation: drop the
        // `already`-injected check — the second pass doubles the
        // routing block.
        let registry = build_test_registry(&[("ping", "Routing.", "Body.", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        let once = tool_desc(&server, "ping");
        super::serve_prompts(&registry, &mut server);
        let twice = tool_desc(&server, "ping");
        assert_eq!(once, twice);
        assert_eq!(once.matches("<!-- mcp-skill:ping -->").count(), 1, "{once}");
        assert_eq!(
            once.matches("Load the full methodology").count(),
            1,
            "{once}"
        );
    }

    // ─── The `skill(name)` loader tool ────────────────────────────

    #[test]
    fn skill_loader_is_registered_whenever_skills_are_on() {
        // Registered for a non-empty registry even with no lazy skill,
        // so an active-skills index can always point at it.
        let registry =
            build_registry_with_frontmatter(&[("ping", "Routing.", "Body.", "delivery: eager")]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        assert!(server
            .tools_snapshot()
            .get(super::SKILL_TOOL_NAME)
            .is_some());
    }

    #[test]
    fn skill_loader_absent_on_the_zero_skills_boot_path() {
        // The zero-skills contract: a server with no registry grows no
        // tools. Mutation: register the loader unconditionally.
        let registry = crate::server::skills::ResolvedRegistry::default();
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);
        assert!(active.is_empty());
        assert!(server
            .tools_snapshot()
            .get(super::SKILL_TOOL_NAME)
            .is_none());
    }

    #[test]
    fn a_skill_named_after_the_loader_is_rejected() {
        // Pathological: it would inject into the tool that fetches it.
        // Mutation: drop the guard — the route registers and the skill
        // becomes active.
        let registry = build_registry_with_refs(&[("skill", "Routing.", "Body.", "[ping]")]);
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);
        assert!(active.is_empty(), "{active:?}");
        assert!(!server.prompts_snapshot().map.contains_key("skill"));
        assert!(!tool_desc(&server, "ping").contains("<!-- mcp-skill:skill -->"));
    }

    #[test]
    fn a_downstream_tool_owning_the_loader_name_forces_eager_delivery() {
        // A routing line pointing at a tool that fetches something
        // else is the pre-0.3.37 failure this tier exists to avoid, so
        // the framework yields the name and ships the body instead.
        // Mutation: overwrite the downstream route — the pointer
        // appears and the body vanishes.
        let registry = build_test_registry(&[("ping", "Routing.", "BODY-SENTINEL", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        server.register_typed_tool("skill", "downstream tool", |args: EchoArgs| args.text);
        super::serve_prompts(&registry, &mut server);
        assert_eq!(
            tool_desc(&server, "skill"),
            "downstream tool",
            "the downstream tool must keep its own description"
        );
        let desc = tool_desc(&server, "ping");
        assert!(desc.contains("BODY-SENTINEL"), "{desc}");
        assert!(!desc.contains("Load the full methodology"), "{desc}");
    }

    // ─── Active-skills accessor ───────────────────────────────────

    #[test]
    fn active_skills_is_the_post_activation_set_with_tiers() {
        // Exactly the post-`applies_when`, post-target-check set.
        // Mutation: push to `active` before either gate — `suppressed`
        // and `unreachable` appear.
        use crate::server::skills::{Registry as SkillsBuilder, SkillProvenance};
        let dir = tempfile::tempdir().unwrap();
        let yaml = dir.path().join("test_mcp.yaml");
        std::fs::write(&yaml, "name: t\nskills: true\n").unwrap();
        let skills_dir = dir.path().join("test_mcp.skills");
        std::fs::create_dir(&skills_dir).unwrap();
        std::fs::write(
            skills_dir.join("lazy_one.md"),
            "---\nname: lazy_one\ndescription: Lazy routing.\nreferences_tools: [ping]\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("eager_one.md"),
            "---\nname: eager_one\ndescription: Eager routing.\ndelivery: eager\nreferences_tools: [grep]\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("suppressed.md"),
            "---\nname: suppressed\ndescription: d.\nreferences_tools: [ping]\napplies_when:\n  tool_registered: nope\n---\n\nBody.\n",
        )
        .unwrap();
        std::fs::write(
            skills_dir.join("unreachable.md"),
            "---\nname: unreachable\ndescription: d.\nreferences_tools: [nope]\n---\n\nBody.\n",
        )
        .unwrap();

        let registry = SkillsBuilder::new()
            .auto_detect_project_layer(&yaml)
            .finalise()
            .unwrap();
        let mut server = McpServer::new(ServerOptions::default());
        let active = super::serve_prompts(&registry, &mut server);

        assert_eq!(
            active,
            vec![
                super::ActiveSkill {
                    name: "eager_one".to_string(),
                    description: "Eager routing.".to_string(),
                    delivery: Delivery::Eager,
                    provenance: SkillProvenance::Project,
                },
                super::ActiveSkill {
                    name: "lazy_one".to_string(),
                    description: "Lazy routing.".to_string(),
                    delivery: Delivery::Lazy,
                    provenance: SkillProvenance::Project,
                },
            ]
        );
        assert_eq!(server.active_skills(), active);
    }

    // ─── Post-serve re-resolve ────────────────────────────────────

    /// Two eager skills landing on `ping`: the name-match one plus a
    /// cross-tool one that references it.
    fn two_skills_on_ping() -> crate::server::skills::ResolvedRegistry {
        build_registry_with_frontmatter(&[
            ("ping", "First routing.", "FIRST-BODY", "delivery: eager"),
            (
                "helper",
                "Second routing.",
                "SECOND-BODY",
                "references_tools: [ping]\ndelivery: eager",
            ),
        ])
    }

    #[test]
    fn strip_returns_a_two_skill_description_to_its_original_bytes() {
        // Mutation: strip only the first block (drop the `while` loop
        // in `strip_injected_skills` down to one `if`) — the second
        // skill's marker, routing and body stay behind and the
        // byte-for-byte assertion fails.
        let mut server = McpServer::new(ServerOptions::default());
        let original = tool_desc(&server, "ping");
        assert!(
            !original.contains(SKILL_MARKER_OPEN),
            "precondition: a bare `ping` carries no injection: {original}"
        );

        super::serve_prompts(&two_skills_on_ping(), &mut server);
        let injected = tool_desc(&server, "ping");
        assert_eq!(
            injected.matches(SKILL_MARKER_OPEN).count(),
            2,
            "precondition: both skills must have injected: {injected}"
        );
        assert!(injected.contains("FIRST-BODY") && injected.contains("SECOND-BODY"));

        let empty = crate::server::skills::ResolvedRegistry::default();
        server.reinject_skills(&empty).unwrap();
        assert_eq!(
            tool_desc(&server, "ping"),
            original,
            "a re-resolve must return the description to its pre-injection bytes"
        );
    }

    #[test]
    fn strip_handles_a_description_that_was_nothing_but_injection() {
        // A dynamic tool registered with an empty description ends up
        // with the marker at byte 0 and no `\n\n` in front of it;
        // stripping must produce `None`, not a lone blank line.
        assert_eq!(
            super::strip_injected_skills("<!-- mcp-skill:a -->\n\n## When to use\n\nx"),
            None
        );
        assert_eq!(super::strip_injected_skills("kept"), Some("kept".into()));
    }

    #[test]
    fn re_resolve_adds_a_skill() {
        // Mutation: make `reinject_skills` return early before phase B
        // — `grep` never grows its pointer and the prompt route never
        // appears.
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(
            &build_test_registry(&[("ping", "Ping routing.", "PING-BODY", true)]),
            &mut server,
        );
        assert!(!server.prompts_snapshot().map.contains_key("grep"));

        let grown = build_test_registry(&[
            ("ping", "Ping routing.", "PING-BODY", true),
            ("grep", "Grep routing.", "GREP-BODY", true),
        ]);
        let active = server.reinject_skills(&grown).unwrap();

        assert!(server.prompts_snapshot().map.contains_key("grep"));
        let desc = tool_desc(&server, "grep");
        assert!(desc.contains("<!-- mcp-skill:grep -->"), "{desc}");
        assert!(
            desc.contains("Load the full methodology with skill(\"grep\") before first use."),
            "{desc}"
        );
        assert!(!desc.contains("GREP-BODY"), "still the lazy tier: {desc}");
        let names: Vec<&str> = active.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["grep", "ping"]);
    }

    #[test]
    fn re_resolve_removes_a_skill() {
        // Mutation: keep the old prompt routes (drop the
        // `previous_prompts` removal) — `grep` stays in prompts/list
        // after the registry stopped declaring it.
        let mut server = McpServer::new(ServerOptions::default());
        let grep_before = tool_desc(&server, "grep");
        super::serve_prompts(
            &build_test_registry(&[
                ("ping", "Ping routing.", "PING-BODY", true),
                ("grep", "Grep routing.", "GREP-BODY", true),
            ]),
            &mut server,
        );
        assert!(server.prompts_snapshot().map.contains_key("grep"));

        let shrunk = build_test_registry(&[("ping", "Ping routing.", "PING-BODY", true)]);
        let active = server.reinject_skills(&shrunk).unwrap();

        assert!(
            !server.prompts_snapshot().map.contains_key("grep"),
            "a skill the new registry does not declare must leave prompts/list"
        );
        assert_eq!(tool_desc(&server, "grep"), grep_before);
        assert_eq!(active.len(), 1, "{active:?}");
        assert_eq!(server.active_skills(), active);
    }

    #[test]
    fn re_resolve_keeps_a_downstream_prompt_route() {
        // Only the routes this module registered are dropped. Mutation:
        // clear the whole prompt router instead of removing
        // `skill_prompts` — `downstream` disappears.
        let mut server = McpServer::new(ServerOptions::default());
        server.prompt_router_mut().add_route(PromptRoute::new_dyn(
            Prompt::new("downstream", Some("Not ours."), None),
            |_ctx| {
                Box::pin(async {
                    Ok(
                        GetPromptResult::new(vec![PromptMessage::new_text(
                            Role::Assistant,
                            "body",
                        )])
                        .into(),
                    )
                })
            },
        ));
        super::serve_prompts(
            &build_test_registry(&[("ping", "Ping routing.", "PING-BODY", true)]),
            &mut server,
        );
        server
            .reinject_skills(&crate::server::skills::ResolvedRegistry::default())
            .unwrap();
        assert!(
            server.prompts_snapshot().map.contains_key("downstream"),
            "a route added through prompt_router_mut is not the skill pass's to remove"
        );
    }

    #[test]
    fn the_skill_loader_survives_a_re_resolve_exactly_once() {
        // Mutation: register the loader with a fresh name on every
        // pass (`skill`, `skill_`, …) — the count goes to two.
        let mut server = McpServer::new(ServerOptions::default());
        let registry = build_test_registry(&[("ping", "Routing.", "BODY", true)]);
        super::serve_prompts(&registry, &mut server);
        server.reinject_skills(&registry).unwrap();
        let loaders = server
            .tools_snapshot()
            .list_all()
            .into_iter()
            .filter(|t| t.name == super::SKILL_TOOL_NAME)
            .count();
        assert_eq!(loaders, 1);

        // And it goes away when the registry does: answering from a
        // map that no longer matches any declared skill is worse than
        // not answering.
        server
            .reinject_skills(&crate::server::skills::ResolvedRegistry::default())
            .unwrap();
        assert!(server
            .tools_snapshot()
            .get(super::SKILL_TOOL_NAME)
            .is_none());
    }

    #[test]
    fn a_downstream_tool_named_skill_still_owns_the_name_after_a_re_resolve() {
        let mut server = McpServer::new(ServerOptions::default());
        server.register_typed_tool("skill", "downstream tool", |args: EchoArgs| args.text);
        let registry = build_test_registry(&[("ping", "Routing.", "BODY-SENTINEL", true)]);
        super::serve_prompts(&registry, &mut server);
        server.reinject_skills(&registry).unwrap();
        assert_eq!(tool_desc(&server, "skill"), "downstream tool");
        assert!(tool_desc(&server, "ping").contains("BODY-SENTINEL"));
    }

    #[test]
    fn re_resolve_is_byte_stable_when_the_registry_is_unchanged() {
        // Strip-then-inject must be a round trip, not a slow drift.
        let mut server = McpServer::new(ServerOptions::default());
        let registry = two_skills_on_ping();
        super::serve_prompts(&registry, &mut server);
        let once = tool_desc(&server, "ping");
        server.reinject_skills(&registry).unwrap();
        server.reinject_skills(&registry).unwrap();
        assert_eq!(once, tool_desc(&server, "ping"));
    }

    #[test]
    fn get_info_advertises_list_changed_on_tools_always_and_on_prompts_with_them() {
        // Mutation: drop `.enable_tool_list_changed()` — a client has
        // no reason to re-fetch after `notifications/tools/list_changed`
        // and the rebuilt surface never reaches it.
        let bare = McpServer::new(ServerOptions::default());
        let caps = bare.get_info().capabilities;
        assert_eq!(caps.tools.as_ref().and_then(|t| t.list_changed), Some(true));
        assert!(
            caps.prompts.is_none(),
            "the zero-skills path still advertises no prompts capability"
        );

        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(
            &build_test_registry(&[("ping", "Routing.", "BODY", true)]),
            &mut server,
        );
        let caps = server.get_info().capabilities;
        assert_eq!(
            caps.prompts.as_ref().and_then(|p| p.list_changed),
            Some(true)
        );
    }

    #[test]
    fn a_rebuild_from_inside_a_result_postprocess_hook_is_refused_by_name() {
        // The hook has no `Peer`, so a rebuild there would change the
        // surface with no way to announce it. Refused rather than
        // performed — and the refusal must *return*, which is what the
        // timeout below checks.
        //
        // Mutation: drop the `inside_no_rebuild_region()` check at the
        // top of `resolve_skills` — the hook's call succeeds, `refused`
        // is empty and the first assertion fails.
        use std::sync::OnceLock;
        let reloader: Arc<OnceLock<SkillReloader>> = Arc::new(OnceLock::new());
        let refused: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hook = {
            let reloader = reloader.clone();
            let refused = refused.clone();
            Arc::new(
                move |_tool: &str,
                      _args: &serde_json::Value,
                      _body: &str,
                      _ctx: &ResultCtx|
                      -> Option<String> {
                    if let Some(handle) = reloader.get() {
                        let empty = crate::server::skills::ResolvedRegistry::default();
                        match handle.reinject_skills(&empty) {
                            Ok(_) => refused.lock().unwrap().push("UNEXPECTED OK".to_string()),
                            Err(e) => refused.lock().unwrap().push(e),
                        }
                    }
                    Some("HOOK-FOOTER".to_string())
                },
            )
        };
        let mut server =
            McpServer::new(ServerOptions::default().with_result_postprocess(hook.clone()));
        super::serve_prompts(
            &build_test_registry(&[("ping", "Routing.", "BODY", true)]),
            &mut server,
        );
        let _ = reloader.set(server.skill_reloader());

        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let server = server.clone();
            std::thread::spawn(move || {
                let _ = tx.send(server.finish("ping", &serde_json::json!({}), "body".to_string()));
            })
        };
        let finished = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the postprocess hook never returned: a refused rebuild must not block");
        worker.join().unwrap();

        let refused = refused.lock().unwrap();
        assert_eq!(refused.len(), 1, "the hook must have run: {refused:?}");
        assert!(
            refused[0].starts_with("reinject_skills was called from inside"),
            "the refusal must name the entry point, got: {}",
            refused[0]
        );
        assert!(finished.contains("HOOK-FOOTER"), "{finished}");
        // The refusal is local to the hook: the skill layer is
        // untouched, so `ping` still carries what the pass injected.
        assert!(tool_desc(&server, "ping").contains("<!-- mcp-skill:ping -->"));
    }

    // ─── Session activity and the loaded set ──────────────────────

    /// A minimal JSON-RPC client over a duplex transport.
    ///
    /// `tests/lazy_skills.rs` has the same shape and is where the rest
    /// of the session-scoped behaviour is tested. This copy lives
    /// in-crate because the test below has to shorten
    /// `LoadedSkills::ttl`, a private field an integration test cannot
    /// reach — and shortening it is the only way to observe a
    /// ten-minute window without waiting ten minutes.
    struct Rpc {
        read: tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
        write: tokio::io::WriteHalf<tokio::io::DuplexStream>,
        id: usize,
    }

    impl Rpc {
        async fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            self.id += 1;
            let message =
                serde_json::json!({"jsonrpc":"2.0","id":self.id,"method":method,"params":params});
            self.write
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
            loop {
                let mut line = String::new();
                let read = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    self.read.read_line(&mut line),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(read > 0, "server disconnected");
                let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
                if frame["id"] == self.id {
                    return frame;
                }
            }
        }

        /// The text of a tool call's first content block.
        async fn call(&mut self, name: &str) -> String {
            let frame = self
                .request(
                    "tools/call",
                    serde_json::json!({"name":name,"arguments":{}}),
                )
                .await;
            frame["result"]["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("no text content in {frame}"))
                .to_string()
        }
    }

    async fn serve_over_duplex(server: McpServer) -> Rpc {
        use rmcp::ServiceExt;
        use tokio::io::AsyncWriteExt;
        let (ours, theirs) = tokio::io::duplex(1024 * 1024);
        tokio::spawn(async move {
            let service = server.serve(ours).await.unwrap();
            let _ = service.waiting().await;
        });
        let (read, write) = tokio::io::split(theirs);
        let mut client = Rpc {
            read: tokio::io::BufReader::new(read),
            write,
            id: 0,
        };
        let init = client
            .request(
                "initialize",
                serde_json::json!({"protocolVersion":"2024-11-05","capabilities":{},
                                   "clientInfo":{"name":"ttl","version":"1"}}),
            )
            .await;
        assert!(init.get("error").is_none(), "{init}");
        client
            .write
            .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
            .await
            .unwrap();
        client
    }

    #[tokio::test]
    async fn a_working_session_keeps_its_loaded_skills_and_an_idle_one_loses_them() {
        // Expiry is keyed on session *activity*, not on when the last
        // `skill()` happened. Mutation: drop the `loaded_skills.touch`
        // at the top of `call_tool` — `touched` only moves inside
        // `mark`, the record ages out while the agent is still working,
        // and the in-loop assertion fires.
        const TTL: std::time::Duration = std::time::Duration::from_millis(300);
        let registry = build_test_registry(&[("ping", "Routing.", "BODY", true)]);
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(&registry, &mut server);
        server
            .loaded_skills
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ttl = TTL;
        let mut client = serve_over_duplex(server).await;

        const NUDGE: &str = "has not been loaded this session";
        assert!(client.call("ping").await.contains(NUDGE));
        client
            .request(
                "tools/call",
                serde_json::json!({"name":"skill","arguments":{"name":"ping"}}),
            )
            .await;
        assert!(!client.call("ping").await.contains(NUDGE));

        // Work continuously for well over the window, never calling
        // `skill()` again. The loaded set must survive all of it.
        let started = std::time::Instant::now();
        while started.elapsed() < TTL * 3 {
            tokio::time::sleep(TTL / 6).await;
            let body = client.call("ping").await;
            assert!(
                !body.contains(NUDGE),
                "a session that never stopped calling tools must keep what it loaded \
                 ({:?} in): {body}",
                started.elapsed()
            );
        }

        // Go quiet for longer than the window: now the record is gone
        // and the next call is nudged again.
        tokio::time::sleep(TTL * 2).await;
        let body = client.call("ping").await;
        assert!(
            body.contains(NUDGE),
            "a session idle past the window must start empty: {body}"
        );
    }

    #[test]
    fn a_reloader_outliving_its_server_reports_that_instead_of_panicking() {
        let mut server = McpServer::new(ServerOptions::default());
        super::serve_prompts(
            &build_test_registry(&[("ping", "Routing.", "BODY", true)]),
            &mut server,
        );
        let reloader = server.skill_reloader();
        assert!(reloader.is_live());
        drop(server);
        assert!(!reloader.is_live());
        let err = reloader
            .reinject_skills(&crate::server::skills::ResolvedRegistry::default())
            .unwrap_err();
        assert!(err.contains("has been dropped"), "{err}");
    }
}
