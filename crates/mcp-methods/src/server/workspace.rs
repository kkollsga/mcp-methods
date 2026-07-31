//! Workspace mode — two variants.
//!
//! **Github mode** (`Workspace::open`, the default when
//! `--workspace DIR` is set): the agent activates a GitHub repo via
//! `repo_management('org/repo')`, the binary clones it into the
//! workspace, and the active repo becomes the bound source root for
//! `read_source` / `grep` / `list_source`. Idle repos auto-sweep after
//! `--stale-after-days`. Layout:
//!   workspace/
//!     repos/<org>/<repo>/         — cloned source
//!     inventory.json              — per-repo access tracking
//!
//! **Local mode** (`Workspace::open_local`, the manifest-driven
//! `workspace: { kind: local, root: ... }` variant): the active source
//! root is a fixed local directory, not a clone target. `repo_management`
//! reports the active root and triggers rebuilds; an `set_root_dir`
//! tool can swap the root at runtime. Closes the `code_review_mcp_server`
//! use case from the kglite wishlist.
//!
//! Both modes share one activation state machine. Existing consumers may use
//! the serialized [`PostActivateHook`] callback family; concurrency-aware
//! consumers use [`ActivationTransactionHook`] to prepare off-lock and publish
//! only while their request generation is current. Both honour the same
//! `last_built_sha` gating to skip pointless rebuilds.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Repo name format: ``org/repo``. Letters, digits, dots, hyphens, underscores.
fn validate_repo_name(name: &str) -> Result<()> {
    let mut parts = name.split('/');
    let org = parts.next().unwrap_or("");
    let repo = parts.next().unwrap_or("");
    if parts.next().is_some() || org.is_empty() || repo.is_empty() {
        return Err(anyhow!(
            "Invalid repo name {name:?}. Expected 'org/repo' (exactly one slash)."
        ));
    }
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    if !valid(org) || !valid(repo) {
        return Err(anyhow!(
            "Invalid repo name {name:?}. Letters/digits/dots/hyphens/underscores only."
        ));
    }
    Ok(())
}

/// Hook fired after a successful clone or update. Receives the absolute
/// path to the cloned repo and the org/repo name. Legacy callback activations
/// are serialized through summary generation. Errors abort publication of the
/// framework's new active source state; use [`ActivationTransactionHook`] when
/// downstream product installation must also be deferred until commit.
pub type PostActivateHook = Arc<dyn Fn(&Path, &str) -> Result<()> + Send + Sync>;

/// Optional hook that returns a short agent-facing summary appended to
/// the activation result message — the "graph ready" mini-map / opening
/// steer (e.g. `"Graph ready: 9,999 Functions · 656 Classes · 31k CALLS.
/// Open with graph_overview() → cypher_query; grep = literal text only."`).
///
/// Kept separate from [`PostActivateHook`] so adding it is a non-breaking
/// addition — existing consumers that register only the build hook are
/// unaffected. Receives the repo path + name; returns `Some(text)` to
/// append (blank-line separated), or `None` for the terse default
/// message. Called after a successful activation (skipped when the build
/// hook failed).
pub type ActivationSummaryHook = Arc<dyn Fn(&Path, &str) -> Option<String> + Send + Sync>;

/// Hook fired after a successful clone/update **when revisions were
/// requested** on the activation call (`repo_management(revs=…)` /
/// `set_root_dir(revs=…)`). Receives the repo path, the `org/repo` (or
/// synthetic local) name, and the resolved revspecs in **oldest→newest**
/// order — for a `Count(n)` request the final entry is always `HEAD`, so
/// a downstream multi-rev builder can merge oldest→newest with HEAD's
/// signature winning. Set via [`Workspace::with_post_activate_revs`].
///
/// Additive by design (mirrors [`ActivationSummaryHook`]): existing
/// consumers that register only the plain [`PostActivateHook`] are
/// unaffected. When revs are requested but this hook is *not* set, the
/// plain hook runs instead (a single-rev / HEAD build) and the resolved
/// list is not reported in the activation message.
pub type PostActivateRevsHook = Arc<dyn Fn(&Path, &str, &[String]) -> Result<()> + Send + Sync>;

/// Monotonically increasing identity for one workspace activation request.
///
/// Identities are allocated before an activation mutates active source state.
/// A higher identity is therefore newer intent; a prepared activation may
/// commit only while its identity is still the latest requested one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ActivationId(u64);

impl ActivationId {
    /// The process-local monotonically increasing integer value.
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ActivationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Work required for a request-scoped activation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivationBuild {
    /// Build the root at its current working-tree / HEAD state.
    Plain,
    /// Build the already-resolved revisions in oldest-to-newest order.
    Revisions(Vec<String>),
    /// Reuse the product already live for this root; only refresh its summary.
    Reuse,
}

/// Immutable input to an [`ActivationTransactionHook`].
///
/// The hook may perform expensive preparation before returning. It must not
/// publish the prepared product itself; publication belongs in the
/// [`PreparedActivation`] closure so the framework can discard stale work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationRequest {
    id: ActivationId,
    path: PathBuf,
    name: String,
    build: ActivationBuild,
}

impl ActivationRequest {
    pub fn id(&self) -> ActivationId {
        self.id
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn build(&self) -> &ActivationBuild {
        &self.build
    }
}

/// Prepared downstream activation that has not yet been published.
///
/// The closure should atomically install the prepared product and return the
/// summary describing that exact product. The framework runs it only when the
/// request is still current, under the same generation boundary used to
/// publish active source and built identity. Dropping this value must be safe:
/// stale requests are superseded by dropping their prepared activation. The
/// closure runs while workspace activation state is write-locked, so it must
/// not call back into [`Workspace`] accessors; keep it to the downstream slot
/// swap and request-scoped summary generation.
pub struct PreparedActivation {
    commit: Box<dyn FnOnce() -> Result<Option<String>> + Send + 'static>,
}

impl PreparedActivation {
    pub fn new<F>(commit: F) -> Self
    where
        F: FnOnce() -> Result<Option<String>> + Send + 'static,
    {
        Self {
            commit: Box::new(commit),
        }
    }

    /// A prepared activation with no publication side effect.
    pub fn summary(summary: Option<String>) -> Self {
        Self::new(move || Ok(summary))
    }

    fn commit(self) -> Result<Option<String>> {
        (self.commit)()
    }
}

/// Request-scoped activation transaction.
///
/// Preparation runs concurrently and off-lock. The returned
/// [`PreparedActivation`] is committed only if this request remains the latest
/// intent; otherwise it is dropped and the caller receives a superseded
/// outcome. This single callback replaces the legacy plain/revisions/summary
/// trio for consumers that need coherent concurrent activation.
pub type ActivationTransactionHook =
    Arc<dyn Fn(&ActivationRequest) -> Result<PreparedActivation> + Send + Sync>;

/// A revisions request carried by the activation tools. `Count(n)`
/// resolves to the newest `n` **stable release** tags of the repo's
/// dominant tag family (plus `HEAD`); `List(revs)` is an explicit set of
/// git revspecs used verbatim. The untagged deserialization maps a JSON
/// integer to `Count` and a JSON array of strings to `List`, so the tool
/// arg accepts `int | [str]`. Resolution happens at activate time — see
/// [`Workspace::resolve_revs`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum RevsRequest {
    /// Last `n` **stable** release tags of the repo's dominant tag family,
    /// ordered oldest→newest with `HEAD` appended. Tags are classified
    /// into `(prefix, version, is_prerelease)` and grouped by prefix; the
    /// family with the most stable tags wins, so on a repo with several
    /// tag families (e.g. `apache-arrow-*`, `go/v*`, `r-*`) the release
    /// line is chosen, not an unrelated package family. Prereleases (rc,
    /// alpha, beta, dev, pre, preview) and non-version tags (e.g.
    /// `r-universe-release`) are excluded. See [`Workspace::resolve_revs`]
    /// for the full selection + fallback semantics.
    Count(usize),
    /// Explicit git revspecs (tags, branches, or SHAs), used as given.
    List(Vec<String>),
}

/// Per-repo inventory entry persisted in `inventory.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct InventoryEntry {
    cloned_at: String,
    last_accessed: String,
    #[serde(default)]
    access_count: u64,
    #[serde(default)]
    stale: bool,
    /// HEAD SHA at the time the post-activate hook last completed
    /// successfully. Drives auto-rebuild gating: when an `update=True`
    /// call ends with `action=="current"` AND the new HEAD matches this,
    /// the post-activate hook can be skipped. `serde(default)` keeps
    /// older inventory.json files (without this field) loading cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_built_sha: Option<String>,
    /// The revisions request last **successfully** built for this repo,
    /// when that build was a multi-rev (`revs=`) activation via the
    /// revs-aware hook. `None` when the last build was a plain
    /// (single-rev / HEAD) activation, or the revs hook was absent (the
    /// plain-hook fallback loads HEAD only, so it records no request).
    /// Two jobs: (1) the skip gate refuses to skip a plain re-activation
    /// when the last build was multi-rev (else the tool would report a
    /// plain activation while the live product is still the rev-set);
    /// (2) `update=True` with no explicit `revs` re-applies this stored
    /// request (re-resolving it so `HEAD`/`Count(n)` re-point). Additive:
    /// `serde(default)` keeps older inventory.json files (without this
    /// field) loading cleanly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_built_revs: Option<RevsRequest>,
}

// `WorkspaceKind` is re-used from the manifest module so config and
// runtime share one enum — the values mean the same thing.
pub use crate::server::manifest::WorkspaceKind;

/// Who chose the currently active root.
///
/// The flag exists so a root proposed by an *external party* (an MCP
/// client advertising `roots`, see [`Workspace::adopt_client_root`])
/// can never silently displace one the operator chose. It is a
/// precedence marker, not an access-control mechanism — containment is
/// [`Workspace::with_sandbox_root`]'s job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootOwnership {
    /// No root is bound and nobody has claimed one. Only an unanchored
    /// local boot ([`Workspace::open_local_unanchored`]) starts here.
    Unowned,
    /// The active root came from a client-advertised MCP root. A later
    /// `roots/list_changed` may replace it.
    Adopted,
    /// The active root came from the operator — manifest `workspace.root`,
    /// a CLI flag, or an explicit `set_root_dir` call. **Permanent**: no
    /// client advertisement ever overrides it.
    Operator,
}

/// Workspace runtime state. Shared across MCP request clones via Arc.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<WorkspaceInner>,
}

struct WorkspaceInner {
    kind: WorkspaceKind,
    workspace_dir: PathBuf,
    stale_after_days: u32,
    state: RwLock<WorkspaceState>,
    /// Serializes inventory read-modify-write cycles across concurrent
    /// activation preparations so per-repo SHA/revision receipts are not
    /// lost when two requests finish close together.
    inventory: Mutex<()>,
    /// Serializes the legacy callback trio, whose signatures cannot carry a
    /// request id or defer publication. Transaction-hook activations do not
    /// take this lock: they prepare concurrently and commit by generation.
    legacy_activation: Mutex<()>,
    post_activate: Option<PostActivateHook>,
    /// Optional summary hook (see [`ActivationSummaryHook`]). Set via
    /// [`Workspace::with_activation_summary`], `None` by default.
    activation_summary: Option<ActivationSummaryHook>,
    /// Optional revs-aware hook (see [`PostActivateRevsHook`]). Set via
    /// [`Workspace::with_post_activate_revs`], `None` by default. Called
    /// in place of `post_activate` only when the activation carried a
    /// revs request AND this hook is set.
    post_activate_revs: Option<PostActivateRevsHook>,
    /// Request-scoped prepare/commit contract. When configured it replaces
    /// the legacy callback trio for activation work and summary generation.
    activation_transaction: Option<ActivationTransactionHook>,
    /// Optional outer containment boundary for runtime root swaps, stored
    /// **canonicalized**. Set via [`Workspace::with_sandbox_root`] (manifest
    /// key `workspace.sandbox_root`), `None` by default.
    ///
    /// `None` means unbounded — [`Workspace::set_root_dir`] accepts any
    /// directory, which is the historical behaviour. When `Some`, a swap
    /// target whose canonical path is not inside this directory is rejected
    /// before any state is touched.
    sandbox_root: Option<PathBuf>,
    /// Who owns the active root (see [`RootOwnership`]). `Operator` for
    /// every workspace opened with a configured root; `Unowned` only
    /// after [`Workspace::open_local_unanchored`].
    ///
    /// Every **writer** also holds [`root_swap`](Self::root_swap), so the
    /// value is stable for as long as that guard is held. This lock exists
    /// separately only so the cheap [`Workspace::root_ownership`] read
    /// never queues behind a root swap's activation.
    root_ownership: Mutex<RootOwnership>,
    /// Orders a client-root adoption against operator root swaps.
    ///
    /// **Read** = an operator swap ([`Workspace::set_root_dir`]);
    /// **write** = an adoption ([`Workspace::adopt_client_root`]).
    ///
    /// Operator swaps deliberately still overlap *each other* — two
    /// concurrent `set_root_dir` calls are made coherent by activation
    /// generations (the newer request supersedes the older one), not by
    /// exclusion. An adoption cannot use that mechanism, because its
    /// precedence rule is not "newest wins" but "the operator always
    /// wins", and that rule spans three steps: read the ownership flag,
    /// swap, publish the flag. Holding the write side across all three
    /// makes those steps atomic with respect to any operator swap.
    ///
    /// Without it the two entry points interleave as: adoption passes its
    /// ownership check → the operator swaps to B → adoption's activation
    /// commits A last → adoption publishes `Adopted` → the operator
    /// publishes `Operator`. Final state: the *client's* root active,
    /// flagged as the operator's, after the operator's tool call already
    /// returned naming a different path.
    root_swap: RwLock<()>,
    /// Local mode, unanchored boot only: the directory that ends up
    /// holding `.mcp-workspace/`, chosen at the first activation that
    /// **commits** and then fixed forever (the anchored constructors
    /// decide it up front and leave this unset). Fixed-forever mirrors
    /// the anchored rule that `workspace_dir` survives root swaps so the
    /// inventory does too — see the note in [`Workspace::set_root_dir`].
    /// Set by [`Workspace::anchor_inventory`], never by a mere attempt.
    deferred_anchor: OnceLock<PathBuf>,
    /// Opt-in (`workspace.adopt_client_roots`): may this server adopt a
    /// root advertised by the MCP client as a *fallback* when the
    /// operator configured none? `false` by default, and the only thing
    /// that makes the `roots/list` round-trip happen at all.
    adopt_client_roots: bool,
}

#[derive(Debug, Default)]
struct WorkspaceState {
    active_repo_name: Option<String>,
    active_repo_path: Option<PathBuf>,
    /// Last identity allocated under this state lock. Allocation and
    /// publication both use the lock, so an older completion can never
    /// overwrite a newer request's intent.
    last_activation_id: u64,
    latest_requested: Option<ActivationId>,
    /// The product currently live in this process. Deliberately not
    /// persisted: a new process must rehydrate its downstream product even
    /// when inventory says the source SHA was built previously.
    active_build: Option<ActiveBuildState>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveBuildState {
    activation_id: ActivationId,
    name: String,
    path: PathBuf,
    head_sha: String,
    resolved_revs: Option<Vec<String>>,
}

impl Workspace {
    /// Open a github-flavoured workspace (clone + track flow).
    pub fn open(
        workspace_dir: PathBuf,
        stale_after_days: u32,
        post_activate: Option<PostActivateHook>,
    ) -> Result<Self> {
        if !workspace_dir.is_dir() {
            fs::create_dir_all(&workspace_dir).with_context(|| {
                format!("failed to create workspace dir {}", workspace_dir.display())
            })?;
        }
        let repos_dir = workspace_dir.join("repos");
        if !repos_dir.is_dir() {
            fs::create_dir_all(&repos_dir)
                .with_context(|| format!("failed to create repos dir {}", repos_dir.display()))?;
        }
        let ws = Self {
            inner: Arc::new(WorkspaceInner {
                kind: WorkspaceKind::Github,
                workspace_dir,
                stale_after_days,
                state: RwLock::new(WorkspaceState::default()),
                inventory: Mutex::new(()),
                legacy_activation: Mutex::new(()),
                post_activate,
                activation_summary: None,
                post_activate_revs: None,
                activation_transaction: None,
                sandbox_root: None,
                root_ownership: Mutex::new(RootOwnership::Operator),
                root_swap: RwLock::new(()),
                deferred_anchor: OnceLock::new(),
                adopt_client_roots: false,
            }),
        };
        ws.reconcile_inventory()?;
        Ok(ws)
    }

    /// Open a local-directory workspace.
    ///
    /// Binds `root` as the active source root immediately and fires the
    /// post-activate hook (subject to last-built-sha gating). `inventory.json`
    /// is kept under `<root>/.mcp-workspace/` so the local mode mirrors
    /// the same gating / fingerprinting infra without polluting the
    /// user's tree with a `repos/` directory.
    pub fn open_local(root: PathBuf, post_activate: Option<PostActivateHook>) -> Result<Self> {
        if !root.is_dir() {
            anyhow::bail!(
                "local workspace root does not exist or is not a directory: {}",
                root.display()
            );
        }
        let canon_root = root
            .canonicalize()
            .with_context(|| format!("failed to canonicalize local root {}", root.display()))?;
        // Store inventory under a hidden subdir so we don't litter the
        // user's repo. The "workspace dir" for local mode IS the root.
        let inv_dir = canon_root.join(".mcp-workspace");
        if !inv_dir.is_dir() {
            fs::create_dir_all(&inv_dir).with_context(|| {
                format!("failed to create local-workspace dir {}", inv_dir.display())
            })?;
        }
        let mut state = WorkspaceState::default();
        let synthetic_name = synthesize_local_name(&canon_root);
        state.active_repo_name = Some(synthetic_name);
        state.active_repo_path = Some(canon_root.clone());
        Ok(Self {
            inner: Arc::new(WorkspaceInner {
                kind: WorkspaceKind::Local,
                workspace_dir: canon_root,
                stale_after_days: u32::MAX, // sweeping is github-only
                state: RwLock::new(state),
                inventory: Mutex::new(()),
                legacy_activation: Mutex::new(()),
                post_activate,
                activation_summary: None,
                post_activate_revs: None,
                activation_transaction: None,
                sandbox_root: None,
                // A configured root is the operator's choice, so it is
                // `Operator` from the first instant — that is exactly what
                // makes client-root adoption fallback-only.
                root_ownership: Mutex::new(RootOwnership::Operator),
                root_swap: RwLock::new(()),
                deferred_anchor: OnceLock::new(),
                adopt_client_roots: false,
            }),
        })
    }

    /// Open a local-directory workspace with **no active root**.
    ///
    /// The unanchored sibling of [`open_local`](Self::open_local), for the
    /// case where the root is expected to arrive later from an MCP client
    /// (`workspace.adopt_client_roots`, see
    /// [`adopt_client_root`](Self::adopt_client_root)). Nothing is bound,
    /// nothing is created on disk, and no hook fires: until something
    /// activates, [`active_repo_path`](Self::active_repo_path) is `None`
    /// and the source tools behave exactly as they do with no root — which
    /// is the designed outcome when no root ever arrives.
    ///
    /// The inventory directory is deliberately *not* created here: with no
    /// root there is nowhere to put it. The first activation that
    /// **commits** picks the directory (`<root>/.mcp-workspace/`) and it is
    /// fixed from then on, mirroring the anchored mode's rule that the
    /// inventory survives later root swaps. An activation that fails its
    /// build picks nothing and creates nothing — a client-proposed root
    /// that never activated must not end up owning the inventory, nor
    /// gain a directory it did not have.
    ///
    /// [`root_ownership`](Self::root_ownership) starts at
    /// [`RootOwnership::Unowned`] — the only constructor that does.
    pub fn open_local_unanchored(post_activate: Option<PostActivateHook>) -> Result<Self> {
        Ok(Self {
            inner: Arc::new(WorkspaceInner {
                kind: WorkspaceKind::Local,
                // Sentinel: no directory is known yet. `inventory_dir()`
                // treats an empty base as "unanchored" and skips inventory
                // I/O entirely until `deferred_anchor` is set.
                workspace_dir: PathBuf::new(),
                stale_after_days: u32::MAX, // sweeping is github-only
                state: RwLock::new(WorkspaceState::default()),
                inventory: Mutex::new(()),
                legacy_activation: Mutex::new(()),
                post_activate,
                activation_summary: None,
                post_activate_revs: None,
                activation_transaction: None,
                sandbox_root: None,
                root_ownership: Mutex::new(RootOwnership::Unowned),
                root_swap: RwLock::new(()),
                deferred_anchor: OnceLock::new(),
                adopt_client_roots: false,
            }),
        })
    }

    /// Allow this workspace to adopt a client-advertised MCP root as a
    /// fallback when the operator configured none (manifest key
    /// `workspace.adopt_client_roots`).
    ///
    /// Off by default. With it off no `roots/list` request is ever issued,
    /// which is what keeps clients that do not advertise roots — and
    /// deployments that never opt in — bit-for-bit unaffected.
    ///
    /// Setting it on a workspace that already has a root is harmless and
    /// intentional: ownership is already [`RootOwnership::Operator`], so
    /// adoption is refused. Call before the workspace is cloned into
    /// [`crate::server::ServerOptions`], like the other builders.
    ///
    /// <div class="warning">
    ///
    /// MCP `roots` is **deprecated** as of protocol revision `2026-07-28`
    /// ([SEP-2577]) and is eligible for removal in the first revision
    /// released on or after 2027-07-28. New deployments should prefer the
    /// spec's own migration path — pass directories via tool parameters,
    /// resource URIs, or server configuration (`workspace.root`).
    ///
    /// [SEP-2577]: https://github.com/modelcontextprotocol/modelcontextprotocol/pull/2577
    ///
    /// </div>
    pub fn with_adopt_client_roots(mut self) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.adopt_client_roots = true,
            None => tracing::warn!(
                "with_adopt_client_roots called after the workspace was cloned; client roots will not be adopted"
            ),
        }
        self
    }

    /// Is client-root adoption enabled (`workspace.adopt_client_roots`)?
    pub fn adopts_client_roots(&self) -> bool {
        self.inner.adopt_client_roots
    }

    /// Who chose the active root — see [`RootOwnership`].
    ///
    /// Never blocks on an in-flight activation: the flag has a lock of its
    /// own, distinct from the swap ordering lock that a root swap holds
    /// across its (arbitrarily long) build.
    pub fn root_ownership(&self) -> RootOwnership {
        *self.lock_ownership()
    }

    /// Lock the root-ownership flag, recovering from poisoning.
    ///
    /// The guarded value is one `Copy` enum written by a single
    /// assignment, so a panic elsewhere under the guard cannot leave it
    /// half-updated — there is nothing torn to protect against.
    /// Recovering matters because these guards are now reachable from a
    /// request handler and from the client-`roots` task: a panicking
    /// post-activate hook must not turn every later root operation
    /// (including the read-only `root_ownership()`) into a panic of its
    /// own.
    fn lock_ownership(&self) -> MutexGuard<'_, RootOwnership> {
        self.inner
            .root_ownership
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Shared side of the swap ordering lock — taken by an operator swap.
    /// See [`WorkspaceInner::root_swap`]. Poison-recovering for the same
    /// reason as [`lock_ownership`](Self::lock_ownership); the guarded
    /// value is `()`.
    fn lock_operator_swap(&self) -> RwLockReadGuard<'_, ()> {
        self.inner
            .root_swap
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Exclusive side of the swap ordering lock — taken by a client-root
    /// adoption for its whole check-swap-publish sequence.
    fn lock_adoption(&self) -> RwLockWriteGuard<'_, ()> {
        self.inner
            .root_swap
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Bound runtime root swaps to a containment boundary (local mode).
    ///
    /// With a boundary attached, [`set_root_dir`](Self::set_root_dir)
    /// refuses any target whose *canonical* path is not inside `boundary`
    /// — `..` traversals and symlinks out of the tree are therefore
    /// rejected too, and the rejection happens before any state is
    /// touched. Without it (the default) `set_root_dir` stays unbounded,
    /// which is the historical behaviour.
    ///
    /// Call immediately after `open_local`, before the workspace is cloned
    /// into [`crate::server::ServerOptions`] — like
    /// [`with_activation_summary`](Self::with_activation_summary) it mutates
    /// the still-unique inner `Arc`. Unlike the hook builders, a late call
    /// is an **error** rather than a warning: a containment boundary that
    /// silently failed to attach is worse than no boundary at all.
    ///
    /// Errors when the boundary does not exist, when the workspace is not
    /// local-flavoured, or when the already-active root lies outside the
    /// boundary — a config that contradicts itself must die at boot, not at
    /// the first swap.
    pub fn with_sandbox_root(mut self, boundary: &Path) -> Result<Self> {
        if !matches!(self.inner.kind, WorkspaceKind::Local) {
            anyhow::bail!(
                "sandbox_root is only valid for local workspaces (this one is {})",
                self.inner.kind.as_str()
            );
        }
        if !boundary.is_dir() {
            anyhow::bail!(
                "sandbox_root does not exist or is not a directory: {}",
                boundary.display()
            );
        }
        let canon = boundary.canonicalize().with_context(|| {
            format!("failed to canonicalize sandbox_root {}", boundary.display())
        })?;
        if let Some(active) = self.active_repo_path() {
            if !active.starts_with(&canon) {
                anyhow::bail!(
                    "active root {} is outside sandbox_root {}: the configured root must lie inside the containment boundary",
                    active.display(),
                    canon.display()
                );
            }
        }
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.sandbox_root = Some(canon),
            None => anyhow::bail!(
                "with_sandbox_root called after the workspace was cloned; \
                 the containment boundary {} would not be enforced",
                canon.display()
            ),
        }
        Ok(self)
    }

    /// Attach an [`ActivationSummaryHook`]. Call immediately after
    /// `open`/`open_local` (before the workspace is cloned into
    /// `ServerOptions`): it mutates the still-unique inner `Arc`. Calling
    /// it after the workspace has been cloned is a no-op with a warning —
    /// the summary simply won't be attached.
    pub fn with_activation_summary(mut self, hook: ActivationSummaryHook) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.activation_summary = Some(hook),
            None => tracing::warn!(
                "with_activation_summary called after the workspace was cloned; summary not attached"
            ),
        }
        self
    }

    /// Attach a [`PostActivateRevsHook`]. Call immediately after
    /// `open`/`open_local` (before the workspace is cloned into
    /// `ServerOptions`): it mutates the still-unique inner `Arc`, exactly
    /// like [`with_activation_summary`](Self::with_activation_summary).
    /// Calling it after the workspace has been cloned is a no-op with a
    /// warning. Additive — consumers that don't set it keep the plain
    /// single-rev activation behaviour.
    pub fn with_post_activate_revs(mut self, hook: PostActivateRevsHook) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.post_activate_revs = Some(hook),
            None => tracing::warn!(
                "with_post_activate_revs called after the workspace was cloned; revs hook not attached"
            ),
        }
        self
    }

    /// Attach the request-scoped activation prepare/commit contract.
    ///
    /// When set, this hook owns plain builds, revision-set builds, cheap-skip
    /// summaries, and atomic product publication. It replaces the legacy
    /// `post_activate`, `post_activate_revs`, and `activation_summary`
    /// callbacks for activation calls. Configure it before cloning the
    /// workspace into [`crate::server::ServerOptions`].
    pub fn with_activation_transaction(mut self, hook: ActivationTransactionHook) -> Self {
        match Arc::get_mut(&mut self.inner) {
            Some(inner) => inner.activation_transaction = Some(hook),
            None => tracing::warn!(
                "with_activation_transaction called after the workspace was cloned; transaction not attached"
            ),
        }
        self
    }

    pub fn kind(&self) -> WorkspaceKind {
        self.inner.kind
    }

    /// The directory this workspace keeps its bookkeeping under.
    ///
    /// After an unanchored local boot ([`open_local_unanchored`](Self::open_local_unanchored))
    /// this is the empty path until the first activation anchors it; every
    /// other constructor knows it up front.
    pub fn workspace_dir(&self) -> &Path {
        match self.inner.deferred_anchor.get() {
            Some(anchored) => anchored.as_path(),
            None => &self.inner.workspace_dir,
        }
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.workspace_dir().join("repos")
    }

    /// Base directory for inventory bookkeeping, or `None` while an
    /// unanchored local workspace has yet to activate anything.
    fn inventory_base(&self) -> Option<&Path> {
        let base = self.workspace_dir();
        (!base.as_os_str().is_empty()).then_some(base)
    }

    fn inventory_path(&self) -> Option<PathBuf> {
        let base = self.inventory_base()?;
        Some(match self.inner.kind {
            WorkspaceKind::Github => base.join("inventory.json"),
            WorkspaceKind::Local => base.join(".mcp-workspace").join("inventory.json"),
        })
    }

    /// Active repo's full org/repo name, or None if nothing is active.
    pub fn active_repo_name(&self) -> Option<String> {
        self.inner.state.read().unwrap().active_repo_name.clone()
    }

    /// Active repo's filesystem path, or None.
    pub fn active_repo_path(&self) -> Option<PathBuf> {
        self.inner.state.read().unwrap().active_repo_path.clone()
    }

    /// Default `org/repo` for the GitHub tools when the caller passes none.
    ///
    /// Github mode: the active repo — there the inventory key *is* the
    /// `org/repo`. Local mode: the active root's `origin` remote parsed
    /// to `org/repo`, or `None` when there's no GitHub remote. Crucially
    /// it is *never* the `local/<dir>` inventory key (see
    /// [`active_repo_name`](Self::active_repo_name)), which is a
    /// filesystem-derived key, not a valid repo slug.
    pub fn default_github_repo(&self) -> Option<String> {
        match self.inner.kind {
            WorkspaceKind::Github => self.active_repo_name(),
            WorkspaceKind::Local => self.active_repo_path().and_then(|p| parse_origin_repo(&p)),
        }
    }

    // ------------------------------------------------------------------
    // Inventory management
    // ------------------------------------------------------------------

    fn load_inventory_unlocked(&self) -> BTreeMap<String, InventoryEntry> {
        let Some(path) = self.inventory_path() else {
            return BTreeMap::new();
        };
        let Ok(text) = fs::read_to_string(&path) else {
            return BTreeMap::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn save_inventory_unlocked(&self, inv: &BTreeMap<String, InventoryEntry>) -> Result<()> {
        // No anchor yet (unanchored local boot) means nowhere to write.
        // Bookkeeping is best-effort in that window, by construction.
        let Some(path) = self.inventory_path() else {
            return Ok(());
        };
        let body = serde_json::to_string_pretty(inv).context("failed to serialise inventory")?;
        fs::write(&path, body).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    fn load_inventory(&self) -> BTreeMap<String, InventoryEntry> {
        let _guard = self.inner.inventory.lock().unwrap();
        self.load_inventory_unlocked()
    }

    fn reconcile_inventory(&self) -> Result<()> {
        let _guard = self.inner.inventory.lock().unwrap();
        let mut inv = self.load_inventory_unlocked();
        let mut on_disk: Vec<String> = Vec::new();
        if self.repos_dir().is_dir() {
            for org_entry in fs::read_dir(self.repos_dir())? {
                let Ok(org_entry) = org_entry else { continue };
                if !org_entry.path().is_dir() {
                    continue;
                }
                let org = org_entry.file_name().to_string_lossy().into_owned();
                if org.starts_with('.') {
                    continue;
                }
                for repo_entry in fs::read_dir(org_entry.path())? {
                    let Ok(repo_entry) = repo_entry else { continue };
                    if !repo_entry.path().is_dir() {
                        continue;
                    }
                    let repo = repo_entry.file_name().to_string_lossy().into_owned();
                    if repo.starts_with('.') {
                        continue;
                    }
                    let rname = format!("{org}/{repo}");
                    on_disk.push(rname.clone());
                    inv.entry(rname).or_insert_with(|| {
                        let mtime = repo_entry
                            .metadata()
                            .ok()
                            .and_then(|m| m.modified().ok())
                            .map(format_iso)
                            .unwrap_or_else(now_iso);
                        InventoryEntry {
                            cloned_at: mtime.clone(),
                            last_accessed: mtime,
                            access_count: 0,
                            stale: false,
                            last_built_sha: None,
                            last_built_revs: None,
                        }
                    });
                }
            }
        }
        for (rname, entry) in inv.iter_mut() {
            if !on_disk.contains(rname) && !entry.stale {
                entry.stale = true;
            }
        }
        self.save_inventory_unlocked(&inv)?;
        Ok(())
    }

    fn bump_access(&self, name: &str, action: &str) {
        let _guard = self.inner.inventory.lock().unwrap();
        let mut inv = self.load_inventory_unlocked();
        let now = now_iso();
        let entry = inv
            .entry(name.to_string())
            .or_insert_with(|| InventoryEntry {
                cloned_at: now.clone(),
                last_accessed: now.clone(),
                access_count: 0,
                stale: false,
                last_built_sha: None,
                last_built_revs: None,
            });
        entry.last_accessed = now.clone();
        entry.access_count += 1;
        entry.stale = false;
        if action == "cloned" || entry.cloned_at.is_empty() {
            entry.cloned_at = now;
        }
        let _ = self.save_inventory_unlocked(&inv);
    }

    fn mark_stale(&self, name: &str) {
        let _guard = self.inner.inventory.lock().unwrap();
        let mut inv = self.load_inventory_unlocked();
        if let Some(entry) = inv.get_mut(name) {
            entry.stale = true;
            let _ = self.save_inventory_unlocked(&inv);
        }
    }

    fn sweep_stale(&self) -> Vec<String> {
        // Local mode has nothing to sweep — the operator owns the root.
        if matches!(self.inner.kind, WorkspaceKind::Local) {
            return Vec::new();
        }
        let active = self.active_repo_name();
        let _guard = self.inner.inventory.lock().unwrap();
        let mut inv = self.load_inventory_unlocked();
        let cutoff = SystemTime::now()
            - std::time::Duration::from_secs(self.inner.stale_after_days as u64 * 86_400);
        let mut swept: Vec<String> = Vec::new();
        for (rname, entry) in inv.iter_mut() {
            if entry.stale {
                continue;
            }
            if Some(rname.as_str()) == active.as_deref() {
                continue;
            }
            let last = parse_iso(&entry.last_accessed).unwrap_or(SystemTime::UNIX_EPOCH);
            if last >= cutoff {
                continue;
            }
            let parts: Vec<&str> = rname.splitn(2, '/').collect();
            if parts.len() != 2 {
                continue;
            }
            let repo_path = self.repos_dir().join(parts[0]).join(parts[1]);
            if repo_path.exists() {
                let _ = fs::remove_dir_all(&repo_path);
            }
            entry.stale = true;
            swept.push(rname.clone());
        }
        if !swept.is_empty() {
            let _ = self.save_inventory_unlocked(&inv);
            self.prune_empty_org_dirs();
        }
        swept
    }

    fn prune_empty_org_dirs(&self) {
        let Ok(entries) = fs::read_dir(self.repos_dir()) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Ok(children) = fs::read_dir(&path) {
                let real: Vec<_> = children
                    .flatten()
                    .filter(|c| !c.file_name().to_string_lossy().starts_with('.'))
                    .collect();
                if real.is_empty() {
                    let _ = fs::remove_dir_all(&path);
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // Git operations
    // ------------------------------------------------------------------

    /// Clone (if missing) or fast-forward (if cloned). Returns the
    /// action label, the repo path, and the new HEAD SHA after the op.
    ///
    /// Local-mode short-circuits: there's nothing to clone or fetch.
    /// The "SHA" is a cheap content fingerprint (recursive walk of file
    /// mtimes + sizes) so the auto-rebuild gate still works.
    fn clone_or_update(
        &self,
        name: &str,
        requested_local_root: Option<&Path>,
    ) -> Result<(String, PathBuf, String)> {
        if matches!(self.inner.kind, WorkspaceKind::Local) {
            // `set_root_dir` passes its canonical target explicitly. This
            // avoids publishing the requested path before its build commits,
            // and prevents a concurrent activation from changing the path
            // fingerprinted by this request. Refresh calls snapshot the
            // currently committed root and pass it through the same argument.
            //
            // So every local caller supplies the root — `repo_management`
            // having already refused with "No active local root." when
            // there is none. The fallback below is an internal invariant,
            // not a user-facing state, and deliberately does not describe
            // one.
            let root = requested_local_root
                .map(Path::to_path_buf)
                .or_else(|| self.active_repo_path())
                .context("internal error: local activation without a root")?;
            // NB: the inventory anchor is *not* chosen here. An unanchored
            // boot picks the directory that owns `.mcp-workspace/` at the
            // first activation that actually commits — see
            // `anchor_inventory` — so a proposal that fails its build
            // writes nothing inside the proposed root.
            let prev_sha = self.last_built_sha(name);
            let fingerprint = fingerprint_dir(&root);
            let action = match prev_sha {
                Some(p) if p == fingerprint => "current",
                None => "cloned", // first activation
                Some(_) => "updated",
            };
            return Ok((action.to_string(), root, fingerprint));
        }
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        let repo_path = self.repos_dir().join(parts[0]).join(parts[1]);
        if !repo_path.exists() {
            fs::create_dir_all(repo_path.parent().unwrap()).ok();
            let url = format!("https://github.com/{name}.git");
            // Treeless clone (`--filter=tree:0`): keeps the FULL commit
            // history — so `git log -S` (pickaxe) and any rev walk work —
            // while fetching tree/blob objects lazily on demand, keeping
            // the initial transfer near a shallow clone's cost. `--tags`
            // pulls all tags up front so tag-scoped rev reads
            // (`read_source rev=v1.2.3`) resolve without a follow-up fetch.
            // (Was `--depth 1`, which truncated history and broke pickaxe.)
            let out = Command::new("git")
                .args([
                    "clone",
                    "--filter=tree:0",
                    "--tags",
                    &url,
                    repo_path.to_str().unwrap(),
                ])
                .output()
                .context("failed to spawn `git clone`")?;
            if !out.status.success() {
                anyhow::bail!(
                    "git clone failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let sha = git_rev_parse(&repo_path, "HEAD")?;
            return Ok(("cloned".to_string(), repo_path, sha));
        }

        // Fetch + check head delta. Plain `git fetch origin --tags` (no
        // `--depth 1`) so the treeless clone stays history-complete and
        // newly-pushed tags become available for rev-scoped reads; blobs
        // are still fetched lazily. FETCH_HEAD records the remote's
        // default-branch tip, so the SHA-gate below is unchanged.
        Command::new("git")
            .args(["fetch", "origin", "--tags"])
            .current_dir(&repo_path)
            .output()
            .context("git fetch failed")?;
        let local = git_rev_parse(&repo_path, "HEAD")?;
        let remote = git_rev_parse(&repo_path, "FETCH_HEAD")?;
        if local != remote {
            Command::new("git")
                .args(["reset", "--hard", "FETCH_HEAD"])
                .current_dir(&repo_path)
                .output()
                .context("git reset failed")?;
            let sha = git_rev_parse(&repo_path, "HEAD")?;
            return Ok(("updated".to_string(), repo_path, sha));
        }
        Ok(("current".to_string(), repo_path, local))
    }

    /// Resolve a [`RevsRequest`] against the git repo at `repo_path` into
    /// a concrete, ordered list of git revspecs.
    ///
    /// - `Count(n)`: the newest `n` **stable release** tags of the repo's
    ///   dominant tag family, **oldest→newest**, with `HEAD` appended as
    ///   the final (newest) rev. Selection (see [`select_family_tags`]):
    ///   every tag is classified into `(prefix, version, is_prerelease)`
    ///   by stripping a trailing version component; tags with no version
    ///   component (e.g. `r-universe-release`) are excluded. Tags are
    ///   grouped by prefix and the family with the most **stable**
    ///   (non-prerelease) tags is chosen; within it the newest `n` stable
    ///   tags (version-sorted) are taken. Prerelease markers (rc, alpha,
    ///   beta, dev, pre, preview — case-insensitive) never count as
    ///   releases. Fallback chain for degenerate repos: if the winning
    ///   family has no stable tags its prereleases are used; if no tag is
    ///   version-like at all, the raw `git tag --sort=-v:refname` top-`n`
    ///   is used. Errors only if the repo has no tags whatsoever. Fewer
    ///   than `n` matching tags is not an error — all available are used.
    /// - `List(revs)`: each revspec is validated with
    ///   `git rev-parse --verify <rev>^{commit}` and used verbatim (no
    ///   sort, no `HEAD` appended). Errors on the first unknown rev.
    ///
    /// The resolved list is deduplicated order-preserving on the label
    /// string (first occurrence wins) before being returned — see
    /// [`dedup_labels`] — so a downstream hook never receives duplicate
    /// revspecs (e.g. `revs=["HEAD","HEAD"]`). Dedup is on the label, not
    /// the resolved commit: two *different* revspecs that happen to point
    /// at the same commit are deliberately both kept, because labels are
    /// the graph-facing names a multi-rev builder attaches.
    ///
    /// A non-git `repo_path` surfaces as the `git tag` / `git rev-parse`
    /// failure with a clear message.
    fn resolve_revs(&self, repo_path: &Path, req: &RevsRequest) -> Result<Vec<String>> {
        let resolved = match req {
            RevsRequest::Count(n) => {
                let out = Command::new("git")
                    .args(["tag", "--sort=-v:refname"])
                    .current_dir(repo_path)
                    .output()
                    .context("failed to spawn `git tag`")?;
                if !out.status.success() {
                    anyhow::bail!(
                        "cannot resolve revs: `git tag` failed in {} (is it a git repo?): {}",
                        repo_path.display(),
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                let tags: Vec<String> = String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(|l| l.trim().to_string())
                    .filter(|l| !l.is_empty())
                    .collect();
                if tags.is_empty() {
                    anyhow::bail!(
                        "revs={n} requested but '{}' has no tags to resolve",
                        repo_path.display()
                    );
                }
                // Classify + pick the dominant release family's newest `n`
                // stable tags (oldest→newest). Falls back to the raw
                // version-sorted top-`n` when no tag is version-like.
                let mut chosen = select_family_tags(&tags, *n).unwrap_or_else(|| {
                    let mut raw: Vec<String> = tags.into_iter().take(*n).collect();
                    raw.reverse();
                    raw
                });
                // HEAD last so a multi-rev builder merges oldest→newest
                // with HEAD's signature winning.
                chosen.push("HEAD".to_string());
                chosen
            }
            RevsRequest::List(revs) => {
                if revs.is_empty() {
                    anyhow::bail!("revs list is empty — pass at least one revision");
                }
                for r in revs {
                    let out = Command::new("git")
                        .args([
                            "rev-parse",
                            "--verify",
                            "--quiet",
                            &format!("{r}^{{commit}}"),
                        ])
                        .current_dir(repo_path)
                        .output()
                        .context("failed to spawn `git rev-parse`")?;
                    if !out.status.success() {
                        anyhow::bail!("revision '{r}' does not exist in '{}'", repo_path.display());
                    }
                }
                revs.clone()
            }
        };
        Ok(dedup_labels(resolved))
    }

    /// Activate a repo: prepare source, build, and publish if still current.
    ///
    /// Auto-rebuild gating: if `force_rebuild` is false AND no `revs` were
    /// requested AND the repo is already at the HEAD it was last built at
    /// (`action == "current"` AND `prev_built_sha == new_head`), the
    /// post-activate hook is skipped. This makes `repo_management(update=True)`
    /// cheap when upstream hasn't moved. Set `force_rebuild=true` to bypass
    /// (e.g. after upgrading the builder itself).
    ///
    /// When `revs` are requested the skip gate never applies — a
    /// revs-requested activation always fires the hook (see the gate
    /// comment below). If the revs-aware hook is set, it is called with
    /// the resolved revspecs; otherwise the plain hook runs (single-rev
    /// build) and the resolved list is not reported.
    ///
    /// Each request receives an identity before active source state mutates.
    /// A transaction hook prepares off-lock and returns the commit closure
    /// that installs its product and generates its summary. If newer intent
    /// arrives first, the closure is dropped and the response reports
    /// supersession. On commit, source identity, in-process built identity,
    /// and inventory receipt publish under one generation boundary.
    ///
    /// On successful hook completion the new HEAD SHA is persisted to
    /// `inventory.json[name].last_built_sha`. If the hook fails the SHA and
    /// active source state are not changed, so the next request retries.
    fn activate(
        &self,
        name: &str,
        force_rebuild: bool,
        revs: Option<&RevsRequest>,
        requested_local_root: Option<&Path>,
    ) -> Result<String> {
        // Legacy callbacks publish their downstream product inside the hook,
        // so they cannot safely overlap. Keep that API coherent by
        // serializing the complete request before allocating its identity.
        // Transaction hooks prepare concurrently and therefore skip this
        // lock; their stale work is discarded at the generation gate below.
        let _legacy_guard = self
            .inner
            .activation_transaction
            .is_none()
            .then(|| self.inner.legacy_activation.lock().unwrap());
        let activation_id = {
            let mut state = self.inner.state.write().unwrap();
            state.last_activation_id += 1;
            let id = ActivationId(state.last_activation_id);
            state.latest_requested = Some(id);
            id
        };
        let prev_built_sha = self.last_built_sha(name);
        let prev_built_revs = self.last_built_revs(name);
        let (action, repo_path, head_sha) = self
            .clone_or_update(name, requested_local_root)
            .with_context(|| {
                format!("activation request {activation_id} source preparation failed")
            })?;
        // Resolve any requested revs before mutating active state, so a
        // bad request (no tags / unknown rev) returns a clean error with
        // the repo cloned-but-not-activated rather than half-bound.
        let resolved_revs = match revs {
            Some(req) => Some(self.resolve_revs(&repo_path, req).with_context(|| {
                format!("activation request {activation_id} revision resolution failed")
            })?),
            None => None,
        };
        self.bump_access(name, &action);
        let is_active_built = {
            let state = self.inner.state.read().unwrap();
            state.active_build.as_ref().is_some_and(|built| {
                built.name == name && built.path == repo_path && built.resolved_revs.is_none()
            })
        };

        // The skip gate must be satisfied on BOTH axes: the git repo is
        // at its last-built SHA (persisted, cross-process) AND `name` is
        // the *currently active* built root in this process (in-memory).
        // Without the second axis a fresh process would inherit
        // `last_built_sha` from disk, skip the hook, and leave the
        // consumer's in-memory state (e.g. the code graph) empty —
        // activate would report success with nothing loaded. The axis
        // checks the *active* built name, not any name ever built, so an
        // A→B→A swap correctly rebuilds A: after activate(B) the live
        // slot holds B, so re-binding A must not skip (see the
        // `active_build` field doc).
        // Skip-gate / revs interaction: a revs-requested activation ALWAYS
        // fires the hook — the SHA-skip gate only applies to the plain
        // (no-revs) path (`resolved_revs.is_none()`). Rationale: the gate
        // keys off HEAD's SHA alone, which says nothing about which *set*
        // of revs a prior build loaded, so a rev-set request at the same
        // HEAD must rebuild. The tradeoff is that repeat `revs=` calls at
        // an unchanged HEAD re-parse every rev — acceptable for an
        // explicit multi-rev request.
        //
        // The plain path additionally requires `prev_built_revs.is_none()`
        // — a plain re-activation must NOT skip when the last build was
        // multi-rev. Without this the tool would report a plain activation
        // (and, downstream, the plain hook rebuilding a single-rev graph
        // was bypassed) while the live product is still the rev-set union.
        // A non-skip here means the plain hook runs and `record_built`
        // clears the stored request — resetting to a genuine plain graph.
        let already_built = !force_rebuild
            && resolved_revs.is_none()
            && prev_built_revs.is_none()
            && action == "current"
            && prev_built_sha.as_deref() == Some(head_sha.as_str())
            && is_active_built;
        let uses_transaction = self.inner.activation_transaction.is_some();
        let revision_build = !already_built
            && resolved_revs.is_some()
            && (uses_transaction || self.inner.post_activate_revs.is_some());
        let build = if already_built {
            ActivationBuild::Reuse
        } else if revision_build {
            ActivationBuild::Revisions(resolved_revs.clone().unwrap_or_default())
        } else {
            ActivationBuild::Plain
        };
        let request = ActivationRequest {
            id: activation_id,
            path: repo_path.clone(),
            name: name.to_string(),
            build,
        };

        let prepared = if let Some(hook) = &self.inner.activation_transaction {
            hook(&request)
        } else {
            // Compatibility path. The complete request is serialized by
            // `_legacy_guard`, making the old build-then-summary sequence
            // coherent even though those callbacks cannot carry an id.
            let hook_result = match request.build() {
                ActivationBuild::Reuse => Ok(()),
                ActivationBuild::Revisions(resolved) => self
                    .inner
                    .post_activate_revs
                    .as_ref()
                    .map_or(Ok(()), |hook| hook(&repo_path, name, resolved)),
                ActivationBuild::Plain => self
                    .inner
                    .post_activate
                    .as_ref()
                    .map_or(Ok(()), |hook| hook(&repo_path, name)),
            };
            hook_result.map(|()| {
                let summary = self
                    .inner
                    .activation_summary
                    .as_ref()
                    .and_then(|hook| hook(&repo_path, name));
                PreparedActivation::summary(summary)
            })
        };

        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let latest = self.inner.state.read().unwrap().latest_requested;
                if latest != Some(activation_id) {
                    return Ok(format!(
                        "Activation request {activation_id} for '{name}' was superseded by request {} before its failed build could publish.",
                        latest.map_or_else(|| "unknown".to_string(), |id| id.to_string())
                    ));
                }
                return Err(anyhow!(
                    "activation request {activation_id} for '{name}' failed during preparation: {error}"
                ));
            }
        };

        // Generation commit point. While this write lock is held, no newer
        // identity can be allocated. The downstream product, its request-
        // scoped summary, source binding, and built identity therefore become
        // visible as one ordered transaction.
        let summary = {
            let mut state = self.inner.state.write().unwrap();
            if state.latest_requested != Some(activation_id) {
                let superseding = state.latest_requested;
                drop(state);
                drop(prepared);
                return Ok(format!(
                    "Activation request {activation_id} for '{name}' was superseded by request {} before publication; its prepared build was discarded.",
                    superseding.map_or_else(|| "unknown".to_string(), |id| id.to_string())
                ));
            }
            let summary = prepared.commit().with_context(|| {
                format!("activation request {activation_id} for '{name}' failed during commit")
            })?;
            // The build is live, so this activation is the one that gets to
            // fix the inventory anchor (no-op unless this is the first
            // commit after an unanchored boot). Must precede `record_built`,
            // which needs somewhere to write its receipt.
            self.anchor_inventory(&repo_path, name, &action);
            if !matches!(request.build(), ActivationBuild::Reuse) {
                let built_revs = revision_build.then_some(revs).flatten();
                self.record_built(name, &head_sha, built_revs);
            }
            state.active_repo_name = Some(name.to_string());
            state.active_repo_path = Some(repo_path.clone());
            state.active_build = Some(ActiveBuildState {
                activation_id,
                name: name.to_string(),
                path: repo_path.clone(),
                head_sha: head_sha.clone(),
                resolved_revs: match request.build() {
                    ActivationBuild::Revisions(resolved) => Some(resolved.clone()),
                    ActivationBuild::Plain | ActivationBuild::Reuse => None,
                },
            });
            summary
        };

        let verb = match action.as_str() {
            "cloned" => "Cloned",
            "updated" => "Updated",
            "current" => "Activated (already up to date)",
            other => other,
        };
        let suffix = if already_built {
            " [build skipped: HEAD matches last-built SHA]"
        } else {
            ""
        };
        let mut base = format!("{verb} '{name}' at {}.{suffix}", repo_path.display());
        // Name the resolved revisions on their own line so agents see
        // exactly what got loaded. Only when the revs-hook actually ran
        // (revs requested AND hook set AND it succeeded) — a fallback to
        // the plain hook loads HEAD only, so claiming a rev-set would lie.
        if let ActivationBuild::Revisions(resolved) = request.build() {
            base.push_str(&format!("\nrevs: {}", resolved.join(", ")));
        }
        Ok(match summary {
            Some(s) if !s.is_empty() => format!("{base}\n\n{s}"),
            _ => base,
        })
    }

    /// Fix the inventory home on the first activation that **commits**.
    ///
    /// A no-op outside one window: a local workspace that booted
    /// unanchored ([`open_local_unanchored`](Self::open_local_unanchored))
    /// and has not committed an activation yet. Every anchored constructor
    /// decides the directory up front.
    ///
    /// Deliberately driven from the commit block rather than from
    /// `clone_or_update`. The root of an unanchored workspace arrives from
    /// an *external* party (an MCP client's advertised root), so a
    /// proposal that fails its build must leave nothing of itself behind:
    /// no `.mcp-workspace/` created inside the client's directory, and no
    /// permanently-fixed anchor pointing at a root that never activated.
    ///
    /// `bump_access` already ran for this activation, while there was
    /// still nowhere to write; it is repeated here so the entry
    /// `record_built` is about to update exists. Not a double count — the
    /// earlier call's save was a no-op.
    ///
    /// A directory-creation failure is logged, not propagated: the
    /// downstream product is published by the time this runs, and
    /// bookkeeping in the unanchored window is best-effort by
    /// construction (see [`save_inventory_unlocked`](Self::save_inventory_unlocked)).
    /// The workspace stays unanchored and the next activation retries.
    fn anchor_inventory(&self, root: &Path, name: &str, action: &str) {
        if !matches!(self.inner.kind, WorkspaceKind::Local) || self.inventory_base().is_some() {
            return;
        }
        let inv_dir = root.join(".mcp-workspace");
        if let Err(e) = fs::create_dir_all(&inv_dir) {
            tracing::warn!(
                "failed to create local-workspace dir {}: {e}",
                inv_dir.display()
            );
            return;
        }
        let _ = self.inner.deferred_anchor.set(root.to_path_buf());
        self.bump_access(name, action);
    }

    /// Record the outcome of a successful build: the HEAD SHA plus the
    /// revisions request that produced it (`Some` only for a multi-rev
    /// build via the revs hook; `None` for a plain / HEAD-only build,
    /// which *clears* any previously-stored request). Called only on hook
    /// success — a failed build records nothing, so the next `update` retries.
    fn record_built(&self, name: &str, sha: &str, revs: Option<&RevsRequest>) {
        let _guard = self.inner.inventory.lock().unwrap();
        let mut inv = self.load_inventory_unlocked();
        if let Some(entry) = inv.get_mut(name) {
            entry.last_built_sha = Some(sha.to_string());
            entry.last_built_revs = revs.cloned();
            let _ = self.save_inventory_unlocked(&inv);
        }
    }

    /// Read the SHA recorded after the last successful post-activate hook
    /// for the named repo. `None` if the repo was never built (or the
    /// hook last failed). Useful for downstream consumers gating
    /// "is the active graph up to date with the repo HEAD?" checks.
    pub fn last_built_sha(&self, name: &str) -> Option<String> {
        self.load_inventory()
            .get(name)
            .and_then(|e| e.last_built_sha.clone())
    }

    /// Read the revisions request last successfully built for the named
    /// repo — `Some` when the last build was multi-rev (`revs=`), `None`
    /// for a plain / HEAD-only build or a never-built repo. Drives the
    /// rev-set-aware skip gate and the `update`-preserves-rev-set path.
    pub fn last_built_revs(&self, name: &str) -> Option<RevsRequest> {
        self.load_inventory()
            .get(name)
            .and_then(|e| e.last_built_revs.clone())
    }

    fn delete(&self, name: &str) -> Result<String> {
        let parts: Vec<&str> = name.splitn(2, '/').collect();
        if parts.len() != 2 {
            anyhow::bail!("Invalid repo name");
        }
        let repo_path = self.repos_dir().join(parts[0]).join(parts[1]);
        let mut deleted = Vec::new();
        if repo_path.exists() {
            fs::remove_dir_all(&repo_path).context("failed to remove repo dir")?;
            deleted.push("repo");
        }
        self.mark_stale(name);
        self.prune_empty_org_dirs();
        if deleted.is_empty() {
            return Ok(format!("Nothing to delete — '{name}' not found."));
        }
        let mut state = self.inner.state.write().unwrap();
        if state.active_repo_name.as_deref() == Some(name) {
            state.active_repo_name = None;
            state.active_repo_path = None;
            state.active_build = None;
            return Ok(format!(
                "Deleted {}. Active repo cleared.",
                deleted.join(", ")
            ));
        }
        Ok(format!("Deleted {}.", deleted.join(", ")))
    }

    fn list(&self) -> String {
        let inv = self.load_inventory();
        if inv.is_empty() {
            return "No repos cloned yet. Call repo_management('org/repo') to clone one."
                .to_string();
        }
        let active = self.active_repo_name();
        let mut live: Vec<String> = Vec::new();
        let mut stale_lines: Vec<String> = Vec::new();
        for (rname, entry) in &inv {
            let marker = if Some(rname.as_str()) == active.as_deref() {
                " [active]"
            } else {
                ""
            };
            let access = format!(
                "{} access{}, last {}",
                entry.access_count,
                if entry.access_count == 1 { "" } else { "es" },
                relative_time(&entry.last_accessed)
            );
            if entry.stale {
                stale_lines.push(format!(
                    "  {rname}  [STALE — re-fetch with repo_management('{rname}')]  ({access})"
                ));
            } else {
                live.push(format!("  {rname}{marker}  ({access})"));
            }
        }
        let mut out = String::new();
        if !live.is_empty() {
            out.push_str(&format!(
                "{} live repo(s):\n{}",
                live.len(),
                live.join("\n")
            ));
        }
        if !stale_lines.is_empty() {
            if !out.is_empty() {
                out.push_str("\n\n");
            }
            out.push_str(&format!(
                "{} stale repo(s):\n{}",
                stale_lines.len(),
                stale_lines.join("\n")
            ));
        }
        out
    }

    /// Public entry for the `repo_management` MCP tool.
    ///
    /// - `name`: `org/repo` to activate (None = list / refresh mode).
    /// - `delete`: remove the named repo + inventory entry. Github only.
    /// - `update`: refresh the active repo (auto-rebuild gated).
    /// - `force_rebuild`: with `update=true` (or initial activation),
    ///   re-run the post-activate hook even when the HEAD SHA matches
    ///   `last_built_sha`. Useful after the builder itself has been
    ///   upgraded.
    ///
    /// Local mode behaviour: `name` and `delete` are rejected; pass
    /// `update=true` (or no args after the initial activation) to
    /// re-fingerprint the root and rebuild if anything changed.
    pub fn repo_management(
        &self,
        name: Option<&str>,
        delete: bool,
        update: bool,
        force_rebuild: bool,
        revs: Option<&RevsRequest>,
    ) -> String {
        // Local mode: most github-only semantics are nonsensical here.
        if matches!(self.inner.kind, WorkspaceKind::Local) {
            if name.is_some() {
                return "Local-workspace mode does not accept a repo name. Use `set_root_dir(path)` \
                        to switch the active root, or pass `update=true` / `force_rebuild=true` \
                        to rebuild against the current root."
                    .to_string();
            }
            if delete {
                return "Local-workspace mode does not support `delete`. The root is owned by the \
                        operator; remove it manually."
                    .to_string();
            }
            let active = match self.active_repo_name() {
                Some(n) => n,
                None => return "No active local root.".to_string(),
            };
            // `update`: re-fingerprint and rebuild if anything changed.
            // `force_rebuild`: rebuild even when the fingerprint matches.
            // Either flag (or neither — initial bind path) routes through
            // `activate`; `activate` itself consults the gate using the
            // force flag plus the SHA comparison.
            let _ = update; // explicit: update is implicit in local mode
                            // A local `repo_management` call is always a refresh of the
                            // bound root, so when no explicit `revs` are passed re-apply
                            // the stored rev-set (if the last build was multi-rev) — a
                            // bare refresh must not silently collapse a rev-set graph to
                            // HEAD-only. Re-`set_root_dir` (which passes `revs` verbatim)
                            // is the way to reset back to a plain single-rev build.
            let effective = match revs {
                Some(r) => Some(r.clone()),
                None => self.last_built_revs(&active),
            };
            let active_root = self.active_repo_path();
            return self
                .activate(
                    &active,
                    force_rebuild,
                    effective.as_ref(),
                    active_root.as_deref(),
                )
                .unwrap_or_else(|e| format!("rebuild failed: {e}"));
        }

        let swept = self.sweep_stale();
        let prefix = if swept.is_empty() {
            String::new()
        } else {
            format!(
                "[Swept {} idle repo(s) (>{}d): {}]\n\n",
                swept.len(),
                self.inner.stale_after_days,
                swept.join(", ")
            )
        };

        if name.is_none() && !update {
            return prefix + &self.list();
        }

        if update {
            let Some(active) = self.active_repo_name() else {
                return prefix + "No active repository. Call repo_management('org/repo') first.";
            };
            // `update=True` refreshes the active repo. When no explicit
            // `revs` are passed, re-apply the stored rev-set (if the last
            // build was multi-rev) so a bare `update` after HEAD moves
            // re-resolves and rebuilds the SAME rev-set rather than
            // collapsing it to a single-rev HEAD build. An explicit `revs`
            // argument overrides; a plain re-activation (name path, not
            // `update`) still resets to plain.
            let effective = match revs {
                Some(r) => Some(r.clone()),
                None => self.last_built_revs(&active),
            };
            return prefix
                + &self
                    .activate(&active, force_rebuild, effective.as_ref(), None)
                    .unwrap_or_else(|e| format!("update failed: {e}"));
        }

        let Some(name) = name else {
            return prefix + "Provide a repo name (e.g. repo_management('org/repo')).";
        };
        if let Err(e) = validate_repo_name(name) {
            return prefix + &e.to_string();
        }
        if delete {
            return prefix
                + &self
                    .delete(name)
                    .unwrap_or_else(|e| format!("delete failed: {e}"));
        }
        prefix
            + &self
                .activate(name, force_rebuild, revs, None)
                .unwrap_or_else(|e| format!("activate failed: {e}"))
    }

    /// Swap the active root (local mode only). Re-fires the post-activate
    /// hook against the new root. Errors if the workspace is github-flavoured.
    ///
    /// `revs` (optional): resolve revisions against the new root (which
    /// must be a git repo) and fire the revs-aware hook — see
    /// [`activate`](Self::activate) / [`RevsRequest`].
    ///
    /// Concurrency: two `set_root_dir` calls may overlap — the newer
    /// activation supersedes the older one. A client-root adoption may not
    /// overlap either of them: it is exclusive with every operator swap
    /// for its whole check-swap-publish sequence, which is what makes
    /// "the operator always wins" true rather than merely likely. Must
    /// not be called from inside an activation hook (that has always been
    /// true — the legacy hook path already serializes on its own lock).
    pub fn set_root_dir(&self, new_root: &Path, revs: Option<&RevsRequest>) -> String {
        // Held across the swap *and* the ownership publication below, so
        // an adoption cannot slip its own activation between them. Shared,
        // so concurrent operator swaps keep overlapping as before.
        let _swap = self.lock_operator_swap();
        match self.swap_root(new_root, revs, "set_root_dir") {
            Ok(msg) => {
                // Operator intent is permanent: from here on no
                // client-advertised root may displace this one, including
                // via `roots/list_changed`.
                *self.lock_ownership() = RootOwnership::Operator;
                msg
            }
            Err(msg) => msg,
        }
    }

    /// Adopt a root proposed by the MCP client (`roots/list`).
    ///
    /// Routes through **the same** validation and containment path as
    /// [`set_root_dir`](Self::set_root_dir) — one code path, so a
    /// `workspace.sandbox_root` boundary applies identically to an
    /// operator swap and to a path proposed by an external party. The MCP
    /// spec is explicit that roots are "informational guidance rather than
    /// an access-control mechanism", so the boundary, not the client, is
    /// what bounds this.
    ///
    /// Refuses (without touching any state) when ownership is already
    /// [`RootOwnership::Operator`] — an operator-chosen root always wins.
    /// On success ownership becomes [`RootOwnership::Adopted`], leaving a
    /// later `roots/list_changed` free to replace it.
    ///
    /// `Err` is a human-readable reason for the log; adoption failure is
    /// never fatal to the server.
    pub fn adopt_client_root(&self, new_root: &Path) -> Result<String, String> {
        // Exclusive for the whole check → swap → publish sequence, which
        // is what makes "the operator always wins" true rather than
        // merely likely. A racing `set_root_dir` either has not started
        // (it then waits here and ends up both active and `Operator`), or
        // it completed first (it published `Operator` before releasing,
        // so the check below refuses). What is *not* possible any more is
        // the two overlapping: the operator swapping while this adoption
        // is mid-activation, and this activation committing last.
        let _swap = self.lock_adoption();
        if *self.lock_ownership() == RootOwnership::Operator {
            return Err(
                "the active root was chosen by the operator; client roots are fallback-only"
                    .to_string(),
            );
        }
        let msg = self.swap_root(new_root, None, "adopt_client_root")?;
        *self.lock_ownership() = RootOwnership::Adopted;
        Ok(msg)
    }

    /// The shared root-swap path: local-mode check, existence check,
    /// canonicalize, **containment**, activate. Both the operator entry
    /// point ([`set_root_dir`](Self::set_root_dir)) and the client-root
    /// entry point ([`adopt_client_root`](Self::adopt_client_root)) go
    /// through here, so neither can grow its own boundary semantics.
    ///
    /// `Err` carries the same message the tool would have returned; the
    /// caller decides whether that is a tool result or a log line.
    ///
    /// Both callers hold [`root_swap`](WorkspaceInner::root_swap) across
    /// this call — shared for an operator swap, exclusive for an adoption.
    ///
    /// `who` names the entry point and appears in every message this
    /// returns, so a rejection logged by the adoption path never claims to
    /// be about `set_root_dir`.
    fn swap_root(
        &self,
        new_root: &Path,
        revs: Option<&RevsRequest>,
        who: &str,
    ) -> Result<String, String> {
        if !matches!(self.inner.kind, WorkspaceKind::Local) {
            return Err(format!("{who} is only valid in local-workspace mode."));
        }
        if !new_root.is_dir() {
            return Err(format!(
                "Path does not exist or is not a directory: {}",
                new_root.display()
            ));
        }
        let canon = match new_root.canonicalize() {
            Ok(p) => p,
            Err(e) => return Err(format!("canonicalize failed: {e}")),
        };
        // Containment (opt-in, see `with_sandbox_root`). Tested on the
        // *canonical* path — never the raw argument — so `..` traversals and
        // symlinks pointing out of the tree are caught. Returns before
        // `activate`, so a rejected swap leaves the active root untouched.
        if let Some(sandbox) = self.inner.sandbox_root.as_ref() {
            if !canon.starts_with(sandbox) {
                return Err(format!(
                    "{who}: {} escapes workspace.sandbox_root ({}). \
                     The active root is unchanged.",
                    canon.display(),
                    sandbox.display()
                ));
            }
        }
        let synthetic = synthesize_local_name(&canon);
        // Note: the WorkspaceInner.workspace_dir field is the path the
        // inventory is stored under. We keep the *original* one (from
        // open_local, or the first activation after an unanchored boot) so
        // the inventory survives across root swaps.
        self.activate(&synthetic, false, revs, Some(&canon))
            .map_err(|e| format!("{who} failed: {e}"))
    }
}

/// Deduplicate a resolved revspec list order-preserving, first
/// occurrence wins. Dedup is on the **label** string, not the resolved
/// commit: a downstream multi-rev builder attaches each label as a
/// graph-facing name, so two *different* labels pointing at the same
/// commit are deliberately kept — only literal repeats (e.g. a `HEAD`
/// that appears twice) collapse.
fn dedup_labels(revs: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    revs.into_iter()
        .filter(|r| seen.insert(r.clone()))
        .collect()
}

/// Prerelease markers recognised in a tag's trailing suffix
/// (case-insensitive, with an optional `-`/`.`/`_` separator). A tag
/// whose version is followed by one of these is never treated as a
/// stable release.
const PRERELEASE_MARKERS: &[&str] = &["rc", "alpha", "beta", "dev", "pre", "preview"];

/// A git tag decomposed into its release family `prefix`, numeric
/// `version` components, and whether it carries a prerelease marker.
/// Produced by [`classify_tag`]; consumed by [`select_family_tags`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassifiedTag {
    raw: String,
    prefix: String,
    version: Vec<u64>,
    is_prerelease: bool,
}

/// Classify a single tag into `(prefix, version, is_prerelease)` by
/// locating its trailing version component.
///
/// The version is the *first* `DIGITS(.DIGITS)*` run whose remainder is
/// empty or a recognised prerelease suffix; everything before it is the
/// family `prefix`. Taking the first *cleanly-parsing* run resolves both
/// tricky shapes: `arrow2-0.17.0` skips the `2` in `arrow2` (its
/// remainder `-0.17.0` isn't a recognised suffix, so that run is
/// rejected) and lands on `0.17.0`; while `v3.0.0-rc1` stops at `3.0.0`
/// (with `-rc1` recognised as a prerelease) rather than mistaking the
/// trailing `1` of `rc1` for a version. Returns `None` when the tag has
/// no version-like component at all (e.g. `r-universe-release`), so such
/// tags are excluded from family selection.
fn classify_tag(tag: &str) -> Option<ClassifiedTag> {
    let bytes = tag.as_bytes();
    for i in 0..bytes.len() {
        if !bytes[i].is_ascii_digit() {
            continue;
        }
        // Only consider the *start* of a digit run as a version start.
        if i > 0 && bytes[i - 1].is_ascii_digit() {
            continue;
        }
        if let Some((version, is_prerelease)) = parse_version_at(&tag[i..]) {
            return Some(ClassifiedTag {
                raw: tag.to_string(),
                prefix: tag[..i].to_string(),
                version,
                is_prerelease,
            });
        }
    }
    None
}

/// Parse `s` as `DIGITS(.DIGITS)*` optionally followed by a recognised
/// prerelease suffix. Returns the numeric components and whether a
/// prerelease marker follows. `None` if `s` doesn't start with a digit,
/// or carries an *unrecognised* trailing suffix (so the caller rejects
/// this candidate start and tries an earlier digit run).
fn parse_version_at(s: &str) -> Option<(Vec<u64>, bool)> {
    let bytes = s.as_bytes();
    let mut nums: Vec<u64> = Vec::new();
    let mut idx = 0usize;
    loop {
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == start {
            return None; // expected digits (leading, or after a '.')
        }
        nums.push(s[start..idx].parse().ok()?);
        // Continue only when a '.' is followed by another digit.
        if idx + 1 < bytes.len() && bytes[idx] == b'.' && bytes[idx + 1].is_ascii_digit() {
            idx += 1;
            continue;
        }
        break;
    }
    let rest = &s[idx..];
    if rest.is_empty() {
        return Some((nums, false));
    }
    // A single optional separator, then a recognised prerelease marker.
    let after_sep = rest
        .strip_prefix(|c| c == '-' || c == '.' || c == '_')
        .unwrap_or(rest);
    let lower = after_sep.to_ascii_lowercase();
    if PRERELEASE_MARKERS.iter().any(|m| lower.starts_with(m)) {
        Some((nums, true))
    } else {
        None
    }
}

/// From a repo's tag list, choose the dominant **release family** and
/// return its newest `n` tags **oldest→newest** (HEAD is appended by the
/// caller, not here). Returns `None` when no tag is version-like, so the
/// caller can fall back to the raw version-sorted top-`n`.
///
/// Tags are classified ([`classify_tag`]) and grouped by prefix. The
/// family with the most **stable** (non-prerelease) tags wins; if no
/// family has any stable tag, the family with the most tags overall wins
/// and its prereleases are used. Within the winning family the newest
/// `n` tags in the applicable pool (stable if any exist, else
/// prerelease) are taken by descending version, then reversed to
/// oldest→newest. Deterministic: grouping iterates prefixes in sorted
/// order and the version/`raw` sort is total, so a given tag set always
/// resolves to the same list. Family ties are broken toward the
/// lexicographically-greatest prefix.
fn select_family_tags(tags: &[String], n: usize) -> Option<Vec<String>> {
    let classified: Vec<ClassifiedTag> = tags.iter().filter_map(|t| classify_tag(t)).collect();
    if classified.is_empty() {
        return None;
    }
    // Group by family prefix (BTreeMap → deterministic prefix order).
    let mut families: BTreeMap<String, Vec<&ClassifiedTag>> = BTreeMap::new();
    for c in &classified {
        families.entry(c.prefix.clone()).or_default().push(c);
    }
    let stable_count = |v: &Vec<&ClassifiedTag>| v.iter().filter(|c| !c.is_prerelease).count();
    let any_stable = families.values().any(|v| stable_count(v) > 0);
    // Prefer the family with the most stable tags; when nothing is
    // stable anywhere, prefer the family with the most tags overall.
    // `max_by` returns the last maximum, and BTreeMap yields ascending
    // prefixes, so ties resolve to the greatest prefix — deterministic.
    let chosen = families.values().max_by(|a, b| {
        if any_stable {
            stable_count(a).cmp(&stable_count(b))
        } else {
            a.len().cmp(&b.len())
        }
    })?;
    let mut pool: Vec<&ClassifiedTag> = if any_stable {
        chosen
            .iter()
            .copied()
            .filter(|c| !c.is_prerelease)
            .collect()
    } else {
        chosen.to_vec()
    };
    // Newest first: descending version, then descending raw for a total,
    // stable order on identical versions.
    pool.sort_by(|a, b| b.version.cmp(&a.version).then_with(|| b.raw.cmp(&a.raw)));
    let mut newest: Vec<String> = pool.into_iter().take(n).map(|c| c.raw.clone()).collect();
    newest.reverse(); // oldest→newest
    Some(newest)
}

/// Synthesise a stable "repo name" for a local workspace from its path.
/// Used as the inventory key so the same gating + persistence code paths
/// that github mode uses can apply to local mode unchanged.
fn synthesize_local_name(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "local".to_string());
    format!("local/{name}")
}

/// Parse the `org/repo` slug from a local checkout's `origin` remote.
///
/// Shells out to `git -C <root> remote get-url origin` and parses both
/// canonical GitHub remote forms, stripping the trailing `.git`:
///   - `git@github.com:kkollsga/kglite.git`     → `kkollsga/kglite`
///   - `https://github.com/kkollsga/kglite.git` → `kkollsga/kglite`
///
/// Returns `None` for a non-git directory, a missing `origin` remote, or
/// a non-GitHub remote — so the GitHub tools fall back to their existing
/// empty-default path (ask the caller for `repo_name`).
fn parse_origin_repo(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8(out.stdout).ok()?;
    parse_github_remote(url.trim())
}

/// Pure-string half of [`parse_origin_repo`]: turn a GitHub remote URL
/// into `org/repo`, or `None` if it isn't a recognisable GitHub remote.
fn parse_github_remote(url: &str) -> Option<String> {
    // Accept both SSH (`git@github.com:org/repo`) and HTTPS
    // (`https://github.com/org/repo`) forms; everything after the host
    // separator is the path.
    let path = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("https://github.com/"))
        .or_else(|| url.strip_prefix("http://github.com/"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let path = path.strip_suffix(".git").unwrap_or(path);
    let path = path.trim_end_matches('/');
    // Must be exactly `org/repo` — both segments non-empty, one slash.
    let mut parts = path.split('/');
    let org = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{org}/{repo}"))
}

/// Cheap recursive content fingerprint of a directory tree. Walks files
/// (respecting common ignore patterns) and folds `(path, mtime, len)`
/// into a 64-bit hash, then hex-formats it. Good enough to detect
/// "did anything change?" for auto-rebuild gating — not cryptographic.
fn fingerprint_dir(root: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let walker = ignore::WalkBuilder::new(root)
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .build();
    for entry in walker.flatten() {
        if !entry.path().is_file() {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        entry.path().to_string_lossy().hash(&mut hasher);
        mtime.hash(&mut hasher);
        meta.len().hash(&mut hasher);
    }
    format!("local-{:016x}", hasher.finish())
}

fn git_rev_parse(repo_path: &Path, refspec: &str) -> Result<String> {
    let out = Command::new("git")
        .args(["rev-parse", refspec])
        .current_dir(repo_path)
        .output()
        .context("git rev-parse failed")?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn now_iso() -> String {
    format_iso(SystemTime::now())
}

fn format_iso(t: SystemTime) -> String {
    let secs = t
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Lightweight RFC3339-ish formatter. Drop sub-second precision; matches Python isoformat(timespec=seconds).
    chrono_lite::format_secs(secs)
}

fn parse_iso(s: &str) -> Option<SystemTime> {
    let secs = chrono_lite::parse_secs(s)?;
    SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(secs))
}

fn relative_time(iso: &str) -> String {
    let Some(t) = parse_iso(iso) else {
        return "unknown".to_string();
    };
    let now = SystemTime::now();
    let delta = now.duration_since(t).unwrap_or_default().as_secs();
    if delta < 3600 {
        "just now".to_string()
    } else if delta < 86_400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86_400)
    }
}

/// Tiny self-contained ISO-8601 (seconds-precision) formatter so we
/// don't pull in `chrono` for a handful of timestamps.
mod chrono_lite {
    pub fn format_secs(secs: u64) -> String {
        // Civil-from-days algorithm (Howard Hinnant). Output: YYYY-MM-DDTHH:MM:SS.
        let days = (secs / 86_400) as i64;
        let time = secs % 86_400;
        let (y, mo, d) = days_to_civil(days + 719_468);
        let h = time / 3600;
        let m = (time / 60) % 60;
        let s = time % 60;
        format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}")
    }

    pub fn parse_secs(s: &str) -> Option<u64> {
        // Accept "YYYY-MM-DDTHH:MM:SS" (no zone) — same shape as format_secs output
        // and Python's datetime.isoformat(timespec="seconds").
        let bytes = s.as_bytes();
        if bytes.len() < 19 {
            return None;
        }
        let y: i64 = s.get(0..4)?.parse().ok()?;
        let mo: u32 = s.get(5..7)?.parse().ok()?;
        let d: u32 = s.get(8..10)?.parse().ok()?;
        let h: u64 = s.get(11..13)?.parse().ok()?;
        let m: u64 = s.get(14..16)?.parse().ok()?;
        let sc: u64 = s.get(17..19)?.parse().ok()?;
        let days = civil_to_days(y, mo, d) - 719_468;
        Some((days * 86_400) as u64 + h * 3600 + m * 60 + sc)
    }

    fn days_to_civil(z: i64) -> (i64, u32, u32) {
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = (yoe as i64) + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
        let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
        let y = if m <= 2 { y + 1 } else { y };
        (y, m, d)
    }

    fn civil_to_days(y: i64, m: u32, d: u32) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = (y - era * 400) as u64;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe as i64
    }
}

// silences unused-import-when-helper-only-via-json! macro check.
#[allow(dead_code)]
fn _json_keepalive() {
    let _ = json!({});
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_repo_names() {
        assert!(validate_repo_name("pydata/xarray").is_ok());
        assert!(validate_repo_name("my-org.x/repo_v2").is_ok());
        assert!(validate_repo_name("xarray").is_err());
        assert!(validate_repo_name("a/b/c").is_err());
        assert!(validate_repo_name("foo/bar; rm -rf").is_err());
    }

    #[test]
    fn open_creates_layout() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert!(ws.repos_dir().is_dir());
    }

    #[test]
    fn empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(None, false, false, false, None);
        assert!(out.contains("No repos cloned yet"));
    }

    #[test]
    fn invalid_repo_name_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(Some("bad name with spaces"), false, false, false, None);
        assert!(out.contains("Invalid repo name"));
    }

    #[test]
    fn delete_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(Some("nope/none"), true, false, false, None);
        assert!(out.contains("Nothing to delete"));
    }

    #[test]
    fn iso_round_trip() {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let s = chrono_lite::format_secs(now);
        let back = chrono_lite::parse_secs(&s).unwrap();
        assert_eq!(now, back);
    }

    #[test]
    fn last_built_sha_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        // Seed an inventory entry directly (clone_or_update needs git).
        ws.bump_access("acme/widgets", "cloned");
        assert_eq!(ws.last_built_sha("acme/widgets"), None);
        ws.record_built("acme/widgets", "abc1234deadbeef", None);
        assert_eq!(
            ws.last_built_sha("acme/widgets").as_deref(),
            Some("abc1234deadbeef")
        );
        // Survives an Workspace::open re-read (proves persistence).
        let ws2 = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert_eq!(
            ws2.last_built_sha("acme/widgets").as_deref(),
            Some("abc1234deadbeef")
        );
    }

    #[test]
    fn inventory_loads_legacy_entries_without_sha_field() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        // Hand-craft an old-style inventory.json without `last_built_sha`.
        let legacy = r#"{
            "old/repo": {
                "cloned_at": "2024-01-01T00:00:00",
                "last_accessed": "2024-01-01T00:00:00",
                "access_count": 5,
                "stale": false
            }
        }"#;
        std::fs::write(dir.path().join("inventory.json"), legacy).unwrap();
        // Re-open and confirm graceful read.
        let ws2 = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert_eq!(ws2.last_built_sha("old/repo"), None);
        let _ = ws;
    }

    #[test]
    fn auto_rebuild_gate_skips_when_sha_matches() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = calls.clone();
        let hook: PostActivateHook = Arc::new(move |_path, _name| {
            calls_h.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        // Build a workspace pointing at a tempdir with a fake repo dir,
        // then simulate consecutive activates. We can't drive clone_or_update
        // without git, so test the gating directly by tracking the SHA
        // record-then-re-record case via Workspace::record_built +
        // last_built_sha — the same predicate `activate` uses.
        let ws = Workspace::open(dir.path().to_path_buf(), 7, Some(hook)).unwrap();
        // Seed inventory entry + initial sha record.
        ws.bump_access("acme/widgets", "cloned");
        ws.record_built("acme/widgets", "sha_one", None);
        assert_eq!(
            ws.last_built_sha("acme/widgets").as_deref(),
            Some("sha_one")
        );
        // Repeated record with the same value is idempotent (gating
        // logic uses last_built_sha as the source of truth).
        ws.record_built("acme/widgets", "sha_one", None);
        assert_eq!(
            ws.last_built_sha("acme/widgets").as_deref(),
            Some("sha_one")
        );
        // No hook calls have been driven directly — this test exercises
        // the persistence path that the gate consults.
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn local_workspace_binds_root_immediately() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        assert_eq!(ws.kind(), WorkspaceKind::Local);
        assert!(ws.active_repo_path().is_some());
        assert!(ws.active_repo_name().unwrap().starts_with("local/"));
    }

    #[test]
    fn local_workspace_rejects_github_ops() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        let out = ws.repo_management(Some("acme/widgets"), false, false, false, None);
        assert!(out.contains("does not accept a repo name"));
        let out = ws.repo_management(None, true, false, false, None);
        assert!(out.contains("does not support `delete`"));
    }

    #[test]
    fn local_workspace_update_rebuilds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        // Drop a file so the fingerprint has something to hash.
        std::fs::write(dir.path().join("x.txt"), b"hi").unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = calls.clone();
        let hook: PostActivateHook = Arc::new(move |_p, _n| {
            calls_h.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let ws = Workspace::open_local(dir.path().to_path_buf(), Some(hook)).unwrap();
        // First update: nothing built yet → hook fires.
        let _ = ws.repo_management(None, false, true, false, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Second update without changes → SHA matches → hook skipped.
        let out = ws.repo_management(None, false, true, false, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "auto-rebuild gate must skip"
        );
        assert!(out.contains("build skipped"));
    }

    #[test]
    fn parses_github_remote_forms() {
        assert_eq!(
            parse_github_remote("git@github.com:kkollsga/kglite.git").as_deref(),
            Some("kkollsga/kglite")
        );
        assert_eq!(
            parse_github_remote("https://github.com/kkollsga/kglite.git").as_deref(),
            Some("kkollsga/kglite")
        );
        // No .git suffix, trailing slash.
        assert_eq!(
            parse_github_remote("https://github.com/acme/widget/").as_deref(),
            Some("acme/widget")
        );
        assert_eq!(
            parse_github_remote("ssh://git@github.com/acme/widget.git").as_deref(),
            Some("acme/widget")
        );
        // Non-github / malformed → None.
        assert_eq!(
            parse_github_remote("https://gitlab.com/acme/widget.git"),
            None
        );
        assert_eq!(parse_github_remote("git@github.com:acme.git"), None);
        assert_eq!(parse_github_remote("not a url"), None);
    }

    #[test]
    fn local_default_github_repo_uses_origin_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // Stand up a real git repo with a faked origin so default_github_repo
        // exercises the actual `git remote get-url` path.
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init"]).status.success() {
            // git unavailable in this environment — skip rather than fail.
            return;
        }
        git(&[
            "remote",
            "add",
            "origin",
            "https://github.com/acme/widget.git",
        ]);
        let ws = Workspace::open_local(root.to_path_buf(), None).unwrap();
        assert_eq!(
            ws.default_github_repo().as_deref(),
            Some("acme/widget"),
            "local default repo must come from the origin remote, not the inventory key"
        );
        // The inventory key remains the synthetic local name.
        assert!(ws.active_repo_name().unwrap().starts_with("local/"));
    }

    #[test]
    fn local_default_github_repo_none_without_remote() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        // No git remote → None, and crucially NOT Some("local/<dir>").
        let def = ws.default_github_repo();
        assert!(
            def.is_none(),
            "expected None for a non-git local root, got {def:?}"
        );
    }

    #[test]
    fn set_root_dir_only_in_local_mode() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.set_root_dir(dir.path(), None);
        assert!(out.contains("only valid in local-workspace"));
    }

    #[test]
    fn update_with_no_active_repo() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let out = ws.repo_management(None, false, true, false, None);
        assert!(out.contains("No active repository"));
    }

    #[test]
    fn set_root_dir_updates_active_path() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        let _ = ws.set_root_dir(&child, None);
        assert_eq!(
            ws.active_repo_path().unwrap(),
            child.canonicalize().unwrap(),
            "set_root_dir didn't update active_repo_path"
        );
    }

    #[test]
    fn set_root_dir_post_activate_fires_against_new_root() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(child.join("a.txt"), b"hi").unwrap();
        let seen_path: Arc<std::sync::Mutex<Option<PathBuf>>> = Arc::new(Default::default());
        let seen = seen_path.clone();
        let hook: PostActivateHook = Arc::new(move |p, _n| {
            *seen.lock().unwrap() = Some(p.to_path_buf());
            Ok(())
        });
        let ws = Workspace::open_local(dir.path().to_path_buf(), Some(hook)).unwrap();
        let _ = ws.set_root_dir(&child, None);
        assert_eq!(
            seen_path.lock().unwrap().clone().unwrap(),
            child.canonicalize().unwrap(),
            "post_activate hook saw the wrong root after set_root_dir"
        );
    }

    /// Containment-test layout: `<base>/sandbox/child` and a sibling
    /// `<base>/outside`, with **every path canonicalized**. macOS tempdirs
    /// live under the `/var` → `/private/var` symlink, so an
    /// un-canonicalized boundary would make every `starts_with` assertion
    /// (and every raw-vs-canonical mutation) vacuously true.
    fn sandbox_layout() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let sandbox = base.join("sandbox");
        let inside = sandbox.join("child");
        let outside = base.join("outside");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        (td, sandbox, inside, outside)
    }

    #[test]
    fn set_root_dir_outside_sandbox_root_rejected_and_active_root_unchanged() {
        let (_td, sandbox, _inside, outside) = sandbox_layout();
        let ws = Workspace::open_local(sandbox.clone(), None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();
        let before = ws.active_repo_path().unwrap();
        assert_eq!(before, sandbox);

        let out = ws.set_root_dir(&outside, None);
        assert!(
            out.contains("sandbox_root") && out.contains(&sandbox.display().to_string()),
            "rejection must name the boundary it violated, got: {out}"
        );
        // The failure that matters is a partial activation, not the string.
        assert_eq!(
            ws.active_repo_path().unwrap(),
            before,
            "a rejected swap must leave the active root untouched"
        );
    }

    #[test]
    fn set_root_dir_inside_sandbox_root_activates() {
        let (_td, sandbox, inside, _outside) = sandbox_layout();
        let ws = Workspace::open_local(sandbox.clone(), None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();
        let out = ws.set_root_dir(&inside, None);
        assert_eq!(
            ws.active_repo_path().unwrap(),
            inside,
            "a target inside the boundary must activate; set_root_dir said: {out}"
        );
    }

    #[test]
    fn set_root_dir_dotdot_traversal_out_of_sandbox_rejected() {
        let (_td, sandbox, inside, outside) = sandbox_layout();
        let ws = Workspace::open_local(sandbox.clone(), None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();
        // Lexically inside the boundary, actually outside it — only the
        // canonicalized path reveals the escape.
        let traversal = inside.join("..").join("..").join("outside");
        assert!(
            traversal.starts_with(&sandbox),
            "test is meaningless unless the raw path looks contained"
        );
        let out = ws.set_root_dir(&traversal, None);
        assert!(
            out.contains("sandbox_root"),
            "`..` escape must be rejected, got: {out}"
        );
        assert_eq!(ws.active_repo_path().unwrap(), sandbox);
        assert_ne!(ws.active_repo_path().unwrap(), outside);
    }

    #[cfg(unix)]
    #[test]
    fn set_root_dir_symlink_out_of_sandbox_rejected() {
        let (_td, sandbox, _inside, outside) = sandbox_layout();
        let link = sandbox.join("escape-hatch");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let ws = Workspace::open_local(sandbox.clone(), None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();
        assert!(
            link.starts_with(&sandbox),
            "test is meaningless unless the raw path looks contained"
        );
        let out = ws.set_root_dir(&link, None);
        assert!(
            out.contains("sandbox_root"),
            "symlink escape must be rejected, got: {out}"
        );
        assert_eq!(ws.active_repo_path().unwrap(), sandbox);
    }

    #[test]
    fn no_sandbox_root_configured_keeps_swaps_unbounded() {
        // The backwards-compatibility bar: without the opt-in key an
        // arbitrary sibling directory still activates, exactly as before.
        let (_td, sandbox, _inside, outside) = sandbox_layout();
        let ws = Workspace::open_local(sandbox, None).unwrap();
        let out = ws.set_root_dir(&outside, None);
        assert_eq!(
            ws.active_repo_path().unwrap(),
            outside,
            "unbounded default broken; set_root_dir said: {out}"
        );
    }

    #[test]
    fn with_sandbox_root_rejects_active_root_outside_the_boundary() {
        // A manifest whose `root` sits outside its own `sandbox_root`
        // contradicts itself — it must die at boot, not at the first swap.
        let (_td, sandbox, _inside, outside) = sandbox_layout();
        let err = Workspace::open_local(outside.clone(), None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .map(|_| ())
            .expect_err("root outside the boundary must not boot");
        let msg = err.to_string();
        assert!(
            msg.contains(&sandbox.display().to_string())
                && msg.contains(&outside.display().to_string()),
            "boot error must name both the root and the boundary, got: {msg}"
        );
    }

    #[test]
    fn with_sandbox_root_accepts_root_equal_to_the_boundary() {
        let (_td, sandbox, inside, _outside) = sandbox_layout();
        assert!(Workspace::open_local(sandbox.clone(), None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .is_ok());
        // …and a root strictly inside it.
        assert!(Workspace::open_local(inside, None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .is_ok());
    }

    #[test]
    fn with_sandbox_root_rejects_github_workspaces_and_missing_dirs() {
        let (_td, sandbox, _inside, _outside) = sandbox_layout();
        let gh = Workspace::open(sandbox.join("gh"), 7, None).unwrap();
        assert!(
            gh.with_sandbox_root(&sandbox)
                .map(|_| ())
                .unwrap_err()
                .to_string()
                .contains("only valid for local"),
            "sandbox_root on a github workspace must be a loud error"
        );
        let missing = sandbox.join("nope");
        assert!(Workspace::open_local(sandbox, None)
            .unwrap()
            .with_sandbox_root(&missing)
            .is_err());
    }

    // ------------------------------------------------------------------
    // Unanchored boot + client-root adoption (`workspace.adopt_client_roots`)
    // ------------------------------------------------------------------

    #[test]
    fn unanchored_boot_binds_nothing_and_creates_nothing() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let ws = Workspace::open_local_unanchored(None).unwrap();
        assert!(ws.active_repo_path().is_none());
        assert!(ws.active_repo_name().is_none());
        assert_eq!(ws.root_ownership(), RootOwnership::Unowned);
        assert!(!ws.adopts_client_roots(), "the knob is opt-in");
        assert!(
            !base.join(".mcp-workspace").exists(),
            "an unanchored boot must not create an inventory dir anywhere"
        );
    }

    #[test]
    fn open_local_is_operator_owned_from_the_start() {
        let td = tempfile::tempdir().unwrap();
        let ws = Workspace::open_local(td.path().to_path_buf(), None).unwrap();
        assert_eq!(
            ws.root_ownership(),
            RootOwnership::Operator,
            "a configured root is the operator's, which is what makes adoption fallback-only"
        );
    }

    #[test]
    fn adopt_client_root_activates_and_defers_the_inventory_dir() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let ws = Workspace::open_local_unanchored(None).unwrap();

        ws.adopt_client_root(&project).unwrap();

        assert_eq!(ws.active_repo_path().as_deref(), Some(project.as_path()));
        assert_eq!(ws.root_ownership(), RootOwnership::Adopted);
        assert!(
            project.join(".mcp-workspace").is_dir(),
            "the first activation must create the deferred inventory dir"
        );
        assert_eq!(ws.workspace_dir(), project.as_path());
    }

    #[test]
    fn the_inventory_home_is_fixed_at_the_first_adoption() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let first = base.join("first");
        let second = base.join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let ws = Workspace::open_local_unanchored(None).unwrap();
        ws.adopt_client_root(&first).unwrap();
        ws.set_root_dir(&second, None);

        assert_eq!(ws.active_repo_path().as_deref(), Some(second.as_path()));
        assert_eq!(
            ws.workspace_dir(),
            first.as_path(),
            "the inventory must survive later root swaps, exactly as it does after open_local"
        );
        assert!(!second.join(".mcp-workspace").exists());
    }

    #[test]
    fn adoption_is_refused_once_the_operator_owns_the_root() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let configured = base.join("configured");
        let advertised = base.join("advertised");
        std::fs::create_dir_all(&configured).unwrap();
        std::fs::create_dir_all(&advertised).unwrap();

        let ws = Workspace::open_local(configured.clone(), None).unwrap();
        let err = ws.adopt_client_root(&advertised).unwrap_err();
        assert!(err.contains("operator"), "unexpected reason: {err}");
        assert_eq!(
            ws.active_repo_path().as_deref(),
            Some(configured.as_path()),
            "a refused adoption must not touch the active root"
        );
    }

    #[test]
    fn set_root_dir_claims_ownership_permanently() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let adopted = base.join("adopted");
        let operator = base.join("operator");
        let later = base.join("later");
        for d in [&adopted, &operator, &later] {
            std::fs::create_dir_all(d).unwrap();
        }
        let ws = Workspace::open_local_unanchored(None).unwrap();
        ws.adopt_client_root(&adopted).unwrap();
        assert_eq!(ws.root_ownership(), RootOwnership::Adopted);

        ws.set_root_dir(&operator, None);
        assert_eq!(ws.root_ownership(), RootOwnership::Operator);

        // Which is exactly what a later `roots/list_changed` runs into.
        assert!(ws.adopt_client_root(&later).is_err());
        assert_eq!(ws.active_repo_path().as_deref(), Some(operator.as_path()));
    }

    #[test]
    fn a_failed_set_root_dir_does_not_claim_ownership() {
        let (_td, sandbox, inside, outside) = sandbox_layout();
        let ws = Workspace::open_local_unanchored(None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();
        let msg = ws.set_root_dir(&outside, None);
        assert!(msg.contains("sandbox_root"), "unexpected message: {msg}");
        assert_eq!(
            ws.root_ownership(),
            RootOwnership::Unowned,
            "a rejected swap must not lock out adoption"
        );
        // ... so a valid client root can still be adopted afterwards.
        ws.adopt_client_root(&inside).unwrap();
        assert_eq!(ws.root_ownership(), RootOwnership::Adopted);
    }

    #[test]
    fn adoption_goes_through_the_same_containment_check_as_set_root_dir() {
        let (_td, sandbox, inside, outside) = sandbox_layout();
        let ws = Workspace::open_local_unanchored(None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();

        let err = ws.adopt_client_root(&outside).unwrap_err();
        assert!(
            err.contains("sandbox_root") && err.contains(&sandbox.display().to_string()),
            "the rejection must name the boundary it violated: {err}"
        );
        assert!(
            ws.active_repo_path().is_none(),
            "a rejected adoption must leave the server unanchored"
        );
        assert_eq!(ws.root_ownership(), RootOwnership::Unowned);

        ws.adopt_client_root(&inside).unwrap();
        assert_eq!(ws.active_repo_path().as_deref(), Some(inside.as_path()));
    }

    #[test]
    fn adoption_rejects_a_dotdot_escape_from_the_sandbox() {
        let (_td, sandbox, inside, outside) = sandbox_layout();
        let ws = Workspace::open_local_unanchored(None)
            .unwrap()
            .with_sandbox_root(&sandbox)
            .unwrap();
        // Lexically inside, actually outside — only the canonicalized form
        // catches it.
        let traversal = inside.join("..").join("..").join("outside");
        assert!(ws.adopt_client_root(&traversal).is_err());
        assert!(ws.active_repo_path().is_none());
        let _ = outside;
    }

    #[test]
    fn unanchored_refresh_before_adoption_is_a_clean_error() {
        let ws = Workspace::open_local_unanchored(None).unwrap();
        let out = ws.repo_management(None, false, true, false, None);
        // The exact message, not a family of them: `repo_management`
        // refuses here, before `activate` is ever called, and asserting
        // loosely would let a second (unreachable) error string pass for
        // coverage it does not have.
        assert_eq!(out, "No active local root.", "unexpected output: {out}");
        assert!(ws.active_repo_path().is_none());
    }

    /// A build that fails must leave nothing of itself in a root the
    /// *client* proposed: no `.mcp-workspace/` created inside it, and no
    /// permanently-fixed inventory anchor pointing at a root that never
    /// activated.
    #[test]
    fn a_failed_first_adoption_writes_nothing_into_the_clients_root() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let rejected = base.join("rejected");
        let good = base.join("good");
        std::fs::create_dir_all(&rejected).unwrap();
        std::fs::create_dir_all(&good).unwrap();

        let hook: PostActivateHook = {
            let rejected = rejected.clone();
            Arc::new(move |path, _name| {
                if path == rejected {
                    anyhow::bail!("builder refused this root");
                }
                Ok(())
            })
        };
        let ws = Workspace::open_local_unanchored(Some(hook)).unwrap();

        assert!(
            ws.adopt_client_root(&rejected).is_err(),
            "the hook refused, so the adoption must fail"
        );
        assert!(
            !rejected.join(".mcp-workspace").exists(),
            "a failed adoption must not create a directory inside the client's root"
        );
        assert!(ws.active_repo_path().is_none(), "nothing activated");
        assert_eq!(
            ws.workspace_dir(),
            Path::new(""),
            "a failed attempt must not fix the inventory anchor"
        );

        // ... so the *next* root, the first one that actually commits, is
        // the one that gets to own the inventory.
        ws.adopt_client_root(&good).unwrap();
        assert_eq!(ws.workspace_dir(), good.as_path());
        assert!(good.join(".mcp-workspace").is_dir());
        assert!(
            good.join(".mcp-workspace").join("inventory.json").is_file(),
            "the anchoring activation must still write its inventory receipt"
        );
        assert!(!rejected.join(".mcp-workspace").exists());
    }

    /// `swap_root`'s mode check names the entry point that called it, so a
    /// rejection logged by the adoption path never claims to be about
    /// `set_root_dir`. (Reachable only through `swap_root` itself: a
    /// github workspace is `Operator`-owned, so `adopt_client_root`
    /// refuses one step earlier.)
    #[test]
    fn a_non_local_swap_names_the_caller_that_attempted_it() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        let err = ws
            .swap_root(dir.path(), None, "adopt_client_root")
            .unwrap_err();
        assert_eq!(
            err, "adopt_client_root is only valid in local-workspace mode.",
            "the message must name the caller: {err}"
        );
    }

    /// A one-shot gate: a flag plus a condvar, so two threads can be
    /// sequenced by *signal* rather than by sleeping.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        cv: std::sync::Condvar,
    }

    impl Gate {
        fn open(&self) {
            *self.open.lock().unwrap() = true;
            self.cv.notify_all();
        }

        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.cv.wait(open).unwrap();
            }
        }

        /// Wait, but give up after `limit`. Used where the *point* of the
        /// fix is that the awaited signal never comes.
        fn wait_until(&self, limit: std::time::Duration) {
            let open = self.open.lock().unwrap();
            let _ = self
                .cv
                .wait_timeout_while(open, limit, |open| !*open)
                .unwrap();
        }
    }

    /// A client-root adoption may not interleave with an operator swap.
    ///
    /// The window this closes: adoption passes its ownership check, the
    /// operator's `set_root_dir` swaps to its own root, adoption's
    /// activation commits *last*, and the ownership publications land
    /// `Adopted` then `Operator`. The result was the **client's** root
    /// active while flagged as the operator's — after the operator's tool
    /// call had already returned.
    ///
    /// Sequencing is by signal, not by sleep. The operator's activation is
    /// held inside the transaction hook until the adoption's own hook
    /// reports that it got past the ownership check and into preparation —
    /// precisely the state the race needs, and the reason the failure is
    /// deterministic without the fix. *With* the fix the adoption never
    /// reaches that point (it waits on the swap ordering lock), so the
    /// operator's wait falls through on its belt; the outcome is then the
    /// same for every interleaving, which is the whole point.
    #[test]
    fn an_adoption_cannot_displace_a_concurrent_operator_swap() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let client_root = base.join("client");
        let operator_root = base.join("operator");
        std::fs::create_dir_all(&client_root).unwrap();
        std::fs::create_dir_all(&operator_root).unwrap();

        let operator_in_hook = Arc::new(Gate::default());
        let adoption_in_hook = Arc::new(Gate::default());
        let hook: ActivationTransactionHook = {
            let client_root = client_root.clone();
            let operator_in_hook = operator_in_hook.clone();
            let adoption_in_hook = adoption_in_hook.clone();
            Arc::new(move |request| {
                if request.path() == client_root {
                    adoption_in_hook.open();
                } else {
                    operator_in_hook.open();
                    adoption_in_hook.wait_until(std::time::Duration::from_secs(2));
                }
                Ok(PreparedActivation::summary(None))
            })
        };
        // Unanchored, so adoption is permitted at all.
        let ws = Workspace::open_local_unanchored(None)
            .unwrap()
            .with_activation_transaction(hook);

        let operator = {
            let ws = ws.clone();
            let target = operator_root.clone();
            std::thread::spawn(move || ws.set_root_dir(&target, None))
        };
        // The operator now holds an activation id and is mid-build.
        operator_in_hook.wait();
        let adoption = {
            let ws = ws.clone();
            let target = client_root.clone();
            std::thread::spawn(move || ws.adopt_client_root(&target))
        };

        let operator_out = operator.join().unwrap();
        let adoption_out = adoption.join().unwrap();

        assert_eq!(
            ws.active_repo_path().as_deref(),
            Some(operator_root.as_path()),
            "a client root displaced the operator's swap \
             (operator said: {operator_out}; adoption said: {adoption_out:?})"
        );
        assert_eq!(
            ws.root_ownership(),
            RootOwnership::Operator,
            "the surviving root must also be flagged as the operator's"
        );
        assert!(
            adoption_out.is_err(),
            "the adoption ran second and must have been refused: {adoption_out:?}"
        );
    }

    /// The mirror image: an operator swap issued while an adoption is
    /// already mid-activation still ends as the operator's root. The
    /// adoption completes first (it holds the swap lock), the operator's
    /// swap then lands on top of it — no ordering leaves the client's root
    /// active.
    #[test]
    fn an_operator_swap_wins_when_an_adoption_is_already_mid_activation() {
        let td = tempfile::tempdir().unwrap();
        let base = td.path().canonicalize().unwrap();
        let client_root = base.join("client");
        let operator_root = base.join("operator");
        std::fs::create_dir_all(&client_root).unwrap();
        std::fs::create_dir_all(&operator_root).unwrap();

        let adoption_in_hook = Arc::new(Gate::default());
        let release_adoption = Arc::new(Gate::default());
        let hook: ActivationTransactionHook = {
            let client_root = client_root.clone();
            let adoption_in_hook = adoption_in_hook.clone();
            let release_adoption = release_adoption.clone();
            Arc::new(move |request| {
                if request.path() == client_root {
                    adoption_in_hook.open();
                    release_adoption.wait();
                }
                Ok(PreparedActivation::summary(None))
            })
        };
        let ws = Workspace::open_local_unanchored(None)
            .unwrap()
            .with_activation_transaction(hook);

        let adoption = {
            let ws = ws.clone();
            let target = client_root.clone();
            std::thread::spawn(move || ws.adopt_client_root(&target))
        };
        adoption_in_hook.wait();
        let operator = {
            let ws = ws.clone();
            let target = operator_root.clone();
            std::thread::spawn(move || ws.set_root_dir(&target, None))
        };
        release_adoption.open();

        let adoption_out = adoption.join().unwrap();
        let operator_out = operator.join().unwrap();

        assert_eq!(
            ws.active_repo_path().as_deref(),
            Some(operator_root.as_path()),
            "the operator's swap must survive an in-flight adoption \
             (adoption said: {adoption_out:?}; operator said: {operator_out})"
        );
        assert_eq!(ws.root_ownership(), RootOwnership::Operator);
    }

    #[test]
    fn activation_summary_appended_to_activate_message() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let summary: ActivationSummaryHook =
            Arc::new(|_p, _n| Some("Graph ready: 3 Functions.".to_string()));
        let ws = Workspace::open_local(dir.path().to_path_buf(), None)
            .unwrap()
            .with_activation_summary(summary);
        let out = ws.repo_management(None, false, true, false, None);
        assert!(
            out.contains("Graph ready: 3 Functions."),
            "activation message should include the summary; got: {out}"
        );
    }

    #[test]
    fn activation_summary_absent_when_not_configured() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        let ws = Workspace::open_local(dir.path().to_path_buf(), None).unwrap();
        let out = ws.repo_management(None, false, true, false, None);
        assert!(!out.contains("Graph ready"));
        assert!(
            out.contains(" at "),
            "expected the terse default message; got: {out}"
        );
    }

    #[test]
    fn hook_fires_once_per_process_even_when_sha_matches() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Local mode fingerprints the dir instead of a git SHA, so we can
        // drive the real `activate` path without git. A stable file keeps
        // the fingerprint constant across both simulated processes.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"stable").unwrap();

        let calls = Arc::new(AtomicUsize::new(0));
        let make_hook = || -> PostActivateHook {
            let c = calls.clone();
            Arc::new(move |_p, _n| {
                c.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };

        // --- Process 1 ---------------------------------------------------
        let ws = Workspace::open_local(dir.path().to_path_buf(), Some(make_hook())).unwrap();
        // First activate (fingerprint not yet recorded) → hook fires.
        let _ = ws.repo_management(None, false, true, false, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "first activate must hydrate"
        );
        // Second activate, same process, unchanged fingerprint → cheap-skip.
        let out = ws.repo_management(None, false, true, false, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "repeat activate in same process must skip the hook"
        );
        assert!(
            out.contains("build skipped"),
            "expected skip suffix, got: {out}"
        );
        drop(ws);

        // --- Process 2 (restart) ----------------------------------------
        // Same dir → inventory.json + last_built_sha persist, but the
        // in-memory hydration set does not. The first activate here must
        // re-fire the hook to rehydrate the consumer's in-memory state.
        let ws2 = Workspace::open_local(dir.path().to_path_buf(), Some(make_hook())).unwrap();
        assert!(
            ws2.last_built_sha(&ws2.active_repo_name().unwrap())
                .is_some(),
            "sanity: last_built_sha should survive the restart"
        );
        let _ = ws2.repo_management(None, false, true, false, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "fresh process must re-fire the hook even when the SHA matches"
        );
    }

    #[test]
    fn a_b_a_swap_rebuilds_intervening_root() {
        // Regression for the single-slot-consumer stale-graph bug: an
        // A→B→A swap must rebuild A on the second bind, because activating
        // B overwrote the consumer's single live slot. Before the fix the
        // skip gate keyed off "A was hydrated at some point this process"
        // and wrongly skipped, leaving B's product live under A's name.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("projA");
        let b = root.path().join("projB");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        // Stable, distinct contents so each root's fingerprint holds
        // constant across re-binds (so `action == "current"` on the
        // second bind of A — the exact condition the gate keys on).
        std::fs::write(a.join("a.txt"), b"alpha").unwrap();
        std::fs::write(b.join("b.txt"), b"beta").unwrap();

        // The hook records which root it last built into the single slot,
        // mirroring a single-active-graph consumer.
        let built: Arc<std::sync::Mutex<Option<PathBuf>>> = Arc::new(Default::default());
        let built_h = built.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_h = calls.clone();
        let hook: PostActivateHook = Arc::new(move |p, _n| {
            *built_h.lock().unwrap() = Some(p.to_path_buf());
            calls_h.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });

        let ws = Workspace::open_local(a.clone(), Some(hook)).unwrap();
        // open_local binds A but doesn't fire the hook; first set_root_dir(A)
        // hydrates it.
        let _ = ws.set_root_dir(&a, None);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "first bind of A hydrates");
        assert_eq!(
            built.lock().unwrap().clone(),
            Some(a.canonicalize().unwrap())
        );

        let _ = ws.set_root_dir(&b, None);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "bind of B rebuilds");
        assert_eq!(
            built.lock().unwrap().clone(),
            Some(b.canonicalize().unwrap())
        );

        // The bug: re-binding A must rebuild (slot currently holds B), not
        // cheap-skip. The single slot must end up holding A again.
        let out = ws.set_root_dir(&a, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "A→B→A must rebuild A; the intervening B overwrote the live slot"
        );
        assert!(
            !out.contains("build skipped"),
            "re-bind of a non-active root must not skip; got: {out}"
        );
        assert_eq!(
            built.lock().unwrap().clone(),
            Some(a.canonicalize().unwrap()),
            "after A→B→A the live slot must hold A, not B"
        );

        // And an immediate re-bind of the *currently active* root (A→A)
        // still cheap-skips — the win the gate was added for is preserved.
        let out = ws.set_root_dir(&a, None);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            3,
            "re-binding the already-active root must skip the hook"
        );
        assert!(
            out.contains("build skipped"),
            "expected skip suffix, got: {out}"
        );
    }

    #[test]
    fn transaction_slow_a_fast_b_discards_stale_build_and_keeps_responses_coherent() {
        use std::sync::Barrier;

        #[derive(Debug, Clone, PartialEq, Eq)]
        struct Installed {
            id: ActivationId,
            path: PathBuf,
        }

        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("slow-a");
        let b = root.path().join("fast-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("a.txt"), b"a").unwrap();
        std::fs::write(b.join("b.txt"), b"b").unwrap();
        let a = a.canonicalize().unwrap();
        let b = b.canonicalize().unwrap();

        let a_entered = Arc::new(Barrier::new(2));
        let release_a = Arc::new(Barrier::new(2));
        let installed: Arc<Mutex<Option<Installed>>> = Arc::new(Mutex::new(None));
        let hook: ActivationTransactionHook = {
            let a = a.clone();
            let a_entered = a_entered.clone();
            let release_a = release_a.clone();
            let installed = installed.clone();
            Arc::new(move |request| {
                if request.path() == a {
                    a_entered.wait();
                    release_a.wait();
                }
                let product = Installed {
                    id: request.id(),
                    path: request.path().to_path_buf(),
                };
                let installed = installed.clone();
                Ok(PreparedActivation::new(move || {
                    *installed.lock().unwrap() = Some(product.clone());
                    Ok(Some(format!(
                        "product {} for {}",
                        product.id,
                        product.path.display()
                    )))
                }))
            })
        };
        let ws = Workspace::open_local(a.clone(), None)
            .unwrap()
            .with_activation_transaction(hook);

        let slow_ws = ws.clone();
        let slow_a = a.clone();
        let slow = std::thread::spawn(move || slow_ws.set_root_dir(&slow_a, None));
        a_entered.wait();

        let fast_ws = ws.clone();
        let fast_b = b.clone();
        let fast = std::thread::spawn(move || fast_ws.set_root_dir(&fast_b, None));
        let fast_out = fast.join().unwrap();
        release_a.wait();
        let slow_out = slow.join().unwrap();

        assert!(
            fast_out.contains(&b.display().to_string())
                && fast_out.contains("product 2")
                && !fast_out.contains(&a.display().to_string()),
            "fast request response must describe only its own committed product: {fast_out}"
        );
        assert!(
            slow_out.contains("request 1")
                && slow_out.contains("superseded by request 2")
                && !slow_out.contains("product 1"),
            "stale request must report supersession, not a false activation: {slow_out}"
        );
        assert_eq!(ws.active_repo_path(), Some(b.clone()));
        assert_eq!(installed.lock().unwrap().as_ref().unwrap().path, b);
        assert_eq!(
            ws.inner
                .state
                .read()
                .unwrap()
                .active_build
                .as_ref()
                .unwrap()
                .activation_id,
            ActivationId(2),
            "latest request must own the final framework state"
        );
    }

    #[test]
    fn legacy_callbacks_are_serialized_through_build_and_summary() {
        use std::sync::Barrier;

        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("slow-a");
        let b = root.path().join("queued-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let a = a.canonicalize().unwrap();
        let b = b.canonicalize().unwrap();
        let a_entered = Arc::new(Barrier::new(2));
        let release_a = Arc::new(Barrier::new(2));
        let installed: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
        let hook: PostActivateHook = {
            let a = a.clone();
            let a_entered = a_entered.clone();
            let release_a = release_a.clone();
            let installed = installed.clone();
            Arc::new(move |path, _name| {
                if path == a {
                    a_entered.wait();
                    release_a.wait();
                }
                *installed.lock().unwrap() = Some(path.to_path_buf());
                Ok(())
            })
        };
        let summary: ActivationSummaryHook = {
            let installed = installed.clone();
            Arc::new(move |_path, _name| {
                installed
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|path| format!("legacy product {}", path.display()))
            })
        };
        let ws = Workspace::open_local(a.clone(), Some(hook))
            .unwrap()
            .with_activation_summary(summary);

        let a_ws = ws.clone();
        let a_root = a.clone();
        let a_thread = std::thread::spawn(move || a_ws.set_root_dir(&a_root, None));
        a_entered.wait();
        let b_ws = ws.clone();
        let b_root = b.clone();
        let b_thread = std::thread::spawn(move || b_ws.set_root_dir(&b_root, None));
        release_a.wait();
        let a_out = a_thread.join().unwrap();
        let b_out = b_thread.join().unwrap();

        assert!(
            a_out.contains(&format!("legacy product {}", a.display()))
                && !a_out.contains(&format!("legacy product {}", b.display())),
            "legacy A response crossed activation products: {a_out}"
        );
        assert!(
            b_out.contains(&format!("legacy product {}", b.display()))
                && !b_out.contains(&format!("legacy product {}", a.display())),
            "legacy B response crossed activation products: {b_out}"
        );
        assert_eq!(ws.active_repo_path(), Some(b.clone()));
        assert_eq!(*installed.lock().unwrap(), Some(b));
    }

    #[test]
    fn transaction_same_root_plain_vs_revisions_is_generation_ordered() {
        use std::sync::Barrier;

        let Some((_dir, root)) = git_repo_with_tags(&["v1.0.0", "v2.0.0"]) else {
            return;
        };
        let root = root.canonicalize().unwrap();
        let plain_entered = Arc::new(Barrier::new(2));
        let release_plain = Arc::new(Barrier::new(2));
        let installed: Arc<Mutex<Option<(ActivationId, ActivationBuild)>>> =
            Arc::new(Mutex::new(None));
        let hook: ActivationTransactionHook = {
            let plain_entered = plain_entered.clone();
            let release_plain = release_plain.clone();
            let installed = installed.clone();
            Arc::new(move |request| {
                if matches!(request.build(), ActivationBuild::Plain) {
                    plain_entered.wait();
                    release_plain.wait();
                }
                let id = request.id();
                let build = request.build().clone();
                let installed = installed.clone();
                Ok(PreparedActivation::new(move || {
                    *installed.lock().unwrap() = Some((id, build.clone()));
                    Ok(Some(format!("installed request {id}: {build:?}")))
                }))
            })
        };
        let ws = Workspace::open_local(root.clone(), None)
            .unwrap()
            .with_activation_transaction(hook);

        let plain_ws = ws.clone();
        let plain_root = root.clone();
        let plain = std::thread::spawn(move || plain_ws.set_root_dir(&plain_root, None));
        plain_entered.wait();

        let revs_ws = ws.clone();
        let revs_root = root.clone();
        let revs = std::thread::spawn(move || {
            revs_ws.set_root_dir(&revs_root, Some(&RevsRequest::Count(2)))
        });
        let revs_out = revs.join().unwrap();
        release_plain.wait();
        let plain_out = plain.join().unwrap();

        assert!(revs_out.contains("revs: v1.0.0, v2.0.0, HEAD"));
        assert!(revs_out.contains("installed request 2: Revisions"));
        assert!(plain_out.contains("superseded by request 2"));
        let state = ws.inner.state.read().unwrap();
        assert_eq!(state.active_repo_path.as_deref(), Some(root.as_path()));
        assert_eq!(
            state
                .active_build
                .as_ref()
                .and_then(|built| built.resolved_revs.clone()),
            Some(vec!["v1.0.0".into(), "v2.0.0".into(), "HEAD".into()])
        );
        assert!(matches!(
            installed.lock().unwrap().as_ref(),
            Some((ActivationId(2), ActivationBuild::Revisions(_)))
        ));
    }

    #[test]
    fn transaction_current_failure_preserves_committed_source_and_product() {
        #[derive(Debug, Clone, PartialEq, Eq)]
        struct Installed(ActivationId, PathBuf);

        let root = tempfile::tempdir().unwrap();
        let good = root.path().join("good");
        let broken = root.path().join("broken");
        std::fs::create_dir_all(&good).unwrap();
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(good.join("good.txt"), b"good").unwrap();
        std::fs::write(broken.join("broken.txt"), b"broken").unwrap();
        let good = good.canonicalize().unwrap();
        let broken = broken.canonicalize().unwrap();

        let installed: Arc<Mutex<Option<Installed>>> = Arc::new(Mutex::new(None));
        let hook: ActivationTransactionHook = {
            let broken = broken.clone();
            let installed = installed.clone();
            Arc::new(move |request| {
                if request.path() == broken {
                    anyhow::bail!("builder rejected broken root");
                }
                let product = Installed(request.id(), request.path().to_path_buf());
                let installed = installed.clone();
                Ok(PreparedActivation::new(move || {
                    *installed.lock().unwrap() = Some(product.clone());
                    Ok(Some(format!("installed request {}", product.0)))
                }))
            })
        };
        let ws = Workspace::open_local(good.clone(), None)
            .unwrap()
            .with_activation_transaction(hook);

        let good_out = ws.set_root_dir(&good, None);
        assert!(good_out.contains("installed request 1"));
        let broken_out = ws.set_root_dir(&broken, None);
        assert!(
            broken_out.contains("request 2")
                && broken_out.contains("failed during preparation")
                && broken_out.contains("builder rejected broken root"),
            "failure must be explicit and request-scoped: {broken_out}"
        );
        assert_eq!(ws.active_repo_path(), Some(good.clone()));
        assert_eq!(installed.lock().unwrap().as_ref().unwrap().1, good);
    }

    #[test]
    fn transaction_stale_failure_reports_superseded_not_current_failure() {
        use std::sync::Barrier;

        let root = tempfile::tempdir().unwrap();
        let slow = root.path().join("slow-failure");
        let fast = root.path().join("fast-success");
        std::fs::create_dir_all(&slow).unwrap();
        std::fs::create_dir_all(&fast).unwrap();
        let slow = slow.canonicalize().unwrap();
        let fast = fast.canonicalize().unwrap();
        let slow_entered = Arc::new(Barrier::new(2));
        let release_slow = Arc::new(Barrier::new(2));
        let hook: ActivationTransactionHook = {
            let slow = slow.clone();
            let slow_entered = slow_entered.clone();
            let release_slow = release_slow.clone();
            Arc::new(move |request| {
                if request.path() == slow {
                    slow_entered.wait();
                    release_slow.wait();
                    anyhow::bail!("late preparation failure");
                }
                Ok(PreparedActivation::summary(Some(format!(
                    "committed request {}",
                    request.id()
                ))))
            })
        };
        let ws = Workspace::open_local(slow.clone(), None)
            .unwrap()
            .with_activation_transaction(hook);

        let slow_ws = ws.clone();
        let slow_root = slow.clone();
        let slow_thread = std::thread::spawn(move || slow_ws.set_root_dir(&slow_root, None));
        slow_entered.wait();
        let fast_out = ws.set_root_dir(&fast, None);
        release_slow.wait();
        let slow_out = slow_thread.join().unwrap();

        assert!(fast_out.contains("committed request 2"));
        assert!(
            slow_out.contains("superseded by request 2")
                && slow_out.contains("failed build")
                && !slow_out.contains("set_root_dir failed"),
            "a stale failure is a superseded outcome: {slow_out}"
        );
        assert_eq!(ws.active_repo_path(), Some(fast));
    }

    // ------------------------------------------------------------------
    // revs (multi-revision activation)
    // ------------------------------------------------------------------

    /// Stand up a real git repo at a fresh tempdir with the given tags
    /// created in order (so version-sort ordering is exercised). Returns
    /// the tempdir (keep alive) + its path, or `None` if git is
    /// unavailable in the environment (test then skips).
    fn git_repo_with_tags(tags: &[&str]) -> Option<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
        };
        if !git(&["init"]).status.success() {
            return None; // git unavailable — caller skips.
        }
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "Test"]);
        git(&["config", "commit.gpgsign", "false"]);
        for (i, tag) in tags.iter().enumerate() {
            std::fs::write(root.join("f.txt"), format!("rev {i}")).unwrap();
            git(&["add", "-A"]);
            assert!(
                git(&["commit", "-m", &format!("c{i}")]).status.success(),
                "git commit failed"
            );
            assert!(git(&["tag", tag]).status.success(), "git tag {tag} failed");
        }
        Some((dir, root))
    }

    // ---- tag classification (pure, no git) --------------------------

    #[test]
    fn classify_tag_extracts_prefix_version_prerelease() {
        let c = classify_tag("apache-arrow-25.0.0").unwrap();
        assert_eq!(c.prefix, "apache-arrow-");
        assert_eq!(c.version, vec![25, 0, 0]);
        assert!(!c.is_prerelease);

        // Prerelease markers with various separators, case-insensitive.
        for t in [
            "apache-arrow-25.0.0.dev",
            "apache-arrow-25.0.0-rc1",
            "apache-arrow-25.0.0-RC0",
            "v1.2.3-beta2",
            "v1.2.3_alpha",
            "v2.0.0-preview",
        ] {
            assert!(
                classify_tag(t).unwrap().is_prerelease,
                "{t} should be prerelease"
            );
        }

        // Distinct families keyed on prefix.
        assert_eq!(classify_tag("go/v18.0.0").unwrap().prefix, "go/v");
        assert_eq!(classify_tag("r-15.0.1").unwrap().prefix, "r-");
        assert_eq!(classify_tag("v1.2.3").unwrap().prefix, "v");

        // Last digit run wins: the `2` in `arrow2` is not the version.
        let c = classify_tag("arrow2-0.17.0").unwrap();
        assert_eq!(c.prefix, "arrow2-");
        assert_eq!(c.version, vec![0, 17, 0]);
    }

    #[test]
    fn classify_tag_excludes_non_version_tags() {
        assert_eq!(classify_tag("r-universe-release"), None);
        assert_eq!(classify_tag("latest"), None);
        assert_eq!(classify_tag("nightly"), None);
        // A version followed by an *unrecognised* suffix is not version-like.
        assert_eq!(classify_tag("v1.2.3-foobar"), None);
    }

    #[test]
    fn select_family_tags_picks_dominant_release_family_skipping_prereleases() {
        // Mirrors the apache/arrow shape: a large `apache-arrow-*` release
        // family (with newest entries being prereleases), plus unrelated
        // `r-*` / `go/v*` families and a rolling non-version pointer.
        let tags: Vec<String> = [
            "apache-arrow-22.0.0",
            "apache-arrow-23.0.0",
            "apache-arrow-24.0.0",
            "apache-arrow-25.0.0-rc0",
            "apache-arrow-25.0.0-rc1",
            "apache-arrow-25.0.0.dev",
            "go/v18.0.0",
            "r-15.0.1",
            "r-16.1.0",
            "r-universe-release",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Newest 2 STABLE of the dominant (apache-arrow-) family,
        // oldest→newest; the 25.0.0 prereleases and r-*/go/v* are excluded.
        let got = select_family_tags(&tags, 2).unwrap();
        assert_eq!(got, vec!["apache-arrow-23.0.0", "apache-arrow-24.0.0"]);
    }

    #[test]
    fn select_family_tags_fewer_stable_than_requested_uses_all_stable() {
        let tags: Vec<String> = ["v1.0.0", "v2.0.0", "v3.0.0-rc1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // Only two stable; the rc is skipped even though it's newest.
        let got = select_family_tags(&tags, 5).unwrap();
        assert_eq!(got, vec!["v1.0.0", "v2.0.0"]);
    }

    #[test]
    fn select_family_tags_prerelease_only_family_falls_back_to_prereleases() {
        let tags: Vec<String> = ["v1.0.0-rc1", "v1.0.0-rc2", "v0.9.0-beta"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // No stable tag anywhere → newest prereleases of the family.
        let got = select_family_tags(&tags, 2).unwrap();
        assert_eq!(got, vec!["v1.0.0-rc1", "v1.0.0-rc2"]);
    }

    #[test]
    fn select_family_tags_no_version_like_tags_returns_none() {
        let tags: Vec<String> = ["latest", "nightly", "stable"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(select_family_tags(&tags, 3), None);
    }

    // ---- resolve_revs Count (git-gated) -----------------------------

    #[test]
    fn resolve_revs_count_falls_back_to_raw_when_no_version_tags() {
        // Non-version tags → the family selector yields None and
        // resolve_revs preserves the raw version-sorted top-n behaviour.
        let Some((_d, root)) = git_repo_with_tags(&["latest", "nightly", "stable"]) else {
            return;
        };
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        let resolved = ws.resolve_revs(&root, &RevsRequest::Count(2)).unwrap();
        // Exactly 2 tags + HEAD, HEAD last; contents come from raw top-n.
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved.last().unwrap(), "HEAD");
        assert!(resolved[..2].iter().all(|r| r != "HEAD"));
    }

    #[test]
    fn resolve_revs_count_skips_prereleases_of_dominant_family() {
        let Some((_d, root)) =
            git_repo_with_tags(&["v1.0.0", "v2.0.0", "v3.0.0-rc1", "v3.0.0.dev"])
        else {
            return;
        };
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        let resolved = ws.resolve_revs(&root, &RevsRequest::Count(2)).unwrap();
        // Newest 2 stable (v1.0.0, v2.0.0) oldest→newest, then HEAD —
        // the v3 prereleases are excluded.
        assert_eq!(resolved, vec!["v1.0.0", "v2.0.0", "HEAD"]);
    }

    #[test]
    fn resolve_revs_count_picks_newest_n_oldest_first_head_last() {
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0", "v1.1.0", "v2.0.0"]) else {
            return;
        };
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        let resolved = ws
            .resolve_revs(&root, &RevsRequest::Count(2))
            .expect("resolve should succeed");
        // Newest 2 = v2.0.0, v1.1.0 → oldest→newest → v1.1.0, v2.0.0, then HEAD.
        assert_eq!(resolved, vec!["v1.1.0", "v2.0.0", "HEAD"]);
    }

    #[test]
    fn resolve_revs_count_fewer_tags_than_requested_uses_all() {
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0", "v2.0.0"]) else {
            return;
        };
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        let resolved = ws.resolve_revs(&root, &RevsRequest::Count(10)).unwrap();
        assert_eq!(resolved, vec!["v1.0.0", "v2.0.0", "HEAD"]);
    }

    #[test]
    fn resolve_revs_count_errors_when_no_tags() {
        let Some((_d, root)) = git_repo_with_tags(&[]) else {
            return;
        };
        // Empty repo has no commits yet; make one commit but no tags.
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(args)
                .output()
                .unwrap()
        };
        std::fs::write(root.join("f.txt"), b"x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-m", "c0"]);
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        let err = ws
            .resolve_revs(&root, &RevsRequest::Count(3))
            .expect_err("no tags → error");
        assert!(
            err.to_string().contains("no tags"),
            "expected a 'no tags' error, got: {err}"
        );
    }

    // ---- dedup of resolved revs -------------------------------------

    #[test]
    fn dedup_labels_is_order_preserving_first_wins() {
        assert_eq!(
            dedup_labels(vec!["HEAD".into(), "HEAD".into()]),
            vec!["HEAD"]
        );
        assert_eq!(
            dedup_labels(vec![
                "v1".into(),
                "v2".into(),
                "v1".into(),
                "v3".into(),
                "v2".into(),
            ]),
            vec!["v1", "v2", "v3"]
        );
        // Empty and already-unique lists pass through untouched.
        assert_eq!(dedup_labels(vec![]), Vec::<String>::new());
        assert_eq!(dedup_labels(vec!["a".into(), "b".into()]), vec!["a", "b"]);
    }

    #[test]
    fn resolve_revs_list_dedups_duplicate_revspecs() {
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0"]) else {
            return;
        };
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        // `["HEAD","HEAD"]` collapses to a single `HEAD`.
        let got = ws
            .resolve_revs(
                &root,
                &RevsRequest::List(vec!["HEAD".into(), "HEAD".into()]),
            )
            .unwrap();
        assert_eq!(got, vec!["HEAD"]);
        // First-occurrence order is preserved across mixed duplicates.
        let got = ws
            .resolve_revs(
                &root,
                &RevsRequest::List(vec!["v1.0.0".into(), "HEAD".into(), "v1.0.0".into()]),
            )
            .unwrap();
        assert_eq!(got, vec!["v1.0.0", "HEAD"]);
    }

    #[test]
    fn resolve_revs_list_validates_and_rejects_unknown() {
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0", "v1.1.0"]) else {
            return;
        };
        let ws = Workspace::open_local(root.clone(), None).unwrap();
        // Explicit list is used verbatim (no HEAD appended, no sort).
        let ok = ws
            .resolve_revs(
                &root,
                &RevsRequest::List(vec!["v1.1.0".into(), "v1.0.0".into()]),
            )
            .unwrap();
        assert_eq!(ok, vec!["v1.1.0", "v1.0.0"]);
        // An unknown rev is a clear error naming the bad rev.
        let err = ws
            .resolve_revs(&root, &RevsRequest::List(vec!["v9.9.9".into()]))
            .expect_err("unknown rev → error");
        assert!(
            err.to_string().contains("v9.9.9") && err.to_string().contains("does not exist"),
            "expected an unknown-rev error, got: {err}"
        );
    }

    #[test]
    fn revs_hook_receives_resolved_revs_and_plain_hook_untouched() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0", "v1.1.0", "v2.0.0"]) else {
            return;
        };
        let plain_calls = Arc::new(AtomicUsize::new(0));
        let seen_revs: Arc<std::sync::Mutex<Option<Vec<String>>>> = Arc::new(Default::default());
        let pc = plain_calls.clone();
        let plain: PostActivateHook = Arc::new(move |_p, _n| {
            pc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let sr = seen_revs.clone();
        let revs_hook: PostActivateRevsHook = Arc::new(move |_p, _n, revs| {
            *sr.lock().unwrap() = Some(revs.to_vec());
            Ok(())
        });
        let ws = Workspace::open_local(root.clone(), Some(plain))
            .unwrap()
            .with_post_activate_revs(revs_hook);
        let out = ws.repo_management(None, false, true, false, Some(&RevsRequest::Count(2)));
        // The revs-hook ran with the resolved list; the plain hook did NOT.
        assert_eq!(
            seen_revs.lock().unwrap().clone().unwrap(),
            vec!["v1.1.0", "v2.0.0", "HEAD"]
        );
        assert_eq!(
            plain_calls.load(Ordering::SeqCst),
            0,
            "plain hook must not fire when the revs-hook handled the request"
        );
        // The activation message names the resolved revs on one line.
        assert!(
            out.contains("revs: v1.1.0, v2.0.0, HEAD"),
            "activation message should list the resolved revs; got: {out}"
        );
    }

    #[test]
    fn plain_hook_used_and_no_revs_line_when_no_revs_requested() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0"]) else {
            return;
        };
        let plain_calls = Arc::new(AtomicUsize::new(0));
        let revs_seen = Arc::new(AtomicUsize::new(0));
        let pc = plain_calls.clone();
        let plain: PostActivateHook = Arc::new(move |_p, _n| {
            pc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let rs = revs_seen.clone();
        let revs_hook: PostActivateRevsHook = Arc::new(move |_p, _n, _revs| {
            rs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let ws = Workspace::open_local(root.clone(), Some(plain))
            .unwrap()
            .with_post_activate_revs(revs_hook);
        // No revs → plain hook fires, revs-hook untouched, no `revs:` line.
        let out = ws.repo_management(None, false, true, false, None);
        assert_eq!(plain_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            revs_seen.load(Ordering::SeqCst),
            0,
            "revs-hook must not fire when no revs were requested"
        );
        assert!(
            !out.contains("revs:"),
            "no revs line expected on a plain activation; got: {out}"
        );
    }

    #[test]
    fn revs_requested_without_revs_hook_falls_back_to_plain_no_revs_line() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let Some((_d, root)) = git_repo_with_tags(&["v1.0.0", "v2.0.0"]) else {
            return;
        };
        let plain_calls = Arc::new(AtomicUsize::new(0));
        let pc = plain_calls.clone();
        let plain: PostActivateHook = Arc::new(move |_p, _n| {
            pc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        // No revs-hook attached: a revs request degrades to the plain
        // (HEAD-only) build and does NOT claim a rev-set in the message.
        let ws = Workspace::open_local(root.clone(), Some(plain)).unwrap();
        let out = ws.repo_management(None, false, true, false, Some(&RevsRequest::Count(1)));
        assert_eq!(plain_calls.load(Ordering::SeqCst), 1);
        assert!(
            !out.contains("revs:"),
            "must not report a rev-set when only the plain hook ran; got: {out}"
        );
    }

    // ---- rev-set-aware skip gate + stored-request persistence -------

    /// Build a local workspace over a git repo with tags, wired with both
    /// a plain and a revs hook, each incrementing a shared counter.
    /// Returns (workspace, tempdir-guard, root, plain_calls, revs_calls).
    #[allow(clippy::type_complexity)]
    fn ws_with_both_hooks(
        tags: &[&str],
    ) -> Option<(
        Workspace,
        tempfile::TempDir,
        PathBuf,
        Arc<std::sync::atomic::AtomicUsize>,
        Arc<std::sync::atomic::AtomicUsize>,
    )> {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (d, root) = git_repo_with_tags(tags)?;
        let plain_calls = Arc::new(AtomicUsize::new(0));
        let revs_calls = Arc::new(AtomicUsize::new(0));
        let pc = plain_calls.clone();
        let plain: PostActivateHook = Arc::new(move |_p, _n| {
            pc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let rc = revs_calls.clone();
        let revs_hook: PostActivateRevsHook = Arc::new(move |_p, _n, _r| {
            rc.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let ws = Workspace::open_local(root.clone(), Some(plain))
            .unwrap()
            .with_post_activate_revs(revs_hook);
        Some((ws, d, root, plain_calls, revs_calls))
    }

    #[test]
    fn plain_activation_after_revs_build_rebuilds_plain() {
        use std::sync::atomic::Ordering;
        let Some((ws, _d, root, plain_calls, revs_calls)) =
            ws_with_both_hooks(&["v1.0.0", "v2.0.0"])
        else {
            return;
        };
        // Multi-rev build first.
        let _ = ws.set_root_dir(&root, Some(&RevsRequest::Count(2)));
        assert_eq!(revs_calls.load(Ordering::SeqCst), 1);
        assert_eq!(plain_calls.load(Ordering::SeqCst), 0);
        // A plain re-bind at the SAME (unchanged) root must NOT cheap-skip
        // just because HEAD matches — the last build was multi-rev. It
        // rebuilds plain, and the message claims no rev-set.
        let out = ws.set_root_dir(&root, None);
        assert_eq!(
            plain_calls.load(Ordering::SeqCst),
            1,
            "plain re-activation after a revs build must fire the plain hook"
        );
        assert!(
            !out.contains("build skipped"),
            "must not skip a plain re-activation after a revs build; got: {out}"
        );
        assert!(
            !out.contains("revs:"),
            "plain rebuild must not claim revs; got: {out}"
        );
        // The stored request is cleared, so a further plain re-bind now
        // cheap-skips (proves the reset took).
        let out = ws.set_root_dir(&root, None);
        assert_eq!(
            plain_calls.load(Ordering::SeqCst),
            1,
            "second plain re-bind skips"
        );
        assert!(
            out.contains("build skipped"),
            "expected skip suffix; got: {out}"
        );
    }

    #[test]
    fn update_after_revs_build_reapplies_stored_revs() {
        use std::sync::atomic::Ordering;
        let Some((ws, _d, root, plain_calls, revs_calls)) =
            ws_with_both_hooks(&["v1.0.0", "v2.0.0"])
        else {
            return;
        };
        // Multi-rev build first.
        let _ = ws.set_root_dir(&root, Some(&RevsRequest::Count(2)));
        assert_eq!(revs_calls.load(Ordering::SeqCst), 1);
        // A bare `update` (no revs) must re-apply the stored rev-set —
        // re-firing the revs hook, not collapsing to a plain HEAD build.
        let out = ws.repo_management(None, false, true, false, None);
        assert_eq!(
            revs_calls.load(Ordering::SeqCst),
            2,
            "bare update must re-apply the stored rev-set"
        );
        assert_eq!(
            plain_calls.load(Ordering::SeqCst),
            0,
            "bare update after a revs build must not fall to the plain hook"
        );
        assert!(
            out.contains("revs:"),
            "re-applied update should list the revs; got: {out}"
        );
    }

    #[test]
    fn revs_activation_after_plain_build_always_rebuilds() {
        use std::sync::atomic::Ordering;
        let Some((ws, _d, root, plain_calls, revs_calls)) =
            ws_with_both_hooks(&["v1.0.0", "v2.0.0"])
        else {
            return;
        };
        // Plain build first.
        let _ = ws.set_root_dir(&root, None);
        assert_eq!(plain_calls.load(Ordering::SeqCst), 1);
        assert_eq!(revs_calls.load(Ordering::SeqCst), 0);
        // A revs request at the unchanged HEAD still always fires the revs
        // hook (revs requests are never skipped by the SHA gate).
        let _ = ws.set_root_dir(&root, Some(&RevsRequest::Count(2)));
        assert_eq!(
            revs_calls.load(Ordering::SeqCst),
            1,
            "a revs request must always rebuild, even at an unchanged HEAD"
        );
    }

    #[test]
    fn last_built_revs_round_trips_and_clears_on_plain_build() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        ws.bump_access("acme/widgets", "cloned");
        assert_eq!(ws.last_built_revs("acme/widgets"), None);
        // Record a multi-rev build.
        ws.record_built("acme/widgets", "sha1", Some(&RevsRequest::Count(3)));
        assert_eq!(
            ws.last_built_revs("acme/widgets"),
            Some(RevsRequest::Count(3))
        );
        // Survives a reopen (persisted to inventory.json).
        let ws2 = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert_eq!(
            ws2.last_built_revs("acme/widgets"),
            Some(RevsRequest::Count(3))
        );
        // A subsequent plain build clears the stored request.
        ws2.record_built("acme/widgets", "sha2", None);
        assert_eq!(ws2.last_built_revs("acme/widgets"), None);
        // A List request round-trips too.
        ws2.record_built(
            "acme/widgets",
            "sha3",
            Some(&RevsRequest::List(vec!["v1".into(), "v2".into()])),
        );
        assert_eq!(
            ws2.last_built_revs("acme/widgets"),
            Some(RevsRequest::List(vec!["v1".into(), "v2".into()]))
        );
    }

    #[test]
    fn inventory_loads_legacy_entries_without_revs_field() {
        // An entry carrying last_built_sha but no last_built_revs (an
        // inventory written before the field existed) loads cleanly with
        // the request defaulting to None.
        let dir = tempfile::tempdir().unwrap();
        let legacy = r#"{
            "old/repo": {
                "cloned_at": "2024-01-01T00:00:00",
                "last_accessed": "2024-01-01T00:00:00",
                "access_count": 5,
                "stale": false,
                "last_built_sha": "deadbeef"
            }
        }"#;
        std::fs::write(dir.path().join("inventory.json"), legacy).unwrap();
        let ws = Workspace::open(dir.path().to_path_buf(), 7, None).unwrap();
        assert_eq!(ws.last_built_sha("old/repo").as_deref(), Some("deadbeef"));
        assert_eq!(ws.last_built_revs("old/repo"), None);
    }
}
