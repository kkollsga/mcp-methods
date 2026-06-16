# Downstream Binary

When the generic `mcp-server` CLI (bundled in `pip install mcp-methods`) isn't enough — you need Cypher dispatch, custom domain tools, a database connection, embedding loaders — you build a **downstream binary**: a small Rust binary that depends on `mcp-methods` and wraps `McpServer::new(...)` with your additions.

The pattern is ~50-500 LOC depending on how much you layer.

## Quick start

The minimal example lives at [`examples/downstream_binary/`](https://github.com/kkollsga/mcp-methods/tree/main/examples/downstream_binary). ~60 LOC of Rust, one custom tool, shared mutable state. Copy it and modify.

```bash
git clone https://github.com/kkollsga/mcp-methods
cp -r mcp-methods/examples/downstream_binary ./my-server
cd my-server
cargo run --release -- --name "My Server"
```

Or write from scratch:

### `Cargo.toml`

```toml
[package]
name = "my-mcp-server"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "my-mcp-server"
path = "src/main.rs"

[dependencies]
mcp-methods = { version = "0.3", features = ["server"] }
anyhow = "1"
clap = { version = "4", features = ["derive"] }
rmcp = { version = "1.6", features = ["server", "macros", "transport-io"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread", "io-std"] }
schemars = "1"
serde = { version = "1", features = ["derive"] }
```

### `src/main.rs`

```rust
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Parser, Debug)]
struct Cli {
    #[arg(long, default_value = "My Server")]
    name: String,
}

#[derive(Deserialize, JsonSchema, Default)]
struct MyToolArgs {
    /// What to look up.
    #[serde(default)]
    query: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Defaults. For a real server, load a manifest:
    //     let m = mcp_methods::server::manifest::load(&yaml_path)?;
    //     ServerOptions::from_manifest(Some(&m), "My Server")
    let mut options = ServerOptions::from_manifest(None, "My Server");
    options.name = Some(cli.name);

    let mut server = McpServer::new(options);

    // Shared state captured by the tool closure. For a real server, this
    // would be a database connection, a knowledge graph handle, etc.
    let state: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let state_ref = state.clone();

    server.register_typed_tool::<MyToolArgs, _>(
        "lookup",
        "Look up a value. Returns the count of matching entries.",
        move |args: MyToolArgs| -> String {
            let s = state_ref.lock().unwrap();
            let count = s.iter().filter(|e| e.contains(&args.query)).count();
            format!("{count} matches for '{}'", args.query)
        },
    );

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
```

That's a complete, working MCP server.

## Adding more tools

Each tool registration is one `register_typed_tool` call:

```rust
server.register_typed_tool::<ArgsA, _>(
    "tool_a",
    "Tool A's description.",
    move |args: ArgsA| -> String { /* ... */ },
);

server.register_typed_tool::<ArgsB, _>(
    "tool_b",
    "Tool B's description.",
    move |args: ArgsB| -> String { /* ... */ },
);
```

The `T` type parameter is your tool's argument schema. It must implement:

- `serde::Deserialize` — for parsing the agent's call arguments
- `schemars::JsonSchema` — for generating the tool's argument schema at `initialize` time
- `Default` — for the case where the agent invokes the tool with no arguments
- `Send + Sync + 'static`

The handler is a sync `Fn(T) -> String`. The string body becomes the tool's response.

## Loading a manifest

Real servers almost always load a manifest:

```rust
use mcp_methods::server::manifest;

let yaml_path = std::path::Path::new("workspace_mcp.yaml");
let m = manifest::load(yaml_path)?;
let options = ServerOptions::from_manifest(Some(&m), "My Server");

// Enforce trust gates BEFORE constructing the server.
if m.extensions.get("my_hook").is_some() && !m.trust.allow_embedder {
    anyhow::bail!(
        "extensions.my_hook requires trust.allow_embedder: true"
    );
}
```

The framework's source tools are automatically available based on the manifest's `source_roots:` / `workspace:` declarations — you don't register them, the framework does.

## Operating modes

You can support the same flag set as `mcp-server` by porting [its main.rs](https://github.com/kkollsga/mcp-methods/blob/main/crates/mcp-server/src/main.rs) — `--source-root`, `--workspace`, `--watch`, `--mcp-config`. Or just hardcode the mode you need.

The [kglite-mcp-server](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server) reference adds its own flag (`--graph X.kgl`) on top of the framework's modes.

## Distributing

| If you want | Then |
|---|---|
| Operators install via pip | Wrap your binary in a Python wheel using maturin. See [Python Bindings](python-bindings.md) for the abi3 + bundled-binary pattern. |
| Operators install via cargo | `cargo publish` your binary to crates.io. Operators run `cargo install my-mcp-server`. |
| Operators install from git | Stay unpublished. Operators run `cargo install --git https://github.com/you/my-mcp-server`. |

The wheel-bundle pattern requires a libpython-free binary — which `mcp-methods` itself enables. If your binary doesn't transitively link libpython (which it won't, unless you depend on a pyo3 crate), you can ship one abi3 wheel per OS regardless of Python version.

If your binary does link libpython (e.g. you depend on a pyo3 crate that re-exports Python types), you'll be back to the per-Python-version wheel matrix — that's what bit kglite at 0.9.18 and drove them to ship their MCP server as a Python entry point (`kglite.mcp_server.server:main`) that uses FastMCP and calls into their pyo3 bindings, instead of a Rust binary.

## Read the source

- [`examples/downstream_binary/src/main.rs`](https://github.com/kkollsga/mcp-methods/blob/main/examples/downstream_binary/src/main.rs) — the minimal pattern (~60 LOC)
- [`kglite-mcp-server/src/main.rs`](https://github.com/kkollsga/kglite/blob/main/crates/kglite-mcp-server/src/main.rs) — the production reference (~500 LOC with Cypher dispatch, code-tree builder, watch mode, custom CLI flags)
- [`crates/mcp-server/src/main.rs`](https://github.com/kkollsga/mcp-methods/blob/main/crates/mcp-server/src/main.rs) — the generic CLI source (~370 LOC, all modes, no domain layer)

## See also

- [Writing a Manifest](writing-a-manifest.md)
- [Trust Gates](trust-gates.md) — enforcement responsibilities
- [Architecture](../explanation/architecture.md) — three-crate layout
