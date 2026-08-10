---
name: release
description: Cut an mcp-methods release — goal-check the change, run the gate, bump the single workspace version, finalize the CHANGELOG, commit (version in the fix/feat subject), then push main to fire the crates.io + PyPI publish workflows (invoking `/release` is the authorization), verify both registries, then ping any downstream counterpart the cut affects.
---

# Release

Ship an mcp-methods release. A release is **a version bump merged to
`main`** — the workflows (`publish_crates.yml` → crates.io,
`build_wheels.yml` → PyPI) re-read the version, skip if it's already live
(`should_publish`), and otherwise publish after CI passes (`ci-gate`).
**Publishes are immutable** — a version, once on crates.io/PyPI, can never
be overwritten or unpublished. There is no branch guard on the workflows,
so the bump must land only on the ref you mean to release from.

## Preconditions
- **No release already staged.** `git log origin/main..HEAD --oneline`.
  If an unpushed commit already carries a version bump, **keep that
  version** — fold this work into the same `## x.y.z` CHANGELOG block and
  the same commit (one version bump per push).
- **On `main`** (the release ref). Working tree ideally clean — but if
  there's **unrelated uncommitted work** (e.g. in-flight `AGENTS.md` /
  docs edits), don't block on it and don't sweep it in: **stage every
  release file explicitly by path** (`git add Cargo.toml CHANGELOG.md
  <src…>`, never `git add -A`/`.`) and leave the rest for its author.
  Confirm with `git status --porcelain` that only release files are staged.

  **That confirmation is load-bearing, because `git add` is all-or-nothing
  across its pathspecs**: one bad path — a typo, a file you thought you
  changed — aborts the whole invocation and stages *nothing*, including
  `Cargo.toml`. The failure is quiet in the reassuring direction: the commit
  still succeeds, without the version bump, and here the bump IS the publish
  trigger — so the push goes green and ships nothing. Read back
  `git diff --cached --name-only` and check `Cargo.toml` is in it.
  (KGLite hit this on 2026-08-09.)

## Steps
0. **Doctrine sync — one file read when there is nothing to do.** Read
   `../doctrine/VERSION` and compare it against `dev-docs/.doctrine-synced`
   (this step owns that marker and creates it; **missing means never synced**,
   so read the changelog from its first entry).
   Equal: continue. Doctrine ahead: read `../doctrine/CHANGELOG.md` forward
   from the marker and act on every entry newer than it, per its class —
   `[skills-update]` merges into this repo's declared authority (`CLAUDE.md`,
   `.claude/skills/`) and regenerates the adapters from it, never the reverse;
   `[local-sweep]` runs the check command the entry states, and a red sweep is
   release work made visible, not a deferral; `[info]` needs nothing. Write the
   new version into the marker **only after** those actions completed — a
   marker written first permanently hides the entry it skipped, because the
   next sync compares against the marker and sees nothing.

   This repo has no planning skill, so the release flow is the one procedure
   that reliably runs before work ships; the step lives here for that reason.
1. **Goal check — did the change do what it set out to do?** Re-read the
   diff and any triggering inbox message / issue. Confirm the fix or
   feature is complete and the CHANGELOG entry explains the *why* (not
   just the what), matching the prose quality of recent entries. Surface
   anything dropped or deferred before bumping — don't let it vanish.
2. **Gate — all green before continuing.**
   - Rust (always runnable here): `cargo fmt -- --check`,
     `cargo clippy --workspace --all-targets -- -D warnings`,
     `cargo test -p mcp-methods`,
     `cargo test -p mcp-methods --test deployed_manifests`,
     `cargo test -p mcp-server`.
   - Python (`make lint`'s `ruff check .` + `pytest tests/`): part of the
     canonical gate, **but this sandbox can't run them** (ruff / pytest-cov
     / a built `mcp_methods` are absent). Run the Rust gate locally and let
     **CI** run the full gate (it does — `ci.yml` is the `ci-gate` the
     publish workflows wait on). Don't claim the Python gate passed locally
     when it was skipped.
   - **Where the environment CAN run those legs, run them before the first
     push — not after CI reports them.** Every step a branch has never executed
     accumulates failures independently until CI sees them all at once;
     KGLite's 2026-08-09 program found four CI blockers this way on a branch
     whose fast gate was green throughout. Here the stakes are sharper than a
     red PR: `ci-gate` failing after a `Cargo.toml` bump lands leaves the
     version **unpublished**, and the `workflow_dispatch` retry gap (CLAUDE.md,
     "Failed-CI retry gap") means fixing it with a `.rs`-only commit does not
     re-fire publish — both workflows must then be dispatched by hand.
   - **Review what the diff earned, against the bar** in CLAUDE.md "Review —
     report what is broken" (estate rule R15). A finding names a concrete
     failure; "no findings" is a valid outcome and does not hold the release.
3. **Bump the version — patch by default** (`0.3.Z` → `0.3.Z+1`). One
   line: `version` under `[workspace.package]` in the **root
   `Cargo.toml`**. All three crates inherit it via `version.workspace =
   true`; `pyproject.toml` is `dynamic = ["version"]` (maturin reads
   Cargo.toml), so there is **no** per-manifest or pyproject bump.
   `Cargo.lock` is gitignored — leave it.
   - **Minor/major** (`0.3` → `0.4`): if the change is a new feature,
     breaking change, or scope expansion, STOP and confirm the bump level
     first (see `CONTRIBUTING.md` "Release cadence"). Only on a minor/major
     do you also bump the `mcp-server` → `mcp-methods` dependency pin
     (`mcp-methods = { version = "0.3", … }` in
     `crates/mcp-server/Cargo.toml`) — patch bumps don't touch it.
     `mcp-methods-py` depends by path only; never needs a pin bump.
4. **Finalize the CHANGELOG top block.** Set the date, drop any
   `(proposed)` marker on the `## x.y.z` header. Entries are required.
5. **Commit — version rides the fix/feat subject** (our convention, not a
   standalone `release(x.y.z):`): e.g. `fix(workspace): … (0.3.47)`. The
   version bump + CHANGELOG finalization + the code change go in **one**
   commit. Use `--no-verify` — the ruff pre-commit hook can't run in this
   sandbox; CI is the real gate.
6. **Push — `/release` is the authorization.** Invoking the skill authorized
   this too: the bump, the CHANGELOG, the commit, the gate, and this push.
   No separate prompt.

   State the exact version and anything the run turned up that the owner did
   not know at invocation, then push. That is a report, not a gate — it was
   briefly a blocking confirmation and was reverted 2026-07-31, because
   blocking fires after the irreversible decision is already made and it
   stalls unattended runs.

   Be aware of the geometry, because it is sharper here than in kglite: the
   push IS the publish. The workflows trigger on the root `Cargo.toml` path
   with no branch guard, and nothing stands between the two. That is an
   argument for strong checks BEFORE this line — Rust gate green, surgical
   staging, on `main`, fast-forward clean — not for a prompt at it.

   Push: `git push origin main`.
7. **Poll CI until green.** Three workflows fire: **CI** (`ci.yml`),
   **Publish to crates.io** (`publish_crates.yml`), **Build & Publish
   Wheels** (`build_wheels.yml`). Poll the GitHub Checks API directly
   (`gh run list` / `gh run watch`). The wheel matrix (multi-OS) lags the
   crates.io publish.
   - CI fix-and-push loop: if a run fails on a shipped-code/infra bug (not
     a scope change), push `fix(…)` / `ci(…)` without re-asking until
     green. Stop after ~3 iterations or any release-shape change.
**Verify the artifact SET, not just the version.** A version check answers
"did something publish", never "did everything publish". Cross-compiled legs
are often `continue-on-error`, and an `upload-artifact` step without
`if-no-files-found: error` uploads an *empty* artifact from a green build — so
with `skip-existing: true` a partial set ships and nothing says so. Compare the
artifact count and platform tags against the previous release. Conversely, an
empty version read out of `Cargo.toml` (`grep … | cut` reports cut's status,
always 0) yields a green run that publishes *nothing* — a silent non-release.
Assert the extracted version is well-formed before it drives any publish
decision.

Two more shapes of the same trap, added from KGLite's 2026-08-09/10 program:
- **`grep -c` exits 1 when the count is zero.** So `... | grep -c <tag> && …`
  breaks the chain on exactly the artifact that is *missing* — the case this
  whole check exists to catch — and under `set -e` it kills the script mid-
  verification. A zero count is an answer, not an error: capture it
  (`n=$(… | grep -c … || true)`) and test the number.
- **Read a backgrounded run's outcome from its artifact, never from the
  wrapper.** An echoed exit status, a "done" line, or the absence of visible
  errors is not the result — open the log or output file the run wrote. This is
  how a failed background build gets reported as a passing one.

8. **Verify published** — poll both registries; 200 = live:
   - crates.io (needs an explicit User-Agent, else 403 that looks like a
     failed publish): `curl -fsSL -A "mcp-methods-release" \
     https://crates.io/api/v1/crates/mcp-methods | jq -r '.versions[].num' \
     | grep -qx <x.y.z>`. Only `mcp-methods` publishes — `mcp-server` and
     `mcp-methods-py` are `publish = false` (the binary ships in the wheel
     + via `cargo install`).
   - PyPI: `curl -fsSL https://pypi.org/pypi/mcp-methods/<x.y.z>/json` →
     200. The wheel matrix finishes after crates.io, so this may lag by
     minutes.
9. **Notify downstream — only counterparts the cut affects.** Per
   CLAUDE.md's mail interface, if a party is waiting on this version or the
   cut changes their deployment, drop a "it's live" message into **their**
   `inbox/unread/` (filename `YYYY-MM-DD-from-mcp-methods-<topic>.md`, with
   the frontmatter block). Common triggers: **kglite** asked to be pinged
   on publish, or a framework change touches **mcp-servers**' running
   deployment. No blanket announcement — only where there's a real
   dependency. Then archive any related incoming message to `inbox/read/`.
10. **Adapter resync — diff each adapter against its declared authority,
    rename-aware.** Identical: done. Divergent: classify each hunk before
    touching either side — an *improvement* is merged into the **authority**
    first and the adapter regenerated from it; *staleness* is simply
    regenerated away. Never run a blind sync on a divergent pair: blind sync
    deletes improvements (sonara, 2026-08-10, ~20 lines), and no sync preserves
    stale doctrine the other harness will follow. The mirror check must pass
    afterwards.

    Here that is `CLAUDE.md` → `AGENTS.md` and each `.claude/skills/<n>/SKILL.md`
    → `.agents/skills/<n>/SKILL.md`, substituting `CLAUDE.md`→`AGENTS.md`
    everywhere **except the authority-declaration line**, which names the
    authority literally in every copy — a substituted declaration inverts
    itself and tells the adapter's reader to edit the adapter.

## Notes
- **Version source of truth:** `[workspace.package] version` in the root
  `Cargo.toml`. The publish workflows extract it with
  `grep -m1 '^version' Cargo.toml`.
- **Idempotency:** each workflow's `should_publish` check skips a version
  already on the registry, and `cargo publish` treats "already exists" as
  a no-op — so a re-run after a partial publish is safe.
- Keep responses tight; write long diffs/logs to the scratchpad and report
  the path rather than pasting them into the conversation.
