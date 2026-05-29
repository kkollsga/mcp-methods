# mcp-methods

## Releasing

GitHub Actions does the heavy lifting. A release is just **a version
bump merged to `main`** — the workflows build and publish to both
crates.io and PyPI automatically.

The trigger: any push to `main` that touches
`crates/mcp-methods/Cargo.toml`. `publish_crates.yml` and
`build_wheels.yml` each re-read the version, skip if it's already live
on the registry (`should_publish` check), and otherwise publish after
CI passes (`ci-gate`). Publishes are **immutable** — a version, once on
crates.io/PyPI, can never be overwritten or unpublished. There is no
branch guard on the workflows, so the bump must only land on the ref
you intend to release from.

### Steps

1. **Bump the version in all four places** (they must match — the
   version is the single source of truth in
   `crates/mcp-methods/Cargo.toml`):
   - `crates/mcp-methods/Cargo.toml` → `version`
   - `crates/mcp-methods-py/Cargo.toml` → `version`
   - `crates/mcp-server/Cargo.toml` → `version` **and** its
     `mcp-methods = { version = "..." }` dependency pin
   - (`Cargo.lock` is gitignored — no need to touch it)
2. **Finalize the CHANGELOG** top block: set the date, drop any
   `(proposed)` marker. Entries are required; match the prose quality
   of recent entries (explain the *why*).
3. **Gate locally**: `make lint` + the full test suite (`cargo test -p
   mcp-methods`, `cargo test -p mcp-methods --test deployed_manifests`,
   `cargo test -p mcp-server`, `pytest tests/`). All must pass.
4. **Land on `main`** — merge the PR (or push `main`). The push fires
   the publish workflows. Patch bumps (`0.3.X`) ship liberally for
   fixes and additive changes; see CONTRIBUTING's release-cadence
   table for when to go minor/major.
5. **Confirm it landed** before announcing — poll the registries:
   `curl -fsSL -A "ua" https://crates.io/api/v1/crates/mcp-methods` and
   `https://pypi.org/pypi/mcp-methods/<version>/json` (200 = live). The
   wheel matrix (multi-OS) takes longer than the crates.io publish.

Commit convention for a release: the version bump rides in the
fix/feat commit with the version in the subject, e.g.
`fix(github): … (0.3.39)` — see git history.

## Mail interface

Three projects coordinate by dropping markdown files into each other's
`inbox/` folders. We (mcp-methods) correspond with two counterparts:

| Party | Their inbox (where we drop messages TO them) |
|---|---|
| **kglite** | `/Volumes/EksternalHome/Koding/Rust/KGLite/inbox/unread/` |
| **mcp-servers** | `/Volumes/EksternalHome/Koding/MCP servers/inbox/unread/` (note the space in the path) |

Our own inbox (messages FROM them land here):
`/Volumes/EksternalHome/Koding/Rust/mcp-methods/inbox/unread/`

### Workflow

- **Reading**: messages start in `inbox/unread/`. After reading, move
  them to `inbox/read/`. Don't delete — the archive is the record.
- **Writing**: drop the file directly into the recipient's
  `inbox/unread/` folder. They'll move it to their `read/` when they
  process it. We don't keep an outbox; the file is the only copy.

### Naming convention

`YYYY-MM-DD-from-<sender>-<short-topic>.md` — kebab-case topic,
sender slug matches the party name (`mcp-methods`, `kglite`,
`mcp-servers`).

### Frontmatter

Every message opens with a YAML block:

```markdown
---
date: 2026-05-18
from: mcp-methods
re: <recipient>/inbox/read/<prior-message>.md   # or "" if new thread
status: <one-paragraph TL;DR — what this message does, what it asks
        for, what action (if any) the recipient should take>
---
```

The `status:` block is the agent-readable summary; downstream agents
sort and prioritise inbox work from it without opening every file.

### Three-party relationship

- **mcp-methods** (here): the Rust framework. Ships `mcp-methods` on
  crates.io (pure library) and `mcp-methods` on PyPI (pyo3 bindings +
  bundled `mcp-server` CLI).
- **kglite**: downstream consumer that builds on top of the framework.
  Treats us as the vendor. Paying-customer relationship — we don't
  volunteer maintenance-burden concessions; they raise needs, we
  decide on shape.
- **mcp-servers**: operator running kglite deployments
  (legal/o&g/code). One hop further downstream — most coordination
  with them goes via kglite, but we ship release notes directly when a
  framework cut affects their deployment.
