# mcp-methods examples

Runnable examples showing how to consume the `mcp-methods` framework.
Each subdirectory is a self-contained crate (its own `Cargo.toml`)
that depends on `mcp-methods` from crates.io — exactly the shape a
real downstream consumer would use.

These examples are NOT part of the workspace (`exclude`'d in the
root `Cargo.toml`). Build and run them from inside the example
directory:

| Example | Demonstrates |
|---|---|
| `downstream_binary/` | The minimum pattern for a domain-specific MCP server binary built on `mcp-methods::server::McpServer`. Adds a single `greet` tool with shared mutable state to show the closure capture pattern. ~60 LOC. |

## Running `downstream_binary`

```bash
cd examples/downstream_binary
cargo run --release -- --name "Greeter"
```

The binary serves over stdio. Connect an MCP client to invoke the
`greet` tool. To plug it into Claude Code or similar:

```json
"greeter": {
  "command": "/absolute/path/to/examples/downstream_binary/target/release/greeter",
  "args": ["--name", "Greeter"]
}
```

## What this teaches

The downstream-binary pattern is the answer to "I want graph queries
/ custom domain tools but mcp-methods's generic `mcp-server` CLI
doesn't have them." You depend on `mcp-methods` from crates.io,
construct your own `McpServer` with `ServerOptions` derived from a
manifest (or defaults), register your tools, and serve.

For a production example, see
[`kglite-mcp-server`](https://github.com/kkollsga/kglite/tree/main/crates/kglite-mcp-server)
— ~500 LOC, layers `cypher_query`, `graph_overview`, `save_graph`,
and `read_code_source` on top of the same `McpServer::new(options)`
foundation this example uses. Same pattern, more tools.
