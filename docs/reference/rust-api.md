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
- [`mcp_methods::server::McpServer::register_typed_tool`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.register_typed_tool) — register a custom tool
- [`mcp_methods::server::McpServer::serve`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.McpServer.html#method.serve) — start the server (via rmcp transport)
- [`mcp_methods::server::ServerOptions`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.ServerOptions.html) — config used to construct the server
- [`mcp_methods::server::ServerOptions::from_manifest`](https://docs.rs/mcp-methods/latest/mcp_methods/server/server/struct.ServerOptions.html#method.from_manifest)

### Workspace

- [`mcp_methods::server::workspace::Workspace`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/struct.Workspace.html)
- [`Workspace::open`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/struct.Workspace.html#method.open) (GitHub mode) / [`Workspace::open_local`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/struct.Workspace.html#method.open_local) (local mode)
- [`PostActivateHook`](https://docs.rs/mcp-methods/latest/mcp_methods/server/workspace/type.PostActivateHook.html) — callback signature

### Skills

- [`mcp_methods::server::SkillRegistry`](https://docs.rs/mcp-methods/latest/mcp_methods/server/skills/struct.Registry.html) — builder for the three-layer resolved set
- [`mcp_methods::server::ResolvedRegistry`](https://docs.rs/mcp-methods/latest/mcp_methods/server/skills/struct.ResolvedRegistry.html) — post-resolution skill set
- [`mcp_methods::server::serve_prompts`](https://docs.rs/mcp-methods/latest/mcp_methods/server/fn.serve_prompts.html) — wire a resolved registry into `prompts/list` / `prompts/get`
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
mcp-methods = { version = "0.3", default-features = false }
```

## See also

- [Downstream Binary](../guides/downstream-binary.md) — how to wrap `McpServer::new`
- [Architecture](../explanation/architecture.md) — three-crate layout
- The published [crates.io page](https://crates.io/crates/mcp-methods)
