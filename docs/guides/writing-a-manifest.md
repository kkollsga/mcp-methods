# Writing a Manifest

The YAML manifest is the source of truth for an `mcp-methods`-based server. The generic `mcp-server` CLI (bundled in `pip install mcp-methods`) and every downstream binary (`kglite-mcp-server`, your own builds) read the same schema.

## Minimal manifest

The smallest useful manifest:

```yaml
name: My Source Navigator
source_roots:
  - ./src
```

This binds the source tools (`read_source`, `grep`, `list_source`) to `./src` and serves them over stdio. Run with:

```bash
mcp-server --mcp-config minimal.yaml
```

## Full schema

```yaml
# Identity — surfaced via MCP `initialize`.
name: My MCP Server              # optional; falls back to "MCP Server (<mode>)"
instructions: |                  # optional; sent to the agent on initialize
  Multi-line guidance for the agent. Use this to tell the agent
  what kinds of questions it should answer with this server's tools.
overview_prefix: "OPS team"      # optional; prepended to graph_overview output
                                 # (only meaningful in downstream binaries that
                                 # implement a graph_overview tool)

# Source binding — either a single root or a list.
source_root: ./src               # alias for source_roots: [./src]
source_roots:                    # list of paths; relative to the YAML's parent dir
  - ./src
  - ./lib

# Trust gates — advisory metadata. The framework records them; consumers enforce.
trust:
  allow_python_tools: false           # tools[].python: factories
  allow_embedder: false               # extensions.embedder loaders

# Builtins — framework-level behaviour switches.
builtins:
  save_graph: false                   # whether the save_graph tool is registered
                                      # (only by downstream binaries; framework no-op)
  temp_cleanup: never                 # "never" | "on_overview" — when to clear temp/
  github: false                       # opt in to github_issues / github_api /
                                      # screen_stargazers. Default OFF — a reachable
                                      # GITHUB_TOKEN alone never registers them.
  screen_stargazers: true             # only meaningful when github: true

# Manifest-declared tools. The generic mcp-server CLI registers `python:` tools
# if trust permits, but does NOT execute `cypher:` tools (no graph backend).
# Downstream binaries (kglite-mcp-server) dispatch cypher tools.
tools:
  - name: list_active_users
    cypher: "MATCH (u:User {active: true}) RETURN u.name, u.email"
    description: "List users who are currently active"
    parameters:                       # JSON Schema for the tool's arguments
      type: object
      properties: {}

  - name: rewrite_query
    python: ./hooks.py                # path to a Python module (.py file)
    function: rewrite                 # callable name in that module
    description: "Normalise an incoming query before dispatch"
    parameters:
      type: object
      properties:
        query:
          type: string

# Embedder configuration — read by downstream binaries that load an embedder
# under trust.allow_embedder. The framework parses but doesn't instantiate.
embedder:
  module: ./embedder.py               # path to a Python module
  class: SentenceTransformerEmbedder  # class name in that module
  kwargs:                             # JSON-compatible keyword args
    model_name: "BAAI/bge-m3"
    cache_dir: ./.cache/embedder

# Environment file — `.env`-style key=value pairs, auto-loaded at boot.
env_file: .env                        # optional; otherwise walks up from yaml dir

# Workspace mode declaration — wins over CLI --workspace.
workspace:
  kind: local                          # "github" | "local"
  root: ./repo                         # local mode only: the dir to bind
  watch: true                          # local mode only: enable filesystem watcher
  sandbox_root: ./                     # local mode only, optional: outer bound
                                       # for set_root_dir swaps (default: none)
  adopt_client_roots: false            # local mode only, optional: adopt the
                                       # MCP client's advertised root when no
                                       # root is configured (default: false)

# Opaque passthrough — downstream-binary-specific config. The framework
# validates only the top-level "extensions:" key and stores whatever is
# under it verbatim. Use this for kglite-specific or your-binary-specific
# blocks that aren't part of the framework's schema.
extensions:
  cypher_preprocessor:
    module: ./preprocessor.py
    class: WikidataPreprocessor
    kwargs:
      log_rewrites: false
  csv_http_server:
    enabled: true
    host: 127.0.0.1
    port: 8765
```

## Field-by-field reference

### `name`, `instructions`, `overview_prefix`

`name` is surfaced to the agent during MCP `initialize`. `instructions` is sent in the same response and shapes how the agent uses the server's tools. `overview_prefix` is consumed only by downstream binaries that implement a `graph_overview` tool — the framework parses and stores it but does nothing with it.

### `source_root` vs `source_roots`

Choose one — specifying both is a validation error. The single-form `source_root: PATH` is an alias for `source_roots: [PATH]`.

Paths are resolved relative to the manifest's parent directory. `~` is NOT expanded (use absolute paths or paths relative to the YAML location).

An entry that doesn't resolve to an existing directory does **not** stop the server booting: the reference binary warns per failed entry, serves the roots that did resolve, and lists the rest as `unresolved source roots: [...]` in its boot summary. With no root left, the source tools stay registered and say so when called. The same applies to a missing `env_file:` and to a watcher that won't start — see [Operating Modes](operating-modes.md#what-is-fatal-at-boot-and-what-degrades). Callers that want the strict, all-or-nothing check (linters, pre-flight validation) use `resolve_source_roots`; the boot path uses `resolve_source_roots_lenient`.

### `trust:`

Each gate defaults to `false`. The framework parses the block and surfaces it via:

- Rust: `manifest.trust.allow_python_tools`, etc.
- JSON (`Manifest::to_json()`): `manifest["trust"]["allow_python_tools"]`, etc.
- pyo3 wrapper: `manifest.as_dict()["trust"]["allow_python_tools"]`

**The framework does NOT refuse to boot when a manifest declares a hook that its corresponding trust flag is false.** Enforcement is the consumer's job. See [Trust Gates](trust-gates.md).

### `builtins:`

`save_graph` controls whether downstream binaries that ship a `save_graph` tool should register it. The framework doesn't ship `save_graph` itself.

`temp_cleanup` accepts `"never"` (default) or `"on_overview"`. `on_overview` is consumed by downstream binaries to clear a `temp/` directory whenever the `graph_overview` tool runs.

`github` (default `false`) opts the deployment into the GitHub tools — `github_issues`, `github_api`, and `screen_stargazers`. **A reachable token is not an opt-in.** Before this key existed, registration keyed off token reachability alone, so a `GITHUB_TOKEN` in the environment — or one the `env_file:` walk-up picked up from a `.env` several directories above the server's root — silently added three authenticated GitHub tools to servers that had nothing to do with GitHub. Now the manifest declares intent and the token only decides whether the opted-in tools can actually work: with `github: true` and no reachable token the tools still stay out of `tools/list` (an agent should not see a tool that is guaranteed to fail). Both decisions are made at boot — restart the server after changing either.

`screen_stargazers` (default `true`) is subordinate to `github`: with `github: false` it registers nothing whatever its value. Set it to `false` inside an opted-in deployment to keep `github_issues` / `github_api` but drop the stargazer screener.

**Token requirement — `repo=` seeding needs a wide token; `users=` seeding needs none.** Seeding a screen with `repo=` calls `GET /repos/{owner}/{repo}/stargazers`, and GitHub gates that endpoint behind **Contents: Read and write** for fine-grained PATs — reading a *public* star list requires a push-capable credential. A classic token with the `repo` scope also works. On a fine-grained PAT without that grant the call 403s (`x-accepted-github-permissions: metadata=read; contents=write`), and the tool now says so, including the header GitHub sent.

Widening the token is the wrong response: it turns a read-oriented review credential into one that can push to every selected repo. Seed with `users=` instead — pass the logins directly (e.g. from `gh api repos/OWNER/REPO/stargazers --paginate --jq '.[].login'`), which needs no repo permission at all. The only thing lost is `repo=`'s auto-derivation of keywords/stack from the seed repo; pass `keywords=` / `stack=` explicitly to replace it. Drill-downs (`user:<login>`, `cohort:<key>`) behave identically either way.

```yaml
builtins:
  github: true              # register github_issues / github_api / screen_stargazers
  screen_stargazers: false  # …but not the stargazer screener
```

### `tools:`

Each entry has a `kind` (`cypher` or `python`) discriminator inferred from which key is present (`cypher:` makes it a Cypher tool; `python:` + `function:` makes it a Python factory).

| Tool kind | What the generic `mcp-server` does | What kglite-mcp-server does |
|---|---|---|
| `cypher:` | Parses the tool, warns at boot ("no graph backend"), proceeds without dispatching. | Registers a tool that runs the Cypher against the active graph and returns results. |
| `python:` + `function:` | Parses the tool, warns at boot (Python hook execution removed in 0.3.26), proceeds without registering. | Layers a pyo3 hook factory that imports `module:`, calls `function:`, and registers the result as a tool. |

If you need Cypher dispatch or Python factories, **use a downstream binary, not the generic CLI**. The framework parses both kinds for forward-compat.

### `embedder:`

Same shape as `python:` tools — `module:` (a `.py` path), `class:` (an importable name), `kwargs:` (JSON-compatible). The framework parses but doesn't load. Downstream binaries enforce `trust.allow_embedder` before loading.

### `env_file:`

Pointer to a `.env`-style file. If unset, the framework walks upward from the manifest's parent directory looking for `.env`. Loaded values are inserted into the process environment with no-overwrite semantics (existing env vars take precedence).

### `workspace:`

When set, this wins over the CLI `--workspace` flag.

- `kind: github` — declares that the workspace, once bound, is the clone-and-track flow. **It does not create one.** Unlike `kind: local`, the reference `mcp-server` binary does not turn this block into a workspace: the clone directory comes from `--workspace DIR` and nothing else, so a manifest declaring `kind: github` booted without that flag binds no workspace at all — `repo_management` is not registered, and the block's only effect is the validation below (the binary warns at boot when it finds this combination). What the key drives when a workspace *is* bound: `repo_management` stays registered (it is dropped for `kind: local`, which gets `set_root_dir` instead), and the bundled `repo_management` skill's `applies_when: tool_registered:` gate follows that registration. `root:` is accepted and ignored (the active source root is the clone, not a path you pick). `watch:`, `sandbox_root:` and `adopt_client_roots:` are **rejected at boot**, not ignored — each is a `local`-only key and setting it under `kind: github` is a manifest error.
- `kind: local` — bind a fixed local directory. `root:` is required (path to bind) unless `adopt_client_roots: true` is set. `watch: true` enables the filesystem watcher (calls the post-activate hook on changes) and requires `root:`. `sandbox_root:` (optional) bounds the runtime `set_root_dir` swap to a subtree — see [Watch & Workspace](watch-and-workspace.md#bounding-the-swap-sandbox_root). Omitted, swaps stay unbounded, which is the default. `adopt_client_roots: true` (optional) lets the server take its root from the MCP client when the operator configured none — fallback-only, and built on an [upstream-deprecated](watch-and-workspace.md#adopting-the-clients-root-adopt_client_roots) MCP feature.

### `extensions:`

Anything under `extensions:` is opaque to the framework — validated only at the top level. Downstream binaries (kglite, your own) parse this block according to their own schema. Use it for domain-specific config that isn't part of the framework's surface.

## Validation behaviour

The framework rejects:

- Unknown top-level keys (`extensions:` is special — see above)
- Unknown keys under `trust:`, `builtins:`, `workspace:`, `embedder:`, `tools[]` entries
- Wrong types (e.g. `trust.allow_python_tools: "yes"` instead of `true`)
- Both `source_root` and `source_roots` set
- Non-existent paths under `workspace.root:` (local mode)
- `workspace.watch:`, `workspace.sandbox_root:` or `workspace.adopt_client_roots:` under `workspace.kind: github` — all three are local-only
- `workspace.watch: true` with no `workspace.root:` (an adoption-only workspace has nothing to watch at boot)

Errors include the YAML's file path and a description: `manifest.yaml: trust.allow_python_tools must be a bool`.

## See also

- [Operating Modes](operating-modes.md) — what each mode does at boot
- [Trust Gates](trust-gates.md) — the security audit pattern
- [Examples](../examples/minimal-manifest.md) — worked manifests
- [Manifest Schema (reference)](../reference/manifest-schema.md) — generated reference
