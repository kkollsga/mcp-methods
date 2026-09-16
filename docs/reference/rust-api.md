# Rust API Reference

The Rust crate `mcp-methods` is published on crates.io. Full rustdoc is auto-built by docs.rs:

➡️ **[docs.rs/mcp-methods](https://docs.rs/mcp-methods)**

## Quick index

The most-imported types and functions, grouped:

### Manifest

- [`mcp_methods::server::manifest::load`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/fn.load.html) — load + validate a YAML manifest
- [`mcp_methods::server::Manifest`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/struct.Manifest.html) — the parsed manifest struct
- [`mcp_methods::server::Manifest::to_json`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/struct.Manifest.html#method.to_json) — JSON view for FFI/RPC bridging
- [`mcp_methods::server::manifest::find_workspace_manifest`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/fn.find_workspace_manifest.html) / [`find_sibling_manifest`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/fn.find_sibling_manifest.html)
- [`mcp_methods::server::TrustConfig`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/struct.TrustConfig.html)
- [`mcp_methods::server::WorkspaceConfig`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/struct.WorkspaceConfig.html)
- [`mcp_methods::server::BuiltinsConfig`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/struct.BuiltinsConfig.html)
- [`mcp_methods::server::ToolSpec`](https://docs.rs/mcp-methods/latest/mcp_methods/server/manifest/enum.ToolSpec.html)

### Server framework

- [`mcp_methods::server::McpServer`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html) — the framework's MCP server
- [`mcp_methods::server::McpServer::new`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.new)
- [`mcp_methods::server::McpServer::register_typed_tool`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.register_typed_tool) — register a custom tool (infallible handler)
- [`mcp_methods::server::McpServer::register_typed_tool_fallible`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.register_typed_tool_fallible) — register a custom tool whose `Err` arm sets `isError: true` on the MCP result
- [`mcp_methods::server::McpServer::serve`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.serve) — start the server (via rmcp transport)
- [`mcp_methods::server::ServerOptions`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.ServerOptions.html) — config used to construct the server
- [`mcp_methods::server::ServerOptions::from_manifest`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.ServerOptions.html#method.from_manifest)

### Workspace

- [`mcp_methods::server::workspace::Workspace`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/struct.Workspace.html)
- [`Workspace::open`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/struct.Workspace.html#method.open) (GitHub mode) / [`Workspace::open_local`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/struct.Workspace.html#method.open_local) (local mode)
- [`PostActivateHook`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/type.PostActivateHook.html) — callback signature

### Skills

- [`mcp_methods::server::SkillRegistry`](https://docs.rs/mcp-methods/latest/mcp_methods/server/skills/struct.Registry.html) — builder for the layered resolved set
- [`mcp_methods::server::ResolvedRegistry`](https://docs.rs/mcp-methods/latest/mcp_methods/server/skills/struct.ResolvedRegistry.html) — post-resolution skill set
- [`mcp_methods::server::serve_prompts`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.serve_prompts.html) — wire a resolved registry into `prompts/list` / `prompts/get`, register the `skill(name)` tool, and inject each skill into the tools it targets. Returns `Vec<ActiveSkill>`.
- [`mcp_methods::server::ActiveSkill`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.ActiveSkill.html) — one skill that survived both activation gates: `name`, `description`, `delivery`, `provenance`
- [`mcp_methods::server::McpServer::active_skills`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.active_skills) — the same list, readable from a tool handler at request time. Returns an owned `Vec<ActiveSkill>`.
- [`mcp_methods::server::McpServer::reinject_skills`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.reinject_skills) — re-resolve a registry **after** the server is serving: strip the previous injection, replace the skill prompt routes, re-register the loader, inject again. Takes `&self`, returns `Result<Vec<ActiveSkill>, String>`. See [Rebuilding skills at runtime](../guides/authoring-skills.md#rebuilding-skills-at-runtime).
- [`mcp_methods::server::McpServer::skill_reloader`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.skill_reloader) / [`SkillReloader`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.SkillReloader.html) — a cloneable handle a dynamic tool handler captures before `serve`, so a `Fn(T) -> Result<String, String>` closure with no `&self` can drive the rebuild
- [`mcp_methods::server::notify_skills_changed`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.notify_skills_changed.html) — `notifications/tools/list_changed` then `notifications/prompts/list_changed` on a `Peer`. The framework stores no peers; the caller owns the notification.
- [`mcp_methods::server::Delivery`](https://docs.rs/mcp-methods/latest/mcp_methods/server/skills/enum.Delivery.html) — `Eager` | `Lazy`, the frontmatter `delivery:` key. **Absent means `Lazy`.**
- [`mcp_methods::server::SKILL_TOOL_NAME`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/constant.SKILL_TOOL_NAME.html) / [`SkillArgs`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.SkillArgs.html) — the `skill(name)` loader tool registered by `serve_prompts`: one required string argument `name`, returns that skill's body, `isError: true` naming the active set for an unknown or inactive name. The framework-owned loader is **exempt from the default response budget** — a body is capped at `HARD_SIZE_LIMIT_BYTES` (16 KB) when it loads, so the exemption is bounded and the body is never returned as a preview excerpt. A downstream tool that took the `skill` name is not the framework loader and stays budgeted.
- [`ResolvedRegistry::total_body_bytes`](https://docs.rs/mcp-methods/latest/mcp_methods/server/skills/struct.ResolvedRegistry.html#method.total_body_bytes) — the sum checked against `SESSION_TOTAL_LIMIT_BYTES`, counted over both delivery tiers
- [`mcp_methods::server::library_bundled_skills`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.library_bundled_skills.html) — framework defaults Vec
- [`mcp_methods::server::render_skill_template`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.render_skill_template.html) / [`write_skill_template`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.write_skill_template.html) — scaffold a starter SKILL.md
- [`mcp_methods::server::cli::skills_lint`](https://docs.rs/mcp-methods/latest/mcp_methods/server/cli/fn.skills_lint.html) / [`skills_list`](https://docs.rs/mcp-methods/latest/mcp_methods/server/cli/fn.skills_list.html) / [`skills_show`](https://docs.rs/mcp-methods/latest/mcp_methods/server/cli/fn.skills_show.html) / [`skills_new`](https://docs.rs/mcp-methods/latest/mcp_methods/server/cli/fn.skills_new.html) — composable CLI helpers

### Watch + env

- [`mcp_methods::server::maybe_watch`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.maybe_watch.html) — spawn the filesystem watcher
- [`mcp_methods::server::load_env_for_mode`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.load_env_for_mode.html) — `.env` resolution
- [`mcp_methods::server::init_tracing`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.init_tracing.html)
- [`mcp_methods::server::resolve_source_roots`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.resolve_source_roots.html)

### Primitives (always available, no `server` feature needed)

- [`mcp_methods::cache::ElementCache`](https://docs.rs/mcp-methods/latest/mcp_methods/cache/struct.ElementCache.html)
- [`mcp_methods::compact`](https://docs.rs/mcp-methods/latest/mcp_methods/compact/index.html)
- [`mcp_methods::files`](https://docs.rs/mcp-methods/latest/mcp_methods/files/index.html)
- [`mcp_methods::git_refs`](https://docs.rs/mcp-methods/latest/mcp_methods/git_refs/index.html)
- [`mcp_methods::github`](https://docs.rs/mcp-methods/latest/mcp_methods/github/index.html)
- [`mcp_methods::grep`](https://docs.rs/mcp-methods/latest/mcp_methods/grep/index.html)
- [`mcp_methods::html`](https://docs.rs/mcp-methods/latest/mcp_methods/html/index.html)
- [`mcp_methods::json_grep`](https://docs.rs/mcp-methods/latest/mcp_methods/json_grep/index.html)
- [`mcp_methods::list_dir`](https://docs.rs/mcp-methods/latest/mcp_methods/list_dir/index.html)

## Features

| Feature | Default | What it enables |
|---|---|---|
| `server` | ✅ on | The full framework (`mcp_methods::server::*`): rmcp + tokio + clap + manifest + tool routing |

Disable with `default-features = false` for the bare primitives:

```toml
mcp-methods = { version = "0.4", default-features = false }
```

## See also

- [Downstream Binary](../guides/downstream-binary.md) — how to wrap `McpServer::new`
- [Architecture](../explanation/architecture.md) — three-crate layout
- The published [crates.io page](https://crates.io/crates/mcp-methods)

## Default tool response budgets

`McpServer` applies the [response budget contract](../guides/using-fastmcp-helpers.md#default-response-budgets)
to completed calls from builtin, typed and custom router tools. The reference
`mcp-server` binary inherits it. No server option is needed to enable it.
`tools/list` advertises `_response` controls and the retained-result expansion
tool, including collision-safe names — on every tool except the framework's own
`skill(name)` loader, which is exempt from the budget and so advertises no
controls. Direct calls to a handler bypass protocol
presentation; so does dispatching through the router yourself
(`tool_router_mut()` derefs to the rmcp `ToolRouter`, whose `call(...)` is the
raw route, not the presented result).

Use `McpServer::with_response_preview_hook` to provide domain-specific summary,
coverage and next-query JSON from the tool name, original arguments and complete
MCP result. The callback runs outside the retention lock; its guidance is
included in previews and retained with the result. Custom routes may instead
supply `_meta["mcp_methods/preview"]` themselves. The optional guidance hook does
not enable the budget: the budget is already the default.

The always-available `mcp_methods::response_budget` module exposes the shared
`ResponseStore`, `ResponseOptions` and `Expansion` primitives for adapters.
Validate controls before executing a tool, provide a unique authenticated
session owner, and serialize the complete result before applying the budget.
Native sessions use negotiated peer identity, never the client display name.
