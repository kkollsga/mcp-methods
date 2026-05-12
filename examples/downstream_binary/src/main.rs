//! Minimal downstream-binary example built on the `mcp-methods` framework.
//!
//! Shows the pattern downstream Rust binaries (`kglite-mcp-server`,
//! your own domain server, etc.) use to layer custom tools on top of
//! the framework's boot sequence:
//!
//! 1. Depend on `mcp-methods` from crates.io with `features = ["server"]`.
//! 2. Build a [`ServerOptions`] (here defaulted via `from_manifest(None, ...)`).
//! 3. Construct an [`McpServer`].
//! 4. Register one or more custom tools via [`McpServer::register_typed_tool`].
//! 5. Serve over stdio via rmcp.
//!
//! Run with:
//!     cargo run -p greeter -- --name "Greeter Server"
//!
//! Then point an MCP client at its stdio. The server exposes a single
//! `greet` tool that takes a `name` argument and returns a friendly
//! greeting with a per-session counter — demonstrating shared mutable
//! state captured by the tool closure.

use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::Parser;
use mcp_methods::server::{McpServer, ServerOptions};
use rmcp::transport::stdio;
use rmcp::ServiceExt;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Parser, Debug)]
#[command(
    name = "greeter",
    about = "Minimal MCP server demonstrating the mcp-methods downstream-binary pattern"
)]
struct Cli {
    /// Server display name surfaced via the MCP `initialize` request.
    #[arg(long, default_value = "Greeter Server")]
    name: String,
}

/// Schema for the `greet` tool's arguments. `JsonSchema` lets rmcp
/// generate the tool schema returned at initialize time; `Default`
/// is required because the framework passes a default-constructed
/// `T` when the agent invokes the tool with no arguments.
#[derive(Deserialize, JsonSchema, Default)]
struct GreetArgs {
    /// Who to greet.
    #[serde(default)]
    name: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Defaults — no manifest, no source roots, no workspace.
    // Real downstream binaries typically load a manifest via
    // `mcp_methods::server::manifest::load(path)` and call
    // `ServerOptions::from_manifest(Some(&manifest), fallback_name)`.
    let mut options = ServerOptions::from_manifest(None, "Greeter Server");
    options.name = Some(cli.name);

    let mut server = McpServer::new(options);

    // Per-session state captured by the tool closure. Real downstream
    // binaries wrap this in an `Arc<RwLock<...>>` over a domain handle
    // (a database client, a knowledge graph, a session store, …).
    let counter: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let counter_ref = counter.clone();

    server.register_typed_tool::<GreetArgs, _>(
        "greet",
        "Send a friendly greeting. Increments a per-session counter.",
        move |args: GreetArgs| {
            let target = if args.name.is_empty() {
                "world".to_string()
            } else {
                args.name
            };
            let mut n = counter_ref.lock().unwrap();
            *n += 1;
            format!("Hello, {target}! (greeting #{n} this session)")
        },
    );

    eprintln!("greeter: serving 'greet' tool over stdio");

    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
