# Changelog

## 0.4.5 — 2026-08-16

### Changed — GitHub tools are manifest opt-in, default off (behaviour break)

**Every deployment that relies on token-presence registration must add
`builtins.github: true` to its manifest, or `github_issues`, `github_api`
and `screen_stargazers` disappear from its `tools/list`.** There is no
migration shim and no deprecation window: the whole point of the change is
that the old trigger — a reachable credential — is not an expression of
intent and must stop registering anything.

Registration used to key off `has_git_token()` alone. An operator
reproduced what that means in practice: a personal music server silently
gained three authenticated GitHub tools because a `.env` several
directories above its root happened to hold a `GITHUB_TOKEN`, and the
framework's walk-up found it. `tools/list` went from 14 tools to 17 on a
two-line env diff nobody would have connected to GitHub. An ambient credential picked
up from a parent directory is exactly the credential whose blast radius
should be smallest, and it was widening the agent-facing surface of an
unrelated server on its own.

The gates now run in the order intent-then-capability. `builtins.github`
(new, default `false`) decides whether the GitHub tools are part of this
deployment at all; only then does token reachability decide whether they
can actually register — an opted-in server with no reachable token still
hides them, because an agent should not see a tool that is guaranteed to
fail. `builtins.screen_stargazers` (unchanged, default `true`) is now
subordinate to the new key: with `github: false` it registers nothing
whatever its value.

`BuiltinsConfig` is publicly re-exported and is not `#[non_exhaustive]`,
and it gained a `github` field. Downstream code that builds it with a
struct literal, or destructures it exhaustively, will not compile until it
accounts for the field; `..Default::default()`, `clone()` and field access
are unaffected. `Manifest::to_json()` gains a `builtins.github` key —
additive, per the documented stability rule for that shape.

The bundled `github_issues` skill carried the old rule in its description
and its Authorisation section, and that text is injected into the
`github_issues` tool description agents read — so a server missing the
opt-in would have told its agent to go provision a token it already had.
Both statements now name both gates, opt-in first.

## 0.4.4 — 2026-08-06

### Changed — rmcp 3.1.1 alignment (public source break)

`mcp-methods` and the bundled `mcp-server` now use rmcp 3.1.1. KGLite's
rmcp 3 upgrade could not be made independently: this framework deliberately
exposes rmcp's `ToolRouter`, accepts dynamic `ToolRoute` values, and implements
rmcp's serving traits, so Cargo resolving rmcp 2 and 3 side by side produced
unrelated Rust types across that public boundary. Thirteen jobs in KGLite PR
#105 failed on that split. Both direct dependencies now move in lockstep again.

This is a source-level break for downstream crates using the low-level escape
hatch. Their direct rmcp requirement must move to 3.1.1, and
`ToolRoute::new_dyn` handlers that complete synchronously now return
`Result<CallToolResponse, ErrorData>` rather than
`Result<CallToolResult, ErrorData>`; wrap an ordinary completed result with
`.into()`. Custom prompt handlers make the corresponding
`GetPromptResult` → `GetPromptResponse` conversion. The high-level
`McpServer::register_typed_tool` API is unchanged. Per the repository's
standing release policy this ships as patch 0.4.4 despite the public source
break.

rmcp 3.1.1 declares Rust 1.88 as its minimum, so every published crate now
states the same `rust-version` instead of leaving consumers to discover the
effective floor during resolution. CI builds the workspace on 1.88 and also
compiles the excluded downstream-binary example against the checkout; the
latter exercises the direct-rmcp/public-framework boundary that previously
failed only after KGLite upgraded. The example and Rust documentation no
longer advertise the obsolete mcp-methods 0.3 / rmcp 1.6 pairing.

The server continues to negotiate legacy MCP revisions, including the
deprecated opt-in client-roots path. A session that negotiates the 2026-07-28
revision now ignores a stale `roots` capability instead of emitting the
`roots/list` method removed by that revision. A real stdio protocol test pins
both halves of that contract: no removed roots request is sent, while a tool
call still completes with the modern `resultType: "complete"` envelope.

## 0.4.3 — 2026-07-31

**Sequential behaviour is unchanged.** A deployment that sets neither new
manifest key sees no difference in any single-threaded path — both features are
opt-in by config presence.

**Under concurrency there is one deliberate change, and it reaches everyone**,
including deployments setting neither key: activations now declare whether they
*bind* a root or merely *refresh* one, and a refresh can no longer supersede a
bind. In plain github mode that means a `repo_management(update=true)` racing a
`repo_management('org/repo')` now abandons its own rebuild rather than
overwriting the bind, with a correspondingly different response string. See
Fixed, below — it is the general form of a bug that was reachable in the new
adoption path.

**One source-level break, called out rather than hidden.** `WorkspaceConfig` is
publicly re-exported and is not `#[non_exhaustive]`, and it gained two fields
(`sandbox_root`, `adopt_client_roots`). Downstream code that builds it with a
struct literal, or destructures it exhaustively, will not compile until it
accounts for them; `..Default::default()` and field access are unaffected. The
version still moves as a **patch** per this project's standing policy — this
note is where the semver-relevant fact is recorded, so it reaches the people it
affects.

New public surface: `Workspace::with_sandbox_root`,
`Workspace::with_adopt_client_roots`, `Workspace::adopt_client_root`,
`Workspace::open_local_unanchored`, `Workspace::adopts_client_roots`,
`Workspace::root_ownership`, the `RootOwnership` enum, the `server::roots`
module and its `file_uri_to_path`, and the two `WorkspaceConfig` fields above.

### Added — `workspace.sandbox_root`, an opt-in boundary on root swaps

- **`set_root_dir` accepted any directory on the filesystem.** It checked
  local mode, checked `is_dir`, canonicalized, and activated. There was no
  containment test — because no containment *concept* existed anywhere in the
  framework. The read sandbox is derived from the active root (the
  `source_roots` provider installed by `with_workspace`), so the `starts_with`
  checks in the source tools were containment relative to wherever the server
  currently pointed; they never constrained where it could be pointed. Setting
  `workspace.sandbox_root` now bounds that: a swap whose canonicalized target
  falls outside the boundary is refused, with an error naming the boundary.
  Because the check runs on the *canonical* path, `..` traversals and symlinks
  leading out of the tree are caught; because it returns before `activate`, a
  refused swap leaves the active root untouched rather than half-swapped.
- **A manifest that contradicts itself now dies at boot.** `workspace.root`
  outside `workspace.sandbox_root` is a startup error, not a surprise at the
  first swap.
- **The default is unbounded and unchanged.** This was surfaced by kglite while
  planning roots adoption, and it is worth being precise about what it was: not
  a defect against this project's documentation, which describes `set_root_dir`
  as an unbounded runtime swap and uses a sibling-directory swap as its worked
  example. It is new machinery for consumers who need a boundary, not a
  restored check. Consumers who swap roots freely today are unaffected.

### Added — `workspace.adopt_client_roots`, MCP `roots` as a fallback root source

- **The client already told us where the workspace is; nobody was listening.**
  Hosts such as opencode advertise their workspace directory over the MCP
  `roots` capability on every connection. With `workspace.adopt_client_roots`
  set, the server issues `roots/list` after the handshake and — only when no
  root is explicitly configured — adopts the first advertised root that
  resolves to a real directory. Requested by kglite on behalf of codingest.
- **Deprecated upstream; enable with that in view.** MCP `roots` is deprecated
  as of protocol revision `2026-07-28` (SEP-2577): *"New implementations SHOULD
  NOT adopt it; existing implementations SHOULD migrate to passing directories
  or files via tool parameters, resource URIs, or server configuration."* The
  earliest revision that may remove it is the first released on or after
  2027-07-28. It ships here because it is opt-in, because `rmcp` negotiates a
  revision where the mechanism is live, and because the requester asked for it
  — but the migration path is already published and a new deployment should
  prefer explicit configuration.
- **Ordering is client-dependent, by omission in the spec.** `ListRootsResult`
  is an unordered array in every published schema revision; no text makes the
  first element primary. "First valid root" is a heuristic that matches what
  real clients send. Set `workspace.root` explicitly if you need determinism.
- **Adoption cannot escape containment.** Adoption and `set_root_dir` share one
  internal swap path, and the `sandbox_root` check lives inside it — an
  externally proposed root is bounded by exactly the same rule as an
  agent-proposed one.
- **Operator intent is permanent.** Once an explicit `set_root_dir` has chosen a
  root, a later `roots/list_changed` never overrides it.
- **Failure is inert by construction.** A client that errors the request,
  answers late, sends a non-`file://` URI, names a foreign host, or points at a
  nonexistent path leaves the server unanchored with a warning — never a crash,
  never a retry loop. Clients that do not advertise `roots` are never sent the
  request at all and pay nothing at connect time.

### Fixed — an activation could publish a root nobody asked for

These were found by adversarial review of the two features above, before
release. Two were confirmed by execution, not inferred. Both broke the same
invariant — *the root an operator explicitly chose is the one that stays
active* — which the docs state without qualification.

- **An adopted client root could displace an operator's `set_root_dir`.**
  `set_root_dir` marked ownership `Operator` only *after* its swap finished,
  while adoption held its own lock across the whole sequence. Interleaved, the
  adoption's activation committed last and the operator's flag landed on the
  *client's* root — after the operator's tool call had already returned success
  naming a different path. Root swaps now take a shared lock and adoption an
  exclusive one, so overlapping operator swaps still supersede each other as
  before (that ordering is load-bearing and has its own tests) while an
  adoption is exclusive with all of them from its ownership check through to
  publication.
- **A refresh could decide which root was active.** `repo_management(update=true)`
  reached the activation path without that lock, so it won generation
  supersession against an in-flight swap and re-committed the root it had read
  earlier — the same wrong end state through a different entry point. A refresh
  means "rebuild whatever is bound" and must never change *which* root is
  bound; that is now enforced structurally. A refresh cannot supersede a bind,
  and its commit is a compare-and-swap against the binding it read, checked
  when its generation is allocated and again at commit, so it can only ever
  rewrite the identical value. This is the part that also affects github mode.
- **A failed first adoption left state inside the client's directory.** The
  inventory directory was created, and the inventory anchor permanently fixed,
  before the activation committed — so an adoption that passed containment and
  then failed still wrote `.mcp-workspace/` into a client-proposed root and
  pinned all later inventory there. Anchoring now happens only when an
  activation commits.
- **`watch: true` with no root is now refused by the manifest loader**, not
  only by the `mcp-server` binary. The reference documentation already said
  `watch` requires `root`; a library consumer loading a manifest directly was
  getting a silently dead watcher instead of an error.

## 0.4.2 — 2026-07-28

### Fixed — declared dependency floors

- **`ignore` was declared as `"0.4"` while the code needs 0.4.15.**
  `grep/walker.rs` and `list_dir.rs` call `WalkBuilder::filter_entry` and
  `::sort_by_file_path`; bisected against the individual releases, 0.4.14 has
  neither and 0.4.15 has both. The understatement was structurally invisible
  here — our lockfile already held a working version and a fresh resolve picks
  the newest 0.4.x — but a consumer resolving to declared minimums got
  `no method named filter_entry`. Found by kglite's `minimal-versions` CI job,
  which resolves direct dependencies down to their declared floors and builds
  the result; this was the one crate still failing it.
- **The sdist carried no LICENSE file.** `pyproject.toml` declared
  `license = {text = "MIT"}`, which records the licence name but never
  includes the file. Wheels were unaffected (maturin adds
  `dist-info/licenses/LICENSE` regardless) and no sdist has been published, so
  there is no live exposure — but a locally built sdist shipped without a
  licence. Verified by building both ways: 0 entries without `license-files`,
  exactly 1 with it.

## 0.4.1 — 2026-07-21

### Fixed — request-coherent workspace activation

Concurrent `set_root_dir` calls could cross their identities: one response
named root A while its separately generated graph summary — and the live
downstream product — belonged to root B. The framework published source
identity before building, invoked plain/revision/summary callbacks without a
request identity, and let a slow older completion overwrite newer intent.
Path and repo name alone could not distinguish same-root plain and
multi-revision overlaps downstream.

Workspace activation now has an additive request-scoped prepare/commit API:
`ActivationTransactionHook` receives an `ActivationRequest` carrying a
monotonic `ActivationId`, canonical path/name, and `ActivationBuild`
(`Plain`, resolved `Revisions`, or `Reuse`), then returns a
`PreparedActivation`. Expensive preparation stays off-lock; the prepared
closure installs its product and generates the summary for that exact request
only if its generation is still current. Stale preparations are dropped with
an explicit superseded response. Active source path/name, in-process built
identity/revisions, inventory receipt, and downstream publication commit under
the same generation boundary; a current failure leaves the prior source and
product active.

The existing callback trio remains source-compatible and is serialized
through summary generation, closing its cross-request response race while
consumers migrate to the transaction hook for deferred atomic installation.
Inventory read/modify/write cycles are serialized as well, preventing nearby
activation completions from losing each other's per-repo receipts. Barrier-led
regressions cover slow A / fast B, coherent per-request summaries, latest
ownership, same-root plain/revision overlap, current and stale failures, and
legacy callback serialization. This unblocks KGLite's generation-aware generic
workspace-graph lifecycle migration.

## 0.4.0 — 2026-07-19

### Changed — rmcp 2.2 (breaking: rmcp types in our public API)

The rmcp dependency moves `^1.6` → `^2.2` in both `mcp-methods` and
`mcp-server`, at kglite's request: they consume `McpServer`,
`SourceRootsProvider`, and dynamic `ToolRoute` registration directly,
so rmcp types flow through our public API — a consumer cannot select
rmcp 2.x independently without splitting the MCP type graph. That
same fact makes this a semver-minor cut (0.4.0, not a patch): any
downstream crate naming those types must move its own rmcp req in
lockstep.

rmcp 2.x aligns the model types with the MCP 2025-11-25 spec. The
migration surface in our code was small: `Content::text(…)` →
`ContentBlock::text(…)` (the `Annotated`/`RawContent` split collapsed
into one `ContentBlock` union) and `PromptMessageRole` → `Role`. Wire
behavior is unchanged — text content still serializes as
`{"type":"text","text":…}`, and the server still negotiates older
client protocol versions (verified by a live stdio
initialize/tools-list/tools-call handshake). All public capabilities
kglite relies on are preserved: manifest loading, `McpServer`,
`SourceRootsProvider`, skill/source tools, and dynamic `ToolRoute`
registration. The `mcp-server` → `mcp-methods` dep pin bumps `0.3` →
`0.4` per the minor-release rule.

## 0.3.50 — 2026-07-10

### Fixed — multi-revision activation hardening

Three defects in the 0.3.49 multi-revision workspace activation
(`revs=`), all surfaced by live testing against a multi-family repo
(apache/arrow: `apache-arrow-*`, `go/v*`, `r-*`, and a rolling
`r-universe-release` pointer).

- **`revs=N` now picks the dominant release family and skips
  prereleases.** Resolution was `git tag --sort=-v:refname | head -n`,
  which grabbed whichever tag family sorted highest — on arrow that was
  the R-package family *including* a non-version rolling pointer, so
  `revs=3` loaded a union of ~2-year-apart trees instead of the last
  three releases. Tags are now classified into
  `(prefix, version, is_prerelease)`, grouped by prefix; the family with
  the most stable (non-prerelease) tags wins, and its newest `N` stable
  tags (version-sorted, oldest→newest) are taken before `HEAD`.
  Prerelease markers (rc, alpha, beta, dev, pre, preview) and
  non-version tags (e.g. `r-universe-release`) are excluded. Fallbacks
  keep degenerate repos working (prerelease-only family → its
  prereleases; no version-like tags → the prior raw top-`n`).

- **The build skip-gate and `update` path are now rev-set-aware.** A
  bare `update=True` after a multi-rev build no longer collapses the
  graph to HEAD-only — it re-applies (re-resolves) the stored rev-set;
  and a plain re-activation after a multi-rev build no longer wrongly
  cheap-skips while the live product is still the rev-set union. The
  last-built request is persisted per repo in an additive inventory
  field (older `inventory.json` files load unchanged).

- **Resolved revs are deduplicated** order-preserving (first occurrence
  wins), so `revs=["HEAD","HEAD"]` no longer hands duplicate revspecs to
  the hook. Dedup is on the label, not the commit — distinct labels that
  resolve to the same commit are intentionally kept.

## 0.3.49 — 2026-07-09

### Added — history-capable github-workspace clones

`repo_management` cloned each repo with `git clone --depth 1`, which
strips all but the tip commit: `git log -S` (pickaxe — "which commit
introduced symbol X") and any history/tag walk were impossible on a
cloned repo, forcing a manual tag-by-tag content sweep for
cross-release comparison. The clone now uses a **treeless partial
clone** (`--filter=tree:0 --tags`): the full commit history and all
tags come down, while tree/blob objects are fetched lazily on demand —
keeping the initial transfer near a shallow clone's cost but making
history queries and tag-scoped reads work. The update path fetches with
plain `git fetch origin --tags` (no `--depth 1`) so history stays
complete and newly-pushed tags become available; FETCH_HEAD still
records the remote default-branch tip, so the SHA-gate that skips
no-op rebuilds is unchanged. Local-workspace (fingerprint) mode is
untouched.

### Added — rev-aware `read_source`

`read_source` gained an optional `rev` parameter (a tag, branch, or
commit SHA). When set, the file content is read at that git revision via
`git show <rev>:<path>` instead of the working tree, then the existing
slicing / `grep` / `max_chars` pipeline is applied to the historical
content unchanged — so an agent can compare a file across releases (e.g.
how a function differs between two tags) without a manual `git checkout`.
The path argument keeps its traversal protection (absolute paths and
`..` escapes reject before reaching git); a non-git source root or an
unknown rev/path returns a clear `Error: …` string. Rev-scoped *search*
across a tree is intentionally not offered — `grep` walks the working
tree only; read a known file at a rev instead. Both changes were
requested by kglite from a concrete cross-tag native-crash
investigation (comparing microsoft/mimalloc across three release tags).

### Added — multi-revision workspace activation (`revs`)

The workspace activation tools gained an optional `revs` argument
(`repo_management` in github mode, `set_root_dir` in local mode):
`int | [str] | null`. An integer **N** resolves to the last N
version-sorted release tags (`git tag --sort=-v:refname`, take N),
ordered oldest→newest with `HEAD` appended; a list of strings is used as
explicit git revspecs (tags, branches, or SHAs) verbatim. Resolution
happens at activate time — a repo with no tags (when an int was
requested) or an unknown named rev returns a clear `Error: …`. This lets
a downstream builder (e.g. kglite's multi-revision code graph) load
several revisions of a repo into one graph from a single activation
call.

Additively, the framework gained a second post-activate hook slot,
`PostActivateRevsHook = Fn(&Path, &str, &[String])`, set via
`Workspace::with_post_activate_revs` (mirroring `with_activation_summary`
— existing consumers that register only the plain `PostActivateHook`
compile and behave unchanged). When revs are requested **and** the
revs-hook is set, it is called with the resolved revspecs instead of the
plain hook; otherwise the plain hook runs (a HEAD-only build) and no
rev-set is claimed. On a successful revs-hook run the resolved list is
appended to the activation message on a one-line `revs: …` so agents see
exactly what got loaded. The SHA-skip gate deliberately applies only to
the plain path — a `revs` request always fires the hook (HEAD's SHA says
nothing about which rev-*set* a prior build loaded; rev-aware skip logic
is intentionally not built). Stacks on the treeless-clone `--tags`
fetching above, which makes the tags available to resolve.

## 0.3.48 — 2026-07-08

### Added — retry transient GitHub API failures with exponential backoff

The `github_issues` / `github_api` tools issued every REST call as a
single attempt: a transient GitHub 5xx or a momentary network blip
failed the whole tool call outright, forcing the agent to re-run the
query. GET-shaped calls now retry on transient failures (429, 5xx, and
transport errors — DNS / connect / TLS / timeout) with exponential
backoff (500 ms, doubling, capped at 30 s per sleep; 3 retries). The
design mirrors KGLite's shared dataset HTTP client so both codebases
classify and back off identically.

Non-transient statuses bubble immediately, unchanged — a **403**
rate-limit still returns its "set GITHUB_TOKEN" hint on the first
response (retrying can't recover an hourly rate window and would only
delay that guidance), and 404 "Not found" / 401 auth / 422
search-validation are likewise not retried. The retry sits *under* each
call site's existing error mapping, so it is invisible when the first
attempt succeeds. POST / GraphQL mutations are never retried
(non-idempotent).

## 0.3.47 — 2026-07-08

### Fixed — activate skip-gate goes stale for single-slot consumers (A→B→A)

The auto-rebuild skip gate keyed its in-memory axis off
`hydrated_this_process` — a *set* of every root whose post-activate hook
had fired at any point this process. The implicit invariant was "hook
fired for X ⇒ X's product is still live", which only holds for a consumer
that keeps a **per-name** product. A consumer with a *single* active-graph
slot (kglite-mcp-server, and likely every graph consumer) overwrites that
slot on each activate, so after `activate(A) → activate(B)` the slot holds
B. Re-binding A then found `action=="current"` (fingerprint unchanged) AND
`hydrated.contains(A)` → skipped the hook → the slot still held B, yet
activate reported success and the summary hook read B. A code-review agent
swapping `set_root_dir(projA → projB → projA)` got projB's graph back
under projA's name. The bug rode through both `set_root_dir` (local) and
`repo_management` (github) since both route through `activate()`.

The fix tracks the **most-recently-activated** name
(`WorkspaceState::active_built_name: Option<String>`) instead of a set,
and skips only when re-binding the *currently active* root — never a root
merely hydrated earlier and since overwritten. This preserves the cheap
same-root `update=True` / re-bind win the gate was added for (A→A still
skips), while an A→B→A swap correctly rebuilds A. Correct for single-slot
and per-name consumers alike; cost is a rebuild only on an actual swap.
Reported by kglite (petekSuite → kglite → us, 2026-07-06/07-08).

## 0.3.46 — 2026-07-02

### Added — result-postprocess hook (runtime tool-result steering)

The framework had exactly one consumer→agent text channel — tool
descriptions, read once at select-time. Nothing corrected course at the
moment of misuse. `ServerOptions::with_result_postprocess(hook)` adds a
*runtime* channel: a `ResultPostprocessHook` (`Fn(&str tool, &Value args,
&str body, &ResultCtx) -> Option<String>`) runs after every builtin tool
produces its text result and may append a steering footer (blank-line
separated); returning `None` leaves the result byte-for-byte unchanged.

Both dispatch paths funnel through one footer contract: the static
`#[tool]` methods (`grep` / `read_source` / `list_source` /
`repo_management` / `ping`) via a shared `finish(...)` helper, and the
dynamic `register_typed_tool` tools at their choke point. The hook sees
the raw args (to detect, e.g., a definition-shaped `grep` pattern) and
the result body (to detect a zero-match dead-end); `ResultCtx` exposes
the active source roots + repo. The framework owns the mechanism; the
graph-aware content stays downstream — a graph-backed consumer supplies
the "prefer `cypher_query`" line from its own graph state. `None`
(default) is a zero-cost no-op, so existing deployments are unaffected.
Requested by kglite (petekSuite deployment, 2026-07-02).

### Added — activation-summary hook (opening-steer mini-map)

`Workspace::with_activation_summary(hook)` lets a consumer append a short
mini-map to the activation result message — the one line every agent
reads before choosing its first tool. `ActivationSummaryHook`
(`Fn(&Path, &str) -> Option<String>`) is called after a successful
activation (fresh build or cheap-skip — the in-memory product is live
either way) and its `Some(text)` is appended to the terse
`"Updated 'X' at …"` message, turning a dead slot into
`"Graph ready: 9,999 Functions · 656 Classes · 31k CALLS. Open with
graph_overview() → cypher_query; grep = literal text only."`. Kept
separate from `PostActivateHook` (whose `Result<()>` signature is
unchanged) so the addition is non-breaking; attach it immediately after
`open`/`open_local`, before the workspace is cloned. Extends the richer
activation payload flagged in the 2026-07-01 hydration thread.

### Changed — `grep` bundled skill: SKIP-first, structural-lookups-out-of-TRIGGER

The bundled `grep` skill's description led with the structural use case
("find a symbol / function / class by name, locate call sites") and only
mentioned the graph as a trailing SKIP. When two tools claim the same use
case, an agent's prior intent wins — so `grep`'s own description was
*validating* the exact anti-pattern the `code_graph_analysis` skill warns
against (a downstream code-review agent opened a graph-backed repo with
`grep` and never called `graph_overview`/`cypher_query`).

The description now puts SKIP **first** — structural questions go to the
graph — and narrows TRIGGER to literal text a graph can't index (error
strings, log lines, comments, config keys, string literals). Symbol /
definition / call-site sweeps remain a TRIGGER only as an explicit
fallback *when no structural index is available for the source root* —
grep is still the right tool in a plain source workspace with no graph.
A doctrine-first banner leads the skill body to match. Because the
bundled skill is domain-neutral by constraint (it can't know at authoring
time whether a graph is live), the static text can only set the default;
the *runtime* "graph is active → never grep for structure" nudge is
delivered by the new result-postprocess hook (below). Reported by kglite
(petekSuite deployment, 2026-07-02).

## 0.3.45 — 2026-07-01

### Fixed — post-activate hook now re-fires on the first activate of each process

`Workspace::activate` gated the post-activate hook on `last_built_sha`
alone: if the git repo (github mode) or the directory fingerprint (local
mode) already matched the recorded SHA, the hook was skipped as
"already built". But `last_built_sha` is persisted to `inventory.json`
and survives process restarts, whereas the hook's *product* — the
consumer's in-memory state, e.g. kglite-mcp-server's code graph — does
not. On a fresh process the persisted SHA still matched HEAD, so the
first `repo_management(...)` / `set_root_dir(...)` reported success
(`[build skipped: HEAD matches last-built SHA]`) while leaving no active
graph; `graph_overview` / `cypher_query` then returned "No active
graph" until a `force_rebuild=True` forced the hook. Both modes were
affected (github mode's SHA and local mode's directory fingerprint hit
the identical gate).

The gate now requires two independent facts: the repo is at its
last-built SHA (persisted, cross-process) **and** the hook has already
fired in the current process (tracked in a new, deliberately
non-persisted `hydrated_this_process` set on the workspace state). The
first activate per process therefore always hydrates; repeated
activations within the same process still cheap-skip, and
`inventory.json` / `last_built_sha` semantics are unchanged. As a
side-benefit the `build skipped` message is now honest — it only appears
when the in-memory product genuinely exists. Reported by kglite against
0.3.44 (kglite-mcp-server 0.12.4).

## 0.3.44 — 2026-06-24

### Added — `screen_stargazers`: cheap GitHub people-screening

A new built-in GitHub tool that screens the people around a project —
the stargazers of a repo, or an explicit user list — to surface
relevant developers, notable / "coding-legend" devs, architectural
peers, and *actual adopters*, at minimal API + token cost.

It reuses the `github_issues` drill-down shape: one bulk pass fetches
each person's public repo portfolio over plain REST (~1 request per
person — no GraphQL, no READMEs), classifies them into cohorts, and
enriches only a bounded shortlist (follower counts, dependency-adoption,
Rust+Python co-location, external contributions). The whole fetch lives
in an in-memory store (the stargazer analogue of `ElementCache`); the
agent drills via `element_id` (`cohort:<key>` / `user:<login>` /
`…/repo:<name>` / `…/readme`) as cache hits, and re-ranks for free. Only
the README drill costs a request.

It lives in the framework, not a downstream binary, because it is
*generic* GitHub access — no graph / Cypher / domain knowledge — so it
sits next to `github_issues` / `github_api`. It auto-configures from the
seed repo (derives relevance keywords + tech stack from the repo's
topics, description, and languages), so `screen_stargazers(repo=…)`
works with zero tuning; or seed an explicit `users=` list.

Ranking is the heart of it. Every person gets a normalized 0–100 vector
on four orthogonal axes — relatedness, popularity, effort, recency
(log-damped + percentile-ranked so power-law signals are comparable). A
`filter → rank → take` selection — named presets (`outreach`, `peers`,
`legends`, `intel`, `adopters`) or a raw `rank_by` + filters
(`min_keywords`, `active_since`, `adopters_only`, `stack_only`, `top`) —
turns one bulk fetch into materially different, decision-relevant
orderings from the same cached data, which is also the mechanism for
bounding breadth (rank then take the top-N).

### Added — `builtins.screen_stargazers` toggle

Operators get the tool **for free on upgrade**: it auto-registers when a
GitHub token is reachable, exactly like the other GitHub tools, and
existing manifests are unchanged. Set `builtins.screen_stargazers:
false` to keep `github_issues` / `github_api` but drop stargazer
screening. Default on.

Note for downstream Rust consumers: `BuiltinsConfig` gained a
`screen_stargazers: bool` field. If you construct it by struct literal
rather than via the manifest loader or `Default`, add
`..Default::default()`.

## 0.3.43 — 2026-06-16

### Removed — `trust.allow_query_preprocessor`

The third advisory trust gate, added in 0.3.29, is retired. It gated
kglite's `extensions.cypher_preprocessor` hook — a raw-query-text
rewriter. kglite removed that extension entirely in 0.10.27 (it could
corrupt string literals and `RETURN` aliases by rewriting text before
parsing) and replaced it with `extensions.value_codecs`: position-scoped
literal codecs applied *after* parsing. kglite was the gate's only
consumer, so it is now dead surface.

A no-op trust gate is worse than no gate: an operator could set
`allow_query_preprocessor: true` believing it gates something, when
nothing reads it. So rather than leave it parsed-but-unused, we remove it
outright — from `TrustConfig`, the parser, the allowed-trust-keys list,
and `Manifest::to_json()`. Two gates remain: `allow_python_tools` and
`allow_embedder`.

**Breaking, but with no live impact.** The manifest validator is strict,
so a manifest still carrying `allow_query_preprocessor:` now fails to load
as an unknown trust key. None of the five production manifests in the
`deployed_manifests` regression suite use it, and kglite drops it in
0.10.27 — so no deployed manifest carries it once that ships. Operators
who set the key (only kglite did) must remove the line.

We deliberately did **not** add a replacement `allow_value_codecs` gate.
The `trust:` block gates host-access / code-execution risk:
`allow_python_tools` runs operator Python, `allow_embedder` loads a Python
module, and the preprocessor this gate guarded could carry a `command:`
subprocess hook — which is *why* it needed a gate. Tier-1 value codecs
(prefix / map / regex) are pure declarative data transforms: no
subprocess, no code execution, no host access. Their opt-in is the
presence of an operator-authored `value_codecs:` block, the same posture
as `tools:`. Gating them would dilute what `trust:` means without a
corresponding risk to gate against.

### Docs
- The trust-gate pattern explainers (`docs/explanation/trust-pattern.md`,
  `docs/guides/trust-gates.md`) and every copy-paste manifest example now
  use `allow_embedder` as the canonical worked gate. Schema/reference
  tables drop the retired row. The 0.3.29 entry in the architecture
  version-log is left as historical record.

## 0.3.42 — 2026-06-16

### Added — `serve_prompts` injects skill routing and honors `references_tools`

The auto-inject pass that embeds skills into live tool descriptions did
two things that left the most useful half of a skill unreachable by the
agent. Both are now fixed; the change is additive and needs no
frontmatter schema change (both fields already parsed).

1. **The skill `description` now reaches the tool channel.** Previously
   only the `body` was injected (under `## Methodology`); the
   `description` — which carries the skill's TRIGGER/SKIP *routing* —
   went only to `prompts/list`, a surface mainstream MCP clients
   (Claude Code, Desktop, Cursor, Continue) never expose to the model.
   The pass now injects the description under a `## When to use` header
   ahead of the methodology. It's small by design, so it leads and is
   never subject to the body's size caps (4 KB soft / 16 KB hard). An
   empty description omits the block.

2. **`references_tools` is honored for injection.** A skill is now
   injected into its name-match tool *and* every tool it lists in
   `references_tools`. This is the only way to express a **cross-tool**
   skill — one not named after any single tool (e.g. a graph-analysis
   strategy that spans `cypher_query`, `graph_overview`, `grep`, and
   `read_source`). Previously such a skill injected nowhere.

A tool may now carry several skills (its own plus any that reference
it). Each injection is fenced by a per-skill marker
(`<!-- mcp-skill:<name> -->`), keeping the pass idempotent per
(skill, tool) pair: a self-referencing skill injects once, and
re-running the pass never double-appends.

Backward compatible: a skill with no `references_tools` and a
tool-matching name behaves as before, additively gaining a `## When to
use` block. `auto_inject_hint: false` still opts out entirely.

Requested by kglite (consolidating onto a pure-Rust MCP server), which
was holding two cross-tool, routing-first skills (`code_graph_analysis`,
`code_graph_views`) that the old name-match + body-only pass could not
surface.

## 0.3.41 — 2026-06-06

### Fixed — local-mode GitHub default repo

In local-workspace mode the GitHub tools (`github_issues` /
`github_api`) defaulted their repo to the `local/<dir>` *inventory key*
whenever the caller omitted `repo_name`, producing 404s against
`repos/local/<dir>/…` — even when the active root was a real GitHub
checkout. The inventory key is filesystem-derived (a local root needn't
even be a git repo) and was never a valid `org/repo`; it was simply the
wrong source for the GitHub default.

`Workspace::active_repo_name()` was doing two jobs with one field:
inventory key *and* GitHub default repo. Split them. The new
`Workspace::default_github_repo()` resolves the default per mode —
github mode keeps using the active repo (there the inventory key *is*
the `org/repo`), while local mode now derives it from the active root's
`origin` remote (`git remote get-url origin`, both SSH and HTTPS forms,
`.git` stripped), returning `None` for a non-git / no-remote /
non-GitHub root so the existing "ask the caller for repo_name" path is
preserved verbatim. The builder (`with_workspace`) points the
default-repo provider at the new resolver. No inventory migration — the
`local/<dir>` keys on disk stay valid.

Reported by kglite (code-review MCP server, local-workspace mode).

New API:

```rust
impl Workspace {
    /// Default `org/repo` for the GitHub tools when the caller passes
    /// none. Github mode: the active repo. Local mode: the active
    /// root's `origin` remote, or None — never the `local/<dir>` key.
    pub fn default_github_repo(&self) -> Option<String>;
}
```

## 0.3.40 — 2026-05-29

### Added — `server::watch` default skip-patterns

The watch primitive now drops events under conventional noise paths
(`.git/`, `target/`, `node_modules/`, `__pycache__/`, `.venv/`,
`build/`, `dist/`, `.DS_Store`) and noise extensions (`.pyc`, `.pyo`,
`.swp`, `.swo`, `.tmp`) **before** the debounced callback runs. A
wide-sandbox watcher under active development can generate hundreds
of FSEvents per second from `cargo build` / `npm install` / `git`
operations alone; without the filter every consumer either rebuilt
wastefully or implemented the same skip list. With it, consumers see
only events that could plausibly matter.

New API:

```rust
pub const DEFAULT_SKIP_SUBSTRINGS: &[&str] = &[
    "/.git/", "/target/", "/node_modules/", "/__pycache__/",
    "/.venv/", "/build/", "/dist/", "/.DS_Store",
];
pub const DEFAULT_SKIP_EXTENSIONS: &[&str] = &[
    "pyc", "pyo", "swp", "swo", "tmp",
];

pub struct WatchConfig {
    pub skip_substrings: Vec<String>,
    pub skip_extensions: Vec<String>,
}
impl WatchConfig {
    pub fn default() -> Self;     // recommended; uses the DEFAULT_* sets above
    pub fn unfiltered() -> Self;  // empty skip set; every event reaches the callback
    pub fn is_skipped(&self, path: &Path) -> bool;
}

pub fn watch_with_config(
    dir: &Path,
    on_change: Option<ChangeHandler>,
    debounce: Option<Duration>,
    config: WatchConfig,
) -> Result<WatchHandle>;
```

The existing `pub fn watch(dir, on_change, debounce)` is unchanged at
the signature level; internally it now delegates to
`watch_with_config(..., WatchConfig::default())`. Consumers that want
every event opt out via
`watch_with_config(..., WatchConfig::unfiltered())`.

Skip-substring matching is anchored with `/` on both sides where
appropriate so a file literally named `target` at the repo root
doesn't false-match (but `/repo/target/foo` does). Matching is
allocation-free per event. Empty post-filter batches (a pure-noise
storm) skip the callback invocation entirely.

#### Why now

Originally surfaced by an MCP-servers operator deploying
kglite-mcp-server with a wide `workspace.root`
(`/Volumes/EksternalHome/Koding`, ~360k files). The `cargo build`
storm under that root generated ~120 FSEvents/sec; the downstream
kglite watch callback rebuilt the active code-tree on every event.
kglite shipped its own fix on 2026-05-25 (active-root scoping +
`language_for_path` filter + deferred rebuild — kglite main commits
`6596da6` + `0306655`), but the upstream `server::watch` is still
walking those events through the FFI / debouncer cycle before the
kglite filter drops them.

This change moves the skip to the source. Every `server::watch`
consumer benefits — kglite's filter becomes defense-in-depth, any
future code-indexer-shaped consumer gets correct defaults, and the
mcp-methods binary's own log-changes mode emits fewer noise lines.

#### Compatibility

Existing callers see strictly fewer callback invocations for paths
under the skip set. Any consumer relying on `.git/objects/` writes
hitting their callback has a bug already. No changes to
`watch(dir, on_change, debounce)`'s signature; downstream kglite
needs no source change to adopt — just bumps its `mcp-methods` pin
when 0.3.40 ships.

Unit tests cover the default skip set, the unfiltered escape hatch,
custom skip configs, anchoring (`target` file vs `target/` dir), a
positive end-to-end "callback fires on a real file change" test, and
the debouncer's batch-retention decision (`retain_unskipped`) — a
noise-only batch retains nothing (so no callback fires), a mixed batch
keeps only non-noise paths. The retention decision is tested as a pure
function rather than against a live watcher, since inotify (Linux) and
FSEvents (macOS) report different event paths for the same writes.

### Fixed — `graph_overview` against kglite 0.10

`fastmcp.register_overview` forwarded a `limit=` kwarg to
`graph.describe()`, which kglite dropped from `KnowledgeGraph.describe()`
in the 0.10 line. Every `graph_overview()` call against a kglite ≥ 0.10
graph raised `TypeError: describe() got an unexpected keyword argument
'limit'` — a broken first-reach escape-hatch tool for any consumer on
the current kglite. The `limit` parameter is removed; `describe()`
already adapts its output to graph scale, so no replacement knob is
needed. The test/example graph stubs now mirror the post-0.10
`describe(types, connections)` signature, so a regression that
re-introduces `limit` forwarding fails the suite instead of passing
against an over-permissive mock. Reported by kglite-docs (kglite 0.10.5,
mcp-methods 0.3.39).

### Fixed — `cypher_query` text mode returns a non-string

`fastmcp.register_cypher_query` returned `graph.cypher(query)` raw in
text mode. kglite's `cypher()` returns a lazy `ResultView` (Polars-backed,
not a `str`), so the `-> str` tool handed a non-string to FastMCP's
output validation and the call failed. The text path now coerces with
`str()` — `ResultView.__str__` renders the result table — matching the
defensive coercion the CSV path already did. Graph impls that already
return a `str` pass through unchanged. Flagged by kglite-docs alongside
the `graph_overview` report.

## 0.3.39 — 2026-05-22

### Fixed — `git_api` doubled `/repos/` for leading-slash paths

`git_api_internal` decided pass-through-vs-prefix by testing the path
against a `top_level` list (`repos/`, `search/`, …) whose entries have no
leading slash. A path written in the idiomatic absolute form the GitHub
REST docs render — `/repos/owner/name`, `/search/issues?q=…` — matched
none of them, fell into the relative branch, and got wrapped in
`/repos/<repo>/`, producing a doubled prefix and a 404:

```text
git_api("someorg/somerepo", "/repos/kkollsga/kglite")
  actual: https://api.github.com/repos/someorg/somerepo//repos/kkollsga/kglite → 404
  want:   https://api.github.com/repos/kkollsga/kglite
```

The same call without the leading slash worked, which made the failure
surprising and slow to diagnose. URL construction now strips a single
leading slash before the `top_level` check, so `/repos/…` and `repos/…`
(and `/search/…` vs `search/…`) are equivalent.

Reported by kglite (their issue #19), surfaced through kglite's
`github_api` MCP tool against non-active repos.

### Changed — URL construction lifted into `build_git_api_url`

The pure URL-building logic is now a `build_git_api_url(repo, path)`
helper that `git_api_internal` calls, so a regression test can assert on
the constructed URL without a network round-trip — a URL-building bug
with no unit coverage is the kind that silently comes back.

### Tests

- `leading_slash_paths_normalise` — leading and non-leading slash forms
  build the same URL for both top-level and relative paths.

## 0.3.38 — 2026-05-18

### Added — `Registry::from_manifest` on the Rust crate

The one-shot manifest → resolved-skills-registry orchestration that previously lived only in `mcp-methods-py`'s `SkillRegistry.from_manifest` is now a public method on the Rust `Registry` builder:

```rust
let registry = mcp_methods::server::Registry::from_manifest(&manifest_path, true)?;
```

Loads the manifest, optionally merges framework bundled defaults, auto-detects the project layer at `<basename>.skills/`, layers in operator-declared `skills:` paths, and finalises in a single call. Use the builder directly for bespoke layering (e.g. supplying a `SkillPredicateEvaluator` via `with_predicate_evaluator`, or `include_str!`'d downstream bundled skills via `add_bundled`).

The pyo3 wheel's `SkillRegistry.from_manifest` now delegates to this method — no public Python API change. Downstream pyo3 wrappers that consume `mcp-methods` from crates.io (kglite's incoming `kglite._mcp_internal` wrapper crate, others) can now call the same library function instead of replicating the six-line orchestration, eliminating drift risk on future layering tweaks.

### Added — `SkillError::Manifest` variant

Carries the manifest path + message when `from_manifest` fails at the manifest-load step (before any skill loading happens). Lets callers distinguish "your YAML is broken" from "your skills/ tree is broken" without parsing error messages.

Motivated by kglite as part of the "consume the Cargo crate, not the wheel" framing — by widening the library's public surface, downstream wheels (kglite, others) can stay thin pyo3 wrappers instead of carrying business logic.

### Tests

- `from_manifest_resolves_full_stack` — happy path: bundled + project + operator-declared paths compose through the one-shot call.
- `from_manifest_surfaces_manifest_load_error` — broken manifest YAML surfaces as `SkillError::Manifest`, not a downstream parsing error.

## 0.3.37 — 2026-05-14

### Fixed — agent retrieval gap (the load-bearing one)

The auto-inject pass for `auto_inject_hint: true` skills previously appended a `[See prompts/get <name> for full methodology.]` pointer to the matched tool's description. **Agents in real MCP clients can't reach `prompts/get`** — Claude Code, Claude Desktop, Cursor, and Continue all expose only `tools/*` to the model; the `prompts/*` plane was designed for human-invoked slash commands. The pointer was a dangling reference; operator-authored methodology was unreachable by the agent.

This release replaces the pointer with **full-body embedding** under a `## Methodology` header in the matched tool's description. Bounded by the existing 4 KB soft / 16 KB hard size caps the framework enforces per skill. Operators who want the older pointer-shaped behaviour (or no inject at all) set `auto_inject_hint: false` per skill.

`prompts/list` and `prompts/get` continue to work — they're still useful for MCP clients that do surface prompts to the agent, plus CLI introspection (`mcp-server skills-show`). The auto-inject path just becomes the primary delivery channel for the agentic clients in use today.

Found by an mcp-servers operator deploying kglite 0.9.31 + mcp-methods 0.3.36 against a live Claude Code session. Their three eval queries passed only because `graph_overview` happened to inline schema + methodology in one tool response; skills with non-parallel property prefixes, multi-hop temporal idioms, or out-of-schema content would have failed silently.

### Fixed — silent file-drop on YAML parse error

`SkillRegistry.from_manifest` previously silently skipped any SKILL.md whose frontmatter failed to parse. The intent was "one broken skill in a domain pack shouldn't take down the rest" — but the surface was *too* silent: operators hitting an unquoted colon in a description (`First clause: second clause`, which PyYAML reads as a mapping value separator) spent 25-minute debug sessions wondering why their override "took" but didn't show up in `prompts/list`.

Two-channel fix:

1. **`tracing::warn!`** continues to fire per dropped file. Operators with structured tracing get the warning immediately.
2. **New `ResolvedRegistry.parse_warnings()`** Rust getter and **`SkillRegistry.parse_warnings` Python getter** — durable structured surface that downstream binaries can render in their boot summary. Returns `Vec<ParseWarning>` (Rust) or `list[dict]` (Python) with per-file path + error.

```rust
let registry = SkillRegistry::new()
    .auto_detect_project_layer(&yaml_path)
    .finalise()?;
for warning in registry.parse_warnings() {
    eprintln!("skill drop: {} — {}", warning.path.display(), warning.error);
}
```

```python
reg = SkillRegistry.from_manifest("./my_mcp.yaml")
for w in reg.parse_warnings():
    print(f"skill drop: {w['path']} — {w['error']}")
```

Same pattern as the existing `(no .env found)` boot lines downstream binaries already render — operators internalised that channel.

### Tests + docs

- `load_skills_from_dir_surfaces_yaml_parse_failure_as_warning` — reproduces the operator's colon-in-description scenario; good skill still loads, broken file surfaces as a structured warning.
- `resolved_registry_parse_warnings_propagated_from_project_layer` — end-to-end through `Registry::finalise`.
- `serve_prompts_auto_injects_full_body_into_matching_tool` — replaces the pre-0.3.37 pointer-assertion test; now asserts `## Methodology` header + body sentinel are present, and that `prompts/get` is **not** referenced (anti-regression).
- `docs/guides/writing-effective-skills.md` gains a "How skill bodies reach the agent" section explaining the retrieval-gap rationale and authoring implications.

### Backwards compatibility

- A skill without `auto_inject_hint` (or with `auto_inject_hint: true`, the default) now produces a larger tool description than in 0.3.36. The size is bounded by the per-skill caps; operators who want the old behaviour set `auto_inject_hint: false`.
- `parse_warnings` is a new field on `ResolvedRegistry` — additive, doesn't break any existing usage.
- The `load_skills_from_dir` function's return type changed from `Result<Vec<Skill>, _>` to `Result<(Vec<Skill>, Vec<ParseWarning>), _>`. This is technically a breaking change for anyone calling that function directly, but kglite's audit and our own grep confirmed no downstream consumers reach for it — they go through `Registry::finalise` instead.

## 0.3.36 — 2026-05-14

### Added — `applies_when:` predicate gating on individual skills

Skills can now declare predicate conditions for activation. Skills with `applies_when:` predicates that don't evaluate to true are silently omitted from `prompts/list` and `prompts/get`. Inspired by kglite's three-deployment scenario (legal / o&g / code) where the same skill catalogue ships to every deployment but only some skills should fire per-graph.

```yaml
---
name: read_code_source
description: Resolve qualified_name → source slice. TRIGGER when ...
applies_when:
  graph_has_node_type: [Function, Class]
  graph_has_property: { node_type: Function, prop_name: module }
  tool_registered: cypher_query
  extension_enabled: csv_http_server
---
```

Bounded predicate set — not a DSL. All populated predicates are ANDed. Predicate categories:

- **Framework-internal** — `tool_registered`, `extension_enabled`. Dispatched against `ServerOptions.{tool_router, extensions}` without consulting any evaluator.
- **Domain** — `graph_has_node_type`, `graph_has_property`. Dispatched via the new `SkillPredicateEvaluator` trait. Downstream binaries register one with `Registry::with_predicate_evaluator(...)` before `finalise()`.

Evaluators that don't recognise a clause return `None`; the framework records `Unknown` and treats it as inactive — safer than silently activating the wrong-domain skill.

### Behaviour split: operator-facing vs agent-facing

- **Agent-facing `prompts/list` / `prompts/get`** — inactive skills are silently omitted. `tracing::info!` logs the failed clauses so the boot log carries full attribution.
- **Operator-facing `mcp-server skills-list`** — shows every resolved skill with an `active`/`inactive` column and indented per-clause outcomes for inactive ones. Mirrors the existing loud-collision pattern.

Mirrors the existing split for workspace activation: operator sees full provenance, agent sees only the resolved choice.

### `ServerOptions.extensions` field

`ServerOptions` gains an `extensions: serde_json::Map<String, Value>` field, populated from the manifest's `extensions:` block by `from_manifest`. Empty map when no `extensions:` block is present.

Downstream binaries that previously read `Manifest.extensions` directly can now read the same data off `ServerOptions` without re-loading the manifest. The framework uses this internally for the `extension_enabled:` predicate.

### Python surface — `Skill.applies_when`

`mcp_methods.Skill` gains an `applies_when` getter returning a `dict | None`. Python operators can pre-filter their registries before calling `register_skills_as_prompts`:

```python
for skill in registry.skills():
    if skill.applies_when and "graph_has_node_type" in skill.applies_when:
        # filter against our domain state, skip if predicate fails
        ...
```

The Python `SkillPredicateEvaluator` trait is not surfaced in this release — operators who need domain predicate dispatch from Python should pre-filter manually using `applies_when` introspection. Native Python evaluator support requires a Python-callable → Rust-trait bridge that's deferred to a future release.

### Backwards compatibility

A SKILL.md without `applies_when:` is always active — every existing skill keeps working unchanged. The `tool_registered` and `extension_enabled` framework-internal predicates work without any evaluator wired in, so single-domain consumers get most of the predicate engine for free.

## 0.3.35 — 2026-05-14

### Added — skills-aware MCP via the `prompts/` namespace

A new top-level manifest field, `skills:`, opts a deployment into
shipping operator-authored methodology as MCP prompts. The three-layer
composition is **project → domain pack → bundled defaults**, with
later-layer (i.e. project) entries fully replacing same-named entries
in the lower layers:

```yaml
name: kglite-mcp-server
skills:
  - true                    # framework defaults (5 bundled skills)
  - ./my-skills/            # domain skill-pack (operator-declared dir)
```

Polymorphic shape: `skills: false` (default), `skills: true` (bundled
only), `skills: ./path/` (one domain pack), or a list mixing booleans
and paths. The auto-detected project layer is `<basename>.skills/`
adjacent to the manifest YAML — if present, it wins over both
domain-pack and bundled.

Bundled framework defaults ship five SKILL.md files (`grep`,
`read_source`, `list_source`, `github_issues`, `repo_management`) that
operators inherit when `skills: true`. Downstream binaries can add
their own bundled layer via `Registry::add_bundled_many(...)` before
finalising.

### Wiring helpers for downstream binaries

Downstream binaries reach the wiring with roughly ten lines of glue:

```rust
let manifest = mcp_methods::server::load_manifest(&path)?;
let registry = mcp_methods::server::SkillRegistry::new()
    .merge_framework_defaults()
    .auto_detect_project_layer(&path)
    .layer_dirs(&manifest.skills, &path)?
    .finalise()?;
let mut server = mcp_methods::server::McpServer::new(opts);
mcp_methods::server::serve_prompts(&registry, &mut server);
```

`serve_prompts(registry, server)` populates the `prompts/list` /
`prompts/get` surface and applies an auto-injection pass on tool
descriptions: skills with `auto_inject_hint: true` whose name matches
a registered tool get a "see `prompts/get` `<name>` for full
methodology" pointer appended to that tool's description, so agents
who only read `tools/list` can still find the methodology.

### Zero impact on existing deployments

A manifest with no `skills:` declaration is a verbatim-current deploy:
no `prompts/` capability advertised in `get_info`, no behavioural
diff. The constraint is enforced at the type level via
`SkillsSource::Disabled` (the default) and at the runtime level via
the empty `prompt_router` — the rmcp default `list_prompts` /
`get_prompt` impls return empty / not-found when no skills are
registered.

### Python bindings

`mcp_methods.SkillRegistry` and `mcp_methods.Skill` ship as thin
pyo3 wrappers (newtype + delegate). `SkillRegistry.from_manifest(path,
include_bundled=True)` builds the registry; `register_skills_as_prompts(app,
registry)` in `mcp_methods.fastmcp` wires it into a FastMCP server in
one call. The framework crate (`mcp-methods`) remains pyo3-free; the
CI gate (`cargo tree -p mcp-methods -e all | grep pyo3` returns empty)
is unchanged.

### Frontmatter schema

Each SKILL.md file ships YAML frontmatter for metadata:

```yaml
---
name: cypher_query
description: One-line summary for `prompts/list`.
applies_to:
  mcp_methods: ">=0.3.35"
references_tools:
  - cypher_query
references_arguments:
  - cypher_query.format
auto_inject_hint: true     # default
applies_when: []           # phase-3 predicate hooks; parsed but inert
---

# Methodology body in markdown...
```

`name` and `description` are required; everything else is optional.
The `applies_when:` predicate evaluator is deferred to a later release
— predicates parse but don't gate yet.

### Size limits

The registry enforces three soft/hard caps to keep the prompt surface
healthy: 4 KB lint-warns, 16 KB hard-fails a single skill, 64 KB caps
the resolved-set total. The framework's bundled skills round-trip
through CI tests that pin the limits.

### Bundled-skill rewrite + authoring template

The five bundled skills (`grep`, `read_source`, `list_source`,
`github_issues`, `repo_management`) ship with rewritten descriptions
and bodies aligned to the patterns Anthropic publishes for their own
skills at github.com/anthropics/skills. Descriptions grew from ~15
words to 80-140 words with explicit TRIGGER / SKIP language (the
recommended shape for combating Claude's tendency to undertrigger).
Bodies gained Quick Reference tables, Common Pitfalls sections with
❌/✅ markers, and (for `github_issues`) an ASCII decision tree
mapping user phrasings to the three modes.

Operators authoring their own skills get a scaffold helper:

```bash
mcp-server skills-new ./my.skills/ my_method "TRIGGER when ..."
```

```python
mcp_methods.write_skill_template("./my.skills/", name="my_method", description="TRIGGER when ...")
```

The template emits a parse-valid SKILL.md with the body skeleton
(Overview → Quick Reference → Common Pitfalls → "When wrong") and
the optional extension fields commented out. Empty descriptions are
refused — a blank description guarantees the skill will never
trigger.

A new docs guide,
[Writing Effective Skills](docs/guides/writing-effective-skills.md),
distils the patterns from Anthropic's published skills repo plus
their best-practices doc.

## 0.3.34 — 2026-05-14

### Added — `tools[].bundled: rename:` per-deployment tool aliasing

The bundled-override shape from 0.3.31 gains a third axis:

```yaml
tools:
  - bundled: cypher_query
    rename: legal_cypher_query           # expose to agent under a different name
    description: "Cypher against the legal corpus knowledge graph."
  - bundled: graph_overview
    rename: legal_graph_overview
  - bundled: ping
    hidden: true
```

Use case: when an operator runs multiple downstream servers each
backed by a different data source (e.g. three kglite-mcp-servers
for `open_source`, `legal`, `prospect`), the bundled tool names
collide across servers and MCP `ToolSearch` results rank them
near-identically. The agent can't disambiguate which `cypher_query`
to use. Rename lets each deployment expose distinct names —
`open_source_cypher_query`, `legal_cypher_query`,
`prospect_cypher_query` — so search ranking and tool selection
behave correctly.

The rename composes with the existing `description:` and `hidden:`
axes; an override can use any combination.

### Schema

| Field | Type | Default | Notes |
|---|---|---|---|
| `rename:` | string | unset | Replaces the bundled tool's agent-facing name. Must be a valid identifier (`^[a-zA-Z_][a-zA-Z0-9_]*$`). `None` means "keep the original bundled name." |

Permitted alongside `bundled:`, `description:`, `hidden:`. Forbidden
on tool-creation (`cypher:` / `python:`) entries — the override-only
constraint from 0.3.31 still applies.

### Framework records, consumer enforces

The framework parses `rename:` and surfaces it via `BundledOverride.rename`
and `Manifest::to_json()`. It does **not** validate that the renamed
name collides with another tool (bundled, cypher, or python) in the
manifest — the framework doesn't know the downstream binary's full
bundled-tool catalogue, so collision detection has to live in the
consumer.

The kglite consumer landing in 0.9.30 does this check at boot:
"rename doesn't shadow any other registered tool" → exit cleanly
with a clear error if it does. Same pattern as advisory trust
gates: framework declares, consumer enforces.

### JSON shape addition

`Manifest::to_json()` emits the new field on bundled entries:

```json
{
  "kind": "bundled",
  "name": "cypher_query",
  "rename": "legal_cypher_query",   // null when not declared
  "description": "...",
  "hidden": false
}
```

Non-breaking under the existing JSON-shape stability guarantee —
consumers that ignore unknown keys keep working; consumers that
want rename support match on the `rename` key.

### Rust API change (heads up)

`BundledOverride` struct gains a `rename: Option<String>` field.
Consumers destructuring this struct will get a compile error and
need to handle the new field:

```rust
// Before 0.3.34
let BundledOverride { name, description, hidden } = &override_spec;
// After 0.3.34 — compile error: field `rename` missing
let BundledOverride { name, description, hidden, rename } = &override_spec;
```

Non-exhaustive struct attribute isn't applied — additions remain
visible at consumer-build time, which is the right failure mode
for adding new fields to a public struct.

### Tests

Six new tests in `manifest.rs::tests` under the `bundled_*` family:

- `bundled_rename_parses_when_valid_identifier` — happy path
- `bundled_rename_alongside_description_parses` — composes with `description:`
- `bundled_rename_defaults_to_none` — omitting `rename:` produces None
- `rejects_bundled_rename_with_invalid_identifier` — `123-bad` rejected
- `rejects_bundled_rename_with_non_string_value` — `42` rejected
- `bundled_rename_serialises_to_json` — JSON shape addition verified

123 mcp-methods unit tests pass (was 117 at 0.3.33; +6 net). 7
deployed_manifests + 7 mcp-server tests unchanged.

### Origin

Reported by the mcp-servers operator's post-0.9.29 ToolSearch audit:
multiple kglite servers with identical tool surfaces caused ranking
ambiguity. kglite implemented the framework half and pushed it to
this tree; reviewed, no design changes needed.

## 0.3.33 — 2026-05-14

### Changed — `workspace.applies_to` now accepts glob patterns + lists

The opt-in declaration introduced in 0.3.32 was a single literal
path string. 0.3.33 widens it to three shapes:

```yaml
workspace:
  # 1. Literal pattern (existing in 0.3.32) — match this exact name
  applies_to: ./repos

  # 2. Glob pattern (NEW) — match a subset by naming convention
  applies_to: ./prod-*      # any child whose name starts with "prod-"
  applies_to: ./*           # any direct child (wildcard)
  applies_to: ./test-?      # one-char suffix variant

  # 3. List of patterns (NEW) — match if any pattern matches
  applies_to: [./repos, ./clones]
  applies_to: [./prod-*, ./test-*]   # multiple naming conventions
```

Glob syntax follows [globset](https://docs.rs/globset) (the
ripgrep/cargo crate): `*` (any chars), `?` (one char), `[abc]`
(character class), `**` (any depth — irrelevant here since the
parent-walk is single-level).

The opt-in semantics are unchanged: when the parent-walk fallback
finds a manifest, the framework compares the actual `workspace_dir`'s
basename against the declared pattern(s). Match → use the manifest.
No match or no declaration → operator must pass `--mcp-config`
explicitly. The accidental-discovery footgun stays closed; this
release just widens what an author can opt into.

### Use case

Operators with a `repos/` sandbox containing many projects no
longer need to enumerate each child:

```text
repos/
├── workspace_mcp.yaml      # applies_to: ./*
├── project-alpha/          # --workspace repos/project-alpha/  ✓
├── project-beta/           # --workspace repos/project-beta/   ✓
└── client-acme/            # --workspace repos/client-acme/    ✓
```

Or filter by naming convention:

```text
deployments/
├── workspace_mcp.yaml      # applies_to: ./prod-*
├── prod-api/               # ✓
├── prod-web/               # ✓
├── stage-api/              # ✗  (parent-walk discovery refuses)
└── test-runner/            # ✗
```

### Parse-time validation

Patterns are validated at boot via `globset::Glob::new`:

- Empty patterns rejected (`applies_to: ""`)
- Multi-segment paths rejected (`applies_to: ./a/b/c` —
  parent-walk is single-level, so deeper paths could never match)
- Parent-relative rejected (`applies_to: ..`)
- Absolute paths rejected (`applies_to: /abs/foo`)
- Invalid glob syntax rejected (e.g. unterminated `[`)

Operators get a clear error at boot rather than silent non-matching.

### Internal shape change

`WorkspaceConfig::applies_to` changed from `Option<String>` to
`Option<AppliesTo>` where:

```rust
pub enum AppliesTo {
    Pattern(String),         // single pattern
    Patterns(Vec<String>),   // multiple patterns
}
```

Leading `./` is stripped at parse time (storage is normalised).
This is a Rust-API surface change since 0.3.32; consumers that read
`manifest.workspace.applies_to` need to handle the enum.

### JSON shape change

`Manifest::to_json()` now emits `workspace.applies_to` polymorphically:

- `Pattern("repos")` → `"repos"` (string)
- `Patterns(["repos", "clones"])` → `["repos", "clones"]` (array)
- `None` → `null`

Mirrors the YAML shape. Consumers should accept both types.

### New dependency

Direct dependency on `globset = "0.4"` added to `mcp-methods`. The
crate was already a transitive dep via `ignore`; making it direct
just lets the manifest parser validate glob patterns at boot.

### Tests

Six new tests in `manifest.rs::tests`:

- `find_workspace_applies_to_wildcard_matches_any_child` — `*` matches three different child names
- `find_workspace_applies_to_glob_matches_prefix` — `./prod-*` matches `prod-api`/`prod-web` but not `test-*`
- `find_workspace_applies_to_list_matches_any_entry` — `[./repos, ./clones]` matches either, rejects others
- `applies_to_rejects_deep_path_at_parse_time` — `./a/b/c` errors at boot
- `applies_to_rejects_invalid_glob_at_parse_time` — `./[unterminated` errors at boot
- `applies_to_rejects_parent_relative` — `..` and `../up` both error

117 mcp-methods unit tests pass (was 111 at 0.3.32; +6 net). 7
deployed_manifests + 7 mcp-server tests unchanged.

### Origin

Operator scenario raised by Kristian: a `repos/` directory holding
many child repos, with one manifest covering them all. Single-path
match didn't cover this; glob + list shapes do.

## 0.3.32 — 2026-05-14

### Changed — `find_workspace_manifest` parent-walk fallback (opt-in)

`find_workspace_manifest(workspace_dir)` now also looks one level up
for `workspace_mcp.yaml`, but only when the parent manifest *declares*
it applies to the given workspace dir via a new field:

```yaml
# open_source/workspace_mcp.yaml
workspace:
  kind: github
  applies_to: ./repos    # opt-in: parent-walk auto-detection allowed
                         # for `--workspace open_source/repos/`
```

When the operator passes `--workspace open_source/repos/`:
1. Primary location (`open_source/repos/workspace_mcp.yaml`) is
   checked first; if present, it wins unconditionally.
2. If absent, the parent (`open_source/workspace_mcp.yaml`) is
   considered — but only if it declares `workspace.applies_to: ./repos`
   AND that path canonicalises to the actual `workspace_dir`.

Without the declaration (or with a mismatched path), the framework
refuses to auto-detect — operator must pass `--mcp-config` explicitly.
The opt-in eliminates the accidental-discovery footgun where a
project-root manifest would silently attach to any sibling dir an
operator happened to pass to `--workspace`.

#### Why opt-in

The original design considered an unconditional parent-walk fallback.
That would have introduced a regression: an operator with
`~/proj/workspace_mcp.yaml` (for their real workspace) accidentally
running `mcp-server --workspace ~/proj/some_other_dir/` would silently
inherit the wrong manifest. The opt-in eliminates this entirely — the
manifest author *declares* which child the manifest covers, and the
framework only matches that declaration.

The natural layout this enables:

```text
open_source/
├── workspace_mcp.yaml     # declares workspace.applies_to: ./repos
└── repos/                 # --workspace points here; auto-detect works
```

Operators in this layout no longer pass `--mcp-config` for every
workspace launch.

#### New schema field

`workspace.applies_to: <relative path>` — optional string, must be
non-empty when set. Resolved against the manifest's parent
directory. Unset = no parent-walk discovery for this manifest (the
safe default for existing manifests).

`Manifest::to_json()` emits the new field under `workspace.applies_to`
(string or null). Non-breaking JSON shape addition under the existing
stability guarantee.

#### Logging

When the parent-walk fallback is considered, the framework emits a
`tracing` log at INFO level explaining the discovery outcome:

- `"manifest discovered via parent-walk fallback (workspace.applies_to matched)"` — on success
- `"parent-walk manifest does not declare workspace.applies_to; ignoring"` — declaration absent
- `"parent-walk manifest's workspace.applies_to does not match this workspace_dir; ignoring"` — mismatch

Operators with `RUST_LOG=info` (or any tracing subscriber wired up by
their binary, which `mcp-server`'s `init_tracing` does by default)
see exactly why discovery did or did not succeed.

### Tests

Six tests under `find_workspace_*`:

- `find_workspace_works` — primary location (existing).
- `find_workspace_walks_one_level_up_with_applies_to` — opt-in
  match returns the parent manifest.
- `find_workspace_ignores_parent_without_applies_to` — parent
  manifest without declaration is NOT auto-detected.
- `find_workspace_ignores_parent_with_mismatched_applies_to` —
  declaration that doesn't match is rejected.
- `find_workspace_returns_none_when_missing_everywhere` — no
  manifest in child or parent → None.
- `find_workspace_primary_wins_over_parent_fallback` — primary
  always preempts even when both could match.

111 unit tests pass (was 108 at 0.3.31 head; +3 net after refactor —
two new opt-in tests, the existing walks-up test renamed to make the
opt-in requirement explicit).

### Origin

kglite operator audit during the 0.6.x → 0.9.x migration: workspace
manifests sitting next to (not inside) the workspace dir required
`--mcp-config` for every launch. They proposed and implemented the
parent-walk in their local tree; we picked the opt-in shape over
their unconditional version to eliminate the accidental-discovery
risk.

## 0.3.31 — 2026-05-13

### Added — `tools[].bundled:` override shape

Manifest `tools:` arrays now accept a third entry kind alongside
`cypher:` and `python:`: `bundled:` entries customise the
agent-facing surface of a bundled tool the downstream binary
provides natively (e.g. `cypher_query`, `repo_management`,
`graph_overview`).

```yaml
tools:
  - bundled: repo_management        # name in the value, no `name:` line
    description: |                  # override the agent-visible description
      FIRST STEP for this server. Call repo_management('org/repo')
      to clone + build a repo before any other tool.

  - bundled: ping                   # hide a tool from tools/list AND
    hidden: true                    # reject calls to it

  - name: similar_sessions          # existing cypher-tool shape unchanged
    cypher: |
      MATCH ...
```

Field semantics:

| Field | Type | Default | Notes |
|---|---|---|---|
| `bundled:` | string | (required) | The bundled tool's name. Must match `^[a-zA-Z_][a-zA-Z0-9_]*$`. The framework does NOT validate against an actual catalogue (it doesn't know what bundled tools the downstream binary provides); the consumer is responsible for "unknown tool name" errors at boot. |
| `description:` | string | None | Replaces the bundled tool's default agent-facing description. `None` means "keep the default." |
| `hidden:` | bool | false | When true, the downstream consumer should omit the tool from `tools/list` AND reject calls to it. |

Mutually exclusive with `cypher:` / `python:` / `name:` /
`parameters:` / `function:`. The parser surfaces clear errors at
boot for combinations that don't fit.

### Motivation

The pre-0.3.31 customisation surface for bundled tools was the
manifest's global `instructions:` block — useful for first-message
agent orientation, but a single blob detached from individual
tools. Operators wanting to teach agents that `repo_management` is
the FIRST STEP, or hide `ping` from a production server, had to
stuff that into the global instructions and hope the agent
correlates it. Bundled overrides attach those concerns to the
tools directly: descriptions ride the `tools/list` response next
to the schemas, and `hidden:true` is the surgical opt-out for
"this tool is in the bundled catalogue but my agent shouldn't see
it."

The downstream `kglite-mcp-server` is the first consumer; it ships
bundled-override support in its next release after this lands. The
framework half of the work is small (~150 LoC parser + struct +
to_json variant + 11 tests) and entirely additive — pre-existing
`tools[].cypher` and `tools[].python` entries parse unchanged.

### JSON shape addition

`Manifest::to_json()` emits bundled override entries as:

```json
{
  "kind": "bundled",
  "name": "repo_management",
  "description": "FIRST STEP for this server...",
  "hidden": false
}
```

Non-breaking field addition to the existing `tools[]` array shape.
Consumers that ignore unknown `kind` values stay safe; consumers
that want bundled-override support match on `kind == "bundled"`.

### Tests

11 new parser tests in `crates/mcp-methods/src/server/manifest.rs::tests`:

- `bundled_override_with_description_parses`
- `bundled_override_with_hidden_parses`
- `bundled_override_alongside_cypher_tools_parses`
- `rejects_bundled_with_cypher_kind`
- `rejects_bundled_with_name_field`
- `rejects_bundled_with_parameters_field`
- `rejects_bundled_with_non_bool_hidden`
- `rejects_hidden_on_cypher_tool`
- `rejects_duplicate_bundled_overrides`
- `rejects_bundled_with_invalid_identifier`
- `bundled_override_to_json_shape`

Plus the existing 93 mcp-methods tests stay green (additive change;
no behaviour shift on the cypher / python paths).

### Fixed — `repo_management` cross-binary surface drift

The generic `mcp-server` CLI used to always register `repo_management`
in `tools/list`, regardless of whether a workspace was bound. Without
a workspace, calls to the tool returned `"repo_management requires
--workspace mode."` — but the tool was still visible to the agent.

Downstream binaries (e.g. `kglite-mcp-server`) gate the same tool
out of `tools/list` entirely when no workspace is bound. The
inconsistency surfaced when operators ran the same YAML against
`mcp-server` (saw the tool) and `kglite-mcp-server` (didn't see it).

`McpServer::new` now calls `tool_router.remove_route("repo_management")`
when `options.workspace.is_none()`. Tool surface matches downstream
gating; agents only see the tool when it can do something useful.

Two new tests anchor the behaviour:
- `repo_management_gated_to_workspace_mode` — bare framework,
  no workspace, tool is absent from `list_all()`.
- `repo_management_present_when_workspace_bound` — with a workspace,
  tool is present.

Reported by kglite while comparing the two binaries against the
same YAML post-0.9.25 deployment.

## 0.3.30 — 2026-05-12

### Distribution shape — bundled binary in the pip wheel

`pip install mcp-methods` now puts the `mcp-server` CLI on PATH again,
restoring the pattern we shipped in 0.3.25. The wheel bundles the
native Rust binary at `mcp_methods/_bin/mcp-server`; the Python entry
point `mcp_methods._cli:main` execs it on invocation.

This is the kglite-style "pip install gives you everything" UX,
adapted for our case: because our `mcp-server` binary has zero
libpython link (kept from the 0.3.26 pure-Rust separation), we stay
at 3 abi3 wheels per OS — no per-Python-version matrix.

### Why the revert

The 0.3.26 polars-shape split removed the bundled binary on
architectural grounds. Re-introducing it: pip is the operator-facing
distribution and the CLI is the operator-facing artefact. A separate
crates.io entry for `mcp-server` (the 0.3.29 plan) would have added a
distribution path nobody asked for. Single distribution path is
clearer.

`crates/mcp-server/` stays in the workspace as the binary's source
home — used by the wheel build to compile + bundle, and available
for downstream Rust users via `cargo install --git`. **Not published
to crates.io.**

### crates.io — library publication

`mcp-methods` 0.3.30 is the first version published to crates.io.
The library is pure Rust, zero pyo3 in the dep tree (the polars
shape from 0.3.26 is preserved). docs.rs auto-builds the rustdoc.
Downstream consumers can now write:

```toml
mcp-methods = "0.3"
```

…instead of pinning to a git rev. kglite's existing git-rev pin
keeps working unchanged.

### Files touched
- `pyproject.toml` — restored `[project.scripts]` + `[tool.maturin]
  include` blocks.
- `python/mcp_methods/_cli.py` — restored.
- `Makefile` — restored `bundle-bin` / `dev-with-bin` targets.
- `.github/workflows/build_wheels.yml` — restored the
  cargo-build-binary + copy-into-wheel step (per-OS).
- `.gitignore` — re-ignore `python/mcp_methods/_bin/`.
- `crates/mcp-methods/Cargo.toml` — publish metadata
  (repository, keywords, categories, docs.rs config).
- `crates/mcp-server/Cargo.toml` — version 0.3.29 → 0.3.30 (still
  workspace-local; not published).
- README.md — drop the `cargo install mcp-server` section; fold the
  CLI into the Python-install path.

### No regression for downstream Rust consumers

The library crate source is unchanged. kglite's `rev = "71f7ba6..."`
pin resolves identically. The new 0.3.30 publish to crates.io is
opt-in — downstream consumers who want to switch from git-rev to
`version = "0.3"` can, but nothing forces them.

## 0.3.29 — 2026-05-12

### Added
- `trust.allow_query_preprocessor` — third advisory trust gate
  alongside `allow_python_tools` and `allow_embedder`. The framework
  parses the bool and surfaces it through `TrustConfig` and
  `Manifest::to_json()`; downstream consumers enforce. Mirrors the
  existing `allow_embedder` pattern: the framework records the
  operator's declared trust, the consumer (e.g. `kglite-mcp-server`)
  refuses to boot the corresponding extension hook when the flag is
  false. Driven by the mcp-servers-operator's 0.9.25 Cypher
  preprocessor spec.

### JSON shape
- `Manifest::to_json()` now emits `trust.allow_query_preprocessor`.
  Non-breaking field addition; consumers that ignore unknown keys are
  unaffected. The `to_json_shape_is_stable` snapshot test has been
  updated to expect the new key.

### Tests
- `allow_query_preprocessor_trust_parses` — positive case.
- `allow_query_preprocessor_rejects_non_bool` — type validation.

## 0.3.28 — 2026-05-12

### Fixed
- `Workspace::set_root_dir` no longer clobbers the just-set
  `active_repo_path` back to the configured `workspace_dir`. The
  local-mode branch of `clone_or_update` now reads the current
  `active_repo_path` from state (falling back to `workspace_dir` only
  when state is unset), so the activate-after-set_root_dir sequence
  preserves the new root, and the post-activate hook fires against the
  bound target rather than the original configured root.

  Reported by kglite while wrapping `Workspace` in pyo3 for 0.9.24.
  Bug had no effect on github-mode workspaces; only local-mode
  `set_root_dir` callers who read `active_repo_path()` after the swap
  (or whose post-activate hook needed to rebuild against the new root)
  were affected.

### Added (tests)
- `set_root_dir_updates_active_path` — anchors the invariant.
- `set_root_dir_post_activate_fires_against_new_root` — confirms the
  hook fires against the bound target.

## 0.3.27 — 2026-05-12

### Added
- `Manifest::to_json() -> serde_json::Value` for FFI / RPC introspection.
  Returns a JSON-friendly view of the validated manifest with stable
  shape across patch releases. Intended for pyo3 wrappers, JSON-RPC
  bridges, and any consumer that needs programmatic access to the
  loaded manifest without per-field getters. Schema knowledge lives
  end-to-end on the framework side; downstream wrappers shrink to
  roughly one passthrough method plus a generic `serde_json::Value ->
  PyObject` (or equivalent) converter.

### Notes
- No breaking changes. No new dependencies (`serde_json` was already
  in the dep tree).
- The JSON shape is pinned by the `to_json_shape_is_stable` test in
  `crates/mcp-methods/src/server/manifest.rs` — read it as the canonical
  contract. Future renames or removals in this shape are breaking
  changes; additions are non-breaking.
- No `#[derive(Serialize)]` on any manifest struct: keeps the
  hand-rolled parsing symmetry on input and output, and avoids
  coupling JSON shape to Rust field names.

## 0.3.26

The polars / pydantic-core distribution shape. `mcp-methods` is now a
pure-Rust library with zero PyO3 in source or deps; the Python wheel
is built from a separate `mcp-methods-py` binding crate. `cargo add
mcp-methods` from Rust sees no traces of Python anywhere.

### Workspace restructure

Three-crate workspace (one was four-crate-ish in 0.3.25 with bundled-
binary machinery; that's gone too):

- **`crates/mcp-methods`** — pure-Rust library (rlib only). Contains
  every primitive (`cache`, `compact`, `files`, `git_refs`, `github`,
  `grep`, `html`, `json_grep`, `list_dir`) plus the `server`
  framework module behind the default-on `server` feature. **No
  `pyo3`, no `#[pyfunction]`, no `Py<…>`, no `PyResult` anywhere in
  the source.** CI verifies via `cargo tree -p mcp-methods | grep pyo3`
  as a fail-on-leak gate.
- **`crates/mcp-methods-py`** — PyO3 bindings crate (cdylib only).
  Depends on `mcp-methods` and provides `#[pyfunction]` / `#[pyclass]`
  wrappers around the library functions. `PyElementCache` is a newtype
  wrapper over `mcp_methods::cache::ElementCache`. The wheel is built
  from this crate via `pyproject.toml`'s `manifest-path` setting.
- **`crates/mcp-server`** — standalone CLI binary. Depends on
  `mcp-methods` with `features = ["server"]`. Published to crates.io
  alongside the library; install via `cargo install mcp-server`.

### Distribution

| Audience | Command | Result |
|---|---|---|
| Rust library users | `cargo add mcp-methods` | Pure-Rust crate. No pyo3 in dep tree. |
| Python users | `pip install mcp-methods` | **One abi3 wheel per OS** (3 total) — works on Python 3.10 through 3.13 without reinstall. Down from 12 per-Python-version wheels in 0.3.25. |
| CLI users | `cargo install mcp-server` | Binary on PATH. No libpython link. |

The bundled-binary mechanism from 0.3.25 is gone: no more
`mcp_methods/_bin/`, no `_cli.py` Python launcher, no
`[project.scripts]` entry, no maturin `include` for the binary, no
`make bundle-bin`. `pip install mcp-methods` ships only the cdylib
and Python wrapper; the binary lives in its own crate.

### Python `embedder:` / `tools[].python:` removed

The legacy framework hooks for loading Python embedder classes and
Python tool callables were removed. They required PyO3 in the
framework's source — incompatible with the pure-Rust contract. No
deployed manifest uses `tools[].python:`, and the lone embedder
consumer (kglite's sodir / norwegian_law / petrel servers) migrated
to fastembed-rs in 0.9.18. Downstream binaries that need Python
extension hooks add a pyo3 wrapper layer in their own cdylib.

### API shape changes

Callback-bound functions kept their Python-side surface (the wheel's
`read_file(transform=…)`, `list_dir(annotate=…)`, `ripgrep_files(transform=…)`
still take callables). On the Rust side, the callback is now a
generic `&dyn Fn(...)` parameter on a `ReadFileOpts` / `ListDirOpts`
/ `RipgrepFilesOpts` struct. Pure-Rust callers default-construct
the opts and skip the callback; the binding crate bridges
`Py<PyAny>` → Rust closure that re-enters Python via `Python::attach`.

`compact_discussion` returns `Result<(String, Option<String>), String>`
in Rust; the binding crate's wrapper maps to `PyValueError`.
`ripgrep_lines` and `ripgrep_json_fields` return typed
`Vec<RipgrepLinesGroup>` / `Vec<JsonGrepMatch>`; the wrapper converts
to Python lists of dicts.

### Migration

**For Python users:** zero changes. `from mcp_methods import …`
imports identical. Wheel layout (`mcp_methods/_mcp_methods.so`)
preserved.

**For Rust library consumers (kglite, etc.):** Cargo.toml stays the
same — `mcp-methods = { …, features = ["server"] }` still works
because we kept the `server` feature default-on. Source paths are
unchanged: `mcp_methods::server::McpServer`, `mcp_methods::cache::ElementCache`,
`mcp_methods::github::fetch_issue_internal`, etc. — all in the same
locations. Two things to know about:
- `mcp_methods::server::embedder` and `mcp_methods::server::python`
  modules are gone. kglite stopped using them in 0.9.18; if a future
  consumer needed them, they re-implement in their own pyo3 crate.
- `mcp_methods::server::apply_python_extensions` and
  `PythonExtensions` are gone for the same reason.

**For CLI users:** the binary distribution changed. Old: `pip
install mcp-methods` put it on PATH. New: `cargo install mcp-server`.
Reflects the binary's actual nature (a Rust CLI tool, not a
Python package).

### Tests

87 cargo unit tests + 7 mcp-methods integration tests + 7 mcp-server
binary tests + 176 python tests = 277 total. `cargo tree -p mcp-methods
| grep pyo3` is a CI gate; if it ever returns non-zero, the build fails.

## 0.3.25

Structural consolidation + the long-tail of the PyO3 decoupling work.
This release closes both distribution goals: `pip install mcp-methods`
puts `mcp-server` on PATH, and downstream Rust binaries
(`kglite-mcp-server`, etc.) can build with no transitive libpython
linkage via `default-features = false`.

### Workspace consolidation

- **`crates/mcp-server` is gone.** Its modules moved into the root
  `mcp-methods` crate under a new `server` feature. Every public type
  and function name is preserved — paths shift by one segment:
  - `mcp_server::McpServer` → `mcp_methods::server::McpServer`
  - `mcp_server::manifest::ToolSpec` → `mcp_methods::server::manifest::ToolSpec`
  - `mcp_server::build_tool_attr` → `mcp_methods::server::build_tool_attr`
  - etc.
- **Lib name renamed** `_mcp_methods` → `mcp_methods`. The leading-
  underscore convention is Python-internal; in Rust it forced
  consumers to write `use _mcp_methods::*`. Now they write
  `use mcp_methods::*`. Python's `from mcp_methods._mcp_methods import …`
  still works — maturin's `module-name` override places the cdylib
  at the same wheel path as before.
- **`crate::cache::ElementCache`, `crate::github::*`, `crate::git_refs::*`,
  `crate::compact::*`, `crate::html::*`** stay at the crate root —
  intra-crate references shift from `_mcp_methods::*` to `crate::*`.

### `pip install mcp-methods` ships `mcp-server` on PATH

- Wheel now bundles the native Rust binary under
  `mcp_methods/_bin/mcp-server` and registers a Python console-script
  entry point (`mcp-server = "mcp_methods._cli:main"`) that execs it.
- `make dev-with-bin` builds + bundles the binary locally; the wheel-
  build CI workflow runs the same sequence before maturin packages.
- Per-platform wheel size grows ~10 MB (the native binary).

### Cargo features

The crate's feature surface is now coherent across both build modes:

- `default = ["python", "server"]` — standard `cargo install` or wheel
  build path includes everything.
- `python` — opt-out drops PyO3 (no `_mcp_methods` cdylib, no
  callback-bound helpers like `read_file`/`ripgrep`/`list_dir`/
  `json_grep`, no manifest-declared `python:` tools or `embedder:`
  blocks).
- `server` — opt-out drops rmcp + tokio + clap + the framework
  modules + the `mcp-server` binary.
- `python-extension` — implies `python`, adds `pyo3/extension-module`
  for the wheel cdylib.

A `--no-default-features` build is the pure-Rust primitives subset.
A `--no-default-features --features server` build is the
framework-only path with no libpython linkage — verified with
`otool -L target/debug/mcp-server` showing no `libpython3.x.dylib`.

### Downstream migration (kglite + similar)

```toml
# Before — two separate deps:
mcp-methods = { git = "...", rev = "0.3.24", default-features = false }
mcp-server  = { git = "...", rev = "0.3.24", default-features = false }

# After — one dep with the `server` feature:
mcp-methods = { git = "...", rev = "0.3.25", default-features = false, features = ["server"] }
```

Source rename pattern: `mcp_server::X` → `mcp_methods::server::X`;
`_mcp_methods::X` → `mcp_methods::X`.

### CI

- Workspace-root rows added: `cargo build/test`,
  `cargo build/test --no-default-features`,
  `cargo build/test --no-default-features --features server`.
- Wheel-build workflow updated to build + copy the bundled binary
  before maturin runs.

### Tests

- 90 cargo unit tests (was 17 + 73 split across two crates) +
  7 manifest regression + 7 binary CLI tests + 19 smoke + 157 python
  = **280 tests** pass.
- All 5 deployed YAML manifests parse unchanged.

## 0.3.24

Responds to three kglite asks (inbox/read/2026-05-11-*). Three phases land
together; consumer-visible APIs remain backward-compatible.

### `mcp-server` framework

- **`extensions:` top-level manifest passthrough.** New top-level
  YAML key that the framework accepts but does not validate.
  Stored on `Manifest` as
  `extensions: serde_json::Map<String, serde_json::Value>`.
  Downstream binaries read whatever keys they need from it.
  Strict-unknown-key validation stays in force for the framework's
  own surfaces (`builtins:`, `workspace:`, …) — only the
  `extensions:` block is intentionally unvalidated. Closes
  kglite's csv-http extension ask; matches the domain-agnostic
  boundary set in 0.3.23.
- **PyO3 is now an optional Cargo feature on `crates/mcp-server`.**
  New `python` feature in `crates/mcp-server/Cargo.toml`, default-on.
  Standalone `cargo install mcp-server` is unchanged. Downstream
  binaries can disable with `default-features = false` to remove
  the direct PyO3 dep from this crate. Note: the top-level
  `mcp-methods` crate (the Python wheel) still depends on PyO3
  unconditionally — that is by design. The feature flag on
  `mcp-server` removes the *direct* link from this crate only;
  for a fully PyO3-free downstream binary the consumer also needs
  to gate the `mcp-methods` dep itself. CI now runs both
  `cargo build -p mcp-server --no-default-features` and
  `cargo test -p mcp-server --no-default-features` to catch leaks.
- **Cfg-gated modules**: `python.rs` and `embedder.rs` are now
  `#[cfg(feature = "python")]`. `runtime::PythonExtensions` and
  `runtime::apply_python_extensions` are similarly gated.
  `mcp-server` binary's `main()` emits a `warn`-level log if a
  manifest declares `python:` tools or `embedder:` in a binary
  built without the `python` feature, so operators aren't silently
  ignored.

### Tests + infrastructure

- **Framework smoke-test suite.** New `tests/test_mcp_server_smoke.py`
  drives the `mcp-server` binary over JSON-RPC stdio and exercises
  every tool category: source navigation, GitHub access (with and
  without token), `.env` walk-up + explicit `env_file:`, local-
  workspace mode, bare boot. 19 tests, ~3 seconds, auto-skips when
  the binary isn't built. Adapted from kglite's smoke suite shape;
  kglite-specific tests (graph / Cypher / read_code_source) stay
  in their repo.

## 0.3.23

Patch release in response to kglite's 0.3.22 smoke-test findings
(inbox/read/2026-05-10-from-kglite-smoke-test-findings.md). The
framework stays domain-agnostic — Cypher tool registration is a
graph-engine concern, so it stays on the kglite side. What we expose
here is the primitive that lets downstream binaries build that
registration cleanly.

### `mcp-server` framework

- **`mcp_server::build_tool_attr` now public.** Closes the primitives
  half of Gap A: downstream binaries can now build the rmcp `Tool`
  attr from `(name, description, json_schema)` in the same shape the
  framework uses internally and pair it with `ToolRoute::new_dyn` to
  register their own dynamic tools — Cypher runners, custom DSL
  handlers, anything else. No API divergence between the framework's
  own registrations and downstream ones.

### `mcp-methods` (Rust core)

- **`auth_token` filters empty strings.** Closes Gap C: an env var
  set to `""` (e.g. for clearing the token without unbinding) was
  reported as "token present" by `has_git_token()`, causing the
  github tools to register and then 401 on the first call. Now an
  empty string is treated identically to a missing var. Test added.

## 0.3.22

Closing the kglite wishlist gaps (inbox/read/2026-05-10) so the 5 deployed
YAML manifests + ~3,000 LoC of custom Python MCP servers can retire onto
this framework.

### `mcp-server` framework

- **`.env` auto-loading at startup.** Walks upward from the workspace /
  source-root / watch / cwd looking for `.env`; loads `KEY=VALUE` lines
  into the process env (skip blanks/`#`, strip outer quotes, do not
  overwrite existing). Operators who want a non-implicit pick declare
  `env_file: ../.env` at the YAML top level.
- **`github_issues` element drill-down.** New `element_id` / `lines` /
  `grep` / `context` / `refresh` arguments on the MCP tool. FETCH
  responses cache collapsed `cb_N` / `patch_N` / `comment_N` / `overflow`
  elements server-side (via `_mcp_methods::cache::ElementCache`); pass
  `element_id="cb_1"` (with the same `number=N`) to retrieve a single
  element without re-fetching, optionally narrowed by `lines="40-60"` or
  `grep="pat"`.
- **Honest tool listing.** `github_issues` and `github_api` are now
  registered dynamically at boot only when `GITHUB_TOKEN` is set; they
  no longer appear in tool-listing responses when the agent couldn't use
  them. Boot-time decision — restart to pick up a token that appears
  later. The framework binary logs an `info` line announcing the skip.
- **`Workspace::last_built_sha(name)`.** New public reader exposing the
  HEAD SHA persisted after the last successful post-activate hook for a
  repo. Backed by an additive `last_built_sha` field on the per-repo
  `inventory.json` entry (`#[serde(default)]` keeps older inventories
  loading cleanly). Foundation for the auto-rebuild-gating work landing
  in Phase C.
- **Embedder lifecycle (`mcp_server::embedder`).** New module exposing
  `EmbedderHandle` (load/unload/embed/touch + idle tracking) and
  `spawn_idle_watch` for the eviction tokio task. `PythonExtensions`
  now yields `Option<Arc<EmbedderHandle>>` plus `embedder_cooldown` and
  `embedder_watcher`. The framework owns the cooldown timer; the value
  is extracted from `embedder.kwargs.cooldown` in the manifest. Embedder
  classes must expose `embed` + `dimension`; `load` / `unload` are
  optional but called automatically when present.
- **Manifest schema** — new `env_file:` top-level key (added to
  `ALLOWED_TOP_KEYS`; strict-unknown-key validation preserved).
- **Auto-rebuild gating on `repo_management(update=True)`.** Captures
  the new HEAD SHA on each clone/fetch; when the gate sees
  `action == "current"` AND `prev_built_sha == new_head` AND the user
  did not pass `force_rebuild`, the post-activate hook is skipped and
  the response carries a `[build skipped: ...]` marker. New
  `force_rebuild: bool` argument on the `repo_management` MCP tool
  bypasses the gate (useful after the builder itself has been
  upgraded).
- **`workspace.kind: local` mode** — new top-level manifest block
  `workspace: { kind, root, watch }`. `kind: local` binds a fixed
  directory as the source root and reuses the same auto-rebuild
  gating (with a cheap recursive-mtime fingerprint instead of git
  HEAD SHA). When the manifest declares `kind: local`, that wins over
  the CLI `--workspace` flag's interpretation — manifest is the
  source of truth, same rule as `source_root:`. `watch: true` wires
  the framework's debounced file watcher to the root for hot-reload
  rebuilds. Local mode rejects `name=` / `delete=true` in
  `repo_management` with a friendly error. New `set_root_dir(path)`
  MCP tool (registered automatically when in local mode) swaps the
  active root at runtime — drop-in for the legacy `code_review` server
  workflow. `inventory.json` is stored under `<root>/.mcp-workspace/`
  in local mode to keep the user's tree clean.
- **`McpServer::builtins()` public getter.** Surfaces the manifest's
  `builtins:` block (`save_graph`, `temp_cleanup`) verbatim so
  downstream consumers (e.g. kglite's `graph_overview` tool deciding
  whether to wipe `temp/`) can read the operator's intent without
  re-parsing YAML. The framework does not act on these — it doesn't
  know what an "overview" call means.

### `mcp_methods.fastmcp` (Python)

New composable helpers for FastMCP authors so each one can drop the
boilerplate that the YAML+CLI binary handles. Each helper takes the
FastMCP `app` and the dependency it needs (graph, source roots, …)
and registers the same tools the bundled binary ships, one-to-one.

- `register_overview(app, graph, overview_prefix=None)` — `graph_overview`.
- `register_cypher_query(app, graph, csv_dir="temp/")` — `cypher_query`
  with `format="csv"` writing uuid-named files for streaming/exports.
- `register_source_tools(app, source_roots=[...])` — `read_source`,
  `grep`, `list_source` with the same path-sandbox semantics as the
  Rust framework (`os.path.realpath` rejects traversal). Wraps the
  existing PyO3 surface; ~10-line wrappers, no logic duplication.
- `register_save_graph(app, graph)` — `save_graph(path)`.
- `serve_csv_via_http(directory, port=0, bind="127.0.0.1")` — CORS-
  enabled HTTP server for serving CSV exports to browser-side agent
  contexts. Returns `(server, base_url)`; runs in a daemon thread.

A runnable end-to-end stub lives at `examples/fastmcp_demo.py`.

### Documentation + regression suite

- **README "Deployment" section.** Documents the `cargo install --path
  crates/mcp-server` recipe, the symlink path for deployments that
  pinned a binary elsewhere, and a table of the operating modes (bare /
  source-root / workspace[github] / workspace[local] / watch) with how
  to set each via CLI flag or manifest.
- **Regression test for deployed manifests.** New integration test
  (`crates/mcp-server/tests/deployed_manifests.rs`) asserts the schema
  shapes used by the 5 production manifests at
  `/Volumes/EksternalHome/Koding/MCP servers/` continue to load
  cleanly. If any starts failing after a schema change, a production
  manifest has been broken — fix the schema or migrate the deployment.

## 0.3.21

- **`McpServer::register_typed_tool<T, F>(name, description, handler)`**
  — typed dynamic-tool registration helper. Compresses the boilerplate
  of building a `Tool` attr from a JSON Schema (via schemars), serde-
  deserialising per-call arguments, and wrapping the handler in a
  `ToolRoute::new_dyn` closure. Domain binaries (kglite-mcp-server) use
  this to register their tools in ~5 lines instead of ~35. The handler
  is `Fn(T) -> String`; state lives in the closure environment.

## 0.3.20

### Added — `mcp-server` crate (Rust-native MCP server framework + binary)

A new sibling crate at `crates/mcp-server/` providing a Rust-native MCP
server built on the official `rmcp` SDK (v1.6) with a stdio transport.
Designed to replace the Python `kglite.mcp_server` over the next few
phases. The new binary is `mcp-server`.

**CLI refit** — drop `--graph` (graph concept lives in downstream
binaries like kglite-mcp-server, not in mcp-methods). Replace with
`--source-root DIR` for direct binding of the source tools to a fixed
directory. Modes are now: `bare` / `--source-root` / `--workspace` /
`--watch`. Help text rewritten to clarify the framework's domain-
agnostic role; downstream binaries (kglite, etc.) layer their domain
tools on top by re-using `McpServer::new` with custom registrations.
Also dropped `--embedder` (the manifest's `embedder:` block remains
the source of truth).

**Library extraction (post-phase-6).** The crate now exposes a
`[lib]` target alongside the `[[bin]]`, with the framework boilerplate
(`apply_python_extensions`, `resolve_source_roots`, `init_tracing`,
`maybe_watch`) lifted into a new `mcp_server::runtime` module. This
lets downstream binaries (kglite-mcp-server, etc.) reuse the entire
boot sequence without copy-pasting hundreds of LoC of glue.

`mcp_server::python::json_to_py` is now `pub` so dynamic-tool
implementations can reuse it for forwarding JSON kwargs to Python
callables (e.g. `graph.describe(**kwargs)`).

**Phase 6** — Workspace mode (`--workspace DIR`). Multi-repo
clone-and-track flow with idle-sweep inventory.

- `repo_management` MCP tool: pass `name='org/repo'` to clone (if
  missing) and activate; `delete=true` to remove; `update=true` to
  fast-forward the active repo; no args to list with access counts.
- Active repo state lives in `Workspace::Arc<RwLock<…>>`; source tools
  pick up the swap on the very next call (`with_workspace` wires
  dynamic source-roots and default-repo providers).
- Inventory persists to `<workspace>/inventory.json` with
  `cloned_at` / `last_accessed` / `access_count` / `stale` per repo;
  reconciles with on-disk state at boot (un-tracked clones get
  inventory entries; vanished repos marked stale).
- Auto-sweeps idle repos older than `--stale-after-days` (default 7);
  active repo is exempt; stale entries preserve their access history
  even after deletion.
- Git operations shell out to `git` (clone --depth 1, fetch + reset
  --hard FETCH_HEAD); no `libgit2` dep.
- `PostActivateHook` callback type lets downstream binaries fire
  custom logic after each successful clone/update — kglite-mcp-server
  will use this to invoke `code_tree::build` on the freshly-activated
  repo and pin the resulting graph.
- Self-contained ISO-8601 (seconds-precision) formatter avoids
  pulling in `chrono` for a handful of timestamps.
- End-to-end test: cloned `rust-lang/rustlings` from github.com,
  `list_source` returned the active repo's tree, `repo_management()`
  listing shows it as `[active]` with access counts.

**Phase 5** — Watch mode (`--watch DIR`). The CLI now spawns a
recursive debounced filesystem watcher (default 500 ms debounce) that
logs change events at INFO level and can fire a downstream-supplied
callback on each batch. Source roots are auto-pinned to the watched
directory so the source tools (`read_source` / `grep` / `list_source`)
operate on the live tree. Powered by the `notify` + `notify-debouncer-mini`
crates. Downstream binaries (kglite-mcp-server, etc.) plug in their
rebuild logic via the `ChangeHandler` callback type — phase-6 work
will wire one in for code-tree rebuilding once kglite layers in.

**Phase 4** — Python extension layer. Embeds CPython into the
binary via PyO3's `auto-initialize` feature so manifest-declared
`python:` tools and custom embedder factories work out of the box.

- Manifest `tools: [{ name, python: ./X.py, function: F }]` entries
  load via `importlib.util` (no `sys.path` mutation), introspect the
  function signature with `inspect.signature` to derive a JSON Schema
  for MCP, and register dynamically on rmcp's tool router via
  `ToolRoute::new_dyn` so the agent sees them in `tools/list`
  alongside the built-in source / GitHub tools.
- Manifest `embedder: { module, class, kwargs }` block is loaded
  + instantiated at boot. The PyObject is held in memory; downstream
  binaries (kglite-mcp-server) wire it to a graph's text-score path
  via their own integration.
- Two-signal trust gating: `python:` tools require both
  `trust.allow_python_tools: true` and `--trust-tools`; embedders
  require `trust.allow_embedder: true` + `--trust-tools`. Refusing
  either way is the default. Boot-time errors are clear and specific.
- Type-hint → JSON Schema mapping: `str → string`, `int → integer`,
  `float → number`, `bool → boolean`, `list → array`, `dict → object`.
  Unmatched annotations fall back to `string` with the Python repr
  in the schema's `title` field. Defaults are JSON-encoded into
  `default`.
- `ServerHandler` impl on `McpServer` switched from `Self::tool_router()`
  (static) to `self.tool_router` (instance) so dynamic routes added
  before serving show up in `tools/list`.
- End-to-end test: a `def greet(name: str, count: int = 3) -> str`
  Python function loads, registers with the right schema (`name`
  required, `count` defaulted), and dispatches correctly through
  `tools/call`.

The FastMCP-API-compat shim (`from mcp_methods.fastmcp import FastMCP`)
is deferred to a follow-up. The manifest form already accepts any
plain Python function — users with existing FastMCP code just declare
each decorated function as a separate manifest entry until the shim
ships.

**Phase 3** — GitHub tools (`github_issues`, `github_api`) registered
on the rmcp server. Both tools resolve the active repo via a
caller-supplied dynamic provider with an optional per-call `repo_name=`
override; auto-detects from cwd's git remote as a last resort.
`github_issues` covers all three modes — FETCH (`number=`), SEARCH
(`query=`), LIST (no args) — by delegating to the existing
`mcp_methods::github` Rust internals (no PyO3 in the hot path). To make
that delegation possible, mcp-methods now ships as both a Python
extension (`cdylib`) and a regular Rust library (`rlib`), with the
`extension-module` PyO3 feature gated behind the new `python-extension`
Cargo feature so `cargo build` can produce both. `pyproject.toml`
maturin features updated accordingly. End-to-end stdio test against
github.com/rust-lang/rustlings confirmed.

**Phase 2** — source tools (`read_source`, `grep`, `list_source`)
registered on the rmcp server. Each tool is gated on the server having
an active source-roots provider (static or dynamic); when none is
configured, the tool returns a friendly "configure source_root in your
manifest" error rather than crashing. Implementation lives in
`crates/mcp-server/src/source.rs` and uses the same `ignore` +
`grep-matcher`/`grep-regex`/`grep-searcher` crates as the existing
mcp-methods primitives. `ServerOptions` gains `with_static_source_roots`
+ `with_dynamic_source_roots`. Manifest `source_root(s)` are now
canonicalised at boot and wired into the server. 15 new unit tests
covering read/grep/list semantics, glob filtering, traversal blocking.

**Phase 1** — bootstrap. Boots a working MCP server with the framework
wired end-to-end, plus the manifest schema parsed and validated. No
real tools yet — that's phase 2+.

- Cargo workspace conversion of `mcp-methods` (root crate unchanged;
  `crates/mcp-server/` added as a sibling member).
- YAML manifest parser at `crates/mcp-server/src/manifest.rs` — direct
  port of the Python `kglite.mcp_server.manifest` schema. Same keys
  (`name`, `instructions`, `overview_prefix`, `source_root(s)`,
  `trust.{allow_python_tools, allow_embedder}`, `tools`, `embedder`,
  `builtins.{save_graph, temp_cleanup}`), same validation messages,
  same auto-detection of sibling (`<basename>_mcp.yaml`) and
  workspace-level (`workspace_mcp.yaml`) manifests.
- `clap`-based CLI matching the Python flags: `--graph` /
  `--workspace` / `--watch` (mutually exclusive), `--mcp-config`,
  `--embedder`, `--name`, `--trust-tools`, `--stale-after-days`.
- rmcp `ServerHandler` impl with one `ping` tool to verify the
  framework dispatch is wired. End-to-end stdio handshake confirmed
  (initialize → tools/list → tools/call).
- 25 unit tests covering manifest parsing edge cases and CLI mode
  picking.

## 0.3.19

- **Built-in `html_to_text` function** — converts HTML to clean, readable text optimized for LLM consumption. Strips `<head>`, `<script>`, `<style>`, and comments. Converts headings to markdown `#` prefixes, list items to `- ` bullets, bold/strong to `**text**`, images to `[image: alt]`, tables to tab-separated text, and links to plain text. Decodes 75+ named HTML entities (including Scandinavian æøå) plus numeric/hex references. Available as a standalone function (`from mcp_methods import html_to_text`) and as a string-based transform on `read_file` (`transform="html"`).
- **String-based `transform` on `read_file`** — `transform` now accepts `"html"` in addition to callables. The built-in html transform runs *after* section extraction (so `id` attributes remain available) but *before* grep (so patterns match clean text, not raw tags). Callable transforms preserve existing behaviour.

## 0.3.17

- **Neutral error for non-existent items** — fetching a number that doesn't exist as an Issue, PR, or Discussion now returns `#N not found in repo (checked Issues, PRs, and Discussions)` instead of leaking the GraphQL fallback error (`"Could not resolve to a Discussion"`).

## 0.3.16

- **Renamed `github_discussions` → `github_issues`** — one tool, three modes: FETCH (by number), SEARCH (by query), LIST (default). `github_discussions` remains as a backward-compat alias.
- **Search mode** — `query="datatree coordinates"` searches via REST `search/issues` for issues+PRs, and GraphQL `search(type: DISCUSSION)` for Discussions. `kind` routes to the right API; `kind="all"` runs both and concatenates results. Sort defaults to relevance for search, `"created"` for listing.
- **Renamed `ElementCache.fetch_discussion` → `fetch_issue`**.
- **`labels` parameter simplified** — now a comma-separated string (`"bug,P0"`) instead of `Vec<String>`.
- **`sort` parameter now optional** — defaults depend on mode: `None` (relevance) for search, `"created"` for listing.

## 0.3.15

- **GitHub Discussions support (GraphQL)** — `fetch_discussion` and `github_discussions` now transparently handle GitHub Discussions, not just Issues and PRs. When a REST lookup returns 404, the library falls back to the GraphQL API to fetch the Discussion with full threaded comments (top-level + nested replies), category, and answered status. Listing mode supports `kind="discussion"` to query Discussions via GraphQL with state and sort filtering. Ref collection also extracts GitHub references from threaded reply bodies.

## 0.3.14

- **`max_matches` parameter for `read_file` grep** — limit the number of matches returned when grepping within a file. When a dense document has 125 hits, `max_matches=20` returns the first 20 with context, and the header changes to "showing 20 of 125 matches". The ripgrep module already had `max_results`/`match_limit`; this brings the same control to single-file grep. Also improved truncation footers: when `max_chars` cuts grep output, the footer now includes the total match count (e.g., "125 matches, 279865 chars total") so you know whether to refine the pattern or increase the limit.

## 0.3.13

- **`grep` parameter for `read_file`** — search within a single file and return only matching lines with context. Avoids reading entire large documents into context when only specific passages are needed. Supports `grep_context` (default 2) for surrounding lines, merges overlapping context windows, works with `section`, `start_line`/`end_line`, `transform`, and `max_chars`. Uses `--` separators between non-contiguous match groups, consistent with ripgrep output.

## 0.3.12

- **UTF-8 fix for `section` extraction** — the byte-stepping loop panicked on multi-byte characters (§, æ, ø, å, é, etc.) inside extracted sections. Now advances by full UTF-8 codepoints. Also hardened all `max_chars` truncation sites to avoid splitting multi-byte characters.

## 0.3.11

- **`section` parameter for `read_file`** — extract an HTML element by its `id` attribute. Solves single-line HTML navigation: instead of getting 127KB on line 96, request `section="PARAGRAF_4-7"` to get just that element. Infers tag name from the matched opening tag, handles nested elements of the same type, works with `transform` and `max_chars`.

## 0.3.10

- **Comment TOC** — bare `element_id="comments_middle"` returns a lightweight table of contents (`_index`, `author`, `created_at`, 80-char snippet) instead of dumping raw JSON.

## 0.3.9

- **`lines` on array elements** — `lines="1-20"` on `comments_middle` returns structured comment objects by index range.
- **`_index` on highlights** — maintainer highlights include `_index` for drill-down.
- **Grep metadata on comments** — grep matches include `author`, `created_at`, `comment_index`, and `element_id`.

## 0.3.7

- **Thread digest** — discussions with 50+ comments are condensed: first 5 + maintainer highlights + last 5 inline, middle cached as `comment_N` and searchable `comments_middle`.
- **Bookend pagination** — fetches first 5 + last 5 comment pages, skipping the middle. Prevents freezing on huge threads.
- **Lazy related refs** — cross-references listed as `related_refs` instead of eagerly fetched.
- **Unicode safety** — fixed byte-slicing panics on multi-byte UTF-8 characters.

## 0.3.6

### Improvements

- **Budget-based adaptive compaction** — `fetch_discussion` / `compact_discussion` now start with full content and only compact what's needed to fit within a byte budget (default 60KB). Small/medium PRs return fully expanded with no user effort. Large PRs gracefully degrade through 9 progressive tiers (bot filter → code blocks → comments → large patches → body → reviews → all patches → aggressive).
- **Removed `expand` parameter** — no longer needed. Compaction is fully automatic based on size constraints. Compacted content is always available via `element_id` drill-down.
- **Per-item budget** (default 15KB) prevents any single patch, comment, or body from consuming more than 25% of the total budget, ensuring balanced output even when one file dominates a PR.
- **`budget` / `item_budget` parameters** on `compact_discussion` for power users who want to tune output size.
- **`_compaction` metadata** in output describes what was compacted and at which tier, so callers know what to drill into.

## 0.3.5

### Improvements

- **Smart diff sizing** — small PR diffs (≤200 total lines) are shown inline with per-file collapsing for large individual patches. Large diffs show a navigation tree with `+/-` counts; drill into specific files via `element_id="patch_N"`. All patches are always cached regardless of size.
- **Review comments as `review_N` cache elements** — inline review comments are now individually drillable via `element_id="review_3"` with `grep` and `lines` support. Previews extended from 1 line/120 chars to 3 lines/300 chars.

## 0.3.4

### Fixes

- **`list_dir` annotate callback paths** — the callback now receives paths relative to `relative_to` (e.g. `src/core/engine.py`) instead of bare filenames (`engine.py`). This was a bug that made it impossible to match callback paths against external data sources (knowledge graphs, databases) when listing subdirectories.

## 0.3.3

### Improvements

- **PR diffs as collapsed cache elements** — when fetching a PR via `ElementCache.fetch_discussion`, diff patches are automatically collapsed into `patch_N` cache elements. Each stores filename, additions/deletions, and full diff text. Drill into specific files with `element_id="patch_3"`, search within patches with `grep="pattern"`, or slice with `lines="10-30"`. Fits the existing progressive-disclosure pattern alongside `cb_N`, `comment_N`, and `details_N`.
- **`refresh` flag on `fetch_discussion`** — subsequent calls for the same `(repo, number)` now return a cached summary instead of re-fetching. Pass `refresh=True` to force a fresh fetch when the discussion has changed.
- **Removed `git_diff`** — PR diffs are better served as collapsed elements in `fetch_discussion` (progressive disclosure). For comparing tags/branches outside a PR context, use `git_api("compare/v1.0...v2.0")`.
- Removed `globset` direct dependency (was only used by `git_diff`)

## 0.3.2

### Improvements

- **`git_diff` GitHub API fallback** — when local `git diff` fails (shallow clones, missing refs), automatically falls back to the GitHub compare API (`/repos/{repo}/compare/{base}...{head}`). Repo is auto-detected from git remote or can be passed explicitly via the new `repo` parameter.

## 0.3.1

### Fixes

- Fixed CI import check referencing removed `git_issue` export
- Fixed stale `git_issue` reference in `ElementCache` error message

## 0.3.0

### Breaking changes

- **Renamed `head_limit` → `max_results`** across the API. In `ripgrep()`, the parameter is now `max_results`. In `ripgrep_files()`, the old `max_results` (engine-level early termination) is now `match_limit`, and `head_limit` (output truncation) is now `max_results`.
- **Renamed `git_issue` → `github_discussions`**. Now supports both fetching a single discussion and listing discussions with filters.
- **Renamed `ElementCache.fetch_issue` → `ElementCache.fetch_discussion`**.

### New features

- **`github_discussions` listing mode** — list issues, PRs, or both with `kind`, `state`, `sort`, `limit`, and `labels` filters. Auto-detects repo from git remote when `repo` is omitted.
- **`list_dir` annotate callback** — optional `annotate` parameter accepts a callable that receives each entry's relative path and returns an annotation string (e.g. `"(144 loc)"`). The tree formatter handles column alignment within each directory level.

## 0.2.11

### New features

- **`list_dir`** — tree-formatted directory listing with depth control, glob filtering, `.gitignore` support, directory summaries, and `relative_to` for path display.

### Performance optimizations

- Eliminated double filesystem walk in `list_dir` (merged into single walk at depth+1)
- Replaced HashSet+sort+dedup with O(n) sorted merge for context line merging in grep
- Thread-local buffering with `FlushGuard` Drop pattern in parallel walker (one lock per thread instead of per file)
- Thread-local single-entry regex cache in `json_grep` and `cache` modules
- Pre-canonicalize `allowed_dirs` in `read_file` to reduce filesystem stat calls
- `eq_ignore_ascii_case()` replacing `.to_lowercase()` allocation in HTML tag detection
- `LazyLock<HashSet<&'static str>>` for zero-allocation default skip directories

## 0.2.10

### Bug fixes

- Fixed `head_limit` semantics: changed from `int` (0 = unlimited) to `int | None` (None = unlimited)
- Added `relative_to` parameter to `ripgrep()` wrapper

## 0.2.9

### Bug fixes

- Removed default `max_results` limit that silently truncated search results

## 0.2.8

### Changes

- Renamed all `grep` methods to `ripgrep` across the codebase
- Dropped deprecated `macos-13` CI runners from wheel build workflow
- Removed tracked `__pycache__` files from git

## 0.2.7

### Changes

- Fixed wheel build workflow for cross-platform distribution

## 0.2.5

### Improvements

- Searcher reuse in transform (callback) path — build searcher and sink once, reuse across files
- Fast path for no-context matches in `format_content` (skip HashSet/sort/dedup)
- Context merge moved inside the context branch to avoid allocation when unused

## 0.2.3

### Major rewrite — Rust conversion

- Rewrote core from pure Python to Rust via PyO3/maturin
- **grep**: Uses `grep-regex`, `grep-searcher`, and `ignore` crates (parallel file walking, mmap, SIMD literal optimization, `.gitignore` support)
- **GitHub integration**: `git_issue`, `git_api`, `ElementCache` with drill-down caching, text compaction
- **File I/O**: `read_file` with path traversal protection
- **Compaction**: `compact_discussion`, `collapse_code_blocks`, `compact_text`
- Added `ripgrep()` Claude Code Grep-compatible wrapper
- Added CI (cargo fmt, clippy, pytest) and wheel build workflows

## 0.1.1

- Packaging fix

## 0.1.0

- Initial release — pure Python implementation
